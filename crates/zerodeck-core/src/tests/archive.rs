//! ZIP extraction safety and SHA-256 policy.

use super::*;

#[test]
fn extract_zip_safely_rejects_zip_slip_traversal() {
    let dir = TestDir::new("zip-slip");
    let dest = dir.0.join("out");
    fs::create_dir_all(&dest).expect("create out dir");

    let mut buf = Cursor::new(Vec::new());
    {
        let mut writer = ZipWriter::new(&mut buf);
        writer
            .start_file("../../evil.txt", SimpleFileOptions::default())
            .expect("start evil entry");
        writer.write_all(b"pwned").expect("write evil entry");
        writer.finish().expect("finish zip");
    }

    let err = extract_zip_safely(buf.get_ref(), &dest).expect_err("must reject Zip-Slip");
    assert!(matches!(err, CoreError::UnsafeArchiveEntry { .. }));
    assert_eq!(fs::read_dir(&dest).expect("read out dir").count(), 0);
}

#[test]
fn verify_sha256_policy_enforces_remote_and_local_rules() {
    let payload = b"zerodeck-plugin-bytes";
    let expected_hex = hex::encode(Sha256::digest(payload));

    let err_missing_remote =
        verify_sha256_policy(payload, &InstallSource::Remote { sha256: None }, false)
            .expect_err("remote without hash must fail");
    assert!(matches!(err_missing_remote, CoreError::MissingSha256));

    let ok_remote = verify_sha256_policy(
        payload,
        &InstallSource::Remote {
            sha256: Some(expected_hex.clone()),
        },
        false,
    )
    .expect("matching remote hash must succeed");
    assert_eq!(ok_remote, expected_hex);

    let err_local_unconfirmed = verify_sha256_policy(
        payload,
        &InstallSource::LocalFile {
            sha256: None,
            user_confirmed: false,
        },
        false,
    )
    .expect_err("unconfirmed local sideload without hash must fail");
    assert!(matches!(
        err_local_unconfirmed,
        CoreError::ConfirmationRequired
    ));

    let ok_local_confirmed = verify_sha256_policy(
        payload,
        &InstallSource::LocalFile {
            sha256: None,
            user_confirmed: true,
        },
        false,
    )
    .expect("confirmed local sideload succeeds");
    assert_eq!(ok_local_confirmed, expected_hex);

    let whitespace_remote = verify_sha256_policy(
        payload,
        &InstallSource::Remote {
            sha256: Some("   ".to_owned()),
        },
        false,
    )
    .expect_err("whitespace-only hash must be treated as missing");
    assert!(matches!(whitespace_remote, CoreError::MissingSha256));

    let mismatch = verify_sha256_policy(
        payload,
        &InstallSource::Remote {
            sha256: Some("0".repeat(64)),
        },
        false,
    )
    .expect_err("wrong sha256 must fail");
    assert!(matches!(mismatch, CoreError::Sha256Mismatch { .. }));

    let allowed_override =
        verify_sha256_policy(payload, &InstallSource::Remote { sha256: None }, true)
            .expect("allow_unverified overrides missing hash");
    assert_eq!(allowed_override, expected_hex);
}

#[test]
fn extract_zip_safely_rejects_symlinks_and_oversized_entries() {
    let dir = TestDir::new("zip-hardening");
    let dest = dir.0.join("out");
    fs::create_dir_all(&dest).expect("create out dir");

    let mut sym_buf = Cursor::new(Vec::new());
    {
        let mut writer = ZipWriter::new(&mut sym_buf);
        writer
            .add_symlink("link", "/etc/passwd", SimpleFileOptions::default())
            .expect("add symlink");
        writer.finish().expect("finish symlink zip");
    }
    let err_sym =
        extract_zip_safely(sym_buf.get_ref(), &dest).expect_err("must reject zip symlink");
    assert!(matches!(err_sym, CoreError::UnsafeArchiveEntry { .. }));

    let mut big_buf = Cursor::new(Vec::new());
    {
        let mut writer = ZipWriter::new(&mut big_buf);
        writer
            .start_file("huge.bin", SimpleFileOptions::default())
            .expect("start huge entry");
        writer
            .write_all(&vec![b'A'; 1024])
            .expect("write huge bytes");
        writer.finish().expect("finish huge zip");
    }
    let err_limit = crate::extract_zip_safely_with_limit(big_buf.get_ref(), &dest, 512)
        .expect_err("must reject entry exceeding byte limit");
    assert!(matches!(err_limit, CoreError::ArchiveSizeExceeded { .. }));
}

#[test]
fn extract_zip_failure_mid_write_leaves_existing_install_untouched() {
    let dir = TestDir::new("zip-staging-fail");
    let dest = dir.0.join("plugin");
    fs::create_dir_all(&dest).expect("create dest");
    fs::write(dest.join("old.txt"), b"previous install").expect("seed old install");

    // `b.txt` is stored uncompressed, so flipping one payload byte breaks its CRC. Pre-validation
    // passes and the failure only surfaces while `b.txt` is being written, after `a.txt`.
    let mut bytes = zip_with_files(&[("a.txt", b"fine"), ("b.txt", b"CORRUPTME")]);
    let pos = bytes
        .windows(9)
        .position(|w| w == b"CORRUPTME")
        .expect("find stored payload");
    bytes[pos] ^= 0xFF;

    extract_zip_safely(&bytes, &dest).expect_err("corrupt entry must fail extraction");

    assert_eq!(sorted_names(&dest), vec!["old.txt".to_owned()]);
    assert_eq!(
        fs::read(dest.join("old.txt")).expect("read old"),
        b"previous install"
    );
    assert_eq!(sorted_names(&dir.0), vec!["plugin".to_owned()]);
}

#[test]
fn extract_zip_success_replaces_destination_without_leftovers() {
    let dir = TestDir::new("zip-staging-ok");
    let dest = dir.0.join("plugin");
    fs::create_dir_all(&dest).expect("create dest");
    fs::write(dest.join("old.txt"), b"previous install").expect("seed old install");

    let bytes = zip_with_files(&[("main.py", b"class Plugin: pass"), ("dist/index.js", b"x")]);
    let extracted = extract_zip_safely(&bytes, &dest).expect("extract");

    assert_eq!(
        sorted_names(&dest),
        vec!["dist".to_owned(), "main.py".to_owned()]
    );
    assert!(
        extracted
            .iter()
            .all(|p| p.starts_with(fs::canonicalize(&dest).expect("canon")))
    );
    assert!(extracted.iter().all(|p| p.is_file()));
    assert_eq!(sorted_names(&dir.0), vec!["plugin".to_owned()]);
}

#[test]
fn extract_zip_rejects_archives_with_too_many_entries() {
    let dir = TestDir::new("zip-entries");
    let dest = dir.0.join("plugin");
    let bytes = zip_with_files(&[("1", b""), ("2", b""), ("3", b""), ("4", b"")]);

    let limits = crate::ExtractLimits {
        max_entries: 3,
        ..crate::ExtractLimits::default()
    };
    let err = crate::extract_zip_safely_with_limits(&bytes, &dest, &limits)
        .expect_err("must reject entry count above limit");
    assert!(matches!(
        err,
        CoreError::ArchiveTooManyEntries {
            count: 4,
            max_entries: 3
        }
    ));
    assert!(!dest.exists());
}

#[cfg(unix)]
#[test]
fn extract_zip_normalizes_permissions_without_unix_mode() {
    use std::os::unix::fs::PermissionsExt;

    if std::env::var_os("ZERODECK_TEST_UMASK_077").is_none() {
        let exe = std::env::current_exe().expect("current test binary");
        let status = std::process::Command::new("sh")
            .arg("-c")
            .arg("umask 077 && exec \"$1\" --exact \"$2\"")
            .arg("sh")
            .arg(&exe)
            .arg("tests::extract_zip_normalizes_permissions_without_unix_mode")
            .env("ZERODECK_TEST_UMASK_077", "1")
            .status()
            .expect("spawn umask 077 subtest");
        assert!(status.success(), "umask 077 subtest must succeed");
        return;
    }

    let dir = TestDir::new("zip-perms-umask077");
    let dest = dir.0.join("plugin");

    let mut buf = Cursor::new(Vec::new());
    {
        let mut writer = ZipWriter::new(&mut buf);
        let stripped = SimpleFileOptions::default().unix_permissions(0);
        let dos_no_mode = SimpleFileOptions::default().external_attributes(0);
        let executable = SimpleFileOptions::default().unix_permissions(0o755);

        writer
            .add_directory("explicit_dir", stripped)
            .expect("add explicit dir");
        writer
            .start_file("sub/file.txt", stripped)
            .expect("start stripped file");
        writer.write_all(b"hello").expect("write stripped file");
        writer
            .start_file("sub/dos.txt", dos_no_mode)
            .expect("start dos file");
        writer.write_all(b"dos").expect("write dos file");
        writer
            .start_file("sub/bin/run", executable)
            .expect("start exec file");
        writer.write_all(b"#!/bin/sh\n").expect("write exec file");
        writer.finish().expect("finish zip");
    }

    extract_zip_safely(buf.get_ref(), &dest).expect("extract zip under umask 077");

    let mode_of = |rel: &str| -> u32 {
        let p = if rel.is_empty() {
            dest.clone()
        } else {
            dest.join(rel)
        };
        fs::metadata(&p)
            .unwrap_or_else(|e| panic!("stat {}: {e}", p.display()))
            .permissions()
            .mode()
            & 0o777
    };

    assert_eq!(mode_of(""), 0o755, "destination root must be 0o755");
    assert_eq!(mode_of("explicit_dir"), 0o755, "explicit dir must be 0o755");
    assert_eq!(mode_of("sub"), 0o755, "implicit parent dir must be 0o755");
    assert_eq!(mode_of("sub/bin"), 0o755, "nested parent dir must be 0o755");
    assert_eq!(
        mode_of("sub/file.txt"),
        0o644,
        "stripped file must be normalized to 0o644"
    );
    assert_eq!(
        mode_of("sub/dos.txt"),
        0o644,
        "DOS entry without unix_mode must be normalized to 0o644"
    );
    assert_eq!(
        mode_of("sub/bin/run"),
        0o755,
        "executable file must remain 0o755"
    );
}
