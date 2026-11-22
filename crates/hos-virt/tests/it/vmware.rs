//! VMware vCenter provider against a wiremock server (no real network).

use hos_core::IntegrationError;
use hos_core::integrations::VirtProvider;
use hos_core::integrations::virt::{
    ConsoleKind, IpConfig, NetworkSpec, PowerAction, SnapshotRequest, VmRef, VmSpec,
};
use hos_core::secret::SecretString;
use hos_virt::{OutboundPolicy, VmwareProvider};
use serde_json::{Value, json};
use wiremock::matchers::{
    body_partial_json, body_string_contains, header, method, path, query_param,
};
use wiremock::{Mock, MockServer, ResponseTemplate};

const USER: &str = "hostingos@vsphere.local";
const PASSWORD: &str = "VmwPass-1";
/// base64("hostingos@vsphere.local:VmwPass-1")
const BASIC: &str = "Basic aG9zdGluZ29zQHZzcGhlcmUubG9jYWw6Vm13UGFzcy0x";
const SID: &str = "vmware-api-session-id";
const REL: &str = "8.0.2.0";

fn provider(server: &MockServer, extra: Value) -> VmwareProvider {
    let mut cfg = json!({
        "api_url": server.uri(),
        "username": USER,
        "vm_folder": "group-v3",
        "datastore": "datastore-12",
        "network": "dvportgroup-21",
        "task_poll_ms": 10,
        "task_timeout_secs": 5,
    });
    for (k, v) in extra.as_object().unwrap() {
        cfg[k] = v.clone();
    }
    VmwareProvider::from_config_with_policy(
        &cfg,
        &json!({ "password": PASSWORD }),
        OutboundPolicy::Unrestricted,
    )
    .unwrap()
}

fn json_string(s: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!(s))
}

fn vapi_error(status: u16, error_type: &str, message: &str) -> ResponseTemplate {
    ResponseTemplate::new(status).set_body_json(json!({
        "error_type": error_type,
        "messages": [{"args": [], "default_message": message, "id": "com.vmware.api.vcenter.error"}]
    }))
}

fn moref(kind: &str, value: &str) -> Value {
    json!({"_typeName": "ManagedObjectReference", "type": kind, "value": value})
}

async fn mount_login(server: &MockServer, session: &str) {
    Mock::given(method("POST"))
        .and(path("/api/session"))
        .and(header("authorization", BASIC))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!(session)))
        .mount(server)
        .await;
}

async fn mount_vim_login(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path(format!(
            "/sdk/vim25/{REL}/SessionManager/SessionManager/Login"
        )))
        .and(body_partial_json(
            json!({"userName": USER, "password": PASSWORD}),
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header(SID, "vim-sess-1")
                .set_body_json(json!({"_typeName": "UserSession", "userName": USER})),
        )
        .expect(1)
        .mount(server)
        .await;
}

fn vm_info(power: &str, disk_bytes: u64) -> Value {
    json!({
        "name": "web42",
        "power_state": power,
        "guest_OS": "UBUNTU_64",
        "cpu": {"count": 2, "cores_per_socket": 1},
        "memory": {"size_MiB": 2048},
        "disks": {"2000": {"label": "Hard disk 1", "capacity": disk_bytes}},
        "nics": {"4000": {"label": "Network adapter 1", "mac_address": "00:50:56:aa:bb:cc"}},
        "identity": {"name": "web42", "instance_uuid": "5030c2b4-0000-0000-0000-000000000001"}
    })
}

#[tokio::test]
async fn library_deploy_full_sequence() {
    let server = MockServer::start().await;
    mount_login(&server, "sess-1").await;
    mount_vim_login(&server).await;
    // The source is verified first: a VM template of the content library.
    Mock::given(method("GET"))
        .and(path("/api/content/library/item/lib-item-1"))
        .and(header(SID, "sess-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "lib-item-1", "type": "vm-template", "library_id": "lib-1", "name": "ubuntu"
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/vcenter/vm-template/library-items/lib-item-1"))
        .and(query_param("action", "deploy"))
        .and(header(SID, "sess-1"))
        .and(body_partial_json(json!({
            "name": "web42",
            "placement": {"folder": "group-v3"},
            "powered_on": false,
            "disk_storage": {"datastore": "datastore-12"},
            "hardware_customization": {"cpu_update": {"num_cpus": 2}, "memory_update": {"memory": 2048}}
        })))
        .respond_with(json_string("vm-1001"))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/vcenter/vm/vm-1001"))
        .and(header(SID, "sess-1"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(vm_info("POWERED_OFF", 10 * 1024 * 1024 * 1024)),
        )
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/api/vcenter/vm/vm-1001/hardware/ethernet/4000"))
        .and(body_partial_json(
            json!({"backing": {"type": "DISTRIBUTED_PORTGROUP", "network": "dvportgroup-21"}}),
        ))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/sdk/vim25/{REL}/VirtualMachine/vm-1001/config")))
        .and(header(SID, "vim-sess-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "_typeName": "VirtualMachineConfigInfo",
            "hardware": {"device": [
                {"_typeName": "VirtualE1000", "key": 4000},
                {"_typeName": "VirtualDisk", "key": 2000, "controllerKey": 1000, "unitNumber": 0,
                 "capacityInKB": 10_485_760_u64, "capacityInBytes": 10_737_418_240_u64,
                 "backing": {"_typeName": "VirtualDiskFlatVer2BackingInfo", "fileName": "[ds1] web42/web42.vmdk"}}
            ]}
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!(
            "/sdk/vim25/{REL}/VirtualMachine/vm-1001/ReconfigVM_Task"
        )))
        .and(header(SID, "vim-sess-1"))
        .and(body_partial_json(
            json!({"spec": {"deviceChange": [{"operation": "edit",
            "device": {"key": 2000, "capacityInBytes": 21_474_836_480_u64}}]}}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(moref("Task", "task-501")))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/sdk/vim25/{REL}/Task/task-501/info")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"_typeName": "TaskInfo", "state": "success"})),
        )
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/api/vcenter/vm/vm-1001/guest/customization"))
        .and(body_string_contains("CLOUDINIT"))
        .and(body_string_contains("local-hostname"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/vcenter/vm/vm-1001/power"))
        .and(query_param("action", "start"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    let p = provider(&server, json!({"vijson_release": REL}));
    let out = p
        .create_vm(&VmSpec {
            name: "web42".into(),
            hostname: "web42.example.com".into(),
            template: "lib-item-1".into(),
            cpu_cores: 2,
            memory_mb: 2048,
            disk_gb: 20,
            network: NetworkSpec {
                ipv4: Some(IpConfig::Dhcp),
                ..NetworkSpec::default()
            },
            root_password: Some(SecretString::from("Root-Pass-123")),
            ..VmSpec::default()
        })
        .await
        .unwrap();
    assert_eq!(out.vm, VmRef::new("vm-1001"));
    assert!(out.task.is_none());
    assert_eq!(out.extra["warnings"], json!([]));

    let order: Vec<String> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.method.as_str() != "GET")
        .map(|r| format!("{} {}", r.method, r.url.path()))
        .collect();
    assert_eq!(
        order,
        [
            "POST /api/session".to_owned(),
            "POST /api/vcenter/vm-template/library-items/lib-item-1".to_owned(),
            "PATCH /api/vcenter/vm/vm-1001/hardware/ethernet/4000".to_owned(),
            format!("POST /sdk/vim25/{REL}/SessionManager/SessionManager/Login"),
            format!("POST /sdk/vim25/{REL}/VirtualMachine/vm-1001/ReconfigVM_Task"),
            "PUT /api/vcenter/vm/vm-1001/guest/customization".to_owned(),
            "POST /api/vcenter/vm/vm-1001/power".to_owned(),
        ]
    );
}

#[tokio::test]
async fn clone_failure_cleans_up_the_vm() {
    let server = MockServer::start().await;
    mount_login(&server, "sess-1").await;
    // Cloning a MoRef needs VI/JSON to verify that the source is a template.
    mount_vim_login(&server).await;
    Mock::given(method("GET"))
        .and(path(format!("/sdk/vim25/{REL}/VirtualMachine/vm-500/summary")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "_typeName": "VirtualMachineSummary",
            "config": {"_typeName": "VirtualMachineConfigSummary", "name": "ubuntu-tpl", "template": true}
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/vcenter/vm"))
        .and(query_param("action", "clone"))
        .and(body_partial_json(json!({
            "source": "vm-500", "name": "web43", "power_on": false,
            "placement": {"folder": "group-v3", "datastore": "datastore-12"}
        })))
        .respond_with(json_string("vm-1002"))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/api/vcenter/vm/vm-1002/hardware/cpu"))
        .and(body_partial_json(json!({"count": 4})))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/api/vcenter/vm/vm-1002/hardware/memory"))
        .and(body_partial_json(json!({"size_MiB": 4096})))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/vcenter/vm/vm-1002"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(vm_info("POWERED_OFF", 40 * 1024 * 1024 * 1024)),
        )
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/api/vcenter/vm/vm-1002/hardware/ethernet/4000"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/api/vcenter/vm/vm-1002/guest/customization"))
        .and(body_string_contains("linux_config"))
        .respond_with(vapi_error(
            400,
            "INVALID_ARGUMENT",
            "Customization of the guest OS is not supported",
        ))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/vcenter/vm/vm-1002/power"))
        .and(query_param("action", "stop"))
        .respond_with(vapi_error(
            400,
            "ALREADY_IN_DESIRED_STATE",
            "Virtual machine is already powered off.",
        ))
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/api/vcenter/vm/vm-1002"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    let p = provider(
        &server,
        json!({"customization": "linux", "vijson_release": REL}),
    );
    let err = p
        .create_vm(&VmSpec {
            name: "web43".into(),
            hostname: "web43.example.com".into(),
            template: "vm-500".into(),
            cpu_cores: 4,
            memory_mb: 4096,
            disk_gb: 40,
            network: NetworkSpec {
                ipv4: Some(IpConfig::Dhcp),
                ..NetworkSpec::default()
            },
            ..VmSpec::default()
        })
        .await
        .unwrap_err();
    assert!(
        matches!(&err, IntegrationError::InvalidInput(m) if m.starts_with("INVALID_ARGUMENT")),
        "{err:?}"
    );
}

#[tokio::test]
async fn power_console_and_expired_session() {
    let server = MockServer::start().await;
    // First login → sess-1 (later rejected as expired), second → sess-2.
    Mock::given(method("POST"))
        .and(path("/api/session"))
        .and(header("authorization", BASIC))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!("sess-1")))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    mount_login(&server, "sess-2").await;
    Mock::given(method("POST"))
        .and(path("/api/vcenter/vm/vm-7/power"))
        .and(header(SID, "sess-1"))
        .respond_with(vapi_error(
            401,
            "UNAUTHENTICATED",
            "This method requires authentication.",
        ))
        .expect(1)
        .mount(&server)
        .await;
    for action in ["start", "stop", "reset"] {
        Mock::given(method("POST"))
            .and(path("/api/vcenter/vm/vm-7/power"))
            .and(header(SID, "sess-2"))
            .and(query_param("action", action))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
    }
    for action in ["shutdown", "reboot"] {
        Mock::given(method("POST"))
            .and(path("/api/vcenter/vm/vm-7/guest/power"))
            .and(header(SID, "sess-2"))
            .and(query_param("action", action))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
    }
    Mock::given(method("POST"))
        .and(path("/api/vcenter/vm/vm-7/console/tickets"))
        .and(body_partial_json(json!({"type": "WEBMKS"})))
        .respond_with(
            ResponseTemplate::new(201)
                .set_body_json(json!({"ticket": "wss://esx01.example.com:443/ticket/0123abcd"})),
        )
        .mount(&server)
        .await;

    let p = provider(&server, json!({}));
    let vm = VmRef::new("vm-7");
    for a in [
        PowerAction::Start,
        PowerAction::Stop,
        PowerAction::Reset,
        PowerAction::Shutdown,
        PowerAction::Reboot,
    ] {
        assert_eq!(p.power(&vm, a).await.unwrap(), None, "{a:?}");
    }
    let requests = server.received_requests().await.unwrap();
    let logins = requests
        .iter()
        .filter(|r| r.url.path() == "/api/session")
        .count();
    assert_eq!(logins, 2, "expired session triggers exactly one re-login");
    let paths: Vec<String> = requests
        .iter()
        .filter(|r| r.url.path() != "/api/session")
        .map(|r| format!("{}?{}", r.url.path(), r.url.query().unwrap_or("")))
        .collect();
    assert_eq!(
        paths,
        [
            "/api/vcenter/vm/vm-7/power?action=start",
            "/api/vcenter/vm/vm-7/power?action=start",
            "/api/vcenter/vm/vm-7/power?action=stop",
            "/api/vcenter/vm/vm-7/power?action=reset",
            "/api/vcenter/vm/vm-7/guest/power?action=shutdown",
            "/api/vcenter/vm/vm-7/guest/power?action=reboot",
        ]
    );

    let console = p.console(&vm).await.unwrap();
    assert_eq!(console.kind, ConsoleKind::WebMks);
    assert_eq!(console.url, "wss://esx01.example.com:443/ticket/0123abcd");

    // Without VI/JSON there is no snapshot API.
    assert!(matches!(
        p.list_snapshots(&vm).await,
        Err(IntegrationError::Unsupported(_))
    ));
}

#[tokio::test]
async fn snapshots_via_vijson() {
    let server = MockServer::start().await;
    mount_vim_login(&server).await;
    Mock::given(method("POST"))
        .and(path(format!(
            "/sdk/vim25/{REL}/VirtualMachine/vm-7/CreateSnapshot_Task"
        )))
        .and(header(SID, "vim-sess-1"))
        .and(body_partial_json(
            json!({"name": "before", "memory": true, "quiesce": false}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(moref("Task", "task-601")))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/sdk/vim25/{REL}/Task/task-601/info")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "_typeName": "TaskInfo", "state": "success",
            "result": moref("VirtualMachineSnapshot", "snapshot-77")
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!(
            "/sdk/vim25/{REL}/VirtualMachine/vm-7/snapshot"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "_typeName": "VirtualMachineSnapshotInfo",
            "currentSnapshot": moref("VirtualMachineSnapshot", "snapshot-77"),
            "rootSnapshotList": [{
                "_typeName": "VirtualMachineSnapshotTree",
                "snapshot": moref("VirtualMachineSnapshot", "snapshot-77"),
                "name": "before", "description": "", "createTime": "2026-11-16T10:00:00Z",
                "state": "poweredOn", "childSnapshotList": []
            }]
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!(
            "/sdk/vim25/{REL}/VirtualMachineSnapshot/snapshot-77/RevertToSnapshot_Task"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(moref("Task", "task-602")))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/sdk/vim25/{REL}/Task/task-602/info")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "_typeName": "TaskInfo", "state": "error",
            "error": {"_typeName": "LocalizedMethodFault", "localizedMessage": "Insufficient disk space",
                      "fault": {"_typeName": "InsufficientResourcesFault"}}
        })))
        .mount(&server)
        .await;

    let p = provider(&server, json!({"vijson_release": REL}));
    let vm = VmRef::new("vm-7");
    let created = p
        .create_snapshot(
            &vm,
            &SnapshotRequest {
                name: "before".into(),
                description: None,
                include_memory: true,
            },
        )
        .await
        .unwrap();
    assert_eq!(created.snapshot_id, "snapshot-77");
    let list = p.list_snapshots(&vm).await.unwrap();
    assert_eq!(list.len(), 1);
    assert!(list[0].includes_memory);
    // A snapshot of another VM is refused before any task is started.
    assert!(matches!(
        p.rollback_snapshot(&vm, "snapshot-99").await,
        Err(IntegrationError::NotFound(_))
    ));
    match p.rollback_snapshot(&vm, "snapshot-77").await {
        Err(IntegrationError::Remote { code, message, .. }) => {
            assert_eq!(code.as_deref(), Some("InsufficientResourcesFault"));
            assert_eq!(message, "Insufficient disk space");
        }
        other => panic!("expected task failure, got {other:?}"),
    }
}

#[tokio::test]
async fn error_mapping() {
    let server = MockServer::start().await;
    mount_login(&server, "sess-1").await;
    Mock::given(method("GET"))
        .and(path("/api/vcenter/vm/vm-404"))
        .respond_with(vapi_error(
            404,
            "NOT_FOUND",
            "The virtual machine was not found.",
        ))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/vcenter/vm/vm-500"))
        .respond_with(vapi_error(500, "ERROR", "A general system error occurred."))
        .expect(3)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/vcenter/vm/vm-401"))
        .respond_with(vapi_error(
            401,
            "UNAUTHENTICATED",
            "Authentication required.",
        ))
        .expect(2)
        .mount(&server)
        .await;
    let p = provider(&server, json!({}));
    assert!(matches!(
        p.get_vm(&VmRef::new("vm-404")).await,
        Err(IntegrationError::NotFound(m)) if m.contains("not found")
    ));
    // 5xx on a GET is retried (3 attempts), then Remote with the vAPI error type as code.
    match p.get_vm(&VmRef::new("vm-500")).await {
        Err(IntegrationError::Remote { status, code, .. }) => {
            assert_eq!(status, Some(500));
            assert_eq!(code.as_deref(), Some("ERROR"));
        }
        other => panic!("expected Remote, got {other:?}"),
    }
    // 401 → one re-login, then Auth.
    assert!(matches!(
        p.get_vm(&VmRef::new("vm-401")).await,
        Err(IntegrationError::Auth(_))
    ));

    // Wrong credentials.
    let bad = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/session"))
        .respond_with(vapi_error(
            401,
            "UNAUTHENTICATED",
            "Authentication required.",
        ))
        .mount(&bad)
        .await;
    match provider(&bad, json!({})).test_connection().await {
        Err(e @ IntegrationError::Auth(_)) => assert!(!e.to_string().contains(PASSWORD)),
        other => panic!("expected Auth, got {other:?}"),
    }

    // No /api/session at all (standalone ESXi or vCenter < 7.0 U2).
    let esxi_like = MockServer::start().await;
    assert!(matches!(
        provider(&esxi_like, json!({})).test_connection().await,
        Err(IntegrationError::Config(m)) if m.contains("ESXi")
    ));
}

#[tokio::test]
async fn standalone_esxi_is_rejected_without_requests() {
    let server = MockServer::start().await;
    let p = provider(&server, json!({"deployment": "esxi"}));
    assert_eq!(p.capabilities(), Default::default());
    assert!(matches!(
        p.test_connection().await,
        Err(IntegrationError::Config(m)) if m.contains("vCenter")
    ));
    assert!(matches!(
        p.list_vms(None).await,
        Err(IntegrationError::Unsupported(_))
    ));
    assert!(matches!(
        p.power(&VmRef::new("1"), PowerAction::Start).await,
        Err(IntegrationError::Unsupported(_))
    ));
    assert!(server.received_requests().await.unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// Reinstall source validation: only VM templates may be cloned.
// ---------------------------------------------------------------------------

fn reinstall_from(template: &str) -> hos_core::integrations::virt::VmReinstall {
    hos_core::integrations::virt::VmReinstall {
        template: template.into(),
        ..Default::default()
    }
}

/// No request may stop, delete or (re)deploy anything.
async fn assert_no_writes(server: &MockServer) {
    for r in server.received_requests().await.unwrap() {
        let p = r.url.path();
        let destructive = r.method.as_str() == "DELETE"
            || p.contains("/power")
            || p.ends_with("/api/vcenter/vm")
            || p.contains("library-items");
        assert!(!destructive, "unexpected {} {p}", r.method);
    }
}

#[tokio::test]
async fn reinstall_refuses_cloning_a_regular_vm() {
    // Without VI/JSON the template flag cannot be verified: refused upfront.
    let server = MockServer::start().await;
    let p = provider(&server, json!({}));
    match p
        .reinstall(&VmRef::new("vm-1"), &reinstall_from("vm-202"))
        .await
    {
        Err(IntegrationError::InvalidInput(m)) => assert!(m.contains("vijson_release"), "{m}"),
        other => panic!("expected InvalidInput, got {other:?}"),
    }
    // Create applies the same rule.
    match p.create_vm(&create_from("vm-202")).await {
        Err(IntegrationError::InvalidInput(m)) => assert!(m.contains("vijson_release"), "{m}"),
        other => panic!("create: expected InvalidInput, got {other:?}"),
    }
    assert!(server.received_requests().await.unwrap().is_empty());

    // With VI/JSON: a VM whose summary says `template: false` is refused.
    let server = MockServer::start().await;
    mount_vim_login(&server).await;
    Mock::given(method("GET"))
        .and(path(format!("/sdk/vim25/{REL}/VirtualMachine/vm-202/summary")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "_typeName": "VirtualMachineSummary",
            "config": {"_typeName": "VirtualMachineConfigSummary", "name": "other-customer", "template": false}
        })))
        .expect(2)
        .mount(&server)
        .await;
    let p = provider(&server, json!({"vijson_release": REL}));
    match p
        .reinstall(&VmRef::new("vm-1"), &reinstall_from("vm-202"))
        .await
    {
        Err(IntegrationError::InvalidInput(m)) => {
            assert!(m.contains("not an available VM template"), "{m}")
        }
        other => panic!("expected InvalidInput, got {other:?}"),
    }
    match p.create_vm(&create_from("vm-202")).await {
        Err(IntegrationError::InvalidInput(m)) => {
            assert!(m.contains("not an available VM template"), "create: {m}")
        }
        other => panic!("create: expected InvalidInput, got {other:?}"),
    }
    assert_no_writes(&server).await;
}

#[tokio::test]
async fn reinstall_only_accepts_vm_template_library_items() {
    let server = MockServer::start().await;
    mount_login(&server, "sess-1").await;
    Mock::given(method("GET"))
        .and(path("/api/content/library/item/ovf-item"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "ovf-item", "type": "ovf", "library_id": "lib-1", "name": "appliance"
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/content/library/item/foreign-item"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "foreign-item", "type": "vm-template", "library_id": "lib-other", "name": "x"
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/content/library/item/missing"))
        .respond_with(vapi_error(404, "NOT_FOUND", "not found"))
        .mount(&server)
        .await;
    let p = provider(&server, json!({"content_library": "lib-1"}));
    for template in ["ovf-item", "foreign-item", "missing"] {
        match p
            .reinstall(&VmRef::new("vm-1"), &reinstall_from(template))
            .await
        {
            Err(IntegrationError::InvalidInput(m)) => {
                assert!(
                    m.contains("not an available VM template"),
                    "{template}: {m}"
                )
            }
            other => panic!("{template}: expected InvalidInput, got {other:?}"),
        }
        match p.create_vm(&create_from(template)).await {
            Err(IntegrationError::InvalidInput(m)) => {
                assert!(
                    m.contains("not an available VM template"),
                    "create {template}: {m}"
                )
            }
            other => panic!("create {template}: expected InvalidInput, got {other:?}"),
        }
    }
    assert_no_writes(&server).await;
}

fn create_from(template: &str) -> VmSpec {
    VmSpec {
        name: "web44".into(),
        hostname: "web44.example.com".into(),
        template: template.into(),
        cpu_cores: 1,
        memory_mb: 1024,
        disk_gb: 20,
        ..VmSpec::default()
    }
}

#[test]
fn vcenter_url_is_checked_against_the_outbound_policy() {
    let cfg = json!({"api_url": "https://169.254.169.254", "username": USER});
    assert!(matches!(
        VmwareProvider::from_config_with_policy(
            &cfg,
            &json!({ "password": PASSWORD }),
            OutboundPolicy::Private
        ),
        Err(IntegrationError::Config(_))
    ));
}
