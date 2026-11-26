//! Offloading blocking work from the async runtime.

use std::path::PathBuf;

use crate::CoreError;

/// Runs blocking filesystem or CPU-heavy work on tokio's blocking pool so it never stalls the
/// async runtime (WebSocket keepalives, concurrent RPCs).
pub(crate) async fn run_blocking<T, F>(work: F) -> Result<T, CoreError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, CoreError> + Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|join_err| CoreError::Io {
            path: PathBuf::new(),
            source: std::io::Error::other(format!("blocking task failed: {join_err}")),
        })?
}
