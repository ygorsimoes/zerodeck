use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};
use zerodeck_platform::atomic_write;

use crate::CoreError;

/// Atomic JSON settings store that preserves corrupt files rather than truncating them.
#[derive(Debug, Clone)]
pub struct SettingsManager {
    path: PathBuf,
    values: Map<String, Value>,
}

impl SettingsManager {
    /// Opens an existing settings JSON file or initializes an empty map when absent.
    pub fn open(path: PathBuf) -> Result<Self, CoreError> {
        let values = match fs::read_to_string(&path) {
            Ok(raw) => {
                let parsed: Value =
                    serde_json::from_str(&raw).map_err(|source| CoreError::SettingsParse {
                        path: path.clone(),
                        source,
                    })?;
                match parsed {
                    Value::Object(map) => map,
                    _ => return Err(CoreError::SettingsNotObject { path }),
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Map::new(),
            Err(source) => return Err(CoreError::Io { path, source }),
        };

        Ok(Self { path, values })
    }

    /// Path to the backing settings JSON file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Reads and deserializes a setting value if present and compatible with `T`.
    #[must_use]
    pub fn get<T: DeserializeOwned>(&self, key: &str) -> Option<T> {
        self.values
            .get(key)
            .cloned()
            .and_then(|v| serde_json::from_value(v).ok())
    }

    /// Updates a setting key and atomically flushes the JSON file to disk.
    pub fn set<T: Serialize>(&mut self, key: &str, value: T) -> Result<(), CoreError> {
        let encoded = serde_json::to_value(value)?;
        self.values.insert(key.to_owned(), encoded);
        self.commit()
    }

    fn commit(&self) -> Result<(), CoreError> {
        let bytes = serde_json::to_vec_pretty(&Value::Object(self.values.clone()))?;
        atomic_write(&self.path, &bytes)?;
        Ok(())
    }
}
