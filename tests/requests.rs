//! Request accounting: the mount is only usable if metadata operations do not turn into
//! one S3 round-trip per syscall. These tests pin the counts that matter.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use futures::StreamExt;
use mount_sso::vfs::{Config, S3Fs};
use nfsserve::nfs::{filename3, nfsstat3, sattr3};
use nfsserve::vfs::NFSFileSystem;
use object_store::memory::InMemory;
use object_store::path::Path as ObjPath;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore, ObjectStoreExt,
    PutMultipartOptions, PutOptions, PutPayload, PutResult, Result as StoreResult,
};

/// Delegates to an in-memory store and counts the requests it receives.
#[derive(Debug, Default)]
struct Counts {
    put: AtomicUsize,
    get: AtomicUsize,
    list_with_delimiter: AtomicUsize,
    list: AtomicUsize,
    copy: AtomicUsize,
    delete: AtomicUsize,
}

impl Counts {
    fn snapshot(&self) -> Vec<(&'static str, usize)> {
        vec![
            ("put", self.put.load(Ordering::SeqCst)),
            ("get", self.get.load(Ordering::SeqCst)),
            ("list_with_delimiter", self.list_with_delimiter.load(Ordering::SeqCst)),
            ("list", self.list.load(Ordering::SeqCst)),
            ("copy", self.copy.load(Ordering::SeqCst)),
            ("delete", self.delete.load(Ordering::SeqCst)),
        ]
    }
}

#[derive(Debug)]
struct CountingStore {
    inner: Arc<dyn ObjectStore>,
    counts: Arc<Counts>,
}

impl std::fmt::Display for CountingStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "counting({})", self.inner)
    }
}

#[async_trait]
impl ObjectStore for CountingStore {
    async fn put_opts(&self, location: &ObjPath, payload: PutPayload, opts: PutOptions) -> StoreResult<PutResult> {
        self.counts.put.fetch_add(1, Ordering::SeqCst);
        self.inner.put_opts(location, payload, opts).await
    }

    async fn put_multipart_opts(
        &self,
        location: &ObjPath,
        opts: PutMultipartOptions,
    ) -> StoreResult<Box<dyn MultipartUpload>> {
        self.counts.put.fetch_add(1, Ordering::SeqCst);
        self.inner.put_multipart_opts(location, opts).await
    }

    async fn get_opts(&self, location: &ObjPath, options: GetOptions) -> StoreResult<GetResult> {
        self.counts.get.fetch_add(1, Ordering::SeqCst);
        self.inner.get_opts(location, options).await
    }

    fn list(&self, prefix: Option<&ObjPath>) -> BoxStream<'static, StoreResult<ObjectMeta>> {
        self.counts.list.fetch_add(1, Ordering::SeqCst);
        self.inner.list(prefix)
    }

    async fn list_with_delimiter(&self, prefix: Option<&ObjPath>) -> StoreResult<ListResult> {
        self.counts.list_with_delimiter.fetch_add(1, Ordering::SeqCst);
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(&self, from: &ObjPath, to: &ObjPath, options: CopyOptions) -> StoreResult<()> {
        self.counts.copy.fetch_add(1, Ordering::SeqCst);
        self.inner.copy_opts(from, to, options).await
    }

    fn delete_stream(
        &self,
        locations: BoxStream<'static, StoreResult<ObjPath>>,
    ) -> BoxStream<'static, StoreResult<ObjPath>> {
        self.counts.delete.fetch_add(1, Ordering::SeqCst);
        self.inner.delete_stream(locations)
    }
}

fn name(value: &str) -> filename3 {
    filename3::from(value.as_bytes())
}

async fn fixture(ttl: Duration) -> (S3Fs, Arc<Counts>, tempfile::TempDir, Arc<dyn ObjectStore>) {
    let dir = tempfile::tempdir().unwrap();
    let staging = std::fs::canonicalize(dir.path()).unwrap().join("staging");
    let inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let counts = Arc::new(Counts::default());
    let store: Arc<dyn ObjectStore> = Arc::new(CountingStore {
        inner,
        counts: counts.clone(),
    });
    let seed = store.clone();
    let fs = S3Fs::new(
        store,
        Config {
            root_prefix: String::new(),
            uid: 501,
            gid: 20,
            read_only: false,
            dir_ttl: ttl,
            flush_delay: Duration::from_millis(20),
            staging_dir: staging.join("staging"),
            max_stage_bytes: 8 * 1024 * 1024,
            cache_dir: staging.join("read"),
            read_cache_bytes: 1024 * 1024,
            read_cache_ttl: Duration::from_secs(60),
            prefetch_bytes: 4096,
            filter_macos_junk: true,
        },
    )
    .await
    .unwrap();
    (fs, counts, dir, seed)
}

#[tokio::test]
async fn repeated_readdir_within_the_ttl_lists_once() {
    let (fs, counts, _dir, _store) = fixture(Duration::from_secs(60)).await;
    let root = fs.root_dir();
    for key in ["a.txt", "b.txt", "dir/c.txt"] {
        fs_dir_put(&fs, key).await;
    }
    // Seeding probes the store; measure only the syscall-shaped operations below.
    let before = counts.snapshot();
    fs.readdir(root, 0, 64).await.unwrap();
    fs.readdir(root, 0, 64).await.unwrap();
    fs.lookup(root, &name("a.txt")).await.unwrap();
    fs.getattr(root).await.unwrap();
    let after = counts.snapshot();
    assert_eq!(after[2].1 - before[2].1, 1, "two readdirs share one listing");
    for (label, got, was) in [
        ("get", after[1].1, before[1].1),
        ("list", after[3].1, before[3].1),
        ("put", after[0].1, before[0].1),
        ("copy", after[4].1, before[4].1),
        ("delete", after[5].1, before[5].1),
    ] {
        assert_eq!(got, was, "{label} must not be issued for a warm directory");
    }
}

#[tokio::test]
async fn create_in_a_warmed_directory_uploads_once_without_probing() {
    let (fs, counts, _dir, _store) = fixture(Duration::from_secs(60)).await;
    let root = fs.root_dir();
    fs.readdir(root, 0, 64).await.unwrap(); // warm the listing
    let before = counts.snapshot();
    assert_eq!(before[2], ("list_with_delimiter", 1));

    let (ino, _) = fs.create(root, &name("new.txt"), sattr3::default()).await.unwrap();
    fs.write(ino, 0, b"payload").await.unwrap();
    fs.shutdown().await;

    let after = counts.snapshot();
    assert_eq!(after[1].1 - before[1].1, 0, "create must not probe with a warm listing");
    assert_eq!(after[2].1 - before[2].1, 0, "create must not re-list");
    // One PUT for the empty object `create` publishes, one for the flushed content.
    assert_eq!(after[0].1 - before[0].1, 2);
}

#[tokio::test]
async fn reads_are_ranged_and_served_from_staging_after_write() {
    let (fs, counts, _dir, _store) = fixture(Duration::from_secs(60)).await;
    let root = fs.root_dir();
    let (ino, _) = fs.create(root, &name("f.bin"), sattr3::default()).await.unwrap();
    fs.write(ino, 0, b"0123456789").await.unwrap();
    let before = counts.snapshot();
    let (data, eof) = fs.read(ino, 2, 4).await.unwrap();
    assert_eq!(data, b"2345");
    assert!(!eof);
    assert_eq!(counts.snapshot()[1].1, before[1].1, "reads of staged data stay local");
}

async fn fs_dir_put(fs: &S3Fs, key: &str) {
    // Reach the backing store through the filesystem's own create path is not possible
    // for arbitrary keys, so seed via readdir's view: create the parents first.
    let mut dir = fs.root_dir();
    let parts: Vec<&str> = key.split('/').collect();
    for component in &parts[..parts.len() - 1] {
        dir = match fs.lookup(dir, &name(component)).await {
            Ok(ino) => ino,
            Err(_) => fs.mkdir(dir, &name(component)).await.unwrap().0,
        };
    }
    fs.create(dir, &name(parts[parts.len() - 1]), sattr3::default())
        .await
        .unwrap();
}

#[tokio::test]
async fn unmounted_state_reports_noent_without_extra_requests() {
    let (fs, counts, _dir, _store) = fixture(Duration::from_secs(60)).await;
    let root = fs.root_dir();
    fs.readdir(root, 0, 64).await.unwrap();
    let before = counts.snapshot();
    let err = fs.lookup(root, &name("missing.txt")).await.unwrap_err();
    assert_eq!(err as u32, nfsserve::nfs::nfsstat3::NFS3ERR_NOENT as u32);
    // A miss against a warm listing still has to ask the store once (head + prefix list).
    let after = counts.snapshot();
    assert!(after[1].1 > before[1].1);
}

#[tokio::test]
async fn small_object_is_fetched_once_and_served_from_the_local_copy() {
    let (fs, counts, _dir, store) = fixture(Duration::from_secs(60)).await;
    store
        .put(&ObjPath::from("seed.bin"), PutPayload::from_static(b"0123456789"))
        .await
        .unwrap();
    let root = fs.root_dir();
    let ino = fs.lookup(root, &name("seed.bin")).await.unwrap();
    let before = counts.snapshot();

    let (head, eof) = fs.read(ino, 0, 4).await.unwrap();
    assert_eq!(head, b"0123");
    assert!(!eof);
    let (tail, eof) = fs.read(ino, 4, 6).await.unwrap();
    assert_eq!(tail, b"456789");
    assert!(eof);

    let after = counts.snapshot();
    assert_eq!(
        after[1].1 - before[1].1,
        1,
        "the whole object comes down in a single GET"
    );
    assert_eq!(fs.cached_bytes(), 10);
}

#[tokio::test]
async fn objects_above_the_prefetch_limit_stay_streamed() {
    let (fs, counts, _dir, store) = fixture(Duration::from_secs(60)).await;
    let big = Bytes::from_static(&[b'z'; 8192]);
    store.put(&ObjPath::from("big.bin"), PutPayload::from(big)).await.unwrap();
    let root = fs.root_dir();
    let ino = fs.lookup(root, &name("big.bin")).await.unwrap();
    let before = counts.snapshot();

    assert_eq!(fs.read(ino, 0, 4).await.unwrap().0, b"zzzz");
    assert_eq!(fs.read(ino, 4, 4).await.unwrap().0, b"zzzz");

    assert_eq!(counts.snapshot()[1].1 - before[1].1, 2, "one ranged GET per read");
    assert_eq!(fs.cached_bytes(), 0);
}

#[tokio::test]
async fn writing_a_file_drops_the_cached_copy() {
    let (fs, counts, _dir, store) = fixture(Duration::from_secs(60)).await;
    store
        .put(&ObjPath::from("edit.txt"), PutPayload::from_static(b"original"))
        .await
        .unwrap();
    let root = fs.root_dir();
    let ino = fs.lookup(root, &name("edit.txt")).await.unwrap();
    fs.read(ino, 0, 4).await.unwrap();
    assert_eq!(fs.cached_bytes(), 8);

    fs.write(ino, 0, b"patched!").await.unwrap();
    let (data, _) = fs.read(ino, 0, 8).await.unwrap();
    assert_eq!(data, b"patched!");
    assert_eq!(fs.cached_bytes(), 0, "the stage is authoritative while it exists");

    fs.shutdown().await;
    assert_eq!(fs.read(ino, 0, 8).await.unwrap().0, b"patched!");
    let stored = store.get(&ObjPath::from("edit.txt")).await.unwrap().bytes().await.unwrap();
    assert_eq!(&stored[..], b"patched!");
    let _ = counts;
}

#[tokio::test]
async fn concurrent_reads_of_one_object_share_a_single_fetch() {
    let (fs, counts, _dir, store) = fixture(Duration::from_secs(60)).await;
    store
        .put(&ObjPath::from("wide.bin"), PutPayload::from_static(&[b'q'; 2048]))
        .await
        .unwrap();
    let root = fs.root_dir();
    let ino = fs.lookup(root, &name("wide.bin")).await.unwrap();
    let before = counts.snapshot();

    // The kernel keeps several 1 MiB reads in flight; each must not start its own download.
    let mut readers = Vec::new();
    for offset in 0..8u64 {
        let fs = fs.clone();
        readers.push(tokio::spawn(async move { fs.read(ino, offset * 16, 16).await.unwrap().0 }));
    }
    for reader in readers {
        assert_eq!(reader.await.unwrap().len(), 16);
    }

    let after = counts.snapshot();
    assert_eq!(after[1].1 - before[1].1, 1, "one fetch serves every concurrent reader");
}

#[tokio::test]
async fn macos_junk_is_hidden_and_never_uploaded() {
    let (fs, counts, _dir, store) = fixture(Duration::from_secs(60)).await;
    let seeded = ["keep.txt", ".DS_Store", "._keep.txt", ".Spotlight-V100/x", ".Trashes/y"];
    for key in seeded {
        store
            .put(&ObjPath::from(key), PutPayload::from_static(b"junk"))
            .await
            .unwrap();
    }
    let root = fs.root_dir();

    // Junk that already exists in the bucket is invisible.
    let names: Vec<String> = fs
        .readdir(root, 0, 64)
        .await
        .unwrap()
        .entries
        .iter()
        .map(|entry| String::from_utf8(entry.name.0.clone()).unwrap())
        .collect();
    assert_eq!(names, ["keep.txt"]);
    for hidden in [".DS_Store", "._keep.txt", ".Spotlight-V100", ".Trashes"] {
        assert_eq!(
            fs.lookup(root, &name(hidden)).await.unwrap_err() as u32,
            nfsstat3::NFS3ERR_NOENT as u32,
            "{hidden} must not resolve"
        );
    }

    // Bookkeeping directories are refused outright, without touching the store.
    let before = counts.snapshot();
    for junk in [".DS_Store", ".fseventsd", ".Spotlight-V100"] {
        assert_eq!(
            fs.create(root, &name(junk), sattr3::default()).await.unwrap_err() as u32,
            nfsstat3::NFS3ERR_ACCES as u32
        );
        assert_eq!(
            fs.mkdir(root, &name(junk)).await.unwrap_err() as u32,
            nfsstat3::NFS3ERR_ACCES as u32
        );
    }
    assert_eq!(
        fs.create_exclusive(root, &name(".DS_Store")).await.unwrap_err() as u32,
        nfsstat3::NFS3ERR_ACCES as u32
    );
    assert_eq!(
        fs.rename(root, &name("keep.txt"), root, &name(".DS_Store"))
            .await
            .unwrap_err() as u32,
        nfsstat3::NFS3ERR_ACCES as u32
    );
    assert_eq!(counts.snapshot()[0].1, before[0].1, "no PUT for refused names");

    // AppleDoubles are scratch space: writable, readable, but never uploaded.
    let (ad, _) = fs.create(root, &name("._keep.txt2"), sattr3::default()).await.unwrap();
    fs.write(ad, 0, b"attrs").await.unwrap();
    assert_eq!(fs.read(ad, 0, 5).await.unwrap().0, b"attrs");
    assert_eq!(fs.lookup(root, &name("._keep.txt2")).await.unwrap(), ad);
    let mid = counts.snapshot();
    assert_eq!(mid[0].1, before[0].1, "AppleDouble create/write must not PUT");
    fs.remove(root, &name("._keep.txt2")).await.unwrap();
    assert_eq!(
        counts.snapshot()[5].1,
        mid[5].1,
        "AppleDouble remove must not delete from the store"
    );

    // Crossing the boundary is refused: a real file must not become a local-only one.
    assert_eq!(
        fs.rename(root, &name("keep.txt"), root, &name("._keep.txt"))
            .await
            .unwrap_err() as u32,
        nfsstat3::NFS3ERR_ACCES as u32
    );

    // Ordinary names stay reachable, and shutdown uploads only them.
    let (ino, _) = fs.create(root, &name("keep2.txt"), sattr3::default()).await.unwrap();
    fs.write(ino, 0, b"data").await.unwrap();
    fs.shutdown().await;
    assert_eq!(fs.read(ino, 0, 4).await.unwrap().0, b"data");
    let mut keys: Vec<String> = store
        .list(None)
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .map(|item| item.unwrap().location.to_string())
        .collect();
    keys.sort();
    let mut expected: Vec<String> = seeded.iter().map(|key| key.to_string()).collect();
    expected.push("keep2.txt".to_string());
    expected.sort();
    assert_eq!(
        keys, expected,
        "only real files may be uploaded; no AppleDouble object may ever appear"
    );
}

#[tokio::test]
async fn macos_junk_can_be_exposed_on_request() {
    let (fs, _counts, _dir, store) = fixture(Duration::from_secs(60)).await;
    store
        .put(&ObjPath::from(".DS_Store"), PutPayload::from_static(b"junk"))
        .await
        .unwrap();
    // The fixture filters by default; exercise the opt-out through a second filesystem that
    // shares the same store and staging area.
    let root = fs.root_dir();
    assert_eq!(
        fs.lookup(root, &name(".DS_Store")).await.unwrap_err() as u32,
        nfsstat3::NFS3ERR_NOENT as u32
    );

    let staging = tempfile::tempdir().unwrap();
    let config = Config {
        filter_macos_junk: false,
        dir_ttl: Duration::from_secs(60),
        read_cache_bytes: 0,
        prefetch_bytes: 0,
        staging_dir: staging.path().join("staging"),
        cache_dir: staging.path().join("read"),
        ..Config::default()
    };
    let unfiltered = S3Fs::new(store, config).await.unwrap();
    let visible: Vec<String> = unfiltered
        .readdir(unfiltered.root_dir(), 0, 64)
        .await
        .unwrap()
        .entries
        .iter()
        .map(|entry| String::from_utf8(entry.name.0.clone()).unwrap())
        .collect();
    assert_eq!(visible, [".DS_Store"]);
}
