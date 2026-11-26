//! Archive staging, extraction, limits, and cache-reuse fault tests.

#![cfg(unix)]

use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use flate2::Compression;
use flate2::write::GzEncoder;
use semver::Version;
use serde_json::json;
use sha2::{Digest, Sha256};
use tar::{Builder, EntryType, Header};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use url::Url;
use zerodeck_core::{CheckedHttpClient, ExtractLimits, OutboundHttpPolicy};
use zerodeck_updater::{
    AuthorizedRelease, Channel, GenerationId, GenerationStore, StageFault, StageLimits,
    UpdateError, stage_release_with,
};

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

struct TestDir(PathBuf);

impl TestDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "zerodeck-stage-{label}-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("create temp test dir");
        Self(path)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn generation_id(fill: char) -> GenerationId {
    GenerationId::new(fill.to_string().repeat(64)).expect("valid generation id")
}

fn generation_entries(generations: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(generations)
        .expect("read generations")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn setup_store(label: &str) -> (TestDir, GenerationStore, GenerationId) {
    let dir = TestDir::new(label);
    let store = GenerationStore::new(&dir.0).expect("open generation store");
    let active = generation_id('a');
    fs::create_dir(store.generation_dir(&active)).expect("create active generation");
    store.switch_to(&active).expect("activate generation");
    (dir, store, active)
}

fn manifest(version: &str, build_id: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "schema": 1,
        "build_id": build_id,
        "version": version,
    }))
    .expect("serialize release manifest")
}

enum ArchiveEntry<'a> {
    File {
        path: &'a str,
        data: &'a [u8],
        mode: u32,
    },
    Directory {
        path: &'a str,
    },
    Symlink {
        path: &'a str,
        target: &'a str,
    },
    Hardlink {
        path: &'a str,
        target: &'a str,
    },
    RawFile {
        raw_path: &'a [u8],
        data: &'a [u8],
    },
}

fn build_archive(entries: &[ArchiveEntry<'_>]) -> Vec<u8> {
    let encoder = GzEncoder::new(Vec::new(), Compression::default());
    let mut builder = Builder::new(encoder);
    for entry in entries {
        match entry {
            ArchiveEntry::File { path, data, mode } => {
                let mut header = Header::new_gnu();
                header.set_size(u64::try_from(data.len()).expect("file size"));
                header.set_mode(*mode);
                header.set_entry_type(EntryType::Regular);
                builder
                    .append_data(&mut header, path, *data)
                    .expect("append file");
            }
            ArchiveEntry::Directory { path } => {
                let mut header = Header::new_gnu();
                header.set_size(0);
                header.set_mode(0o755);
                header.set_entry_type(EntryType::Directory);
                builder
                    .append_data(&mut header, path, io::empty())
                    .expect("append directory");
            }
            ArchiveEntry::Symlink { path, target } => {
                let mut header = Header::new_gnu();
                header.set_entry_type(EntryType::Symlink);
                header.set_size(0);
                header.set_mode(0o777);
                builder
                    .append_link(&mut header, path, target)
                    .expect("append symlink");
            }
            ArchiveEntry::Hardlink { path, target } => {
                let mut header = Header::new_gnu();
                header.set_entry_type(EntryType::Link);
                header.set_size(0);
                header.set_mode(0o644);
                builder
                    .append_link(&mut header, path, target)
                    .expect("append hardlink");
            }
            ArchiveEntry::RawFile { raw_path, data } => {
                let mut header = Header::new_gnu();
                header.set_size(u64::try_from(data.len()).expect("file size"));
                header.set_mode(0o644);
                header.set_entry_type(EntryType::Regular);
                header.as_old_mut().name[..raw_path.len()].copy_from_slice(raw_path);
                header.set_cksum();
                builder.append(&header, *data).expect("append raw file");
            }
        }
    }
    builder.finish().expect("finish archive");
    builder
        .into_inner()
        .expect("finish gzip stream")
        .finish()
        .expect("finish gzip encoder")
}

fn release_for(bytes: &[u8], url: Url) -> AuthorizedRelease {
    AuthorizedRelease {
        channel: Channel::Stable,
        version: Version::parse("0.1.2").expect("parse release version"),
        digest: GenerationId::new(hex::encode(Sha256::digest(bytes))).expect("archive digest"),
        size: u64::try_from(bytes.len()).expect("archive size"),
        build_id: "build-42".to_owned(),
        min_updater: Version::parse("0.1.0").expect("parse minimum updater"),
        target: "linux-x86_64/zerodeck-0.1.2.tar.gz".to_owned(),
        url: Some(url),
    }
}

struct MockServer {
    port: u16,
    hits: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}

impl MockServer {
    async fn start(body: Vec<u8>, truncate: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind archive server");
        let port = listener.local_addr().expect("server address").port();
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&hits);
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                counter.fetch_add(1, Ordering::SeqCst);
                let mut request = [0u8; 2048];
                let _ = socket.read(&mut request).await;
                let shared = if truncate {
                    &body[..body.len() / 2]
                } else {
                    &body[..]
                };
                let mut response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .into_bytes();
                response.extend_from_slice(shared);
                let _ = socket.write_all(&response).await;
                let _ = socket.shutdown().await;
            }
        });
        Self { port, hits, task }
    }

    fn url(&self) -> Url {
        Url::parse(&format!(
            "http://127.0.0.1:{}/linux-x86_64/zerodeck-0.1.2.tar.gz",
            self.port
        ))
        .expect("archive url")
    }

    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn client_for(server: &MockServer) -> CheckedHttpClient {
    OutboundHttpPolicy::for_loopback_ports_for_testing(vec![server.port])
        .with_timeouts(Some(Duration::from_secs(5)), Some(Duration::from_secs(30)))
        .build_checked_client()
        .expect("build checked HTTP client")
}

struct NoFault;

impl StageFault for NoFault {}

struct EnospcFault;

impl StageFault for EnospcFault {
    fn before_write(&self, _path: &Path, _bytes: usize) -> io::Result<()> {
        Err(io::Error::from_raw_os_error(28))
    }
}

fn assert_only_active(store: &GenerationStore, active: &GenerationId) {
    let generations = store
        .generation_dir(active)
        .parent()
        .expect("generations directory")
        .to_path_buf();
    assert_eq!(
        generation_entries(&generations),
        vec![active.to_string()],
        "failed staging must not create or promote a generation"
    );
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn stage_keeps_current_on_truncation_limit_or_bad_digest() {
    let manifest_bytes = manifest("0.1.2", "build-42");
    let valid_entries = [
        ArchiveEntry::Directory { path: "bin" },
        ArchiveEntry::File {
            path: "release.json",
            data: &manifest_bytes,
            mode: 0o644,
        },
        ArchiveEntry::File {
            path: "bin/zerodeckd",
            data: b"#!/bin/sh\nexit 0\n",
            mode: 0o755,
        },
        ArchiveEntry::File {
            path: "python/host.py",
            data: b"import sys\n",
            mode: 0o644,
        },
        ArchiveEntry::File {
            path: "frontend/index.js",
            data: b"console.log('zerodeck');\n",
            mode: 0o644,
        },
    ];
    let valid_archive = build_archive(&valid_entries);

    let (valid_dir, valid_store, active) = setup_store("valid");
    let valid_server = MockServer::start(valid_archive.clone(), false).await;
    let valid_client = client_for(&valid_server);
    let release = release_for(&valid_archive, valid_server.url());
    let digest = stage_release_with(
        &release,
        &valid_store,
        &valid_client,
        StageLimits::default(),
        &NoFault,
    )
    .await
    .expect("stage a valid release");
    assert_eq!(digest, release.digest);
    assert_eq!(
        valid_store.current().expect("read active generation"),
        Some(active.clone()),
        "staging must not change the active reference"
    );
    let generation = valid_store.generation_dir(&digest);
    assert!(generation.join("release.json").is_file());
    assert!(generation.join("bin/zerodeckd").is_file());
    assert!(generation.join("python/host.py").is_file());
    assert!(generation.join("frontend/index.js").is_file());
    assert_eq!(
        fs::metadata(generation.join("bin/zerodeckd"))
            .expect("stat executable")
            .mode()
            & 0o777,
        0o755
    );

    let marker = generation.join("cache-hit-marker");
    fs::write(&marker, b"keep").expect("write cache marker");
    let metadata = fs::metadata(&generation).expect("generation metadata");
    let hits_before = valid_server.hits();
    let reused = stage_release_with(
        &release,
        &valid_store,
        &valid_client,
        StageLimits::default(),
        &NoFault,
    )
    .await
    .expect("reuse a complete cached generation");
    assert_eq!(reused, digest);
    assert_eq!(
        valid_server.hits(),
        hits_before,
        "a complete cache hit must not download again"
    );
    let after = fs::metadata(&generation).expect("generation metadata after cache hit");
    assert_eq!(
        after.ino(),
        metadata.ino(),
        "cache hit must not rewrite the generation"
    );
    assert_eq!(
        fs::read(&marker).expect("read cache marker"),
        b"keep",
        "cache hit must not replace generation contents"
    );
    assert!(
        !valid_dir.0.join("release.json").exists(),
        "staging must never write outside the generation root"
    );

    let (bad_digest_dir, bad_digest_store, bad_digest_active) = setup_store("bad-digest");
    let bad_digest_server = MockServer::start(valid_archive.clone(), false).await;
    let bad_digest_client = client_for(&bad_digest_server);
    let mut bad_digest = release_for(&valid_archive, bad_digest_server.url());
    bad_digest.digest = generation_id('0');
    let err = stage_release_with(
        &bad_digest,
        &bad_digest_store,
        &bad_digest_client,
        StageLimits::default(),
        &NoFault,
    )
    .await
    .expect_err("a bad archive digest must fail before extraction");
    assert!(
        matches!(err, UpdateError::DigestMismatch { .. }),
        "unexpected bad-digest error: {err:?}"
    );
    assert_eq!(
        bad_digest_store.current().expect("read active generation"),
        Some(bad_digest_active.clone())
    );
    assert_only_active(&bad_digest_store, &bad_digest_active);
    assert!(!bad_digest_dir.0.join("tree").exists());

    let (truncated_dir, truncated_store, truncated_active) = setup_store("truncated");
    let truncated_server = MockServer::start(valid_archive.clone(), true).await;
    let truncated_client = client_for(&truncated_server);
    let truncated = release_for(&valid_archive, truncated_server.url());
    let err = stage_release_with(
        &truncated,
        &truncated_store,
        &truncated_client,
        StageLimits::default(),
        &NoFault,
    )
    .await
    .expect_err("a truncated download must fail");
    assert!(
        matches!(
            err,
            UpdateError::Download { .. } | UpdateError::SizeMismatch { .. }
        ),
        "unexpected truncation error: {err:?}"
    );
    assert_eq!(
        truncated_store.current().expect("read active generation"),
        Some(truncated_active.clone())
    );
    assert_only_active(&truncated_store, &truncated_active);
    assert!(!truncated_dir.0.join("tree").exists());

    let escape_archive = build_archive(&[
        ArchiveEntry::File {
            path: "release.json",
            data: &manifest_bytes,
            mode: 0o644,
        },
        ArchiveEntry::RawFile {
            raw_path: b"../zerodeck-escape.txt",
            data: b"escape",
        },
    ]);
    let (escape_dir, escape_store, escape_active) = setup_store("escape");
    let escape_server = MockServer::start(escape_archive.clone(), false).await;
    let escape_client = client_for(&escape_server);
    let escape = release_for(&escape_archive, escape_server.url());
    let err = stage_release_with(
        &escape,
        &escape_store,
        &escape_client,
        StageLimits::default(),
        &NoFault,
    )
    .await
    .expect_err("a parent-traversal archive entry must be rejected");
    assert!(
        matches!(err, UpdateError::UnsafeArchiveEntry { .. }),
        "unexpected path error: {err:?}"
    );
    assert_eq!(
        escape_store.current().expect("read active generation"),
        Some(escape_active.clone())
    );
    assert_only_active(&escape_store, &escape_active);
    assert!(!escape_dir.0.join("zerodeck-escape.txt").exists());
    assert!(
        !escape_dir
            .0
            .join("generations/zerodeck-escape.txt")
            .exists()
    );

    let link_archive = build_archive(&[
        ArchiveEntry::File {
            path: "release.json",
            data: &manifest_bytes,
            mode: 0o644,
        },
        ArchiveEntry::Symlink {
            path: "passwd",
            target: "/etc/passwd",
        },
    ]);
    let (_link_dir, link_store, link_active) = setup_store("link");
    let link_server = MockServer::start(link_archive.clone(), false).await;
    let link_client = client_for(&link_server);
    let link = release_for(&link_archive, link_server.url());
    let err = stage_release_with(
        &link,
        &link_store,
        &link_client,
        StageLimits::default(),
        &NoFault,
    )
    .await
    .expect_err("a symlink entry must be rejected");
    assert!(
        matches!(err, UpdateError::UnsafeArchiveEntry { .. }),
        "unexpected link error: {err:?}"
    );
    assert_only_active(&link_store, &link_active);

    let hardlink_archive = build_archive(&[
        ArchiveEntry::File {
            path: "release.json",
            data: &manifest_bytes,
            mode: 0o644,
        },
        ArchiveEntry::Hardlink {
            path: "hardlink",
            target: "release.json",
        },
    ]);
    let (_hardlink_dir, hardlink_store, hardlink_active) = setup_store("hardlink");
    let hardlink_server = MockServer::start(hardlink_archive.clone(), false).await;
    let hardlink_client = client_for(&hardlink_server);
    let hardlink = release_for(&hardlink_archive, hardlink_server.url());
    let err = stage_release_with(
        &hardlink,
        &hardlink_store,
        &hardlink_client,
        StageLimits::default(),
        &NoFault,
    )
    .await
    .expect_err("a hardlink entry must be rejected");
    assert!(
        matches!(err, UpdateError::UnsafeArchiveEntry { .. }),
        "unexpected hardlink error: {err:?}"
    );
    assert_only_active(&hardlink_store, &hardlink_active);

    let duplicate_archive = build_archive(&[
        ArchiveEntry::File {
            path: "release.json",
            data: &manifest_bytes,
            mode: 0o644,
        },
        ArchiveEntry::File {
            path: "bin/zerodeckd",
            data: b"first",
            mode: 0o755,
        },
        ArchiveEntry::File {
            path: "bin/zerodeckd",
            data: b"second",
            mode: 0o755,
        },
    ]);
    let (_duplicate_dir, duplicate_store, duplicate_active) = setup_store("duplicate");
    let duplicate_server = MockServer::start(duplicate_archive.clone(), false).await;
    let duplicate_client = client_for(&duplicate_server);
    let duplicate = release_for(&duplicate_archive, duplicate_server.url());
    let err = stage_release_with(
        &duplicate,
        &duplicate_store,
        &duplicate_client,
        StageLimits::default(),
        &NoFault,
    )
    .await
    .expect_err("a duplicate archive entry must be rejected");
    assert!(
        matches!(err, UpdateError::DuplicateArchiveEntry { .. }),
        "unexpected duplicate error: {err:?}"
    );
    assert_only_active(&duplicate_store, &duplicate_active);

    let large_data = vec![b'x'; 4096];
    let limit_archive = build_archive(&[
        ArchiveEntry::File {
            path: "release.json",
            data: &manifest_bytes,
            mode: 0o644,
        },
        ArchiveEntry::File {
            path: "large.bin",
            data: &large_data,
            mode: 0o644,
        },
    ]);
    let (_limit_dir, limit_store, limit_active) = setup_store("limit");
    let limit_server = MockServer::start(limit_archive.clone(), false).await;
    let limit_client = client_for(&limit_server);
    let limit = release_for(&limit_archive, limit_server.url());
    let limits = StageLimits {
        max_download_bytes: 1024 * 1024,
        extract: ExtractLimits {
            max_total_bytes: 1024,
            max_entries: 32,
        },
    };
    let err = stage_release_with(&limit, &limit_store, &limit_client, limits, &NoFault)
        .await
        .expect_err("an over-limit archive must be rejected");
    assert!(
        matches!(err, UpdateError::ArchiveExpandedTooLarge { .. }),
        "unexpected limit error: {err:?}"
    );
    assert_only_active(&limit_store, &limit_active);

    let (_entry_limit_dir, entry_limit_store, entry_limit_active) = setup_store("entry-limit");
    let entry_limit_server = MockServer::start(valid_archive.clone(), false).await;
    let entry_limit_client = client_for(&entry_limit_server);
    let entry_limit = release_for(&valid_archive, entry_limit_server.url());
    let limits = StageLimits {
        max_download_bytes: 1024 * 1024,
        extract: ExtractLimits {
            max_total_bytes: 1024 * 1024,
            max_entries: 1,
        },
    };
    let err = stage_release_with(
        &entry_limit,
        &entry_limit_store,
        &entry_limit_client,
        limits,
        &NoFault,
    )
    .await
    .expect_err("an entry-count limit must be enforced");
    assert!(
        matches!(err, UpdateError::TooManyArchiveEntries { limit: 1 }),
        "unexpected entry-limit error: {err:?}"
    );
    assert_only_active(&entry_limit_store, &entry_limit_active);

    let (_enospc_dir, enospc_store, enospc_active) = setup_store("enospc");
    let enospc_server = MockServer::start(valid_archive.clone(), false).await;
    let enospc_client = client_for(&enospc_server);
    let enospc = release_for(&valid_archive, enospc_server.url());
    let err = stage_release_with(
        &enospc,
        &enospc_store,
        &enospc_client,
        StageLimits::default(),
        &EnospcFault,
    )
    .await
    .expect_err("a write failure must abort staging");
    assert!(
        matches!(&err, UpdateError::Io { source, .. } if source.raw_os_error() == Some(28)),
        "unexpected ENOSPC error: {err:?}"
    );
    assert_eq!(
        enospc_store.current().expect("read active generation"),
        Some(enospc_active.clone())
    );
    assert_only_active(&enospc_store, &enospc_active);

    let wrong_manifest_archive = build_archive(&[ArchiveEntry::File {
        path: "release.json",
        data: &manifest("0.1.2", "different-build"),
        mode: 0o644,
    }]);
    let (_manifest_dir, manifest_store, manifest_active) = setup_store("manifest");
    let manifest_server = MockServer::start(wrong_manifest_archive.clone(), false).await;
    let manifest_client = client_for(&manifest_server);
    let wrong_manifest = release_for(&wrong_manifest_archive, manifest_server.url());
    let err = stage_release_with(
        &wrong_manifest,
        &manifest_store,
        &manifest_client,
        StageLimits::default(),
        &NoFault,
    )
    .await
    .expect_err("a mismatched release manifest must be rejected");
    assert!(
        matches!(err, UpdateError::ReleaseIdentityMismatch { .. }),
        "unexpected manifest error: {err:?}"
    );
    assert_only_active(&manifest_store, &manifest_active);

    let corrupt_archive = b"this is not a tar.gz archive".to_vec();
    let (_corrupt_dir, corrupt_store, corrupt_active) = setup_store("corrupt");
    let corrupt_server = MockServer::start(corrupt_archive.clone(), false).await;
    let corrupt_client = client_for(&corrupt_server);
    let corrupt = release_for(&corrupt_archive, corrupt_server.url());
    let err = stage_release_with(
        &corrupt,
        &corrupt_store,
        &corrupt_client,
        StageLimits::default(),
        &NoFault,
    )
    .await
    .expect_err("a corrupt archive must fail after integrity verification");
    assert!(
        matches!(err, UpdateError::InvalidArchive { .. }),
        "unexpected corrupt-archive error: {err:?}"
    );
    assert_only_active(&corrupt_store, &corrupt_active);
}
