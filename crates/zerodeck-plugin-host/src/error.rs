use thiserror::Error;
use zerodeck_platform::PlatformError;

/// Errors produced when spawning, communicating with, or stopping plugin workers.
#[derive(Debug, Error)]
pub enum HostError {
    /// Plugin is passive (`main.py` absent) and does not run a backend worker.
    #[error("plugin `{0}` is passive and has no backend worker")]
    PassivePlugin(String),

    /// Target plugin is not registered in the supervisor.
    #[error("plugin `{0}` is not loaded")]
    PluginNotFound(String),

    /// Plugin tripped the crash-loop circuit breaker and is in the `Failed` state.
    #[error("plugin `{0}` failed repeatedly and is stopped")]
    PluginFailed(String),

    /// Caller attempted to invoke a private `_`-prefixed plugin method.
    #[error("cannot call private plugin method `{0}`")]
    InvalidMethodName(String),

    /// Worker stopped or crashed while a call was in flight or during backoff.
    #[error("plugin worker stopped before completing call")]
    WorkerStopped,

    /// Worker emitted an IPC line exceeding the maximum allowed frame size.
    #[error("plugin worker IPC frame exceeded maximum size")]
    FrameTooLarge,

    /// Worker did not complete startup within the configured timeout.
    #[error("plugin worker startup timed out")]
    StartupTimeout,

    /// Worker returned a structured exception for an RPC invocation.
    #[error("plugin call failed ({name}): {message}")]
    PluginCallFailed {
        name: String,
        message: String,
        traceback: Option<String>,
    },

    /// Worker process failed to spawn or exited unexpectedly.
    #[error("worker process error for `{plugin}`: {source}")]
    ProcessIo {
        plugin: String,
        #[source]
        source: std::io::Error,
    },

    /// Platform path, directory ownership, or privilege resolution error.
    #[error(transparent)]
    Platform(#[from] PlatformError),

    /// IPC protocol serialization/deserialization error.
    #[error(transparent)]
    Protocol(#[from] zerodeck_protocol::ProtocolError),
}
