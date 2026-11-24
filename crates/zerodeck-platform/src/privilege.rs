use zerodeck_protocol::PluginManifest;

#[cfg(unix)]
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt};
#[cfg(unix)]
use std::path::{Path, PathBuf};

#[cfg(unix)]
use crate::{PlatformError, PlatformPaths};

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

/// Resolved non-root Unix identity used when dropping privileges for standard plugins.
#[cfg(unix)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostUserIdentity {
    pub username: String,
    pub uid: u32,
    pub gid: u32,
    pub supplemental_gids: Vec<u32>,
    pub home_dir: PathBuf,
}

#[cfg(unix)]
#[derive(Debug, Clone, PartialEq, Eq)]
struct PasswdEntry {
    username: String,
    uid: u32,
    gid: u32,
    home_dir: PathBuf,
}

#[cfg(unix)]
impl HostUserIdentity {
    /// Resolves the target unprivileged host user from the active process and `/etc/passwd`.
    pub fn resolve(paths: &PlatformPaths) -> Result<Self, PlatformError> {
        let identity = current_process_identity();
        let env_user = std::env::var("DECKY_USER")
            .ok()
            .filter(|v| !v.is_empty())
            .or_else(|| std::env::var("USER").ok().filter(|v| !v.is_empty()));

        if identity.euid != 0 {
            return Self::resolve_from(paths, identity.euid, "", "", env_user.as_deref(), None);
        }

        let passwd = fs::read_to_string("/etc/passwd").map_err(|source| PlatformError::Io {
            path: PathBuf::from("/etc/passwd"),
            source,
        })?;
        let group = fs::read_to_string("/etc/group").unwrap_or_default();
        let decky_user = std::env::var("DECKY_USER").ok().filter(|v| !v.is_empty());
        let decky_home_owner = std::env::var("DECKY_USER_HOME")
            .ok()
            .filter(|v| !v.is_empty())
            .and_then(|dir| fs::metadata(dir).ok())
            .map(|meta| meta.uid())
            .or_else(|| {
                fs::metadata("/home/deck")
                    .ok()
                    .map(|meta| meta.uid())
                    .filter(|uid| *uid != 0)
            });

        Self::resolve_from(
            paths,
            identity.euid,
            &passwd,
            &group,
            decky_user.as_deref(),
            decky_home_owner,
        )
    }

    /// Resolves the unprivileged host user using injectable `/etc/passwd`, `/etc/group`, and env inputs.
    pub fn resolve_from(
        paths: &PlatformPaths,
        euid: u32,
        passwd: &str,
        group: &str,
        env_user: Option<&str>,
        home_owner_uid: Option<u32>,
    ) -> Result<Self, PlatformError> {
        if euid != 0 {
            let current = current_process_identity();
            let username = env_user
                .filter(|v| !v.is_empty())
                .map_or_else(|| format!("uid-{}", current.uid), str::to_owned);
            return Ok(Self {
                username,
                uid: current.uid,
                gid: current.gid,
                supplemental_gids: Vec::new(),
                home_dir: paths.user_home().to_path_buf(),
            });
        }

        let entries = parse_passwd(passwd);
        let picked = env_user
            .filter(|name| !name.is_empty())
            .and_then(|name| {
                entries
                    .iter()
                    .find(|entry| entry.username == name && entry.uid != 0)
            })
            .or_else(|| {
                home_owner_uid
                    .filter(|uid| *uid != 0)
                    .and_then(|uid| entries.iter().find(|entry| entry.uid == uid))
            })
            .or_else(|| {
                entries
                    .iter()
                    .find(|entry| entry.username == "deck" && entry.uid != 0)
            })
            .ok_or_else(|| {
                PlatformError::HostUserNotFound(
                    "set DECKY_USER or DECKY_USER_HOME to a non-root user".to_owned(),
                )
            })?;

        let supplemental_gids = parse_supplemental_groups(group, &picked.username, picked.gid);
        Ok(Self {
            username: picked.username.clone(),
            uid: picked.uid,
            gid: picked.gid,
            supplemental_gids,
            home_dir: picked.home_dir.clone(),
        })
    }
}

#[cfg(unix)]
fn parse_passwd(contents: &str) -> Vec<PasswdEntry> {
    contents
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                return None;
            }
            let mut fields = trimmed.split(':');
            let username = fields.next()?.trim();
            let _password = fields.next()?;
            let uid: u32 = fields.next()?.trim().parse().ok()?;
            let gid: u32 = fields.next()?.trim().parse().ok()?;
            let _gecos = fields.next()?;
            let home_dir = fields.next()?.trim();
            if username.is_empty() || home_dir.is_empty() {
                return None;
            }
            Some(PasswdEntry {
                username: username.to_owned(),
                uid,
                gid,
                home_dir: PathBuf::from(home_dir),
            })
        })
        .collect()
}

#[cfg(unix)]
fn parse_supplemental_groups(contents: &str, username: &str, primary_gid: u32) -> Vec<u32> {
    let mut gids = Vec::new();
    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let mut fields = trimmed.split(':');
        let Some(_group_name) = fields.next() else {
            continue;
        };
        let Some(_password) = fields.next() else {
            continue;
        };
        let Some(gid) = fields.next().and_then(|raw| raw.trim().parse::<u32>().ok()) else {
            continue;
        };
        let Some(members) = fields.next() else {
            continue;
        };
        if gid != primary_gid && members.split(',').any(|m| m.trim() == username) {
            gids.push(gid);
        }
    }
    gids.sort_unstable();
    gids.dedup();
    gids
}

/// Ensures `path` exists with optional POSIX permissions and ownership (applied only when `euid == 0`).
#[cfg(unix)]
pub fn ensure_owned_dir(
    path: &Path,
    owner: Option<(u32, u32)>,
    mode: Option<u32>,
) -> Result<(), PlatformError> {
    fs::create_dir_all(path).map_err(|source| PlatformError::Io {
        path: path.to_path_buf(),
        source,
    })?;

    if let Some(perms) = mode {
        fs::set_permissions(path, fs::Permissions::from_mode(perms)).map_err(|source| {
            PlatformError::Io {
                path: path.to_path_buf(),
                source,
            }
        })?;
    }

    if let Some((uid, gid)) = owner
        && current_process_identity().euid == 0
    {
        let uid = rustix::fs::Uid::from_raw(uid);
        let gid = rustix::fs::Gid::from_raw(gid);
        rustix::fs::chown(path, Some(uid), Some(gid)).map_err(|err| PlatformError::Io {
            path: path.to_path_buf(),
            source: std::io::Error::from_raw_os_error(err.raw_os_error()),
        })?;
    }

    Ok(())
}
