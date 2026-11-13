# mount-sso

Mount S3-compatible object storage as an ordinary directory on macOS.

The tool serves the bucket over a loopback NFSv3 listener and asks `mount_nfs` to mount it,
so `ls`, `cat`, `grep`, editors, `git` and build tools work without a FUSE extension, a
kernel module or an entitlement. Only the loopback interface is used; nothing is exposed
to the network.

```console
$ mount-sso mount ~/mnt/bucket s3://my-bucket/reports \
    --endpoint http://127.0.0.1:9000 \
    --access-key-id minio --secret-access-key minio-secret

mounted s3://my-bucket/reports at /Users/me/mnt/bucket (loopback NFSv3 on 127.0.0.1:62000)
INFO press Ctrl-C or run `mount-sso umount /Users/me/mnt/bucket` to stop

$ ls ~/mnt/bucket
q3.pdf  summary.csv
$ head -1 ~/mnt/bucket/summary.csv        # downloaded once, then served locally
region,revenue
$ echo draft > ~/mnt/bucket/notes.txt     # staged, uploaded after --flush-ms
$ mount-sso umount ~/mnt/bucket
```

## Why NFSv3 instead of FUSE

macOS ships an NFSv3 client that requires no kext, no entitlement and no `macFUSE`
installation; `/sbin/mount_nfs` exists on every current system. Serving NFSv3 on
`127.0.0.1` therefore gives a mountable filesystem with zero third-party dependencies —
the only privileged step is the `mount_nfs` call itself, which the tool wraps in `sudo`.

## Requirements

- macOS (uses `/sbin/mount_nfs` and `/sbin/umount`).
- Root for the mount step. The tool tries `sudo -n` first and falls back to an interactive
  `sudo`, so it works both in scripts and in a terminal.
- An S3-compatible endpoint: AWS S3, MinIO, Ceph RGW, Cloudflare R2, Alibaba OSS, ...

## Install

Prebuilt binary (Apple silicon, macOS 11+):

```console
$ curl -LO https://github.com/yunhai-dev/mount-sso/releases/download/v0.1.1/mount-sso-0.1.1-aarch64-apple-darwin.tar.gz
$ tar -xzf mount-sso-0.1.1-aarch64-apple-darwin.tar.gz
$ sudo install mount-sso-0.1.1-aarch64-apple-darwin/mount-sso /usr/local/bin/
```

The release also publishes `SHA256SUMS`; verify with `shasum -a 256 -c SHA256SUMS`.
The binary is not notarized, so a download through a browser picks up the quarantine
attribute and Gatekeeper refuses the first run — `xattr -d com.apple.quarantine mount-sso`
(or installing through `curl`/Homebrew, which do not set it) resolves that.

From source:

```console
$ cargo install --path .
$ ./scripts/package-macos.sh    # tarball, zip and SHA256SUMS in dist/
```

Only `aarch64-apple-darwin` is built here; for a universal binary install both targets
(`rustup target add x86_64-apple-darwin`) and merge with
`lipo -create -output mount-sso target/{aarch64,x86_64}-apple-darwin/release/mount-sso`.

## Usage

```
mount-sso mount <MOUNT_POINT> <TARGET> [OPTIONS]
mount-sso umount <MOUNT_POINT>
```

`<TARGET>` is `bucket` or `s3://bucket/prefix`. The mount point is created when missing.
The daemon runs in the foreground until the client unmounts (Finder "Eject", `umount`, or
`mount-sso umount`) or until it receives `SIGINT`/`SIGTERM`/`SIGHUP`; pending writes are
uploaded before the mount goes away.

`--prefix` is equivalent to a prefix in the target — `s3://bucket/reports` and
`s3://bucket --prefix reports` expose the same tree, and conflicting values are rejected.

### Credentials

Explicit flags win over the environment, which in turn wins over the usual AWS chain
(`AWS_PROFILE`, `~/.aws/config`, instance/task roles):

| Flag | Environment |
| --- | --- |
| `--access-key-id` | `AWS_ACCESS_KEY_ID` |
| `--secret-access-key` | `AWS_SECRET_ACCESS_KEY` |
| `--session-token` | `AWS_SESSION_TOKEN` |

Custom endpoints are addressed path-style and may use plain HTTP when the URL starts with
`http://` or when `--allow-http` is given. Without `--endpoint` the standard AWS endpoint
for the region is used.

## Options

| Option | Default | Meaning |
| --- | --- | --- |
| `--endpoint <URL>` | AWS | S3-compatible endpoint |
| `--region <REGION>` | env, then `us-east-1` | SigV4 signing region |
| `--prefix <PREFIX>` | – | Key prefix exposed as the mount root |
| `--allow-http` | off | Permit plain HTTP for AWS endpoints |
| `--read-only` | off | Mount read-only; every mutation returns `EROFS` |
| `--port <PORT>` | `0` (free port) | Loopback NFS port |
| `--dir-cache-ms <MS>` | `2000` | Directory listing / attribute cache TTL; also sets the kernel's `actimeo` (minimum 100 ms) |
| `--flush-ms <MS>` | `1000` | Idle time after a write before the file is uploaded (minimum 50 ms) |
| `--cache-dir <DIR>` | fresh temp dir | Root for staged files and cached object contents; removed on exit unless given explicitly |
| `--max-stage-mib <MIB>` | `4096` | Cap on staged (written but not yet uploaded) bytes on local disk |
| `--read-cache-ms <MS>` | `30000` | How long a fetched object is served from the local copy; `0` disables the read cache |
| `--read-cache-mib <MIB>` | `4096` | Cap on cached object bytes; `0` disables the read cache |
| `--prefetch-mib <MIB>` | `256` | Objects up to this size are fetched whole on first read; `0` streams instead |
| `--keep-macos-junk` | off | Expose macOS bookkeeping files and upload AppleDoubles instead of hiding them |
| `-v`, `-vv` | – | Debug / trace logging to stderr; `MOUNT_SSO_LOG` overrides the filter |

## How the bucket becomes a file tree

- **Keys are paths.** `reports/2024/q3.pdf` below the mount root is `2024/q3.pdf` in the
  mounted tree.
- **Directories are inferred.** A `common_prefixes` entry from `ListObjectsV2` with
  delimiter `/` is a directory; no `dir/` marker objects are written. A directory created
  with `mkdir` exists in memory until a key lands below it, so it does not survive the
  mount (it disappears on unmount or process restart if it stayed empty).
- **Inodes are per session.** `fileid3`s are allocated per path on demand and cached
  together with listings; reading a stale inode returns `NFS3ERR_STALE`.
- **macOS metadata is excluded.** `.DS_Store`, `.Spotlight-V100`, `.Trashes`, `.fseventsd`,
  `.TemporaryItems`, `.DocumentRevisions-V100`, `.apdisk`, `.VolumeIcon.icns` and `Icon\r`
  are hidden from listings and lookups and refused on `create`/`mkdir` with `EACCES`
  (permission denied), even when the bucket already contains them.
- **AppleDouble files stay local.** `._name` siblings are macOS' way of storing extended
  attributes on filesystems without xattr support; `cp -a`, `ditto` and the Finder write one
  next to anything whose attributes they preserve. They are writable and readable for the
  duration of the mount, hidden from listings, and never uploaded — the bytes live in the
  staging file and evaporate on unmount. Refusing them instead would break attribute-
  preserving copies, so they are sunk rather than rejected. Moving a real file into that
  namespace (or out of it) is refused with `EACCES`. `--keep-macos-junk` disables the whole
  filter.
- **Attributes.** Files are `0644`, directories `0755`, both owned by the invoking user
  (the daemon runs unprivileged; only `mount_nfs` is elevated). Size and mtime come from
  the object; `chmod` and `utimes` are accepted and kept in memory for the session — S3 has
  nowhere to store them, so a remount returns to the defaults and to `last_modified`.
- **Unsupported operations.** No hard links, no symlinks (declared in `fsinfo` and
  rejected with `NFS3ERR_NOTSUPP`), no special files. `rename` is implemented as
  `CopyObject` + `DeleteObject` (or a per-key copy for directories), so it is not atomic
  and is not free on large objects.
- **Locking is client-side only** (`locallocks`): two hosts writing the same key are last
  writer wins, and locks are not shared between machines.

## Caching and write-back

Three layers keep a mounted tree from turning into one request per syscall:

| Layer | Behaviour | Tunables |
| --- | --- | --- |
| Metadata | Listings and attributes are served from memory; a file's size/mtime is re-checked with `HEAD` after the TTL | `--dir-cache-ms` |
| Read contents | The first read of an object up to the threshold downloads it whole and every later read is a local `pread` | `--prefetch-mib`, `--read-cache-mib`, `--read-cache-ms` |
| Write contents | Writes go to a staging file and are uploaded after an idle period, on unmount, or before an operation that must observe the bytes | `--flush-ms`, `--max-stage-mib` |

**Read cache.** Cached copies live in `<cache-dir>/read`, keyed by inode. A copy is used
only while the object's observed size/mtime still match and the entry is younger than
`--read-cache-ms`; it is dropped when the file is written (the staging file is
authoritative), when another client replaces the object, and after the TTL. Concurrent
reads of the same uncached object share a single download — the kernel typically keeps
sixteen 1 MiB reads in flight, and without that they would each start their own fetch.
Entries are evicted least-recently-used once `--read-cache-mib` is exceeded, leftover files
from an earlier process are removed at startup, and objects larger than `--prefetch-mib`
stream ranged GETs instead of being copied, so `tail` on a 10 GiB object does not pull
10 GiB.

**Write-back.** `write`, `truncate` and `create` operate on a staging file that starts as a
copy of the current object. The upload happens once the file has been untouched for
`--flush-ms` (so a burst of appends costs one PUT), plus on unmount and before `rename`.
Uploads up to 8 MiB are a single `PutObject`; larger files use multipart upload with 8 MiB
parts and up to 8 parts in flight. `create` publishes an empty object immediately so the
file is visible to other clients; the content follows with the flush.

## Request cost

Measured with `mc admin trace` against MinIO (`rsize/wsize` = 1 MiB, `--dir-cache-ms 2000`,
default read cache) while driving the mounted tree:

| Operation | Requests to S3 |
| --- | --- |
| `ls` (21 entries), first | 1 × `ListObjectsV2` |
| `ls` again inside `--dir-cache-ms` | 0 |
| `stat` a file twice | 0 (attributes cached for `--dir-cache-ms`) |
| `md5` an 8 MiB file, first time | 1 × `HEAD` + 1 × `GetObject` (whole file, cached) |
| `md5` the same file again | 0 |
| `cat` a 16-byte file, first time | 1 × `HEAD` + 1 × `GetObject` |
| `cat` the same file again | 0 |
| three appends to one new file | 2 × `PUT` (empty object on create, content once after idle) |
| object replaced by another client | picked up on the next attribute refresh: 1 × `HEAD` + 1 × `GetObject` |
| file larger than `--prefetch-mib` | 1 × ranged `GET` per MiB, no caching |
| `mkdir` | 0 until a key lands inside |
| `rmdir` | listings only (emptiness check); a directory is never a stored object |
| `rm` a file | 1 × delete |
| `mv` a file | 1 × `CopyObject` + 1 × delete of the source (after flushing it) |

One cost is inherent to the client rather than the server: every `open` is preceded by a
`LOOKUP`, which costs one probe when the name is not in a warm listing. AppleDouble
`._name` siblings that macOS writes next to files with extended attributes stay in the
local staging area, so they cost no request and no object.

## Limits and semantics

- **Not a POSIX filesystem.** There is no locking across hosts, no atomic rename, no
  symlinks, no hard links, and permissions/mtime do not persist.
- **Eventual consistency is the bucket's.** A change made elsewhere is observed after
  `--dir-cache-ms` (metadata) or `--read-cache-ms` (contents at most).
- **Writes are debounced.** Killing the process with `SIGKILL` (or a power loss) loses
  writes that were not uploaded yet; `SIGINT`/`SIGTERM`/unmount flush first.
- **Local disk is required.** Staged writes are bounded by `--max-stage-mib`; uploaded
  staging files are evicted when that budget is exceeded, and a later write re-downloads
  the object. Versioned buckets accumulate old versions on every overwrite.
- **File size.** `fsinfo` advertises a 5 TiB maximum, but the uploader caps a single
  multipart chain at 10 000 parts of 8 MiB, i.e. 80 GiB per file.
- **Empty directories do not survive** a remount, since S3 has no way to represent them.

## Troubleshooting

| Symptom | Cause / fix |
| --- | --- |
| `mounting requires root privileges` | No `sudo -n` and no terminal to prompt in; run it from an interactive shell or configure passwordless sudo for `/sbin/mount_nfs`. |
| `... is already mounted` | Unmount first: `mount-sso umount <dir>` (or `umount -f <dir>` after a crash). |
| `listing the bucket failed` | Wrong endpoint/region/bucket, or the credentials cannot list the prefix. |
| `Resource busy` on unmount | A process is still using the tree; `lsof +D <dir>`, then retry or `umount -f`. |
| Command hangs after a crash | Stale NFS mount: `umount -f <dir>`, then remount. |
| Too many requests | Raise `--dir-cache-ms`, `--read-cache-ms` and `--prefetch-mib`; prefer one large sequential read over many small random ones when the objects are huge. |
| Local disk fills up | Lower `--max-stage-mib` / `--read-cache-mib`, or point `--cache-dir` at a larger volume. |
| `touch .DS_Store` fails with EACCES | Expected: bookkeeping names are refused so metadata stays out of the bucket. |
| `cp -a` warns about attributes | Expected: NFSv3 has no xattrs, so macOS falls back to a local-only `._name` sibling; the copy still succeeds. |
| Junk already in the bucket | It is hidden by the filter; delete it through the bucket itself (S3 console, `mc`, `aws s3 rm`). |

## Architecture

| Module | Role |
| --- | --- |
| `src/main.rs` | CLI, credential/endpoint wiring, loopback listener, signal handling, unmount on exit |
| `src/backend.rs` | Builds the `object_store` S3 client (path-style addressing, HTTP opt-in, region/credential precedence) |
| `src/vfs.rs` | `NFSFileSystem` implementation: namespace, inodes, staging, debounced flush, uploads |
| `src/cache.rs` | On-disk read cache: fetch-once, validation, LRU eviction |
| `src/mount.rs` | macOS `mount_nfs` / `umount` plumbing, including the sudo escalation and mount options |

Mount options are `locallocks,vers=3,tcp,rsize=1048576,actimeo=<dir-cache-ms>,
port=<port>,mountport=<port>,hard` plus `wsize=1048576` or `rdonly`.

## Development

```console
$ cargo build
$ cargo test          # unit tests in src/, integration tests in tests/
$ cargo clippy --all-targets
```

| Test file | Covers |
| --- | --- |
| `tests/vfs.rs` | Namespace and attribute behaviour through the `NFSFileSystem` trait |
| `tests/wire.rs` | NFSv3 framing of a few operations over the loopback server |
| `tests/requests.rs` | Request accounting: one download per object, one listing per TTL, no probes when a directory is warm, write coalescing |

The `requests.rs` suite wraps the in-memory store in a counting decorator, so it pins the
network cost of each operation instead of merely asserting the output. For an end-to-end
check against a real bucket, run MinIO locally (`docker run -p 9100:9000 minio/minio server
/data`) and watch requests with `mc admin trace --verbose`. Integration tests run against
`object_store`'s in-memory backend; no credentials are needed.

## License

MIT — see [LICENSE](LICENSE).
