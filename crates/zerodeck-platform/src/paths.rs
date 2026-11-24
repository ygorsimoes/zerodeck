use std::path::{Path, PathBuf};

/// Resolved `ZeroDeck` filesystem hierarchy compatible with `decky-loader` (`~/homebrew`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformPaths {
    unprivileged_root: PathBuf,
    privileged_root: PathBuf,
    user_home: PathBuf,
}

impl PlatformPaths {
    /// Resolves `ZeroDeck` directories from process environment variables.
    #[must_use]
    pub fn from_env() -> Self {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    /// Resolves `ZeroDeck` directories using an injectable environment lookup.
    #[must_use]
    pub fn from_lookup<F>(lookup: F) -> Self
    where
        F: Fn(&str) -> Option<String>,
    {
        let non_empty = |key: &str| lookup(key).filter(|v| !v.is_empty());

        let user_home = non_empty("DECKY_USER_HOME")
            .or_else(|| non_empty("HOME"))
            .map_or_else(|| PathBuf::from("/home/deck"), PathBuf::from);

        let unprivileged_root = non_empty("UNPRIVILEGED_PATH")
            .or_else(|| non_empty("DECKY_HOME"))
            .map_or_else(|| user_home.join("homebrew"), PathBuf::from);

        let privileged_root =
            non_empty("PRIVILEGED_PATH").map_or_else(|| unprivileged_root.clone(), PathBuf::from);

        Self {
            unprivileged_root,
            privileged_root,
            user_home,
        }
    }

    /// Root directory for unprivileged plugin data, settings, and logs.
    #[must_use]
    pub fn unprivileged_root(&self) -> &Path {
        &self.unprivileged_root
    }

    /// Root directory for privileged loader services and plugin installations.
    #[must_use]
    pub fn privileged_root(&self) -> &Path {
        &self.privileged_root
    }

    /// Home directory of the host user (`DECKY_USER_HOME`).
    #[must_use]
    pub fn user_home(&self) -> &Path {
        &self.user_home
    }

    /// Directory containing installed plugins (`<privileged>/plugins`).
    #[must_use]
    pub fn plugins_dir(&self) -> PathBuf {
        self.privileged_root.join("plugins")
    }

    /// Directory containing loader and plugin settings (`<unprivileged>/settings`).
    #[must_use]
    pub fn settings_dir(&self) -> PathBuf {
        self.unprivileged_root.join("settings")
    }

    /// Directory containing plugin runtime data (`<unprivileged>/data`).
    #[must_use]
    pub fn data_dir(&self) -> PathBuf {
        self.unprivileged_root.join("data")
    }

    /// Directory containing plugin logs (`<unprivileged>/logs`).
    #[must_use]
    pub fn logs_dir(&self) -> PathBuf {
        self.unprivileged_root.join("logs")
    }

    /// Default allowlisted roots exposed to the file picker (`$HOME`, `/run/media`, `DECKY_HOME`).
    #[must_use]
    pub fn filepicker_roots(&self) -> Vec<PathBuf> {
        let mut roots = vec![
            self.user_home.clone(),
            PathBuf::from("/run/media"),
            self.unprivileged_root.clone(),
        ];
        roots.sort();
        roots.dedup();
        roots
    }
}
