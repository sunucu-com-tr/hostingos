//! General settings: company facts (the `tenants` columns printed on invoices,
//! the storefront footer and the e-mail footer) and brand images. The brand
//! colours are edited on the Appearance tab ([`super::appearance`]);
//! `POST /admin/settings/general/brand` remains for existing clients.
//!
//! Images are stored in `tenant_assets` and served by
//! `GET /brand/asset/{logo|logo_dark|favicon}` ([`serve_asset`], public,
//! tenant-scoped, `?v=<hash>` → long-lived cache). Uploads are sniffed by
//! magic bytes (PNG, JPEG, GIF, WebP; ICO for the favicon): SVG and anything
//! else is rejected, the declared content type is ignored.

use askama::Template;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use hos_core::tenant::{Branding, HexColor};
use hos_db::repo::admin_settings::{self as srepo, CompanyProfile};
use hos_db::repo::platform;
use http::{HeaderValue, StatusCode, header};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{invalidate_caches, page_chrome, tabs};
use crate::error::{AppError, AppResult};
use crate::extract::{CurrentStaff, RequirePermission, perm};
use crate::flash::{self, SetFlash};
use crate::forms::{self, FormErrors};
use crate::routes::admin::common::{Audit, FormData, Multipart, done, failed};
use crate::state::AppState;
use crate::tenant::Tenant;
use crate::view::{NavItem, PageCtx, render, render_status};

/// Largest accepted logo.
pub const MAX_LOGO_BYTES: usize = 512 * 1024;
/// Largest accepted favicon.
pub const MAX_FAVICON_BYTES: usize = 128 * 1024;
/// Largest accepted image side (pixels) where the format tells it.
pub const MAX_SIDE: u32 = hos_core::image::MAX_SIDE;

/// Asset kinds.
pub const KINDS: &[&str] = &["logo", "logo_dark", "favicon"];

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/admin/settings/general", get(show).post(save_company))
        .route("/admin/settings/general/brand", post(save_brand))
        .route("/admin/settings/general/assets/{kind}", post(upload))
        .route("/admin/settings/general/assets/{kind}/delete", post(remove))
}

/// Public routes (outside the staff guard).
pub fn public_routes() -> Router<AppState> {
    Router::new().route("/brand/asset/{kind}", get(serve_asset))
}

// ---------------------------------------------------------------------------
// page
// ---------------------------------------------------------------------------

/// Company form values.
#[derive(Debug, Clone, Default)]
pub struct CompanyForm {
    pub company_name: String,
    pub legal_name: String,
    pub tax_office: String,
    pub tax_number: String,
    pub line1: String,
    pub city: String,
    pub state: String,
    pub postal_code: String,
    pub country_code: String,
    pub contact_email: String,
    pub contact_phone: String,
    pub support_email: String,
}

/// Brand colour form values.
#[derive(Debug, Clone, Default)]
pub struct BrandForm {
    pub primary_color: String,
    pub accent_color: String,
}

/// An uploaded image slot.
#[derive(Debug, Clone)]
pub struct AssetSlot {
    pub kind: &'static str,
    pub label: String,
    pub hint: String,
    pub url: String,
}

#[derive(Template)]
#[template(path = "admin/settings/general.html")]
struct GeneralPage {
    ctx: PageCtx,
    tabs: Vec<NavItem>,
    active: String,
    company: CompanyForm,
    errors: FormErrors,
    brand_errors: FormErrors,
    warnings: Vec<String>,
    assets: Vec<AssetSlot>,
}
crate::page!(GeneralPage);

fn s(v: &Option<String>) -> String {
    v.clone().unwrap_or_default()
}

async fn company_form(state: &AppState, tenant: &Tenant) -> AppResult<CompanyForm> {
    let row = platform::get_tenant(&state.pool, tenant.id)
        .await?
        .ok_or(AppError::NotFound)?;
    let a = |k: &str| {
        row.address
            .get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    let b = row.branding();
    Ok(CompanyForm {
        company_name: b.company_name.clone().unwrap_or_else(|| row.name.clone()),
        legal_name: s(&row.legal_name),
        tax_office: s(&row.tax_office),
        tax_number: s(&row.tax_number),
        line1: a("line1"),
        city: a("city"),
        state: a("state"),
        postal_code: a("postal_code"),
        country_code: a("country_code"),
        contact_email: s(&row.contact_email),
        contact_phone: s(&row.contact_phone),
        support_email: s(&b.support_email),
    })
}

fn brand_form(b: &Branding) -> BrandForm {
    BrandForm {
        primary_color: b
            .primary_color
            .as_ref()
            .map(|c| c.as_str().to_owned())
            .unwrap_or_else(|| "#2563eb".into()),
        accent_color: b
            .accent_color
            .as_ref()
            .map(|c| c.as_str().to_owned())
            .unwrap_or_else(|| "#f59e0b".into()),
    }
}

/// WCAG relative luminance of `#rrggbb` (`None` for other forms).
fn luminance(hex: &str) -> Option<f64> {
    let h = hex.strip_prefix('#')?;
    let h = match h.len() {
        3 | 4 => h.chars().take(3).flat_map(|c| [c, c]).collect::<String>(),
        6 | 8 => h[..6].to_owned(),
        _ => return None,
    };
    let channel = |i: usize| -> Option<f64> {
        let v = f64::from(u8::from_str_radix(&h[i..i + 2], 16).ok()?) / 255.0;
        Some(if v <= 0.039_28 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        })
    };
    Some(0.2126 * channel(0)? + 0.7152 * channel(2)? + 0.0722 * channel(4)?)
}

/// Contrast ratio of white text on `hex` (1–21).
pub fn contrast_with_white(hex: &str) -> Option<f64> {
    luminance(hex).map(|l| 1.05 / (l + 0.05))
}

fn contrast_warnings(ctx: &PageCtx, brand: &BrandForm) -> Vec<String> {
    let mut out = Vec::new();
    for (label, color) in [
        ("admin_settings.general.primary_color", &brand.primary_color),
        ("admin_settings.general.accent_color", &brand.accent_color),
    ] {
        if let Some(ratio) = contrast_with_white(color)
            && ratio < 4.5
        {
            out.push(ctx.tv2(
                "admin_settings.general.contrast_warning",
                "color",
                ctx.t(label),
                "ratio",
                format!("{ratio:.1}"),
            ));
        }
    }
    out
}

fn asset_slots(ctx: &PageCtx, b: &Branding) -> Vec<AssetSlot> {
    KINDS
        .iter()
        .map(|k| AssetSlot {
            kind: k,
            label: ctx.t(&format!("admin_settings.general.asset_{k}")),
            hint: ctx.t(if *k == "favicon" {
                "admin_settings.general.favicon_hint"
            } else {
                "admin_settings.general.logo_hint"
            }),
            url: match *k {
                "logo" => s(&b.logo_url),
                "logo_dark" => s(&b.logo_dark_url),
                _ => s(&b.favicon_url),
            },
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn page(
    mut ctx: PageCtx,
    staff: &CurrentStaff,
    branding: &Branding,
    company: CompanyForm,
    brand: BrandForm,
    errors: FormErrors,
    brand_errors: FormErrors,
) -> AppResult<Response> {
    page_chrome(&mut ctx, staff, "admin_settings.tabs.general");
    let warnings = contrast_warnings(&ctx, &brand);
    let failed = errors.any() || brand_errors.any();
    let page = GeneralPage {
        tabs: tabs(&ctx),
        active: ctx.url("/admin/settings/general"),
        assets: asset_slots(&ctx, branding),
        company,
        errors,
        brand_errors,
        warnings,
        ctx,
    };
    if failed {
        render_status(StatusCode::UNPROCESSABLE_ENTITY, &page)
    } else {
        render(&page)
    }
}

async fn show(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SettingsManage>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let company = company_form(&state, &tenant).await?;
    let branding = current_branding(&state, &tenant).await?;
    page(
        ctx,
        &staff,
        &branding,
        company,
        brand_form(&branding),
        FormErrors::default(),
        FormErrors::default(),
    )
}

async fn current_branding(state: &AppState, tenant: &Tenant) -> AppResult<Branding> {
    Ok(platform::get_tenant(&state.pool, tenant.id)
        .await?
        .ok_or(AppError::NotFound)?
        .branding())
}

fn opt(v: &str) -> Option<String> {
    let t = v.trim();
    (!t.is_empty()).then(|| t.to_owned())
}

async fn save_company(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SettingsManage>,
    au: Audit,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let form = CompanyForm {
        company_name: f.text("company_name", 120),
        legal_name: f.text("legal_name", 200),
        tax_office: f.text("tax_office", 120),
        tax_number: f.text("tax_number", 40),
        line1: f.text("line1", 200),
        city: f.text("city", 100),
        state: f.text("state", 100),
        postal_code: f.text("postal_code", 20),
        country_code: f.text("country_code", 2).to_ascii_uppercase(),
        contact_email: f.text("contact_email", 254),
        contact_phone: f.text("contact_phone", 40),
        support_email: f.text("support_email", 254),
    };
    let mut errors = FormErrors::default();
    if form.company_name.is_empty() {
        errors.add("company_name", ctx.t("admin.common.required"));
    }
    for (k, v) in [
        ("contact_email", &form.contact_email),
        ("support_email", &form.support_email),
    ] {
        if !v.is_empty() && !forms::valid_email(v) {
            errors.add(k, ctx.t("admin.common.invalid_email"));
        }
    }
    if !form.country_code.is_empty()
        && (form.country_code.len() != 2
            || !form.country_code.bytes().all(|b| b.is_ascii_uppercase()))
    {
        errors.add("country_code", ctx.t("admin_settings.errors.country"));
    }
    if !form.tax_number.is_empty()
        && !form
            .tax_number
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == ' ' || c == '-')
    {
        errors.add("tax_number", ctx.t("admin.common.invalid_value"));
    }
    let branding = current_branding(&state, &tenant).await?;
    if errors.any() {
        let brand = brand_form(&branding);
        return page(
            ctx,
            &staff,
            &branding,
            form,
            brand,
            errors,
            FormErrors::default(),
        );
    }
    let before = company_form(&state, &tenant).await?;
    let address = json!({
        "line1": form.line1, "city": form.city, "state": form.state,
        "postal_code": form.postal_code, "country_code": form.country_code,
    });
    srepo::update_company_profile(
        &state.pool,
        tenant.id,
        &CompanyProfile {
            name: tenant.name.clone(),
            legal_name: opt(&form.legal_name),
            tax_office: opt(&form.tax_office),
            tax_number: opt(&form.tax_number),
            address,
            contact_email: opt(&form.contact_email),
            contact_phone: opt(&form.contact_phone),
        },
    )
    .await?;
    let mut nb = branding.clone();
    nb.company_name = opt(&form.company_name);
    nb.support_email = opt(&form.support_email);
    if nb != branding {
        platform::update_branding(&state.pool, tenant.id, &nb).await?;
    }
    let diff = company_diff(&before, &form);
    if !diff.is_empty() {
        au.log(&state, "settings.company", None, Value::Object(diff))
            .await?;
    }
    invalidate_caches(&state, &tenant);
    Ok(done(&ctx, "/admin/settings/general", "admin.common.saved"))
}

fn company_diff(a: &CompanyForm, b: &CompanyForm) -> serde_json::Map<String, Value> {
    let pairs = [
        ("company_name", &a.company_name, &b.company_name),
        ("legal_name", &a.legal_name, &b.legal_name),
        ("tax_office", &a.tax_office, &b.tax_office),
        ("tax_number", &a.tax_number, &b.tax_number),
        ("line1", &a.line1, &b.line1),
        ("city", &a.city, &b.city),
        ("state", &a.state, &b.state),
        ("postal_code", &a.postal_code, &b.postal_code),
        ("country_code", &a.country_code, &b.country_code),
        ("contact_email", &a.contact_email, &b.contact_email),
        ("contact_phone", &a.contact_phone, &b.contact_phone),
        ("support_email", &a.support_email, &b.support_email),
    ];
    pairs
        .into_iter()
        .filter(|(_, x, y)| x != y)
        .map(|(k, x, y)| (k.to_owned(), json!({"from": x, "to": y})))
        .collect()
}

async fn save_brand(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SettingsManage>,
    au: Audit,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let form = BrandForm {
        primary_color: f.text("primary_color", 9),
        accent_color: f.text("accent_color", 9),
    };
    let mut errors = FormErrors::default();
    let primary = HexColor::new(&form.primary_color);
    let accent = HexColor::new(&form.accent_color);
    if primary.is_err() {
        errors.add("primary_color", ctx.t("admin_settings.errors.color"));
    }
    if accent.is_err() {
        errors.add("accent_color", ctx.t("admin_settings.errors.color"));
    }
    let branding = current_branding(&state, &tenant).await?;
    let (Ok(primary), Ok(accent)) = (primary, accent) else {
        let company = company_form(&state, &tenant).await?;
        return page(
            ctx,
            &staff,
            &branding,
            company,
            form,
            FormErrors::default(),
            errors,
        );
    };
    let mut nb = branding.clone();
    nb.primary_color = Some(primary);
    nb.accent_color = Some(accent);
    if nb != branding {
        platform::update_branding(&state.pool, tenant.id, &nb).await?;
        au.log(
            &state,
            "settings.branding",
            None,
            json!({
                "primary_color": {"from": branding.primary_color, "to": nb.primary_color},
                "accent_color": {"from": branding.accent_color, "to": nb.accent_color},
            }),
        )
        .await?;
    }
    invalidate_caches(&state, &tenant);
    let warnings = contrast_warnings(&ctx, &brand_form(&nb));
    let mut flash = SetFlash::success(ctx.t("admin.common.saved"));
    for w in warnings {
        flash = flash.and(crate::flash::FlashKind::Warning, w);
    }
    Ok(flash::redirect(&ctx.url("/admin/settings/general"), flash))
}

// ---------------------------------------------------------------------------
// images
// ---------------------------------------------------------------------------

/// Sniffs an uploaded image: `(content type, width, height)`; `None` for
/// anything that is not one of the allowed raster formats (SVG included).
/// The size comes from the headers ([`hos_core::image`]); it is `None` when
/// the headers do not give it, and such an image is refused like an
/// oversized one (a JPEG / WebP of unknown size could decode to gigabytes in
/// the PDF renderer).
pub fn sniff_image(data: &[u8], favicon: bool) -> Option<(&'static str, Option<(u32, u32)>)> {
    use hos_core::image::{ImageFormat, dimensions};
    let format = if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        ImageFormat::Png
    } else if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        ImageFormat::Jpeg
    } else if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
        ImageFormat::Gif
    } else if data.len() >= 12 && data.starts_with(b"RIFF") && &data[8..12] == b"WEBP" {
        ImageFormat::Webp
    } else if data.len() >= 6 && data.starts_with(&[0, 0, 1, 0]) && data[4..6] != [0, 0] {
        ImageFormat::Ico
    } else {
        return None;
    };
    let allowed = if favicon {
        matches!(format, ImageFormat::Png | ImageFormat::Ico)
    } else {
        format != ImageFormat::Ico
    };
    if !allowed {
        return None;
    }
    let dims = dimensions(data)
        .filter(|(f, _, _)| *f == format)
        .map(|(_, w, h)| (w, h));
    Some((format.content_type(), dims))
}

fn kind_of(raw: &str) -> AppResult<&'static str> {
    KINDS
        .iter()
        .copied()
        .find(|k| *k == raw)
        .ok_or(AppError::NotFound)
}

fn set_url(b: &mut Branding, kind: &str, url: Option<String>) {
    match kind {
        "logo" => b.logo_url = url,
        "logo_dark" => b.logo_dark_url = url,
        _ => b.favicon_url = url,
    }
}

async fn upload(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::SettingsManage>,
    au: Audit,
    Path(kind): Path<String>,
    ctx: PageCtx,
    mp: Multipart,
) -> AppResult<Response> {
    let kind = kind_of(&kind)?;
    let back = "/admin/settings/general";
    let Some(part) = mp.file("file") else {
        return Ok(failed(&ctx, back, ctx.t("admin_settings.general.no_file")));
    };
    let favicon = kind == "favicon";
    let max = if favicon {
        MAX_FAVICON_BYTES
    } else {
        MAX_LOGO_BYTES
    };
    if part.data.len() > max {
        return Ok(failed(
            &ctx,
            back,
            ctx.tv("admin_settings.general.too_large", "kb", max / 1024),
        ));
    }
    let Some((content_type, dims)) = sniff_image(&part.data, favicon) else {
        let key = if favicon {
            "admin_settings.general.bad_favicon"
        } else {
            "admin_settings.general.bad_image"
        };
        return Ok(failed(&ctx, back, ctx.t(key)));
    };
    if dims.is_none_or(|(w, h)| w == 0 || h == 0 || w > MAX_SIDE || h > MAX_SIDE) {
        return Ok(failed(
            &ctx,
            back,
            ctx.t("admin_settings.general.bad_dimensions"),
        ));
    }
    let hash = hos_core::crypto::sha256_hex(&part.data);
    srepo::put_tenant_asset(
        &state.pool,
        tenant.id,
        kind,
        content_type,
        &part.data,
        &hash,
    )
    .await?;
    let mut b = current_branding(&state, &tenant).await?;
    set_url(
        &mut b,
        kind,
        Some(format!("/brand/asset/{kind}?v={}", &hash[..12])),
    );
    platform::update_branding(&state.pool, tenant.id, &b).await?;
    au.log(
        &state,
        "settings.asset_upload",
        None,
        json!({"kind": kind, "content_type": content_type, "bytes": part.data.len(), "sha256": hash}),
    )
    .await?;
    invalidate_caches(&state, &tenant);
    Ok(done(&ctx, back, "admin_settings.general.uploaded"))
}

async fn remove(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::SettingsManage>,
    au: Audit,
    Path(kind): Path<String>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let kind = kind_of(&kind)?;
    srepo::delete_tenant_asset(&state.pool, tenant.id, kind).await?;
    let mut b = current_branding(&state, &tenant).await?;
    set_url(&mut b, kind, None);
    platform::update_branding(&state.pool, tenant.id, &b).await?;
    au.log(&state, "settings.asset_delete", None, json!({"kind": kind}))
        .await?;
    invalidate_caches(&state, &tenant);
    Ok(done(
        &ctx,
        "/admin/settings/general",
        "admin.common.deleted",
    ))
}

/// `?v=` of an asset URL.
#[derive(Debug, Default, Deserialize)]
pub struct AssetQuery {
    #[serde(default)]
    v: Option<String>,
}

/// `GET /brand/asset/{kind}` — the tenant's uploaded image.
pub async fn serve_asset(
    State(state): State<AppState>,
    tenant: Tenant,
    Path(kind): Path<String>,
    Query(q): Query<AssetQuery>,
    headers: http::HeaderMap,
) -> AppResult<Response> {
    let kind = kind_of(&kind)?;
    let row = srepo::get_tenant_asset(&state.pool, tenant.id, kind)
        .await?
        .ok_or(AppError::NotFound)?;
    let etag = format!("\"{}\"", &row.sha256[..32.min(row.sha256.len())]);
    let versioned =
        q.v.as_deref()
            .is_some_and(|v| row.sha256.starts_with(v) && !v.is_empty());
    let cache = if versioned {
        "public, max-age=31536000, immutable"
    } else {
        "public, max-age=300"
    };
    let not_modified = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim() == etag));
    let mut res = if not_modified {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        row.data.into_response()
    };
    let h = res.headers_mut();
    if let Ok(v) = HeaderValue::from_str(&row.content_type) {
        h.insert(header::CONTENT_TYPE, v);
    }
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    if let Ok(v) = HeaderValue::from_str(&etag) {
        h.insert(header::ETAG, v);
    }
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_static("inline"),
    );
    Ok(res)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffing() {
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        png.extend_from_slice(&64u32.to_be_bytes());
        png.extend_from_slice(&32u32.to_be_bytes());
        assert_eq!(
            sniff_image(&png, false),
            Some(("image/png", Some((64, 32))))
        );
        assert_eq!(sniff_image(&png, true).map(|s| s.0), Some("image/png"));
        assert_eq!(
            sniff_image(b"<svg xmlns=\"http://www.w3.org/2000/svg\"></svg>", false),
            None
        );
        assert_eq!(sniff_image(b"<?xml version=\"1.0\"?><svg/>", true), None);
        assert_eq!(
            sniff_image(&[0xFF, 0xD8, 0xFF, 0xE0, 0, 0], false),
            Some(("image/jpeg", None)),
            "a JPEG without a frame header has no known size"
        );
        assert_eq!(sniff_image(&[0xFF, 0xD8, 0xFF, 0xE0, 0, 0], true), None);
        assert_eq!(
            sniff_image(&[0, 0, 1, 0, 1, 0, 16, 16], true).map(|s| s.0),
            Some("image/x-icon")
        );
        assert_eq!(
            sniff_image(b"GIF89a\x10\0\x08\0\0\0\0\x3B", false),
            Some(("image/gif", Some((16, 8))))
        );
        // A 16383 × 16383 lossless WebP of a few bytes: its size is read
        // from the header, so the upload is refused.
        let bits: u32 = 16382 | (16382 << 14);
        let mut bomb = b"RIFF\x1a\0\0\0WEBPVP8L\x0d\0\0\0\x2f".to_vec();
        bomb.extend_from_slice(&bits.to_le_bytes());
        assert_eq!(
            sniff_image(&bomb, false),
            Some(("image/webp", Some((16383, 16383))))
        );
        let big_jpeg = [
            0xFF, 0xD8, 0xFF, 0xC0, 0x00, 0x11, 0x08, 0x13, 0x88, 0x00, 0x10, 0x03,
        ];
        assert_eq!(
            sniff_image(&big_jpeg, false),
            Some(("image/jpeg", Some((16, 5000))))
        );
    }

    #[test]
    fn contrast() {
        assert!(contrast_with_white("#ffffff").unwrap() < 1.1);
        assert!(contrast_with_white("#000000").unwrap() > 20.0);
        assert!(contrast_with_white("#2563eb").unwrap() > 4.5);
        assert!(contrast_with_white("#f59e0b").unwrap() < 4.5);
        assert!(contrast_with_white("#fff").unwrap() < 1.1);
    }
}
