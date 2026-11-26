//! Path-jailed file picker listings.

use super::*;

fn filepicker_test_paths(
    home: &std::path::Path,
    media: &std::path::Path,
) -> zerodeck_platform::PlatformPaths {
    let home_str = home.display().to_string();
    let media_str = media.display().to_string();
    let lookup = std::collections::HashMap::from([
        ("DECKY_USER_HOME", home_str.as_str()),
        ("HOME", home_str.as_str()),
        ("UNPRIVILEGED_PATH", media_str.as_str()),
        ("PRIVILEGED_PATH", media_str.as_str()),
    ]);
    zerodeck_platform::PlatformPaths::from_lookup(|k| lookup.get(k).map(|v| (*v).to_owned()))
}

fn entry_names(listing: &crate::FilePickerListing) -> Vec<&str> {
    listing.files.iter().map(|f| f.name.as_str()).collect()
}

#[test]
fn filepicker_lists_allowed_roots_filters_extensions_and_omits_escaping_symlinks() {
    let dir = TestDir::new("filepicker");
    let home = dir.0.join("home");
    let media = dir.0.join("media");
    let outside = TestDir::new("filepicker-outside");

    fs::create_dir_all(home.join("sub")).expect("create home/sub");
    fs::create_dir_all(&media).expect("create media root");
    fs::write(home.join(".hidden"), b"secret-dot").expect("write .hidden");
    fs::write(home.join("a.zip"), b"zip-bytes").expect("write a.zip");
    fs::write(home.join("b.txt"), b"txt").expect("write b.txt");
    fs::write(media.join("pkg.zip"), b"media-pkg").expect("write media/pkg.zip");
    let outside_secret = outside.0.join("secret.txt");
    fs::write(&outside_secret, b"outside").expect("write outside secret");

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&outside_secret, home.join("leak"))
            .expect("create escaping symlink");
        std::os::unix::fs::symlink(dir.0.join("missing-target"), home.join("dangling"))
            .expect("create broken symlink");
        std::os::unix::fs::symlink(&media, home.join("to_media"))
            .expect("create allowed cross-root symlink");
    }

    let paths = filepicker_test_paths(&home, &media);
    let listing = crate::FilePicker::list(&paths, crate::FilePickerQuery::default())
        .expect("default home listing must succeed without /run/media");
    assert!(!listing.truncated);
    assert_eq!(
        listing.realpath,
        fs::canonicalize(&home)
            .expect("canonical home")
            .display()
            .to_string()
    );

    #[cfg(unix)]
    assert_eq!(
        entry_names(&listing),
        vec!["sub", "to_media", "a.zip", "b.txt"]
    );
    #[cfg(not(unix))]
    assert_eq!(entry_names(&listing), vec!["sub", "a.zip", "b.txt"]);

    let sub = listing.files.iter().find(|f| f.name == "sub").expect("sub");
    assert!(sub.isdir && sub.size == 0 && sub.modified > 0.0);
    let zip = listing
        .files
        .iter()
        .find(|f| f.name == "a.zip")
        .expect("a.zip");
    assert!(!zip.isdir && zip.size == 9 && zip.modified > 0.0);

    let hidden = crate::FilePicker::list(
        &paths,
        crate::FilePickerQuery {
            show_hidden: true,
            ..crate::FilePickerQuery::default()
        },
    )
    .expect("show_hidden listing");
    assert!(entry_names(&hidden).contains(&".hidden"));

    let ext = crate::FilePicker::list(
        &paths,
        crate::FilePickerQuery {
            extensions: vec!["ZIP".to_owned()],
            ..crate::FilePickerQuery::default()
        },
    )
    .expect("extension listing");
    assert!(entry_names(&ext).contains(&"a.zip") && entry_names(&ext).contains(&"sub"));
    assert!(!entry_names(&ext).contains(&"b.txt"));

    let media_listing = crate::FilePicker::list(
        &paths,
        crate::FilePickerQuery {
            path: Some(media.display().to_string()),
            ..crate::FilePickerQuery::default()
        },
    )
    .expect("media root listing");
    assert_eq!(entry_names(&media_listing), vec!["pkg.zip"]);

    let truncated = crate::FilePicker::list(
        &paths,
        crate::FilePickerQuery {
            max_entries: 1,
            ..crate::FilePickerQuery::default()
        },
    )
    .expect("truncated listing");
    assert!(truncated.truncated);
    assert_eq!(entry_names(&truncated), vec!["sub"]);

    let err_escape = crate::FilePicker::list(
        &paths,
        crate::FilePickerQuery {
            path: Some(outside.0.display().to_string()),
            ..crate::FilePickerQuery::default()
        },
    )
    .expect_err("outside path must fail with PathEscape");
    assert!(matches!(
        err_escape,
        CoreError::Platform(zerodeck_platform::PlatformError::PathEscape { .. })
    ));
}

#[test]
fn filepicker_handles_empty_path_non_dir_filters_sorting_and_serde() {
    let dir = TestDir::new("filepicker-edge");
    let home = dir.0.join("home");
    fs::create_dir_all(home.join("Zebra")).expect("create Zebra dir");
    fs::create_dir_all(home.join("alpha")).expect("create alpha dir");
    fs::write(home.join("Beta.txt"), b"beta").expect("write Beta.txt");
    fs::write(home.join("alpha.tar.gz"), b"targz").expect("write alpha.tar.gz");
    fs::write(home.join("myzip"), b"not-zip").expect("write myzip");
    fs::write(home.join(".zip"), b"dotzip").expect("write .zip");

    let paths = filepicker_test_paths(&home, &home);

    // Empty string path defaults to home, and dirs sort before files case-insensitively.
    let listing = crate::FilePicker::list(
        &paths,
        crate::FilePickerQuery {
            path: Some(String::new()),
            ..crate::FilePickerQuery::default()
        },
    )
    .expect("empty path defaults to home");
    assert_eq!(
        entry_names(&listing),
        vec!["alpha", "Zebra", "alpha.tar.gz", "Beta.txt", "myzip"]
    );

    // Multi-part extension with leading dot matches `alpha.tar.gz` and ignores `myzip`/`.zip`.
    let ext_listing = crate::FilePicker::list(
        &paths,
        crate::FilePickerQuery {
            include_folders: false,
            show_hidden: true,
            extensions: vec![".TAR.GZ".to_owned()],
            ..crate::FilePickerQuery::default()
        },
    )
    .expect("multi-part extension filter");
    assert_eq!(entry_names(&ext_listing), vec!["alpha.tar.gz"]);

    // Case-insensitive substring and `?`/`*` glob filter.
    let sub_listing = crate::FilePicker::list(
        &paths,
        crate::FilePickerQuery {
            filter: Some("ETA.TX".to_owned()),
            ..crate::FilePickerQuery::default()
        },
    )
    .expect("substring filter");
    assert_eq!(entry_names(&sub_listing), vec!["Beta.txt"]);

    let glob_listing = crate::FilePicker::list(
        &paths,
        crate::FilePickerQuery {
            include_folders: false,
            filter: Some("?eta*".to_owned()),
            ..crate::FilePickerQuery::default()
        },
    )
    .expect("question-mark glob");
    assert_eq!(entry_names(&glob_listing), vec!["Beta.txt"]);

    // Overlong filter (> 256 chars) is rejected with InvalidSetting.
    let err_filter = crate::FilePicker::list(
        &paths,
        crate::FilePickerQuery {
            filter: Some("a".repeat(257)),
            ..crate::FilePickerQuery::default()
        },
    )
    .expect_err("overlong filter must fail");
    assert!(matches!(err_filter, CoreError::InvalidSetting(_)));

    // Querying a regular file instead of a directory fails cleanly.
    let err_file = crate::FilePicker::list(
        &paths,
        crate::FilePickerQuery {
            path: Some(home.join("Beta.txt").display().to_string()),
            ..crate::FilePickerQuery::default()
        },
    )
    .expect_err("non-directory target must fail");
    assert!(matches!(err_file, CoreError::Io { .. }));

    // Serde defaults and Decky wire fields roundtrip cleanly.
    let parsed_query: crate::FilePickerQuery =
        serde_json::from_str("{}").expect("default FilePickerQuery from JSON");
    assert_eq!(parsed_query, crate::FilePickerQuery::default());

    let json_listing = serde_json::to_value(&listing).expect("serialize FilePickerListing");
    assert!(json_listing.get("path").is_some());
    assert!(json_listing.get("realpath").is_some());
    assert!(json_listing.get("truncated").is_some());
    let first_file = &json_listing["files"][0];
    for key in ["name", "path", "realpath", "isdir", "size", "modified"] {
        assert!(first_file.get(key).is_some(), "missing key `{key}`");
    }
}
