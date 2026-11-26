//! Resolution of the immutable loader generation selected for managed operation.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

const RELEASE_MANIFEST: &str = "release.json";
const RELEASE_MANIFEST_SCHEMA: u32 = 1;
const MAX_RELEASE_MANIFEST_BYTES: u64 = 64 * 1024;
const DAEMON_BINARY: &str = "bin/zerodeckd";
const PYTHON_DIR: &str = "python";
const FRONTEND_DIR: &str = "frontend";

/// Compile-time build identity used by standalone installations.
#[must_use]
pub fn compile_time_build_id() -> String {
    option_env!("ZERODECK_BUILD_ID")
        .filter(|build_id| !build_id.is_empty())
        .unwrap_or("dev")
        .to_owned()
}

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

#[derive(Debug, Deserialize)]
struct ReleaseManifest {
    schema: u32,
    build_id: String,
    version: String,
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
        let metadata = fs::metadata(&canonical_root).with_context(|| {
            format!("inspect managed generation `{}`", canonical_root.display())
        })?;
        if !metadata.is_dir() {
            bail!(
                "managed generation `{}` is not a directory",
                canonical_root.display()
            );
        }

        let binary = ensure_regular_file(&canonical_root, DAEMON_BINARY)?;
        let python_dir = ensure_directory(&canonical_root, PYTHON_DIR)?;
        let frontend_dir = ensure_directory(&canonical_root, FRONTEND_DIR)?;
        let release_path = ensure_regular_file(&canonical_root, RELEASE_MANIFEST)?;
        for path in [&binary, &release_path] {
            ensure_within(&canonical_root, path)?;
        }
        ensure_binary_is_executable(&binary)?;

        overrides.validate(&python_dir, &frontend_dir)?;

        let python_host_path = python_dir
            .to_str()
            .with_context(|| {
                format!(
                    "managed generation Python path `{}` is not valid UTF-8",
                    python_dir.display()
                )
            })?
            .to_owned();
        let manifest_size = fs::metadata(&release_path)
            .with_context(|| format!("inspect `{}`", release_path.display()))?
            .len();
        if manifest_size > MAX_RELEASE_MANIFEST_BYTES {
            bail!(
                "`{}` exceeds {MAX_RELEASE_MANIFEST_BYTES} bytes",
                release_path.display()
            );
        }
        let release: ReleaseManifest = serde_json::from_slice(
            &fs::read(&release_path)
                .with_context(|| format!("read `{}`", release_path.display()))?,
        )
        .with_context(|| format!("parse `{}`", release_path.display()))?;
        if release.schema != RELEASE_MANIFEST_SCHEMA {
            bail!(
                "`{}` uses unsupported schema {}",
                release_path.display(),
                release.schema
            );
        }
        if release.build_id.trim().is_empty() {
            bail!("`{}` has an empty build_id", release_path.display());
        }
        let embedded_build_id = compile_time_build_id();
        if release.build_id != embedded_build_id {
            bail!(
                "`{}` build_id `{}` does not match this binary's embedded build ID `{}`; \
                 refusing to serve a mismatched managed generation",
                release_path.display(),
                release.build_id,
                embedded_build_id
            );
        }
        if release.version.trim().is_empty() {
            bail!("`{}` has an empty version", release_path.display());
        }

        Ok(Self {
            root: canonical_root,
            binary,
            python_host_path,
            frontend_dir,
            release_version: release.version,
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

fn ensure_regular_file(root: &Path, relative: &str) -> Result<PathBuf> {
    let path = root.join(relative);
    let metadata = fs::symlink_metadata(&path)
        .with_context(|| format!("managed generation is missing `{}`", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!(
            "managed generation path `{}` is not a regular file",
            path.display()
        );
    }
    Ok(path)
}

fn ensure_directory(root: &Path, relative: &str) -> Result<PathBuf> {
    let path = root.join(relative);
    let metadata = fs::symlink_metadata(&path)
        .with_context(|| format!("managed generation is missing `{}`", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!(
            "managed generation path `{}` is not a directory",
            path.display()
        );
    }
    let canonical =
        fs::canonicalize(&path).with_context(|| format!("resolve `{}`", path.display()))?;
    ensure_within(root, &canonical)?;
    Ok(canonical)
}

fn ensure_within(root: &Path, path: &Path) -> Result<()> {
    let canonical =
        fs::canonicalize(path).with_context(|| format!("resolve `{}`", path.display()))?;
    if !canonical.starts_with(root) {
        bail!(
            "managed generation path `{}` escapes `{}`",
            canonical.display(),
            root.display()
        );
    }
    Ok(())
}

fn ensure_binary_is_executable(binary: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(binary)
            .with_context(|| format!("inspect `{}`", binary.display()))?
            .permissions()
            .mode();
        if mode & 0o111 == 0 {
            bail!(
                "managed generation binary `{}` is not executable",
                binary.display()
            );
        }
    }
    #[cfg(not(unix))]
    let _ = binary;
    Ok(())
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
                r#"{{"schema":1,"build_id":"{}","version":"0.1.0"}}"#,
                super::compile_time_build_id()
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
        assert_eq!(
            resolution.root(),
            root.canonicalize().expect("canonical generation")
        );
        assert_eq!(resolution.binary(), root.join("bin/zerodeckd"));
        assert!(resolution.python_host_path().ends_with("/python"));
        assert_eq!(resolution.frontend_dir(), root.join("frontend"));
        assert_eq!(resolution.release_version(), "0.1.0");
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
    fn managed_generation_rejects_a_release_build_id_that_differs_from_the_binary() {
        let dir = TestDir::new("build-id");
        let root = dir.0.join("generation");
        write_generation(&root);
        fs::write(
            root.join("release.json"),
            r#"{"schema":1,"build_id":"not-the-running-binary","version":"0.1.0"}"#,
        )
        .expect("rewrite release.json");

        let error = ManagedGeneration::resolve(&root, &PathOverrides::default())
            .expect_err("a release build ID must match the embedded binary identity")
            .to_string();
        assert!(error.contains("build_id"), "{error}");
        assert!(error.contains("embedded"), "{error}");
    }
}
