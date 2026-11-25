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

    /// Returns a read-only view of all top-level settings entries.
    #[must_use]
    pub fn values(&self) -> &Map<String, Value> {
        &self.values
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
        let mut next = self.values.clone();
        next.insert(key.to_owned(), encoded);
        self.commit_map(&next)?;
        self.values = next;
        Ok(())
    }

    /// Reads a setting by dotted key path (e.g. `"developer.allow_unverified_installs"`).
    ///
    /// Returns `None` when the key is missing, contains empty segments, or traverses a non-object
    /// intermediate value.
    #[must_use]
    pub fn get_path(&self, dotted_key: &str) -> Option<Value> {
        let segments = parse_dotted_segments(dotted_key).ok()?;
        let (last, parents) = segments.split_last()?;
        let mut current = &self.values;
        for segment in parents {
            current = current.get(*segment)?.as_object()?;
        }
        current.get(*last).cloned()
    }

    /// Updates a setting by dotted key path, creating missing intermediate JSON objects.
    ///
    /// Rejects empty segments and non-object intermediate values without modifying on-disk state.
    pub fn set_path(&mut self, dotted_key: &str, value: Value) -> Result<(), CoreError> {
        let segments = parse_dotted_segments(dotted_key)?;
        let (last, parents) = segments.split_last().ok_or_else(|| {
            CoreError::InvalidSetting("setting path must not be empty".to_owned())
        })?;

        let mut next = self.values.clone();
        let mut current = &mut next;
        for (idx, segment) in parents.iter().enumerate() {
            if !current.contains_key(*segment) {
                current.insert((*segment).to_owned(), Value::Object(Map::new()));
            }
            let entry = current.get_mut(*segment).ok_or_else(|| {
                CoreError::InvalidSetting(format!("missing intermediate key `{segment}`"))
            })?;
            let Some(obj) = entry.as_object_mut() else {
                let prefix = parents[..=idx].join(".");
                return Err(CoreError::InvalidSetting(format!(
                    "cannot traverse non-object setting `{prefix}` while setting `{dotted_key}`"
                )));
            };
            current = obj;
        }

        current.insert((*last).to_owned(), value);
        self.commit_map(&next)?;
        self.values = next;
        Ok(())
    }

    /// Removes a top-level setting key and atomically flushes changes to disk if it existed.
    pub fn remove(&mut self, key: &str) -> Result<Option<Value>, CoreError> {
        if !self.values.contains_key(key) {
            return Ok(None);
        }
        let mut next = self.values.clone();
        let removed = next.remove(key);
        self.commit_map(&next)?;
        self.values = next;
        Ok(removed)
    }

    fn commit_map(&self, values: &Map<String, Value>) -> Result<(), CoreError> {
        let bytes = serde_json::to_vec_pretty(&Value::Object(values.clone()))?;
        atomic_write(&self.path, &bytes)?;
        Ok(())
    }
}

fn parse_dotted_segments(dotted_key: &str) -> Result<Vec<&str>, CoreError> {
    if dotted_key.is_empty() {
        return Err(CoreError::InvalidSetting(
            "setting path must not be empty".to_owned(),
        ));
    }
    let segments: Vec<&str> = dotted_key.split('.').collect();
    if segments.iter().any(|segment| segment.is_empty()) {
        return Err(CoreError::InvalidSetting(format!(
            "setting path `{dotted_key}` contains an empty segment"
        )));
    }
    Ok(segments)
}
