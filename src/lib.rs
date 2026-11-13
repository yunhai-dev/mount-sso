//! Mount S3-compatible object storage as a local POSIX filesystem on macOS.
//!
//! The crate is split into the S3-backed `NFSFileSystem` implementation ([`vfs`]), the
//! object-store client setup ([`backend`]) and the macOS mount plumbing ([`mount`]).

pub mod backend;
pub mod daemon;
pub mod cache;
pub mod mount;
pub mod vfs;
