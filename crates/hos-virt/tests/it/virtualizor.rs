//! Virtualizor admin API provider against a wiremock server (no real network).

use hos_core::IntegrationError;
use hos_core::integrations::VirtProvider;
use hos_core::integrations::virt::{
    IpConfig, NetworkSpec, PowerAction, TaskRef, TaskState, VmRef, VmResize, VmSpec,
};
use hos_core::secret::SecretString;
use hos_virt::{OutboundPolicy, VirtualizorProvider};
use serde_json::{Value, json};
use wiremock::matchers::{body_string_contains, method, path, query_param};
use wiremock::{Mock, MockBuilder, MockServer, ResponseTemplate};

const KEY: &str = "VKEY1234567890";
const PASS: &str = "VPASS0987654321";
const SSH_KEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIHostingOSTestKeyMaterial0123 admin@example";

fn provider(server: &MockServer) -> VirtualizorProvider {
    VirtualizorProvider::from_config_with_policy(
        &json!({
            "api_url": server.uri(),
            "default_server_id": 3,
            "default_user_id": 12,
        }),
        &json!({"api_key": KEY, "api_pass": PASS}),
        OutboundPolicy::Unrestricted,
    )
    .unwrap()
}

/// `index.php?act=..&api=json&adminapikey=..&adminapipass=..`
fn act(verb: &str, a: &str) -> MockBuilder {
    Mock::given(method(verb))
        .and(path("/index.php"))
        .and(query_param("act", a))
        .and(query_param("api", "json"))
        .and(query_param("adminapikey", KEY))
        .and(query_param("adminapipass", PASS))
}

fn ok(v: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(v)
}

#[tokio::test]
async fn addvs_full_request() {
    let server = MockServer::start().await;
    act("POST", "addvs")
        .and(body_string_contains("addvps=1"))
        .and(body_string_contains("virt=kvm"))
        .and(body_string_contains("osid=909"))
        .and(body_string_contains("hostname=vps1.example.com"))
        .and(body_string_contains("rootpass=Root-Pass-123"))
        .and(body_string_contains("ram=2048"))
        .and(body_string_contains("space=40"))
        .and(body_string_contains("cores=2"))
        .and(body_string_contains("serid=3"))
        .and(body_string_contains("uid=12"))
        .and(body_string_contains("plid=5"))
        .and(body_string_contains("num_ips=1"))
        .and(body_string_contains("num_ips6=1"))
        .and(body_string_contains("network_speed=12500"))
        .and(body_string_contains("eu_sshkeys=ssh-ed25519+AAAA"))
        .and(body_string_contains("noemail=1"))
        .respond_with(ok(json!({
            "title": "Add VPS",
            "done": "1",
            "actid": "77",
            "vs_info": {"vpsid": "1012", "vps_name": "v1012", "serid": "3",
                        "ips": {"5": "198.51.100.20"}, "ips6": ["2001:db8::20"]}
        })))
        .expect(1)
        .mount(&server)
        .await;
    act("GET", "tasks")
        .and(query_param("actid", "77"))
        .respond_with(ok(json!({"tasks": {"77": {"actid": "77", "status": "1", "progress": "40", "status_txt": "Installing OS"}}})))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    act("GET", "tasks")
        .and(query_param("actid", "77"))
        .respond_with(ok(json!({"tasks": {"77": {"actid": "77", "status": "1", "progress": "100", "status_txt": "Completed"}}})))
        .with_priority(5)
        .mount(&server)
        .await;

    let p = provider(&server);
    let out = p
        .create_vm(&VmSpec {
            name: "svc-1012".into(),
            hostname: "vps1.example.com".into(),
            template: "909".into(),
            plan: Some("5".into()),
            cpu_cores: 2,
            memory_mb: 2048,
            disk_gb: 40,
            network: NetworkSpec {
                ipv4: Some(IpConfig::Auto),
                ipv6: Some(IpConfig::Auto),
                rate_limit_mbps: Some(100),
                ..NetworkSpec::default()
            },
            ssh_keys: vec![SSH_KEY.into()],
            root_password: Some(SecretString::from("Root-Pass-123")),
            ..VmSpec::default()
        })
        .await
        .unwrap();
    assert_eq!(out.vm, VmRef::on_node("3", "1012"));
    assert!(out.root_password.is_none());
    assert_eq!(
        out.ipv4,
        vec!["198.51.100.20".parse::<std::net::IpAddr>().unwrap()]
    );
    assert_eq!(
        out.ipv6,
        vec!["2001:db8::20".parse::<std::net::IpAddr>().unwrap()]
    );
    let task = out.task.unwrap();
    assert_eq!(task.id, "77");
    assert_eq!(
        p.task_status(&task).await.unwrap(),
        TaskState::Running {
            progress_percent: Some(40)
        }
    );
    assert_eq!(p.task_status(&task).await.unwrap(), TaskState::Succeeded);
}

#[tokio::test]
async fn power_operations_and_state_changing_gets() {
    let server = MockServer::start().await;
    for action in ["start", "poweroff", "stop", "restart"] {
        act("GET", "vs")
            .and(query_param("action", action))
            .and(query_param("serid", "3"))
            .and(query_param("vpsid", "1012"))
            .respond_with(ok(json!({"done": 1, "done_msg": "ok", "output": null})))
            .mount(&server)
            .await;
    }
    act("GET", "vs")
        .and(query_param("suspend", "1012"))
        .respond_with(ok(json!({"done": 1})))
        .expect(1)
        .mount(&server)
        .await;
    // State-changing GET answered with 503: must NOT be retried.
    act("GET", "vs")
        .and(query_param("delete", "1012"))
        .respond_with(ResponseTemplate::new(503))
        .expect(1)
        .mount(&server)
        .await;

    let p = provider(&server);
    let vm = VmRef::on_node("3", "1012");
    for a in [
        PowerAction::Start,
        PowerAction::Stop,
        PowerAction::Shutdown,
        PowerAction::Reboot,
        PowerAction::Reset,
    ] {
        assert_eq!(p.power(&vm, a).await.unwrap(), None, "{a:?}");
    }
    let actions: Vec<String> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter_map(|r| {
            r.url
                .query_pairs()
                .find(|(k, _)| k == "action")
                .map(|(_, v)| v.into_owned())
        })
        .collect();
    // Reset = hard power-off + start (no reset verb in the API).
    assert_eq!(
        actions,
        ["start", "poweroff", "stop", "restart", "poweroff", "start"]
    );
    p.suspend(&vm).await.unwrap();
    assert!(matches!(
        p.delete_vm(&vm).await,
        Err(IntegrationError::Remote {
            status: Some(503),
            ..
        })
    ));
    assert!(matches!(
        p.list_snapshots(&vm).await,
        Err(IntegrationError::Unsupported(_))
    ));
}

#[tokio::test]
async fn resize_and_console() {
    let server = MockServer::start().await;
    act("GET", "managevps")
        .and(query_param("vpsid", "1012"))
        .respond_with(ok(json!({"vps": {"vpsid": "1012", "serid": "3", "space": "40", "ram": "2048", "cores": "2"}})))
        .mount(&server)
        .await;
    act("POST", "editvs")
        .and(query_param("vpsid", "1012"))
        .and(body_string_contains("editvps=1"))
        .and(body_string_contains("cores=4"))
        .and(body_string_contains("space=80"))
        .respond_with(ok(json!({"done": {"msg": "VPS updated"}})))
        .expect(1)
        .mount(&server)
        .await;
    act("GET", "vs")
        .and(query_param("vs_status", "1012"))
        .respond_with(ok(json!({"status": {"1012": "1"}})))
        .mount(&server)
        .await;
    act("GET", "vs")
        .and(query_param("action", "restart"))
        .respond_with(ok(json!({"done": 1})))
        .expect(1)
        .mount(&server)
        .await;
    act("GET", "vnc")
        .and(query_param("novnc", "1012"))
        .respond_with(ok(json!({"info": {"vnc_hostname": "node3.example.com", "port": "5901", "virt": "kvm", "password": "vncpw"}})))
        .mount(&server)
        .await;

    let p = provider(&server);
    let vm = VmRef::on_node("3", "1012");
    assert!(matches!(
        p.resize(
            &vm,
            &VmResize {
                disk_gb: Some(20),
                ..VmResize::default()
            }
        )
        .await,
        Err(IntegrationError::InvalidInput(_))
    ));
    p.resize(
        &vm,
        &VmResize {
            cpu_cores: Some(4),
            disk_gb: Some(80),
            allow_restart: true,
            ..VmResize::default()
        },
    )
    .await
    .unwrap();
    let console = p.console(&vm).await.unwrap();
    assert_eq!(console.url, "vnc://node3.example.com:5901");
    assert_eq!(console.password.unwrap().expose_str(), "vncpw");
}

#[tokio::test]
async fn errors_on_http_200_and_status_mapping() {
    let server = MockServer::start().await;
    act("GET", "vs")
        .and(query_param("action", "start"))
        .and(query_param("vpsid", "1"))
        .respond_with(ok(json!({"error": {"0": "The VPS does not exist"}})))
        .mount(&server)
        .await;
    act("GET", "vs")
        .and(query_param("action", "start"))
        .and(query_param("vpsid", "2"))
        .respond_with(ok(json!({"error": ["The VPS is already running"]})))
        .mount(&server)
        .await;
    act("GET", "vs")
        .and(query_param("action", "start"))
        .and(query_param("vpsid", "3"))
        .respond_with(ok(json!({"error": [format!("Invalid API key {KEY}")]})))
        .mount(&server)
        .await;
    act("GET", "vs")
        .and(query_param("action", "start"))
        .and(query_param("vpsid", "4"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<html><body>Login</body></html>"))
        .mount(&server)
        .await;
    act("GET", "vs")
        .and(query_param("action", "start"))
        .and(query_param("vpsid", "5"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    act("GET", "vs")
        .and(query_param("action", "start"))
        .and(query_param("vpsid", "6"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    act("GET", "vs")
        .and(query_param("action", "start"))
        .and(query_param("vpsid", "7"))
        .respond_with(ok(json!({"done": 0})))
        .mount(&server)
        .await;
    // Idempotent read with 500 → retried, then Remote.
    act("GET", "servers")
        .respond_with(ResponseTemplate::new(500))
        .expect(3)
        .mount(&server)
        .await;

    let p = provider(&server);
    let start = |id: &'static str| {
        let p = &p;
        async move { p.power(&VmRef::new(id), PowerAction::Start).await }
    };
    assert!(matches!(
        start("1").await,
        Err(IntegrationError::NotFound(_))
    ));
    assert!(matches!(
        start("2").await,
        Err(IntegrationError::Conflict(_))
    ));
    match start("3").await {
        Err(e @ IntegrationError::Auth(_)) => {
            let text = e.to_string();
            assert!(!text.contains(KEY), "{text}");
        }
        other => panic!("expected Auth, got {other:?}"),
    }
    assert!(
        matches!(start("4").await, Err(IntegrationError::Transport(m)) if m.contains("non-JSON"))
    );
    assert!(matches!(start("5").await, Err(IntegrationError::Auth(_))));
    assert!(matches!(
        start("6").await,
        Err(IntegrationError::NotFound(_))
    ));
    assert!(matches!(
        start("7").await,
        Err(IntegrationError::Remote { status: None, .. })
    ));
    match p.list_nodes().await {
        Err(
            e @ IntegrationError::Remote {
                status: Some(500), ..
            },
        ) => {
            let text = e.to_string();
            assert!(!text.contains(KEY) && !text.contains(PASS), "{text}");
        }
        other => panic!("expected Remote 500, got {other:?}"),
    }
    assert!(matches!(
        p.task_status(&TaskRef {
            id: "abc".into(),
            node: None
        })
        .await,
        Err(IntegrationError::InvalidInput(_))
    ));
}

// ---------------------------------------------------------------------------
// Reinstall source validation: only offered OS templates.
// ---------------------------------------------------------------------------

async fn mount_ostemplates(server: &MockServer) {
    act("GET", "ostemplates")
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ostemplates": {
            "100": {"osid": 100, "type": "kvm", "name": "ubuntu-24.04-x86_64", "distro": "ubuntu"},
            "200": {"osid": 200, "type": "openvz", "name": "centos-7", "distro": "centos"}
        }})))
        .mount(server)
        .await;
}

fn reinstall_from(template: &str) -> hos_core::integrations::virt::VmReinstall {
    hos_core::integrations::virt::VmReinstall {
        template: template.into(),
        root_password: Some(SecretString::from("N3w-Root-Pass!")),
        ..Default::default()
    }
}

#[tokio::test]
async fn reinstall_refuses_unlisted_os_templates() {
    let server = MockServer::start().await;
    mount_ostemplates(&server).await;
    act("POST", "rebuild")
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"done": 1})))
        .expect(0)
        .mount(&server)
        .await;
    let p = provider(&server);
    let vm = VmRef::on_node("3", "1012");
    // 200 is another virtualisation type; 999 does not exist.
    for template in ["200", "999"] {
        match p.reinstall(&vm, &reinstall_from(template)).await {
            Err(IntegrationError::InvalidInput(m)) => {
                assert!(m.contains("not an available OS template"), "{m}")
            }
            other => panic!("{template}: expected InvalidInput, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn reinstall_with_a_listed_os_template_rebuilds() {
    let server = MockServer::start().await;
    mount_ostemplates(&server).await;
    act("POST", "rebuild")
        .and(body_string_contains("osid=100"))
        .and(body_string_contains("vpsid=1012"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"done": 1, "actid": 55})))
        .expect(1)
        .mount(&server)
        .await;
    let out = provider(&server)
        .reinstall(&VmRef::on_node("3", "1012"), &reinstall_from("100"))
        .await
        .unwrap();
    assert_eq!(out.task.map(|t| t.id), Some("55".to_owned()));
}
