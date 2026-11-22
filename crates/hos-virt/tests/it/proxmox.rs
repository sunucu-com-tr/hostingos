//! Proxmox VE provider against a wiremock server (no real network).

use hos_core::IntegrationError;
use hos_core::integrations::VirtProvider;
use hos_core::integrations::virt::{
    IpConfig, NetworkSpec, PowerAction, SnapshotRequest, StaticIp, TaskRef, TaskState, VmRef,
    VmReinstall, VmSpec,
};
use hos_core::secret::SecretString;
use hos_virt::{OutboundPolicy, ProxmoxProvider};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use wiremock::matchers::{body_string_contains, header, method, path, path_regex, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const AUTH: &str = "PVEAPIToken=hostingos@pve!hos=s3cr3t-token";
const SSH_KEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIHostingOSTestKeyMaterial0123 admin@example";
const UPID_CLONE: &str = "UPID:pve1:0000ABCD:0012F00D:6650A1B2:qmclone:9000:hostingos@pve!hos:";
const UPID_RESIZE: &str = "UPID:pve1:0000ABCE:0012F00E:6650A1B3:resize:101:hostingos@pve!hos:";
const UPID_START: &str = "UPID:pve1:0000ABCF:0012F00F:6650A1B4:qmstart:101:hostingos@pve!hos:";

fn provider(uri: &str) -> ProxmoxProvider {
    ProxmoxProvider::from_config_with_policy(
        &json!({
            "api_url": uri,
            "token_id": "hostingos@pve!hos",
            "task_poll_ms": 10,
            "task_timeout_secs": 5,
        }),
        &json!({"token_secret": "s3cr3t-token"}),
        OutboundPolicy::Unrestricted,
    )
    .unwrap()
}

fn data(v: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({ "data": v }))
}

async fn mount_get(server: &MockServer, p: &str, body: Value) {
    Mock::given(method("GET"))
        .and(path(p))
        .and(header("authorization", AUTH))
        .respond_with(data(body))
        .mount(server)
        .await;
}

async fn mount_resources(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/api2/json/cluster/resources"))
        .and(query_param("type", "vm"))
        .and(header("authorization", AUTH))
        .respond_with(data(json!([
            {"id": "qemu/9000", "type": "qemu", "node": "pve1", "vmid": 9000, "status": "stopped", "template": 1, "name": "ubuntu-2404"},
            {"id": "qemu/101", "type": "qemu", "node": "pve1", "vmid": 101, "status": "running", "name": "web1"},
            {"id": "lxc/200", "type": "lxc", "node": "pve2", "vmid": 200, "status": "running", "name": "ct1"},
        ])))
        .mount(server)
        .await;
}

/// Every task finishes with `OK`, except the UPIDs mounted explicitly first.
async fn mount_tasks_ok(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path_regex(
            r"^/api2/json/nodes/pve[12]/tasks/UPID:[^/]+/status$",
        ))
        .and(header("authorization", AUTH))
        .respond_with(data(json!({"status": "stopped", "exitstatus": "OK"})))
        .with_priority(10)
        .mount(server)
        .await;
}

fn qemu_spec() -> VmSpec {
    VmSpec {
        name: "svc-42".into(),
        hostname: "web42.example.com".into(),
        template: "9000".into(),
        cpu_cores: 2,
        memory_mb: 2048,
        disk_gb: 20,
        network: NetworkSpec {
            ipv4: Some(IpConfig::Static(StaticIp {
                address: "203.0.113.20".parse().unwrap(),
                prefix_len: 24,
                gateway: Some("203.0.113.1".parse().unwrap()),
            })),
            dns_servers: vec!["1.1.1.1".parse().unwrap()],
            vlan: Some(20),
            rate_limit_mbps: Some(100),
            ..NetworkSpec::default()
        },
        ssh_keys: vec![SSH_KEY.into()],
        root_password: Some(SecretString::from("Root-Pass-123")),
        ..VmSpec::default()
    }
}

#[tokio::test]
async fn create_qemu_full_sequence() {
    let server = MockServer::start().await;
    mount_resources(&server).await;
    mount_tasks_ok(&server).await;
    mount_get(&server, "/api2/json/cluster/nextid", json!("101")).await;
    Mock::given(method("POST"))
        .and(path("/api2/json/nodes/pve1/qemu/9000/clone"))
        .and(header("authorization", AUTH))
        .and(body_string_contains("newid=101"))
        .and(body_string_contains("full=1"))
        .and(body_string_contains("name=web42.example.com"))
        .respond_with(data(json!(UPID_CLONE)))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api2/json/nodes/pve1/qemu/101/config"))
        .and(header("authorization", AUTH))
        .and(body_string_contains("cores=2"))
        .and(body_string_contains("memory=2048"))
        .and(body_string_contains("ciuser=root"))
        .and(body_string_contains("cipassword=Root-Pass-123"))
        // URL-encoded once by us (PVE rule), then form-encoded: `%20` → `%2520`.
        .and(body_string_contains(
            "sshkeys=ssh-ed25519%2520AAAAC3NzaC1lZDI1NTE5AAAAIHostingOSTestKeyMaterial0123%2520admin%2540example",
        ))
        .and(body_string_contains(
            "ipconfig0=ip%3D203.0.113.20%2F24%2Cgw%3D203.0.113.1",
        ))
        .and(body_string_contains("nameserver=1.1.1.1"))
        .and(body_string_contains("searchdomain=example.com"))
        .and(body_string_contains(
            "net0=virtio%2Cbridge%3Dvmbr0%2Ctag%3D20%2Crate%3D12.5",
        ))
        .respond_with(data(Value::Null))
        .expect(1)
        .mount(&server)
        .await;
    mount_get(
        &server,
        "/api2/json/nodes/pve1/qemu/101/config",
        json!({"boot": "order=scsi0;net0", "scsi0": "local-lvm:vm-101-disk-0,size=10G", "ide2": "local-lvm:vm-101-cloudinit,media=cdrom"}),
    )
    .await;
    Mock::given(method("PUT"))
        .and(path("/api2/json/nodes/pve1/qemu/101/resize"))
        .and(header("authorization", AUTH))
        .and(body_string_contains("disk=scsi0"))
        .and(body_string_contains("size=20G"))
        .respond_with(data(json!(UPID_RESIZE)))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api2/json/nodes/pve1/qemu/101/status/start"))
        .and(header("authorization", AUTH))
        .respond_with(data(json!(UPID_START)))
        .expect(1)
        .mount(&server)
        .await;

    let p = provider(&server.uri());
    let out = p.create_vm(&qemu_spec()).await.unwrap();
    assert_eq!(out.vm, VmRef::on_node("pve1", "101"));
    assert_eq!(
        out.task,
        Some(TaskRef {
            id: UPID_START.into(),
            node: Some("pve1".into())
        })
    );
    assert_eq!(
        out.ipv4,
        vec!["203.0.113.20".parse::<std::net::IpAddr>().unwrap()]
    );
    assert!(out.root_password.is_none(), "caller supplied the password");

    // Order: clone → (wait) → config → resize → start.
    let calls: Vec<String> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.method.as_str() != "GET")
        .map(|r| format!("{} {}", r.method, r.url.path()))
        .collect();
    assert_eq!(
        calls,
        vec![
            "POST /api2/json/nodes/pve1/qemu/9000/clone",
            "POST /api2/json/nodes/pve1/qemu/101/config",
            "PUT /api2/json/nodes/pve1/qemu/101/resize",
            "POST /api2/json/nodes/pve1/qemu/101/status/start",
        ]
    );
}

#[tokio::test]
async fn failed_clone_task_is_reported() {
    let server = MockServer::start().await;
    mount_resources(&server).await;
    mount_get(&server, "/api2/json/cluster/nextid", json!(102)).await;
    Mock::given(method("POST"))
        .and(path("/api2/json/nodes/pve1/qemu/9000/clone"))
        .respond_with(data(json!(UPID_CLONE)))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!(
            "/api2/json/nodes/pve1/tasks/{UPID_CLONE}/status"
        )))
        .respond_with(data(
            json!({"status": "stopped", "exitstatus": "storage 'fast' is not online"}),
        ))
        .mount(&server)
        .await;

    let p = provider(&server.uri());
    let mut spec = qemu_spec();
    spec.root_password = None;
    match p.create_vm(&spec).await {
        Err(IntegrationError::Remote { code, message, .. }) => {
            assert_eq!(code.as_deref(), Some("qmclone"));
            assert!(message.contains("is not online"), "{message}");
        }
        other => panic!("expected task failure, got {other:?}"),
    }
}

#[tokio::test]
async fn create_lxc_returns_task_and_generated_password() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api2/json/nodes/pve2/storage/local/content"))
        .and(query_param("content", "vztmpl"))
        .and(header("authorization", AUTH))
        .respond_with(data(json!([
            {"volid": "local:vztmpl/debian-12-standard_12.7-1_amd64.tar.zst", "content": "vztmpl", "size": 1},
        ])))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api2/json/nodes/pve2/lxc"))
        .and(header("authorization", AUTH))
        .and(body_string_contains("vmid=205"))
        .and(body_string_contains(
            "ostemplate=local%3Avztmpl%2Fdebian-12-standard_12.7-1_amd64.tar.zst",
        ))
        .and(body_string_contains("rootfs=local-lvm%3A8"))
        .and(body_string_contains("unprivileged=1"))
        .and(body_string_contains("password="))
        .respond_with(data(json!(
            "UPID:pve2:00001111:00002222:6650A1B5:vzcreate:205:hostingos@pve!hos:"
        )))
        .expect(1)
        .mount(&server)
        .await;

    let p = provider(&server.uri());
    let out = p
        .create_vm(&VmSpec {
            name: "ct-205".into(),
            hostname: "ct205".into(),
            node: Some("pve2".into()),
            storage: Some("local-lvm".into()),
            template: "local:vztmpl/debian-12-standard_12.7-1_amd64.tar.zst".into(),
            disk_gb: 8,
            network: NetworkSpec {
                ipv4: Some(IpConfig::Dhcp),
                ..NetworkSpec::default()
            },
            extra: json!({"vmid": 205}),
            ..VmSpec::default()
        })
        .await
        .unwrap();
    assert_eq!(out.vm, VmRef::on_node("pve2", "205"));
    assert_eq!(out.task.unwrap().node.as_deref(), Some("pve2"));
    assert!(out.root_password.unwrap().expose_str().len() >= 16);
}

#[tokio::test]
async fn power_snapshots_console_metrics_and_tasks() {
    let server = MockServer::start().await;
    mount_resources(&server).await;
    let upid = "UPID:pve1:00003333:00004444:6650A1B6:qmshutdown:101:hostingos@pve!hos:";
    let failed = "UPID:pve1:00003334:00004445:6650A1B7:qmreboot:101:hostingos@pve!hos:";
    Mock::given(method("POST"))
        .and(path("/api2/json/nodes/pve1/qemu/101/status/shutdown"))
        .and(header("authorization", AUTH))
        .respond_with(data(json!(upid)))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api2/json/nodes/pve1/tasks/{upid}/status")))
        .respond_with(data(json!({"status": "running", "pid": 13107})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api2/json/nodes/pve1/tasks/{failed}/status")))
        .respond_with(data(
            json!({"status": "stopped", "exitstatus": "VM quit/powerdown failed"}),
        ))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api2/json/nodes/pve1/qemu/101/snapshot"))
        .and(body_string_contains("snapname=before_upgrade"))
        .and(body_string_contains("vmstate=1"))
        .respond_with(data(json!(
            "UPID:pve1:00005555:00006666:6650A1B8:qmsnapshot:101:hostingos@pve!hos:"
        )))
        .expect(1)
        .mount(&server)
        .await;
    mount_get(
        &server,
        "/api2/json/nodes/pve1/qemu/101/snapshot",
        json!([
            {"name": "before_upgrade", "description": "x", "snaptime": 1_717_000_000, "vmstate": 1},
            {"name": "current", "parent": "before_upgrade", "running": 1},
        ]),
    )
    .await;
    Mock::given(method("POST"))
        .and(path("/api2/json/nodes/pve1/qemu/101/vncproxy"))
        .and(body_string_contains("websocket=1"))
        .respond_with(data(json!({
            "port": "5900", "ticket": "PVEVNC:6650A1B9::abc+def/ghi=", "user": "hostingos@pve!hos",
            "cert": "-----BEGIN CERTIFICATE-----", "upid": "UPID:pve1:00007777:00008888:6650A1B9:vncproxy:101:hostingos@pve!hos:"
        })))
        .mount(&server)
        .await;
    mount_get(
        &server,
        "/api2/json/nodes/pve1/qemu/101/status/current",
        json!({"status": "running", "qmpstatus": "running", "cpu": 0.25, "mem": 1_073_741_824_u64,
               "maxmem": 2_147_483_648_u64, "maxdisk": 21_474_836_480_u64, "uptime": 3600}),
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/api2/json/nodes/pve1/qemu/101/rrddata"))
        .and(query_param("timeframe", "hour"))
        .and(query_param("cf", "AVERAGE"))
        .respond_with(data(json!([
            {"time": 1, "netin": 100.0, "netout": 50.0, "diskread": 10.0, "diskwrite": 20.0},
            {"time": 2}
        ])))
        .mount(&server)
        .await;

    let p = provider(&server.uri());
    let vm = VmRef::new("101");

    let task = p.power(&vm, PowerAction::Shutdown).await.unwrap().unwrap();
    assert_eq!(task.id, upid);
    assert_eq!(task.node.as_deref(), Some("pve1"));
    assert_eq!(
        p.task_status(&task).await.unwrap(),
        TaskState::Running {
            progress_percent: None
        }
    );
    let failed_task = TaskRef {
        id: failed.into(),
        node: None,
    };
    assert_eq!(
        p.task_status(&failed_task).await.unwrap(),
        TaskState::Failed {
            message: "VM quit/powerdown failed".into()
        }
    );

    // LXC has no hardware reset.
    assert!(matches!(
        p.power(&VmRef::new("200"), PowerAction::Reset).await,
        Err(IntegrationError::Unsupported(_))
    ));
    // Unknown VMID → NotFound without guessing a node.
    assert!(matches!(
        p.power(&VmRef::new("999"), PowerAction::Start).await,
        Err(IntegrationError::NotFound(_))
    ));

    let snap = p
        .create_snapshot(
            &vm,
            &SnapshotRequest {
                name: "before upgrade".into(),
                description: None,
                include_memory: true,
            },
        )
        .await
        .unwrap();
    assert_eq!(snap.snapshot_id, "before_upgrade");
    assert!(snap.task.is_some());
    let snaps = p.list_snapshots(&vm).await.unwrap();
    assert_eq!(snaps.len(), 1, "`current` is not a snapshot");
    assert!(snaps[0].includes_memory);

    let console = p.console(&vm).await.unwrap();
    assert!(console.url.starts_with("ws://"), "{}", console.url);
    assert!(
        console
            .url
            .ends_with("/api2/json/nodes/pve1/qemu/101/vncwebsocket?port=5900&vncticket=PVEVNC%3A6650A1B9%3A%3Aabc%2Bdef%2Fghi%3D"),
        "{}",
        console.url
    );
    assert_eq!(console.port, Some(5900));

    let m = p.metrics(&vm).await.unwrap();
    assert_eq!(m.cpu_usage_percent, Some(25.0));
    assert_eq!(m.memory_used_mb, Some(1024));
    assert_eq!(m.net_in_bps, Some(100.0));
    assert_eq!(m.uptime_secs, Some(3600));
}

#[tokio::test]
async fn error_mapping() {
    // 401 → Auth
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api2/json/version"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    assert!(matches!(
        provider(&server.uri()).test_connection().await,
        Err(IntegrationError::Auth(_))
    ));

    // 404 → NotFound
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api2/json/version"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    assert!(matches!(
        provider(&server.uri()).test_connection().await,
        Err(IntegrationError::NotFound(_))
    ));

    // 500 with a PVE message → Remote (not retried: 500 is not transient for PVE);
    // the token secret never leaks into the message.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api2/json/version"))
        .respond_with(
            ResponseTemplate::new(500).set_body_json(
                json!({"data": null, "message": "internal error near s3cr3t-token\n"}),
            ),
        )
        .expect(1)
        .mount(&server)
        .await;
    match provider(&server.uri()).test_connection().await {
        Err(IntegrationError::Remote {
            status, message, ..
        }) => {
            assert_eq!(status, Some(500));
            assert!(message.contains("internal error"));
            assert!(!message.contains("s3cr3t-token"), "{message}");
        }
        other => panic!("expected Remote, got {other:?}"),
    }

    // 500 "does not exist" → NotFound, "is locked" → Conflict (body `errors` included).
    let server = MockServer::start().await;
    mount_resources(&server).await;
    Mock::given(method("POST"))
        .and(path("/api2/json/nodes/pve1/qemu/101/status/start"))
        .respond_with(
            ResponseTemplate::new(500)
                .set_body_json(json!({"data": null, "message": "VM 101 is locked (backup)\n"})),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api2/json/nodes/pve1/qemu/101/snapshot"))
        .respond_with(ResponseTemplate::new(400).set_body_json(
            json!({"data": null, "message": "Parameter verification failed.\n", "errors": {"snapname": "invalid format"}}),
        ))
        .mount(&server)
        .await;
    let p = provider(&server.uri());
    assert!(matches!(
        p.power(&VmRef::new("101"), PowerAction::Start).await,
        Err(IntegrationError::Conflict(m)) if m.contains("is locked")
    ));
    match p
        .create_snapshot(
            &VmRef::new("101"),
            &SnapshotRequest {
                name: "x".into(),
                ..SnapshotRequest::default()
            },
        )
        .await
    {
        Err(IntegrationError::InvalidInput(m)) => {
            assert!(m.contains("snapname: invalid format"), "{m}")
        }
        other => panic!("expected InvalidInput, got {other:?}"),
    }

    // 503 on a GET is retried (3 attempts) and then surfaces as Remote.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api2/json/version"))
        .respond_with(ResponseTemplate::new(503))
        .expect(3)
        .mount(&server)
        .await;
    assert!(matches!(
        provider(&server.uri()).test_connection().await,
        Err(IntegrationError::Remote {
            status: Some(503),
            ..
        })
    ));
}

/// Older PVE versions only put the error text into the HTTP reason phrase.
#[tokio::test]
async fn reason_phrase_is_used_as_message() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let mut buf = vec![0u8; 4096];
            let mut read = 0;
            while !buf[..read].windows(4).any(|w| w == b"\r\n\r\n") {
                match sock.read(&mut buf[read..]).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => read += n,
                }
            }
            let body = b"{\"data\":null}";
            let head = format!(
                "HTTP/1.1 500 VM 101 is locked (snapshot-delete)\r\nContent-Type: application/json;charset=UTF-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = sock.write_all(head.as_bytes()).await;
            let _ = sock.write_all(body).await;
            let _ = sock.shutdown().await;
        }
    });
    let p = provider(&format!("http://{addr}"));
    match p.test_connection().await {
        Err(IntegrationError::Conflict(m)) => {
            assert!(m.contains("is locked (snapshot-delete)"), "{m}")
        }
        other => panic!("expected Conflict from reason phrase, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Reinstall source validation (only templates may be cloned) and the
// outbound policy / response hardening of the HTTP layer.
// ---------------------------------------------------------------------------

/// Guests: template 9000, the customer's VM 101, another customer's VM 102
/// and container 200.
async fn mount_tenant_guests(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/api2/json/cluster/resources"))
        .and(query_param("type", "vm"))
        .respond_with(data(json!([
            {"id": "qemu/9000", "type": "qemu", "node": "pve1", "vmid": 9000, "status": "stopped", "template": 1, "name": "ubuntu-2404"},
            {"id": "qemu/101", "type": "qemu", "node": "pve1", "vmid": 101, "status": "running", "name": "web1"},
            {"id": "qemu/102", "type": "qemu", "node": "pve1", "vmid": 102, "status": "running", "template": 0, "name": "other-customer"},
            {"id": "lxc/200", "type": "lxc", "node": "pve2", "vmid": 200, "status": "running", "name": "ct1"},
        ])))
        .mount(server)
        .await;
}

/// Nothing may be stopped, destroyed, cloned or created.
async fn forbid_writes(server: &MockServer) {
    for verb in ["POST", "PUT", "DELETE"] {
        Mock::given(method(verb))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(server)
            .await;
    }
}

fn reinstall_from(template: &str) -> VmReinstall {
    VmReinstall {
        template: template.into(),
        ssh_keys: vec![SSH_KEY.into()],
        ..VmReinstall::default()
    }
}

#[tokio::test]
async fn reinstall_refuses_cloning_a_non_template_guest() {
    let server = MockServer::start().await;
    mount_tenant_guests(&server).await;
    forbid_writes(&server).await;
    let p = provider(&server.uri());
    let vm = VmRef::new("101");
    for template in ["102", "200", "4242"] {
        match p.reinstall(&vm, &reinstall_from(template)).await {
            Err(IntegrationError::InvalidInput(m)) => {
                assert!(m.contains("not an available QEMU template"), "{m}");
                assert!(!m.contains("other-customer"), "{m}");
            }
            other => panic!("{template}: expected InvalidInput, got {other:?}"),
        }
        // Create applies the same rule: no full clone of a customer's VM.
        let spec = VmSpec {
            template: template.into(),
            ..qemu_spec()
        };
        match p.create_vm(&spec).await {
            Err(IntegrationError::InvalidInput(m)) => {
                assert!(m.contains("not an available QEMU template"), "create: {m}");
                assert!(!m.contains("other-customer"), "create: {m}");
            }
            other => panic!("create {template}: expected InvalidInput, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn lxc_reinstall_only_accepts_listed_os_template_volumes() {
    let server = MockServer::start().await;
    mount_tenant_guests(&server).await;
    forbid_writes(&server).await;
    Mock::given(method("GET"))
        .and(path("/api2/json/nodes/pve2/storage/local/content"))
        .and(query_param("content", "vztmpl"))
        .respond_with(data(json!([
            {"volid": "local:vztmpl/debian-12-standard_12.7-1_amd64.tar.zst", "content": "vztmpl", "size": 1},
        ])))
        .mount(&server)
        .await;
    let p = provider(&server.uri());
    let ct = VmRef::new("200");
    for template in [
        // A backup of another container, another guest's disk, a path, an ISO.
        "local:backup/vzdump-lxc-201-2024_01_01-00_00_00.tar.zst",
        "local-lvm:vm-201-disk-0",
        "/var/lib/vz/dump/vzdump-lxc-201.tar.zst",
        "local:iso/debian.iso",
        "local:vztmpl/../dump/vzdump-lxc-201.tar.zst",
        // Syntactically fine but not in the storage listing.
        "local:vztmpl/not-uploaded.tar.zst",
    ] {
        match p.reinstall(&ct, &reinstall_from(template)).await {
            Err(IntegrationError::InvalidInput(m)) => {
                assert!(
                    m.contains("not an available LXC OS template"),
                    "{template}: {m}"
                )
            }
            other => panic!("{template}: expected InvalidInput, got {other:?}"),
        }
        // Create applies the same rule to the new container's root disk.
        let spec = VmSpec {
            name: "ct-206".into(),
            node: Some("pve2".into()),
            storage: Some("local-lvm".into()),
            template: template.into(),
            disk_gb: 8,
            extra: json!({"guest_type": "lxc", "vmid": 206}),
            ..VmSpec::default()
        };
        match p.create_vm(&spec).await {
            Err(IntegrationError::InvalidInput(m)) => {
                assert!(m.contains("OS template"), "create {template}: {m}")
            }
            other => panic!("create {template}: expected InvalidInput, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn list_templates_only_offers_templates_and_os_template_volumes() {
    let server = MockServer::start().await;
    mount_tenant_guests(&server).await;
    mount_get(
        &server,
        "/api2/json/nodes/pve1/qemu/9000/config",
        json!({"ostype": "l26"}),
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/api2/json/nodes/pve1/storage"))
        .respond_with(data(json!([{"storage": "local", "shared": 0}])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api2/json/nodes/pve1/storage/local/content"))
        .respond_with(data(json!([
            {"volid": "local:vztmpl/debian-12-standard_12.7-1_amd64.tar.zst", "content": "vztmpl"},
            {"volid": "local:backup/vzdump-lxc-201.tar.zst", "content": "backup"},
            {"volid": "local:vztmpl/odd.tar.zst", "content": "backup"},
        ])))
        .mount(&server)
        .await;
    let ids: Vec<String> = provider(&server.uri())
        .list_templates(Some("pve1"))
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.id)
        .collect();
    assert_eq!(
        ids,
        vec![
            "9000",
            "local:vztmpl/debian-12-standard_12.7-1_amd64.tar.zst"
        ]
    );
}

#[test]
fn api_url_is_checked_against_the_outbound_policy() {
    let secrets = json!({"token_secret": "s3cr3t-token"});
    for url in [
        "https://127.0.0.1:8006",
        "https://2130706433:8006",
        "https://[::1]:8006",
        "http://169.254.169.254",
        "https://localhost:8006",
    ] {
        let cfg = json!({"api_url": url, "token_id": "hostingos@pve!hos"});
        for policy in [OutboundPolicy::Private, OutboundPolicy::Public] {
            match ProxmoxProvider::from_config_with_policy(&cfg, &secrets, policy) {
                Err(IntegrationError::Config(m)) => {
                    assert!(m.contains("HOS_OUTBOUND_POLICY"), "{url}: {m}")
                }
                other => panic!("{url}: expected Config error, got {other:?}"),
            }
        }
    }
    // A LAN hypervisor is fine for a self-hosted company, not for `public`.
    let lan = json!({"api_url": "https://10.0.0.5:8006", "token_id": "hostingos@pve!hos"});
    assert!(
        ProxmoxProvider::from_config_with_policy(&lan, &secrets, OutboundPolicy::Private).is_ok()
    );
    assert!(
        ProxmoxProvider::from_config_with_policy(&lan, &secrets, OutboundPolicy::Public).is_err()
    );
}

#[tokio::test]
async fn redirects_are_not_followed() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api2/json/version"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("location", format!("{}/elsewhere", server.uri())),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/elsewhere"))
        .respond_with(data(json!({"version": "8.2"})))
        .expect(0)
        .mount(&server)
        .await;
    assert!(provider(&server.uri()).test_connection().await.is_err());
}

#[tokio::test]
async fn oversized_responses_are_refused() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api2/json/version"))
        .respond_with(ResponseTemplate::new(200).set_body_string("x".repeat(17 * 1024 * 1024)))
        .expect(1)
        .mount(&server)
        .await;
    match provider(&server.uri()).test_connection().await {
        Err(IntegrationError::Other(m)) => assert!(m.contains("exceeds"), "{m}"),
        other => panic!("expected the body cap, got {other:?}"),
    }
}
