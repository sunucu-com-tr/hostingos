//! Tenant (hosting company) request context.
//!
//! `hos-web` resolves the tenant from the `Host` header (`tenant_domains`, or
//! `HOS_DEV_TENANT` on `localhost`) and puts a [`TenantContext`] into request
//! extensions. Every repository call takes `ctx.id` as its first argument.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::domain::{Locale, TenantStatus};
use crate::ids::TenantId;
use crate::money::Currency;

/// Everything request handlers need to know about the current tenant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TenantContext {
    /// Tenant id (scope for every query).
    pub id: TenantId,
    /// URL-safe unique slug (`{slug}.{HOS_BASE_DOMAIN}`).
    pub slug: String,
    /// Display name.
    pub name: String,
    /// Platform lifecycle status.
    pub status: TenantStatus,
    /// Default UI / email language.
    pub default_locale: Locale,
    /// Default billing currency.
    pub default_currency: Currency,
    /// IANA time zone used for due dates and reports (e.g. `Europe/Istanbul`).
    pub timezone: String,
    /// Canonical public host (custom domain or platform subdomain), without scheme.
    pub primary_domain: Option<String>,
    /// Branding / white-label settings.
    #[serde(default)]
    pub branding: Branding,
}

impl TenantContext {
    /// Whether requests should be served (trial or active).
    pub fn is_operational(&self) -> bool {
        self.status.is_operational()
    }

    /// Name shown to customers: the branding company name, else the tenant name.
    pub fn display_name(&self) -> &str {
        self.branding.company_name.as_deref().unwrap_or(&self.name)
    }
}

/// White-label settings of a tenant. All optional; the UI falls back to defaults.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Branding {
    /// Legal / display company name.
    pub company_name: Option<String>,
    /// Logo URL for light backgrounds.
    pub logo_url: Option<String>,
    /// Logo URL for dark backgrounds.
    pub logo_dark_url: Option<String>,
    /// Favicon URL.
    pub favicon_url: Option<String>,
    /// Primary brand colour.
    pub primary_color: Option<HexColor>,
    /// Accent colour.
    pub accent_color: Option<HexColor>,
    /// Public support email.
    pub support_email: Option<String>,
    /// Display name for outgoing email (`From: "<name>" <address>`).
    pub email_from_name: Option<String>,
    /// Sender address for outgoing email.
    pub email_from_address: Option<String>,
}

impl Branding {
    /// CSS custom properties for the tenant theme, e.g.
    /// `--hos-color-primary:#1a73e8;--hos-color-accent:#ff6600;`. Safe to inline
    /// into a `style` attribute/element because [`HexColor`] is validated.
    pub fn css_variables(&self) -> String {
        let mut css = String::new();
        if let Some(c) = &self.primary_color {
            css.push_str("--hos-color-primary:");
            css.push_str(c.as_str());
            css.push(';');
        }
        if let Some(c) = &self.accent_color {
            css.push_str("--hos-color-accent:");
            css.push_str(c.as_str());
            css.push(';');
        }
        css
    }
}

/// A validated CSS hex colour: `#rgb`, `#rgba`, `#rrggbb` or `#rrggbbaa`
/// (stored lower-case). Validation makes it safe to emit into CSS.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct HexColor(String);

/// Rejected colour string.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid hex colour {0:?} (expected #rgb, #rgba, #rrggbb or #rrggbbaa)")]
pub struct InvalidHexColor(pub String);

impl HexColor {
    /// Validates and normalises (`"#1A73E8"` → `#1a73e8`).
    pub fn new(value: &str) -> Result<Self, InvalidHexColor> {
        let v = value.trim();
        let digits = v.strip_prefix('#').unwrap_or("");
        let ok =
            matches!(digits.len(), 3 | 4 | 6 | 8) && digits.bytes().all(|b| b.is_ascii_hexdigit());
        if ok {
            Ok(Self(v.to_ascii_lowercase()))
        } else {
            Err(InvalidHexColor(v.chars().take(32).collect()))
        }
    }

    /// The normalised colour string, including `#`.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for HexColor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "HexColor({})", self.0)
    }
}

impl fmt::Display for HexColor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for HexColor {
    type Err = InvalidHexColor;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

impl Serialize for HexColor {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for HexColor {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = std::borrow::Cow::<'de, str>::deserialize(deserializer)?;
        Self::new(&s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_color_validation() {
        assert_eq!(HexColor::new("#1A73E8").unwrap().as_str(), "#1a73e8");
        assert!(HexColor::new("#abc").is_ok());
        assert!(HexColor::new("#abcd").is_ok());
        assert!(HexColor::new("#11223344").is_ok());
        assert!(HexColor::new("1a73e8").is_err());
        assert!(HexColor::new("#12345").is_err());
        assert!(HexColor::new("#ggg").is_err());
        assert!(HexColor::new("red;}body{display:none").is_err());
        assert!(serde_json::from_str::<HexColor>("\"#zzz\"").is_err());
    }

    #[test]
    fn context_serde_and_css() {
        let ctx = TenantContext {
            id: TenantId::new(),
            slug: "acme".into(),
            name: "Acme Hosting".into(),
            status: TenantStatus::Active,
            default_locale: Locale::Tr,
            default_currency: Currency::TRY,
            timezone: "Europe/Istanbul".into(),
            primary_domain: Some("acme.example.com".into()),
            branding: Branding {
                primary_color: Some(HexColor::new("#112233").unwrap()),
                accent_color: Some(HexColor::new("#ABC").unwrap()),
                ..Branding::default()
            },
        };
        assert!(ctx.is_operational());
        assert_eq!(ctx.display_name(), "Acme Hosting");
        assert_eq!(
            ctx.branding.css_variables(),
            "--hos-color-primary:#112233;--hos-color-accent:#abc;"
        );
        let json = serde_json::to_string(&ctx).unwrap();
        let back: TenantContext = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ctx);
    }
}
