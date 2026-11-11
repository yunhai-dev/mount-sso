//! Mount S3-compatible object storage as a local POSIX filesystem on macOS.
//!
//! The tool runs a loopback NFSv3 server backed by an S3 bucket and asks macOS to mount it
//! through `mount_nfs`, so every process on the machine sees a normal directory tree.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use nfsserve::tcp::{NFSTcp, NFSTcpListener};
use object_store::ObjectStore;
use object_store::path::Path as ObjPath;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};
use tracing_subscriber::EnvFilter;

use mount_sso::vfs::S3Fs;
use mount_sso::{backend, mount, vfs};

#[derive(Parser)]
#[command(
    name = "mount-sso",
    version,
    about = "Mount S3-compatible object storage as a POSIX filesystem (loopback NFSv3 server)"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Mount a bucket (or a key prefix inside it) at a mount point.
    Mount(Box<MountArgs>),
    /// Unmount a previously mounted bucket.
    Umount {
        /// Directory the bucket was mounted on.
        mount_point: PathBuf,
    },
}

#[derive(Args)]
struct MountArgs {
    /// Local directory to mount on (created when missing).
    mount_point: PathBuf,
    /// Bucket to mount: `bucket` or `s3://bucket/prefix`.
    target: String,
    /// S3-compatible endpoint, e.g. `https://s3.us-east-1.amazonaws.com` or `http://127.0.0.1:9000`.
    #[arg(long)]
    endpoint: Option<String>,
    /// Signing region (defaults to the environment, then `us-east-1`).
    #[arg(long)]
    region: Option<String>,
    /// Key prefix inside the bucket to expose as the mount root.
    #[arg(long)]
    prefix: Option<String>,
    #[arg(long, env = "AWS_ACCESS_KEY_ID")]
    access_key_id: Option<String>,
    #[arg(long, env = "AWS_SECRET_ACCESS_KEY")]
    secret_access_key: Option<String>,
    #[arg(long, env = "AWS_SESSION_TOKEN")]
    session_token: Option<String>,
    /// Allow plain HTTP endpoints.
    #[arg(long)]
    allow_http: bool,
    /// Mount read-only.
    #[arg(long)]
    read_only: bool,
    /// Loopback NFS port (0 = pick a free port).
    #[arg(long, default_value_t = 0)]
    port: u16,
    /// How long directory listings and attributes are cached, in milliseconds.
    #[arg(long, default_value_t = 2000)]
    dir_cache_ms: u64,
    /// Idle time after a write before it is uploaded, in milliseconds.
    #[arg(long, default_value_t = 1000)]
    flush_ms: u64,
    /// Directory for staging files and the read cache (defaults to a fresh temp directory).
    #[arg(long)]
    cache_dir: Option<PathBuf>,
    /// Cap on staged (written but uploaded) bytes kept on local disk, in MiB.
    #[arg(long, default_value_t = 4096)]
    max_stage_mib: u64,
    /// How long a fetched object is served from the local copy before it is fetched again,
    /// in milliseconds; `0` disables the read cache.
    #[arg(long, default_value_t = 30000)]
    read_cache_ms: u64,
    /// Cap on cached object contents kept on local disk, in MiB (`0` disables the cache).
    #[arg(long, default_value_t = 4096)]
    read_cache_mib: u64,
    /// Objects up to this size are fetched whole on first read, in MiB (`0` streams instead).
    #[arg(long, default_value_t = 256)]
    prefetch_mib: u64,
    /// Increase log verbosity.
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let verbosity = match &cli.command {
        Command::Mount(args) => args.verbose,
        Command::Umount { .. } => 0,
    };
    init_tracing(verbosity);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("starting the async runtime")?;
    runtime.block_on(async move {
        match cli.command {
            Command::Mount(args) => run_mount(*args).await,
            Command::Umount { mount_point } => run_umount(mount_point).await,
        }
    })
}

fn init_tracing(verbosity: u8) {
    let default = match verbosity {
        0 => "mount_sso=info,warn",
        1 => "mount_sso=debug,info",
        _ => "mount_sso=trace,debug",
    };
    let filter = EnvFilter::try_from_env("MOUNT_SSO_LOG").unwrap_or_else(|_| EnvFilter::new(default));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .try_init();
}

async fn run_mount(args: MountArgs) -> Result<()> {
    let (bucket, prefix) = parse_target(&args.target, args.prefix.as_deref())?;
    let mount_point = prepare_mount_point(&args.mount_point)?;
    if mount::is_mounted(&mount_point) {
        bail!(
            "{} is already mounted; run `mount-sso umount {}` first",
            mount_point.display(),
            mount_point.display()
        );
    }

    let store = backend::build_store(&backend::S3Options {
        bucket: bucket.clone(),
        endpoint: args.endpoint.clone(),
        region: args.region.clone(),
        access_key_id: args.access_key_id.clone(),
        secret_access_key: args.secret_access_key.clone(),
        session_token: args.session_token.clone(),
        allow_http: args.allow_http,
    })?;
    check_access(&store, &prefix).await?;

    let cache_root = args
        .cache_dir
        .clone()
        .unwrap_or_else(|| std::env::temp_dir().join(format!("mount-sso-{}", std::process::id())));
    let staging_dir = cache_root.join("staging");
    let read_cache_dir = cache_root.join("read");
    let owned_cache_root = args.cache_dir.is_none();

    let config = vfs::Config {
        root_prefix: prefix.clone(),
        uid: current_uid(),
        gid: current_gid(),
        read_only: args.read_only,
        dir_ttl: Duration::from_millis(args.dir_cache_ms.max(100)),
        flush_delay: Duration::from_millis(args.flush_ms.max(50)),
        staging_dir: staging_dir.clone(),
        max_stage_bytes: args.max_stage_mib.saturating_mul(1024 * 1024),
        cache_dir: read_cache_dir,
        read_cache_bytes: args.read_cache_mib.saturating_mul(1024 * 1024),
        // `--read-cache-ms 0` is the documented way to turn the cache off, so it must not
        // be clamped up to the minimum age.
        read_cache_ttl: if args.read_cache_ms == 0 {
            Duration::ZERO
        } else {
            Duration::from_millis(args.read_cache_ms.max(100))
        },
        prefetch_bytes: args.prefetch_mib.saturating_mul(1024 * 1024),
    };
    let fs = S3Fs::new(store, config).await?;

    let address = format!("127.0.0.1:{}", args.port);
    let mut listener = NFSTcpListener::bind(&address, fs.clone())
        .await
        .with_context(|| format!("binding the loopback NFS server on {address}"))?;
    let port = listener.get_listen_port();
    let (mount_tx, mut mount_rx) = mpsc::channel(4);
    listener.set_mount_listener(mount_tx);
    let mut server = tokio::spawn(async move {
        if let Err(err) = listener.handle_forever().await {
            error!("NFS server stopped: {err}");
        }
    });

    let actimeo_secs = (args.dir_cache_ms / 1000).max(1);
    let mount_point_for_mount = mount_point.clone();
    let read_only = args.read_only;
    tokio::task::spawn_blocking(move || mount::mount_nfs(port, &mount_point_for_mount, actimeo_secs, read_only))
        .await
        .context("joining the mount task")??;

    println!(
        "mounted s3://{}{} at {} (loopback NFSv3 on 127.0.0.1:{port}{})",
        bucket,
        if prefix.is_empty() { String::new() } else { format!("/{prefix}") },
        mount_point.display(),
        if read_only { ", read-only" } else { "" }
    );
    info!("press Ctrl-C or run `mount-sso umount {}` to stop", mount_point.display());

    let mut sigint = signal_stream(libc::SIGINT)?;
    let mut sigterm = signal_stream(libc::SIGTERM)?;
    let mut sighup = signal_stream(libc::SIGHUP)?;
    // The mount listener reports both directions: `true` on mount, `false` on unmount.
    loop {
        tokio::select! {
            signal = mount_rx.recv() => match signal {
                Some(true) => debug!("client mounted the export"),
                Some(false) => {
                    info!("client unmounted the export");
                    break;
                }
                None => {
                    warn!("mount listener closed");
                    break;
                }
            },
            _ = sigint.recv() => {
                info!("received SIGINT");
                break;
            }
            _ = sigterm.recv() => {
                info!("received SIGTERM");
                break;
            }
            _ = sighup.recv() => {
                info!("received SIGHUP");
                break;
            }
            _ = &mut server => {
                warn!("NFS server task finished unexpectedly");
                break;
            }
        }
    }

    // Upload anything still staged before the mount goes away.
    fs.shutdown().await;
    let mount_point_for_umount = mount_point.clone();
    let unmounted = tokio::task::spawn_blocking(move || mount::unmount(&mount_point_for_umount))
        .await
        .context("joining the unmount task")?;
    match unmounted {
        Ok(true) => println!("unmounted {}", mount_point.display()),
        Ok(false) => info!("{} was already unmounted", mount_point.display()),
        Err(err) => warn!("unmounting {} failed: {err:#}", mount_point.display()),
    }

    if owned_cache_root {
        let _ = std::fs::remove_dir_all(&cache_root);
    }
    Ok(())
}

async fn run_umount(mount_point: PathBuf) -> Result<()> {
    let mount_point = mount_point
        .canonicalize()
        .with_context(|| format!("resolving {}", mount_point.display()))?;
    let target = mount_point.clone();
    let unmounted = tokio::task::spawn_blocking(move || mount::unmount(&target))
        .await
        .context("joining the unmount task")??;
    if unmounted {
        println!("unmounted {}", mount_point.display());
    } else {
        println!("{} is not mounted", mount_point.display());
    }
    Ok(())
}

/// Splits `bucket` / `s3://bucket/prefix` into a bucket name and a normalised key prefix.
fn parse_target(target: &str, prefix_flag: Option<&str>) -> Result<(String, String)> {
    let raw = target.trim();
    let raw = raw.strip_prefix("s3://").unwrap_or(raw);
    let (bucket, url_prefix) = match raw.split_once('/') {
        Some((bucket, rest)) => (bucket.trim(), rest.trim_matches('/').to_string()),
        None => (raw, String::new()),
    };
    if bucket.is_empty() {
        bail!("invalid target `{target}`: expected `bucket` or `s3://bucket/prefix`");
    }
    let flag_prefix = prefix_flag.unwrap_or("").trim_matches('/').to_string();
    let prefix = if !url_prefix.is_empty() && !flag_prefix.is_empty() && url_prefix != flag_prefix {
        bail!("conflicting prefixes: `{url_prefix}` in the target and `--prefix {flag_prefix}`");
    } else if !url_prefix.is_empty() {
        url_prefix
    } else {
        flag_prefix
    };
    Ok((bucket.to_string(), prefix))
}

fn prepare_mount_point(path: &Path) -> Result<PathBuf> {
    if !path.exists() {
        std::fs::create_dir_all(path).with_context(|| format!("creating mount point {}", path.display()))?;
    }
    let metadata = std::fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    if !metadata.is_dir() {
        bail!("mount point {} is not a directory", path.display());
    }
    path.canonicalize()
        .with_context(|| format!("resolving {}", path.display()))
}

/// Fails fast on bad credentials, a wrong bucket or an unreachable endpoint.
async fn check_access(store: &std::sync::Arc<dyn ObjectStore>, prefix: &str) -> Result<()> {
    let path = if prefix.is_empty() {
        None
    } else {
        Some(ObjPath::from(prefix))
    };
    store
        .list_with_delimiter(path.as_ref())
        .await
        .context("listing the bucket failed (check the endpoint, region, bucket and credentials)")?;
    Ok(())
}

fn signal_stream(signal: i32) -> Result<tokio::signal::unix::Signal> {
    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::from_raw(signal))
        .with_context(|| format!("installing handler for signal {signal}"))
}

fn current_uid() -> u32 {
    // SAFETY: `geteuid` has no preconditions.
    unsafe { libc::geteuid() }
}

fn current_gid() -> u32 {
    // SAFETY: `getegid` has no preconditions.
    unsafe { libc::getegid() }
}
