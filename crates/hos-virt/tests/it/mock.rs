//! State-machine tests of the in-memory "Demo / Test" provider.

use hos_core::IntegrationError;
use hos_core::integrations::VirtProvider;
use hos_core::integrations::virt::{
    IpConfig, NetworkSpec, PowerAction, SnapshotRequest, TaskRef, TaskState, VmReinstall, VmResize,
    VmSpec, VmStatus,
};
use hos_virt::MockProvider;
use serde_json::json;

fn spec(name: &str) -> VmSpec {
    VmSpec {
        name: name.into(),
        hostname: format!("{name}.example.com"),
        template: "ubuntu-24.04".into(),
        cpu_cores: 2,
        memory_mb: 2048,
        disk_gb: 20,
        network: NetworkSpec {
            ipv4: Some(IpConfig::Auto),
            ipv6: Some(IpConfig::Auto),
            ..NetworkSpec::default()
        },
        ..VmSpec::default()
    }
}

async fn drain(p: &MockProvider, task: &TaskRef) -> Vec<TaskState> {
    let mut seen = Vec::new();
    for _ in 0..20 {
        let s = p.task_status(task).await.unwrap();
        let done = !matches!(s, TaskState::Running { .. });
        seen.push(s);
        if done {
            break;
        }
    }
    seen
}

#[tokio::test]
async fn sync_lifecycle() {
    let p = MockProvider::new();
    let created = p.create_vm(&spec("web1")).await.unwrap();
    assert!(created.task.is_none());
    assert!(
        created.root_password.is_some(),
        "no password or keys → generated"
    );
    assert_eq!(created.ipv4.len(), 1);
    assert_eq!(created.ipv6.len(), 1);
    let vm = created.vm;
    assert_eq!(p.get_vm(&vm).await.unwrap().status, VmStatus::Running);

    // Start on a running VM is a conflict; stop works; stop again conflicts.
    assert!(matches!(
        p.power(&vm, PowerAction::Start).await,
        Err(IntegrationError::Conflict(_))
    ));
    p.power(&vm, PowerAction::Shutdown).await.unwrap();
    assert_eq!(p.get_vm(&vm).await.unwrap().status, VmStatus::Stopped);
    assert!(matches!(
        p.power(&vm, PowerAction::Reboot).await,
        Err(IntegrationError::Conflict(_))
    ));
    assert!(matches!(
        p.console(&vm).await,
        Err(IntegrationError::Conflict(_))
    ));
    p.power(&vm, PowerAction::Start).await.unwrap();
    let console = p.console(&vm).await.unwrap();
    assert!(console.url.starts_with("wss://"));

    let m = p.metrics(&vm).await.unwrap();
    assert_eq!(m.memory_total_mb, Some(2048));
    assert!(m.cpu_usage_percent.unwrap() > 0.0);

    // Duplicate names are rejected.
    assert!(matches!(
        p.create_vm(&spec("web1")).await,
        Err(IntegrationError::Conflict(_))
    ));

    p.delete_vm(&vm).await.unwrap();
    assert!(matches!(
        p.get_vm(&vm).await,
        Err(IntegrationError::NotFound(_))
    ));
}

#[tokio::test]
async fn async_tasks_progress_and_busy_states() {
    let p = MockProvider::new().with_async_tasks(3);
    let created = p.create_vm(&spec("db1")).await.unwrap();
    let vm = created.vm;
    let task = created.task.expect("async create returns a task");
    assert_eq!(p.get_vm(&vm).await.unwrap().status, VmStatus::Provisioning);
    // Busy VMs reject other operations.
    assert!(matches!(
        p.power(&vm, PowerAction::Stop).await,
        Err(IntegrationError::Conflict(_))
    ));
    assert!(matches!(
        p.delete_vm(&vm).await,
        Err(IntegrationError::Conflict(_))
    ));
    let states = drain(&p, &task).await;
    assert_eq!(
        states,
        vec![
            TaskState::Running {
                progress_percent: Some(33)
            },
            TaskState::Running {
                progress_percent: Some(66)
            },
            TaskState::Succeeded,
        ]
    );
    assert_eq!(p.get_vm(&vm).await.unwrap().status, VmStatus::Running);

    let stop = p.power(&vm, PowerAction::Stop).await.unwrap().unwrap();
    assert_eq!(p.get_vm(&vm).await.unwrap().status, VmStatus::Stopping);
    drain(&p, &stop).await;
    assert_eq!(p.get_vm(&vm).await.unwrap().status, VmStatus::Stopped);

    let unknown = TaskRef {
        id: "mock-task-999".into(),
        node: None,
    };
    assert!(matches!(
        p.task_status(&unknown).await,
        Err(IntegrationError::NotFound(_))
    ));
}

#[tokio::test]
async fn resize_rules() {
    let p = MockProvider::new();
    let vm = p.create_vm(&spec("app1")).await.unwrap().vm;
    let shrink = VmResize {
        disk_gb: Some(10),
        ..VmResize::default()
    };
    assert!(matches!(
        p.resize(&vm, &shrink).await,
        Err(IntegrationError::InvalidInput(_))
    ));
    let grow_cpu = VmResize {
        cpu_cores: Some(4),
        ..VmResize::default()
    };
    assert!(matches!(
        p.resize(&vm, &grow_cpu).await,
        Err(IntegrationError::Conflict(_))
    ));
    let with_restart = VmResize {
        cpu_cores: Some(4),
        disk_gb: Some(40),
        allow_restart: true,
        ..VmResize::default()
    };
    p.resize(&vm, &with_restart).await.unwrap();
    let info = p.get_vm(&vm).await.unwrap();
    assert_eq!(info.cpu_cores, Some(4));
    assert_eq!(info.disk_gb, Some(40));
    assert_eq!(info.status, VmStatus::Running);
}

#[tokio::test]
async fn snapshots_roundtrip() {
    let p = MockProvider::new();
    let vm = p.create_vm(&spec("snap1")).await.unwrap().vm;
    let base = p
        .create_snapshot(
            &vm,
            &SnapshotRequest {
                name: "base".into(),
                description: Some("before upgrade".into()),
                include_memory: true,
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        p.create_snapshot(
            &vm,
            &SnapshotRequest {
                name: "base".into(),
                ..SnapshotRequest::default()
            }
        )
        .await,
        Err(IntegrationError::Conflict(_))
    ));
    p.resize(
        &vm,
        &VmResize {
            memory_mb: Some(4096),
            allow_restart: true,
            ..VmResize::default()
        },
    )
    .await
    .unwrap();
    let child = p
        .create_snapshot(
            &vm,
            &SnapshotRequest {
                name: "after".into(),
                ..SnapshotRequest::default()
            },
        )
        .await
        .unwrap();
    let list = p.list_snapshots(&vm).await.unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[1].parent.as_deref(), Some(base.snapshot_id.as_str()));
    assert!(list[0].includes_memory);

    // Memory snapshot of a running VM → still running after rollback, old shape back.
    p.rollback_snapshot(&vm, &base.snapshot_id).await.unwrap();
    let info = p.get_vm(&vm).await.unwrap();
    assert_eq!(info.memory_mb, Some(2048));
    assert_eq!(info.status, VmStatus::Running);
    // Disk-only snapshot → powered off after rollback.
    p.rollback_snapshot(&vm, &child.snapshot_id).await.unwrap();
    assert_eq!(p.get_vm(&vm).await.unwrap().status, VmStatus::Stopped);

    p.delete_snapshot(&vm, &base.snapshot_id).await.unwrap();
    let list = p.list_snapshots(&vm).await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].parent, None, "child re-parented");
    assert!(matches!(
        p.rollback_snapshot(&vm, "nope").await,
        Err(IntegrationError::NotFound(_))
    ));
}

#[tokio::test]
async fn suspend_reinstall_and_failures() {
    let p = MockProvider::new();
    let vm = p.create_vm(&spec("cust1")).await.unwrap().vm;
    p.suspend(&vm).await.unwrap();
    assert_eq!(p.get_vm(&vm).await.unwrap().status, VmStatus::Suspended);
    assert!(matches!(
        p.power(&vm, PowerAction::Start).await,
        Err(IntegrationError::Conflict(_))
    ));
    assert!(matches!(
        p.suspend(&vm).await,
        Err(IntegrationError::Conflict(_))
    ));
    p.unsuspend(&vm).await.unwrap();
    assert_eq!(p.get_vm(&vm).await.unwrap().status, VmStatus::Running);

    let before = p.get_vm(&vm).await.unwrap();
    let re = p
        .reinstall(
            &vm,
            &VmReinstall {
                template: "debian-12".into(),
                hostname: Some("new.example.com".into()),
                ..VmReinstall::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(re.vm, vm, "reinstall keeps the id");
    assert_eq!(re.ipv4, before.ipv4, "and the addresses");
    let after = p.get_vm(&vm).await.unwrap();
    assert_eq!(after.template.as_deref(), Some("debian-12"));
    assert_eq!(after.hostname.as_deref(), Some("new.example.com"));

    // Windows needs 40 GiB.
    let mut win = spec("win1");
    win.template = "windows-2022".into();
    assert!(matches!(
        p.create_vm(&win).await,
        Err(IntegrationError::InvalidInput(_))
    ));

    let broken =
        MockProvider::from_config(&json!({"fail_operations": "create_vm"}), &json!({})).unwrap();
    match broken.create_vm(&spec("x")).await {
        Err(IntegrationError::Remote { status, code, .. }) => {
            assert_eq!(status, Some(500));
            assert_eq!(code.as_deref(), Some("simulated_failure"));
        }
        other => panic!("expected simulated failure, got {other:?}"),
    }
    assert_eq!(broken.list_templates(None).await.unwrap().len(), 4);
    assert_eq!(broken.list_nodes().await.unwrap().len(), 2);
}

#[tokio::test]
async fn built_through_the_registry() {
    let p = hos_virt::build(hos_virt::MOCK, &json!({"nodes": 3}), &json!({})).unwrap();
    assert_eq!(p.kind(), hos_virt::MOCK);
    assert_eq!(p.list_nodes().await.unwrap().len(), 3);
    assert!(p.capabilities().snapshots);
}
