use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

use zerodeck_platform::{PlatformPaths, SafePathSet};

use crate::CoreError;

/// Default maximum number of entries returned by [`FilePicker::list`].
pub const DEFAULT_FILEPICKER_MAX_ENTRIES: usize = 5_000;

/// Hard upper bound on `max_entries` accepted by [`FilePicker::list`].
pub const MAX_FILEPICKER_MAX_ENTRIES: usize = 20_000;

const MAX_FILTER_CHARS: usize = 256;

/// Query parameters for directory browsing via [`FilePicker::list`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct FilePickerQuery {
    pub path: Option<String>,
    pub include_files: bool,
    pub include_folders: bool,
    pub filter: Option<String>,
    pub extensions: Vec<String>,
    pub show_hidden: bool,
    pub max_entries: usize,
}

impl Default for FilePickerQuery {
    fn default() -> Self {
        Self {
            path: None,
            include_files: true,
            include_folders: true,
            filter: None,
            extensions: Vec::new(),
            show_hidden: false,
            max_entries: DEFAULT_FILEPICKER_MAX_ENTRIES,
        }
    }
}

/// Single directory entry returned by [`FilePicker::list`], matching Decky's wire schema.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FilePickerEntry {
    pub name: String,
    pub path: String,
    pub realpath: String,
    pub isdir: bool,
    pub size: u64,
    pub modified: f64,
}

/// Directory listing returned by [`FilePicker::list`], matching Decky's wire schema.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FilePickerListing {
    pub path: String,
    pub realpath: String,
    pub files: Vec<FilePickerEntry>,
    pub truncated: bool,
}

/// Path-jailed directory browser built on [`SafePathSet`].
#[derive(Debug, Default, Clone, Copy)]
pub struct FilePicker;

impl FilePicker {
    /// Lists entries inside `query.path` (defaulting to `paths.user_home()`), jailed to
    /// existing roots from [`PlatformPaths::filepicker_roots`].
    pub fn list(
        paths: &PlatformPaths,
        query: FilePickerQuery,
    ) -> Result<FilePickerListing, CoreError> {
        let safe_set = SafePathSet::from_existing(paths.filepicker_roots())?;
        let compiled_filter = compile_filter(query.filter.as_deref())?;
        let normalized_extensions = normalize_extensions(&query.extensions);
        let max_entries = query.max_entries.min(MAX_FILEPICKER_MAX_ENTRIES);

        let requested_path = match query.path {
            Some(raw) if !raw.is_empty() => PathBuf::from(raw),
            _ => paths.user_home().to_path_buf(),
        };

        let canonical_dir = safe_set.resolve(&requested_path)?;
        let dir_meta = fs::metadata(&canonical_dir).map_err(|source| CoreError::Io {
            path: canonical_dir.clone(),
            source,
        })?;
        if !dir_meta.is_dir() {
            return Err(CoreError::Io {
                path: canonical_dir,
                source: std::io::Error::new(
                    std::io::ErrorKind::NotADirectory,
                    "requested filepicker path is not a directory",
                ),
            });
        }

        let display_dir = resolve_display_dir(&requested_path, &canonical_dir, &safe_set);
        let mut entries = Vec::new();
        let read_dir = fs::read_dir(&canonical_dir).map_err(|source| CoreError::Io {
            path: canonical_dir.clone(),
            source,
        })?;

        for entry_res in read_dir {
            let Ok(entry) = entry_res else {
                continue;
            };
            let file_name = entry.file_name();
            let name = file_name.to_string_lossy().into_owned();
            if !query.show_hidden && name.starts_with('.') {
                continue;
            }

            // Broken symlinks or symlinks resolving outside SafePathSet are omitted silently.
            let Ok(canonical_entry) = safe_set.resolve(entry.path()) else {
                continue;
            };
            let Ok(meta) = fs::metadata(&canonical_entry) else {
                continue;
            };

            let isdir = meta.is_dir();
            if !isdir && !meta.is_file() {
                continue;
            }
            if isdir && !query.include_folders {
                continue;
            }
            if !isdir && !query.include_files {
                continue;
            }
            if !isdir && !matches_extensions(&name, &normalized_extensions) {
                continue;
            }
            if let Some(ref filter) = compiled_filter
                && !filter.matches(&name)
            {
                continue;
            }

            let size = if isdir { 0 } else { meta.len() };
            let modified = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map_or(0.0, |d| d.as_secs_f64());

            entries.push(FilePickerEntry {
                name,
                path: display_dir.join(&file_name).to_string_lossy().into_owned(),
                realpath: canonical_entry.to_string_lossy().into_owned(),
                isdir,
                size,
                modified,
            });
        }

        entries.sort_by(|a, b| {
            b.isdir
                .cmp(&a.isdir)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
                .then_with(|| a.name.cmp(&b.name))
        });

        let truncated = if entries.len() > max_entries {
            entries.truncate(max_entries);
            true
        } else {
            false
        };

        Ok(FilePickerListing {
            path: display_dir.to_string_lossy().into_owned(),
            realpath: canonical_dir.to_string_lossy().into_owned(),
            files: entries,
            truncated,
        })
    }
}

fn resolve_display_dir(
    requested_path: &Path,
    canonical_dir: &Path,
    safe_set: &SafePathSet,
) -> PathBuf {
    let has_dot_components = requested_path
        .components()
        .any(|c| matches!(c, Component::CurDir | Component::ParentDir));
    if has_dot_components {
        return canonical_dir.to_path_buf();
    }

    let (Some(parent), Some(file_name)) = (requested_path.parent(), requested_path.file_name())
    else {
        return canonical_dir.to_path_buf();
    };

    if let Ok(canonical_parent) = safe_set.resolve(parent) {
        canonical_parent.join(file_name)
    } else {
        canonical_dir.to_path_buf()
    }
}

fn normalize_extensions(extensions: &[String]) -> Vec<String> {
    extensions
        .iter()
        .map(|ext| ext.trim_start_matches('.').to_lowercase())
        .filter(|ext| !ext.is_empty())
        .collect()
}

fn matches_extensions(name: &str, extensions: &[String]) -> bool {
    if extensions.is_empty() {
        return true;
    }
    let lowered = name.to_lowercase();
    extensions.iter().any(|ext| {
        let Some(prefix) = lowered.strip_suffix(ext.as_str()) else {
            return false;
        };
        prefix
            .strip_suffix('.')
            .is_some_and(|stem| !stem.is_empty())
    })
}

enum CompiledFilter {
    Substring(String),
    Glob(Vec<char>),
}

impl CompiledFilter {
    fn matches(&self, name: &str) -> bool {
        let lowered = name.to_lowercase();
        match self {
            Self::Substring(sub) => lowered.contains(sub),
            Self::Glob(pattern) => {
                let text: Vec<char> = lowered.chars().collect();
                glob_matches(pattern, &text)
            }
        }
    }
}

fn compile_filter(filter: Option<&str>) -> Result<Option<CompiledFilter>, CoreError> {
    let Some(raw) = filter else {
        return Ok(None);
    };
    if raw.is_empty() {
        return Ok(None);
    }
    if raw.chars().count() > MAX_FILTER_CHARS {
        return Err(CoreError::InvalidSetting(format!(
            "filepicker filter exceeds maximum length of {MAX_FILTER_CHARS} characters"
        )));
    }

    let lowered = raw.to_lowercase();
    if lowered.contains('*') || lowered.contains('?') {
        let mut collapsed = Vec::with_capacity(lowered.len());
        for ch in lowered.chars() {
            if ch == '*' && collapsed.last() == Some(&'*') {
                continue;
            }
            collapsed.push(ch);
        }
        Ok(Some(CompiledFilter::Glob(collapsed)))
    } else {
        Ok(Some(CompiledFilter::Substring(lowered)))
    }
}

fn glob_matches(pattern: &[char], text: &[char]) -> bool {
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star_pi: Option<usize> = None;
    let mut match_ti = 0usize;

    while ti < text.len() {
        if pi < pattern.len() && (pattern[pi] == '?' || pattern[pi] == text[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < pattern.len() && pattern[pi] == '*' {
            star_pi = Some(pi);
            match_ti = ti;
            pi += 1;
        } else if let Some(sp) = star_pi {
            pi = sp + 1;
            match_ti += 1;
            ti = match_ti;
        } else {
            return false;
        }
    }

    while pi < pattern.len() && pattern[pi] == '*' {
        pi += 1;
    }

    pi == pattern.len()
}
