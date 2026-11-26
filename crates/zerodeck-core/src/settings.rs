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
        self.update_map(|map| {
            map.insert(key.to_owned(), encoded);
            Ok(true)
        })
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
        self.update_map(|map| {
            insert_dotted(map, dotted_key, value)?;
            Ok(true)
        })
    }

    /// Removes a top-level setting key and atomically flushes changes to disk if it existed.
    pub fn remove(&mut self, key: &str) -> Result<Option<Value>, CoreError> {
        let mut removed = None;
        self.update_map(|map| {
            removed = map.remove(key);
            Ok(removed.is_some())
        })?;
        Ok(removed)
    }

    /// Atomically applies an in-memory settings mutation, flushing to disk only when `mutate`
    /// returns `Ok(true)`.
    pub(crate) fn update_map<F>(&mut self, mutate: F) -> Result<(), CoreError>
    where
        F: FnOnce(&mut Map<String, Value>) -> Result<bool, CoreError>,
    {
        if let Some(pending) = self.prepare_update(mutate)? {
            pending.write()?;
            self.apply(pending);
        }
        Ok(())
    }

    /// Computes the next settings map and its serialized bytes without touching disk or the
    /// in-memory values; `None` when `mutate` reports no change.
    ///
    /// Callers write the result with [`PendingSettings::write`] (blocking I/O) and then commit it
    /// with [`SettingsManager::apply`], so readers never wait on `fsync`.
    pub fn prepare_update<F>(&self, mutate: F) -> Result<Option<PendingSettings>, CoreError>
    where
        F: FnOnce(&mut Map<String, Value>) -> Result<bool, CoreError>,
    {
        let mut next = self.values.clone();
        if !mutate(&mut next)? {
            return Ok(None);
        }
        let bytes = serde_json::to_vec_pretty(&next)?;
        Ok(Some(PendingSettings {
            values: next,
            bytes,
            path: self.path.clone(),
        }))
    }

    /// Makes a successfully written [`PendingSettings`] the current in-memory values.
    pub fn apply(&mut self, pending: PendingSettings) {
        self.values = pending.values;
    }
}

/// Settings computed by [`SettingsManager::prepare_update`], waiting to be written and applied.
#[derive(Debug)]
pub struct PendingSettings {
    values: Map<String, Value>,
    bytes: Vec<u8>,
    path: PathBuf,
}

impl PendingSettings {
    /// Atomically writes these settings to disk (temp file, `fsync`, rename, directory sync).
    pub fn write(&self) -> Result<(), CoreError> {
        atomic_write(&self.path, &self.bytes)?;
        Ok(())
    }
}

/// Sets `value` at `dotted_key` inside `map`, creating missing intermediate objects.
pub(crate) fn insert_dotted(
    map: &mut Map<String, Value>,
    dotted_key: &str,
    value: Value,
) -> Result<(), CoreError> {
    let segments = parse_dotted_segments(dotted_key)?;
    let (last, parents) = segments
        .split_last()
        .ok_or_else(|| CoreError::InvalidSetting("setting path must not be empty".to_owned()))?;

    let mut current = map;
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
    Ok(())
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

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use uuid::Uuid;

    use super::SettingsManager;

    #[test]
    fn prepare_update_returns_none_when_unchanged_and_apply_updates_values() {
        let dir = std::env::temp_dir().join(format!("zerodeck-settings-{}", Uuid::new_v4()));
        let path = dir.join("loader.json");
        let mut settings = SettingsManager::open(path.clone()).expect("open settings");

        let unchanged = settings
            .prepare_update(|_| Ok(false))
            .expect("prepare no-op");
        assert!(unchanged.is_none());
        assert!(!path.exists(), "a no-op must not touch disk");

        let pending = settings
            .prepare_update(|map| {
                map.insert("k".to_owned(), json!(7));
                Ok(true)
            })
            .expect("prepare update")
            .expect("update is pending");
        assert_eq!(
            settings.values().get("k"),
            None,
            "memory changes only on apply"
        );

        pending.write().expect("write pending settings");
        let on_disk: Value =
            serde_json::from_slice(&std::fs::read(&path).expect("read loader.json"))
                .expect("parse loader.json");
        assert_eq!(on_disk, json!({"k": 7}));

        settings.apply(pending);
        assert_eq!(settings.values().get("k"), Some(&json!(7)));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
