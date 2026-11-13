# mount-sso

[English](README.md) | 简体中文

把 S3 兼容对象存储挂载成 macOS 上的一个普通目录。

工具在本地回环地址上提供 NFSv3 服务，再交给 `mount_nfs` 挂载成文件系统，因此 `ls`、
`cat`、`grep`、编辑器、`git` 和构建工具都能直接使用，不需要 macFUSE、内核扩展或任何
签名授权。只监听回环接口，不对外暴露端口。

```console
$ mount-sso mount ~/mnt/bucket s3://my-bucket/reports \
    --endpoint http://127.0.0.1:9000 \
    --access-key-id minio --secret-access-key minio-secret

mounted s3://my-bucket/reports at /Users/me/mnt/bucket (loopback NFSv3 on 127.0.0.1:62000)
INFO press Ctrl-C or run `mount-sso umount /Users/me/mnt/bucket` to stop

$ ls ~/mnt/bucket
q3.pdf  summary.csv
$ head -1 ~/mnt/bucket/summary.csv        # 首次读取会整份拉到本地，之后走本地缓存
region,revenue
$ echo draft > ~/mnt/bucket/notes.txt     # 先写本地 staging，空闲后上传
$ mount-sso umount ~/mnt/bucket
```

## 为什么用 NFSv3 而不是 FUSE

macOS 自带 NFSv3 客户端，无需内核扩展、无需授权、无需安装 `macFUSE`；
`/sbin/mount_nfs` 在所有现行系统上都存在。把 NFSv3 服务跑在 `127.0.0.1` 上即可获得
可挂载的文件系统，唯一需要提权的动作就是 `mount_nfs` 本身，工具已用 `sudo` 包好。

## 环境要求

- macOS（使用 `/sbin/mount_nfs` 和 `/sbin/umount`）。
- 挂载这一步需要 root：工具先尝试 `sudo -n`，失败后再交互提示输入密码，因此脚本和
  终端里都能用。
- 一个 S3 兼容端点：AWS S3、MinIO、Ceph RGW、Cloudflare R2、阿里云 OSS 等。

## 安装

预编译二进制（Apple silicon，macOS 11+）：

```console
$ curl -LO https://github.com/yunhai-dev/mount-sso/releases/download/v0.1.3/mount-sso-0.1.3-aarch64-apple-darwin.tar.gz
$ tar -xzf mount-sso-0.1.3-aarch64-apple-darwin.tar.gz
$ install -m 755 mount-sso-0.1.3-aarch64-apple-darwin/mount-sso ~/.local/bin/
```

发布页同时提供 `SHA256SUMS`，可用 `shasum -a 256 -c SHA256SUMS` 校验。二进制未做公证，
浏览器下载会带 quarantine 属性，首次运行被 Gatekeeper 拦下时执行
`xattr -d com.apple.quarantine mount-sso` 即可（用 `curl` 或 Homebrew 安装不会带该属性）。

从源码构建：

```console
$ cargo install --path .
$ ./scripts/package-macos.sh    # 在 dist/ 下生成 tar.gz、zip 和 SHA256SUMS
```

目前只构建 `aarch64-apple-darwin`。需要通用二进制时装上两个目标
（`rustup target add x86_64-apple-darwin`），再用
`lipo -create -output mount-sso target/{aarch64,x86_64}-apple-darwin/release/mount-sso` 合并。

## 用法

```
mount-sso mount <挂载点> <目标> [选项]
mount-sso umount <挂载点>
```

`<目标>` 可以是 `bucket` 或 `s3://bucket/prefix`；挂载点不存在时会自动创建。守护进程
在前台运行，直到客户端卸载（Finder 推出、`umount` 或 `mount-sso umount`）或收到
`SIGINT`/`SIGTERM`/`SIGHUP`，退出前会把尚未上传的内容写完。

`--prefix` 与目标里的前缀等价：`s3://bucket/reports` 和 `s3://bucket --prefix reports`
挂出来是同一棵树，两处写了不同的值会直接报错。

### 凭据

显式参数优先于环境变量，环境变量优先于标准 AWS 凭据链（`AWS_PROFILE`、`~/.aws/config`、
实例/任务角色）：

| 参数 | 环境变量 |
| --- | --- |
| `--access-key-id` | `AWS_ACCESS_KEY_ID` |
| `--secret-access-key` | `AWS_SECRET_ACCESS_KEY` |
| `--session-token` | `AWS_SESSION_TOKEN` |

自定义端点一律使用 path-style 寻址；URL 以 `http://` 开头或显式给出 `--allow-http` 时
允许明文 HTTP。不给 `--endpoint` 时使用该区域的标准 AWS 端点。

## 选项

| 选项 | 默认值 | 说明 |
| --- | --- | --- |
| `--endpoint <URL>` | AWS | S3 兼容端点 |
| `--region <REGION>` | 环境变量，其次 `us-east-1` | SigV4 签名区域 |
| `--prefix <PREFIX>` | 无 | 作为挂载根暴露的 key 前缀 |
| `--allow-http` | 关闭 | 允许 AWS 端点使用明文 HTTP |
| `--read-only` | 关闭 | 只读挂载，所有写操作返回 `EROFS` |
| `--port <PORT>` | `0`（自动） | 回环 NFS 端口 |
| `--dir-cache-ms <MS>` | `2000` | 目录列表/属性缓存时长，同时作为内核 `actimeo`（最小 100 ms） |
| `--flush-ms <MS>` | `1000` | 写入空闲多久后上传（最小 50 ms） |
| `--cache-dir <DIR>` | 临时目录（退出即删） | staging 与读缓存的根目录 |
| `--max-stage-mib <MIB>` | `4096` | 本地暂存（已写未传）字节上限 |
| `--read-cache-ms <MS>` | `30000` | 本地副本可被复用的时长，`0` 关闭读缓存 |
| `--read-cache-mib <MIB>` | `4096` | 读缓存占用上限，`0` 关闭读缓存 |
| `--prefetch-mib <MIB>` | `256` | 不超过该大小的对象首次读整份拉取；`0` 改为流式读取 |
| `--keep-macos-junk` | 关闭 | 不再隐藏 macOS 记账文件，并上传 AppleDouble |
| `-v`、`-vv` | – | 输出 debug / trace 日志；`MOUNT_SSO_LOG` 可覆盖过滤规则 |

## 桶如何变成目录树

- **key 就是路径。** 挂载根下的 `reports/2024/q3.pdf` 在挂载视图里是 `2024/q3.pdf`。
- **目录是推断出来的。** `ListObjectsV2` 带 `/` 分隔符返回的 `common_prefixes` 就是目录，
  不会写入 `dir/` 之类的标记对象。用 `mkdir` 创建的目录只存在于内存，直到有 key 落进去，
  因此空目录在卸载或重启后不会保留。
- **inode 以会话为界。** `fileid3` 按路径按需分配并随列表缓存；使用过期 inode 会得到
  `NFS3ERR_STALE`。
- **macOS 记账文件被排除。** `.DS_Store`、`.Spotlight-V100`、`.Trashes`、`.fseventsd`、
  `.TemporaryItems`、`.DocumentRevisions-V100`、`.apdisk`、`.VolumeIcon.icns` 和 `Icon\r`
  不会出现在列表与 lookup 中，`create`/`mkdir` 会直接返回 `EACCES`（权限不足），即使
  桶里本来就有这些对象也一样。
- **AppleDouble 只留在本地。** `._name` 是 macOS 在不支持 xattr 的文件系统上保存扩展属性
  的方式；`cp -a`、`ditto` 和 Finder 在保留属性时会写这样一个兄弟文件。它在挂载期间可读
  可写、不出现在列表里、也永不上传，字节留在 staging 文件中，卸载即消失。若改成拒绝写入，
  保留属性的复制会失败，所以这里选择"接收但丢弃"。把真实文件移入或移出该命名空间会被
  `EACCES` 拒绝。`--keep-macos-junk` 可关闭整层过滤。
- **属性。** 文件 `0644`、目录 `0755`，属主是调用者（守护进程以普通用户身份运行，只有
  `mount_nfs` 提权）。大小与 mtime 来自对象；`chmod`、`utimes` 会被接受但只存在于内存，
  S3 无处存放，重新挂载后回到默认值与 `last_modified`。
- **不支持的操作。** 没有硬链接、没有符号链接（`fsinfo` 中已声明，操作返回
  `NFS3ERR_NOTSUPP`）、没有特殊文件。`rename` 用 `CopyObject` + `DeleteObject` 实现
  （目录是逐个 key 复制），因此不是原子的，大对象上也不便宜。
- **锁只在客户端本地生效**（`locallocks`）：两台机器写同一个 key 是最后写入者获胜，
  锁不跨机器共享。

## 缓存与回写

三层缓存让挂载树不至于变成"每次系统调用一次网络请求"：

| 层 | 行为 | 可调项 |
| --- | --- | --- |
| 元数据 | 目录列表与属性走内存；过期后用 `HEAD` 复核文件大小/mtime | `--dir-cache-ms` |
| 读内容 | 不超过阈值的对象首次读整份下载，之后全是本地 `pread` | `--prefetch-mib`、`--read-cache-mib`、`--read-cache-ms` |
| 写内容 | 写入先落 staging，空闲一段时间后上传；卸载前、以及需要看到数据的操作前也会上传 | `--flush-ms`、`--max-stage-mib` |

**读缓存。** 副本存放在 `<cache-dir>/read`，按 inode 索引。只有当对象观测到的
大小/mtime 仍然一致且条目未超过 `--read-cache-ms` 时才复用；文件被写入（staging 才是
权威）、被其他客户端替换、或超过 TTL 时立即失效。同一个未缓存对象的并发读共享一次下载
——内核通常会同时挂起十六个 1 MiB 的读请求，不合并的话每个都会各自发起一次完整拉取。
超过 `--read-cache-mib` 时按 LRU 淘汰；启动时清掉上个进程遗留的缓存文件；大于
`--prefetch-mib` 的对象改为范围 GET 流式读取，所以 `tail` 一个 10 GiB 对象不会拉下 10 GiB。

**回写。** `write`、`truncate`、`create` 都作用在以当前对象内容为初值的 staging 文件上。
文件空闲 `--flush-ms` 后上传（因此连续追加只产生一次 PUT），卸载前以及 `rename` 前也会
上传。不超过 8 MiB 用单次 `PutObject`，更大的走分段上传（8 MiB 一段，最多 8 段并发）。
`create` 会立刻发布一个空对象，让文件名对其他客户端立即可见，内容随后随 flush 上传。

## 卸载与同步完成判定

`SIGINT`/`SIGTERM`/`SIGHUP`、`mount-sso umount`、Finder 推出，三种方式最终都走同一条路：
先把所有 staging 内容上传，报告结果，然后守护进程才退出。

| 退出方式 | 顺序 | 你会看到 |
| --- | --- | --- |
| 对守护进程 Ctrl-C / `SIGTERM` | 上传 → 报告 → 卸载 → 退出 | `sync complete: 3 files uploaded`（exit 0），或 `sync incomplete: 1 file(s) not uploaded` 并附原因（exit 1） |
| `mount-sso umount <目录>` | 卸载 → 等待守护进程 → 报告其结果 | 同上；退出码与上传结果一致 |
| Finder 推出、裸 `umount` | 先卸载，守护进程自行上传并把结果打在它的终端 | 事后执行 `mount-sso umount <目录>` 读出记录的结果 |

守护进程把 `pid`、挂载点、上传结果写入 `~/Library/Caches/mount-sso/<hash>.state`；
`umount` 是另一个进程，靠这个文件知道要等谁（最多十分钟），然后告诉你同步是否完成。

**保证**：拆卸前写入的每个文件要么上传成功，要么被明确报告为失败并返回非零退出码，
失败不会静默。

**不保证**：`SIGKILL`、崩溃或断电会跳过上传；最近 `--flush-ms` 窗口内尚未上传的写入会
丢失，此时 `umount` 会报告 `the daemon exited without reporting a flush result`。
缩小该窗口就调小 `--flush-ms`。

想独立复核：把桶的列表和目录树对比（`mc ls -r`、`aws s3 ls`），或者写一个哨兵文件、
卸载、重新挂载后 `cat` 回来。

## 请求开销

用 `mc admin trace` 对 MinIO 实测（`rsize/wsize` = 1 MiB，`--dir-cache-ms 2000`，默认读缓存）：

| 操作 | 发往 S3 的请求 |
| --- | --- |
| `ls`（21 项）首次 | 1 × `ListObjectsV2` |
| 在 `--dir-cache-ms` 内再次 `ls` | 0 |
| 连续两次 `stat` 同一文件 | 0（属性缓存） |
| 首次 `md5` 一个 8 MiB 文件 | 1 × `HEAD` + 1 × `GetObject`（整份，随后缓存） |
| 再次 `md5` 同一文件 | 0 |
| 首次 `cat` 一个 16 字节文件 | 1 × `HEAD` + 1 × `GetObject` |
| 再次 `cat` 同一文件 | 0 |
| 对新文件连续三次追加写 | 2 × `PUT`（create 的空对象 + 空闲后的一次内容上传） |
| 对象被其他客户端替换 | 下一次属性刷新即可发现：1 × `HEAD` + 1 × `GetObject` |
| 读取大于 `--prefetch-mib` 的文件 | 每 MiB 一次范围 `GET`，不缓存 |
| `mkdir` | 0，直到有 key 落进去 |
| `rmdir` | 仅列表（判空）；目录从不作为对象存储 |
| `rm` 文件 | 1 次删除 |
| `mv` 文件 | 1 × `CopyObject` + 1 次源删除（删除前会先 flush） |

### 批量复制

`cp` 本身是逐个参数顺序处理的——这是 `cp` 的行为，不是挂载的限制：每个文件要一次
`LOOKUP`、一次 `CREATE`、一次 `WRITE`，其中只有 `CREATE` 会等网络（发布空对象的那次
`PUT`）。文件内容是先落 staging，再在 `--flush-ms` 之后并行上传的。本地变更会原地更新
缓存中的目录列表而不是丢弃它，所以一次批量复制里的 lookup 和 create 都不会出网。

200 个小文件、本地 MinIO、release 构建：

| 方式 | 耗时 | S3 请求 |
| --- | --- | --- |
| `cp -R src/. dst/` | 2.1 s | 400 × `PUT` + 4 次探测 |
| `ls \| xargs -P8 -n 25 cp` | 1.0 s | 400 × `PUT` + 18 次探测 |

每个文件那两次 `PUT` 分别是"让文件名立即可见"的空对象和内容上传；批量传输的提升手段是
并行化复制。

还有一项成本来自客户端而非服务端：每次 `open` 前都有一次 `LOOKUP`，当名字不在新鲜列表里
时会付出一次探测。macOS 为带扩展属性的文件写的 `._name` AppleDouble 会留在本地 staging，
既不产生请求也不产生对象。

## 限制与语义

- **不是真正的 POSIX 文件系统。** 无跨主机锁、无原子 rename、无符号链接、无硬链接，
  权限与 mtime 不持久。
- **一致性由桶决定。** 其他位置做的修改会在 `--dir-cache-ms`（元数据）或
  `--read-cache-ms`（内容）之后被观察到。
- **写入是防抖的。** `SIGKILL` 或断电会丢掉尚未上传的写入；`SIGINT`/`SIGTERM`/卸载会先
  上传。
- **需要本地磁盘。** 暂存写入受 `--max-stage-mib` 约束；超出预算时已上传的 staging 会被
  淘汰，之后的写入会重新下载对象。开启版本控制的桶每次覆盖都会累积旧版本。
- **单文件大小。** `fsinfo` 宣称上限 5 TiB，但上传器单条分段链最多 10 000 段 × 8 MiB，
  即每文件 80 GiB。
- **空目录不持久**，重新挂载后消失，因为 S3 无法表示空目录。

## 故障排查

| 现象 | 原因 / 处理 |
| --- | --- |
| `mounting requires root privileges` | 既没有 `sudo -n` 也无法交互输入密码；请在可交互的终端里运行，或为 `/sbin/mount_nfs` 配置免密 sudo。 |
| `... is already mounted` | 先卸载：`mount-sso umount <目录>`（崩溃后用 `umount -f <目录>`）。 |
| `listing the bucket failed` | 端点/区域/桶不对，或凭据无权列出该前缀。 |
| 卸载报 `Resource busy` | 仍有进程在用该目录；`lsof +D <目录>` 后重试，或 `umount -f`。 |
| 崩溃后命令卡住 | 残留的 NFS 挂载：`umount -f <目录>` 后重新挂载。 |
| `touch .DS_Store` 报 EACCES | 预期行为：记账文件名被拒绝，以免污染桶。 |
| `cp -a` 提示属性复制失败 | 预期行为：NFSv3 没有 xattr，macOS 回退为本地 `._name`，复制本身仍然成功。 |
| 请求太多 | 调大 `--dir-cache-ms`、`--read-cache-ms`、`--prefetch-mib`；大对象尽量顺序读，避免大量小随机读。 |
| 本地磁盘被占满 | 调小 `--max-stage-mib` / `--read-cache-mib`，或把 `--cache-dir` 指到大盘。 |

## 架构

| 模块 | 职责 |
| --- | --- |
| `src/main.rs` | 命令行、凭据/端点装配、回环监听、信号处理、退出前卸载 |
| `src/backend.rs` | 构建 `object_store` 的 S3 客户端（path-style、HTTP 开关、区域与凭据优先级） |
| `src/vfs.rs` | `NFSFileSystem` 实现：命名空间、inode、staging、防抖上传、分段上传 |
| `src/cache.rs` | 磁盘读缓存：一次拉取、元数据校验、LRU 淘汰 |
| `src/daemon.rs` | 守护进程与 `umount` 之间的状态交接（pid、挂载点、上传结果） |
| `src/mount.rs` | macOS `mount_nfs` / `umount` 封装，含 sudo 升级与挂载参数 |

挂载参数为 `locallocks,vers=3,tcp,rsize=1048576,actimeo=<dir-cache-ms>,port=<port>,
mountport=<port>,hard`，可写时追加 `wsize=1048576`，只读时改用 `rdonly`。

## 开发

```console
$ cargo build
$ cargo test          # src/ 下的单元测试 + tests/ 下的集成测试
$ cargo clippy --all-targets
```

| 测试文件 | 覆盖内容 |
| --- | --- |
| `tests/vfs.rs` | 通过 `NFSFileSystem` trait 验证命名空间与属性行为（含中文/保留字符、前缀碰撞） |
| `tests/wire.rs` | 真实回环服务上的 NFSv3 报文往返 |
| `tests/requests.rs` | 请求计数：每个对象一次下载、每个 TTL 一次列表、热目录零探测、写入合并 |

`requests.rs` 用带计数的 `object_store` 包装器记录每个操作发出的请求数，所以它锁住的是
网络成本，而不只是输出结果。对真实桶做端到端验证时，本地起 MinIO
（`docker run -p 9100:9000 minio/minio server /data`），用
`mc admin trace --verbose` 观察请求。集成测试跑在 `object_store` 的内存后端上，不需要
任何凭据。

## 许可证

MIT，见 [LICENSE](LICENSE)。
