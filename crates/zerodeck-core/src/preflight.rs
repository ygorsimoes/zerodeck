//! Checks that run before every plugin worker spawn.

use tracing::warn;
use zerodeck_platform::OwnershipViolation;

/// What [`preflight`] needs to know about one spawn.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PreflightInput<'a> {
    pub requires_root: bool,
    pub daemon_is_root: bool,
    pub allow_unsafe_root_plugins: bool,
    pub python_host_missing: Option<&'a str>,
}

/// Returns the reason a plugin must not start, or `Ok` when it may.
///
/// A missing Python host blocks every backend plugin. A `"root"` plugin started by a root daemon
/// must pass `tree_check` unless `developer.allow_unsafe_root_plugins` is set, which only logs a
/// warning.
pub(crate) fn preflight(
    input: &PreflightInput<'_>,
    tree_check: impl FnOnce() -> Result<(), OwnershipViolation>,
) -> Result<(), String> {
    if let Some(reason) = input.python_host_missing {
        return Err(reason.to_owned());
    }
    if !(input.requires_root && input.daemon_is_root) {
        return Ok(());
    }
    if input.allow_unsafe_root_plugins {
        warn!(
            "developer.allow_unsafe_root_plugins is set: starting a root plugin without checking \
             that only root can modify its files"
        );
        return Ok(());
    }
    tree_check().map_err(|violation| violation.to_string())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use zerodeck_platform::{OwnershipViolation, ViolationKind};

    use super::{PreflightInput, preflight};

    fn input(requires_root: bool, daemon_is_root: bool) -> PreflightInput<'static> {
        PreflightInput {
            requires_root,
            daemon_is_root,
            allow_unsafe_root_plugins: false,
            python_host_missing: None,
        }
    }

    fn violation() -> Result<(), OwnershipViolation> {
        Err(OwnershipViolation {
            path: PathBuf::from("/home/deck/homebrew/plugins/demo"),
            kind: ViolationKind::NotRootOwned { uid: 1000 },
        })
    }

    #[test]
    fn blocks_root_plugin_with_violation_when_daemon_is_root() {
        let reason = preflight(&input(true, true), violation).expect_err("must block");
        assert!(
            reason.contains("/home/deck/homebrew/plugins/demo"),
            "{reason}"
        );
        assert!(preflight(&input(true, true), || Ok(())).is_ok());
    }

    #[test]
    fn skips_tree_check_when_daemon_is_not_root() {
        let result = preflight(&input(true, false), || panic!("tree check must not run"));
        assert_eq!(result, Ok(()));
    }

    #[test]
    fn skips_tree_check_for_non_root_plugins() {
        let result = preflight(&input(false, true), || panic!("tree check must not run"));
        assert_eq!(result, Ok(()));
    }

    #[test]
    fn opt_in_bypasses_tree_check() {
        let opted_in = PreflightInput {
            allow_unsafe_root_plugins: true,
            ..input(true, true)
        };
        let result = preflight(&opted_in, || panic!("tree check must not run"));
        assert_eq!(result, Ok(()));
    }

    #[test]
    fn missing_python_host_blocks_every_backend_plugin() {
        for (requires_root, daemon_is_root, allow) in [
            (false, false, false),
            (false, true, false),
            (true, true, false),
            (true, true, true),
        ] {
            let missing = PreflightInput {
                allow_unsafe_root_plugins: allow,
                python_host_missing: Some("zerodeck_py_host is not importable"),
                ..input(requires_root, daemon_is_root)
            };
            let result = preflight(&missing, || Ok(()));
            assert_eq!(
                result,
                Err("zerodeck_py_host is not importable".to_owned()),
                "requires_root={requires_root} daemon_is_root={daemon_is_root} allow={allow}"
            );
        }
    }
}
