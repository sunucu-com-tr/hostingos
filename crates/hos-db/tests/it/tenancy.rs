//! Tenant isolation: the same natural keys can exist in two tenants and
//! nothing of one tenant is visible through the other's scope.

use crate::common::{self, TestTenant, d, today};
use hos_core::domain::{InvoiceStatus, Locale, UserKind};
use hos_core::money::Currency;
use hos_db::Page;
use hos_db::enums::TenantDomainKind;
use hos_db::repo::{clients, identity, invoices, platform, stats};
use serde_json::json;

fn new_user(email: &str) -> identity::NewUser {
    identity::NewUser {
        email: email.to_owned(),
        password_hash: None,
        kind: UserKind::Client,
        first_name: "Ada".into(),
        last_name: "Lovelace".into(),
        phone: None,
        locale: Locale::Tr,
        email_verified: false,
    }
}

#[tokio::test]
async fn tenants_are_isolated() {
    let Some(pool) = common::pool().await else {
        return;
    };
    let a = TestTenant::create(&pool).await;
    let b = TestTenant::create(&pool).await;

    // Same client e-mail in both tenants is fine; each sees only its own.
    let ca = a.client("Ayse").await;
    let cb = b.client("Ayse").await;
    assert!(
        clients::get_client(&pool, a.id, ca)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        clients::get_client(&pool, b.id, ca)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        clients::get_client(&pool, a.id, cb)
            .await
            .unwrap()
            .is_none()
    );
    let list = clients::list_clients(
        &pool,
        a.id,
        &clients::ClientFilter::default(),
        Page::first(50),
    )
    .await
    .unwrap();
    assert_eq!(list.total, 1);
    assert_eq!(list.items[0].client.id, ca);

    // Same user e-mail in both tenants; duplicate inside one tenant → Conflict
    // (citext: case-insensitive).
    let ua = identity::create_user(&pool, a.id, &new_user("ada@example.com"))
        .await
        .unwrap();
    let ub = identity::create_user(&pool, b.id, &new_user("ada@example.com"))
        .await
        .unwrap();
    assert_ne!(ua.id, ub.id);
    let dup = identity::create_user(&pool, a.id, &new_user("ADA@example.com")).await;
    assert!(
        matches!(dup, Err(hos_core::CoreError::Conflict(_))),
        "{dup:?}"
    );
    let found = identity::user_by_email(&pool, b.id, "Ada@Example.com")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(found.id, ub.id);
    assert!(
        identity::get_user(&pool, b.id, ua.id)
            .await
            .unwrap()
            .is_none()
    );

    // RBAC is per tenant: the owner role of A cannot be assigned in B.
    let owner_a = identity::role_by_slug(&pool, a.id, "owner")
        .await
        .unwrap()
        .unwrap();
    identity::assign_role(&pool, a.id, ua.id, owner_a.id)
        .await
        .unwrap();
    assert_eq!(
        identity::user_permissions(&pool, a.id, ua.id)
            .await
            .unwrap(),
        vec!["*".to_owned()]
    );
    assert!(
        identity::user_permissions(&pool, b.id, ua.id)
            .await
            .unwrap()
            .is_empty()
    );

    // Invoices: numbering and visibility are per tenant.
    let mut conn = pool.acquire().await.unwrap();
    let new_invoice = |client| invoices::NewInvoice {
        client_id: client,
        currency: Currency::TRY,
        issue_date: today(),
        due_date: today(),
        issue: true,
        payment_integration_id: None,
        notes: None,
        billing_snapshot: json!({}),
        items: vec![invoices::NewInvoiceItem::manual("Hosting", d("100"))],
        taxes: vec![],
    };
    let ia = invoices::create_invoice(&mut conn, a.id, &new_invoice(ca))
        .await
        .unwrap();
    let ib = invoices::create_invoice(&mut conn, b.id, &new_invoice(cb))
        .await
        .unwrap();
    assert_eq!(ia.seq, Some(1));
    assert_eq!(ib.seq, Some(1));
    assert_eq!(ia.status, InvoiceStatus::Unpaid);
    assert!(
        invoices::get_invoice(&pool, b.id, ia.id)
            .await
            .unwrap()
            .is_none()
    );
    // A client of tenant B cannot be invoiced in tenant A.
    assert!(
        invoices::create_invoice(&mut conn, a.id, &new_invoice(cb))
            .await
            .is_err()
    );

    let sa = stats::dashboard(&pool, a.id).await.unwrap();
    assert_eq!((sa.active_clients, sa.unpaid_invoices), (1, 1));
    assert_eq!(sa.unpaid_total, d("100"));

    // Host → tenant resolution: subdomains resolve immediately, custom
    // domains only after verification.
    let sub = format!("{}.hostingos.test", a.slug);
    let custom = format!("www.{}.example", b.slug);
    platform::add_tenant_domain(
        &mut conn,
        a.id,
        &sub,
        TenantDomainKind::Subdomain,
        true,
        false,
    )
    .await
    .unwrap();
    let dom = platform::add_tenant_domain(
        &mut conn,
        b.id,
        &custom,
        TenantDomainKind::Custom,
        false,
        false,
    )
    .await
    .unwrap();
    let ctx = platform::tenant_context_by_host(&pool, &sub.to_uppercase())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ctx.id, a.id);
    assert!(
        platform::tenant_context_by_host(&pool, &custom)
            .await
            .unwrap()
            .is_none()
    );
    platform::mark_tenant_domain_verified(&pool, b.id, dom.id)
        .await
        .unwrap();
    let ctx = platform::tenant_context_by_host(&pool, &format!("{custom}:443"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ctx.id, b.id);
    // A host belongs to exactly one tenant.
    assert!(
        platform::add_tenant_domain(
            &mut conn,
            a.id,
            &custom,
            TenantDomainKind::Custom,
            false,
            false
        )
        .await
        .is_err()
    );
    drop(conn);

    a.cleanup().await;
    // B is untouched by A's deletion.
    assert!(
        clients::get_client(&pool, b.id, cb)
            .await
            .unwrap()
            .is_some()
    );
    b.cleanup().await;
}
