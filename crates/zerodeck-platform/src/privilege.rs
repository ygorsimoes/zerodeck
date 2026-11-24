use zerodeck_protocol::PluginManifest;

/// Execution privilege target for a plugin worker process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PrivilegeTarget {
    /// Drop privileges to the unprivileged host user (default).
    HostUser,
    /// Retain effective root privileges (only when `"root"` is in `plugin.json` `flags`).
    Root,
}

impl PrivilegeTarget {
    /// Resolves the target privilege level from a boolean `requires_root` flag.
    #[must_use]
    pub const fn from_requires_root(requires_root: bool) -> Self {
        if requires_root {
            Self::Root
        } else {
            Self::HostUser
        }
    }

    /// Resolves the target privilege level from a parsed [`PluginManifest`].
    #[must_use]
    pub fn for_manifest(manifest: &PluginManifest) -> Self {
        Self::from_requires_root(manifest.requires_root())
    }
}

/// Snapshot of the current daemon process Unix identity.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessIdentity {
    pub uid: u32,
    pub euid: u32,
    pub gid: u32,
    pub egid: u32,
}

/// Queries the current process UID/EUID/GID/EGID via `rustix`.
#[cfg(unix)]
#[must_use]
pub fn current_process_identity() -> ProcessIdentity {
    ProcessIdentity {
        uid: rustix::process::getuid().as_raw(),
        euid: rustix::process::geteuid().as_raw(),
        gid: rustix::process::getgid().as_raw(),
        egid: rustix::process::getegid().as_raw(),
    }
}
