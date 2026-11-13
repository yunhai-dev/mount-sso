//! Crash-safe hand-off between the mounting daemon and `mount-sso umount`.
//!
//! The daemon is the only process that knows whether the final flush succeeded, and
//! `umount` is a separate invocation. They therefore share one small record per mount
//! point: the daemon publishes its pid at startup and its flush result just before it
//! exits, and `umount` waits for that pid to disappear and then reports the result.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::vfs::FlushReport;

/// Directory holding the per-mount records, keyed by a hash of the mount point.
///
/// `$HOME` rather than the temp directory: the daemon and `umount` are separate
/// invocations, and `TMPDIR` is not guaranteed to be identical between them.
fn state_dir() -> PathBuf {
    let base = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("Library/Caches/mount-sso")
}

fn record_path(mount_point: &Path) -> PathBuf {
    // FNV-1a over the canonical path: stable across processes, no extra dependency.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in mount_point.to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    state_dir().join(format!("{hash:016x}.state"))
}

/// Record left behind by a mount, so `umount` can wait for the flush and report it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MountRecord {
    pub pid: Option<u32>,
    pub mount_point: Option<PathBuf>,
    pub result: Option<FlushReport>,
}

impl MountRecord {
    fn parse(text: &str) -> Self {
        let mut record = MountRecord::default();
        for line in text.lines() {
            let Some((key, value)) = line.split_once(' ') else { continue };
            match key {
                "pid" => record.pid = value.trim().parse().ok(),
                "mount" => record.mount_point = Some(PathBuf::from(value.trim())),
                "result" => {
                    let mut parts = value.split(' ');
                    let status = parts.next().unwrap_or_default();
                    let count = parts.next().and_then(|n| n.parse::<usize>().ok()).unwrap_or(0);
                    match status {
                        "ok" => {
                            record.result = Some(FlushReport {
                                uploaded: count,
                                failures: Vec::new(),
                            });
                        }
                        "failed" => {
                            // The failure lines follow; keep the count so a truncated record
                            // still reports an incomplete sync.
                            record.result = Some(FlushReport {
                                uploaded: 0,
                                failures: vec![("(unknown file)".into(), "upload failed".into()); count],
                            });
                        }
                        _ => {}
                    }
                }
                "failed" if !value.is_empty() => {
                    let (path, err) = value.split_once('\t').unwrap_or((value, "upload failed"));
                    if let Some(report) = record.result.as_mut() {
                        // Replace the placeholders as real entries arrive.
                        if report.failures.len() == 1 && report.failures[0].0 == "(unknown file)" {
                            report.failures.clear();
                        }
                        report.failures.push((path.to_string(), err.to_string()));
                    }
                }
                _ => {}
            }
        }
        record
    }
}

/// Owns the record for one mount point.
pub struct StateFile {
    path: PathBuf,
    mount_point: PathBuf,
}

impl StateFile {
    pub fn new(mount_point: &Path) -> Self {
        Self {
            path: record_path(mount_point),
            mount_point: mount_point.to_path_buf(),
        }
    }

    /// Publishes the pid and clears any result left by an earlier run.
    pub fn publish_pid(&self) -> Result<()> {
        std::fs::create_dir_all(state_dir()).context("creating the mount-sso state directory")?;
        std::fs::write(&self.path, format!("pid {}\nmount {}\n", std::process::id(), self.mount_point.display()))
            .with_context(|| format!("writing {}", self.path.display()))
    }

    /// Records the flush outcome; the file stays behind for `umount` to read.
    pub fn publish_result(&self, report: &FlushReport) -> Result<()> {
        let mut text = format!(
            "pid {}\nmount {}\nresult {}\n",
            std::process::id(),
            self.mount_point.display(),
            if report.failures.is_empty() {
                format!("ok {}", report.uploaded)
            } else {
                format!("failed {}", report.failures.len())
            }
        );
        for (path, err) in &report.failures {
            text.push_str(&format!("failed {path}\t{err}\n"));
        }
        std::fs::create_dir_all(state_dir()).context("creating the mount-sso state directory")?;
        std::fs::write(&self.path, text).with_context(|| format!("writing {}", self.path.display()))
    }

    pub fn read(&self) -> MountRecord {
        std::fs::read_to_string(&self.path)
            .map(|text| MountRecord::parse(&text))
            .unwrap_or_default()
    }

    pub fn remove(&self) {
        let _ = std::fs::remove_file(&self.path);
    }

    /// Waits until the daemon that owns this record has exited.
    ///
    /// Returns `false` on timeout or when the record has no usable pid.
    pub fn wait_for_daemon(&self, timeout: Duration) -> bool {
        let Some(pid) = self.read().pid else { return false };
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if !process_alive(pid) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }
}

fn process_alive(pid: u32) -> bool {
    // SAFETY: `kill` with signal 0 only probes for existence.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let mount = dir.path().join("mnt");
        let state = StateFile::new(&mount);
        state.publish_pid().unwrap();
        let record = state.read();
        assert_eq!(record.pid, Some(std::process::id()));
        assert_eq!(record.mount_point.as_deref(), Some(mount.as_path()));
        assert!(record.result.is_none());

        state
            .publish_result(&FlushReport {
                uploaded: 3,
                failures: vec![("a/b.txt".into(), "denied".into())],
            })
            .unwrap();
        let record = state.read();
        assert!(record.result.is_some());
        state.remove();
        assert!(state.read().pid.is_none());
    }

    #[test]
    fn waits_until_the_recorded_process_exits() {
        let dir = tempfile::tempdir().unwrap();
        let mount = dir.path().join("mnt");
        let state = StateFile::new(&mount);
        std::fs::create_dir_all(state_dir()).unwrap();
        let mut child = std::process::Command::new("/bin/sleep").arg("0.3").spawn().unwrap();
        std::fs::write(
            &state.path,
            format!("pid {}\nmount {}\n", child.id(), mount.display()),
        )
        .unwrap();
        // A zombie still answers `kill(pid, 0)`, so reap it like a shell would.
        std::thread::spawn(move || {
            let _ = child.wait();
        });

        let started = Instant::now();
        assert!(state.wait_for_daemon(Duration::from_secs(5)), "wait must observe the exit");
        assert!(started.elapsed() >= Duration::from_millis(200), "wait must not return early");
        state.remove();
    }

    #[test]
    fn waits_for_a_pid_that_is_already_gone() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateFile::new(&dir.path().join("mnt"));
        state.publish_pid().unwrap();
        // Our own pid is alive, so a zero timeout gives up.
        assert!(!state.wait_for_daemon(Duration::from_millis(1)));
        state.remove();
    }
}
