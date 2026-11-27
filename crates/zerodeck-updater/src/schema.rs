//! One JSON-document-per-file store with a schema gate, shared by every persisted document.

use std::fs;
use std::io;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde::de::DeserializeOwned;
use zerodeck_platform::atomic_write_mode;

use crate::UpdateError;

/// A persisted document that carries an explicit schema version.
pub trait Schema: Serialize + DeserializeOwned {
    /// The only schema version this build reads or writes.
    const SCHEMA: u32;

    /// The schema version recorded in this document.
    fn schema(&self) -> u32;
}

/// Atomic JSON persistence for one [`Schema`] document.
#[derive(Debug)]
pub struct SchemaFile<T> {
    path: PathBuf,
    mode: u32,
    _document: PhantomData<fn() -> T>,
}

impl<T> Clone for SchemaFile<T> {
    fn clone(&self) -> Self {
        Self {
            path: self.path.clone(),
            mode: self.mode,
            _document: PhantomData,
        }
    }
}

impl<T> PartialEq for SchemaFile<T> {
    fn eq(&self, other: &Self) -> bool {
        self.path == other.path && self.mode == other.mode
    }
}

impl<T> Eq for SchemaFile<T> {}

impl<T: Schema> SchemaFile<T> {
    /// Creates a store for `path`; written files get permission bits `mode`.
    #[must_use]
    pub fn with_mode(path: impl AsRef<Path>, mode: u32) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
            mode,
            _document: PhantomData,
        }
    }

    /// Creates a world-readable (`0644`) store for `path`.
    #[must_use]
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self::with_mode(path, 0o644)
    }

    /// The backing file path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Reads the document, or `None` when the file is absent.
    ///
    /// # Errors
    ///
    /// Fails on I/O errors, malformed JSON or an unknown schema.
    pub fn read_optional(&self) -> Result<Option<T>, UpdateError> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(UpdateError::Io {
                    path: self.path.clone(),
                    source,
                });
            }
        };
        let document: T = serde_json::from_slice(&bytes)?;
        if document.schema() != T::SCHEMA {
            return Err(UpdateError::UnsupportedSchema(document.schema()));
        }
        Ok(Some(document))
    }

    /// Reads the document, or `T::default()` when the file is absent.
    ///
    /// # Errors
    ///
    /// Same as [`Self::read_optional`].
    pub fn read(&self) -> Result<T, UpdateError>
    where
        T: Default,
    {
        Ok(self.read_optional()?.unwrap_or_default())
    }

    /// Atomically persists `document` after rejecting an unknown schema.
    ///
    /// # Errors
    ///
    /// Fails on an unknown schema or an I/O error.
    pub fn write(&self, document: &T) -> Result<(), UpdateError> {
        if document.schema() != T::SCHEMA {
            return Err(UpdateError::UnsupportedSchema(document.schema()));
        }
        let mut bytes = serde_json::to_vec_pretty(document)?;
        bytes.push(b'\n');
        atomic_write_mode(&self.path, &bytes, self.mode)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use serde::{Deserialize, Serialize};

    use super::{Schema, SchemaFile};
    use crate::UpdateError;

    #[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
    struct Doc {
        #[serde(default)]
        schema: u32,
        value: u32,
    }

    impl Schema for Doc {
        const SCHEMA: u32 = 1;

        fn schema(&self) -> u32 {
            self.schema
        }
    }

    fn temp_path(label: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("zerodeck-schema-{label}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create dir");
        dir.join("doc.json")
    }

    #[test]
    fn schema_file_missing_is_none() {
        let store = SchemaFile::<Doc>::new(temp_path("missing"));
        assert_eq!(store.read_optional().expect("read"), None);
    }

    #[test]
    fn schema_file_rejects_unknown_schema() {
        let path = temp_path("unknown");
        std::fs::write(&path, br#"{"schema":2,"value":1}"#).expect("seed");
        let store = SchemaFile::<Doc>::new(&path);
        assert!(matches!(
            store.read_optional(),
            Err(UpdateError::UnsupportedSchema(2))
        ));
        assert!(matches!(
            store.write(&Doc {
                schema: 2,
                value: 1
            }),
            Err(UpdateError::UnsupportedSchema(2))
        ));
    }

    #[test]
    fn schema_file_roundtrips_with_requested_mode() {
        use std::os::unix::fs::PermissionsExt;
        let path = temp_path("mode");
        let store = SchemaFile::<Doc>::with_mode(&path, 0o600);
        store
            .write(&Doc {
                schema: 1,
                value: 7,
            })
            .expect("write");
        assert_eq!(
            store.read().expect("read"),
            Doc {
                schema: 1,
                value: 7
            }
        );
        let mode = std::fs::metadata(&path).expect("stat").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
