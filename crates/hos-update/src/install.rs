//! How this process was installed, which decides how an update is applied.
//!
//! | Kind | Detected by | Update |
//! |---|---|---|
//! | [`InstallKind::Container`] | `/.dockerenv`, `/run/.containerenv`, `KUBERNETES_SERVICE_HOST`, `container=docker\|podman\|oci`, a docker / containerd / kubepods cgroup | never replaces itself (the image is immutable): shows the new image tag and the commands, optionally calls a deploy webhook |
//! | [`InstallKind::Binary`] | the executable is `<root>/releases/<version>/hostingos` and `<root>/current` links to that directory | in place: download, verify, swap `current`, migrate, exit so the supervisor restarts it |
//! | [`InstallKind::Unknown`] | anything else | instructions only |
//!
//! `HOS_INSTALL_TYPE=binary|container|manual` overrides the detection;
//! `HOS_UPDATE_ROOT` names the root explicitly. An in-place update also
//! needs a supervisor that restarts the process when it exits: systemd is
//! recognised by `INVOCATION_ID`; others set `HOS_SUPERVISED=1`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::layout::{BINARY_NAME, Layout};

/// Container runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerKind {
    /// Docker / Compose / Coolify / Watchtower.
    Docker,
    /// Podman.
    Podman,
    /// Kubernetes.
    Kubernetes,
    /// Forced by `HOS_INSTALL_TYPE=container` or an OCI runtime.
    Generic,
}

impl ContainerKind {
    /// `docker` / `podman` / `kubernetes` / `container`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Docker => "docker",
            Self::Podman => "podman",
            Self::Kubernetes => "kubernetes",
            Self::Generic => "container",
        }
    }
}

/// A bare-binary install with the release layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinaryInstall {
    /// The install root (`/opt/hostingos`).
    pub root: PathBuf,
    /// The running release directory (`<root>/releases/<version>`).
    pub release: PathBuf,
    /// A supervisor restarts the process when it exits.
    pub supervised: bool,
}

impl BinaryInstall {
    /// The layout.
    pub fn layout(&self) -> Layout {
        Layout::new(&self.root)
    }
}

/// The detected install type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallKind {
    /// In-place updates possible (when supervised).
    Binary(BinaryInstall),
    /// Immutable image: instructions / deploy webhook.
    Container(ContainerKind),
    /// Instructions only.
    Unknown {
        /// Why no other kind matched.
        reason: String,
    },
}

impl InstallKind {
    /// `binary` / `container` / `unknown`.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Binary(_) => "binary",
            Self::Container(_) => "container",
            Self::Unknown { .. } => "unknown",
        }
    }

    /// Whether the binary can replace itself here.
    pub fn can_apply_in_place(&self) -> bool {
        matches!(self, Self::Binary(b) if b.supervised)
    }
}

/// Environment variables that matter for detection.
pub const ENV_KEYS: &[&str] = &[
    "HOS_INSTALL_TYPE",
    "HOS_UPDATE_ROOT",
    "HOS_SUPERVISED",
    "INVOCATION_ID",
    "KUBERNETES_SERVICE_HOST",
    "container",
];

/// The facts detection looks at (collected by [`Probe::from_system`]; tests
/// build their own).
#[derive(Debug, Clone, Default)]
pub struct Probe {
    /// `std::env::current_exe()`.
    pub exe: Option<PathBuf>,
    /// The variables of [`ENV_KEYS`] that are set.
    pub env: BTreeMap<String, String>,
    /// `/.dockerenv` exists.
    pub dockerenv: bool,
    /// `/run/.containerenv` exists (Podman).
    pub containerenv: bool,
    /// `/proc/1/cgroup` (empty when unreadable).
    pub cgroup: String,
}

impl Probe {
    /// The running process.
    pub fn from_system() -> Self {
        let env = ENV_KEYS
            .iter()
            .filter_map(|k| {
                std::env::var(k)
                    .ok()
                    .filter(|v| !v.trim().is_empty())
                    .map(|v| ((*k).to_owned(), v))
            })
            .collect();
        Self {
            exe: std::env::current_exe().ok(),
            env,
            dockerenv: Path::new("/.dockerenv").exists(),
            containerenv: Path::new("/run/.containerenv").exists(),
            cgroup: std::fs::read_to_string("/proc/1/cgroup").unwrap_or_default(),
        }
    }

    fn var(&self, k: &str) -> Option<&str> {
        self.env.get(k).map(|v| v.trim()).filter(|v| !v.is_empty())
    }

    fn container(&self) -> Option<ContainerKind> {
        if self.var("KUBERNETES_SERVICE_HOST").is_some() || self.cgroup.contains("kubepods") {
            return Some(ContainerKind::Kubernetes);
        }
        if self.containerenv || self.var("container") == Some("podman") {
            return Some(ContainerKind::Podman);
        }
        if self.dockerenv
            || self.var("container") == Some("docker")
            || self.cgroup.contains("docker")
            || self.cgroup.contains("containerd")
        {
            return Some(ContainerKind::Docker);
        }
        if self.var("container") == Some("oci") {
            return Some(ContainerKind::Generic);
        }
        None
    }

    fn supervised(&self) -> bool {
        self.var("INVOCATION_ID").is_some()
            || self.var("HOS_SUPERVISED").is_some_and(|v| {
                matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
            })
    }
}

/// Detects the install type (see the module docs).
pub fn detect(p: &Probe) -> InstallKind {
    let forced = p.var("HOS_INSTALL_TYPE").map(str::to_ascii_lowercase);
    match forced.as_deref() {
        Some("container" | "docker" | "kubernetes" | "podman") => {
            return InstallKind::Container(p.container().unwrap_or(ContainerKind::Generic));
        }
        Some("manual" | "unknown" | "none") => {
            return InstallKind::Unknown {
                reason: "HOS_INSTALL_TYPE=manual".into(),
            };
        }
        Some("binary") | None => {}
        Some(other) => {
            return InstallKind::Unknown {
                reason: format!("unknown HOS_INSTALL_TYPE {other:?} (binary, container or manual)"),
            };
        }
    }
    if forced.is_none()
        && let Some(kind) = p.container()
    {
        return InstallKind::Container(kind);
    }
    match binary_layout(p) {
        Ok(b) => InstallKind::Binary(b),
        Err(reason) => InstallKind::Unknown { reason },
    }
}

fn binary_layout(p: &Probe) -> Result<BinaryInstall, String> {
    let exe = p
        .exe
        .as_ref()
        .ok_or_else(|| "the path of the running executable is unknown".to_owned())?;
    let exe =
        std::fs::canonicalize(exe).map_err(|e| format!("resolving {}: {e}", exe.display()))?;
    if exe.file_name().and_then(|n| n.to_str()) != Some(BINARY_NAME) {
        return Err(format!("the executable is not named {BINARY_NAME}"));
    }
    let release = exe
        .parent()
        .ok_or_else(|| "the executable has no parent directory".to_owned())?
        .to_path_buf();
    let releases = release.parent().map(Path::to_path_buf);
    let root = match p.var("HOS_UPDATE_ROOT") {
        Some(r) => std::fs::canonicalize(r).map_err(|e| format!("HOS_UPDATE_ROOT {r}: {e}"))?,
        None => releases
            .as_ref()
            .filter(|d| d.file_name().and_then(|n| n.to_str()) == Some("releases"))
            .and_then(|d| d.parent())
            .map(Path::to_path_buf)
            .ok_or_else(|| {
                format!(
                    "{} is not inside <root>/releases/<version>/ (see docs/deploy/install.sh)",
                    exe.display()
                )
            })?,
    };
    let layout = Layout::new(&root);
    if releases.as_deref() != Some(layout.releases_dir().as_path())
        && releases
            .as_ref()
            .and_then(|r| std::fs::canonicalize(r).ok())
            != std::fs::canonicalize(layout.releases_dir()).ok()
    {
        return Err(format!(
            "{} is not inside {}",
            exe.display(),
            layout.releases_dir().display()
        ));
    }
    let current = layout.current().map_err(|_| {
        format!(
            "{} is not a link to a release",
            layout.current_link().display()
        )
    })?;
    if current != release {
        return Err(format!(
            "{} points to {}, but the running binary is {}",
            layout.current_link().display(),
            current.display(),
            exe.display()
        ));
    }
    Ok(BinaryInstall {
        root,
        release,
        supervised: p.supervised(),
    })
}
