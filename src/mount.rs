//! macOS NFSv3 mount/unmount plumbing.
//!
//! The filesystem is served by a loopback NFSv3 server; macOS only exposes it through
//! `mount_nfs`, which requires root privileges.

use std::ffi::CString;
use std::io;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

const MOUNT_NFS: &str = "/sbin/mount_nfs";
const UMOUNT: &str = "/sbin/umount";

/// NFS mount options used for the loopback server.
///
/// * `locallocks` — locking is handled by the local client, the server has no lock manager.
/// * `hard` — never return EIO on server hiccups; keep retrying instead of losing writes.
/// * `actimeo` — attribute cache window, aligned with the server-side listing TTL.
pub fn mount_options(port: u16, actimeo_secs: u64, read_only: bool) -> String {
    let mut options = format!(
        "locallocks,vers=3,tcp,rsize=1048576,actimeo={},port={port},mountport={port},hard",
        actimeo_secs.max(1)
    );
    if read_only {
        options.push_str(",rdonly");
    } else {
        options.push_str(",wsize=1048576");
    }
    options
}

pub fn is_root() -> bool {
    // SAFETY: `geteuid` has no preconditions.
    unsafe { libc::geteuid() == 0 }
}

/// True when `path` is currently mounted from an NFS server.
pub fn is_mounted(path: &Path) -> bool {
    let Ok(raw) = CString::new(path.to_string_lossy().as_bytes()) else {
        return false;
    };
    // SAFETY: `statfs` only writes into the provided buffer.
    unsafe {
        let mut fs: libc::statfs = std::mem::zeroed();
        if libc::statfs(raw.as_ptr(), &mut fs) != 0 {
            return false;
        }
        let fstype = std::ffi::CStr::from_ptr(fs.f_fstypename.as_ptr());
        fstype.to_bytes() == b"nfs"
    }
}

/// Mounts the loopback NFS export at `mount_point`, escalating to `sudo` when needed.
pub fn mount_nfs(port: u16, mount_point: &Path, actimeo_secs: u64, read_only: bool) -> Result<()> {
    if !Path::new(MOUNT_NFS).exists() {
        bail!("{MOUNT_NFS} not found; this tool only supports macOS");
    }
    let options = mount_options(port, actimeo_secs, read_only);
    let target = mount_point.to_string_lossy().to_string();
    let args = ["-o", options.as_str(), "127.0.0.1:/", target.as_str()];

    if is_root() {
        return run_checked(MOUNT_NFS, &args).context("mount_nfs failed");
    }

    // 1. Passwordless sudo (non-interactive, never hangs a script).
    let non_interactive = match run_sudo(&args, false) {
        Ok(()) => return Ok(()),
        Err(err) => err,
    };
    // 2. Interactive sudo: let the user type a password in the terminal.
    match run_sudo(&args, true) {
        Ok(()) => Ok(()),
        Err(err) => bail!(
            "mounting requires root privileges.\n\
             sudo -n failed: {non_interactive:#?}\n\
             interactive sudo failed: {err:#}\n\
             Re-run the command in a terminal that can run sudo."
        ),
    }
}

/// Unmounts `mount_point`, escalating to `sudo` when needed. Returns whether it was mounted.
pub fn unmount(mount_point: &Path) -> Result<bool> {
    if !is_mounted(mount_point) {
        return Ok(false);
    }
    let target = mount_point.to_string_lossy().to_string();
    if is_root() {
        run_checked(UMOUNT, &[target.as_str()]).context("umount failed")?;
    } else if run_sudo_program(UMOUNT, &[target.as_str()], false).is_err() {
        run_sudo_program(UMOUNT, &[target.as_str()], true).with_context(|| {
            format!(
                "unmounting {} requires root (try: sudo {UMOUNT} {target})",
                mount_point.display()
            )
        })?;
    }
    Ok(true)
}

fn run_sudo(args: &[&str], interactive: bool) -> Result<()> {
    run_sudo_program(MOUNT_NFS, args, interactive)
}

fn run_sudo_program(program: &str, args: &[&str], interactive: bool) -> Result<()> {
    let mut command = Command::new("/usr/bin/sudo");
    if !interactive {
        command.arg("-n");
    }
    command.arg(program).args(args);
    if interactive {
        command.stdin(std::process::Stdio::inherit());
        command.stdout(std::process::Stdio::inherit());
        command.stderr(std::process::Stdio::inherit());
        let status = command.status().context("running sudo")?;
        if !status.success() {
            bail!("sudo {program} exited with {status}");
        }
        return Ok(());
    }
    run_checked_command(&mut command)
}

fn run_checked(program: &str, args: &[&str]) -> Result<()> {
    let mut command = Command::new(program);
    command.args(args);
    run_checked_command(&mut command)
}

fn run_checked_command(command: &mut Command) -> Result<()> {
    let output = command.output().with_context(|| format!("spawning {command:?}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let detail = if stderr.is_empty() { stdout } else { stderr };
    Err(io::Error::other(format!(
        "{command:?} exited with {}: {detail}",
        output.status
    ))
    .into())
}
