//! A `NFSFileSystem` view of an S3-compatible bucket.
//!
//! S3 has a flat key space with `/`-delimited keys and no POSIX attributes, so this module
//! implements the translation layer:
//!
//! * **Namespaces** — a key prefix is a directory, everything below the mount root is a
//!   sub-path. Directories are discovered from `list_with_delimiter` results; they are never
//!   materialised as objects (no `key/` markers), so an empty directory lives for the lifetime
//!   of the session only.
//! * **Inodes** — `fileid3`s are allocated per path for the lifetime of the mount and cached
//!   together with directory listings (bounded by a TTL) to keep the S3 request count sane.
//! * **Writes** — objects are immutable and the NFS trait has no close/commit hook, so writes
//!   are staged in a local temp file per inode and uploaded after a short idle period
//!   (`flush_delay`), on unmount, and before any operation that must observe the data
//!   (`rename`, `truncate`, ...).

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use nfsserve::nfs::{
    FSF_CANSETTIME, FSF_HOMOGENEOUS, fattr3, fileid3, filename3, fsinfo3, ftype3, nfspath3, nfsstat3, nfstime3,
    post_op_attr, sattr3, set_atime, set_mtime, set_size3, specdata3,
};
use nfsserve::vfs::{DirEntry, NFSFileSystem, ReadDirResult, VFSCapabilities};
use object_store::path::Path as ObjPath;
use object_store::{
    Error as ObjectStoreError, ObjectStore, ObjectStoreExt, PutMode, PutMultipartOptions, PutOptions, PutPayload,
    WriteMultipart,
};
use tokio::io::AsyncWriteExt;
use tokio::sync::OnceCell;
use tracing::{debug, warn};

use crate::cache::ReadCache;

const ROOT_INO: fileid3 = 1;
const FILE_MODE: u32 = 0o644;
const DIR_MODE: u32 = 0o755;
const DIR_SIZE: u64 = 4096;
/// Objects up to this size are uploaded with a single PUT (one in-memory buffer).
const PUT_THRESHOLD: u64 = 8 * 1024 * 1024;
/// Multipart chunk size, also the read granularity used while uploading from disk.
const UPLOAD_CHUNK: usize = 8 * 1024 * 1024;
/// Maximum number of parts in flight for a multipart upload.
const UPLOAD_CONCURRENCY: usize = 8;

/// macOS bookkeeping that must never reach the bucket: hidden from listings and refused on
/// create. The Finder drops `.DS_Store` into every directory it visits, Spotlight and Time
/// Machine keep their own folders, and nothing in here is object data.
const MACOS_JUNK_NAMES: &[&str] = &[
    ".DS_Store",
    ".Spotlight-V100",
    ".Trashes",
    ".fseventsd",
    ".TemporaryItems",
    ".DocumentRevisions-V100",
    ".apdisk",
    ".VolumeIcon.icns",
    "Icon\r",
];

fn is_macos_junk(name: &str) -> bool {
    MACOS_JUNK_NAMES.contains(&name)
}

/// AppleDouble siblings (`._name`) are how macOS stores extended attributes on filesystems
/// without xattr support. They are hidden from listings and never uploaded, but they are
/// writable: `cp -a`, `ditto` and the Finder write one next to every file whose attributes
/// they want to preserve, and failing those writes breaks the copy. The bytes live in the
/// staging file for the lifetime of the mount and evaporate with it.
fn is_appledouble(name: &str) -> bool {
    name.starts_with("._")
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Dir,
    File,
}

#[derive(Clone, Debug)]
struct Entry {
    path: String,
    kind: Kind,
    size: u64,
    mtime: u64,
    atime: u64,
    mode: u32,
    /// When the S3 metadata backing this entry was last observed.
    checked: Instant,
}

struct DirListing {
    /// Sorted by name; this order is what `readdir` paginates over.
    entries: Vec<(String, fileid3)>,
    fetched: Instant,
}

struct State {
    next_ino: fileid3,
    by_id: HashMap<fileid3, Entry>,
    by_path: HashMap<String, fileid3>,
    dirs: HashMap<fileid3, DirListing>,
    /// Directories that exist only because `mkdir` created them this session.
    pending_dirs: HashSet<String>,
    /// Inodes whose content is authoritative in the staging file.
    staged: HashSet<fileid3>,
    /// AppleDouble scratch inodes: readable and writable, never uploaded.
    local_only: HashSet<fileid3>,
}

impl State {
    fn new() -> Self {
        let mut this = Self {
            next_ino: ROOT_INO,
            by_id: HashMap::new(),
            by_path: HashMap::new(),
            dirs: HashMap::new(),
            pending_dirs: HashSet::new(),
            staged: HashSet::new(),
            local_only: HashSet::new(),
        };
        let now = now_secs();
        this.by_id.insert(
            ROOT_INO,
            Entry {
                path: String::new(),
                kind: Kind::Dir,
                size: DIR_SIZE,
                mtime: now,
                atime: now,
                mode: DIR_MODE,
                checked: Instant::now(),
            },
        );
        this.by_path.insert(String::new(), ROOT_INO);
        this
    }

    /// Returns the inode for `path`, allocating one if needed.
    ///
    /// Freshly observed S3 metadata is applied to existing inodes unless the inode has staged
    /// (not yet uploaded) content, which is the only authority for its own size.
    fn intern(&mut self, path: String, kind: Kind, size: u64, mtime: u64) -> fileid3 {
        if let Some(&ino) = self.by_path.get(&path) {
            let staged = self.staged.contains(&ino);
            if let Some(entry) = self.by_id.get_mut(&ino) {
                entry.kind = kind;
                entry.checked = Instant::now();
                if !staged {
                    entry.size = match kind {
                        Kind::Dir => DIR_SIZE,
                        Kind::File => size,
                    };
                    if mtime != 0 {
                        entry.mtime = mtime;
                    }
                }
            }
            return ino;
        }
        self.next_ino += 1;
        let ino = self.next_ino;
        let now = now_secs();
        self.by_id.insert(
            ino,
            Entry {
                path: path.clone(),
                kind,
                size: match kind {
                    Kind::Dir => DIR_SIZE,
                    Kind::File => size,
                },
                mtime,
                atime: now,
                mode: match kind {
                    Kind::Dir => DIR_MODE,
                    Kind::File => FILE_MODE,
                },
                checked: Instant::now(),
            },
        );
        self.by_path.insert(path, ino);
        ino
    }

    fn forget(&mut self, ino: fileid3) {
        if let Some(entry) = self.by_id.remove(&ino) {
            self.by_path.remove(&entry.path);
            self.dirs.remove(&ino);
        }
        self.staged.remove(&ino);
        self.local_only.remove(&ino);
    }

    /// Rewrite every registered path under `from` (as a directory) to start with `to`.
    fn rewrite_paths(&mut self, from: &str, to: &str) {
        let prefix = format!("{from}/");
        let moved: Vec<(fileid3, String)> = self
            .by_path
            .iter()
            .filter(|(path, _)| *path == from || path.starts_with(&prefix))
            .map(|(path, ino)| {
                let suffix = &path[from.len()..];
                (*ino, format!("{to}{suffix}"))
            })
            .collect();
        for (ino, new_path) in moved {
            if let Some(entry) = self.by_id.get_mut(&ino) {
                let old_path = std::mem::replace(&mut entry.path, new_path.clone());
                self.by_path.remove(&old_path);
            }
            self.by_path.insert(new_path, ino);
        }
        let pending: Vec<String> = self
            .pending_dirs
            .iter()
            .filter(|path| *path == from || path.starts_with(&prefix))
            .cloned()
            .collect();
        for path in pending {
            self.pending_dirs.remove(&path);
            let suffix = &path[from.len()..];
            self.pending_dirs.insert(format!("{to}{suffix}"));
        }
    }
}

struct StageMeta {
    len: u64,
    dirty: bool,
    uploading: bool,
    last_write: Instant,
}

struct Stage {
    ino: fileid3,
    file: Arc<File>,
    meta: StdMutex<StageMeta>,
    /// Number of in-flight operations using this stage; guards against eviction.
    pins: AtomicUsize,
}

/// Pinned reference to a staging file. Dropping it unpins the stage.
struct StageHandle {
    stage: Arc<Stage>,
}

impl StageHandle {
    fn stage(&self) -> &Arc<Stage> {
        &self.stage
    }
}

impl Drop for StageHandle {
    fn drop(&mut self) {
        self.stage.pins.fetch_sub(1, Ordering::SeqCst);
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    /// Key prefix inside the bucket that becomes the mount root (no leading/trailing `/`).
    pub root_prefix: String,
    pub uid: u32,
    pub gid: u32,
    pub read_only: bool,
    /// How long S3 listings/attributes are trusted without re-fetching.
    pub dir_ttl: Duration,
    /// Idle time after a write before its staging file is uploaded.
    pub flush_delay: Duration,
    pub staging_dir: PathBuf,
    /// Total staged bytes allowed before clean stages are evicted.
    pub max_stage_bytes: u64,
    /// Local directory holding object contents fetched for reading.
    pub cache_dir: PathBuf,
    /// Cached bytes allowed on disk; `0` disables the read cache.
    pub read_cache_bytes: u64,
    /// How long a cached object is served before it is fetched again.
    pub read_cache_ttl: Duration,
    /// Objects up to this size are fetched whole on the first read; `0` disables it.
    pub prefetch_bytes: u64,
    /// Hide macOS metadata (`.DS_Store`, `._*`, Spotlight/Time Machine folders) from the
    /// mount and refuse to create it.
    pub filter_macos_junk: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            root_prefix: String::new(),
            uid: 501,
            gid: 20,
            read_only: false,
            dir_ttl: Duration::from_secs(2),
            flush_delay: Duration::from_secs(1),
            staging_dir: PathBuf::from("staging"),
            max_stage_bytes: 4 * 1024 * 1024 * 1024,
            cache_dir: PathBuf::from("read-cache"),
            read_cache_bytes: 4 * 1024 * 1024 * 1024,
            read_cache_ttl: Duration::from_secs(30),
            prefetch_bytes: 256 * 1024 * 1024,
            filter_macos_junk: true,
        }
    }
}

struct Inner {
    store: Arc<dyn ObjectStore>,
    cfg: Config,
    state: StdMutex<State>,
    stages: StdMutex<HashMap<fileid3, Arc<OnceCell<Arc<Stage>>>>>,
    cache: ReadCache,
    /// One lock per inode: concurrent reads of the same uncached object wait for the
    /// single download instead of starting one each.
    prefetch_gates: StdMutex<HashMap<fileid3, Arc<tokio::sync::Mutex<()>>>>,
}

#[derive(Clone)]
pub struct S3Fs {
    inner: Arc<Inner>,
}

// ── helpers ─────────────────────────────────────────────────────────────

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn join_rel(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

fn split_parent(rel: &str) -> (&str, &str) {
    match rel.rfind('/') {
        Some(i) => (&rel[..i], &rel[i + 1..]),
        None => ("", rel),
    }
}

fn nfs_err(err: ObjectStoreError) -> nfsstat3 {
    match err {
        ObjectStoreError::NotFound { .. } => nfsstat3::NFS3ERR_NOENT,
        ObjectStoreError::AlreadyExists { .. } | ObjectStoreError::Precondition { .. } => nfsstat3::NFS3ERR_EXIST,
        ObjectStoreError::NotSupported { .. } | ObjectStoreError::NotImplemented { .. } => nfsstat3::NFS3ERR_NOTSUPP,
        ObjectStoreError::PermissionDenied { .. } | ObjectStoreError::Unauthenticated { .. } => {
            nfsstat3::NFS3ERR_ACCES
        }
        ObjectStoreError::InvalidPath { .. } => nfsstat3::NFS3ERR_INVAL,
        other => {
            warn!("object store error: {other}");
            nfsstat3::NFS3ERR_IO
        }
    }
}

fn io_err(err: std::io::Error) -> nfsstat3 {
    warn!("local io error: {err}");
    nfsstat3::NFS3ERR_IO
}


/// Read `offset..end` from a local copy of the object, reporting EOF at its end.
async fn read_cached(file: File, len: u64, offset: u64, end: u64) -> Result<(Vec<u8>, bool), nfsstat3> {
    let end = std::cmp::min(end, len);
    if offset >= len || end <= offset {
        return Ok((Vec::new(), true));
    }
    let want = (end - offset) as usize;
    let data = tokio::task::spawn_blocking(move || {
        let mut buf = vec![0u8; want];
        file.read_exact_at(&mut buf, offset)?;
        Ok::<Vec<u8>, std::io::Error>(buf)
    })
    .await
    .map_err(|_| nfsstat3::NFS3ERR_IO)?
    .map_err(io_err)?;
    Ok((data, end >= len))
}

fn fattr_of(ino: fileid3, entry: &Entry, uid: u32, gid: u32) -> fattr3 {
    let (ftype, nlink, size) = match entry.kind {
        Kind::Dir => (ftype3::NF3DIR, 2, DIR_SIZE),
        Kind::File => (ftype3::NF3REG, 1, entry.size),
    };
    fattr3 {
        ftype,
        mode: entry.mode,
        nlink,
        uid,
        gid,
        size,
        used: size,
        rdev: specdata3 {
            specdata1: 0,
            specdata2: 0,
        },
        fsid: 0,
        fileid: ino,
        atime: nfstime3 {
            seconds: entry.atime as u32,
            nseconds: 0,
        },
        mtime: nfstime3 {
            seconds: entry.mtime as u32,
            nseconds: 0,
        },
        ctime: nfstime3 {
            seconds: entry.mtime as u32,
            nseconds: 0,
        },
    }
}

struct Child {
    name: String,
    kind: Kind,
    size: u64,
    mtime: u64,
}

// ── Inner ───────────────────────────────────────────────────────────────

impl Inner {
    /// Bucket key for a path relative to the mount root.
    fn abs(&self, rel: &str) -> String {
        match (self.cfg.root_prefix.is_empty(), rel.is_empty()) {
            (_, true) => self.cfg.root_prefix.clone(),
            (true, false) => rel.to_string(),
            (false, false) => format!("{}/{}", self.cfg.root_prefix, rel),
        }
    }

    fn key(&self, rel: &str) -> ObjPath {
        ObjPath::from(self.abs(rel))
    }

    /// True when `name` is macOS bookkeeping that this mount hides and refuses to create.
    fn is_junk(&self, name: &str) -> bool {
        self.cfg.filter_macos_junk && is_macos_junk(name)
    }

    /// True when `name` is an AppleDouble sibling: hidden, local-only, never uploaded.
    fn is_appledouble(&self, name: &str) -> bool {
        self.cfg.filter_macos_junk && is_appledouble(name)
    }

    fn stage_path(&self, ino: fileid3) -> PathBuf {
        self.cfg.staging_dir.join(format!("{ino}.stage"))
    }

    fn entry(&self, ino: fileid3) -> Result<Entry, nfsstat3> {
        self.state
            .lock()
            .expect("state lock poisoned")
            .by_id
            .get(&ino)
            .cloned()
            .ok_or(nfsstat3::NFS3ERR_STALE)
    }

    fn dir_path(&self, ino: fileid3) -> Result<String, nfsstat3> {
        let entry = self.entry(ino)?;
        if entry.kind != Kind::Dir {
            return Err(nfsstat3::NFS3ERR_NOTDIR);
        }
        Ok(entry.path)
    }

    fn invalidate_dir(&self, ino: fileid3) {
        self.state.lock().expect("state lock poisoned").dirs.remove(&ino);
    }

    /// Fetch the direct children of `rel` from S3.
    async fn list_children(&self, rel: &str) -> Result<Vec<Child>, nfsstat3> {
        let base = self.abs(rel);
        let prefix = if base.is_empty() {
            None
        } else {
            Some(ObjPath::from(base.clone()))
        };
        let listing = self
            .store
            .list_with_delimiter(prefix.as_ref())
            .await
            .map_err(nfs_err)?;
        let head = if base.is_empty() {
            String::new()
        } else {
            format!("{base}/")
        };
        let mut out = Vec::with_capacity(listing.objects.len() + listing.common_prefixes.len());
        for meta in listing.objects {
            let full = meta.location.to_string();
            let Some(name) = full.strip_prefix(&head) else {
                continue;
            };
            // Skip the directory's own marker (`dir/`), anything nested deeper, and
            // macOS bookkeeping.
            if name.is_empty()
                || name.ends_with('/')
                || name.contains('/')
                || self.is_junk(name)
                || self.is_appledouble(name)
            {
                continue;
            }
            out.push(Child {
                name: name.to_string(),
                kind: Kind::File,
                size: meta.size,
                mtime: meta.last_modified.timestamp().max(0) as u64,
            });
        }
        for prefix in listing.common_prefixes {
            let full = prefix.to_string();
            let Some(name) = full.strip_prefix(&head) else {
                continue;
            };
            let name = name.trim_end_matches('/');
            if name.is_empty() || name.contains('/') || self.is_junk(name) || self.is_appledouble(name) {
                continue;
            }
            out.push(Child {
                name: name.to_string(),
                kind: Kind::Dir,
                size: DIR_SIZE,
                mtime: 0,
            });
        }
        Ok(out)
    }

    /// Directory contents as (name, inode), sorted by name.
    ///
    /// Serves from the listing cache unless it is stale or `force` is set.
    async fn dir_entries(&self, ino: fileid3, rel: &str, force: bool) -> Result<Vec<(String, fileid3)>, nfsstat3> {
        if !force {
            let cached = {
                let state = self.state.lock().expect("state lock poisoned");
                state
                    .dirs
                    .get(&ino)
                    .filter(|listing| listing.fetched.elapsed() < self.cfg.dir_ttl)
                    .map(|listing| listing.entries.clone())
            };
            if let Some(entries) = cached {
                return Ok(entries);
            }
        }

        let children = self.list_children(rel).await?;

        let mut state = self.state.lock().expect("state lock poisoned");
        let mut entries: Vec<(String, fileid3)> = Vec::with_capacity(children.len());
        for child in &children {
            let child_rel = join_rel(rel, &child.name);
            let child_ino = state.intern(child_rel, child.kind, child.size, child.mtime);
            entries.push((child.name.clone(), child_ino));
        }
        // Directories created via mkdir this session have no objects behind them.
        let pending: Vec<String> = state
            .pending_dirs
            .iter()
            .filter(|path| split_parent(path).0 == rel)
            .cloned()
            .collect();
        for path in pending {
            let (_, name) = split_parent(&path);
            let name = name.to_string();
            let child_ino = state.intern(path, Kind::Dir, DIR_SIZE, 0);
            if !entries.iter().any(|(existing, _)| existing == &name) {
                entries.push((name, child_ino));
            }
        }
        entries.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
        state.dirs.insert(
            ino,
            DirListing {
                entries: entries.clone(),
                fetched: Instant::now(),
            },
        );
        Ok(entries)
    }

    /// Resolve a single relative path to its kind and metadata by talking to S3 directly.
    async fn probe(&self, rel: &str) -> Result<Option<(Kind, u64, u64)>, nfsstat3> {
        match self.store.head(&self.key(rel)).await {
            Ok(meta) => {
                return Ok(Some((
                    Kind::File,
                    meta.size,
                    meta.last_modified.timestamp().max(0) as u64,
                )));
            }
            Err(ObjectStoreError::NotFound { .. }) => {}
            Err(err) => return Err(nfs_err(err)),
        }
        if self
            .state
            .lock()
            .expect("state lock poisoned")
            .pending_dirs
            .contains(rel)
        {
            return Ok(Some((Kind::Dir, DIR_SIZE, 0)));
        }
        // A directory exists iff something is stored below it (or a `rel/` marker does).
        let prefix = ObjPath::from(format!("{}/", self.abs(rel)));
        let mut stream = self.store.list(Some(&prefix));
        match stream.next().await {
            Some(Ok(_)) => Ok(Some((Kind::Dir, DIR_SIZE, 0))),
            Some(Err(err)) => Err(nfs_err(err)),
            None => Ok(None),
        }
    }

    /// Look up a relative path, allocating an inode if it is not known yet.
    /// True when `name` provably does not exist below `dir_ino`.
    ///
    /// No probe is needed: the name has no inode, and the parent listing that is still
    /// inside the TTL window does not mention it. Callers fall back to a store probe
    /// otherwise.
    fn listing_says_absent(&self, dir_ino: fileid3, name: &str, rel: &str) -> bool {
        let state = self.state.lock().expect("state lock poisoned");
        if state.by_path.contains_key(rel) {
            return false;
        }
        state
            .dirs
            .get(&dir_ino)
            .filter(|listing| listing.fetched.elapsed() < self.cfg.dir_ttl)
            .is_some_and(|listing| !listing.entries.iter().any(|(child, _)| child == name))
    }

    async fn resolve_rel(&self, rel: &str) -> Result<(fileid3, Entry), nfsstat3> {
        let known = {
            let state = self.state.lock().expect("state lock poisoned");
            state.by_path.get(rel).and_then(|ino| state.by_id.get(ino).cloned().map(|e| (*ino, e)))
        };
        if let Some((ino, entry)) = known {
            return Ok((ino, entry));
        }
        match self.probe(rel).await? {
            Some((kind, size, mtime)) => {
                let mut state = self.state.lock().expect("state lock poisoned");
                let ino = state.intern(rel.to_string(), kind, size, mtime);
                let entry = state.by_id.get(&ino).cloned().ok_or(nfsstat3::NFS3ERR_STALE)?;
                Ok((ino, entry))
            }
            None => Err(nfsstat3::NFS3ERR_NOENT),
        }
    }

    /// Pin an existing staging file, if any.
    fn acquire_stage(&self, ino: fileid3) -> Option<StageHandle> {
        let stages = self.stages.lock().expect("stages lock poisoned");
        let stage = stages.get(&ino)?.get()?.clone();
        stage.pins.fetch_add(1, Ordering::SeqCst);
        Some(StageHandle { stage })
    }

    /// Get or create the staging file for `ino`.
    /// Serialize prefetching per inode.
    fn prefetch_gate(&self, ino: fileid3) -> Arc<tokio::sync::Mutex<()>> {
        let mut gates = self.prefetch_gates.lock().expect("prefetch gates lock poisoned");
        gates.entry(ino).or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))).clone()
    }

    /// Fetch an object whole into the read cache so later reads stay local.
    async fn prefetch(&self, ino: fileid3, key: &ObjPath, size: u64, mtime: u64) -> Result<(File, u64)> {
        let part = self.cache.part_path(ino);
        let mut file = tokio::fs::File::create(&part)
            .await
            .with_context(|| format!("creating read cache file {}", part.display()))?;
        let result = self.store.get(key).await.context("fetching object")?;
        let mut stream = result.into_stream();
        let mut len = 0u64;
        let downloaded = async {
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.context("streaming object into the read cache")?;
                len += chunk.len() as u64;
                file.write_all(&chunk).await.context("writing read cache file")?;
            }
            file.flush().await.context("flushing read cache file")
        }
        .await;
        drop(file);
        if let Err(err) = downloaded {
            let _ = std::fs::remove_file(&part);
            return Err(err);
        }
        debug!("cached inode {ino} ({len} bytes, object size {size})");
        self.cache.insert(ino, &part, len, size, mtime)
    }

    async fn stage_for(&self, ino: fileid3) -> Result<StageHandle, nfsstat3> {
        let cell = {
            let mut stages = self.stages.lock().expect("stages lock poisoned");
            stages.entry(ino).or_insert_with(|| Arc::new(OnceCell::new())).clone()
        };
        // `get_or_try_init` guarantees exactly one download per inode.
        let stage = cell
            .get_or_try_init(|| async { self.create_stage(ino).await })
            .await
            .map_err(|err: anyhow::Error| {
                warn!("staging inode {ino} failed: {err:#}");
                nfsstat3::NFS3ERR_IO
            })?
            .clone();
        stage.pins.fetch_add(1, Ordering::SeqCst);
        Ok(StageHandle { stage })
    }

    /// Create the staging file, seeding it with the current object contents.
    async fn create_stage(&self, ino: fileid3) -> Result<Arc<Stage>> {
        // The stage becomes authoritative for this inode; a cached copy would be wrong as
        // soon as the first write lands.
        self.cache.invalidate(ino);
        let entry = self
            .entry(ino)
            .map_err(|err| anyhow::anyhow!("stale inode {ino}: {err:?}"))?;
        let path = self.stage_path(ino);
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&path)
            .await
            .with_context(|| format!("creating staging file {}", path.display()))?;
        if entry.size > 0 {
            match self.store.get(&self.key(&entry.path)).await {
                Ok(result) => {
                    let mut stream = result.into_stream();
                    while let Some(chunk) = stream.next().await {
                        let chunk = chunk.map_err(|err| anyhow::anyhow!("reading {}: {err}", entry.path))?;
                        file.write_all(&chunk).await.context("seeding staging file")?;
                    }
                    file.flush().await.context("flushing staging file")?;
                }
                Err(ObjectStoreError::NotFound { .. }) => {}
                Err(err) => return Err(anyhow::anyhow!("reading {}: {err}", entry.path)),
            }
        }
        let file = Arc::new(file.into_std().await);
        self.state.lock().expect("state lock poisoned").staged.insert(ino);
        debug!("staged inode {ino} ({} bytes) at {}", entry.size, path.display());
        Ok(Arc::new(Stage {
            ino,
            file,
            meta: StdMutex::new(StageMeta {
                len: entry.size,
                dirty: false,
                uploading: false,
                last_write: Instant::now(),
            }),
            pins: AtomicUsize::new(0),
        }))
    }

    /// Upload a staging file to the object store, overwriting the object.
    async fn upload(&self, key: &ObjPath, file: &Arc<File>, len: u64) -> Result<()> {
        if len == 0 {
            self.store
                .put(key, PutPayload::new())
                .await
                .context("putting empty object")?;
            return Ok(());
        }
        if len <= PUT_THRESHOLD {
            let handle = file.clone();
            let size = len as usize;
            let data = tokio::task::spawn_blocking(move || {
                let mut buf = vec![0u8; size];
                handle.read_exact_at(&mut buf, 0)?;
                Ok::<Vec<u8>, std::io::Error>(buf)
            })
            .await
            .context("joining staging read")?
            .context("reading staging file")?;
            self.store
                .put(key, PutPayload::from(Bytes::from(data)))
                .await
                .context("putting object")?;
            return Ok(());
        }

        let multipart = self
            .store
            .put_multipart_opts(key, PutMultipartOptions::default())
            .await
            .context("starting multipart upload")?;
        let mut writer = WriteMultipart::new_with_chunk_size(multipart, UPLOAD_CHUNK);
        let mut offset = 0u64;
        while offset < len {
            let want = std::cmp::min(UPLOAD_CHUNK as u64, len - offset) as usize;
            let handle = file.clone();
            let chunk = tokio::task::spawn_blocking(move || {
                let mut buf = vec![0u8; want];
                handle.read_exact_at(&mut buf, offset)?;
                Ok::<Vec<u8>, std::io::Error>(buf)
            })
            .await
            .context("joining staging read")?
            .context("reading staging file")?;
            writer
                .wait_for_capacity(UPLOAD_CONCURRENCY)
                .await
                .context("multipart backpressure")?;
            writer.write(&chunk);
            offset += want as u64;
        }
        writer.finish().await.context("finishing multipart upload")?;
        Ok(())
    }

    /// Upload the staged content of `stage` if it is dirty.
    async fn flush_stage(&self, stage: Arc<Stage>) -> Result<()> {
        let local_only = {
            let state = self.state.lock().expect("state lock poisoned");
            state.local_only.contains(&stage.ino)
        };
        let (file, len) = {
            let mut meta = stage.meta.lock().expect("stage lock poisoned");
            if local_only {
                // AppleDouble scratch: dropped with the mount, never uploaded.
                meta.dirty = false;
                return Ok(());
            }
            if !meta.dirty || meta.uploading {
                return Ok(());
            }
            meta.uploading = true;
            (stage.file.clone(), meta.len)
        };
        let key = match self.entry(stage.ino) {
            Ok(entry) => self.key(&entry.path),
            Err(_) => {
                stage.meta.lock().expect("stage lock poisoned").uploading = false;
                return Ok(());
            }
        };
        let result = self.upload(&key, &file, len).await;
        {
            let mut meta = stage.meta.lock().expect("stage lock poisoned");
            meta.uploading = false;
            // Writes that landed during the upload keep the stage dirty, so the next ticker
            // tick uploads the newer content.
            if result.is_ok() && meta.len == len {
                meta.dirty = false;
            }
        }
        if result.is_ok() {
            let mut state = self.state.lock().expect("state lock poisoned");
            if let Some(entry) = state.by_id.get_mut(&stage.ino) {
                entry.size = len;
                entry.mtime = now_secs();
                entry.checked = Instant::now();
            }
            debug!("uploaded inode {} to {key} ({len} bytes)", stage.ino);
        }
        result
    }

    async fn flush_all(&self) {
        let stages: Vec<Arc<Stage>> = {
            let stages = self.stages.lock().expect("stages lock poisoned");
            stages.values().filter_map(|cell| cell.get().cloned()).collect()
        };
        for stage in stages {
            if let Err(err) = self.flush_stage(stage.clone()).await {
                warn!("flushing inode {} on shutdown failed: {err:#}", stage.ino);
            }
        }
    }

    /// Periodically upload dirty stages and bound the staging directory size.
    async fn flusher(self: Arc<Self>) {
        let tick = std::cmp::max(
            Duration::from_millis(25),
            std::cmp::min(self.cfg.flush_delay / 2, Duration::from_millis(200)),
        );
        loop {
            tokio::time::sleep(tick).await;
            let due: Vec<Arc<Stage>> = {
                let stages = self.stages.lock().expect("stages lock poisoned");
                stages
                    .values()
                    .filter_map(|cell| cell.get())
                    .filter(|stage| {
                        let meta = stage.meta.lock().expect("stage lock poisoned");
                        meta.dirty && !meta.uploading && meta.last_write.elapsed() >= self.cfg.flush_delay
                    })
                    .cloned()
                    .collect()
            };
            for stage in due {
                let inner = self.clone();
                tokio::spawn(async move {
                    if let Err(err) = inner.flush_stage(stage).await {
                        warn!("background flush failed: {err:#}");
                    }
                });
            }
            self.evict_clean_stages();
        }
    }

    /// Drop fully uploaded staging files once the staging budget is exceeded.
    ///
    /// A later write re-materialises the object into a fresh staging file, so eviction is
    /// transparent. Stages with in-flight I/O (`pins > 0`) are never evicted.
    fn evict_clean_stages(&self) {
        let mut total = 0u64;
        let mut candidates: Vec<(fileid3, Instant)> = Vec::new();
        {
            let stages = self.stages.lock().expect("stages lock poisoned");
            for (ino, cell) in stages.iter() {
                let Some(stage) = cell.get() else { continue };
                let meta = stage.meta.lock().expect("stage lock poisoned");
                total = total.saturating_add(meta.len);
                if !meta.dirty && !meta.uploading && stage.pins.load(Ordering::SeqCst) == 0 {
                    candidates.push((*ino, meta.last_write));
                }
            }
            if total <= self.cfg.max_stage_bytes {
                return;
            }
        }
        candidates.sort_by_key(|(_, last_write)| *last_write);
        for (ino, _) in candidates {
            if total <= self.cfg.max_stage_bytes {
                break;
            }
            let mut stages = self.stages.lock().expect("stages lock poisoned");
            let Some(cell) = stages.get(&ino) else { continue };
            let Some(stage) = cell.get() else { continue };
            if stage.pins.load(Ordering::SeqCst) != 0 {
                continue;
            }
            let len = {
                let meta = stage.meta.lock().expect("stage lock poisoned");
                if meta.dirty || meta.uploading {
                    continue;
                }
                meta.len
            };
            stages.remove(&ino);
            drop(stages);
            total = total.saturating_sub(len);
            let _ = std::fs::remove_file(self.stage_path(ino));
            self.state.lock().expect("state lock poisoned").staged.remove(&ino);
        }
    }
}

// ── NFSFileSystem ───────────────────────────────────────────────────────

impl S3Fs {
    pub async fn new(store: Arc<dyn ObjectStore>, cfg: Config) -> Result<Self> {
        std::fs::create_dir_all(&cfg.staging_dir)
            .with_context(|| format!("creating staging directory {}", cfg.staging_dir.display()))?;
        let cache = ReadCache::new(cfg.cache_dir.clone(), cfg.read_cache_bytes, cfg.read_cache_ttl)?;
        let inner = Arc::new(Inner {
            store,
            cfg,
            state: StdMutex::new(State::new()),
            stages: StdMutex::new(HashMap::new()),
            cache,
            prefetch_gates: StdMutex::new(HashMap::new()),
        });
        tokio::spawn(inner.clone().flusher());
        Ok(Self { inner })
    }

    /// Bytes currently held in the local read cache.
    pub fn cached_bytes(&self) -> u64 {
        self.inner.cache.usage()
    }

    /// Upload every pending write. Call before tearing the mount down.
    pub async fn shutdown(&self) {
        self.inner.flush_all().await;
    }

    /// Create the only inode whose staging file may contain `data` without touching S3.
    async fn stage_and_write(&self, ino: fileid3, offset: u64, data: &[u8]) -> Result<fattr3, nfsstat3> {
        let handle = self.inner.stage_for(ino).await?;
        let stage = handle.stage().clone();
        let file = stage.file.clone();
        let bytes = Bytes::copy_from_slice(data);
        tokio::task::spawn_blocking(move || file.write_all_at(&bytes, offset))
            .await
            .map_err(|_| nfsstat3::NFS3ERR_IO)?
            .map_err(io_err)?;
        let len = {
            let mut meta = stage.meta.lock().expect("stage lock poisoned");
            meta.len = meta.len.max(offset + data.len() as u64);
            meta.dirty = true;
            meta.last_write = Instant::now();
            meta.len
        };
        let mut state = self.inner.state.lock().expect("state lock poisoned");
        if let Some(entry) = state.by_id.get_mut(&ino) {
            entry.size = len;
            entry.mtime = now_secs();
            entry.checked = Instant::now();
            return Ok(fattr_of(ino, entry, self.inner.cfg.uid, self.inner.cfg.gid));
        }
        Err(nfsstat3::NFS3ERR_STALE)
    }

    /// Resize the object behind `ino`, staging whatever has to be preserved.
    async fn truncate(&self, ino: fileid3, new_size: u64) -> Result<(), nfsstat3> {
        let entry = self.inner.entry(ino)?;
        if entry.kind == Kind::Dir {
            return Err(nfsstat3::NFS3ERR_ISDIR);
        }
        let current = {
            let stages = self.inner.stages.lock().expect("stages lock poisoned");
            stages
                .get(&ino)
                .and_then(|cell| cell.get())
                .map(|stage| stage.meta.lock().expect("stage lock poisoned").len)
        };
        if current.unwrap_or(entry.size) == new_size {
            return Ok(());
        }
        let handle = self.inner.stage_for(ino).await?;
        let stage = handle.stage().clone();
        let file = stage.file.clone();
        let size = new_size;
        tokio::task::spawn_blocking(move || file.set_len(size))
            .await
            .map_err(|_| nfsstat3::NFS3ERR_IO)?
            .map_err(io_err)?;
        {
            let mut meta = stage.meta.lock().expect("stage lock poisoned");
            meta.len = new_size;
            meta.dirty = true;
            meta.last_write = Instant::now();
        }
        let mut state = self.inner.state.lock().expect("state lock poisoned");
        if let Some(entry) = state.by_id.get_mut(&ino) {
            entry.size = new_size;
            entry.mtime = now_secs();
            entry.checked = Instant::now();
        }
        Ok(())
    }

    fn fattr(&self, ino: fileid3, entry: &Entry) -> fattr3 {
        fattr_of(ino, entry, self.inner.cfg.uid, self.inner.cfg.gid)
    }

    async fn ensure_name_free(&self, rel: &str) -> Result<(), nfsstat3> {
        let known = {
            let state = self.inner.state.lock().expect("state lock poisoned");
            state.by_path.contains_key(rel)
        };
        if known {
            return Err(nfsstat3::NFS3ERR_EXIST);
        }
        match self.inner.probe(rel).await? {
            Some(_) => Err(nfsstat3::NFS3ERR_EXIST),
            None => Ok(()),
        }
    }
}

#[async_trait]
impl NFSFileSystem for S3Fs {
    fn root_dir(&self) -> fileid3 {
        ROOT_INO
    }

    fn capabilities(&self) -> VFSCapabilities {
        if self.inner.cfg.read_only {
            VFSCapabilities::ReadOnly
        } else {
            VFSCapabilities::ReadWrite
        }
    }

    async fn lookup(&self, dirid: fileid3, filename: &filename3) -> Result<fileid3, nfsstat3> {
        let name = std::str::from_utf8(&filename.0).map_err(|_| nfsstat3::NFS3ERR_NOENT)?;
        if name == "." {
            return Ok(dirid);
        }
        let dir_rel = self.inner.dir_path(dirid)?;
        if name == ".." {
            if dir_rel.is_empty() {
                return Ok(ROOT_INO);
            }
            let (parent, _) = split_parent(&dir_rel);
            return Ok(self.inner.resolve_rel(parent).await?.0);
        }
        if name.is_empty() || name.contains('/') {
            return Err(nfsstat3::NFS3ERR_INVAL);
        }
        if self.inner.is_junk(name) {
            return Err(nfsstat3::NFS3ERR_NOENT);
        }
        let rel = join_rel(&dir_rel, name);
        if self.inner.is_appledouble(name) {
            // Invisible unless this mount is the one that created it; objects of that shape
            // left in the bucket stay hidden.
            let known = {
                let state = self.inner.state.lock().expect("state lock poisoned");
                state.by_path.get(&rel).copied()
            };
            return known.ok_or(nfsstat3::NFS3ERR_NOENT);
        }

        // Cheap path: the parent's cached listing already knows the name.
        let cached_hit = {
            let state = self.inner.state.lock().expect("state lock poisoned");
            state.dirs.get(&dirid).and_then(|listing| {
                let fresh = listing.fetched.elapsed() < self.inner.cfg.dir_ttl;
                fresh
                    .then(|| listing.entries.iter().find(|(entry, _)| entry == name).map(|(_, ino)| *ino))
                    .flatten()
            })
        };
        if let Some(ino) = cached_hit {
            return Ok(ino);
        }

        let (ino, entry) = self.inner.resolve_rel(&rel).await?;
        // Keep any cached listing consistent with the freshly resolved name.
        let mut state = self.inner.state.lock().expect("state lock poisoned");
        if entry.kind == Kind::Dir {
            state.pending_dirs.remove(&rel);
        }
        if let Some(listing) = state.dirs.get_mut(&dirid) {
            let known = listing.entries.iter().any(|(entry, _)| entry == name);
            if !known {
                listing.entries.push((name.to_string(), ino));
                listing
                    .entries
                    .sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
            }
        }
        Ok(ino)
    }

    async fn getattr(&self, id: fileid3) -> Result<fattr3, nfsstat3> {
        let mut entry = self.inner.entry(id)?;
        let (staged, local_only) = {
            let state = self.inner.state.lock().expect("state lock poisoned");
            (state.staged.contains(&id), state.local_only.contains(&id))
        };
        if entry.kind == Kind::File
            && !staged
            && !local_only
            && entry.checked.elapsed() >= self.inner.cfg.dir_ttl
        {
            match self.inner.store.head(&self.inner.key(&entry.path)).await {
                Ok(meta) => {
                    entry.size = meta.size;
                    entry.mtime = meta.last_modified.timestamp().max(0) as u64;
                    entry.checked = Instant::now();
                    let mut state = self.inner.state.lock().expect("state lock poisoned");
                    if let Some(cached) = state.by_id.get_mut(&id) {
                        cached.size = entry.size;
                        cached.mtime = entry.mtime;
                        cached.checked = entry.checked;
                    }
                }
                Err(ObjectStoreError::NotFound { .. }) => return Err(nfsstat3::NFS3ERR_NOENT),
                Err(err) => {
                    debug!("getattr refresh for {} failed: {err}", entry.path);
                    entry.checked = Instant::now();
                }
            }
        }
        Ok(self.fattr(id, &entry))
    }

    async fn setattr(&self, id: fileid3, setattr: sattr3) -> Result<fattr3, nfsstat3> {
        if self.inner.cfg.read_only {
            return Err(nfsstat3::NFS3ERR_ROFS);
        }
        let entry = self.inner.entry(id)?;
        if entry.kind == Kind::Dir {
            // Only timestamps and permissions can be set on the synthetic directories.
            if !matches!(setattr.size, set_size3::Void) {
                return Err(nfsstat3::NFS3ERR_ISDIR);
            }
        } else if let set_size3::size(size) = setattr.size {
            self.truncate(id, size).await?;
        }
        let now = now_secs();
        let mut state = self.inner.state.lock().expect("state lock poisoned");
        if let Some(entry) = state.by_id.get_mut(&id) {
            if let nfsserve::nfs::set_mode3::mode(mode) = setattr.mode {
                entry.mode = mode & 0o7777;
            }
            let _ = setattr.uid;
            let _ = setattr.gid;
            match setattr.mtime {
                set_mtime::SET_TO_SERVER_TIME => entry.mtime = now,
                set_mtime::SET_TO_CLIENT_TIME(time) => entry.mtime = time.seconds as u64,
                set_mtime::DONT_CHANGE => {}
            }
            match setattr.atime {
                set_atime::SET_TO_SERVER_TIME => entry.atime = now,
                set_atime::SET_TO_CLIENT_TIME(time) => entry.atime = time.seconds as u64,
                set_atime::DONT_CHANGE => {}
            }
            entry.checked = Instant::now();
            return Ok(fattr_of(id, entry, self.inner.cfg.uid, self.inner.cfg.gid));
        }
        Err(nfsstat3::NFS3ERR_STALE)
    }

    async fn read(&self, id: fileid3, offset: u64, count: u32) -> Result<(Vec<u8>, bool), nfsstat3> {
        let entry = self.inner.entry(id)?;
        if entry.kind == Kind::Dir {
            return Err(nfsstat3::NFS3ERR_ISDIR);
        }
        if let Some(handle) = self.inner.acquire_stage(id) {
            let stage = handle.stage().clone();
            let len = stage.meta.lock().expect("stage lock poisoned").len;
            if offset >= len || count == 0 {
                return Ok((Vec::new(), true));
            }
            let want = std::cmp::min(count as u64, len - offset) as usize;
            let file = stage.file.clone();
            let data = tokio::task::spawn_blocking(move || {
                let mut buf = vec![0u8; want];
                file.read_exact_at(&mut buf, offset)?;
                Ok::<Vec<u8>, std::io::Error>(buf)
            })
            .await
            .map_err(|_| nfsstat3::NFS3ERR_IO)?
            .map_err(io_err)?;
            return Ok((data, offset + want as u64 >= len));
        }
        if offset >= entry.size || count == 0 {
            return Ok((Vec::new(), true));
        }
        let wanted = std::cmp::min(offset + count as u64, entry.size);

        // Locality: an object small enough to cache is fetched once, whole, and served from
        // the local copy until it changes or expires.
        if let Some((file, len)) = self.inner.cache.get(id, entry.size, entry.mtime) {
            return read_cached(file, len, offset, wanted).await;
        }
        let cacheable = self.inner.cfg.prefetch_bytes > 0
            && entry.size <= self.inner.cfg.prefetch_bytes
            && self.inner.cache.enabled();
        if cacheable {
            let gate = self.inner.prefetch_gate(id);
            let _guard = gate.lock().await;
            if let Some((file, len)) = self.inner.cache.get(id, entry.size, entry.mtime) {
                return read_cached(file, len, offset, wanted).await;
            }
            let key = self.inner.key(&entry.path);
            match self.inner.prefetch(id, &key, entry.size, entry.mtime).await {
                Ok((file, len)) => return read_cached(file, len, offset, wanted).await,
                Err(err) => warn!("caching {} failed, streaming instead: {err:#}", entry.path),
            }
        }

        let data = self
            .inner
            .store
            .get_range(&self.inner.key(&entry.path), offset..wanted)
            .await
            .map_err(nfs_err)?;
        Ok((data.to_vec(), wanted >= entry.size))
    }

    async fn write(&self, id: fileid3, offset: u64, data: &[u8]) -> Result<fattr3, nfsstat3> {
        if self.inner.cfg.read_only {
            return Err(nfsstat3::NFS3ERR_ROFS);
        }
        let entry = self.inner.entry(id)?;
        if entry.kind == Kind::Dir {
            return Err(nfsstat3::NFS3ERR_ISDIR);
        }
        self.stage_and_write(id, offset, data).await
    }

    async fn create(&self, dirid: fileid3, filename: &filename3, attr: sattr3) -> Result<(fileid3, fattr3), nfsstat3> {
        if self.inner.cfg.read_only {
            return Err(nfsstat3::NFS3ERR_ROFS);
        }
        let name = std::str::from_utf8(&filename.0).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;
        if self.inner.is_junk(name) {
            return Err(nfsstat3::NFS3ERR_ACCES);
        }
        let local_only = self.inner.is_appledouble(name);
        let dir_rel = self.inner.dir_path(dirid)?;
        let rel = join_rel(&dir_rel, name);

        // AppleDouble siblings are scratch space: register the inode, publish nothing.
        if local_only {
            let ino = {
                let mut state = self.inner.state.lock().expect("state lock poisoned");
                let ino = state.intern(rel, Kind::File, 0, now_secs());
                state.local_only.insert(ino);
                ino
            };
            let entry = self.inner.entry(ino)?;
            return Ok((ino, self.fattr(ino, &entry)));
        }

        // UNCHECKED create on an existing file must not truncate: the client sends an
        // explicit SETATTR(size=0) when it wants O_TRUNC.
        let existing = if self.inner.listing_says_absent(dirid, name, &rel) {
            None
        } else {
            self.inner.resolve_rel(&rel).await.ok()
        };
        if let Some((ino, entry)) = existing {
            self.inner.invalidate_dir(dirid);
            return Ok((ino, self.fattr(ino, &entry)));
        }

        self.inner
            .store
            .put(&self.inner.key(&rel), PutPayload::new())
            .await
            .map_err(nfs_err)?;
        let mode = match attr.mode {
            nfsserve::nfs::set_mode3::mode(mode) => mode & 0o7777,
            nfsserve::nfs::set_mode3::Void => FILE_MODE,
        };
        let ino = {
            let mut state = self.inner.state.lock().expect("state lock poisoned");
            let ino = state.intern(rel, Kind::File, 0, now_secs());
            if let Some(entry) = state.by_id.get_mut(&ino) {
                entry.mode = mode;
            }
            ino
        };
        self.inner.invalidate_dir(dirid);
        let entry = self.inner.entry(ino)?;
        Ok((ino, self.fattr(ino, &entry)))
    }

    async fn create_exclusive(&self, dirid: fileid3, filename: &filename3) -> Result<fileid3, nfsstat3> {
        if self.inner.cfg.read_only {
            return Err(nfsstat3::NFS3ERR_ROFS);
        }
        let name = std::str::from_utf8(&filename.0).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;
        if self.inner.is_junk(name) {
            return Err(nfsstat3::NFS3ERR_ACCES);
        }
        let local_only = self.inner.is_appledouble(name);
        let dir_rel = self.inner.dir_path(dirid)?;
        let rel = join_rel(&dir_rel, name);
        if local_only {
            let mut state = self.inner.state.lock().expect("state lock poisoned");
            if state.by_path.contains_key(&rel) {
                return Err(nfsstat3::NFS3ERR_EXIST);
            }
            let ino = state.intern(rel, Kind::File, 0, now_secs());
            state.local_only.insert(ino);
            return Ok(ino);
        }
        let key = self.inner.key(&rel);
        let options = PutOptions {
            mode: PutMode::Create,
            ..Default::default()
        };
        match self.inner.store.put_opts(&key, PutPayload::new(), options).await {
            Ok(_) => {}
            Err(ObjectStoreError::AlreadyExists { .. } | ObjectStoreError::Precondition { .. }) => {
                return Err(nfsstat3::NFS3ERR_EXIST);
            }
            // Stores without conditional PUT: fall back to a plain existence check.
            Err(ObjectStoreError::NotSupported { .. } | ObjectStoreError::NotImplemented { .. }) => {
                self.ensure_name_free(&rel).await?;
                self.inner
                    .store
                    .put(&key, PutPayload::new())
                    .await
                    .map_err(nfs_err)?;
            }
            Err(err) => return Err(nfs_err(err)),
        }
        let ino = {
            let mut state = self.inner.state.lock().expect("state lock poisoned");
            state.intern(rel, Kind::File, 0, now_secs())
        };
        self.inner.invalidate_dir(dirid);
        Ok(ino)
    }

    async fn mkdir(&self, dirid: fileid3, dirname: &filename3) -> Result<(fileid3, fattr3), nfsstat3> {
        if self.inner.cfg.read_only {
            return Err(nfsstat3::NFS3ERR_ROFS);
        }
        let name = std::str::from_utf8(&dirname.0).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;
        if self.inner.is_junk(name) || self.inner.is_appledouble(name) {
            return Err(nfsstat3::NFS3ERR_ACCES);
        }
        let dir_rel = self.inner.dir_path(dirid)?;
        let rel = join_rel(&dir_rel, name);
        if !self.inner.listing_says_absent(dirid, name, &rel) {
            self.ensure_name_free(&rel).await?;
        }
        let ino = {
            let mut state = self.inner.state.lock().expect("state lock poisoned");
            state.pending_dirs.insert(rel.clone());
            state.intern(rel, Kind::Dir, DIR_SIZE, now_secs())
        };
        self.inner.invalidate_dir(dirid);
        let entry = self.inner.entry(ino)?;
        Ok((ino, self.fattr(ino, &entry)))
    }

    async fn remove(&self, dirid: fileid3, filename: &filename3) -> Result<(), nfsstat3> {
        if self.inner.cfg.read_only {
            return Err(nfsstat3::NFS3ERR_ROFS);
        }
        let name = std::str::from_utf8(&filename.0).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;
        if self.inner.is_junk(name) {
            // Nothing of ours to delete; reporting success keeps `rm` quiet.
            return Ok(());
        }
        let dir_rel = self.inner.dir_path(dirid)?;
        let rel = join_rel(&dir_rel, name);
        let local_only = {
            let state = self.inner.state.lock().expect("state lock poisoned");
            state
                .by_path
                .get(&rel)
                .is_some_and(|ino| state.local_only.contains(ino))
        };
        if local_only {
            let ino = {
                let mut state = self.inner.state.lock().expect("state lock poisoned");
                let ino = state.by_path.get(&rel).copied();
                if let Some(ino) = ino {
                    state.forget(ino);
                }
                ino
            };
            if let Some(ino) = ino {
                self.inner.stages.lock().expect("stages lock poisoned").remove(&ino);
                let _ = std::fs::remove_file(self.inner.stage_path(ino));
            }
            return Ok(());
        }
        let (ino, entry) = self.inner.resolve_rel(&rel).await?;

        if entry.kind == Kind::Dir {
            let children = self.inner.list_children(&rel).await?;
            let has_pending_children = {
                let state = self.inner.state.lock().expect("state lock poisoned");
                state
                    .pending_dirs
                    .iter()
                    .any(|path| path != &rel && split_parent(path).0 == rel)
            };
            if !children.is_empty() || has_pending_children {
                return Err(nfsstat3::NFS3ERR_NOTEMPTY);
            }
            let mut state = self.inner.state.lock().expect("state lock poisoned");
            state.pending_dirs.remove(&rel);
            state.forget(ino);
        } else {
            self.inner
                .store
                .delete(&self.inner.key(&rel))
                .await
                .map_err(nfs_err)?;
            {
                let mut stages = self.inner.stages.lock().expect("stages lock poisoned");
                stages.remove(&ino);
            }
            let _ = std::fs::remove_file(self.inner.stage_path(ino));
            self.inner
                .state
                .lock()
                .expect("state lock poisoned")
                .forget(ino);
        }
        self.inner.invalidate_dir(dirid);
        Ok(())
    }

    async fn rename(
        &self,
        from_dirid: fileid3,
        from_filename: &filename3,
        to_dirid: fileid3,
        to_filename: &filename3,
    ) -> Result<(), nfsstat3> {
        if self.inner.cfg.read_only {
            return Err(nfsstat3::NFS3ERR_ROFS);
        }
        let from_name = std::str::from_utf8(&from_filename.0).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;
        let to_name = std::str::from_utf8(&to_filename.0).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;
        if self.inner.is_junk(from_name) || self.inner.is_junk(to_name) {
            return Err(nfsstat3::NFS3ERR_ACCES);
        }
        let from_local = self.inner.is_appledouble(from_name);
        let to_local = self.inner.is_appledouble(to_name);
        if from_local != to_local {
            // Moving a real object into (or out of) the AppleDouble namespace would either
            // lose data or smuggle a never-uploaded file into the bucket.
            return Err(nfsstat3::NFS3ERR_ACCES);
        }
        let from_dir = self.inner.dir_path(from_dirid)?;
        let to_dir = self.inner.dir_path(to_dirid)?;
        let from_rel = join_rel(&from_dir, from_name);
        let to_rel = join_rel(&to_dir, to_name);
        if from_rel == to_rel {
            return Ok(());
        }
        if from_local && to_local {
            let mut state = self.inner.state.lock().expect("state lock poisoned");
            let Some(ino) = state.by_path.remove(&from_rel) else {
                return Err(nfsstat3::NFS3ERR_NOENT);
            };
            if let Some(displaced) = state.by_path.insert(to_rel.clone(), ino) {
                state.forget(displaced);
            }
            if let Some(entry) = state.by_id.get_mut(&ino) {
                entry.path = to_rel;
            }
            state.dirs.remove(&from_dirid);
            state.dirs.remove(&to_dirid);
            return Ok(());
        }
        let (ino, entry) = self.inner.resolve_rel(&from_rel).await?;
        if entry.kind == Kind::Dir && (to_rel == from_rel || to_rel.starts_with(&format!("{from_rel}/"))) {
            return Err(nfsstat3::NFS3ERR_INVAL);
        }

        if entry.kind == Kind::File {
            // Upload first so the copy is not racing the debounced flush.
            if let Some(handle) = self.inner.acquire_stage(ino) {
                let stage = handle.stage().clone();
                drop(handle);
                self.inner.flush_stage(stage).await.map_err(|_| nfsstat3::NFS3ERR_IO)?;
            }
            if let Ok((dst_ino, dst_entry)) = self.inner.resolve_rel(&to_rel).await {
                let replaces_directory = dst_entry.kind == Kind::Dir;
                if replaces_directory {
                    let children = self.inner.list_children(&to_rel).await?;
                    if !children.is_empty() {
                        return Err(nfsstat3::NFS3ERR_NOTEMPTY);
                    }
                    self.inner
                        .state
                        .lock()
                        .expect("state lock poisoned")
                        .forget(dst_ino);
                }
            }
            self.inner
                .store
                .copy(&self.inner.key(&from_rel), &self.inner.key(&to_rel))
                .await
                .map_err(nfs_err)?;
            self.inner
                .store
                .delete(&self.inner.key(&from_rel))
                .await
                .map_err(nfs_err)?;
            let mut state = self.inner.state.lock().expect("state lock poisoned");
            state.by_path.remove(&from_rel);
            if let Some(target) = state.by_path.remove(&to_rel) {
                state.forget(target);
            }
            if let Some(entry) = state.by_id.get_mut(&ino) {
                entry.path = to_rel.clone();
            }
            state.by_path.insert(to_rel, ino);
        } else {
            // Directory rename: S3 has no atomic prefix move, so copy then delete each key.
            let from_prefix = format!("{}/", self.inner.abs(&from_rel));
            let to_prefix = format!("{}/", self.inner.abs(&to_rel));
            let mut keys: Vec<ObjPath> = Vec::new();
            let mut stream = self.inner.store.list(Some(&ObjPath::from(from_prefix.clone())));
            while let Some(item) = stream.next().await {
                keys.push(item.map_err(nfs_err)?.location);
            }
            // Anything staged below the directory must land before the copy.
            self.inner.flush_all().await;
            for key in &keys {
                let raw = key.to_string();
                let Some(suffix) = raw.strip_prefix(&from_prefix) else {
                    continue;
                };
                let target = ObjPath::from(format!("{to_prefix}{suffix}"));
                self.inner.store.copy(key, &target).await.map_err(nfs_err)?;
            }
            for key in &keys {
                self.inner.store.delete(key).await.map_err(nfs_err)?;
            }
            let mut state = self.inner.state.lock().expect("state lock poisoned");
            state.rewrite_paths(&from_rel, &to_rel);
        }
        self.inner.invalidate_dir(from_dirid);
        self.inner.invalidate_dir(to_dirid);
        Ok(())
    }

    async fn readdir(&self, dirid: fileid3, start_after: fileid3, max_entries: usize) -> Result<ReadDirResult, nfsstat3> {
        let dir_rel = self.inner.dir_path(dirid)?;
        let entries = self.inner.dir_entries(dirid, &dir_rel, false).await?;
        let start = if start_after == 0 {
            0
        } else {
            entries
                .iter()
                .position(|(_, ino)| *ino == start_after)
                .map(|index| index + 1)
                .unwrap_or(0)
        };
        let limit = max_entries.max(1);
        let slice = &entries[std::cmp::min(start, entries.len())..];
        let state = self.inner.state.lock().expect("state lock poisoned");
        let mut result = ReadDirResult {
            entries: Vec::with_capacity(std::cmp::min(slice.len(), limit)),
            end: false,
        };
        for (name, ino) in slice.iter().take(limit) {
            let Some(entry) = state.by_id.get(ino) else { continue };
            result.entries.push(DirEntry {
                fileid: *ino,
                name: name.as_bytes().to_vec().into(),
                attr: fattr_of(*ino, entry, self.inner.cfg.uid, self.inner.cfg.gid),
            });
        }
        result.end = start + result.entries.len() >= entries.len();
        Ok(result)
    }

    async fn symlink(
        &self,
        _dirid: fileid3,
        _linkname: &filename3,
        _symlink: &nfspath3,
        _attr: &sattr3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        Err(nfsstat3::NFS3ERR_NOTSUPP)
    }

    async fn readlink(&self, _id: fileid3) -> Result<nfspath3, nfsstat3> {
        Err(nfsstat3::NFS3ERR_NOTSUPP)
    }

    async fn fsinfo(&self, root_fileid: fileid3) -> Result<fsinfo3, nfsstat3> {
        let dir_attr = match self.getattr(root_fileid).await {
            Ok(attr) => post_op_attr::attributes(attr),
            Err(_) => post_op_attr::Void,
        };
        Ok(fsinfo3 {
            obj_attributes: dir_attr,
            rtmax: 1024 * 1024,
            rtpref: 1024 * 1024,
            rtmult: 1024 * 1024,
            wtmax: 1024 * 1024,
            wtpref: 1024 * 1024,
            wtmult: 1024 * 1024,
            dtpref: 64 * 1024,
            maxfilesize: 5 * 1024 * 1024 * 1024 * 1024,
            time_delta: nfstime3 {
                seconds: 0,
                nseconds: 1000000,
            },
            // No hard links and no symlinks: say so, so clients do not try.
            properties: FSF_HOMOGENEOUS | FSF_CANSETTIME,
        })
    }
}
