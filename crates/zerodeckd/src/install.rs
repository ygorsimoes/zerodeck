//! Resolution of the immutable loader generation selected for managed operation.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use zerodeck_platform::GenerationLayout;

/// Directory overrides that could otherwise mix a managed generation with standalone assets.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PathOverrides {
    /// `PYTHONPATH`, when set to a non-empty value.
    pub python_path: Option<OsString>,
    /// `ZERODECK_FRONTEND_DIR`, when set to a non-empty value.
    pub frontend_dir: Option<OsString>,
}

impl PathOverrides {
    /// Reads the path overrides from the process environment.
    #[must_use]
    pub fn from_env() -> Self {
        Self {
            python_path: std::env::var_os("PYTHONPATH").filter(|value| !value.is_empty()),
            frontend_dir: std::env::var_os("ZERODECK_FRONTEND_DIR")
                .filter(|value| !value.is_empty()),
        }
    }

    fn validate(&self, python_dir: &Path, frontend_dir: &Path) -> Result<()> {
        if let Some(value) = &self.python_path {
            let entries: Vec<PathBuf> = std::env::split_paths(value).collect();
            if entries.len() != 1 {
                bail!(
                    "PYTHONPATH must name only the managed generation's python directory when \
                     --managed-generation is used"
                );
            }
            let selected = fs::canonicalize(&entries[0])
                .with_context(|| format!("resolve PYTHONPATH entry `{}`", entries[0].display()))?;
            if selected != python_dir {
                bail!(
                    "PYTHONPATH selects `{}` but --managed-generation uses `{}`; refusing to mix \
                     a managed generation with a standalone Python path",
                    selected.display(),
                    python_dir.display()
                );
            }
        }

        if let Some(value) = &self.frontend_dir {
            let selected = fs::canonicalize(PathBuf::from(value)).with_context(|| {
                format!(
                    "resolve ZERODECK_FRONTEND_DIR `{}`",
                    value.to_string_lossy()
                )
            })?;
            if selected != frontend_dir {
                bail!(
                    "ZERODECK_FRONTEND_DIR selects `{}` but --managed-generation uses `{}`; \
                     refusing to mix a managed generation with a standalone frontend",
                    selected.display(),
                    frontend_dir.display()
                );
            }
        }

        Ok(())
    }
}

/// One immutable generation resolved for the lifetime of the daemon process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedGeneration {
    root: PathBuf,
    binary: PathBuf,
    python_host_path: String,
    frontend_dir: PathBuf,
    release_version: String,
}

impl ManagedGeneration {
    /// Resolves `root` once and verifies its complete managed layout.
    pub fn resolve(root: &Path, overrides: &PathOverrides) -> Result<Self> {
        let canonical_root = fs::canonicalize(root)
            .with_context(|| format!("resolve managed generation `{}`", root.display()))?;
        // Root daemon: reject a generation reachable through a user-writable or symlinked
        // ancestor. Unprivileged development/test resolution is unchanged.
        zerodeck_platform::ensure_root_owned_ancestors(&canonical_root).map_err(|violation| {
            anyhow::anyhow!(
                "managed generation `{}` is unsafe to run as root: {violation}",
                canonical_root.display()
            )
        })?;
        let metadata = fs::metadata(&canonical_root).with_context(|| {
            format!("inspect managed generation `{}`", canonical_root.display())
        })?;
        if !metadata.is_dir() {
            bail!(
                "managed generation `{}` is not a directory",
                canonical_root.display()
            );
        }

        let layout = GenerationLayout::open(&canonical_root).map_err(|error| {
            anyhow::anyhow!(
                "managed generation `{}` is invalid: {error}",
                canonical_root.display()
            )
        })?;
        overrides.validate(layout.python_dir(), layout.frontend_dir())?;
        let python_host_path = layout
            .python_dir()
            .to_str()
            .with_context(|| {
                format!(
                    "managed generation Python path `{}` is not valid UTF-8",
                    layout.python_dir().display()
                )
            })?
            .to_owned();
        let release = layout.manifest();
        if release.build_id != zerodeck_server::build_id() {
            bail!(
                "`release.json` build_id `{}` does not match this binary's embedded build ID \
                 `{}`; refusing to serve a mismatched managed generation",
                release.build_id,
                zerodeck_server::build_id()
            );
        }
        let binary_version = env!("CARGO_PKG_VERSION");
        if release.version != binary_version {
            bail!(
                "`release.json` version `{}` does not match this binary's version `{binary_version}`; \
                 refusing to serve a mismatched managed generation",
                release.version
            );
        }

        Ok(Self {
            root: layout.root().to_path_buf(),
            binary: layout.daemon().to_path_buf(),
            python_host_path,
            frontend_dir: layout.frontend_dir().to_path_buf(),
            release_version: layout.manifest().version.clone(),
        })
    }

    /// Resolves a generation using the process environment's path overrides.
    pub fn resolve_from_env(root: &Path) -> Result<Self> {
        Self::resolve(root, &PathOverrides::from_env())
    }

    /// Requires the running process to be this generation's packaged `bin/zerodeckd`.
    pub fn verify_running_binary(&self) -> Result<()> {
        let running = std::env::current_exe().context("resolve the running daemon executable")?;
        let running = fs::canonicalize(&running)
            .with_context(|| format!("canonicalize running daemon `{}`", running.display()))?;
        let expected = fs::canonicalize(&self.binary)
            .with_context(|| format!("canonicalize `{}`", self.binary.display()))?;
        if running != expected {
            bail!(
                "running daemon `{}` is not the managed generation binary `{}` for `{}`; \
                 refusing to mix the running executable with the selected generation",
                running.display(),
                expected.display(),
                self.root.display()
            );
        }
        Ok(())
    }

    /// Canonical generation root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The daemon binary packaged in this generation.
    #[must_use]
    pub fn binary(&self) -> &Path {
        &self.binary
    }

    /// The `PYTHONPATH` entry containing `zerodeck_py_host`.
    #[must_use]
    pub fn python_host_path(&self) -> &str {
        &self.python_host_path
    }

    /// The packaged `@zerodeck/frontend` directory.
    #[must_use]
    pub fn frontend_dir(&self) -> &Path {
        &self.frontend_dir
    }

    /// Release version recorded in this generation's `release.json`.
    #[must_use]
    pub fn release_version(&self) -> &str {
        &self.release_version
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::{ManagedGeneration, PathOverrides};

    static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "zerodeck-install-{label}-{}-{}",
                std::process::id(),
                NEXT_DIR.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).expect("create temp test dir");
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn write_generation(root: &Path) {
        fs::create_dir_all(root.join("bin")).expect("create bin");
        fs::create_dir_all(root.join("python")).expect("create python");
        fs::create_dir_all(root.join("frontend")).expect("create frontend");
        let binary = root.join("bin/zerodeckd");
        fs::write(&binary, b"daemon").expect("write binary");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&binary, fs::Permissions::from_mode(0o755))
                .expect("make binary executable");
        }
        fs::write(
            root.join("release.json"),
            format!(
                r#"{{"schema":1,"build_id":"{}","version":"{}"}}"#,
                zerodeck_server::build_id(),
                env!("CARGO_PKG_VERSION")
            ),
        )
        .expect("write release.json");
    }

    #[test]
    fn managed_generation_resolves_components_and_release_identity() {
        let dir = TestDir::new("resolve");
        let root = dir.0.join("generation");
        write_generation(&root);

        let resolution =
            ManagedGeneration::resolve(&root, &PathOverrides::default()).expect("resolve");
        let canonical = root.canonicalize().expect("canonical generation");
        assert_eq!(resolution.root(), canonical);
        assert_eq!(resolution.binary(), canonical.join("bin/zerodeckd"));
        assert!(resolution.python_host_path().ends_with("/python"));
        assert_eq!(resolution.frontend_dir(), canonical.join("frontend"));
        assert_eq!(resolution.release_version(), env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn managed_generation_rejects_overrides_selecting_other_paths() {
        let dir = TestDir::new("overrides");
        let root = dir.0.join("generation");
        write_generation(&root);
        let legacy_python = dir.0.join("legacy/python");
        let legacy_frontend = dir.0.join("legacy/frontend");
        fs::create_dir_all(&legacy_python).expect("create legacy python");
        fs::create_dir_all(&legacy_frontend).expect("create legacy frontend");

        let python_conflict = PathOverrides {
            python_path: Some(legacy_python.into_os_string()),
            ..PathOverrides::default()
        };
        let error = ManagedGeneration::resolve(&root, &python_conflict)
            .expect_err("a different PYTHONPATH must be rejected")
            .to_string();
        assert!(error.contains("PYTHONPATH"), "{error}");
        assert!(error.contains("--managed-generation"), "{error}");

        let frontend_conflict = PathOverrides {
            frontend_dir: Some(legacy_frontend.into_os_string()),
            ..PathOverrides::default()
        };
        let error = ManagedGeneration::resolve(&root, &frontend_conflict)
            .expect_err("a different frontend must be rejected")
            .to_string();
        assert!(error.contains("ZERODECK_FRONTEND_DIR"), "{error}");
        assert!(error.contains("--managed-generation"), "{error}");

        let matching = PathOverrides {
            python_path: Some(root.join("python").into_os_string()),
            frontend_dir: Some(root.join("frontend").into_os_string()),
        };
        ManagedGeneration::resolve(&root, &matching)
            .expect("overrides selecting the managed generation are allowed");
    }

    #[test]
    fn managed_generation_requires_a_complete_layout() {
        let dir = TestDir::new("incomplete");
        let root = dir.0.join("generation");
        write_generation(&root);
        fs::remove_file(root.join("release.json")).expect("remove release.json");

        let error = ManagedGeneration::resolve(&root, &PathOverrides::default())
            .expect_err("missing release.json must fail closed")
            .to_string();
        assert!(error.contains("release.json"), "{error}");
    }

    #[test]
    fn managed_generation_rejects_a_different_running_binary() {
        let dir = TestDir::new("running-binary");
        let root = dir.0.join("generation");
        write_generation(&root);
        let resolution =
            ManagedGeneration::resolve(&root, &PathOverrides::default()).expect("resolve");

        let error = resolution
            .verify_running_binary()
            .expect_err("the test process is not the generation's binary")
            .to_string();
        assert!(error.contains("running daemon"), "{error}");
        assert!(error.contains("managed generation"), "{error}");
    }

    #[test]
    fn managed_generation_rejects_a_release_version_that_differs_from_the_binary() {
        let dir = TestDir::new("release-version");
        let root = dir.0.join("generation");
        write_generation(&root);
        fs::write(
            root.join("release.json"),
            format!(
                r#"{{"schema":1,"build_id":"{}","version":"999.999.999"}}"#,
                zerodeck_server::build_id()
            ),
        )
        .expect("rewrite release.json");

        let error = ManagedGeneration::resolve(&root, &PathOverrides::default())
            .expect_err("a release version must match the binary version")
            .to_string();
        assert!(error.contains("version"), "{error}");
        assert!(error.contains("binary"), "{error}");
    }

    #[test]
    fn managed_generation_rejects_a_release_build_id_that_differs_from_the_binary() {
        let dir = TestDir::new("build-id");
        let root = dir.0.join("generation");
        write_generation(&root);
        fs::write(
            root.join("release.json"),
            format!(
                r#"{{"schema":1,"build_id":"not-the-running-binary","version":"{}"}}"#,
                env!("CARGO_PKG_VERSION")
            ),
        )
        .expect("rewrite release.json");

        let error = ManagedGeneration::resolve(&root, &PathOverrides::default())
            .expect_err("a release build ID must match the embedded binary identity")
            .to_string();
        assert!(error.contains("build_id"), "{error}");
        assert!(error.contains("embedded"), "{error}");
    }
}
