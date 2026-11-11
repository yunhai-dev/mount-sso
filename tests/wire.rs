//! Wire-level test: drives the real loopback NFSv3 TCP server with hand-rolled ONC RPC
//! messages, the same way a client kernel would.

use std::sync::Arc;
use std::time::Duration;

use mount_sso::vfs::{Config, S3Fs};
use nfsserve::tcp::{NFSTcp, NFSTcpListener};
use object_store::memory::InMemory;
use object_store::path::Path as ObjPath;
use object_store::{ObjectStore, ObjectStoreExt, PutPayload};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

const NFS_PROGRAM: u32 = 100003;
const MOUNT_PROGRAM: u32 = 100005;
const RPC_VERSION: u32 = 2;

struct Rpc {
    stream: TcpStream,
    xid: u32,
}

impl Rpc {
    async fn connect(port: u16) -> Self {
        let stream = tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(("127.0.0.1", port)))
            .await
            .expect("connect timed out")
            .expect("connect");
        stream.set_nodelay(true).unwrap();
        Self { stream, xid: 0 }
    }

    /// Sends one AUTH_NULL RPC call and returns the accepted-reply body.
    async fn call(&mut self, program: u32, version: u32, procedure: u32, args: &[u8]) -> Vec<u8> {
        // Real clients never reuse a transaction id: the server drops those as retransmissions.
        self.xid = self.xid.wrapping_add(1);
        let xid = self.xid;
        let mut message = Vec::new();
        message.extend_from_slice(&xid.to_be_bytes());
        message.extend_from_slice(&0u32.to_be_bytes()); // CALL
        message.extend_from_slice(&RPC_VERSION.to_be_bytes());
        message.extend_from_slice(&program.to_be_bytes());
        message.extend_from_slice(&version.to_be_bytes());
        message.extend_from_slice(&procedure.to_be_bytes());
        // AUTH_NULL credential and verifier.
        for _ in 0..4 {
            message.extend_from_slice(&0u32.to_be_bytes());
        }
        message.extend_from_slice(args);

        let mut framed = Vec::with_capacity(message.len() + 4);
        framed.extend_from_slice(&((message.len() as u32) | 0x8000_0000).to_be_bytes());
        framed.extend_from_slice(&message);
        self.stream.write_all(&framed).await.expect("write call");

        let mut header = [0u8; 4];
        tokio::time::timeout(Duration::from_secs(5), self.stream.read_exact(&mut header))
            .await
            .expect("reply timed out")
            .expect("read header");
        let size = u32::from_be_bytes(header) & 0x7fff_ffff;
        let mut reply = vec![0u8; size as usize];
        self.stream.read_exact(&mut reply).await.expect("read body");

        let mut reader = Reader::new(&reply);
        assert_eq!(reader.u32(), self.xid, "xid must be echoed");
        assert_eq!(reader.u32(), 1, "reply message type");
        assert_eq!(reader.u32(), 0, "MSG_ACCEPTED");
        let verifier_flavor = reader.u32();
        let verifier_len = reader.u32();
        assert_eq!(verifier_flavor, 0);
        assert_eq!(verifier_len, 0);
        assert_eq!(reader.u32(), 0, "accept_stat SUCCESS");
        reader.rest().to_vec()
    }
}

struct Reader<'a> {
    data: &'a [u8],
    offset: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, offset: 0 }
    }

    fn take(&mut self, len: usize) -> &'a [u8] {
        let slice = &self.data[self.offset..self.offset + len];
        self.offset += len;
        slice
    }

    fn u32(&mut self) -> u32 {
        u32::from_be_bytes(self.take(4).try_into().unwrap())
    }

    fn u64(&mut self) -> u64 {
        u64::from_be_bytes(self.take(8).try_into().unwrap())
    }

    fn opaque(&mut self) -> &'a [u8] {
        let len = self.u32() as usize;
        let value = self.take(len);
        let padding = (4 - (len % 4)) % 4;
        self.take(padding);
        value
    }

    fn string(&mut self) -> String {
        String::from_utf8(self.opaque().to_vec()).unwrap()
    }

    /// `fattr3` is a fixed 84-byte record in XDR.
    fn skip_fattr(&mut self) {
        self.take(84);
    }

    fn rest(&mut self) -> &'a [u8] {
        &self.data[self.offset..]
    }
}

fn xdr_string(value: &str) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(value.len() as u32).to_be_bytes());
    out.extend_from_slice(value.as_bytes());
    out.resize(out.len() + (4 - (value.len() % 4)) % 4, 0);
    out
}

fn xdr_fh(handle: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(handle.len() as u32).to_be_bytes());
    out.extend_from_slice(handle);
    out.resize(out.len() + (4 - (handle.len() % 4)) % 4, 0);
    out
}

async fn start_server() -> (u16, mpsc::Receiver<bool>, tokio::task::JoinHandle<()>) {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
    let dir = tempfile::tempdir().unwrap();
    let staging = std::fs::canonicalize(dir.path()).unwrap().join("staging");
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    store
        .put(&ObjPath::from("hello.txt"), PutPayload::from_static(b"hi there"))
        .await
        .unwrap();
    store
        .put(&ObjPath::from("nested/inner.txt"), PutPayload::from_static(b"deep"))
        .await
        .unwrap();
    let cfg = Config {
        root_prefix: String::new(),
        uid: 501,
        gid: 20,
        read_only: false,
        dir_ttl: Duration::from_millis(0),
        flush_delay: Duration::from_millis(20),
        staging_dir: staging.join("staging"),
        max_stage_bytes: 8 * 1024 * 1024,
        cache_dir: staging.join("read"),
        read_cache_bytes: 0,
        read_cache_ttl: Duration::ZERO,
        prefetch_bytes: 0,
    };
    let fs = S3Fs::new(store, cfg).await.unwrap();
    let mut listener = NFSTcpListener::bind("127.0.0.1:0", fs).await.unwrap();
    let port = listener.get_listen_port();
    let (tx, rx) = mpsc::channel(4);
    listener.set_mount_listener(tx);
    let handle = tokio::spawn(async move {
        let _ = listener.handle_forever().await;
    });
    // Keep the temp dir alive for the duration of the server.
    std::mem::forget(dir);
    (port, rx, handle)
}

#[tokio::test]
async fn nfsv3_wire_mount_lookup_getattr() {
    let (port, mut mount_signal, server) = start_server().await;
    let mut rpc = Rpc::connect(port).await;

    // NFS NULL: the server answers, proving RPC framing and dispatch work.
    let reply = rpc.call(NFS_PROGRAM, 3, 0, &[]).await;
    assert!(reply.is_empty(), "NULL has no results");

    // MOUNT v3 MNT "/" → a root file handle.
    let reply = rpc.call(MOUNT_PROGRAM, 3, 1, &xdr_string("/")).await;
    let mut reader = Reader::new(&reply);
    assert_eq!(reader.u32(), 0, "mount status");
    let root_fh = reader.opaque().to_vec();
    assert!(!root_fh.is_empty(), "root file handle must not be empty");
    assert!(mount_signal.recv().await.is_some(), "mount listener must fire");

    // GETATTR on the root handle: it must be a directory.
    let args = xdr_fh(&root_fh);
    let reply = rpc.call(NFS_PROGRAM, 3, 1, &args).await;
    let mut reader = Reader::new(&reply);
    assert_eq!(reader.u32(), 0, "getattr status");
    assert_eq!(reader.u32(), 2, "NF3DIR");
    let _mode = reader.u32();
    let _nlink = reader.u32();
    let _uid = reader.u32();
    let _gid = reader.u32();

    // LOOKUP hello.txt, then GETATTR it and compare the size.
    let mut args = xdr_fh(&root_fh);
    args.extend_from_slice(&xdr_string("hello.txt"));
    let reply = rpc.call(NFS_PROGRAM, 3, 3, &args).await;
    let mut reader = Reader::new(&reply);
    assert_eq!(reader.u32(), 0, "lookup status");
    // LOOKUP3resok carries a bare nfs_fh3 (unlike CREATE/MKDIR, which use post_op_fh3).
    let file_fh = reader.opaque().to_vec();
    assert!(!file_fh.is_empty());

    let reply = rpc.call(NFS_PROGRAM, 3, 1, &xdr_fh(&file_fh)).await;
    let mut reader = Reader::new(&reply);
    assert_eq!(reader.u32(), 0, "getattr status");
    assert_eq!(reader.u32(), 1, "NF3REG");
    let _mode = reader.u32();
    let _nlink = reader.u32();
    let _uid = reader.u32();
    let _gid = reader.u32();
    assert_eq!(reader.u64(), 8, "size of hello.txt");

    // LOOKUP a missing name: NFS3ERR_NOENT (2).
    let mut args = xdr_fh(&root_fh);
    args.extend_from_slice(&xdr_string("nope.txt"));
    let reply = rpc.call(NFS_PROGRAM, 3, 3, &args).await;
    let mut reader = Reader::new(&reply);
    assert_eq!(reader.u32(), 2, "NFS3ERR_NOENT");

    // READDIR on the root returns both entries, sorted.
    let mut args = xdr_fh(&root_fh);
    args.extend_from_slice(&0u64.to_be_bytes()); // cookie
    args.extend_from_slice(&[0u8; 8]); // cookieverf
    args.extend_from_slice(&4096u32.to_be_bytes()); // count
    let reply = rpc.call(NFS_PROGRAM, 3, 16, &args).await;
    let mut reader = Reader::new(&reply);
    assert_eq!(reader.u32(), 0, "readdir status");
    let has_dir_attributes = reader.u32();
    if has_dir_attributes == 1 {
        reader.skip_fattr();
    }
    let _cookie_verifier = reader.take(8);
    let mut names = Vec::new();
    loop {
        if reader.u32() == 0 {
            break;
        }
        let _fileid = reader.u64();
        names.push(reader.string());
        // READDIR entries carry no attributes or file handles; READDIRPLUS does.
        let _cookie = reader.u64();
    }
    assert_eq!(reader.u32(), 1, "eof");
    assert_eq!(names, ["hello.txt", "nested"]);

    // UMNT reports the unmount through the same channel.
    let reply = rpc.call(MOUNT_PROGRAM, 3, 3, &xdr_string("/")).await;
    let mut reader = Reader::new(&reply);
    assert_eq!(reader.u32(), 0, "umount status");
    assert!(matches!(mount_signal.recv().await, Some(false)), "umount listener must fire");

    server.abort();
}
