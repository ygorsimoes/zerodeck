//! The `release.json` manifest packaged in every managed loader generation.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// File name of the manifest at the root of a generation.
pub const RELEASE_MANIFEST: &str = "release.json";
/// The only manifest schema this build understands.
pub const RELEASE_MANIFEST_SCHEMA: u32 = 1;
/// Largest manifest accepted, in bytes.
pub const MAX_RELEASE_MANIFEST_BYTES: u64 = 64 * 1024;

/// Reasons a release manifest is rejected.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ReleaseManifestError {
    /// The manifest is larger than [`MAX_RELEASE_MANIFEST_BYTES`].
    #[error("release manifest exceeds {MAX_RELEASE_MANIFEST_BYTES} bytes")]
    TooLarge,
    /// The manifest is not valid JSON of the expected shape.
    #[error("invalid release manifest: {0}")]
    Invalid(String),
    /// The manifest declares a schema this build does not understand.
    #[error("unsupported release manifest schema {0}")]
    UnsupportedSchema(u32),
    /// The manifest's build identity is empty.
    #[error("release manifest build_id is empty")]
    EmptyBuildId,
}

/// Version and build identity of one packaged loader release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseManifest {
    /// Manifest schema, currently [`RELEASE_MANIFEST_SCHEMA`].
    pub schema: u32,
    /// Workspace `SemVer` of the packaged release.
    pub version: String,
    /// Unique build identity compiled into the packaged daemon.
    pub build_id: String,
}

impl ReleaseManifest {
    /// Parses and validates a manifest.
    ///
    /// # Errors
    ///
    /// Returns an error for an oversize, malformed, unknown-schema or empty-build-id manifest.
    pub fn parse(bytes: &[u8]) -> Result<Self, ReleaseManifestError> {
        if bytes.len() as u64 > MAX_RELEASE_MANIFEST_BYTES {
            return Err(ReleaseManifestError::TooLarge);
        }
        let manifest: Self = serde_json::from_slice(bytes)
            .map_err(|error| ReleaseManifestError::Invalid(error.to_string()))?;
        if manifest.schema != RELEASE_MANIFEST_SCHEMA {
            return Err(ReleaseManifestError::UnsupportedSchema(manifest.schema));
        }
        if manifest.build_id.trim().is_empty() {
            return Err(ReleaseManifestError::EmptyBuildId);
        }
        Ok(manifest)
    }
}

#[cfg(test)]
mod tests {
    use super::{ReleaseManifest, ReleaseManifestError};

    const VALID: &str = r#"{"schema":1,"version":"0.1.0","build_id":"v0.1.0-abc"}"#;

    #[test]
    fn parse_accepts_a_valid_manifest() {
        let manifest = ReleaseManifest::parse(VALID.as_bytes()).expect("valid manifest");
        assert_eq!(manifest.version, "0.1.0");
        assert_eq!(manifest.build_id, "v0.1.0-abc");
    }

    #[test]
    fn parse_rejects_unknown_schema() {
        let raw = r#"{"schema":2,"version":"0.1.0","build_id":"x"}"#;
        assert_eq!(
            ReleaseManifest::parse(raw.as_bytes()),
            Err(ReleaseManifestError::UnsupportedSchema(2))
        );
    }

    #[test]
    fn parse_rejects_empty_build_id() {
        let raw = r#"{"schema":1,"version":"0.1.0","build_id":"  "}"#;
        assert_eq!(
            ReleaseManifest::parse(raw.as_bytes()),
            Err(ReleaseManifestError::EmptyBuildId)
        );
    }

    #[test]
    fn parse_rejects_unknown_fields() {
        let raw = r#"{"schema":1,"version":"0.1.0","build_id":"x","extra":1}"#;
        assert!(matches!(
            ReleaseManifest::parse(raw.as_bytes()),
            Err(ReleaseManifestError::Invalid(_))
        ));
    }

    #[test]
    fn parse_rejects_oversize() {
        let padding = " ".repeat(65 * 1024);
        let raw = format!("{VALID}{padding}");
        assert_eq!(
            ReleaseManifest::parse(raw.as_bytes()),
            Err(ReleaseManifestError::TooLarge)
        );
    }
}
