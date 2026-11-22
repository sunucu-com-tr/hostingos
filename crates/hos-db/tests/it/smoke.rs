//! Broad smoke tests: every repository module is exercised at least once
//! against the real schema, so a typo in a runtime SQL string fails here
//! (queries are not checked at compile time).

use crate::common::{self, TestTenant, d, today};
use chrono::{Days, Utc};
use hos_core::domain::{
    DomainStatus, InvoiceStatus, Locale, ProductType, ServiceStatus, TicketPriority, TicketStatus,
    UserKind,
};
use hos_core::integrations::registrar::DnsRecordType;
use hos_core::integrations::virt::VmStatus;
use hos_core::money::{BillingCycle, Currency};
use hos_db::Page;
use hos_db::enums::*;
use hos_db::repo::*;
use serde_json::json;
use uuid::Uuid;

#[tokio::test]
async fn demo_seed_is_readable() {
    let Some(pool) = common::pool().await else {
        return;
    };
    let Some(demo) = platform::tenant_context_by_host(&pool, "localhost")
        .await
        .unwrap()
    else {
        eprintln!("demo tenant not seeded (run `hos-migrate --seed`) — skipping");
        return;
    };
    assert_eq!(demo.slug, "demo");
    let by_sub = platform::tenant_context_by_host(&pool, "demo.localhost:3000")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(by_sub.id, demo.id);
    assert_eq!(by_sub.primary_domain.as_deref(), Some("demo.localhost"));
    let t = demo.id;

    // Catalog tree with prices.
    let tree = catalog::catalog(&pool, t, Currency::TRY, false)
        .await
        .unwrap();
    assert_eq!(tree.len(), 4);
    let products: usize = tree.iter().map(|g| g.products.len()).sum();
    assert_eq!(products, 13);
    assert!(
        tree.iter()
            .flat_map(|g| &g.products)
            .all(|p| !p.prices.is_empty())
    );
    let pro = catalog::catalog_product_by_slug(&pool, t, "hosting-pro", Currency::TRY)
        .await
        .unwrap()
        .unwrap();
    let monthly = pro
        .prices
        .iter()
        .find(|p| p.billing_cycle == BillingCycle::Monthly)
        .unwrap();
    assert_eq!(monthly.price, d("169.90"));
    assert!(
        !catalog::featured_products(&pool, t, Currency::USD, 10)
            .await
            .unwrap()
            .is_empty()
    );
    let vps = catalog::product_by_slug(&pool, t, "vps-m")
        .await
        .unwrap()
        .unwrap();
    assert!(
        !catalog::product_config_options(&pool, t, vps.id, Currency::TRY)
            .await
            .unwrap()
            .is_empty()
    );

    // Domains.
    let table = domains::tld_price_table(&pool, t, Currency::TRY)
        .await
        .unwrap();
    assert_eq!(table.len(), 15);
    let comtr = domains::match_tld(&pool, t, "ornek-firma.com.tr")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(comtr.tld, "com.tr");
    let com = domains::match_tld(&pool, t, "example.com")
        .await
        .unwrap()
        .unwrap();
    let two_years = domains::tld_price(&pool, t, com.id, Currency::USD, TldAction::Register, 2)
        .await
        .unwrap();
    assert_eq!(two_years, Some(d("26.98")));

    // Content.
    let cats = content::kb_categories(&pool, t, Locale::Tr).await.unwrap();
    assert_eq!(cats.len(), 6);
    assert!(cats.iter().any(|c| c.article_count > 0));
    let art = content::published_kb_article(&pool, t, Locale::En, "changing-nameservers")
        .await
        .unwrap()
        .unwrap();
    let tr = content::translations(
        &pool,
        t,
        content::ContentTable::KbArticles,
        art.translation_group_id,
    )
    .await
    .unwrap();
    assert_eq!(tr.len(), 2);
    assert_eq!(
        content::faq_entries(&pool, t, Locale::Tr, 10)
            .await
            .unwrap()
            .len(),
        5
    );
    assert!(
        !content::search_kb(&pool, t, Locale::Tr, "nameserver", 10)
            .await
            .unwrap()
            .is_empty()
    );
    let ann = content::published_announcements(&pool, t, Locale::Tr, Page::first(10))
        .await
        .unwrap();
    assert_eq!(ann.total, 1);
    assert!(
        content::published_page(&pool, t, Locale::Tr, "hakkimizda")
            .await
            .unwrap()
            .is_some()
    );
    // Header (about) + footer (legal and company pages) links. The default
    // site pages (`seed::site`) put several pages in the footer and the demo
    // is edited by hand, so check for the seeded ones rather than a count.
    let nav = content::nav_pages(&pool, t, Locale::En).await.unwrap();
    let linked = |slug: &str| nav.iter().find(|p| p.slug == slug);
    assert!(
        linked("about-us").is_some_and(|p| p.show_in_nav),
        "about-us"
    );
    for slug in ["privacy-policy", "terms-of-service"] {
        assert!(linked(slug).is_some_and(|p| p.show_in_footer), "{slug}");
    }
    assert!(content::sitemap_entries(&pool, t).await.unwrap().len() >= 20);

    // Defaults.
    assert_eq!(mail::list_templates(&pool, t).await.unwrap().len(), 20);
    let welcome = mail::get_template(&pool, t, "welcome", Locale::En)
        .await
        .unwrap()
        .unwrap();
    assert!(welcome.body_html.contains("{{action_url}}"));
    assert_eq!(platform::list_plans(&pool, true).await.unwrap().len(), 3);
    assert!(
        platform::current_subscription(&pool, t)
            .await
            .unwrap()
            .is_some()
    );
    let roles = identity::list_roles(&pool, t).await.unwrap();
    assert_eq!(roles.len(), 5);
    let admin = roles.iter().find(|r| r.slug == "admin").unwrap();
    assert_eq!(
        identity::role_permissions(&pool, t, admin.id)
            .await
            .unwrap()
            .len(),
        // 16 + quotes.*, affiliates.*, status.manage, tasks.manage, fraud.manage (0040)
        23
    );
    assert_eq!(
        support::list_departments(&pool, t, false)
            .await
            .unwrap()
            .len(),
        5
    );
    let billing = platform::get_settings(&pool, t, "billing.").await.unwrap();
    assert!(billing.contains_key("billing.suspend_after_days"));
    assert_eq!(
        catalog::applicable_tax_rules(&pool, t, "TR", None)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        catalog::list_currencies(&pool, t, true)
            .await
            .unwrap()
            .len(),
        3
    );

    // Reports run on the seeded tenant.
    let stats = stats::dashboard(&pool, t).await.unwrap();
    assert_eq!(stats.currency, Currency::TRY);
    assert_eq!(
        stats::monthly_revenue(&pool, t, 12).await.unwrap().len(),
        12
    );
}

#[tokio::test]
async fn services_domains_and_billing_runs() {
    let Some(pool) = common::pool().await else {
        return;
    };
    let t = TestTenant::create(&pool).await;
    let client = t.client("Hizmet").await;
    let gateway = t.gateway().await;
    let tid = t.id;

    let product = catalog::create_product(
        &pool,
        tid,
        &catalog::ProductInput::new("vps-test", "VPS Test", ProductType::Vps, None),
    )
    .await
    .unwrap();
    catalog::set_product_price(
        &pool,
        tid,
        product.id,
        Currency::TRY,
        BillingCycle::Monthly,
        d("449"),
        d("0"),
    )
    .await
    .unwrap();

    // Service: pending → active, provisioning data, events.
    let start = today() - Days::new(40);
    let service = services::create_service(
        &pool,
        tid,
        &services::NewService {
            client_id: client,
            product_id: product.id,
            order_id: None,
            billing_cycle: BillingCycle::Monthly,
            first_payment_amount: d("449"),
            recurring_amount: d("449"),
            currency: Currency::TRY,
            quantity: 1,
            registration_date: start,
            next_due_date: Some(today() - Days::new(10)),
            next_invoice_date: Some(today() - Days::new(17)),
            integration_id: None,
            domain: Some("vps1.example.com".into()),
            username: None,
            config_selections: json!({}),
            custom_fields: json!({}),
        },
    )
    .await
    .unwrap();
    assert_eq!(service.status, ServiceStatus::Pending);
    assert!(
        services::transition_service(
            &pool,
            tid,
            service.id,
            ServiceStatus::Pending,
            ServiceStatus::Active,
            None
        )
        .await
        .unwrap()
    );
    // Forbidden transition is rejected by the state machine.
    assert!(
        services::transition_service(
            &pool,
            tid,
            service.id,
            ServiceStatus::Terminated,
            ServiceStatus::Active,
            None
        )
        .await
        .is_err()
    );
    services::update_provisioning(
        &pool,
        tid,
        service.id,
        &services::ProvisioningUpdate {
            integration_id: None,
            external_id: Some("vm-101".into()),
            username: Some("root".into()),
            password_enc: Some(vec![1, 2, 3]),
            server_hostname: Some("node1".into()),
            dedicated_ip: Some("192.0.2.10".parse().unwrap()),
            assigned_ips: Some(vec!["192.0.2.11".parse().unwrap()]),
            module_data: Some(json!({"node": "pve1"})),
        },
    )
    .await
    .unwrap();
    services::log_event(
        &pool,
        tid,
        service.id,
        &services::NewServiceEvent::info("provisioned", "VM created"),
    )
    .await
    .unwrap();
    assert_eq!(
        services::service_events(&pool, tid, service.id, 10)
            .await
            .unwrap()
            .len(),
        1
    );
    let list = services::list_services(
        &pool,
        tid,
        &services::ServiceFilter::default(),
        Page::first(10),
    )
    .await
    .unwrap();
    assert_eq!(list.total, 1);
    assert_eq!(
        services::client_services(&pool, tid, client)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        services::services_due_for_invoice(&pool, tid, today(), 7, 10)
            .await
            .unwrap()
            .len(),
        1
    );

    // VM + IP pool.
    let vm = services::upsert_vm(
        &pool,
        tid,
        &services::VmInput {
            service_id: Some(service.id),
            integration_id: gateway,
            node: Some("pve1".into()),
            external_id: "101".into(),
            name: "vps1".into(),
            hostname: Some("vps1.example.com".into()),
            os_template: Some("ubuntu-24.04".into()),
            cpu_cores: Some(4),
            memory_mb: Some(8192),
            disk_gb: Some(160),
            status: VmStatus::Running,
            primary_ipv4: Some("192.0.2.10".parse().unwrap()),
            primary_ipv6: None,
            metadata: json!({}),
        },
    )
    .await
    .unwrap();
    assert_eq!(
        services::vm_for_service(&pool, tid, service.id)
            .await
            .unwrap()
            .map(|v| v.id),
        Some(vm.id)
    );
    let ip_pool = services::create_ip_pool(
        &pool,
        tid,
        "TR-IST-1",
        "198.51.100.0/29".parse().unwrap(),
        Some("198.51.100.1".parse().unwrap()),
        &["1.1.1.1".parse().unwrap()],
        None,
    )
    .await
    .unwrap();
    let addrs: Vec<std::net::IpAddr> = (2..=6)
        .map(|i| format!("198.51.100.{i}").parse().unwrap())
        .collect();
    assert_eq!(
        services::add_ip_addresses(&pool, tid, ip_pool.id, &addrs)
            .await
            .unwrap(),
        5
    );
    let mut conn = pool.acquire().await.unwrap();
    let ip = services::assign_free_ip(&mut conn, tid, ip_pool.id, Some(service.id), Some(vm.id))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        ip.address,
        "198.51.100.2".parse::<std::net::IpAddr>().unwrap()
    );
    let usage = services::ip_pool_usage(&pool, tid).await.unwrap();
    assert_eq!((usage[0].total, usage[0].free), (5, 4));
    assert_eq!(
        services::release_service_ips(&pool, tid, service.id)
            .await
            .unwrap(),
        1
    );

    // An overdue renewal invoice → suspension candidate → suspended → terminate candidate.
    let mut renewal = invoices::NewInvoice {
        client_id: client,
        currency: Currency::TRY,
        issue_date: today() - Days::new(17),
        due_date: today() - Days::new(10),
        issue: true,
        payment_integration_id: None,
        notes: None,
        billing_snapshot: json!({}),
        items: vec![invoices::NewInvoiceItem::service(
            service.id,
            "VPS Test (aylık)",
            d("449"),
            today() - Days::new(10),
            None,
        )],
        taxes: vec![],
    };
    let inv = invoices::create_invoice(&mut conn, tid, &renewal)
        .await
        .unwrap();
    assert!(
        invoices::open_invoice_for_service_period(&pool, tid, service.id, today() - Days::new(10))
            .await
            .unwrap()
            .is_some()
    );
    let overdue = invoices::overdue_invoices(&pool, tid, today(), 10)
        .await
        .unwrap();
    assert_eq!(overdue.len(), 1);
    let to_suspend = services::services_to_suspend(&pool, tid, today(), 5, 10)
        .await
        .unwrap();
    assert_eq!(to_suspend.len(), 1);
    assert_eq!(to_suspend[0].overdue_invoice_id, Some(inv.id));
    assert!(
        services::services_to_suspend(&pool, tid, today(), 30, 10)
            .await
            .unwrap()
            .is_empty()
    );
    services::transition_service(
        &pool,
        tid,
        service.id,
        ServiceStatus::Active,
        ServiceStatus::Suspended,
        Some("Overdue invoice"),
    )
    .await
    .unwrap();
    assert_eq!(
        services::services_to_terminate(&pool, tid, today(), 5, 10)
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        services::services_to_terminate(&pool, tid, today(), 30, 10)
            .await
            .unwrap()
            .is_empty()
    );
    let detail = invoices::invoice_detail(&pool, tid, inv.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(detail.items.len(), 1);
    let listed = invoices::list_invoices(
        &pool,
        tid,
        &invoices::InvoiceFilter::default(),
        Page::first(10),
    )
    .await
    .unwrap();
    assert_eq!(listed.total, 1);
    invoices::record_reminder(&pool, tid, inv.id).await.unwrap();

    // Paying it lets the service be reactivated; stats see the payment.
    let paid = payments::record_payment(
        &mut conn,
        tid,
        &payments::NewPayment::gateway(
            client,
            inv.id,
            gateway,
            "stripe",
            "pi_svc",
            d("449"),
            Currency::TRY,
        ),
    )
    .await
    .unwrap();
    assert!(paid.invoice_paid);
    services::transition_service(
        &pool,
        tid,
        service.id,
        ServiceStatus::Suspended,
        ServiceStatus::Active,
        None,
    )
    .await
    .unwrap();
    assert!(
        services::services_to_terminate(&pool, tid, today(), 0, 10)
            .await
            .unwrap()
            .is_empty()
    );
    let stats = stats::dashboard(&pool, tid).await.unwrap();
    assert_eq!(stats.mrr, d("449"));
    assert_eq!(stats.revenue_this_month, d("449"));
    assert_eq!(stats.active_services, 1);
    let rev = stats::monthly_revenue(&pool, tid, 3).await.unwrap();
    assert_eq!(rev.last().unwrap().amount, d("449"));

    // Credits and refunds.
    payments::adjust_credit(
        &mut conn,
        tid,
        client,
        d("50"),
        Currency::TRY,
        CreditKind::Promo,
        "Goodwill",
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        payments::credit_balance(&pool, tid, client, Currency::TRY)
            .await
            .unwrap(),
        d("50")
    );
    renewal.items = vec![invoices::NewInvoiceItem::manual("Ek hizmet", d("30"))];
    renewal.due_date = today();
    renewal.issue_date = today();
    let small = invoices::create_invoice(&mut conn, tid, &renewal)
        .await
        .unwrap();
    let (after_credit, used) =
        payments::apply_credit_to_invoice(&mut conn, tid, small.id, None, None)
            .await
            .unwrap();
    assert_eq!((after_credit.status, used), (InvoiceStatus::Paid, d("30")));
    assert_eq!(
        payments::credit_balance(&pool, tid, client, Currency::TRY)
            .await
            .unwrap(),
        d("20")
    );
    let refund = payments::create_refund(
        &mut conn,
        tid,
        paid.transaction.id,
        d("49"),
        Some("partial"),
        false,
        None,
    )
    .await
    .unwrap();
    payments::complete_refund(&mut conn, tid, refund.id, true, Some("re_1"), None)
        .await
        .unwrap();
    assert_eq!(
        payments::transaction_refunds(&pool, tid, paid.transaction.id)
            .await
            .unwrap()
            .len(),
        1
    );
    // A paid invoice cannot be cancelled.
    assert!(
        !invoices::cancel_invoice(&pool, tid, small.id)
            .await
            .unwrap()
    );

    // Domains: create, register, expire soon, DNS.
    let domain = domains::create_domain(
        &pool,
        tid,
        &domains::NewDomain {
            client_id: client,
            order_id: None,
            name: "Ornek-Firma.com.tr".into(),
            tld: "com.tr".into(),
            registrar_integration_id: None,
            registration_type: DomainRegistrationType::Register,
            registration_period: 1,
            first_payment_amount: d("249"),
            recurring_amount: d("249"),
            currency: Currency::TRY,
            privacy: false,
            nameservers: vec!["ns1.example.net".into(), "ns2.example.net".into()],
            epp_code_enc: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(domain.name, "ornek-firma.com.tr");
    assert!(
        domains::domain_by_name(&pool, tid, "ORNEK-FIRMA.com.tr")
            .await
            .unwrap()
            .is_some()
    );
    domains::transition_domain(
        &pool,
        tid,
        domain.id,
        DomainStatus::Pending,
        DomainStatus::PendingRegistration,
    )
    .await
    .unwrap();
    domains::transition_domain(
        &pool,
        tid,
        domain.id,
        DomainStatus::PendingRegistration,
        DomainStatus::Active,
    )
    .await
    .unwrap();
    let expires = Utc::now() + chrono::Duration::days(20);
    domains::update_registration(
        &pool,
        tid,
        domain.id,
        Some("ord-1"),
        Some(Utc::now()),
        Some(expires),
        Some(expires.date_naive() - Days::new(14)),
        None,
    )
    .await
    .unwrap();
    domains::set_domain_flags(&pool, tid, domain.id, true, false, true)
        .await
        .unwrap();
    domains::set_epp_code(&pool, tid, domain.id, Some(b"enc"))
        .await
        .unwrap();
    domains::record_sync_error(&pool, tid, domain.id, "registry timeout")
        .await
        .unwrap();
    assert_eq!(
        domains::domains_expiring(&pool, tid, 30, true)
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        domains::domains_expiring(&pool, tid, 10, false)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        domains::domains_due_for_invoice(&pool, tid, today(), 7, 10)
            .await
            .unwrap()
            .len(),
        1
    );
    let listed = domains::list_domains(
        &pool,
        tid,
        &domains::DomainFilter::default(),
        Page::first(5),
    )
    .await
    .unwrap();
    assert_eq!(listed.total, 1);
    assert_eq!(
        domains::client_domains(&pool, tid, client)
            .await
            .unwrap()
            .len(),
        1
    );
    let contact = domains::DomainContactInput {
        first_name: "Ayşe".into(),
        last_name: "Yılmaz".into(),
        organization: None,
        email: "ayse@example.com".into(),
        phone: Some("+90.2120000000".into()),
        address_line1: Some("Levent Mah.".into()),
        address_line2: None,
        city: Some("İstanbul".into()),
        state: None,
        postal_code: Some("34330".into()),
        country_code: "TR".into(),
        external_contact_id: None,
    };
    domains::upsert_contact(&pool, tid, domain.id, ContactRole::Registrant, &contact)
        .await
        .unwrap();
    domains::upsert_contact(&pool, tid, domain.id, ContactRole::Registrant, &contact)
        .await
        .unwrap();
    assert_eq!(
        domains::domain_contacts(&pool, tid, domain.id)
            .await
            .unwrap()
            .len(),
        1
    );
    let zone = domains::create_zone(
        &pool,
        tid,
        "ornek-firma.com.tr.",
        Some(domain.id),
        Some(client),
        None,
    )
    .await
    .unwrap();
    let rec = domains::add_record(
        &pool,
        tid,
        zone.id,
        "@",
        DnsRecordType::A,
        "192.0.2.10",
        3600,
        None,
    )
    .await
    .unwrap();
    domains::add_record(
        &pool,
        tid,
        zone.id,
        "@",
        DnsRecordType::Mx,
        "mail.ornek-firma.com.tr",
        3600,
        Some(10),
    )
    .await
    .unwrap();
    assert!(
        domains::zone_by_name(&pool, tid, "Ornek-Firma.com.tr")
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        domains::delete_record(&pool, tid, zone.id, rec.id)
            .await
            .unwrap()
    );
    assert_eq!(
        domains::zone_records(&pool, tid, zone.id)
            .await
            .unwrap()
            .len(),
        1
    );

    // TLD admin.
    let tld = domains::upsert_tld(&pool, tid, &domains::TldInput::new("dev"))
        .await
        .unwrap();
    let prices = [domains::TldPriceInput {
        currency: Currency::TRY,
        action: TldAction::Register,
        years: 1,
        price: d("699"),
    }];
    assert_eq!(
        domains::set_tld_prices(&pool, tid, tld.id, &prices)
            .await
            .unwrap(),
        1
    );
    assert_eq!(domains::list_tlds(&pool, tid, true).await.unwrap().len(), 1);

    drop(conn);
    t.cleanup().await;
}

#[tokio::test]
async fn identity_support_content_and_system_runs() {
    let Some(pool) = common::pool().await else {
        return;
    };
    let t = TestTenant::create(&pool).await;
    let tid = t.id;
    let client = t.client("Destek").await;
    let integration = t.gateway().await;

    // Identity: users, OAuth, sessions, RBAC, TOTP, tokens, API keys.
    let staff = identity::create_user(
        &pool,
        tid,
        &identity::NewUser {
            email: "staff@example.com".into(),
            password_hash: Some("$argon2id$v=19$m=19456,t=2,p=1$c2FsdA$aGFzaA".into()),
            kind: UserKind::Staff,
            first_name: "Mehmet".into(),
            last_name: "Kaya".into(),
            phone: None,
            locale: Locale::Tr,
            email_verified: true,
        },
    )
    .await
    .unwrap();
    let support_role = identity::role_by_slug(&pool, tid, "support")
        .await
        .unwrap()
        .unwrap();
    identity::assign_role(&pool, tid, staff.id, support_role.id)
        .await
        .unwrap();
    identity::assign_role(&pool, tid, staff.id, support_role.id)
        .await
        .unwrap();
    let perms = identity::user_permissions(&pool, tid, staff.id)
        .await
        .unwrap();
    assert!(
        perms.contains(&"support.manage".to_owned())
            && !perms.contains(&"billing.manage".to_owned())
    );
    let custom = identity::create_role(&pool, tid, "auditor", "Auditor", None, false)
        .await
        .unwrap();
    let mut conn = pool.acquire().await.unwrap();
    identity::set_role_permissions(
        &mut conn,
        tid,
        custom.id,
        &["reports.view".into(), "billing.*".into()],
    )
    .await
    .unwrap();
    assert!(
        identity::set_role_permissions(&mut conn, tid, custom.id, &["Not Valid".into()])
            .await
            .is_err()
    );
    assert_eq!(
        identity::role_permissions(&pool, tid, custom.id)
            .await
            .unwrap()
            .len(),
        2
    );
    assert_eq!(identity::list_staff(&pool, tid).await.unwrap().len(), 1);
    let users = identity::list_users(
        &pool,
        tid,
        &identity::UserFilter::default(),
        Page::first(10),
    )
    .await
    .unwrap();
    assert_eq!(users.total, 1);
    identity::record_login_success(&pool, tid, staff.id, Some("203.0.113.9".parse().unwrap()))
        .await
        .unwrap();
    identity::record_login_failure(&pool, tid, staff.id, 5, 900)
        .await
        .unwrap();

    let profile = identity::OAuthProfile {
        provider: OAuthProvider::Github,
        provider_user_id: "12345".into(),
        email: Some("staff@example.com".into()),
        display_name: Some("mkaya".into()),
        avatar_url: None,
        raw: json!({}),
    };
    identity::link_identity(&pool, tid, staff.id, &profile)
        .await
        .unwrap();
    let via = identity::user_by_identity(&pool, tid, OAuthProvider::Github, "12345")
        .await
        .unwrap();
    assert_eq!(via.map(|u| u.id), Some(staff.id));

    let token = format!("tok-{}", common::random_suffix());
    let session = identity::create_session(
        &pool,
        &identity::NewSession {
            owner: identity::SessionOwner::User {
                tenant_id: tid,
                user_id: staff.id,
            },
            token_hash: token.clone(),
            csrf_token: "csrf".into(),
            expires_at: Utc::now() + chrono::Duration::hours(1),
            ip: Some("203.0.113.9".parse().unwrap()),
            user_agent: Some("tests".into()),
            mfa_verified: false,
            impersonator: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        identity::validate_session(&pool, Some(tid), &token)
            .await
            .unwrap()
            .map(|s| s.id),
        Some(session.id)
    );
    // Another tenant's host cannot use this session.
    let other = TestTenant::create(&pool).await;
    assert!(
        identity::validate_session(&pool, Some(other.id), &token)
            .await
            .unwrap()
            .is_none()
    );
    other.cleanup().await;
    identity::set_session_mfa_verified(&pool, session.id)
        .await
        .unwrap();
    assert_eq!(
        identity::list_user_sessions(&pool, tid, staff.id)
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(identity::delete_session(&pool, &token).await.unwrap());
    assert!(
        identity::validate_session(&pool, Some(tid), &token)
            .await
            .unwrap()
            .is_none()
    );

    identity::upsert_totp_secret(
        &pool,
        tid,
        staff.id,
        b"secret",
        &["rc1".into(), "rc2".into()],
    )
    .await
    .unwrap();
    assert!(identity::confirm_totp(&pool, tid, staff.id).await.unwrap());
    assert!(
        identity::use_totp_step(&pool, tid, staff.id, 1000)
            .await
            .unwrap()
    );
    assert!(
        !identity::use_totp_step(&pool, tid, staff.id, 1000)
            .await
            .unwrap(),
        "replayed TOTP step"
    );
    assert!(
        identity::use_recovery_code(&pool, tid, staff.id, "rc1")
            .await
            .unwrap()
    );
    assert!(
        !identity::use_recovery_code(&pool, tid, staff.id, "rc1")
            .await
            .unwrap()
    );

    let expiry = Utc::now() + chrono::Duration::minutes(60);
    identity::create_password_reset(&pool, tid, staff.id, "reset-hash", expiry, None)
        .await
        .unwrap();
    assert_eq!(
        identity::consume_password_reset(&pool, tid, "reset-hash")
            .await
            .unwrap(),
        Some(staff.id)
    );
    assert_eq!(
        identity::consume_password_reset(&pool, tid, "reset-hash")
            .await
            .unwrap(),
        None
    );
    identity::create_email_verification(
        &pool,
        tid,
        staff.id,
        "staff@example.com",
        "verify-hash",
        expiry,
    )
    .await
    .unwrap();
    assert!(
        identity::consume_email_verification(&pool, tid, "verify-hash")
            .await
            .unwrap()
            .is_some()
    );
    let key = identity::create_api_key(
        &pool,
        tid,
        Some(staff.id),
        "CI",
        "hos_live_ab",
        "key-hash",
        &[],
        None,
    )
    .await
    .unwrap();
    assert!(
        identity::api_key_by_hash(&pool, tid, "key-hash")
            .await
            .unwrap()
            .is_some()
    );
    identity::touch_api_key(&pool, key.id, None).await.unwrap();
    assert!(identity::revoke_api_key(&pool, tid, key.id).await.unwrap());

    // Support: open → staff reply (answered) → client reply (customer_reply).
    let dept = support::department_by_slug(&pool, tid, "teknik-destek")
        .await
        .unwrap()
        .unwrap();
    let msg = |kind, body: &str, internal| support::NewTicketMessage {
        author_kind: kind,
        user_id: None,
        author_name: "Test".into(),
        author_email: Some("client@example.com".into()),
        body: body.into(),
        is_internal: internal,
        source: MessageSource::Web,
        message_id: None,
        ip: None,
    };
    let (ticket, _) = support::open_ticket(
        &mut conn,
        tid,
        &support::NewTicket {
            department_id: dept.id,
            client_id: Some(client),
            user_id: None,
            guest_name: None,
            guest_email: None,
            subject: "SSL kurulumu".into(),
            priority: TicketPriority::Medium,
            service_id: None,
            domain_id: None,
            first_message: msg(AuthorKind::Client, "Merhaba, SSL kurulmadı.", false),
        },
    )
    .await
    .unwrap();
    assert_eq!(ticket.status, TicketStatus::Open);
    let (_, ticket) = support::reply_to_ticket(
        &mut conn,
        tid,
        ticket.id,
        &msg(AuthorKind::Staff, "Kuruldu.", false),
    )
    .await
    .unwrap();
    assert_eq!(ticket.status, TicketStatus::Answered);
    let (_, ticket) = support::reply_to_ticket(
        &mut conn,
        tid,
        ticket.id,
        &msg(AuthorKind::Staff, "not", true),
    )
    .await
    .unwrap();
    assert_eq!(ticket.status, TicketStatus::Answered);
    let (_, ticket) = support::reply_to_ticket(
        &mut conn,
        tid,
        ticket.id,
        &msg(AuthorKind::Client, "Teşekkürler", false),
    )
    .await
    .unwrap();
    assert_eq!(ticket.status, TicketStatus::CustomerReply);
    assert_eq!(
        support::ticket_messages(&pool, tid, ticket.id, false)
            .await
            .unwrap()
            .len(),
        3
    );
    assert_eq!(
        support::ticket_messages(&pool, tid, ticket.id, true)
            .await
            .unwrap()
            .len(),
        4
    );
    assert!(
        support::ticket_by_number(&pool, tid, &ticket.number)
            .await
            .unwrap()
            .is_some()
    );
    let filter = support::TicketFilter {
        awaiting_staff: true,
        ..Default::default()
    };
    assert_eq!(
        support::list_tickets(&pool, tid, &filter, Page::first(10))
            .await
            .unwrap()
            .total,
        1
    );
    support::assign_ticket(&pool, tid, ticket.id, Some(staff.id))
        .await
        .unwrap();
    support::update_ticket_routing(&pool, tid, ticket.id, TicketPriority::High, dept.id)
        .await
        .unwrap();
    support::add_attachment(
        &pool,
        tid,
        ticket.id,
        None,
        "log.txt",
        "text/plain",
        12,
        "k/log.txt",
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        support::ticket_attachments(&pool, tid, ticket.id)
            .await
            .unwrap()
            .len(),
        1
    );
    support::create_canned_response(&pool, tid, "Selam", "Merhaba,", Locale::Tr, Some(staff.id))
        .await
        .unwrap();
    assert_eq!(
        support::list_canned_responses(&pool, tid, Some(Locale::Tr))
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        support::set_ticket_status(&pool, tid, ticket.id, TicketStatus::Closed)
            .await
            .unwrap()
    );
    assert_eq!(
        support::close_inactive_tickets(&pool, tid, 72)
            .await
            .unwrap(),
        0
    );

    // Content.
    let cat = content::create_kb_category(&pool, tid, None, "genel", "Genel", None, &json!({}), 0)
        .await
        .unwrap();
    let group = Uuid::now_v7();
    for (locale, slug) in [(Locale::Tr, "ilk-adimlar"), (Locale::En, "first-steps")] {
        content::create_kb_article(
            &pool,
            tid,
            &content::KbArticleInput {
                category_id: Some(cat.id),
                kind: KbArticleKind::Article,
                slug: slug.into(),
                locale,
                translation_group_id: Some(group),
                title: "İlk adımlar".into(),
                excerpt: None,
                body: "Panel".into(),
                status: ContentStatus::Published,
                published_at: Some(Utc::now()),
                tags: vec!["panel".into()],
                is_featured: false,
                sort_order: 0,
                author_id: Some(staff.id),
            },
        )
        .await
        .unwrap();
    }
    let art = content::published_kb_article(&pool, tid, Locale::Tr, "ilk-adimlar")
        .await
        .unwrap()
        .unwrap();
    content::increment_kb_views(&pool, tid, art.id)
        .await
        .unwrap();
    content::vote_kb_article(&pool, tid, art.id, true)
        .await
        .unwrap();
    let paged =
        content::published_kb_articles(&pool, tid, Locale::En, Some(cat.id), None, Page::first(10))
            .await
            .unwrap();
    assert_eq!(paged.total, 1);
    let post_input = content::BlogPostInput {
        slug: "nvme-nedir".into(),
        locale: Locale::Tr,
        translation_group_id: None,
        title: "NVMe nedir?".into(),
        excerpt: None,
        body: "Hızlı disk".into(),
        cover_image_url: None,
        category: Some("teknoloji".into()),
        tags: vec!["donanim".into()],
        status: ContentStatus::Scheduled,
        published_at: Some(Utc::now() - chrono::Duration::minutes(1)),
        reading_minutes: Some(3),
        author_id: Some(staff.id),
        author_name: None,
    };
    let post = content::create_blog_post(&pool, tid, &post_input)
        .await
        .unwrap();
    // A scheduled post is public once due, even before the cron flips it.
    assert!(
        content::published_blog_post(&pool, tid, Locale::Tr, "nvme-nedir")
            .await
            .unwrap()
            .is_some()
    );
    let mut future = post_input.clone();
    future.slug = "gelecek-yazi".into();
    future.published_at = Some(Utc::now() + chrono::Duration::days(1));
    content::create_blog_post(&pool, tid, &future)
        .await
        .unwrap();
    assert!(
        content::published_blog_post(&pool, tid, Locale::Tr, "gelecek-yazi")
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(content::publish_scheduled(&pool, tid).await.unwrap(), 1);
    assert!(
        content::published_blog_post(&pool, tid, Locale::Tr, "nvme-nedir")
            .await
            .unwrap()
            .is_some()
    );
    let tagged =
        content::published_blog_posts(&pool, tid, Locale::Tr, Some("donanim"), Page::first(5))
            .await
            .unwrap();
    assert_eq!(tagged.total, 1);
    content::update_blog_post(&pool, tid, post.id, &post_input)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        content::list_blog_posts(&pool, tid, None, Page::first(5))
            .await
            .unwrap()
            .total,
        2
    );
    content::create_announcement(
        &pool,
        tid,
        "bakim",
        Locale::Tr,
        None,
        "Bakım",
        None,
        "Planlı bakım",
        ContentStatus::Published,
        Some(Utc::now()),
    )
    .await
    .unwrap();
    assert!(
        content::published_announcement(&pool, tid, Locale::Tr, "bakim")
            .await
            .unwrap()
            .is_some()
    );
    let page_input = content::PageInput {
        slug: "kurumsal/hakkimizda".into(),
        locale: Locale::Tr,
        translation_group_id: None,
        title: "Hakkımızda".into(),
        body: "Biz".into(),
        template: "default".into(),
        status: ContentStatus::Published,
        published_at: None,
        show_in_nav: true,
        show_in_footer: false,
        nav_order: 1,
        author_id: None,
    };
    // New tenants start with the default site pages (`seed::site`): count
    // only what this test adds.
    let seeded = content::list_pages(&pool, tid).await.unwrap().len();
    let page = content::create_page(&pool, tid, &page_input).await.unwrap();
    content::update_page(&pool, tid, page.id, &page_input)
        .await
        .unwrap()
        .unwrap();
    let pages = content::list_pages(&pool, tid).await.unwrap();
    assert_eq!(pages.len(), seeded + 1);
    assert_eq!(pages.iter().filter(|p| p.id == page.id).count(), 1);
    assert!(
        content::published_page(&pool, tid, Locale::Tr, "kurumsal/hakkimizda")
            .await
            .unwrap()
            .is_some()
    );
    content::upsert_redirect(&pool, tid, "/eski", "/yeni", 301, None)
        .await
        .unwrap();
    let redirect = content::hit_redirect(&pool, tid, "/eski")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(redirect.to_url, "/yeni");
    assert_eq!(content::list_redirects(&pool, tid).await.unwrap().len(), 1);

    // SEO meta (upsert twice = update), audits, AI generations.
    let target = seo::SeoTarget::Entity(SeoEntityType::BlogPost, post.id.into_uuid());
    let mut meta = seo::SeoMetaInput {
        title: Some("NVMe".into()),
        description: None,
        canonical: None,
        keywords: vec![],
        og: None,
        jsonld: Some(json!({"@type": "BlogPosting"})),
        noindex: false,
        nofollow: false,
    };
    seo::upsert_seo_meta(&pool, tid, &target, Locale::Tr, &meta)
        .await
        .unwrap();
    meta.title = Some("NVMe nedir?".into());
    seo::upsert_seo_meta(&pool, tid, &seo::SeoTarget::Home, Locale::Tr, &meta)
        .await
        .unwrap();
    seo::upsert_seo_meta(&pool, tid, &seo::SeoTarget::Home, Locale::Tr, &meta)
        .await
        .unwrap();
    seo::upsert_seo_meta(&pool, tid, &target, Locale::Tr, &meta)
        .await
        .unwrap();
    assert_eq!(seo::list_seo_meta(&pool, tid, None).await.unwrap().len(), 2);
    let got = seo::get_seo_meta(&pool, tid, &target, Locale::Tr)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got.title.as_deref(), Some("NVMe nedir?"));
    assert!(
        seo::delete_seo_meta(&pool, tid, &seo::SeoTarget::Home, Locale::Tr)
            .await
            .unwrap()
    );
    let audit = seo::create_seo_audit(
        &pool,
        tid,
        "https://demo.localhost/",
        None,
        Some(Locale::Tr),
    )
    .await
    .unwrap();
    seo::start_seo_audit(&pool, tid, audit.id).await.unwrap();
    assert!(
        seo::finish_seo_audit(
            &pool,
            tid,
            audit.id,
            Some(92),
            &json!([]),
            Some("iyi"),
            None
        )
        .await
        .unwrap()
    );
    assert_eq!(
        seo::recent_seo_audits(&pool, tid, 5).await.unwrap()[0].score,
        Some(92)
    );
    let ai = seo::create_ai_generation(
        &pool,
        tid,
        &seo::NewAiGeneration {
            user_id: Some(staff.id),
            integration_id: None,
            purpose: AiPurpose::SeoMeta,
            entity: Some(("blog_post".into(), post.id.into_uuid())),
            locale: Some(Locale::Tr),
            model: "claude-opus-5-5".into(),
            prompt: "Write meta".into(),
            input: json!({}),
        },
    )
    .await
    .unwrap();
    seo::complete_ai_generation(
        &pool,
        tid,
        ai.id,
        "meta",
        None,
        Some(100),
        Some(20),
        Some((d("0.01"), Currency::USD)),
    )
    .await
    .unwrap();
    assert_eq!(
        seo::ai_usage_since(&pool, tid, Utc::now() - chrono::Duration::hours(1))
            .await
            .unwrap()
            .0,
        1
    );
    assert_eq!(
        seo::list_ai_generations(&pool, tid, None, Page::first(5))
            .await
            .unwrap()
            .total,
        1
    );

    // Mail: tenant override, locale fallback, log.
    let input = mail::EmailTemplateInput {
        key: "welcome".into(),
        locale: Locale::Tr,
        category: EmailCategory::Client,
        name: "Hoş geldiniz".into(),
        subject: "Özel".into(),
        body_html: "<p>Merhaba</p>".into(),
        body_text: "Merhaba".into(),
        is_system: true,
    };
    assert!(
        mail::upsert_template(&pool, tid, &input, false)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        mail::upsert_template(&pool, tid, &input, true)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        mail::get_template(&pool, tid, "welcome", Locale::Tr)
            .await
            .unwrap()
            .unwrap()
            .subject,
        "Özel"
    );
    let log = mail::log_email(
        &pool,
        tid,
        &mail::NewEmailLog {
            user_id: None,
            client_id: Some(client),
            template_key: Some("welcome".into()),
            to_address: "client@example.com".into(),
            cc: vec![],
            bcc: vec![],
            from_address: None,
            subject: "Özel".into(),
            body_html: None,
            body_text: Some("Merhaba".into()),
            related: None,
        },
    )
    .await
    .unwrap();
    assert!(
        mail::mark_email_sent(&pool, log, Some("msg-1"))
            .await
            .unwrap()
    );
    assert_eq!(
        mail::list_email_log(&pool, tid, Some(client), Page::first(5))
            .await
            .unwrap()
            .total,
        1
    );

    // Webhooks: the same provider event is stored once.
    let (hook, new) = webhooks::record_incoming(
        &pool,
        tid,
        integration,
        "evt_1",
        Some("payment"),
        &json!({}),
        &json!({}),
        true,
    )
    .await
    .unwrap();
    assert!(new);
    let (again, new) = webhooks::record_incoming(
        &pool,
        tid,
        integration,
        "evt_1",
        Some("payment"),
        &json!({}),
        &json!({}),
        true,
    )
    .await
    .unwrap();
    assert!(!new);
    assert_eq!(again.id, hook.id);
    assert_eq!(
        webhooks::pending_webhooks(&pool, tid, 5, 10)
            .await
            .unwrap()
            .len(),
        1
    );
    webhooks::set_webhook_status(&pool, tid, hook.id, WebhookInStatus::Processed, None)
        .await
        .unwrap();
    assert!(
        webhooks::pending_webhooks(&pool, tid, 5, 10)
            .await
            .unwrap()
            .is_empty()
    );
    let ep = webhooks::create_endpoint(
        &pool,
        tid,
        "https://hooks.example.com/x",
        None,
        None,
        &["invoice.paid".into()],
    )
    .await
    .unwrap();
    assert_eq!(
        webhooks::endpoints_for_event(&pool, tid, "invoice.paid")
            .await
            .unwrap()
            .len(),
        1
    );
    webhooks::record_delivery(&pool, tid, ep.id, Some(200), true)
        .await
        .unwrap();

    // Notifications + audit log.
    notifications::notify(
        &pool,
        tid,
        staff.id,
        "ticket",
        "Yeni yanıt",
        None,
        None,
        &json!({}),
    )
    .await
    .unwrap();
    assert_eq!(
        notifications::unread_count(&pool, tid, staff.id)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        notifications::mark_all_read(&pool, tid, staff.id)
            .await
            .unwrap(),
        1
    );
    audit::write(
        &pool,
        &audit::AuditEntry::new(
            Some(tid),
            ActorKind::User,
            Some(staff.id.into_uuid()),
            "ticket.close",
        )
        .entity("ticket", ticket.id)
        .changes(json!({"status": ["customer_reply", "closed"]})),
    )
    .await
    .unwrap();
    let entries = audit::list(&pool, tid, &audit::AuditFilter::default(), Page::first(5))
        .await
        .unwrap();
    assert_eq!(entries.total, 1);

    // Cart → lines with list prices.
    let product = catalog::create_product(
        &pool,
        tid,
        &catalog::ProductInput::new("web-pro", "Web Pro", ProductType::SharedHosting, None),
    )
    .await
    .unwrap();
    catalog::set_product_price(
        &pool,
        tid,
        product.id,
        Currency::TRY,
        BillingCycle::Annual,
        d("1699"),
        d("0"),
    )
    .await
    .unwrap();
    let tld = domains::upsert_tld(&pool, tid, &domains::TldInput::new("com.tr"))
        .await
        .unwrap();
    domains::set_tld_prices(
        &pool,
        tid,
        tld.id,
        &[domains::TldPriceInput {
            currency: Currency::TRY,
            action: TldAction::Register,
            years: 1,
            price: d("249"),
        }],
    )
    .await
    .unwrap();
    let cart = carts::create_cart(&pool, tid, Currency::TRY, None, None, Some("cart-token"))
        .await
        .unwrap();
    carts::add_item(
        &pool,
        tid,
        cart.id,
        &carts::NewCartItem::product(product.id, BillingCycle::Annual),
    )
    .await
    .unwrap();
    carts::add_item(
        &pool,
        tid,
        cart.id,
        &carts::NewCartItem::domain(LineItemType::DomainRegister, "yeni-firma.com.tr", 1),
    )
    .await
    .unwrap();
    let lines = carts::cart_lines(&pool, tid, cart.id).await.unwrap();
    assert_eq!(lines.len(), 2);
    let mut prices: Vec<_> = lines.iter().map(|l| l.unit_price).collect();
    prices.sort();
    assert_eq!(prices, vec![Some(d("249")), Some(d("1699"))]);
    assert!(
        carts::cart_by_token(&pool, tid, "cart-token")
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        carts::assign_cart_owner(&pool, tid, cart.id, staff.id, Some(client))
            .await
            .unwrap()
    );
    assert!(
        carts::open_cart_for_user(&pool, tid, staff.id)
            .await
            .unwrap()
            .is_some()
    );

    drop(conn);
    t.cleanup().await;
}
