//! Behavioural tests for the S3-backed `NFSFileSystem`, driven through the trait methods
//! the NFSv3 server actually calls.

use std::sync::Arc;
use std::time::Duration;

use mount_sso::vfs::{Config, S3Fs};
use nfsserve::nfs::{filename3, ftype3, nfsstat3, nfstime3, sattr3, set_mtime, set_mode3, set_size3};
use nfsserve::vfs::{NFSFileSystem, VFSCapabilities};
use object_store::memory::InMemory;
use object_store::path::Path as ObjPath;
use object_store::{ObjectStore, ObjectStoreExt, PutPayload};

fn err_code(err: nfsstat3) -> u32 {
    err as u32
}

fn name(value: &str) -> filename3 {
    filename3::from(value.as_bytes())
}

fn config(read_only: bool, root_prefix: &str, staging: std::path::PathBuf) -> Config {
    Config {
        root_prefix: root_prefix.to_string(),
        uid: 501,
        gid: 20,
        read_only,
        // Zero means "always re-list", which keeps the assertions deterministic.
        dir_ttl: Duration::ZERO,
        flush_delay: Duration::from_millis(20),
        staging_dir: staging.join("staging"),
        max_stage_bytes: 64 * 1024 * 1024,
        cache_dir: staging.join("read"),
        read_cache_bytes: 0,
        read_cache_ttl: Duration::ZERO,
        prefetch_bytes: 0,
        filter_macos_junk: true,
    }
}

struct Fixture {
    fs: S3Fs,
    store: Arc<dyn ObjectStore>,
    _dir: tempfile::TempDir,
}

async fn fixture_with(read_only: bool, root_prefix: &str) -> Fixture {
    let dir = tempfile::tempdir().expect("temp dir");
    let staging = std::fs::canonicalize(dir.path()).unwrap().join("staging");
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let fs = S3Fs::new(store.clone(), config(read_only, root_prefix, staging))
        .await
        .expect("filesystem");
    Fixture {
        fs,
        store,
        _dir: dir,
    }
}

async fn fixture(read_only: bool) -> Fixture {
    fixture_with(read_only, "").await
}

async fn read_names(fs: &S3Fs, dir: u64) -> Vec<String> {
    let listing = fs.readdir(dir, 0, 1024).await.expect("readdir");
    listing
        .entries
        .iter()
        .map(|entry| String::from_utf8(entry.name.0.clone()).unwrap())
        .collect()
}

#[tokio::test]
async fn root_is_a_directory_and_reports_capabilities() {
    let f = fixture(false).await;
    let root = f.fs.root_dir();
    let attr = f.fs.getattr(root).await.expect("root attr");
    assert!(matches!(attr.ftype, ftype3::NF3DIR));
    assert_eq!(attr.mode, 0o755);
    assert_eq!(attr.uid, 501);
    assert_eq!(attr.gid, 20);
    assert_eq!(root, 1);
    assert!(matches!(f.fs.capabilities(), VFSCapabilities::ReadWrite));
    assert_eq!(f.fs.lookup(root, &name(".")).await.unwrap(), root);
    assert_eq!(f.fs.lookup(root, &name("..")).await.unwrap(), root);

    let ro = fixture(true).await;
    assert!(matches!(ro.fs.capabilities(), VFSCapabilities::ReadOnly));
}

#[tokio::test]
async fn create_write_read_roundtrip_through_staging() {
    let f = fixture(false).await;
    let root = f.fs.root_dir();

    let (ino, attr) = f
        .fs
        .create(root, &name("hello.txt"), sattr3::default())
        .await
        .expect("create");
    assert!(matches!(attr.ftype, ftype3::NF3REG));
    assert_eq!(attr.size, 0);

    let written = f.fs.write(ino, 0, b"hello ").await.expect("write");
    assert_eq!(written.size, 6);
    let written = f.fs.write(ino, 6, b"world").await.expect("write");
    assert_eq!(written.size, 11);
    assert_eq!(f.fs.getattr(ino).await.unwrap().size, 11);

    let (data, eof) = f.fs.read(ino, 0, 4096).await.expect("read");
    assert_eq!(data, b"hello world");
    assert!(eof);

    let (partial, eof) = f.fs.read(ino, 6, 5).await.expect("read partial");
    assert_eq!(partial, b"world");
    assert!(eof);

    let (tail, eof) = f.fs.read(ino, 11, 10).await.expect("read past eof");
    assert!(tail.is_empty());
    assert!(eof);

    f.fs.shutdown().await;
    let stored = f.store.get(&ObjPath::from("hello.txt")).await.expect("object");
    assert_eq!(stored.bytes().await.unwrap().as_ref(), b"hello world");
}

#[tokio::test]
async fn create_on_existing_name_keeps_the_content() {
    let f = fixture(false).await;
    let root = f.fs.root_dir();
    f.store
        .put(&ObjPath::from("keep.txt"), PutPayload::from_static(b"payload"))
        .await
        .unwrap();

    let (ino, attr) = f
        .fs
        .create(root, &name("keep.txt"), sattr3::default())
        .await
        .expect("create");
    assert_eq!(attr.size, 7);
    let (data, _) = f.fs.read(ino, 0, 100).await.unwrap();
    assert_eq!(data, b"payload");
}

#[tokio::test]
async fn create_exclusive_rejects_existing_names() {
    let f = fixture(false).await;
    let root = f.fs.root_dir();
    let first = f.fs.create_exclusive(root, &name("once.txt")).await.expect("first");
    assert!(first > 1);
    assert_eq!(
        err_code(f.fs.create_exclusive(root, &name("once.txt")).await.unwrap_err()),
        nfsstat3::NFS3ERR_EXIST as u32
    );
}

#[tokio::test]
async fn readdir_is_sorted_and_paginates() {
    let f = fixture(false).await;
    let root = f.fs.root_dir();
    for key in ["b.txt", "a.txt", "dir/c.txt"] {
        f.store
            .put(&ObjPath::from(key), PutPayload::from_static(b"x"))
            .await
            .unwrap();
    }

    let first = f.fs.readdir(root, 0, 2).await.expect("page 1");
    let names: Vec<String> = first
        .entries
        .iter()
        .map(|entry| String::from_utf8(entry.name.0.clone()).unwrap())
        .collect();
    assert_eq!(names, ["a.txt", "b.txt"]);
    assert!(!first.end);

    let last_id = first.entries.last().unwrap().fileid;
    let second = f.fs.readdir(root, last_id, 2).await.expect("page 2");
    let names: Vec<String> = second
        .entries
        .iter()
        .map(|entry| String::from_utf8(entry.name.0.clone()).unwrap())
        .collect();
    assert_eq!(names, ["dir"]);
    assert!(second.end);
    assert!(matches!(second.entries[0].attr.ftype, ftype3::NF3DIR));

    let dir_ino = second.entries[0].fileid;
    assert_eq!(f.fs.lookup(root, &name("dir")).await.unwrap(), dir_ino);
    assert_eq!(read_names(&f.fs, dir_ino).await, ["c.txt"]);
}

#[tokio::test]
async fn mkdir_creates_a_session_directory_and_remove_enforces_notempty() {
    let f = fixture(false).await;
    let root = f.fs.root_dir();

    let (dir_ino, attr) = f.fs.mkdir(root, &name("newdir")).await.expect("mkdir");
    assert!(matches!(attr.ftype, ftype3::NF3DIR));
    assert_eq!(
        err_code(f.fs.mkdir(root, &name("newdir")).await.unwrap_err()),
        nfsstat3::NFS3ERR_EXIST as u32
    );
    assert_eq!(read_names(&f.fs, root).await, ["newdir"]);

    f.fs.create(dir_ino, &name("inner.txt"), sattr3::default())
        .await
        .expect("inner");
    assert_eq!(
        err_code(f.fs.remove(root, &name("newdir")).await.unwrap_err()),
        nfsstat3::NFS3ERR_NOTEMPTY as u32
    );

    f.fs.remove(dir_ino, &name("inner.txt")).await.expect("remove inner");
    f.fs.remove(root, &name("newdir")).await.expect("remove dir");
    assert_eq!(
        err_code(f.fs.lookup(root, &name("newdir")).await.unwrap_err()),
        nfsstat3::NFS3ERR_NOENT as u32
    );
    assert!(read_names(&f.fs, root).await.is_empty());
}

#[tokio::test]
async fn rename_moves_staged_content_before_copying() {
    let f = fixture(false).await;
    let root = f.fs.root_dir();
    let (ino, _) = f
        .fs
        .create(root, &name("draft.bin"), sattr3::default())
        .await
        .unwrap();
    f.fs.write(ino, 0, b"staged bytes").await.unwrap();

    f.fs.rename(root, &name("draft.bin"), root, &name("final.bin"))
        .await
        .expect("rename");

    let stored = f.store.get(&ObjPath::from("final.bin")).await.expect("object");
    assert_eq!(stored.bytes().await.unwrap().as_ref(), b"staged bytes");
    assert!(f.store.head(&ObjPath::from("draft.bin")).await.is_err());

    let (data, _) = f.fs.read(ino, 0, 64).await.expect("read after rename");
    assert_eq!(data, b"staged bytes");
    f.fs.write(ino, 0, b"more").await.unwrap();
    f.fs.shutdown().await;
    let stored = f.store.get(&ObjPath::from("final.bin")).await.expect("object");
    assert_eq!(stored.bytes().await.unwrap().as_ref(), b"moreed bytes");
}

#[tokio::test]
async fn rename_of_a_directory_rewrites_the_prefix() {
    let f = fixture(false).await;
    let root = f.fs.root_dir();
    f.store
        .put(&ObjPath::from("old/a.txt"), PutPayload::from_static(b"A"))
        .await
        .unwrap();
    f.store
        .put(&ObjPath::from("old/nested/b.txt"), PutPayload::from_static(b"B"))
        .await
        .unwrap();

    let old = f.fs.lookup(root, &name("old")).await.unwrap();
    f.fs.rename(root, &name("old"), root, &name("new"))
        .await
        .expect("rename dir");

    assert!(f.store.head(&ObjPath::from("new/a.txt")).await.is_ok());
    assert!(f.store.head(&ObjPath::from("new/nested/b.txt")).await.is_ok());
    assert!(f.store.head(&ObjPath::from("old/a.txt")).await.is_err());
    assert!(f.fs.lookup(root, &name("old")).await.is_err());
    assert_eq!(f.fs.lookup(root, &name("new")).await.unwrap(), old);
}

#[tokio::test]
async fn setattr_truncates_and_updates_times() {
    let f = fixture(false).await;
    let root = f.fs.root_dir();
    f.store
        .put(&ObjPath::from("data.bin"), PutPayload::from_static(b"0123456789"))
        .await
        .unwrap();
    let ino = f.fs.lookup(root, &name("data.bin")).await.unwrap();

    let attr = f
        .fs
        .setattr(
            ino,
            sattr3 {
                size: set_size3::size(4),
                mode: set_mode3::mode(0o600),
                mtime: set_mtime::SET_TO_CLIENT_TIME(nfstime3 {
                    seconds: 1_700_000_000,
                    nseconds: 0,
                }),
                ..Default::default()
            },
        )
        .await
        .expect("setattr");
    assert_eq!(attr.size, 4);
    assert_eq!(attr.mode, 0o600);
    assert_eq!(attr.mtime.seconds, 1_700_000_000);

    let (data, _) = f.fs.read(ino, 0, 16).await.unwrap();
    assert_eq!(data, b"0123");

    f.fs.shutdown().await;
    let stored = f.store.get(&ObjPath::from("data.bin")).await.unwrap();
    assert_eq!(stored.bytes().await.unwrap().as_ref(), b"0123");
}

#[tokio::test]
async fn read_only_mode_rejects_every_mutation() {
    let f = fixture(true).await;
    let root = f.fs.root_dir();
    f.store
        .put(&ObjPath::from("ro.txt"), PutPayload::from_static(b"x"))
        .await
        .unwrap();
    let ino = f.fs.lookup(root, &name("ro.txt")).await.unwrap();

    assert_eq!(err_code(f.fs.write(ino, 0, b"y").await.unwrap_err()), nfsstat3::NFS3ERR_ROFS as u32);
    assert_eq!(
        err_code(f.fs.create(root, &name("new.txt"), sattr3::default()).await.unwrap_err()),
        nfsstat3::NFS3ERR_ROFS as u32
    );
    assert_eq!(
        err_code(f.fs.mkdir(root, &name("d")).await.unwrap_err()),
        nfsstat3::NFS3ERR_ROFS as u32
    );
    assert_eq!(
        err_code(f.fs.remove(root, &name("ro.txt")).await.unwrap_err()),
        nfsstat3::NFS3ERR_ROFS as u32
    );
    assert_eq!(
        err_code(
            f.fs.setattr(
                ino,
                sattr3 {
                    size: set_size3::size(0),
                    ..Default::default()
                }
            )
            .await
            .unwrap_err()
        ),
        nfsstat3::NFS3ERR_ROFS as u32
    );
    let (data, _) = f.fs.read(ino, 0, 8).await.unwrap();
    assert_eq!(data, b"x");
}

#[tokio::test]
async fn root_prefix_is_honoured_and_hides_siblings() {
    let f = fixture_with(false, "users/alice").await;
    for (key, body) in [
        ("users/alice/report.txt", &b"mine"[..]),
        ("users/alice/sub/note.txt", &b"nested"[..]),
        ("users/bob/secret.txt", &b"theirs"[..]),
    ] {
        f.store
            .put(&ObjPath::from(key), PutPayload::from(body.to_vec()))
            .await
            .unwrap();
    }
    let root = f.fs.root_dir();

    let mut names = read_names(&f.fs, root).await;
    names.sort();
    assert_eq!(names, ["report.txt", "sub"]);
    assert_eq!(
        err_code(f.fs.lookup(root, &name("secret.txt")).await.unwrap_err()),
        nfsstat3::NFS3ERR_NOENT as u32
    );
    let file = f.fs.lookup(root, &name("report.txt")).await.unwrap();
    let (data, _) = f.fs.read(file, 0, 64).await.unwrap();
    assert_eq!(data, b"mine");
}

#[tokio::test]
async fn directories_and_files_report_sane_attributes() {
    let f = fixture(false).await;
    let root = f.fs.root_dir();
    f.store
        .put(&ObjPath::from("dir/file.bin"), PutPayload::from_static(b"01234"))
        .await
        .unwrap();

    let dir_ino = f.fs.lookup(root, &name("dir")).await.unwrap();
    let dir_attr = f.fs.getattr(dir_ino).await.unwrap();
    assert!(matches!(dir_attr.ftype, ftype3::NF3DIR));
    assert_eq!(dir_attr.fileid, dir_ino);

    let file_ino = f.fs.lookup(dir_ino, &name("file.bin")).await.unwrap();
    let file_attr = f.fs.getattr(file_ino).await.unwrap();
    assert!(matches!(file_attr.ftype, ftype3::NF3REG));
    assert_eq!(file_attr.size, 5);
    assert_eq!(file_attr.nlink, 1);
    assert_eq!(file_attr.fileid, file_ino);

    assert_eq!(err_code(f.fs.read(root, 0, 1).await.unwrap_err()), nfsstat3::NFS3ERR_ISDIR as u32);
    assert_eq!(
        err_code(f.fs.readdir(file_ino, 0, 1).await.unwrap_err()),
        nfsstat3::NFS3ERR_NOTDIR as u32
    );
    assert!(f.fs.getattr(61409).await.is_err());
}

#[tokio::test]
async fn symlinks_are_unsupported() {
    let f = fixture(false).await;
    let root = f.fs.root_dir();
    assert_eq!(
        err_code(
            f.fs.symlink(
                root,
                &name("link"),
                &filename3::from(b"/target".as_slice()),
                &sattr3::default()
            )
            .await
            .unwrap_err()
        ),
        nfsstat3::NFS3ERR_NOTSUPP as u32
    );
    assert_eq!(
        err_code(f.fs.readlink(root).await.unwrap_err()),
        nfsstat3::NFS3ERR_NOTSUPP as u32
    );
}

#[tokio::test]
async fn fsinfo_describes_the_export() {
    let f = fixture(false).await;
    let info = f.fs.fsinfo(f.fs.root_dir()).await.unwrap();
    assert_eq!(info.dtpref, 64 * 1024);
    assert!(info.wtmax >= info.wtpref);
    assert_ne!(info.properties & nfsserve::nfs::FSF_CANSETTIME, 0);
    assert_eq!(info.properties & nfsserve::nfs::FSF_SYMLINK, 0);
}

#[tokio::test]
async fn deleted_objects_report_noent_after_the_cache_expires() {
    let dir = tempfile::tempdir().unwrap();
    let staging = std::fs::canonicalize(dir.path()).unwrap().join("staging");
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let mut cfg = config(false, "", staging);
    cfg.dir_ttl = Duration::from_millis(1);
    let fs = S3Fs::new(store.clone(), cfg).await.unwrap();

    store
        .put(&ObjPath::from("gone.txt"), PutPayload::from_static(b"x"))
        .await
        .unwrap();
    let ino = fs.lookup(fs.root_dir(), &name("gone.txt")).await.unwrap();
    assert_eq!(fs.getattr(ino).await.unwrap().size, 1);

    store.delete(&ObjPath::from("gone.txt")).await.unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(err_code(fs.getattr(ino).await.unwrap_err()), nfsstat3::NFS3ERR_NOENT as u32);
}

#[tokio::test]
async fn unicode_and_reserved_names_round_trip() {
    let f = fixture(false).await;
    // Seed literal keys the way a real bucket stores them; `Path::from` would pre-encode
    // them and hide them from every request.
    for key in [
        "名字/子目录/文件.txt",
        "weird/hash#1.txt",
        "weird/pct%3.txt",
        "weird/with space.txt",
    ] {
        f.store
            .put(&ObjPath::parse(key).unwrap(), PutPayload::from_static(b"x"))
            .await
            .unwrap();
    }
    let root = f.fs.root_dir();
    assert_eq!(read_names(&f.fs, root).await, ["weird", "名字"]);

    let dir = f.fs.lookup(root, &name("名字")).await.unwrap();
    assert_eq!(read_names(&f.fs, dir).await, ["子目录"]);
    let sub = f.fs.lookup(dir, &name("子目录")).await.unwrap();
    let file = f.fs.lookup(sub, &name("文件.txt")).await.unwrap();
    assert_eq!(f.fs.read(file, 0, 4).await.unwrap().0, b"x");

    let weird = f.fs.lookup(root, &name("weird")).await.unwrap();
    assert_eq!(
        read_names(&f.fs, weird).await,
        ["hash#1.txt", "pct%3.txt", "with space.txt"]
    );
    let hash = f.fs.lookup(weird, &name("hash#1.txt")).await.unwrap();
    assert_eq!(f.fs.read(hash, 0, 4).await.unwrap().0, b"x");

    // Writing through the mount must produce the literal key.
    let (ino, _) = f
        .fs
        .create(root, &name("新建 文件.txt"), sattr3::default())
        .await
        .unwrap();
    f.fs.write(ino, 0, b"hi").await.unwrap();
    f.fs.shutdown().await;
    let stored = f
        .store
        .get(&ObjPath::parse("新建 文件.txt").unwrap())
        .await
        .expect("object stored under its literal key");
    assert_eq!(stored.bytes().await.unwrap().as_ref(), b"hi");
}

#[tokio::test]
async fn a_shared_name_prefix_is_not_a_directory() {
    let f = fixture(false).await;
    f.store
        .put(&ObjPath::from("x/yz.txt"), PutPayload::from_static(b"only"))
        .await
        .unwrap();
    let root = f.fs.root_dir();
    let dir = f.fs.lookup(root, &name("x")).await.unwrap();

    assert_eq!(
        err_code(f.fs.lookup(dir, &name("y")).await.unwrap_err()),
        nfsstat3::NFS3ERR_NOENT as u32,
        "`x/y` must not exist just because `x/yz.txt` does"
    );
    assert!(f.fs.lookup(dir, &name("yz.txt")).await.is_ok());
}
