//! VPS management rules of `hos_billing::vm_ops` against the real database
//! and the mock hypervisor: per-product snapshot limit, ownership of every id
//! taken from a request (another client's VM, snapshot, IP, SSH key), ISO
//! allow-list, suspended services, password-reset limit (nothing stored),
//! one-time console tokens and the per-user session cap, busy VMs.

use crate::common::{Env, suffix};
use hos_billing::provisioning::{ProvisionOutcome, RunContext};
use hos_billing::vm_ops::ConsoleRequester;
use hos_core::CoreError;
use hos_core::domain::{Locale, ProductType, UserKind};
use hos_core::error::IntegrationError;
use hos_core::ids::{ClientId, IntegrationId, ServiceId, UserId};
use hos_core::integrations::virt::PowerAction;
use hos_core::money::BillingCycle;
use hos_db::repo::carts::NewCartItem;
use hos_db::repo::identity::{self, NewUser};
use hos_db::repo::services;
use serde_json::json;

/// An active VPS of a new client (product `module_config` extended by `extra`).
async fn vps(
    env: &Env,
    gw: IntegrationId,
    virt: IntegrationId,
    name: &str,
    extra: serde_json::Value,
) -> (ClientId, ServiceId) {
    let mut config =
        json!({"template": "ubuntu-24.04", "cpu_cores": 1, "memory_mb": 1024, "disk_gb": 40});
    if let (Some(c), Some(e)) = (config.as_object_mut(), extra.as_object()) {
        c.extend(e.clone());
    }
    let product = env
        .product("VPS", ProductType::Vps, Some(virt), config, "100", "0")
        .await;
    let client = env.client(name).await;
    let mut item = NewCartItem::product(product, BillingCycle::Monthly);
    item.hostname = Some(format!("vps-{}.example.com", suffix()));
    let cart = env.cart(client, &[item]).await;
    let out = env.checkout(cart, client, Some(gw)).await;
    env.pay(out.invoice_id, gw).await;
    let svc = out.services[0];
    let mut outcome = env
        .billing
        .provision_service(env.tenant, svc, RunContext::default())
        .await
        .unwrap();
    for _ in 0..5 {
        if outcome == ProvisionOutcome::Activated {
            break;
        }
        let payloads = env.pending_payloads("poll_vm_task").await;
        let p: hos_billing::jobs::PollVmTaskPayload =
            serde_json::from_value(payloads.last().expect("poll job").clone()).unwrap();
        env.complete_jobs("poll_vm_task").await;
        match env
            .billing
            .poll_vm_task(env.tenant, &p, RunContext::default())
            .await
            .unwrap()
        {
            Some(o) => outcome = o,
            // Still running (asynchronous mock): the job runner would retry.
            None => {
                hos_billing::jobs::enqueue_poll_vm_task(
                    &mut env.pool.acquire().await.unwrap(),
                    env.tenant,
                    &p,
                    chrono::Utc::now(),
                )
                .await
                .unwrap();
            }
        }
    }
    assert_eq!(outcome, ProvisionOutcome::Activated);
    env.complete_jobs("provision_service").await;
    (client, svc)
}

async fn user(env: &Env, kind: UserKind) -> UserId {
    identity::create_user(
        &env.pool,
        env.tenant,
        &NewUser {
            email: format!("vm-{}@example.com", suffix()),
            password_hash: None,
            kind,
            first_name: "Vm".into(),
            last_name: "Tester".into(),
            phone: None,
            locale: Locale::En,
            email_verified: true,
        },
    )
    .await
    .expect("user")
    .id
}

fn not_found(e: &CoreError) -> bool {
    matches!(e, CoreError::NotFound { .. })
}

#[tokio::test]
async fn snapshot_limit_and_ownership() {
    let Some(env) = Env::new().await else { return };
    let t = env.tenant;
    let gw = env.gateway().await;
    let virt = env.virt(false).await;
    env.ip_pool(virt).await;
    let (victim, victim_svc) = vps(&env, gw, virt, "Victim", json!({})).await;
    let (client, svc) = vps(&env, gw, virt, "Owner", json!({"snapshot_limit": 1})).await;
    let b = &env.billing;

    // Another client's service is invisible.
    let err = b
        .vm_create_snapshot(t, victim_svc, Some(client), "mine")
        .await
        .unwrap_err();
    assert!(not_found(&err), "{err}");

    // Limit 1 for the client; staff are not capped.
    b.vm_create_snapshot(t, svc, Some(client), "first")
        .await
        .unwrap();
    let err = b
        .vm_create_snapshot(t, svc, Some(client), "second")
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::Conflict(_)), "{err}");
    b.vm_create_snapshot(t, svc, None, "staff-made")
        .await
        .unwrap();
    assert_eq!(b.vm_snapshot_quota(t, svc, Some(client)).await.unwrap(), 1);
    // Invalid names never reach the provider.
    let err = b
        .vm_create_snapshot(t, svc, None, "../x")
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::Validation(_)), "{err}");

    // A snapshot of the victim's VM cannot be rolled back / deleted through
    // the attacker's service.
    let victim_snap = b
        .vm_create_snapshot(t, victim_svc, Some(victim), "victim-snap")
        .await
        .unwrap();
    for res in [
        b.vm_rollback_snapshot(t, svc, Some(client), &victim_snap)
            .await,
        b.vm_delete_snapshot(t, svc, Some(client), &victim_snap)
            .await,
    ] {
        assert!(not_found(&res.unwrap_err()));
    }
    assert!(
        b.vm_snapshots(t, victim_svc, None)
            .await
            .unwrap()
            .iter()
            .any(|s| s.id == victim_snap)
    );
    // Own snapshot: fine; every action is in the operation log.
    let own = b.vm_snapshots(t, svc, Some(client)).await.unwrap();
    let first = own.iter().find(|s| s.name == "first").expect("first");
    b.vm_delete_snapshot(t, svc, Some(client), &first.id)
        .await
        .unwrap();
    let ops = b.vm_operations(t, svc, Some(client), 20).await.unwrap();
    assert!(
        ops.iter()
            .any(|o| o.kind == "snapshot_delete" && o.status == "succeeded")
    );
    assert!(b.vm_operations(t, svc, Some(victim), 20).await.is_err());
    env.cleanup().await;
}

#[tokio::test]
async fn ips_isos_keys_and_backups_belong_to_the_vm() {
    let Some(env) = Env::new().await else { return };
    let t = env.tenant;
    let gw = env.gateway().await;
    let virt = env.virt(false).await;
    env.ip_pool(virt).await;
    let (victim, victim_svc) = vps(&env, gw, virt, "Victim", json!({})).await;
    let (client, svc) = vps(&env, gw, virt, "Owner", json!({})).await;
    let b = &env.billing;

    // Reverse DNS: only the service's own addresses, valid host names.
    let victim_ip = services::vm_for_service(&env.pool, t, victim_svc)
        .await
        .unwrap()
        .and_then(|v| v.primary_ipv4)
        .expect("victim ip");
    let err = b
        .vm_set_reverse_dns(t, svc, Some(client), victim_ip, Some("evil.example.com"))
        .await
        .unwrap_err();
    assert!(not_found(&err), "{err}");
    let own_ips = b.vm_reverse_dns(t, svc, Some(client)).await.unwrap();
    let own = own_ips.first().expect("own ip").ip;
    let err = b
        .vm_set_reverse_dns(t, svc, Some(client), own, Some("not a host"))
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::Validation(_)), "{err}");
    b.vm_set_reverse_dns(t, svc, Some(client), own, Some("mail.example.com"))
        .await
        .unwrap();
    let after = b.vm_reverse_dns(t, svc, Some(client)).await.unwrap();
    assert_eq!(
        after
            .iter()
            .find(|p| p.ip == own)
            .and_then(|p| p.ptr.clone())
            .as_deref(),
        Some("mail.example.com")
    );

    // ISO: nothing approved → nothing offered, mounting refused.
    assert!(b.vm_isos(t, svc, Some(client)).await.unwrap().is_empty());
    let (_, provider) = b.vm_provider_isos(t, svc).await.unwrap();
    let iso = provider.first().expect("mock iso").image.id.clone();
    let err = b
        .vm_mount_iso(t, svc, Some(client), &iso, true)
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::Validation(_)), "{err}");
    b.vm_approve_iso(t, svc, &iso).await.unwrap();
    assert!(b.vm_approve_iso(t, svc, "../../etc/shadow").await.is_err());
    b.vm_mount_iso(t, svc, Some(client), &iso, true)
        .await
        .unwrap();
    assert_eq!(
        b.vm_extras(t, svc, Some(client))
            .await
            .unwrap()
            .mounted_iso
            .as_deref(),
        Some(iso.as_str())
    );
    b.vm_unmount_iso(t, svc, Some(client)).await.unwrap();

    // SSH keys of another client cannot be used for rescue.
    let key = b
        .add_client_ssh_key(
            t,
            victim,
            "laptop",
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl a@b",
            None,
        )
        .await
        .unwrap();
    let err = b
        .vm_set_rescue(t, svc, Some(client), true, &[key.id])
        .await
        .unwrap_err();
    assert!(not_found(&err), "{err}");
    assert!(b.delete_client_ssh_key(t, client, key.id).await.is_err());
    let rescue = b
        .vm_set_rescue(t, victim_svc, Some(victim), true, &[key.id])
        .await
        .unwrap();
    assert!(rescue.password.is_some());
    b.vm_set_rescue(t, victim_svc, Some(victim), false, &[])
        .await
        .unwrap();

    // Backups: only ids listed for this VM (the mock names backups after
    // the VM id, so this is what a backup of the victim's VM looks like).
    let victim_vm = services::vm_for_service(&env.pool, t, victim_svc)
        .await
        .unwrap()
        .expect("victim vm");
    b.vm_backups(t, svc, Some(client)).await.unwrap();
    let foreign = format!("backup-{}-20260101", victim_vm.external_id);
    let err = b
        .vm_restore_backup(t, svc, Some(client), &foreign)
        .await
        .unwrap_err();
    assert!(not_found(&err), "{err}");
    env.cleanup().await;
}

#[tokio::test]
async fn password_reset_console_and_suspension() {
    let Some(env) = Env::new().await else { return };
    let t = env.tenant;
    let gw = env.gateway().await;
    let virt = env.virt(false).await;
    env.ip_pool(virt).await;
    let (client, svc) = vps(&env, gw, virt, "Owner", json!({})).await;
    let b = &env.billing;

    // Password reset: returned once, nothing stored, three per hour.
    for _ in 0..3 {
        let r = b
            .vm_reset_password(t, svc, Some(client), None)
            .await
            .unwrap();
        assert_eq!(r.username, "root");
        assert!(r.password.expose_str().len() >= 12);
    }
    let row = services::get_service(&env.pool, t, svc)
        .await
        .unwrap()
        .unwrap();
    assert!(row.password_enc.is_none());
    let err = b
        .vm_reset_password(t, svc, Some(client), None)
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::Conflict(_)), "{err}");
    let err = b
        .vm_reset_password(t, svc, None, Some("Bad Name"))
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::Validation(_)), "{err}");

    // Console tokens: single use, bound to the user, capped per user.
    let owner = user(&env, UserKind::Client).await;
    let other = user(&env, UserKind::Client).await;
    let who = |user_id| ConsoleRequester {
        user_id,
        client_id: Some(client),
        ip: None,
        user_agent: None,
    };
    let issued = b.vm_console_session(t, svc, &who(owner)).await.unwrap();
    let grant = b
        .vm_console_connect(t, &issued.token, &who(owner), 1)
        .await
        .unwrap();
    assert_eq!(grant.session.service_id, svc);
    let again = b.vm_console_connect(t, &issued.token, &who(owner), 1).await;
    assert!(matches!(again, Err(CoreError::Unauthorized)));
    // Cap 1: a second session of the same user is refused…
    let second = b.vm_console_session(t, svc, &who(owner)).await.unwrap();
    let err = b
        .vm_console_connect(t, &second.token, &who(owner), 1)
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::Conflict(_)), "{err}");
    // …until the first ends.
    b.vm_console_end(t, grant.session.id, 10, 20, "closed")
        .await
        .unwrap();
    let third = b.vm_console_session(t, svc, &who(owner)).await.unwrap();
    // Another user cannot use the token (and burns it).
    let stolen = b.vm_console_connect(t, &third.token, &who(other), 1).await;
    assert!(matches!(stolen, Err(CoreError::Unauthorized)));
    assert!(
        b.vm_console_connect(t, &third.token, &who(owner), 1)
            .await
            .is_err()
    );
    let audit = b.vm_console_sessions(t, svc, 10).await.unwrap();
    assert!(
        audit
            .iter()
            .any(|s| s.close_reason.as_deref() == Some("wrong_user"))
    );
    assert!(audit.iter().any(|s| s.bytes_out == 20));

    // Suspended: clients get nothing, staff may still open the console.
    b.suspend_service(t, svc, Some("test"), RunContext::default())
        .await
        .unwrap();
    let err = b.vm_console_session(t, svc, &who(owner)).await.unwrap_err();
    assert!(matches!(err, CoreError::Forbidden(_)), "{err}");
    let err = b
        .vm_reset_password(t, svc, Some(client), None)
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::Forbidden(_)), "{err}");
    let staff = env.staff().await;
    let staff_who = ConsoleRequester {
        user_id: staff,
        client_id: None,
        ip: None,
        user_agent: None,
    };
    let issued = b.vm_console_session(t, svc, &staff_who).await;
    // The mock refuses a console of a stopped VM; anything but a
    // permission error is fine here.
    if let Err(e) = issued {
        assert!(matches!(e, CoreError::Integration(_)), "{e}");
    }
    env.cleanup().await;
}

#[tokio::test]
async fn running_operations_block_destructive_actions() {
    let Some(env) = Env::new().await else { return };
    let t = env.tenant;
    let gw = env.gateway().await;
    let virt = env.virt(true).await;
    env.ip_pool(virt).await;
    let (client, svc) = vps(&env, gw, virt, "Owner", json!({})).await;
    let b = &env.billing;

    b.vm_power(t, svc, Some(client), PowerAction::Reboot)
        .await
        .unwrap();
    let ops = b.vm_operations(t, svc, Some(client), 5).await.unwrap();
    let op = ops.first().expect("power op");
    assert_eq!((op.kind.as_str(), op.status.as_str()), ("power", "running"));
    let err = b
        .vm_reinstall(t, svc, Some(client), "debian-12", Vec::new())
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::Conflict(_)), "{err}");
    let err = b
        .vm_reset_password(t, svc, Some(client), None)
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::Conflict(_)), "{err}");

    // The poll job finishes the operation, after which actions work again.
    for _ in 0..5 {
        let payloads = env.pending_payloads("poll_vm_task").await;
        let Some(p) = payloads.last() else { break };
        let p: hos_billing::jobs::PollVmTaskPayload = serde_json::from_value(p.clone()).unwrap();
        assert_eq!(p.operation_id, Some(op.id));
        env.complete_jobs("poll_vm_task").await;
        if env
            .billing
            .poll_vm_task(t, &p, RunContext::default())
            .await
            .unwrap()
            .is_some()
        {
            break;
        }
        hos_billing::jobs::enqueue_poll_vm_task(
            &mut env.pool.acquire().await.unwrap(),
            t,
            &p,
            chrono::Utc::now(),
        )
        .await
        .unwrap();
    }
    let ops = b.vm_operations(t, svc, Some(client), 5).await.unwrap();
    assert_eq!(ops.first().map(|o| o.status.as_str()), Some("succeeded"));
    b.ensure_vm_idle(t, svc).await.unwrap();

    // Unsupported features surface as `Unsupported`, not as failures.
    let err = b.vm_console(t, svc, Some(client)).await.unwrap_err();
    assert!(
        matches!(
            err,
            CoreError::Integration(IntegrationError::Unsupported(_))
        ),
        "{err}"
    );
    env.cleanup().await;
}

/// The snapshot limit was counted, then the snapshot created: parallel
/// requests all passed the count. A per-service lock now covers check →
/// provider call → record, so only one of them gets through.
#[tokio::test]
async fn parallel_snapshots_cannot_exceed_the_limit() {
    let Some(env) = Env::new().await else { return };
    let t = env.tenant;
    let gw = env.gateway().await;
    // A slow hypervisor widens the window between the count and the create.
    let virt = env
        .integration(
            hos_core::domain::IntegrationCategory::Virt,
            "mock",
            json!({"async_tasks": false, "latency_ms": 300}),
            json!({}),
        )
        .await;
    env.ip_pool(virt).await;
    let (client, svc) = vps(&env, gw, virt, "Racer", json!({"snapshot_limit": 1})).await;
    let b = &env.billing;
    let snap = |name: &'static str| b.vm_create_snapshot(t, svc, Some(client), name);
    let (r1, r2, r3, r4, r5) = tokio::join!(
        snap("race-1"),
        snap("race-2"),
        snap("race-3"),
        snap("race-4"),
        snap("race-5")
    );
    let results = [r1, r2, r3, r4, r5];
    let ok = results.iter().filter(|r| r.is_ok()).count();
    assert_eq!(ok, 1, "{results:?}");
    assert!(
        results
            .iter()
            .filter_map(|r| r.as_ref().err())
            .all(|e| matches!(e, CoreError::Conflict(_))),
        "{results:?}"
    );
    assert_eq!(b.vm_snapshots(t, svc, None).await.unwrap().len(), 1);
    // Sequential requests still see the limit.
    let err = b
        .vm_create_snapshot(t, svc, Some(client), "after")
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::Conflict(_)), "{err}");
    env.cleanup().await;
}

/// Providers reuse VM ids (a Proxmox vmid is handed out again once its VM is
/// gone). A new service that gets such an id must get a fresh record, never
/// the stored record of the older service with its password and metadata.
#[tokio::test]
async fn reused_provider_ids_never_move_a_vm_record() {
    let Some(env) = Env::new().await else { return };
    let t = env.tenant;
    let gw = env.gateway().await;
    let virt = env.virt(false).await;
    env.ip_pool(virt).await;
    let vm_of = |svc: ServiceId| {
        let pool = env.pool.clone();
        async move {
            services::vm_for_service(&pool, t, svc)
                .await
                .unwrap()
                .expect("vm")
        }
    };
    // What provisioning stores when the hypervisor returns `external_id`.
    let provisioned = |svc: ServiceId, external_id: String| services::VmInput {
        service_id: Some(svc),
        integration_id: virt,
        node: None,
        external_id,
        name: format!("svc-{}", svc.as_uuid().simple()),
        hostname: Some(format!("vps-{}.example.com", suffix())),
        os_template: Some("ubuntu-24.04".into()),
        cpu_cores: Some(1),
        memory_mb: Some(1024),
        disk_gb: Some(40),
        status: hos_core::integrations::virt::VmStatus::Running,
        primary_ipv4: None,
        primary_ipv6: None,
        metadata: json!({"fresh": true}),
    };
    // A paid service whose provisioning has not run yet.
    let ordered = |name: &'static str| {
        let env = &env;
        async move {
            let product = env
                .product(
                    "VPS",
                    ProductType::Vps,
                    Some(virt),
                    json!({"template": "ubuntu-24.04", "disk_gb": 40}),
                    "100",
                    "0",
                )
                .await;
            let client = env.client(name).await;
            let cart = env
                .cart(
                    client,
                    &[NewCartItem::product(product, BillingCycle::Monthly)],
                )
                .await;
            let out = env.checkout(cart, client, Some(gw)).await;
            env.pay(out.invoice_id, gw).await;
            out.services[0]
        }
    };
    let stored = |id: hos_core::ids::VmId| {
        let pool = env.pool.clone();
        async move {
            sqlx::query_as::<_, (Option<ServiceId>, bool, bool, serde_json::Value)>(
                "select service_id, deleted_at is not null, root_password_enc is not null, metadata
                   from vms where id = $1",
            )
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    let remember_secret = |id: hos_core::ids::VmId| {
        let pool = env.pool.clone();
        async move {
            sqlx::query(
                "update vms set root_password_enc = '\\x01'::bytea,
                        metadata = metadata || '{\"old_secret\": true}' where id = $1",
            )
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
        }
    };

    // 1. Terminated in HostingOS: the record keeps no credentials, and the
    //    next service that gets the freed id starts with a fresh record.
    let (_, old) = vps(&env, gw, virt, "Old owner", json!({})).await;
    let old_vm = vm_of(old).await;
    remember_secret(old_vm.id).await;
    env.billing
        .terminate_service(t, old, RunContext::default())
        .await
        .unwrap();
    let (_, deleted, has_password, _) = stored(old_vm.id).await;
    assert!(deleted);
    assert!(!has_password, "terminate drops stored VM credentials");
    let password: Option<Vec<u8>> =
        sqlx::query_scalar("select password_enc from services where id = $1")
            .bind(old)
            .fetch_one(&env.pool)
            .await
            .unwrap();
    assert!(
        password.is_none(),
        "terminate drops the service's root password"
    );

    let new = ordered("New owner").await;
    let new_vm = services::upsert_vm(&env.pool, t, &provisioned(new, old_vm.external_id.clone()))
        .await
        .unwrap();
    assert_ne!(new_vm.id, old_vm.id, "the old record was adopted");
    assert_eq!(new_vm.service_id, Some(new));
    assert!(new_vm.root_password_enc.is_none());
    assert!(
        new_vm.metadata.get("old_secret").is_none(),
        "{}",
        new_vm.metadata
    );
    assert_eq!(new_vm.external_id, old_vm.external_id);
    let (owner, deleted, _, _) = stored(old_vm.id).await;
    assert_eq!(owner, Some(old), "the old record stays with its service");
    assert!(deleted);

    // 2. Deleted on the hypervisor behind HostingOS's back: the live record
    //    of the old service is retired, not handed over.
    let (_, live) = vps(&env, gw, virt, "Live owner", json!({})).await;
    let live_vm = vm_of(live).await;
    remember_secret(live_vm.id).await;
    let other = ordered("Other owner").await;
    let other_vm = services::upsert_vm(
        &env.pool,
        t,
        &provisioned(other, live_vm.external_id.clone()),
    )
    .await
    .unwrap();
    assert_ne!(other_vm.id, live_vm.id);
    assert!(other_vm.root_password_enc.is_none());
    assert!(other_vm.metadata.get("old_secret").is_none());
    let (owner, deleted, has_password, _) = stored(live_vm.id).await;
    assert_eq!(owner, Some(live));
    assert!(deleted);
    assert!(!has_password);
    assert!(
        services::vm_for_service(&env.pool, t, live)
            .await
            .unwrap()
            .is_none()
    );

    // 3. The same service syncing its own VM again keeps its record.
    let again = services::upsert_vm(&env.pool, t, &provisioned(other, live_vm.external_id))
        .await
        .unwrap();
    assert_eq!(again.id, other_vm.id);
    assert_eq!(vm_of(other).await.id, other_vm.id);
    env.cleanup().await;
}
