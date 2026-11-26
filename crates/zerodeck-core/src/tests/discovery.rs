//! Plugin discovery from `plugin.json` / `package.json`.

use super::*;

#[test]
fn discover_plugins_detects_esmodule_legacy_and_passive() {
    let dir = TestDir::new("discover");

    let esm = dir.0.join("esm-plugin");
    fs::create_dir_all(&esm).expect("create esm dir");
    fs::write(
        esm.join("plugin.json"),
        r#"{"name": "ESM Plugin", "author": "Alice", "api_version": 1}"#,
    )
    .expect("write esm plugin.json");
    fs::write(
        esm.join("package.json"),
        r#"{"version": "2.0.0", "type": "module"}"#,
    )
    .expect("write esm package.json");
    fs::write(esm.join("main.py"), "class Plugin: pass").expect("write main.py");

    let legacy = dir.0.join("legacy-plugin");
    fs::create_dir_all(&legacy).expect("create legacy dir");
    fs::write(
        legacy.join("plugin.json"),
        r#"{"name": "Legacy Plugin", "author": "Bob", "flags": ["root"]}"#,
    )
    .expect("write legacy plugin.json");
    fs::write(legacy.join("main.py"), "class Plugin: pass").expect("write legacy main.py");

    let passive = dir.0.join("passive-plugin");
    fs::create_dir_all(&passive).expect("create passive dir");
    fs::write(
        passive.join("plugin.json"),
        r#"{"name": "Passive Plugin", "author": "Carol"}"#,
    )
    .expect("write passive plugin.json");

    let mut plugins = discover_plugins(&dir.0).expect("discover plugins");
    plugins.sort_by(|a, b| a.manifest.name.cmp(&b.manifest.name));

    assert_eq!(plugins.len(), 3);
    assert_eq!(plugins[0].manifest.name, "ESM Plugin");
    assert_eq!(plugins[0].load_type, PluginLoadType::EsModuleV1);
    assert!(!plugins[0].passive);
    assert!(plugins[0].has_entrypoint);

    assert_eq!(plugins[1].manifest.name, "Legacy Plugin");
    assert_eq!(plugins[1].load_type, PluginLoadType::LegacyEvalIife);
    assert!(!plugins[1].passive);
    assert!(plugins[1].manifest.requires_root());

    assert_eq!(plugins[2].manifest.name, "Passive Plugin");
    assert!(plugins[2].passive);
    assert!(!plugins[2].has_entrypoint);
}

#[test]
fn discover_plugins_skips_invalid_manifests_and_keeps_valid_plugins() {
    let dir = TestDir::new("discover-invalid");

    let good = dir.0.join("good-plugin");
    fs::create_dir_all(&good).expect("create good dir");
    fs::write(
        good.join("plugin.json"),
        r#"{"name": "Good Plugin", "author": "Alice"}"#,
    )
    .expect("write good plugin.json");

    let bad_plugin_json = dir.0.join("bad-plugin-json");
    fs::create_dir_all(&bad_plugin_json).expect("create bad plugin.json dir");
    fs::write(bad_plugin_json.join("plugin.json"), "{not json").expect("write bad plugin.json");

    let bad_package_json = dir.0.join("bad-package-json");
    fs::create_dir_all(&bad_package_json).expect("create bad package.json dir");
    fs::write(
        bad_package_json.join("plugin.json"),
        r#"{"name": "Bad Package", "author": "Bob"}"#,
    )
    .expect("write plugin.json");
    fs::write(bad_package_json.join("package.json"), "[1, 2").expect("write bad package.json");

    let plugins = discover_plugins(&dir.0).expect("discovery must tolerate invalid plugins");
    let names: Vec<&str> = plugins.iter().map(|p| p.manifest.name.as_str()).collect();
    assert_eq!(names, vec!["Good Plugin"]);
}

#[test]
fn discover_plugins_returns_plugins_sorted_by_directory() {
    let dir = TestDir::new("discover-order");
    let names = [
        "zeta", "alpha", "Mid", "beta-2", "beta-10", "omega", "kappa", "delta",
    ];
    for name in names {
        let plugin = dir.0.join(name);
        fs::create_dir_all(&plugin).expect("create plugin dir");
        fs::write(
            plugin.join("plugin.json"),
            format!(r#"{{"name": "{name}", "author": "Alice"}}"#),
        )
        .expect("write plugin.json");
    }

    let plugins = discover_plugins(&dir.0).expect("discover plugins");
    let found: Vec<&str> = plugins.iter().map(|p| p.directory.as_str()).collect();
    let mut expected = names.to_vec();
    expected.sort_unstable();
    assert_eq!(found, expected);
}

#[test]
fn discover_plugins_skips_duplicate_names_keeping_first_directory() {
    let dir = TestDir::new("discover-duplicate-name");
    for directory in ["b-orig", "a-copy"] {
        let plugin = dir.0.join(directory);
        fs::create_dir_all(&plugin).expect("create plugin dir");
        fs::write(
            plugin.join("plugin.json"),
            r#"{"name": "Same", "author": "Alice"}"#,
        )
        .expect("write plugin.json");
    }

    let plugins = discover_plugins(&dir.0).expect("discover plugins");
    assert_eq!(plugins.len(), 1);
    assert_eq!(plugins[0].manifest.name, "Same");
    assert_eq!(plugins[0].directory, "a-copy");
}

#[test]
fn discover_plugins_ignores_hidden_staging_directories() {
    let dir = TestDir::new("discover-hidden");
    for name in ["visible", ".visible.staging-1234"] {
        let plugin = dir.0.join(name);
        fs::create_dir_all(&plugin).expect("create plugin dir");
        fs::write(
            plugin.join("plugin.json"),
            format!(r#"{{"name": "{name}", "author": "Alice"}}"#),
        )
        .expect("write plugin.json");
    }

    let plugins = discover_plugins(&dir.0).expect("discover plugins");
    let dirs: Vec<&str> = plugins.iter().map(|p| p.directory.as_str()).collect();
    assert_eq!(dirs, vec!["visible"]);
}
