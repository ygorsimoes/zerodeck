//! Parsers for WebSocket call arguments.

use serde_json::Value;
use zerodeck_core::{
    DEFAULT_FILEPICKER_MAX_ENTRIES, FilePickerQuery, InstallPluginRequest, parse_install_source,
};

fn opt_string_field(val: Option<&Value>, label: &str) -> Result<Option<String>, String> {
    match val {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(format!("`{label}` must be a string")),
    }
}

fn opt_bool_field(val: Option<&Value>, default: bool, label: &str) -> Result<bool, String> {
    match val {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err(format!("`{label}` must be a boolean")),
    }
}

fn opt_usize_field(val: Option<&Value>, default: usize, label: &str) -> Result<usize, String> {
    match val {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Number(n)) => n
            .as_u64()
            .and_then(|u| usize::try_from(u).ok())
            .ok_or_else(|| format!("`{label}` must be a non-negative integer")),
        Some(_) => Err(format!("`{label}` must be a non-negative integer")),
    }
}

fn opt_string_vec_field(val: Option<&Value>, label: &str) -> Result<Vec<String>, String> {
    match val {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| format!("`{label}` elements must be strings"))
            })
            .collect(),
        Some(_) => Err(format!("`{label}` must be an array of strings")),
    }
}

pub(crate) fn parse_filepicker_query(args: &[Value]) -> Result<FilePickerQuery, String> {
    if args.len() == 1
        && let Some(obj) = args[0].as_object()
    {
        return Ok(FilePickerQuery {
            path: opt_string_field(obj.get("path"), "path")?,
            include_files: opt_bool_field(obj.get("include_files"), true, "include_files")?,
            include_folders: opt_bool_field(obj.get("include_folders"), true, "include_folders")?,
            filter: opt_string_field(obj.get("filter"), "filter")?,
            extensions: opt_string_vec_field(obj.get("extensions"), "extensions")?,
            show_hidden: opt_bool_field(obj.get("show_hidden"), false, "show_hidden")?,
            max_entries: opt_usize_field(
                obj.get("max_entries"),
                DEFAULT_FILEPICKER_MAX_ENTRIES,
                "max_entries",
            )?,
        });
    }

    Ok(FilePickerQuery {
        path: opt_string_field(args.first(), "path")?,
        include_files: opt_bool_field(args.get(1), true, "include_files")?,
        filter: opt_string_field(args.get(2), "filter")?,
        extensions: opt_string_vec_field(args.get(3), "extensions")?,
        show_hidden: opt_bool_field(args.get(4), false, "show_hidden")?,
        max_entries: opt_usize_field(args.get(5), DEFAULT_FILEPICKER_MAX_ENTRIES, "max_entries")?,
        include_folders: opt_bool_field(args.get(6), true, "include_folders")?,
    })
}

pub(crate) fn parse_install_request(args: &[Value]) -> Result<InstallPluginRequest, String> {
    let (
        url_or_path,
        expected_name,
        expected_version,
        sha256,
        install_type,
        user_confirmed,
        dir_hint,
    ) = if args.len() == 1
        && let Some(obj) = args[0].as_object()
    {
        let url_val = obj
            .get("url_or_path")
            .or_else(|| obj.get("url"))
            .or_else(|| obj.get("artifact"))
            .or_else(|| obj.get("path"));
        let url_or_path = opt_string_field(url_val, "url_or_path")?
            .ok_or_else(|| "missing `url_or_path` argument".to_owned())?;
        let expected_name = opt_string_field(obj.get("name"), "name")?;
        let expected_version = opt_string_field(obj.get("version"), "version")?;
        let sha256 = opt_string_field(obj.get("sha256").or_else(|| obj.get("hash")), "sha256")?;
        let install_type = obj.get("install_type");
        let user_confirmed = opt_bool_field(obj.get("user_confirmed"), false, "user_confirmed")?;
        let dir_hint = opt_string_field(
            obj.get("directory").or_else(|| obj.get("directory_hint")),
            "directory",
        )?;
        (
            url_or_path,
            expected_name,
            expected_version,
            sha256,
            install_type,
            user_confirmed,
            dir_hint,
        )
    } else {
        let url_or_path = opt_string_field(args.first(), "url_or_path")?
            .ok_or_else(|| "missing `url_or_path` argument".to_owned())?;
        let expected_name = opt_string_field(args.get(1), "name")?;
        let expected_version = opt_string_field(args.get(2), "version")?;
        let sha256 = opt_string_field(args.get(3), "sha256")?;
        let install_type = args.get(4);
        let user_confirmed = opt_bool_field(args.get(5), false, "user_confirmed")?;
        let dir_hint = opt_string_field(args.get(6), "directory")?;
        (
            url_or_path,
            expected_name,
            expected_version,
            sha256,
            install_type,
            user_confirmed,
            dir_hint,
        )
    };

    let source = parse_install_source(&url_or_path, sha256, install_type, user_confirmed)
        .map_err(|err| err.to_string())?;
    Ok(InstallPluginRequest {
        source,
        directory_hint: dir_hint,
        expected_name,
        expected_version,
    })
}

pub(crate) fn parse_plugin_name_arg(args: &[Value]) -> Result<String, String> {
    if args.len() == 1
        && let Some(obj) = args[0].as_object()
    {
        let raw = obj.get("plugin_name").or_else(|| obj.get("name"));
        return opt_string_field(raw, "plugin_name")?
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "missing plugin_name argument".to_owned());
    }
    opt_string_field(args.first(), "plugin_name")?
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "missing plugin_name argument".to_owned())
}

pub(crate) fn parse_uninstall_args(args: &[Value]) -> Result<(String, bool), String> {
    if args.len() == 1
        && let Some(obj) = args[0].as_object()
    {
        let name = opt_string_field(
            obj.get("plugin_name").or_else(|| obj.get("name")),
            "plugin_name",
        )?
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "missing plugin_name argument".to_owned())?;
        let purge = opt_bool_field(obj.get("purge_data"), false, "purge_data")?;
        return Ok((name, purge));
    }
    let name = opt_string_field(args.first(), "plugin_name")?
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "missing plugin_name argument".to_owned())?;
    let purge = opt_bool_field(args.get(1), false, "purge_data")?;
    Ok((name, purge))
}

pub(crate) fn parse_plugin_order_args(args: &[Value]) -> Result<Vec<String>, String> {
    if args.len() == 1
        && let Some(obj) = args[0].as_object()
    {
        let val = obj
            .get("plugin_order")
            .or_else(|| obj.get("pluginOrder"))
            .or_else(|| obj.get("order"));
        let Some(v) = val else {
            return Err("missing plugin_order argument".to_owned());
        };
        return opt_string_vec_field(Some(v), "plugin_order");
    }
    let Some(first) = args.first() else {
        return Err("missing plugin_order argument".to_owned());
    };
    if first.is_null() {
        return Err("`plugin_order` must be an array of strings".to_owned());
    }
    opt_string_vec_field(Some(first), "plugin_order")
}

pub(crate) fn parse_get_settings_key(args: &[Value]) -> Result<Option<String>, String> {
    if args.len() == 1
        && let Some(obj) = args[0].as_object()
    {
        return opt_string_field(obj.get("key"), "key");
    }
    opt_string_field(args.first(), "key")
}

pub(crate) fn parse_set_setting_args(args: &[Value]) -> Result<(String, Value), String> {
    if args.len() == 1
        && let Some(obj) = args[0].as_object()
    {
        let key = opt_string_field(obj.get("key"), "key")?
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "missing setting key".to_owned())?;
        let val = obj
            .get("value")
            .cloned()
            .ok_or_else(|| "missing setting value".to_owned())?;
        return Ok((key, val));
    }
    let key = opt_string_field(args.first(), "key")?
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "missing setting key".to_owned())?;
    let val = args
        .get(1)
        .cloned()
        .ok_or_else(|| "missing setting value".to_owned())?;
    Ok((key, val))
}
