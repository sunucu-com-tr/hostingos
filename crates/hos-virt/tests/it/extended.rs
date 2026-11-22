//! Extended VM operations (console upstream, password reset, metrics
//! history, bandwidth, backups, rDNS, ISO, rescue, firewall) of every driver
//! against wiremock servers, plus the console upstream connector against a
//! local fake websocket VNC server. No real network.

use futures::{SinkExt, StreamExt};
use hos_core::IntegrationError;
use hos_core::integrations::VirtProvider;
use hos_core::integrations::virt::{
    ConsoleTransport, ConsoleUpstream, FirewallDirection, FirewallProtocol, FirewallRule,
    FirewallState, MetricsRange, PasswordResetRequest, VmRef,
};
use hos_core::secret::SecretString;
use hos_virt::console::{self, ConnectOptions};
use hos_virt::{
    HetznerProvider, MockProvider, OutboundPolicy, ProxmoxProvider, VirtualizorProvider,
};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use wiremock::matchers::{
    body_partial_json, body_string_contains, header, method, path, query_param,
};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ---------------------------------------------------------------------------
// Proxmox
// ---------------------------------------------------------------------------

const PVE_AUTH: &str = "PVEAPIToken=hostingos@pve!hos=s3cr3t-token";

fn pve(uri: &str) -> ProxmoxProvider {
    ProxmoxProvider::from_config_with_policy(
        &json!({"api_url": uri, "token_id": "hostingos@pve!hos", "task_poll_ms": 10,
                "task_timeout_secs": 5, "skip_tls_verify": true}),
        &json!({"token_secret": "s3cr3t-token"}),
        OutboundPolicy::Unrestricted,
    )
    .unwrap()
}

fn data(v: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({ "data": v }))
}

async fn pve_server() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api2/json/cluster/resources"))
        .and(header("authorization", PVE_AUTH))
        .respond_with(data(json!([
            {"type": "qemu", "node": "pve1", "vmid": 101, "status": "running", "name": "web1"},
            {"type": "qemu", "node": "pve1", "vmid": 102, "status": "stopped", "name": "web2"},
            {"type": "lxc", "node": "pve2", "vmid": 200, "status": "running", "name": "ct1"},
        ])))
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn proxmox_console_upstream_keeps_the_ticket_server_side() {
    let server = pve_server().await;
    Mock::given(method("POST"))
        .and(path("/api2/json/nodes/pve1/qemu/101/vncproxy"))
        .and(header("authorization", PVE_AUTH))
        .and(body_string_contains("websocket=1"))
        .respond_with(data(json!({"port": 5901, "ticket": "PVEVNC:ABC::tick/et", "password": "vncpass1",
                                  "upid": "UPID:pve1:1:2:3:vncproxy:101:x:", "user": "hostingos@pve!hos", "cert": "-"})))
        .expect(1)
        .mount(&server)
        .await;
    let up = pve(&server.uri())
        .console_upstream(&VmRef::new("101"))
        .await
        .unwrap();
    assert_eq!(up.transport, ConsoleTransport::WebSocket);
    assert!(up.url.starts_with("ws://"), "{}", up.url);
    assert!(up.url.contains("/api2/json/nodes/pve1/qemu/101/vncwebsocket?port=5901&vncticket=PVEVNC%3AABC%3A%3Atick%2Fet"));
    assert_eq!(up.vnc_password.as_ref().unwrap().expose_str(), "vncpass1");
    assert_eq!(up.headers[0].0, "Authorization");
    assert_eq!(up.headers[0].1.expose_str(), PVE_AUTH);
    assert_eq!(up.subprotocols, vec!["binary".to_owned()]);
    assert!(up.accept_invalid_certs);
    assert!(up.expires_at.is_some());
    // The token never shows up in Debug output.
    assert!(!format!("{up:?}").contains("s3cr3t"));
}

#[tokio::test]
async fn proxmox_console_upstream_error_is_mapped() {
    let server = pve_server().await;
    Mock::given(method("POST"))
        .and(path("/api2/json/nodes/pve1/qemu/101/vncproxy"))
        .respond_with(ResponseTemplate::new(403).set_body_json(
            json!({"data": null, "message": "Permission check failed (/vms/101, VM.Console)"}),
        ))
        .mount(&server)
        .await;
    let err = pve(&server.uri())
        .console_upstream(&VmRef::new("101"))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), "auth", "{err:?}");
}

#[tokio::test]
async fn proxmox_password_reset_via_guest_agent() {
    let server = pve_server().await;
    Mock::given(method("POST"))
        .and(path(
            "/api2/json/nodes/pve1/qemu/101/agent/set-user-password",
        ))
        .and(header("authorization", PVE_AUTH))
        .and(body_string_contains("username=root"))
        .and(body_string_contains("password=N3w-Pass-word"))
        .respond_with(data(json!({"result": {}})))
        .expect(1)
        .mount(&server)
        .await;
    let res = pve(&server.uri())
        .reset_password(
            &VmRef::new("101"),
            &PasswordResetRequest {
                username: None,
                password: Some(SecretString::from("N3w-Pass-word")),
            },
        )
        .await
        .unwrap();
    assert_eq!(res.username, "root");
    assert_eq!(res.password.expose_str(), "N3w-Pass-word");
    assert!(res.task.is_none());
}

#[tokio::test]
async fn proxmox_password_reset_without_agent_is_unsupported_and_redacted() {
    let server = pve_server().await;
    Mock::given(method("POST"))
        .and(path(
            "/api2/json/nodes/pve1/qemu/101/agent/set-user-password",
        ))
        .respond_with(
            ResponseTemplate::new(500)
                .set_body_json(json!({"data": null, "message": "QEMU guest agent is not running"})),
        )
        .mount(&server)
        .await;
    let p = pve(&server.uri());
    let req = PasswordResetRequest {
        username: None,
        password: Some(SecretString::from("Secret-123456")),
    };
    let err = p
        .reset_password(&VmRef::new("101"), &req)
        .await
        .unwrap_err();
    assert!(matches!(err, IntegrationError::Unsupported(_)), "{err:?}");
    // Stopped VM and LXC are refused before any call.
    let err = p
        .reset_password(&VmRef::new("102"), &req)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), "conflict");
    let err = p
        .reset_password(&VmRef::new("200"), &req)
        .await
        .unwrap_err();
    assert!(matches!(err, IntegrationError::Unsupported(_)));
}

#[tokio::test]
async fn proxmox_metrics_history_and_bandwidth() {
    let server = pve_server().await;
    let now = chrono::Utc::now().timestamp();
    let rows: Vec<Value> = (0..5)
        .map(|i| {
            json!({"time": now - (4 - i) * 60, "cpu": 0.25, "maxcpu": 2, "mem": 1_073_741_824_u64,
                   "maxmem": 2_147_483_648_u64, "diskread": 100.0, "diskwrite": 200.0,
                   "netin": 1000.0, "netout": 2000.0})
        })
        .collect();
    Mock::given(method("GET"))
        .and(path("/api2/json/nodes/pve1/qemu/101/rrddata"))
        .and(query_param("timeframe", "hour"))
        .and(query_param("cf", "AVERAGE"))
        .respond_with(data(json!(rows)))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api2/json/nodes/pve1/qemu/101/rrddata"))
        .and(query_param("timeframe", "month"))
        .respond_with(data(json!([
            {"time": now - 7200, "netin": 1000.0, "netout": 3000.0},
            {"time": now - 3600, "netin": 1000.0, "netout": 3000.0},
        ])))
        .mount(&server)
        .await;
    let p = pve(&server.uri());
    let h = p
        .metrics_history(&VmRef::new("101"), MetricsRange::Hour)
        .await
        .unwrap();
    assert_eq!(h.samples.len(), 5);
    assert_eq!(h.step_secs, 60);
    let s = &h.samples[0];
    assert_eq!(s.cpu_percent, Some(25.0));
    assert_eq!(s.memory_used_mb, Some(1024.0));
    assert_eq!(s.net_out_bps, Some(2000.0));
    let bw = p.bandwidth_usage(&VmRef::new("101")).await.unwrap();
    assert!(bw.estimated);
    // Two samples × 3600 s × 4000 B/s (only those inside this month count).
    assert!(bw.counted_gb >= 0.0 && bw.counted_gb < 0.03, "{bw:?}");
}

#[tokio::test]
async fn proxmox_backups_are_scoped_to_the_guest() {
    let server = pve_server().await;
    Mock::given(method("GET"))
        .and(path("/api2/json/nodes/pve1/storage"))
        .and(query_param("content", "backup"))
        .respond_with(data(json!([{"storage": "pbs1"}])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api2/json/nodes/pve1/storage/pbs1/content"))
        .and(query_param("vmid", "101"))
        .respond_with(data(json!([
            {"volid": "pbs1:backup/vm/101/2026-11-17T03:00:00Z", "vmid": 101, "ctime": 1_763_348_400, "size": 1_073_741_824_u64, "content": "backup"},
            // A storage that ignored the vmid filter: never offered.
            {"volid": "pbs1:backup/vm/999/2026-11-17T03:00:00Z", "vmid": 999, "ctime": 1_763_348_400, "content": "backup"},
        ])))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api2/json/nodes/pve1/qemu/101/status/stop"))
        .respond_with(data(json!("UPID:pve1:1:2:3:qmstop:101:x:")))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(
            "/api2/json/nodes/pve1/tasks/UPID:pve1:1:2:3:qmstop:101:x:/status",
        ))
        .respond_with(data(json!({"status": "stopped", "exitstatus": "OK"})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api2/json/nodes/pve1/qemu/101/config"))
        .respond_with(data(json!({"scsi0": "local-lvm:vm-101-disk-0,size=20G"})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api2/json/nodes/pve1/qemu"))
        .and(body_string_contains("vmid=101"))
        .and(body_string_contains("force=1"))
        .and(body_string_contains("archive=pbs1%3Abackup%2Fvm%2F101"))
        .respond_with(data(json!("UPID:pve1:1:2:4:qmrestore:101:x:")))
        .expect(1)
        .mount(&server)
        .await;
    let p = pve(&server.uri());
    let list = p.list_backups(&VmRef::new("101")).await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].size_mb, Some(1024));
    // Another guest's archive is refused before anything is touched.
    let err = p
        .restore_backup(
            &VmRef::new("101"),
            "pbs1:backup/vm/999/2026-11-17T03:00:00Z",
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), "not_found");
    let task = p
        .restore_backup(&VmRef::new("101"), &list[0].id)
        .await
        .unwrap()
        .unwrap();
    assert!(task.id.contains("qmrestore"));
}

#[tokio::test]
async fn proxmox_iso_mount_and_unmount() {
    let server = pve_server().await;
    Mock::given(method("GET"))
        .and(path("/api2/json/nodes/pve1/storage"))
        .and(query_param("content", "iso"))
        .respond_with(data(json!([{"storage": "local"}])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api2/json/nodes/pve1/storage/local/content"))
        .and(query_param("content", "iso"))
        .respond_with(data(
            json!([{"volid": "local:iso/debian-12.iso", "size": 734_003_200_u64}]),
        ))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api2/json/nodes/pve1/qemu/101/config"))
        .respond_with(data(
            json!({"scsi0": "local-lvm:vm-101-disk-0,size=20G", "boot": "order=scsi0;net0",
                                  "ide2": "local:iso/debian-12.iso,media=cdrom"}),
        ))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api2/json/nodes/pve1/qemu/101/config"))
        .and(body_string_contains(
            "ide2=local%3Aiso%2Fdebian-12.iso%2Cmedia%3Dcdrom",
        ))
        .and(body_string_contains("boot=order%3Dide2%3Bscsi0"))
        .respond_with(data(json!(null)))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api2/json/nodes/pve1/qemu/101/config"))
        .and(body_string_contains("ide2=none%2Cmedia%3Dcdrom"))
        .respond_with(data(json!(null)))
        .expect(1)
        .mount(&server)
        .await;
    let p = pve(&server.uri());
    let vm = VmRef::new("101");
    let isos = p.list_isos(&vm).await.unwrap();
    assert_eq!(isos[0].id, "local:iso/debian-12.iso");
    assert_eq!(
        p.mount_iso(&vm, "local:iso/other.iso", true)
            .await
            .unwrap_err()
            .kind(),
        "not_found"
    );
    p.mount_iso(&vm, "local:iso/debian-12.iso", true)
        .await
        .unwrap();
    p.unmount_iso(&vm).await.unwrap();
    let extras = p.vm_extras(&vm).await.unwrap();
    assert_eq!(
        extras.mounted_iso.as_deref(),
        Some("local:iso/debian-12.iso")
    );
    assert_eq!(extras.boot_from_iso, Some(false));
    // LXC guests have no ISO support.
    assert!(matches!(
        p.list_isos(&VmRef::new("200")).await.unwrap_err(),
        IntegrationError::Unsupported(_)
    ));
}

#[tokio::test]
async fn proxmox_firewall_replaces_only_managed_rules() {
    let server = pve_server().await;
    Mock::given(method("GET"))
        .and(path("/api2/json/nodes/pve1/qemu/101/firewall/options"))
        .respond_with(data(json!({"enable": 1, "policy_in": "DROP"})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api2/json/nodes/pve1/qemu/101/firewall/rules"))
        .respond_with(data(json!([
            {"pos": 0, "type": "in", "action": "ACCEPT", "proto": "tcp", "dport": "22", "comment": "hostingos: ssh", "enable": 1},
            {"pos": 1, "type": "in", "action": "ACCEPT", "proto": "tcp", "dport": "8006", "comment": "staff rule", "enable": 1},
            {"pos": 2, "type": "in", "action": "ACCEPT", "proto": "icmp", "comment": "hostingos", "enable": 1},
            {"pos": 3, "type": "in", "action": "ACCEPT", "proto": "ipv6-icmp", "comment": "hostingos", "enable": 1},
        ])))
        .mount(&server)
        .await;
    for pos in ["3", "2", "0"] {
        Mock::given(method("DELETE"))
            .and(path(format!(
                "/api2/json/nodes/pve1/qemu/101/firewall/rules/{pos}"
            )))
            .respond_with(data(json!(null)))
            .expect(1)
            .mount(&server)
            .await;
    }
    Mock::given(method("POST"))
        .and(path("/api2/json/nodes/pve1/qemu/101/firewall/rules"))
        .and(body_string_contains("dport=80%3A90"))
        .and(body_string_contains("source=192.0.2.0%2F24"))
        .respond_with(data(json!(null)))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api2/json/nodes/pve1/qemu/101/firewall/rules"))
        .and(body_string_contains("source=2001%3Adb8%3A%3A%2F32"))
        .respond_with(data(json!(null)))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/api2/json/nodes/pve1/qemu/101/firewall/options"))
        .and(body_string_contains("enable=1"))
        .and(body_string_contains("policy_in=DROP"))
        .respond_with(data(json!(null)))
        .expect(1)
        .mount(&server)
        .await;
    let p = pve(&server.uri());
    let vm = VmRef::new("101");
    let fw = p.firewall(&vm).await.unwrap();
    assert!(fw.enabled);
    assert_eq!(fw.rules.len(), 2, "{fw:?}");
    assert_eq!(fw.rules[0].port.as_deref(), Some("22"));
    assert_eq!(fw.rules[0].comment.as_deref(), Some("ssh"));
    assert_eq!(fw.rules[1].protocol, FirewallProtocol::Icmp);
    p.set_firewall(
        &vm,
        &FirewallState {
            enabled: true,
            rules: vec![FirewallRule {
                direction: FirewallDirection::In,
                protocol: FirewallProtocol::Tcp,
                port: Some("80-90".into()),
                cidrs: vec!["192.0.2.0/24".into(), "2001:db8::/32".into()],
                comment: Some("web".into()),
            }],
        },
    )
    .await
    .unwrap();
}

// ---------------------------------------------------------------------------
// Hetzner
// ---------------------------------------------------------------------------

const BEARER: &str = "Bearer hz-token-0123456789";

fn hz(server: &MockServer) -> HetznerProvider {
    HetznerProvider::from_config_with_policy(
        &json!({"api_url": format!("{}/v1", server.uri()), "task_poll_ms": 10, "task_timeout_secs": 5}),
        &json!({"api_token": "hz-token-0123456789"}),
        OutboundPolicy::Unrestricted,
    )
    .unwrap()
}

fn hz_server(id: u64) -> Value {
    json!({"server": {
        "id": id, "name": "web42", "status": "running",
        "public_net": {"ipv4": {"ip": "198.51.100.7", "dns_ptr": "static.7.100.51.198.clients.example"},
                       "ipv6": {"ip": "2001:db8:1234::/64", "dns_ptr": [{"ip": "2001:db8:1234::1", "dns_ptr": "web42.example.com"}]}},
        "server_type": {"cores": 2, "memory": 4.0, "disk": 40},
        "outgoing_traffic": 10_737_418_240_u64, "ingoing_traffic": 1_073_741_824_u64,
        "included_traffic": 21_990_232_555_520_u64, "rescue_enabled": false,
        "iso": {"id": 7, "name": "debian-12", "description": "Debian 12"}
    }})
}

fn hz_action(id: u64, command: &str) -> Value {
    json!({"id": id, "command": command, "status": "running", "progress": 0})
}

#[tokio::test]
async fn hetzner_console_upstream_checks_the_host() {
    let server = MockServer::start().await;
    let host = server.address().to_string();
    Mock::given(method("POST"))
        .and(path("/v1/servers/4711/actions/request_console"))
        .and(header("authorization", BEARER))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "wss_url": format!("ws://{host}/?server_id=4711&token=abc"),
            "password": "vnc-secret", "action": hz_action(1, "request_console")})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/servers/4712/actions/request_console"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "wss_url": "wss://evil.example.net/?token=abc",
            "password": "vnc-secret", "action": hz_action(2, "request_console")})))
        .mount(&server)
        .await;
    let p = hz(&server);
    let up = p.console_upstream(&VmRef::new("4711")).await.unwrap();
    assert_eq!(up.transport, ConsoleTransport::WebSocket);
    assert_eq!(up.vnc_password.unwrap().expose_str(), "vnc-secret");
    assert!(
        up.headers.is_empty(),
        "the API token is not needed upstream"
    );
    let err = p.console_upstream(&VmRef::new("4712")).await.unwrap_err();
    assert_eq!(err.kind(), "transport");
}

#[tokio::test]
async fn hetzner_password_reset_rescue_and_rdns() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/servers/4711/actions/reset_password"))
        .respond_with(ResponseTemplate::new(201).set_body_json(
            json!({"root_password": "zCWbFhnu950dUTko5f40", "action": hz_action(10, "reset_password")}),
        ))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/servers/4711/actions/enable_rescue"))
        .and(body_partial_json(json!({"type": "linux64"})))
        .respond_with(ResponseTemplate::new(201).set_body_json(
            json!({"root_password": "rescue-pw", "action": hz_action(11, "enable_rescue")}),
        ))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/servers/4711"))
        .respond_with(ResponseTemplate::new(200).set_body_json(hz_server(4711)))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/servers/4711/actions/change_dns_ptr"))
        .and(body_partial_json(
            json!({"ip": "2001:db8:1234::5", "dns_ptr": "mail.example.com"}),
        ))
        .respond_with(
            ResponseTemplate::new(201)
                .set_body_json(json!({"action": hz_action(12, "change_dns_ptr")})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let p = hz(&server);
    let vm = VmRef::new("4711");
    let r = p
        .reset_password(&vm, &PasswordResetRequest::default())
        .await
        .unwrap();
    assert_eq!(r.password.expose_str(), "zCWbFhnu950dUTko5f40");
    assert_eq!(r.task.unwrap().id, "10");
    let rescue = p.set_rescue(&vm, true, &[]).await.unwrap();
    assert_eq!(rescue.password.unwrap().expose_str(), "rescue-pw");
    let extras = p.vm_extras(&vm).await.unwrap();
    assert_eq!(extras.rescue_enabled, Some(false));
    assert_eq!(extras.mounted_iso.as_deref(), Some("debian-12"));
    assert_eq!(extras.reverse_dns.len(), 2);
    // Only the server's own addresses.
    let err = p
        .set_reverse_dns(&vm, "203.0.113.9".parse().unwrap(), Some("x.example.com"))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), "invalid_input");
    p.set_reverse_dns(
        &vm,
        "2001:db8:1234::5".parse().unwrap(),
        Some("mail.example.com"),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn hetzner_password_reset_provider_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/servers/4711/actions/reset_password"))
        .respond_with(
            ResponseTemplate::new(409)
                .set_body_json(json!({"error": {"code": "locked", "message": "server is locked"}})),
        )
        .mount(&server)
        .await;
    let err = hz(&server)
        .reset_password(&VmRef::new("4711"), &PasswordResetRequest::default())
        .await
        .unwrap_err();
    assert_eq!(err.kind(), "conflict");
}

#[tokio::test]
async fn hetzner_backups_metrics_bandwidth_and_isos() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/servers/4711"))
        .respond_with(ResponseTemplate::new(200).set_body_json(hz_server(4711)))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/images"))
        .and(query_param("type", "backup"))
        .and(query_param("bound_to", "4711"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"images": [
            {"id": 900, "type": "backup", "bound_to": 4711, "description": "nightly", "created": "2026-11-17T03:00:00+00:00", "image_size": 2.5},
        ], "meta": {"pagination": {"next_page": null}}})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/images/901"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"image": {"id": 901, "type": "backup", "bound_to": 9999}})),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/images/900"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"image": {"id": 900, "type": "backup", "bound_to": 4711}})),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/servers/4711/actions/rebuild"))
        .and(body_partial_json(json!({"image": "900"})))
        .respond_with(
            ResponseTemplate::new(201)
                .set_body_json(json!({"action": hz_action(20, "rebuild_server")})),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/servers/4711/metrics"))
        .and(query_param("type", "cpu,disk,network"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"metrics": {
        "start": "2026-11-17T00:00:00Z", "end": "2026-11-17T01:00:00Z", "step": 60,
        "time_series": {
            "cpu": {"values": [[1_763_337_600.0, "50"], [1_763_337_660.0, "100"]]},
            "network.0.bandwidth.out": {"values": [[1_763_337_600.0, "1024"]]}
        }}})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/isos"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"isos": [
            {"id": 7, "name": "debian-12", "description": "Debian 12", "type": "public", "deprecation": null},
            {"id": 8, "name": "old", "description": "Old", "type": "public", "deprecation": {"unavailable_after": "2020-01-01T00:00:00Z"}},
        ], "meta": {"pagination": {"next_page": null}}})))
        .mount(&server)
        .await;
    let p = hz(&server);
    let vm = VmRef::new("4711");
    let backups = p.list_backups(&vm).await.unwrap();
    assert_eq!(backups.len(), 1);
    assert_eq!(backups[0].size_mb, Some(2560));
    assert_eq!(
        p.restore_backup(&vm, "901").await.unwrap_err().kind(),
        "not_found"
    );
    assert_eq!(
        p.restore_backup(&vm, "abc").await.unwrap_err().kind(),
        "not_found"
    );
    assert_eq!(
        p.restore_backup(&vm, "900").await.unwrap().unwrap().id,
        "20"
    );
    let h = p.metrics_history(&vm, MetricsRange::Hour).await.unwrap();
    assert_eq!(h.samples.len(), 2);
    assert_eq!(h.samples[0].cpu_percent, Some(25.0));
    assert_eq!(h.samples[1].cpu_percent, Some(50.0));
    assert_eq!(h.samples[0].net_out_bps, Some(1024.0));
    let bw = p.bandwidth_usage(&vm).await.unwrap();
    assert!((bw.counted_gb - 10.0).abs() < 1e-9);
    assert!((bw.provider_limit_gb.unwrap() - 20480.0).abs() < 1e-6);
    assert!(!bw.estimated);
    let isos = p.list_isos(&vm).await.unwrap();
    assert_eq!(isos.len(), 1);
    assert_eq!(isos[0].id, "debian-12");
}

#[tokio::test]
async fn hetzner_firewall_is_created_and_applied() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/firewalls"))
        .and(query_param(
            "label_selector",
            "managed-by=hostingos,hostingos-server=4711",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"firewalls": []})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/firewalls"))
        .and(body_partial_json(json!({"name": "hostingos-server-4711",
            "rules": [{"direction": "in", "protocol": "tcp", "port": "22", "source_ips": ["0.0.0.0/0", "::/0"]}]})))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"firewall": {"id": 55, "applied_to": [], "rules": []}, "actions": []})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/firewalls/55/actions/apply_to_resources"))
        .and(body_partial_json(
            json!({"apply_to": [{"type": "server", "server": {"id": 4711}}]}),
        ))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"actions": []})))
        .expect(1)
        .mount(&server)
        .await;
    hz(&server)
        .set_firewall(
            &VmRef::new("4711"),
            &FirewallState {
                enabled: true,
                rules: vec![FirewallRule {
                    direction: FirewallDirection::In,
                    protocol: FirewallProtocol::Tcp,
                    port: Some("22".into()),
                    cidrs: vec![],
                    comment: Some("ssh".into()),
                }],
            },
        )
        .await
        .unwrap();
}

// ---------------------------------------------------------------------------
// Virtualizor, VMware (unsupported), mock
// ---------------------------------------------------------------------------

#[tokio::test]
async fn virtualizor_console_bandwidth_and_unsupported() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/index.php"))
        .and(query_param("act", "vnc"))
        .and(query_param("novnc", "7"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"title": "Virtualizor",
            "info": {"vnc_hostname": "192.0.2.10", "port": "5910", "virt": "kvm", "password": "a1b2c3d4"}})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/index.php"))
        .and(query_param("act", "vsbandwidth"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"vpsusage": {"7": {"used": 24576, "used_gb": "24.00", "limit": 512000, "limit_gb": 500}}})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/index.php"))
        .and(query_param("act", "vnc"))
        .and(query_param("novnc", "8"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"title": "Virtualizor", "error": ["VNC is disabled"]})),
        )
        .mount(&server)
        .await;
    let p = VirtualizorProvider::from_config_with_policy(
        &json!({"api_url": server.uri()}),
        &json!({"api_key": "k", "api_pass": "p"}),
        OutboundPolicy::Unrestricted,
    )
    .unwrap();
    let up = p.console_upstream(&VmRef::new("7")).await.unwrap();
    assert_eq!(up.transport, ConsoleTransport::Tcp);
    assert_eq!(up.url, "192.0.2.10:5910");
    assert_eq!(up.vnc_password.unwrap().expose_str(), "a1b2c3d4");
    assert!(p.console_upstream(&VmRef::new("8")).await.is_err());
    let bw = p.bandwidth_usage(&VmRef::new("7")).await.unwrap();
    assert!((bw.counted_gb - 24.0).abs() < 1e-9);
    assert!((bw.provider_limit_gb.unwrap() - 500.0).abs() < 1e-9);
    let f = p.features();
    assert!(f.console_relay && f.bandwidth && f.metrics_history);
    assert!(!f.password_reset && !f.iso && !f.backups);
    let err = p
        .reset_password(&VmRef::new("7"), &PasswordResetRequest::default())
        .await
        .unwrap_err();
    assert!(matches!(err, IntegrationError::Unsupported(_)));
}

#[tokio::test]
async fn vmware_extended_operations_are_unsupported() {
    let p = hos_virt::build_with_policy(
        "vmware",
        &json!({"api_url": "https://vc.example.com", "username": "a@vsphere.local"}),
        &json!({"password": "x"}),
        OutboundPolicy::Unrestricted,
    )
    .unwrap();
    assert_eq!(p.features(), Default::default());
    let vm = VmRef::new("vm-1");
    for err in [
        p.console_upstream(&vm).await.unwrap_err(),
        p.list_backups(&vm).await.unwrap_err(),
        p.firewall(&vm).await.unwrap_err(),
        p.metrics_history(&vm, MetricsRange::Day).await.unwrap_err(),
    ] {
        assert!(matches!(err, IntegrationError::Unsupported(_)), "{err:?}");
    }
}

#[tokio::test]
async fn mock_simulated_console_authenticates_and_draws() {
    let p = MockProvider::new().adopting_unknown_vms();
    let up = p.console_upstream(&VmRef::new("4242")).await.unwrap();
    assert_eq!(up.transport, ConsoleTransport::Simulated);
    let (mut r, mut w, hs) = console::connect(&up, ConnectOptions::default())
        .await
        .unwrap();
    assert_eq!((hs.minor, hs.security), (8, 2));
    // ClientInit → ServerInit (800×480).
    w.send(&[1]).await.unwrap();
    let init = recv_at_least(&mut r, 24).await;
    assert_eq!(u16::from_be_bytes([init[0], init[1]]), 800);
    assert_eq!(u16::from_be_bytes([init[2], init[3]]), 480);
    // A stopped VM has no console.
    p.power(
        &VmRef::new("4242"),
        hos_core::integrations::virt::PowerAction::Stop,
    )
    .await
    .unwrap();
    assert_eq!(
        p.console_upstream(&VmRef::new("4242"))
            .await
            .unwrap_err()
            .kind(),
        "conflict"
    );
}

async fn recv_at_least(r: &mut console::UpstreamReader, n: usize) -> Vec<u8> {
    let mut buf = Vec::new();
    while buf.len() < n {
        buf.extend(r.recv().await.unwrap().expect("upstream closed"));
    }
    buf
}

// ---------------------------------------------------------------------------
// console::connect against a local fake websocket VNC upstream
// ---------------------------------------------------------------------------

/// A websocket server that checks the upgrade headers and bridges binary
/// frames to a simulated VNC server with `password`.
async fn fake_ws_upstream(password: &'static str, close_after_handshake: bool) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            tokio::spawn(async move {
                #[allow(clippy::result_large_err)]
                let check = |req: &tokio_tungstenite::tungstenite::handshake::server::Request,
                             mut resp: tokio_tungstenite::tungstenite::handshake::server::Response| {
                    let auth = req.headers().get("authorization").and_then(|v| v.to_str().ok());
                    if auth != Some("PVEAPIToken=t") {
                        let mut r = tokio_tungstenite::tungstenite::handshake::server::ErrorResponse::new(None);
                        *r.status_mut() = tokio_tungstenite::tungstenite::http::StatusCode::UNAUTHORIZED;
                        return Err(r);
                    }
                    resp.headers_mut().insert("sec-websocket-protocol", "binary".parse().unwrap());
                    Ok(resp)
                };
                let Ok(ws) = tokio_tungstenite::accept_hdr_async(tcp, check).await else {
                    return;
                };
                let (mut sink, mut stream) = ws.split();
                let sim = console::sim::spawn(SecretString::from(password), "fake");
                let (mut sr, mut sw) = tokio::io::split(sim);
                let down = async {
                    let mut buf = vec![0u8; 65536];
                    let mut sent = 0usize;
                    loop {
                        let n = match sr.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => n,
                        };
                        if sink
                            .send(tokio_tungstenite::tungstenite::Message::Binary(
                                buf[..n].to_vec().into(),
                            ))
                            .await
                            .is_err()
                        {
                            break;
                        }
                        sent += n;
                        // version (12) + types (2) + challenge (16) + result (4)
                        if close_after_handshake && sent >= 34 {
                            let _ = sink.close().await;
                            break;
                        }
                    }
                };
                let up = async {
                    while let Some(Ok(msg)) = stream.next().await {
                        if let tokio_tungstenite::tungstenite::Message::Binary(b) = msg
                            && sw.write_all(&b).await.is_err()
                        {
                            break;
                        }
                    }
                };
                tokio::select! { () = down => {}, () = up => {} }
            });
        }
    });
    format!("ws://{addr}/vnc")
}

fn ws_upstream(url: String, password: &str, token: &str) -> ConsoleUpstream {
    ConsoleUpstream {
        transport: ConsoleTransport::WebSocket,
        url,
        headers: vec![("Authorization".into(), SecretString::from(token))],
        subprotocols: vec!["binary".into()],
        accept_invalid_certs: false,
        vnc_password: Some(SecretString::from(password)),
        expires_at: None,
    }
}

fn unrestricted() -> ConnectOptions {
    ConnectOptions {
        policy: OutboundPolicy::Unrestricted,
        timeout: std::time::Duration::from_secs(5),
    }
}

#[tokio::test]
async fn websocket_upstream_handshake_and_relay() {
    let url = fake_ws_upstream("s3cret", false).await;
    let (mut r, mut w, hs) =
        console::connect(&ws_upstream(url, "s3cret", "PVEAPIToken=t"), unrestricted())
            .await
            .unwrap();
    assert_eq!(hs.security, 2);
    w.send(&[0]).await.unwrap();
    let init = recv_at_least(&mut r, 24).await;
    assert_eq!(&init[20..24], &14u32.to_be_bytes()); // "HostingOS demo"
}

#[tokio::test]
async fn websocket_upstream_refusals() {
    let url = fake_ws_upstream("s3cret", false).await;
    // Wrong API token: the upgrade is refused (HTTP 401 → auth).
    let err = console::connect(
        &ws_upstream(url.clone(), "s3cret", "PVEAPIToken=bad"),
        unrestricted(),
    )
    .await
    .unwrap_err();
    assert_eq!(err.kind(), "auth", "{err:?}");
    // Wrong VNC password.
    let err = console::connect(
        &ws_upstream(url.clone(), "wrong", "PVEAPIToken=t"),
        unrestricted(),
    )
    .await
    .unwrap_err();
    assert_eq!(err.kind(), "auth");
    // Loopback is not allowed by the production policies.
    let err = console::connect(
        &ws_upstream(url.clone(), "s3cret", "PVEAPIToken=t"),
        ConnectOptions {
            policy: OutboundPolicy::Private,
            timeout: std::time::Duration::from_secs(5),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(err.kind(), "config", "{err:?}");
    // An expired ticket is not even tried.
    let mut expired = ws_upstream(url, "s3cret", "PVEAPIToken=t");
    expired.expires_at = Some(chrono::Utc::now() - chrono::Duration::seconds(1));
    assert_eq!(
        console::connect(&expired, unrestricted())
            .await
            .unwrap_err()
            .kind(),
        "conflict"
    );
}

#[tokio::test]
async fn websocket_upstream_close_is_reported() {
    let url = fake_ws_upstream("s3cret", true).await;
    let (mut r, _w, _) =
        console::connect(&ws_upstream(url, "s3cret", "PVEAPIToken=t"), unrestricted())
            .await
            .unwrap();
    assert_eq!(r.recv().await.unwrap(), None);
}
