//! Hetzner Cloud provider against a wiremock server (no real network).

use std::time::Duration;

use hos_core::IntegrationError;
use hos_core::integrations::VirtProvider;
use hos_core::integrations::virt::{
    ConsoleKind, IpConfig, NetworkSpec, PowerAction, SnapshotRequest, TaskRef, TaskState, VmRef,
    VmResize, VmSpec,
};
use hos_core::secret::SecretString;
use hos_virt::{HetznerProvider, OutboundPolicy};
use serde_json::{Value, json};
use wiremock::matchers::{
    body_partial_json, body_string_contains, header, method, path, query_param,
};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TOKEN: &str = "hz-token-0123456789";
const BEARER: &str = "Bearer hz-token-0123456789";
const SSH_KEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIHostingOSTestKeyMaterial0123 admin@example";

fn provider(server: &MockServer) -> HetznerProvider {
    HetznerProvider::from_config_with_policy(
        &json!({
            "api_url": format!("{}/v1", server.uri()),
            "default_location": "fsn1",
            "firewall_ids": "7",
            "task_poll_ms": 10,
            "task_timeout_secs": 5,
        }),
        &json!({ "api_token": TOKEN }),
        OutboundPolicy::Unrestricted,
    )
    .unwrap()
}

fn server_json(status: &str) -> Value {
    json!({
        "id": 4711,
        "name": "web42",
        "status": status,
        "created": "2026-11-16T10:00:00+00:00",
        "public_net": {
            "ipv4": {"ip": "198.51.100.7"},
            "ipv6": {"ip": "2001:db8:1234::/64"}
        },
        "server_type": {"id": 104, "name": "cx22", "cores": 2, "memory": 4.0, "disk": 40},
        "location": {"name": "fsn1"},
        "image": {"id": 161547269, "name": "ubuntu-24.04"},
        "labels": {"managed-by": "hostingos"},
        "outgoing_traffic": 1_073_741_824_u64,
        "primary_disk_size": 40
    })
}

fn action(id: u64, command: &str, status: &str) -> Value {
    json!({"action": {"id": id, "command": command, "status": status, "progress": 0,
                      "started": "2026-11-16T10:00:00+00:00", "finished": null, "error": null,
                      "resources": [{"id": 4711, "type": "server"}]}})
}

fn error(code: &str, message: &str) -> Value {
    json!({"error": {"code": code, "message": message, "details": {}}})
}

#[tokio::test]
async fn create_server_full_sequence() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/images"))
        .and(query_param("name", "ubuntu-24.04"))
        .and(header("authorization", BEARER))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"images": [
            {"id": 161547269, "type": "system", "name": "ubuntu-24.04", "architecture": "x86", "status": "available"}
        ]})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/ssh_keys"))
        .and(header("authorization", BEARER))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"ssh_keys": [], "meta": {"pagination": {"page": 1, "next_page": null}}}),
        ))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/ssh_keys"))
        .and(header("authorization", BEARER))
        .and(body_partial_json(
            json!({"public_key": SSH_KEY, "labels": {"managed-by": "hostingos"}}),
        ))
        .respond_with(ResponseTemplate::new(201).set_body_json(
            json!({"ssh_key": {"id": 555, "name": "hostingos-x", "public_key": SSH_KEY}}),
        ))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/servers"))
        .and(header("authorization", BEARER))
        .and(body_partial_json(json!({
            "name": "web42",
            "server_type": "cx22",
            "image": "ubuntu-24.04",
            "location": "fsn1",
            "start_after_create": true,
            "ssh_keys": [555],
            "firewalls": [{"firewall": 7}],
            "public_net": {"enable_ipv4": true, "enable_ipv6": true},
            "labels": {"managed-by": "hostingos", "env": "prod"},
        })))
        .and(body_string_contains("#cloud-config"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "server": server_json("initializing"),
            "action": action(1, "create_server", "running")["action"],
            "next_actions": [action(2, "start_server", "running")["action"]],
            "root_password": null
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/actions/2"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"action": {"id": 2, "status": "running", "progress": 40}})),
        )
        .mount(&server)
        .await;

    let p = provider(&server);
    let out = p
        .create_vm(&VmSpec {
            name: "web42".into(),
            hostname: "web42.example.com".into(),
            template: "ubuntu-24.04".into(),
            plan: Some("cx22".into()),
            network: NetworkSpec {
                ipv4: Some(IpConfig::Auto),
                ipv6: Some(IpConfig::Auto),
                ..NetworkSpec::default()
            },
            ssh_keys: vec![SSH_KEY.into()],
            root_password: Some(SecretString::from("Root-Pass-123")),
            tags: vec!["env=prod".into()],
            ..VmSpec::default()
        })
        .await
        .unwrap();
    assert_eq!(out.vm, VmRef::on_node("fsn1", "4711"));
    assert_eq!(
        out.task,
        Some(TaskRef {
            id: "2".into(),
            node: None
        })
    );
    assert_eq!(
        out.ipv4,
        vec!["198.51.100.7".parse::<std::net::IpAddr>().unwrap()]
    );
    assert_eq!(
        out.ipv6,
        vec!["2001:db8:1234::1".parse::<std::net::IpAddr>().unwrap()]
    );
    assert!(out.root_password.is_none());
    assert_eq!(
        p.task_status(out.task.as_ref().unwrap()).await.unwrap(),
        TaskState::Running {
            progress_percent: Some(40)
        }
    );

    // A fixed IPv4 needs an existing Primary IP — refused before any request.
    let before = server.received_requests().await.unwrap().len();
    let err = p
        .create_vm(&VmSpec {
            name: "web43".into(),
            template: "ubuntu-24.04".into(),
            plan: Some("cx22".into()),
            network: NetworkSpec {
                ipv4: Some(IpConfig::Static(hos_core::integrations::virt::StaticIp {
                    address: "198.51.100.8".parse().unwrap(),
                    prefix_len: 32,
                    gateway: None,
                })),
                ..NetworkSpec::default()
            },
            ..VmSpec::default()
        })
        .await
        .unwrap_err();
    assert!(matches!(err, IntegrationError::InvalidInput(_)));
    assert_eq!(server.received_requests().await.unwrap().len(), before);
}

#[tokio::test]
async fn power_console_and_tasks() {
    let server = MockServer::start().await;
    for (endpoint, id) in [
        ("poweron", 10),
        ("poweroff", 11),
        ("shutdown", 12),
        ("reboot", 13),
        ("reset", 14),
    ] {
        Mock::given(method("POST"))
            .and(path(format!("/v1/servers/4711/actions/{endpoint}")))
            .and(header("authorization", BEARER))
            .respond_with(ResponseTemplate::new(201).set_body_json(action(id, endpoint, "running")))
            .expect(1)
            .mount(&server)
            .await;
    }
    Mock::given(method("POST"))
        .and(path("/v1/servers/4711/actions/request_console"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "wss_url": "wss://console.hetzner.cloud/?server_id=4711&token=3db32d15-af2f-459c-8bf8-dee1fd05f49c",
            "password": "9MQaTg2VAGI0FIpc10k3UpRXcHj2wQ6x",
            "action": action(15, "request_console", "success")["action"]
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/actions/13"))
        .respond_with(ResponseTemplate::new(200).set_body_json(action(
            13,
            "reboot_server",
            "success",
        )))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/actions/14"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"action": {"id": 14, "status": "error",
            "error": {"code": "action_failed", "message": "Action failed"}}})),
        )
        .mount(&server)
        .await;

    let p = provider(&server);
    let vm = VmRef::new("4711");
    let mut ids = Vec::new();
    for a in [
        PowerAction::Start,
        PowerAction::Stop,
        PowerAction::Shutdown,
        PowerAction::Reboot,
        PowerAction::Reset,
    ] {
        ids.push(p.power(&vm, a).await.unwrap().unwrap().id);
    }
    assert_eq!(ids, ["10", "11", "12", "13", "14"]);
    assert_eq!(
        p.task_status(&TaskRef {
            id: "13".into(),
            node: None
        })
        .await
        .unwrap(),
        TaskState::Succeeded
    );
    assert_eq!(
        p.task_status(&TaskRef {
            id: "14".into(),
            node: None
        })
        .await
        .unwrap(),
        TaskState::Failed {
            message: "Action failed".into()
        }
    );
    assert!(matches!(
        p.task_status(&TaskRef {
            id: "UPID:x".into(),
            node: None
        })
        .await,
        Err(IntegrationError::InvalidInput(_))
    ));

    let console = p.console(&vm).await.unwrap();
    assert_eq!(console.kind, ConsoleKind::NoVnc);
    assert!(console.url.starts_with("wss://console.hetzner.cloud/"));
    assert_eq!(
        console.password.unwrap().expose_str(),
        "9MQaTg2VAGI0FIpc10k3UpRXcHj2wQ6x"
    );

    // Memory snapshots do not exist at Hetzner.
    assert!(matches!(
        p.create_snapshot(
            &vm,
            &SnapshotRequest {
                name: "x".into(),
                include_memory: true,
                ..SnapshotRequest::default()
            }
        )
        .await,
        Err(IntegrationError::Unsupported(_))
    ));
}

#[tokio::test]
async fn resize_shuts_down_changes_type_and_powers_on() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/servers/4711"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"server": server_json("running")})))
        // One read per resize() call below; afterwards the server reports `off`.
        .up_to_n_times(2)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/servers/4711"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"server": server_json("off")})),
        )
        .with_priority(5)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/servers/4711/actions/shutdown"))
        .respond_with(ResponseTemplate::new(201).set_body_json(action(
            20,
            "shutdown_server",
            "running",
        )))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/servers/4711/actions/change_type"))
        .and(body_partial_json(
            json!({"server_type": "cx32", "upgrade_disk": false}),
        ))
        .respond_with(ResponseTemplate::new(201).set_body_json(action(
            21,
            "change_server_type",
            "running",
        )))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/servers/4711/actions/poweron"))
        .respond_with(ResponseTemplate::new(201).set_body_json(action(
            22,
            "start_server",
            "running",
        )))
        .expect(1)
        .mount(&server)
        .await;
    for id in [20, 21] {
        Mock::given(method("GET"))
            .and(path(format!("/v1/actions/{id}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(action(id, "x", "success")))
            .mount(&server)
            .await;
    }

    let p = provider(&server);
    let vm = VmRef::new("4711");
    let without_restart = VmResize {
        plan: Some("cx32".into()),
        ..VmResize::default()
    };
    assert!(matches!(
        p.resize(&vm, &without_restart).await,
        Err(IntegrationError::Conflict(_))
    ));
    let task = p
        .resize(
            &vm,
            &VmResize {
                plan: Some("cx32".into()),
                allow_restart: true,
                ..VmResize::default()
            },
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(task.id, "22");
    let posts: Vec<String> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.method.as_str() == "POST")
        .map(|r| r.url.path().to_owned())
        .collect();
    assert_eq!(
        posts,
        [
            "/v1/servers/4711/actions/shutdown",
            "/v1/servers/4711/actions/change_type",
            "/v1/servers/4711/actions/poweron",
        ]
    );
}

#[tokio::test]
async fn metrics_from_time_series() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/servers/4711"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"server": server_json("running")})),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/servers/4711/metrics"))
        .and(query_param("type", "cpu,disk,network"))
        .and(query_param("step", "60"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"metrics": {
            "start": "2026-11-16T10:00:00Z", "end": "2026-11-16T10:10:00Z", "step": 60,
            "time_series": {
                "cpu": {"values": [[1, "50"], [2, "100"]]},
                "disk.0.bandwidth.read": {"values": [[2, "1024"]]},
                "network.0.bandwidth.in": {"values": [[2, "2048.5"]]}
            }
        }})))
        .mount(&server)
        .await;
    let m = provider(&server)
        .metrics(&VmRef::new("4711"))
        .await
        .unwrap();
    assert_eq!(m.cpu_usage_percent, Some(50.0), "100 per vCPU, 2 vCPUs");
    assert_eq!(m.disk_read_bps, Some(1024.0));
    assert_eq!(m.net_in_bps, Some(2048.5));
    assert_eq!(m.memory_total_mb, Some(4096));
    assert_eq!(m.bandwidth_used_gb, Some(1.0));
}

#[tokio::test]
async fn error_mapping() {
    let server = MockServer::start().await;
    let p = provider(&server);
    let cases: [(u16, Value); 5] = [
        (401, error("unauthorized", "unable to authenticate")),
        (404, error("not_found", "server with ID '1' not found")),
        (
            409,
            error("uniqueness_error", "server name is already used"),
        ),
        (
            422,
            json!({"error": {"code": "invalid_input", "message": "invalid input in field 'name'",
                "details": {"fields": [{"name": "name", "messages": ["is invalid"]}]}}}),
        ),
        (500, error("server_error", "internal error")),
    ];
    for (i, (status, body)) in cases.iter().enumerate() {
        let id = 100 + i;
        Mock::given(method("POST"))
            .and(path(format!("/v1/servers/{id}/actions/poweron")))
            .respond_with(ResponseTemplate::new(*status).set_body_json(body))
            .expect(1)
            .mount(&server)
            .await;
    }
    let call = |id: usize| {
        let p = &p;
        async move {
            p.power(&VmRef::new(id.to_string()), PowerAction::Start)
                .await
        }
    };
    assert!(matches!(call(100).await, Err(IntegrationError::Auth(_))));
    assert!(matches!(
        call(101).await,
        Err(IntegrationError::NotFound(_))
    ));
    assert!(matches!(
        call(102).await,
        Err(IntegrationError::Conflict(_))
    ));
    assert!(
        matches!(call(103).await, Err(IntegrationError::InvalidInput(m)) if m.contains("name"))
    );
    assert!(matches!(
        call(104).await,
        Err(IntegrationError::Remote { status: Some(500), code: Some(ref c), .. }) if c == "server_error"
    ));

    // 429 on a POST: not retried, RateLimit-Reset → retry_after.
    let reset = chrono::Utc::now().timestamp() + 30;
    Mock::given(method("POST"))
        .and(path("/v1/servers/200/actions/reboot"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("RateLimit-Limit", "3600")
                .insert_header("RateLimit-Remaining", "0")
                .insert_header("RateLimit-Reset", reset.to_string().as_str())
                .set_body_json(error(
                    "rate_limit_exceeded",
                    "limit of 3600 requests per hour reached",
                )),
        )
        .expect(1)
        .mount(&server)
        .await;
    match p.power(&VmRef::new("200"), PowerAction::Reboot).await {
        Err(IntegrationError::RateLimited { retry_after }) => {
            let d = retry_after.expect("reset header parsed");
            assert!(
                d > Duration::from_secs(20) && d <= Duration::from_secs(30),
                "{d:?}"
            );
        }
        other => panic!("expected RateLimited, got {other:?}"),
    }

    // 503 on a GET is retried; the token is never part of the error.
    Mock::given(method("GET"))
        .and(path("/v1/servers/300"))
        .respond_with(
            ResponseTemplate::new(503)
                .set_body_json(error("unavailable", &format!("echo {TOKEN}"))),
        )
        .expect(3)
        .mount(&server)
        .await;
    match p.get_vm(&VmRef::new("300")).await {
        Err(
            e @ IntegrationError::Remote {
                status: Some(503), ..
            },
        ) => {
            assert!(!e.to_string().contains(TOKEN), "{e}");
        }
        other => panic!("expected Remote 503, got {other:?}"),
    }
}

#[tokio::test]
async fn snapshots_are_scoped_to_their_server() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/images/900"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"image": {
            "id": 900, "type": "snapshot", "created_from": {"id": 9999, "name": "someone-else"}}})))
        .mount(&server)
        .await;
    let p = provider(&server);
    // Another tenant's snapshot id must not be usable on this server.
    assert!(matches!(
        p.rollback_snapshot(&VmRef::new("4711"), "900").await,
        Err(IntegrationError::NotFound(_))
    ));
    assert!(matches!(
        p.delete_snapshot(&VmRef::new("4711"), "900").await,
        Err(IntegrationError::NotFound(_))
    ));
}

// ---------------------------------------------------------------------------
// Reinstall source validation: only OS images (system / app).
// ---------------------------------------------------------------------------

fn reinstall_from(template: &str) -> hos_core::integrations::virt::VmReinstall {
    hos_core::integrations::virt::VmReinstall {
        template: template.into(),
        ..Default::default()
    }
}

#[tokio::test]
async fn reinstall_refuses_snapshots_and_backups() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/images/900"))
        .and(header("authorization", BEARER))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"image": {
            "id": 900, "type": "snapshot", "name": null, "status": "available",
            "description": "other customer's server", "created_from": {"id": 5000, "name": "victim"}
        }})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/images/901"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"image": {
            "id": 901, "type": "backup", "name": null, "status": "available"
        }})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/images/902"))
        .respond_with(
            ResponseTemplate::new(404).set_body_json(
                json!({"error": {"code": "not_found", "message": "image not found"}}),
            ),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    let p = provider(&server);
    for template in ["900", "901", "902"] {
        match p
            .reinstall(&VmRef::new("4711"), &reinstall_from(template))
            .await
        {
            Err(IntegrationError::InvalidInput(m)) => {
                assert!(m.contains("not an available OS image"), "{template}: {m}")
            }
            other => panic!("{template}: expected InvalidInput, got {other:?}"),
        }
        // Create applies the same rule (no server from someone's snapshot).
        let spec = VmSpec {
            name: "web43".into(),
            template: template.into(),
            plan: Some("cx22".into()),
            ..VmSpec::default()
        };
        match p.create_vm(&spec).await {
            Err(IntegrationError::InvalidInput(m)) => {
                assert!(
                    m.contains("not an available OS image"),
                    "create {template}: {m}"
                )
            }
            other => panic!("create {template}: expected InvalidInput, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn reinstall_with_a_system_image_rebuilds() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/images"))
        .and(query_param("name", "ubuntu-24.04"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"images": [
            {"id": 161547269, "type": "system", "name": "ubuntu-24.04", "architecture": "x86", "status": "available"},
            {"id": 161547270, "type": "system", "name": "ubuntu-24.04", "architecture": "arm", "status": "available"}
        ]})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/servers/4711/actions/rebuild"))
        .and(body_partial_json(json!({"image": "ubuntu-24.04"})))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "action": {"id": 77, "command": "rebuild_server", "status": "running", "progress": 0},
            "root_password": null
        })))
        .expect(1)
        .mount(&server)
        .await;
    let out = provider(&server)
        .reinstall(&VmRef::new("4711"), &reinstall_from("ubuntu-24.04"))
        .await
        .unwrap();
    assert_eq!(out.task.map(|t| t.id), Some("77".to_owned()));
}

#[tokio::test]
async fn list_templates_never_offers_snapshots() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/images"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "images": [
                {"id": 1, "type": "system", "name": "debian-12", "status": "available"},
                {"id": 2, "type": "snapshot", "name": null, "description": "leak", "status": "available"},
                {"id": 3, "type": "app", "name": "docker-ce", "status": "available"}
            ],
            "meta": {"pagination": {"page": 1, "next_page": null}}
        })))
        .mount(&server)
        .await;
    let ids: Vec<String> = provider(&server)
        .list_templates(None)
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.id)
        .collect();
    assert_eq!(ids, vec!["debian-12", "docker-ce"]);
}
