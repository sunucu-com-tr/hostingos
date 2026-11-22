//! Install-type detection.

use hos_update::install::{ContainerKind, InstallKind, Probe, detect};

use crate::support::{TempDir, layout_with};

fn probe(exe: Option<std::path::PathBuf>, env: &[(&str, &str)]) -> Probe {
    Probe {
        exe,
        env: env
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect(),
        ..Probe::default()
    }
}

#[test]
fn bare_binary_layout() {
    let dir = TempDir::new("detect");
    let (_layout, release) = layout_with(dir.path(), "0.1.0");
    // Executed through the `current` link, as systemd does.
    let exe = dir.path().join("current").join("hostingos");
    match detect(&probe(Some(exe.clone()), &[("INVOCATION_ID", "abc")])) {
        InstallKind::Binary(b) => {
            assert!(b.supervised);
            assert_eq!(b.release, release);
            assert_eq!(b.root, dir.path());
        }
        other => panic!("expected binary, got {other:?}"),
    }
    match detect(&probe(Some(exe.clone()), &[])) {
        InstallKind::Binary(b) => assert!(!b.supervised, "no supervisor detected"),
        other => panic!("expected binary, got {other:?}"),
    }
    assert!(detect(&probe(Some(exe.clone()), &[("HOS_SUPERVISED", "1")])).can_apply_in_place());

    // `current` points elsewhere: not this binary's layout.
    let (layout, _) = layout_with(dir.path(), "0.2.0");
    assert_eq!(
        layout.current().unwrap(),
        std::fs::canonicalize(dir.path().join("releases/0.2.0")).unwrap()
    );
    let old_exe = release.join("hostingos");
    assert!(matches!(
        detect(&probe(Some(old_exe), &[])),
        InstallKind::Unknown { .. }
    ));
}

#[test]
fn containers_and_overrides() {
    let docker = Probe {
        dockerenv: true,
        ..Probe::default()
    };
    assert_eq!(
        detect(&docker),
        InstallKind::Container(ContainerKind::Docker)
    );
    let k8s = probe(None, &[("KUBERNETES_SERVICE_HOST", "10.0.0.1")]);
    assert_eq!(
        detect(&k8s),
        InstallKind::Container(ContainerKind::Kubernetes)
    );
    let podman = Probe {
        containerenv: true,
        ..Probe::default()
    };
    assert_eq!(
        detect(&podman),
        InstallKind::Container(ContainerKind::Podman)
    );
    let cgroup = Probe {
        cgroup: "0::/system.slice/docker-abc.scope".into(),
        ..Probe::default()
    };
    assert_eq!(
        detect(&cgroup),
        InstallKind::Container(ContainerKind::Docker)
    );
    let forced = probe(None, &[("HOS_INSTALL_TYPE", "container")]);
    assert_eq!(
        detect(&forced),
        InstallKind::Container(ContainerKind::Generic)
    );
    let manual = Probe {
        dockerenv: true,
        ..probe(None, &[("HOS_INSTALL_TYPE", "manual")])
    };
    assert!(matches!(detect(&manual), InstallKind::Unknown { .. }));
    assert!(matches!(
        detect(&probe(None, &[("HOS_INSTALL_TYPE", "bogus")])),
        InstallKind::Unknown { .. }
    ));
    // A plain binary somewhere else (cargo run, /usr/local/bin): instructions only.
    let dir = TempDir::new("loose");
    let exe = dir.path().join("hostingos");
    std::fs::write(&exe, b"x").unwrap();
    assert!(matches!(
        detect(&probe(Some(exe), &[("INVOCATION_ID", "x")])),
        InstallKind::Unknown { .. }
    ));
    assert!(!detect(&probe(None, &[])).can_apply_in_place());
}
