//! Downloads area: staff upload (content.manage, CSRF, type check by
//! bytes), the public list and file on the storefront, the client list
//! with the access rules (public / signed-in clients / clients with an
//! active service of a linked product), hidden files and categories, a
//! sub-user without the services area, another client and another tenant.

use crate::biz_support::{
    active_service_of, csrf_of, login, multipart, owner, post, product, staff, sub_user,
};
use crate::common::TestApp;
use hos_db::repo::downloads as drepo;
use http::StatusCode;

const PDF: &[u8] = b"%PDF-1.4\n% kilavuz\n1 0 obj << >> endobj\ntrailer << >>\n%%EOF\n";
const ZIP: &[u8] = b"PK\x03\x04\x14\x00\x00\x00\x08\x00araclar";
const TXT: &[u8] = b"Lisans anahtari: yalnizca urun sahipleri icin.\n";

async fn id_of(app: &TestApp, title: &str) -> drepo::DownloadId {
    drepo::list_all(app.pool(), app.tenant_id)
        .await
        .unwrap()
        .into_iter()
        .find(|d| d.title == title)
        .unwrap_or_else(|| panic!("download {title}"))
        .id
}

#[tokio::test]
async fn downloads_area_access_rules_and_negative_cases() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let licensed = product(&app).await;
    let (_a_user, a_client, a_email) = owner(&app, "indirme").await;
    let (_b_user, _b_client, b_email) = owner(&app, "baskasi").await;
    active_service_of(&app, a_client, licensed).await;

    // ---- staff without content.manage: 403
    let csrf = staff(&mut app, "billing").await;
    assert_eq!(
        app.get("/admin/downloads").await.status,
        StatusCode::FORBIDDEN
    );
    let r = multipart(
        &mut app,
        "/admin/downloads",
        &[("csrf_token", &csrf), ("title", "X"), ("access", "public")],
        Some(("file", "x.pdf", PDF)),
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);

    // ---- admin: category + uploads
    staff(&mut app, "admin").await;
    let page = app.get("/admin/downloads").await;
    assert_eq!(page.status, StatusCode::OK);
    let csrf = page.csrf();
    let r = post(
        &mut app,
        "/admin/downloads/categories",
        &csrf,
        &[("name", "Kılavuzlar"), ("sort_order", "0")],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    let cat = drepo::categories(app.pool(), app.tenant_id).await.unwrap()[0].id;
    // missing CSRF (multipart)
    let r = multipart(
        &mut app,
        "/admin/downloads",
        &[("title", "Kurulum kılavuzu"), ("access", "public")],
        Some(("file", "kurulum.pdf", PDF)),
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "missing CSRF");
    // an executable renamed to .pdf is refused by its bytes
    let r = multipart(
        &mut app,
        "/admin/downloads",
        &[
            ("csrf_token", &csrf),
            ("title", "Kötü"),
            ("access", "public"),
            ("sort_order", "0"),
        ],
        Some(("file", "kurulum.pdf", b"MZ\x90\x00\x03\x00\x00\x00")),
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert!(
        drepo::list_all(app.pool(), app.tenant_id)
            .await
            .unwrap()
            .is_empty(),
        "bad type stored"
    );
    // product access without a product is refused
    let r = multipart(
        &mut app,
        "/admin/downloads",
        &[
            ("csrf_token", &csrf),
            ("title", "Lisans dosyası"),
            ("access", "product"),
            ("sort_order", "0"),
        ],
        Some(("file", "lisans.txt", TXT)),
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert!(
        drepo::list_all(app.pool(), app.tenant_id)
            .await
            .unwrap()
            .is_empty()
    );
    // a forged product id is a 404
    let r = multipart(
        &mut app,
        "/admin/downloads",
        &[
            ("csrf_token", &csrf),
            ("title", "Lisans dosyası"),
            ("access", "product"),
            ("product_ids", &uuid::Uuid::now_v7().to_string()),
            ("sort_order", "0"),
        ],
        Some(("file", "lisans.txt", TXT)),
    )
    .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    let cat_s = cat.to_string();
    let licensed_s = licensed.to_string();
    for (title, access, file, bytes, extra) in [
        ("Kurulum kılavuzu", "public", "kurulum.pdf", PDF, None),
        (
            "Yardımcı araçlar",
            "clients",
            "araclar.zip",
            ZIP,
            Some(("category_id", cat_s.as_str())),
        ),
        (
            "Lisans dosyası",
            "product",
            "lisans.txt",
            TXT,
            Some(("product_ids", licensed_s.as_str())),
        ),
        (
            "Gizli dosya",
            "public",
            "gizli.pdf",
            PDF,
            Some(("is_hidden", "1")),
        ),
    ] {
        let mut fields = vec![
            ("csrf_token", csrf.as_str()),
            ("title", title),
            ("access", access),
            ("sort_order", "0"),
        ];
        if let Some(e) = extra {
            fields.push(e);
        }
        let r = multipart(
            &mut app,
            "/admin/downloads",
            &fields,
            Some(("file", file, bytes)),
        )
        .await;
        assert_eq!(r.status, StatusCode::SEE_OTHER, "{title}: {}", r.body);
    }
    let public = id_of(&app, "Kurulum kılavuzu").await;
    let clients_only = id_of(&app, "Yardımcı araçlar").await;
    let licensed_dl = id_of(&app, "Lisans dosyası").await;
    let hidden = id_of(&app, "Gizli dosya").await;
    let row = drepo::get(app.pool(), app.tenant_id, public)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        row.sha256.as_deref(),
        Some(hos_core::crypto::sha256_hex(PDF).as_str())
    );
    assert_eq!(row.size_bytes, i64::try_from(PDF.len()).unwrap());
    // staff can fetch even the hidden one
    let r = app.get(&format!("/admin/downloads/{hidden}/file")).await;
    assert_eq!(r.status, StatusCode::OK);

    // ---- another tenant's staff and clients see nothing of it
    let Some(mut other) = TestApp::new().await else {
        return;
    };
    let other_csrf = staff(&mut other, "owner").await;
    assert_eq!(
        other
            .get(&format!("/admin/downloads/{public}"))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        other
            .get(&format!("/admin/downloads/{public}/file"))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    let r = post(
        &mut other,
        &format!("/admin/downloads/{public}/delete"),
        &other_csrf,
        &[],
    )
    .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    other.cookies.clear();
    assert_eq!(
        other.get(&format!("/downloads/{public}/file")).await.status,
        StatusCode::NOT_FOUND
    );
    other.cleanup().await;
    assert!(
        drepo::get(app.pool(), app.tenant_id, public)
            .await
            .unwrap()
            .is_some()
    );

    // ---- guest: public only
    app.cookies.clear();
    let list = app.get("/downloads").await;
    assert_eq!(list.status, StatusCode::OK);
    assert!(list.body.contains("Kurulum kılavuzu"));
    assert!(
        !list.body.contains("Yardımcı araçlar")
            && !list.body.contains("Lisans dosyası")
            && !list.body.contains("Gizli dosya")
    );
    let f = app.get(&format!("/downloads/{public}/file")).await;
    assert_eq!(f.status, StatusCode::OK);
    assert_eq!(f.content_type(), "application/pdf");
    let disposition = f
        .headers
        .get("content-disposition")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(disposition.starts_with("attachment;") && disposition.contains("kurulum.pdf"));
    assert_eq!(f.headers.get("x-content-type-options").unwrap(), "nosniff");
    assert!(
        f.headers
            .get("content-security-policy")
            .unwrap()
            .to_str()
            .unwrap()
            .contains("sandbox")
    );
    assert!(
        f.headers
            .get("cache-control")
            .unwrap()
            .to_str()
            .unwrap()
            .contains("no-store")
    );
    for id in [clients_only, licensed_dl, hidden] {
        assert_eq!(
            app.get(&format!("/downloads/{id}/file")).await.status,
            StatusCode::NOT_FOUND,
            "{id}"
        );
    }
    assert_eq!(
        app.get("/client/downloads").await.status,
        StatusCode::SEE_OTHER,
        "sign-in required"
    );

    // ---- client with the product: all three visible ones
    login(&mut app, &a_email).await;
    let list = app.get("/client/downloads").await;
    assert_eq!(list.status, StatusCode::OK);
    assert!(
        list.body.contains("Kurulum kılavuzu")
            && list.body.contains("Yardımcı araçlar")
            && list.body.contains("Lisans dosyası")
    );
    assert!(list.body.contains("Kılavuzlar"), "category heading");
    assert!(!list.body.contains("Gizli dosya"));
    let f = app
        .get(&format!("/client/downloads/{licensed_dl}/file"))
        .await;
    assert_eq!(f.status, StatusCode::OK);
    assert!(f.body.contains("Lisans anahtari"));
    assert_eq!(
        app.get(&format!("/client/downloads/{hidden}/file"))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    let count = drepo::get(app.pool(), app.tenant_id, licensed_dl)
        .await
        .unwrap()
        .unwrap()
        .download_count;
    assert_eq!(count, 1);

    // ---- another client (no service of the product): no licence file
    login(&mut app, &b_email).await;
    let list = app.get("/client/downloads").await;
    assert!(list.body.contains("Yardımcı araçlar") && !list.body.contains("Lisans dosyası"));
    assert_eq!(
        app.get(&format!("/client/downloads/{licensed_dl}/file"))
            .await
            .status,
        StatusCode::NOT_FOUND
    );

    // ---- sub-users: the services area is needed
    let no_area = sub_user(&app, a_client, &["tickets"]).await;
    login(&mut app, &no_area).await;
    assert_eq!(
        app.get("/client/downloads").await.status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        app.get(&format!("/client/downloads/{licensed_dl}/file"))
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    let with_area = sub_user(&app, a_client, &["services"]).await;
    login(&mut app, &with_area).await;
    assert_eq!(
        app.get(&format!("/client/downloads/{licensed_dl}/file"))
            .await
            .status,
        StatusCode::OK
    );

    // ---- a hidden category hides its files; a suspended service loses the licence
    staff(&mut app, "admin").await;
    let csrf = csrf_of(&mut app, "/admin/downloads").await;
    let r = post(
        &mut app,
        &format!("/admin/downloads/categories/{cat}"),
        &csrf,
        &[
            ("name", "Kılavuzlar"),
            ("sort_order", "0"),
            ("is_hidden", "1"),
        ],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    sqlx::query("update services set status = 'suspended' where tenant_id = $1 and client_id = $2")
        .bind(app.tenant_id)
        .bind(a_client)
        .execute(app.pool())
        .await
        .unwrap();
    login(&mut app, &a_email).await;
    let list = app.get("/client/downloads").await;
    assert!(list.body.contains("Kurulum kılavuzu"));
    assert!(!list.body.contains("Yardımcı araçlar") && !list.body.contains("Lisans dosyası"));
    assert_eq!(
        app.get(&format!("/client/downloads/{clients_only}/file"))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    app.cleanup().await;
}
