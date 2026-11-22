//! "Restart me": the updater asks the running `hostingos` process to shut
//! down gracefully and exit with [`EXIT_CODE`], which the supervisor
//! restarts (systemd: `Restart=always` + `RestartForceExitStatus=75`).

use std::sync::OnceLock;

use tokio::sync::watch;

/// Exit status after an update or rollback: `EX_TEMPFAIL` (75).
pub const EXIT_CODE: u8 = 75;

fn channel() -> &'static watch::Sender<Option<String>> {
    static CH: OnceLock<watch::Sender<Option<String>>> = OnceLock::new();
    CH.get_or_init(|| watch::channel(None).0)
}

/// Requests a restart (idempotent; the first reason wins).
pub fn request(reason: impl Into<String>) {
    let reason = reason.into();
    channel().send_if_modified(|current| {
        if current.is_none() {
            tracing::warn!(%reason, "restart requested");
            *current = Some(reason);
            true
        } else {
            false
        }
    });
}

/// The reason, once a restart was requested.
pub fn requested() -> Option<String> {
    channel().borrow().clone()
}

/// Resolves when a restart is requested (immediately if it already was).
pub async fn wait() {
    let mut rx = channel().subscribe();
    let _ = rx.wait_for(Option::is_some).await;
}
