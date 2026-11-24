use thiserror::Error;

/// Errors produced when spawning, communicating with, or stopping plugin workers.
#[derive(Debug, Error)]
pub enum HostError {
    /// Plugin is passive (`main.py` absent) and does not run a backend worker.
    #[error("plugin `{0}` is passive and has no backend worker")]
    PassivePlugin(String),

    /// Worker process failed to spawn or exited unexpectedly.
    #[error("worker process error for `{plugin}`: {source}")]
    ProcessIo {
        plugin: String,
        #[source]
        source: std::io::Error,
    },

    /// IPC protocol serialization/deserialization error.
    #[error(transparent)]
    Protocol(#[from] zerodeck_protocol::ProtocolError),
}
