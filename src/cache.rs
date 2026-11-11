//! Local read cache for object contents.
//!
//! Reading an object from the store on every `read` call turns a mounted tree into a
//! network filesystem with no locality: `grep` over one file costs one request per
//! megabyte, and a build tree re-reads the same headers over and over. Objects up to a
//! size limit are therefore fetched whole, once, into a local file and served from there.
//!
//! Entries are keyed by inode and validated against the size/mtime the VFS last observed
//! for that path, plus an age limit (`ttl`), so an object rewritten by another client is
//! re-fetched instead of being served stale. Writes never populate the cache — they go
//! through the staging file, which stays authoritative — and creating or dropping a stage
//! invalidates the entry.

use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use nfsserve::nfs::fileid3;

#[derive(Debug)]
struct CacheEntry {
    path: PathBuf,
    len: u64,
    /// Object size/mtime when the bytes were fetched.
    size: u64,
    mtime: u64,
    fetched: Instant,
    used: Instant,
}

#[derive(Debug)]
pub struct ReadCache {
    dir: PathBuf,
    max_bytes: u64,
    ttl: Duration,
    entries: Mutex<HashMap<fileid3, CacheEntry>>,
}

impl ReadCache {
    pub fn new(dir: PathBuf, max_bytes: u64, ttl: Duration) -> Result<Self> {
        std::fs::create_dir_all(&dir).with_context(|| format!("creating read cache {}", dir.display()))?;
        // Entries are keyed by inode, so copies left by an earlier process can never be
        // reused; clear them instead of letting the directory grow across restarts.
        for entry in std::fs::read_dir(&dir).with_context(|| format!("reading read cache {}", dir.display()))? {
            let path = entry?.path();
            if path.extension().is_some_and(|ext| ext == "cache" || ext == "part") {
                let _ = std::fs::remove_file(&path);
            }
        }
        Ok(Self {
            dir,
            max_bytes,
            ttl,
            entries: Mutex::new(HashMap::new()),
        })
    }

    pub fn enabled(&self) -> bool {
        self.max_bytes > 0 && !self.ttl.is_zero()
    }

    /// Cached bytes for `ino`, provided they still match the observed object metadata.
    pub fn get(&self, ino: fileid3, size: u64, mtime: u64) -> Option<(File, u64)> {
        if !self.enabled() {
            return None;
        }
        let mut entries = self.entries.lock().expect("cache lock poisoned");
        let entry = entries.get_mut(&ino)?;
        if entry.fetched.elapsed() >= self.ttl || entry.size != size || entry.mtime != mtime {
            return None;
        }
        entry.used = Instant::now();
        let file = File::open(&entry.path).ok()?;
        Some((file, entry.len))
    }

    /// Move a freshly downloaded object into the cache and return a readable handle.
    pub fn insert(&self, ino: fileid3, part: &Path, len: u64, size: u64, mtime: u64) -> Result<(File, u64)> {
        let file = File::open(part).with_context(|| format!("opening downloaded object {}", part.display()))?;
        if !self.enabled() {
            return Ok((file, len));
        }
        let final_path = self.entry_path(ino);
        std::fs::rename(part, &final_path)
            .with_context(|| format!("installing read cache entry {}", final_path.display()))?;
        let now = Instant::now();
        {
            let mut entries = self.entries.lock().expect("cache lock poisoned");
            entries.insert(
                ino,
                CacheEntry {
                    path: final_path,
                    len,
                    size,
                    mtime,
                    fetched: Instant::now(),
                    used: now,
                },
            );
        }
        self.evict();
        let file = File::open(self.entry_path(ino)).context("reopening cache entry")?;
        Ok((file, len))
    }

    /// Drop the entry for `ino`; the next read fetches the object again.
    pub fn invalidate(&self, ino: fileid3) {
        let entry = self.entries.lock().expect("cache lock poisoned").remove(&ino);
        if let Some(entry) = entry {
            let _ = std::fs::remove_file(entry.path);
        }
    }

    /// Total cached bytes, for budget accounting and diagnostics.
    pub fn usage(&self) -> u64 {
        self.entries
            .lock()
            .expect("cache lock poisoned")
            .values()
            .map(|entry| entry.len)
            .sum()
    }

    /// Scratch path used while an object is being downloaded.
    pub fn part_path(&self, ino: fileid3) -> PathBuf {
        self.dir.join(format!("{ino}.part"))
    }

    fn entry_path(&self, ino: fileid3) -> PathBuf {
        self.dir.join(format!("{ino}.cache"))
    }

    /// Drop least recently used entries until the cache fits its budget again.
    fn evict(&self) {
        let mut entries = self.entries.lock().expect("cache lock poisoned");
        let mut total: u64 = entries.values().map(|entry| entry.len).sum();
        if total <= self.max_bytes {
            return;
        }
        let mut order: Vec<(fileid3, Instant)> = entries.iter().map(|(ino, e)| (*ino, e.used)).collect();
        order.sort_by_key(|(_, used)| *used);
        for (ino, _) in order {
            if total <= self.max_bytes {
                break;
            }
            if let Some(entry) = entries.remove(&ino) {
                total = total.saturating_sub(entry.len);
                let _ = std::fs::remove_file(entry.path);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_file(dir: &Path, ino: fileid3, bytes: &[u8]) -> PathBuf {
        let part = dir.join(format!("{ino}.part"));
        std::fs::write(&part, bytes).unwrap();
        part
    }

    #[test]
    fn get_requires_matching_metadata_and_fresh_age() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ReadCache::new(dir.path().join("read"), 1024, Duration::from_secs(60)).unwrap();
        let part = write_file(dir.path(), 7, b"payload");
        cache.insert(7, &part, 7, 7, 100).unwrap();

        assert!(cache.get(7, 7, 100).is_some(), "fresh entry matches");
        assert!(cache.get(7, 8, 100).is_none(), "size change invalidates");
        assert!(cache.get(7, 7, 101).is_none(), "mtime change invalidates");
        assert!(cache.get(8, 7, 100).is_none(), "unknown inode misses");
    }

    #[test]
    fn entries_expire_after_the_ttl() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ReadCache::new(dir.path().join("read"), 1024, Duration::from_millis(1)).unwrap();
        let part = write_file(dir.path(), 3, b"x");
        cache.insert(3, &part, 1, 1, 0).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        assert!(cache.get(3, 1, 0).is_none());
    }

    #[test]
    fn eviction_drops_the_least_recently_used_entry() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ReadCache::new(dir.path().join("read"), 6, Duration::from_secs(60)).unwrap();
        for ino in 1..=2 {
            let part = write_file(dir.path(), ino, b"abc");
            cache.insert(ino, &part, 3, 3, 0).unwrap();
            std::thread::sleep(Duration::from_millis(2));
        }
        let _ = cache.get(1, 3, 0);
        let part = write_file(dir.path(), 3, b"abc");
        cache.insert(3, &part, 3, 3, 0).unwrap();

        assert!(cache.get(2, 3, 0).is_none(), "oldest entry was evicted");
        assert!(cache.get(1, 3, 0).is_some());
        assert!(cache.get(3, 3, 0).is_some());
        assert!(cache.usage() <= 6);
    }

    #[test]
    fn leftover_files_from_an_earlier_process_are_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let read = dir.path().join("read");
        std::fs::create_dir_all(&read).unwrap();
        std::fs::write(read.join("2.cache"), b"stale").unwrap();
        std::fs::write(read.join("3.part"), b"half").unwrap();
        std::fs::write(read.join("keep.txt"), b"other").unwrap();

        let cache = ReadCache::new(read.clone(), 1024, Duration::from_secs(60)).unwrap();
        assert!(cache.get(2, 5, 0).is_none());
        assert!(!read.join("2.cache").exists());
        assert!(!read.join("3.part").exists());
        assert!(read.join("keep.txt").exists(), "unrelated files are left alone");
    }

    #[test]
    fn disabled_cache_still_serves_the_download() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ReadCache::new(dir.path().join("read"), 0, Duration::from_secs(60)).unwrap();
        let part = write_file(dir.path(), 9, b"abc");
        let (file, len) = cache.insert(9, &part, 3, 3, 0).unwrap();
        assert_eq!(len, 3);
        assert!(file.metadata().unwrap().is_file());
        assert!(cache.get(9, 3, 0).is_none());
        assert_eq!(cache.usage(), 0);
    }
}
