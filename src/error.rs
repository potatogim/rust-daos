// Copyright (c) 2026 Jihyeon Gim
// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions transcribed from DAOS headers, BSD-2-Clause-Patent, see LICENSE-THIRD-PARTY

//! Error types.
//!
//! libdaos calls (`daos_*`) return negative DER codes; every `dfs_*` call
//! returns 0 or a positive errno because DFS converts DER codes itself. The
//! two cases are kept apart: [`Error::Daos`] carries a [`DerCode`],
//! [`Error::Dfs`] carries an errno.

use std::fmt;
use std::io;

use crate::der_table::DER_TABLE;

/// Result type of this crate.
pub type Result<T> = std::result::Result<T, Error>;

/// `ERESTART`, which DAOS returns for `DER_TX_RESTART`. It exists only on
/// Linux; elsewhere (where DAOS does not run) the mapping falls back to `EIO`.
#[cfg(any(target_os = "linux", target_os = "android"))]
const ERRNO_RESTART: i32 = libc::ERESTART;
#[cfg(not(any(target_os = "linux", target_os = "android")))]
const ERRNO_RESTART: i32 = libc::EIO;

/// A DER error code from libdaos, stored as its positive enum value.
///
/// libdaos returns these as negative numbers; [`DerCode::from_rc`] accepts
/// either sign. The name and description table is embedded (generated from
/// the DAOS 2.8.1 `daos_errno.h`), so formatting needs no loaded library.
///
/// ```
/// use daos::DerCode;
/// let code = DerCode::from_rc(-1005);
/// assert_eq!(code, DerCode::NONEXIST);
/// assert_eq!(code.name(), "DER_NONEXIST");
/// assert_eq!(code.to_errno(), libc::ENOENT);
/// ```
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct DerCode(pub(crate) i32);

impl DerCode {
    /// Build from a libdaos return value (negative) or a positive DER value.
    /// `i32::MIN` has no positive counterpart and becomes an unknown code
    /// (`i32::MAX`).
    pub fn from_rc(rc: i32) -> DerCode {
        DerCode(rc.checked_abs().unwrap_or(i32::MAX))
    }

    /// Positive DER value, for example 1005 for `DER_NONEXIST`.
    pub fn value(self) -> i32 {
        self.0
    }

    fn entry(self) -> Option<&'static (i32, &'static str, &'static str)> {
        DER_TABLE.iter().find(|e| e.0 == self.0)
    }

    /// Symbolic name such as `"DER_NONEXIST"`, or `"DER_UNKNOWN"` when the
    /// value is not in the embedded table.
    pub fn name(self) -> &'static str {
        self.entry().map_or("DER_UNKNOWN", |e| e.1)
    }

    /// Description from `daos_errno.h`, or `"Unknown error"` for an unknown value.
    pub fn description(self) -> &'static str {
        self.entry().map_or("Unknown error", |e| e.2)
    }

    /// True when the value is in the embedded table.
    pub fn is_known(self) -> bool {
        self.entry().is_some()
    }

    /// The errno that DAOS itself would report for this code.
    ///
    /// Transcription of `daos_der2errno` in `src/include/daos/common.h` at
    /// DAOS v2.8.1. `DER_SUCCESS` (0) gives 0; codes without a specific
    /// mapping give `EIO`.
    pub fn to_errno(self) -> i32 {
        use libc::*;
        match self {
            DerCode(0) => 0,
            DerCode::NO_PERM | DerCode::EP_RO | DerCode::EP_OLD | DerCode::CONT_DESTROYING => EPERM,
            DerCode::ENOENT | DerCode::NONEXIST => ENOENT,
            DerCode::INVAL
            | DerCode::NOTYPE
            | DerCode::NOSCHEMA
            | DerCode::NOLOCAL
            | DerCode::NO_HDL
            | DerCode::IO_INVAL => EINVAL,
            DerCode::KEY2BIG | DerCode::REC2BIG => E2BIG,
            DerCode::EXIST => EEXIST,
            DerCode::UNREACH => EHOSTUNREACH,
            DerCode::NOSPACE => ENOSPC,
            DerCode::ALREADY => EALREADY,
            DerCode::DOS | DerCode::NOMEM => ENOMEM,
            DerCode::TIMEDOUT => ETIMEDOUT,
            DerCode::BUSY | DerCode::EQ_BUSY => EBUSY,
            DerCode::AGAIN => EAGAIN,
            DerCode::PROTO => EPROTO,
            DerCode::IO => EIO,
            DerCode::CANCELED => ECANCELED,
            DerCode::OVERFLOW => EOVERFLOW,
            DerCode::BADPATH | DerCode::NOTDIR => ENOTDIR,
            DerCode::STALE => ESTALE,
            DerCode::TX_RESTART => ERRNO_RESTART,
            DerCode::INPROGRESS => EINPROGRESS,
            DerCode::NOTSUPPORTED => ENOTSUP,
            _ => EIO,
        }
    }
}

impl fmt::Debug for DerCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_known() {
            write!(f, "{}({})", self.name(), self.0)
        } else {
            write!(f, "DER_UNKNOWN({})", self.0)
        }
    }
}

impl fmt::Display for DerCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_known() {
            write!(f, "{}(-{}): {}", self.name(), self.0, self.description())
        } else {
            write!(f, "DER_UNKNOWN({})", self.0)
        }
    }
}

/// Errors of this crate.
///
/// The enum and its struct-like variants are `#[non_exhaustive]`: match with
/// `..` so that fields added later do not break your code.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Error {
    /// libdfs could not be loaded.
    #[non_exhaustive]
    LibraryNotFound {
        /// Names or paths given to the loader.
        tried: Vec<String>,
        /// Loader message.
        reason: String,
    },
    /// The client library lacks a symbol: at load for the DFS and connect
    /// calls, at first use for the other groups (pool and container
    /// management, objects, key-value, arrays, transactions).
    #[non_exhaustive]
    MissingSymbol {
        /// Symbol name.
        name: &'static str,
        /// Library name or path.
        library: String,
    },
    /// Not 64-bit Linux.
    UnsupportedPlatform,
    /// A libdaos call (`daos_*`) failed with a DER code.
    #[non_exhaustive]
    Daos {
        /// C function name (the name in the headers, for example
        /// `"daos_pool_connect"` for the exported `daos_pool_connect2`).
        op: &'static str,
        /// The DER code.
        code: DerCode,
        /// Pool or container string the call was about, when there is one.
        /// Object calls carry none (keys are binary and often large).
        target: Option<String>,
    },
    /// A libdfs call failed with an errno.
    #[non_exhaustive]
    Dfs {
        /// C function name.
        op: &'static str,
        /// Positive errno returned by libdfs.
        errno: i32,
        /// Path the call was about, when there is one.
        path: Option<String>,
    },
    /// Fewer bytes than required.
    #[non_exhaustive]
    ShortRead {
        /// File path.
        path: String,
        /// Offset of the read.
        offset: u64,
        /// Bytes requested.
        wanted: usize,
        /// Bytes returned.
        got: usize,
    },
    /// Path rejected before any DAOS call.
    #[non_exhaustive]
    InvalidPath {
        /// The path as given.
        path: String,
        /// Why it was rejected.
        reason: &'static str,
    },
    /// Argument rejected before any DAOS call.
    InvalidArgument(&'static str),
    /// A key read back from DAOS is not UTF-8 where the API returns
    /// `String` (the keys of a [`Kv`](crate::Kv)).
    #[non_exhaustive]
    NotUtf8 {
        /// The key as stored.
        bytes: Vec<u8>,
    },
    /// The caller's buffer cannot hold the value or key (`DER_REC2BIG`,
    /// `DER_KEY2BIG`, or a reported size larger than the buffer).
    #[non_exhaustive]
    BufferTooSmall {
        /// C function name.
        op: &'static str,
        /// Bytes needed, as reported by libdaos; 0 when not reported.
        needed: u64,
    },
    /// The operation exists in DAOS but this crate version does not offer it
    /// for this object or library (for example key enumeration on objects of
    /// an erasure-coded class).
    #[non_exhaustive]
    Unsupported {
        /// What is not offered.
        what: &'static str,
    },
    /// DAOS used in a fork child.
    #[non_exhaustive]
    Forked {
        /// Process that initialised DAOS or created the handle.
        created_pid: u32,
        /// Current process.
        current_pid: u32,
    },
    /// The mount, pool or container connection was closed.
    Closed,
    /// `close(timeout)` timed out: operations still ran, transactions were
    /// still open, or (for a pool) containers of the pool were still open.
    /// Also returned by a container destroy while this process has the
    /// container open. Nothing was closed or destroyed.
    #[non_exhaustive]
    Busy {
        /// Operations that held the handle when the timeout expired. For a
        /// container close whose DFS mount was still in use, the DFS calls
        /// of that mount.
        in_flight: usize,
        /// Transactions of the container still open.
        transactions: usize,
        /// Connections that keep the request from proceeding: the container
        /// connections of the pool for [`Pool::close`](crate::Pool::close),
        /// 1 for a container destroy while this process has the container
        /// open (or is opening it), 0 otherwise.
        open_connections: usize,
    },
    /// [`init`](crate::init) was not called (or did not succeed) before connecting.
    NotInitialized,
    /// [`init`](crate::init) was called again with a different library path.
    AlreadyInitialized,
    /// The request conflicts with process-wide state fixed by an earlier call.
    #[non_exhaustive]
    Conflict {
        /// What conflicts: `"system"` or `"library"`.
        what: &'static str,
        /// The value already fixed in this process.
        existing: String,
        /// The value of this request.
        requested: String,
    },
}

impl Error {
    pub(crate) fn dfs(op: &'static str, errno: i32, path: impl Into<String>) -> Error {
        Error::Dfs {
            op,
            errno,
            path: Some(path.into()),
        }
    }

    /// `Error::Daos` from a libdaos return value, without a target.
    pub(crate) fn daos(op: &'static str, rc: i32) -> Error {
        Error::Daos {
            op,
            code: DerCode::from_rc(rc),
            target: None,
        }
    }

    /// `Error::Daos` from a libdaos return value about a pool or container.
    pub(crate) fn daos_at(op: &'static str, rc: i32, target: impl Into<String>) -> Error {
        Error::Daos {
            op,
            code: DerCode::from_rc(rc),
            target: Some(target.into()),
        }
    }

    pub(crate) fn busy(in_flight: usize, transactions: usize) -> Error {
        Error::Busy {
            in_flight,
            transactions,
            open_connections: 0,
        }
    }

    /// `Busy` because of open connections (see the field).
    pub(crate) fn busy_open(open_connections: usize) -> Error {
        Error::Busy {
            in_flight: 0,
            transactions: 0,
            open_connections,
        }
    }

    /// The DER code, for [`Error::Daos`].
    pub fn der(&self) -> Option<DerCode> {
        match self {
            Error::Daos { code, .. } => Some(*code),
            _ => None,
        }
    }

    /// errno for this error: the DFS errno, or [`DerCode::to_errno`] for
    /// [`Error::Daos`]. `None` for every other variant.
    pub fn raw_os_error(&self) -> Option<i32> {
        match self {
            Error::Dfs { errno, .. } => Some(*errno),
            Error::Daos { code, .. } => Some(code.to_errno()),
            _ => None,
        }
    }

    /// The matching [`std::io::ErrorKind`].
    pub fn kind(&self) -> io::ErrorKind {
        match self {
            Error::Dfs { errno, .. } => errno_kind(*errno),
            Error::Daos { code, .. } => errno_kind(code.to_errno()),
            Error::ShortRead { .. } => io::ErrorKind::UnexpectedEof,
            Error::NotUtf8 { .. } => io::ErrorKind::InvalidData,
            Error::InvalidPath { .. }
            | Error::InvalidArgument(_)
            | Error::BufferTooSmall { .. } => io::ErrorKind::InvalidInput,
            Error::LibraryNotFound { .. }
            | Error::MissingSymbol { .. }
            | Error::UnsupportedPlatform
            | Error::Unsupported { .. } => io::ErrorKind::Unsupported,
            Error::Closed
            | Error::Forked { .. }
            | Error::Busy { .. }
            | Error::NotInitialized
            | Error::AlreadyInitialized
            | Error::Conflict { .. } => io::ErrorKind::Other,
        }
    }

    /// The error is `ENOENT`.
    pub fn is_not_found(&self) -> bool {
        self.raw_os_error() == Some(libc::ENOENT)
    }

    /// The error is `EEXIST`.
    pub fn is_already_exists(&self) -> bool {
        self.raw_os_error() == Some(libc::EEXIST)
    }

    /// The error is `EACCES` or `EPERM`. DFS reports permission failures as `EPERM`.
    pub fn is_permission_denied(&self) -> bool {
        matches!(self.raw_os_error(), Some(libc::EACCES) | Some(libc::EPERM))
    }

    /// The error is `DER_TX_RESTART`: the transaction conflicted and must be
    /// restarted.
    pub fn is_tx_restart(&self) -> bool {
        self.der() == Some(DerCode::TX_RESTART)
    }

    /// The error is [`Error::Busy`] or a DAOS or DFS "busy" code
    /// (`DER_BUSY`, `EBUSY`).
    pub fn is_busy(&self) -> bool {
        matches!(self, Error::Busy { .. }) || self.raw_os_error() == Some(libc::EBUSY)
    }

    /// The error is `DER_NO_HDL`: the handle was closed or never existed.
    pub fn is_no_handle(&self) -> bool {
        self.der() == Some(DerCode::NO_HDL)
    }
}

fn errno_kind(errno: i32) -> io::ErrorKind {
    match errno {
        libc::ENOENT => io::ErrorKind::NotFound,
        libc::EEXIST => io::ErrorKind::AlreadyExists,
        libc::EACCES | libc::EPERM => io::ErrorKind::PermissionDenied,
        libc::EINVAL => io::ErrorKind::InvalidInput,
        libc::ETIMEDOUT => io::ErrorKind::TimedOut,
        libc::ENOTSUP => io::ErrorKind::Unsupported,
        libc::ENOMEM => io::ErrorKind::OutOfMemory,
        libc::EINTR => io::ErrorKind::Interrupted,
        other => io::Error::from_raw_os_error(other).kind(),
    }
}

/// Symbolic name of an errno value on this platform, or `None`.
pub(crate) fn errno_name(errno: i32) -> Option<&'static str> {
    use libc::*;
    let name = match errno {
        EPERM => "EPERM",
        ENOENT => "ENOENT",
        ESRCH => "ESRCH",
        EINTR => "EINTR",
        EIO => "EIO",
        ENXIO => "ENXIO",
        E2BIG => "E2BIG",
        EBADF => "EBADF",
        EAGAIN => "EAGAIN",
        ENOMEM => "ENOMEM",
        EACCES => "EACCES",
        EFAULT => "EFAULT",
        EBUSY => "EBUSY",
        EEXIST => "EEXIST",
        EXDEV => "EXDEV",
        ENODEV => "ENODEV",
        ENOTDIR => "ENOTDIR",
        EISDIR => "EISDIR",
        EINVAL => "EINVAL",
        ENFILE => "ENFILE",
        EMFILE => "EMFILE",
        EFBIG => "EFBIG",
        ENOSPC => "ENOSPC",
        EROFS => "EROFS",
        EMLINK => "EMLINK",
        ERANGE => "ERANGE",
        ENAMETOOLONG => "ENAMETOOLONG",
        ENOSYS => "ENOSYS",
        ENOTEMPTY => "ENOTEMPTY",
        ELOOP => "ELOOP",
        EOVERFLOW => "EOVERFLOW",
        EPROTO => "EPROTO",
        ENOTSUP => "ENOTSUP",
        EALREADY => "EALREADY",
        EINPROGRESS => "EINPROGRESS",
        ETIMEDOUT => "ETIMEDOUT",
        ECONNREFUSED => "ECONNREFUSED",
        EHOSTUNREACH => "EHOSTUNREACH",
        ESTALE => "ESTALE",
        ECANCELED => "ECANCELED",
        EDQUOT => "EDQUOT",
        #[cfg(any(target_os = "linux", target_os = "android"))]
        ERESTART => "ERESTART",
        _ => return None,
    };
    Some(name)
}

/// strerror text for an errno, without the "(os error N)" suffix of std.
fn errno_text(errno: i32) -> String {
    let full = io::Error::from_raw_os_error(errno).to_string();
    match full.rfind(" (os error ") {
        Some(i) => full[..i].to_string(),
        None => full,
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::LibraryNotFound { tried, reason } => {
                write!(f, "DAOS client library not found: tried ")?;
                for (i, t) in tried.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{t:?}")?;
                }
                write!(
                    f,
                    " ({reason}). Install the daos-client package or set RUST_DAOS_LIBDFS / LD_LIBRARY_PATH."
                )
            }
            Error::MissingSymbol { name, library } => {
                write!(f, "DAOS client library {library:?} lacks the required symbol {name}")
            }
            Error::UnsupportedPlatform => write!(
                f,
                "DAOS client libraries are only available on 64-bit Linux; this build cannot use DAOS"
            ),
            Error::Daos { op, code, target } => match target {
                Some(t) => write!(f, "{op}({t:?}) failed: {code}"),
                None => write!(f, "{op} failed: {code}"),
            },
            Error::Dfs { op, errno, path } => {
                match path {
                    Some(p) => write!(f, "{op}({p:?}) failed: ")?,
                    None => write!(f, "{op} failed: ")?,
                }
                match errno_name(*errno) {
                    Some(n) => write!(f, "{n} ({errno}): {}", errno_text(*errno)),
                    None => write!(f, "errno {errno}: {}", errno_text(*errno)),
                }
            }
            Error::ShortRead {
                path,
                offset,
                wanted,
                got,
            } => write!(
                f,
                "short read of {path:?} at offset {offset}: wanted {wanted} bytes, got {got}"
            ),
            Error::InvalidPath { path, reason } => write!(f, "invalid DFS path {path:?}: {reason}"),
            Error::InvalidArgument(what) => write!(f, "invalid argument: {what}"),
            Error::NotUtf8 { bytes } => write!(
                f,
                "a stored key is not UTF-8 ({} bytes); read it as bytes",
                bytes.len()
            ),
            Error::BufferTooSmall { op, needed } => {
                if *needed > 0 {
                    write!(f, "{op}: the buffer is too small, {needed} bytes needed")
                } else {
                    write!(f, "{op}: the buffer is too small")
                }
            }
            Error::Unsupported { what } => write!(f, "not supported by this crate version: {what}"),
            Error::Forked {
                created_pid,
                current_pid,
            } => write!(
                f,
                "DAOS used in a fork child (initialised in pid {created_pid}, now pid {current_pid}). \
                 libdaos and DFS mount state do not survive fork. Start workers with spawn, or fork \
                 before daos::init and initialise in each process."
            ),
            Error::Closed => write!(f, "the mount, pool or container connection was closed"),
            Error::Busy {
                in_flight,
                transactions,
                open_connections,
            } => write!(
                f,
                "busy: {in_flight} operations in flight, {transactions} open transactions, \
                 {open_connections} open connections; nothing was closed or destroyed"
            ),
            Error::NotInitialized => write!(
                f,
                "DAOS is not initialised in this process: call unsafe daos::init() first, before \
                 other threads that read or write the environment are started"
            ),
            Error::AlreadyInitialized => write!(
                f,
                "daos::init was already called in this process with a different library path"
            ),
            Error::Conflict {
                what,
                existing,
                requested,
            } => write!(
                f,
                "{what} conflict: this process already uses {existing:?}, request asks for \
                 {requested:?} (one DAOS system and one client library per process)"
            ),
        }
    }
}

impl std::error::Error for Error {}

impl From<Error> for io::Error {
    /// [`Error::Dfs`] and [`Error::Daos`] become `io::Error::from_raw_os_error`
    /// so that `raw_os_error()` works on the result; the operation name, path
    /// and DER code are not carried over. Every other variant is wrapped with
    /// `io::Error::new(kind, error)`.
    fn from(e: Error) -> io::Error {
        match e {
            Error::Dfs { errno, .. } => io::Error::from_raw_os_error(errno),
            Error::Daos { code, .. } => io::Error::from_raw_os_error(code.to_errno()),
            other => io::Error::new(other.kind(), other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn table_values_and_names_are_unique() {
        let mut values = HashSet::new();
        let mut names = HashSet::new();
        for (v, n, d) in DER_TABLE {
            assert!(values.insert(*v), "duplicate value {v}");
            assert!(names.insert(*n), "duplicate name {n}");
            assert!(n.starts_with("DER_"));
            assert!(!d.is_empty());
        }
        assert_eq!(DER_TABLE.len(), 99);
    }

    fn fixture(text: &str) -> Vec<(String, i32)> {
        text.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| {
                let mut it = l.split_whitespace();
                let n = it.next().unwrap().to_string();
                let v: i32 = it.next().unwrap().parse().unwrap();
                (n, v)
            })
            .collect()
    }

    #[test]
    fn to_errno_matches_daos_der2errno_fixture() {
        let fx = fixture(include_str!("../abi/der2errno-v2.8.1.txt"));
        assert_eq!(fx.len(), DER_TABLE.len());
        for (name, errno) in fx {
            let entry = DER_TABLE.iter().find(|e| e.1 == name).unwrap();
            assert_eq!(DerCode(entry.0).to_errno(), errno, "{name}");
        }
    }

    #[test]
    fn table_is_superset_of_older_headers() {
        for text in [
            include_str!("../abi/layout-v2.6.0.txt"),
            include_str!("../abi/layout-v2.6.4.txt"),
            include_str!("../abi/layout-v2.8.1.txt"),
        ] {
            for line in text.lines().filter(|l| l.starts_with("der ")) {
                let mut it = line.split_whitespace().skip(1);
                let name = it.next().unwrap();
                let value: i32 = it.next().unwrap().parse().unwrap();
                let code = DerCode(value);
                assert_eq!(code.name(), name);
            }
        }
    }

    #[test]
    fn der_code_basics() {
        assert_eq!(DerCode::from_rc(-1005), DerCode::NONEXIST);
        assert_eq!(DerCode::from_rc(1005), DerCode::NONEXIST);
        assert_eq!(DerCode::NONEXIST.value(), 1005);
        assert_eq!(DerCode::AGENT_COMM.value(), 2034);
        assert_eq!(
            DerCode::NONEXIST.description(),
            "The specified entity does not exist"
        );
        assert_eq!(DerCode(7777).name(), "DER_UNKNOWN");
        assert_eq!(format!("{:?}", DerCode(7777)), "DER_UNKNOWN(7777)");
        assert_eq!(format!("{:?}", DerCode::NONEXIST), "DER_NONEXIST(1005)");
        assert_eq!(DerCode(7777).to_errno(), libc::EIO);
        assert_eq!(DerCode(0).to_errno(), 0);
        assert_eq!(DerCode::from_rc(0).to_errno(), 0);
        assert_eq!(DerCode::from_rc(i32::MIN).value(), i32::MAX);
        assert!(!DerCode::from_rc(i32::MIN).is_known());
    }

    #[test]
    fn display_formats() {
        let e = Error::daos("daos_init", -2034);
        assert_eq!(
            e.to_string(),
            "daos_init failed: DER_AGENT_COMM(-2034): Agent communication error"
        );
        let e = Error::daos_at("daos_pool_connect", -1005, "tank");
        assert_eq!(
            e.to_string(),
            "daos_pool_connect(\"tank\") failed: DER_NONEXIST(-1005): The specified entity does not exist"
        );
        let e = Error::BufferTooSmall {
            op: "daos_kv_get",
            needed: 10,
        };
        assert_eq!(
            e.to_string(),
            "daos_kv_get: the buffer is too small, 10 bytes needed"
        );
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
        let e = Error::Unsupported { what: "x" };
        assert_eq!(e.kind(), io::ErrorKind::Unsupported);
        let e = Error::busy(1, 2);
        assert!(e.is_busy());
        assert!(Error::daos("x", -1012).is_busy());
        assert!(Error::daos("x", -1002).is_no_handle());
        assert!(Error::daos("x", -2025).is_tx_restart());
        let e = Error::dfs("dfs_open", libc::ENOENT, "/train/img_000123.npz");
        assert_eq!(
            e.to_string(),
            "dfs_open(\"/train/img_000123.npz\") failed: ENOENT (2): No such file or directory"
        );
        let e = Error::dfs("dfs_connect", libc::EPERM, "tank/data");
        assert_eq!(
            e.to_string(),
            "dfs_connect(\"tank/data\") failed: EPERM (1): Operation not permitted"
        );
        let e = Error::LibraryNotFound {
            tried: vec!["libdfs.so".into()],
            reason: "libdfs.so: cannot open shared object file: No such file or directory".into(),
        };
        assert_eq!(
            e.to_string(),
            "DAOS client library not found: tried \"libdfs.so\" (libdfs.so: cannot open shared \
             object file: No such file or directory). Install the daos-client package or set \
             RUST_DAOS_LIBDFS / LD_LIBRARY_PATH."
        );
        let e = Error::Forked {
            created_pid: 1234,
            current_pid: 1250,
        };
        assert!(e.to_string().starts_with(
            "DAOS used in a fork child (initialised in pid 1234, now pid 1250). libdaos and DFS \
             mount state do not survive fork."
        ));
    }

    #[test]
    fn kinds() {
        assert_eq!(
            Error::dfs("x", libc::ENOENT, "/").kind(),
            io::ErrorKind::NotFound
        );
        assert_eq!(
            Error::dfs("x", libc::EEXIST, "/").kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(
            Error::dfs("x", libc::EPERM, "/").kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            Error::dfs("x", libc::EACCES, "/").kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            Error::dfs("x", libc::EINVAL, "/").kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            Error::dfs("x", libc::ETIMEDOUT, "/").kind(),
            io::ErrorKind::TimedOut
        );
        assert_eq!(
            Error::dfs("x", libc::ENOTSUP, "/").kind(),
            io::ErrorKind::Unsupported
        );
        assert_eq!(
            Error::dfs("x", libc::ENOMEM, "/").kind(),
            io::ErrorKind::OutOfMemory
        );
        assert_eq!(
            Error::dfs("x", libc::EINTR, "/").kind(),
            io::ErrorKind::Interrupted
        );
        assert_eq!(
            Error::dfs("x", libc::EIO, "/").kind(),
            io::Error::from_raw_os_error(libc::EIO).kind()
        );
        let d = Error::daos("daos_init", -1005);
        assert_eq!(d.kind(), io::ErrorKind::NotFound);
        assert!(d.is_not_found());
        assert_eq!(d.der(), Some(DerCode::NONEXIST));
        let s = Error::ShortRead {
            path: "/a".into(),
            offset: 0,
            wanted: 2,
            got: 1,
        };
        assert_eq!(s.kind(), io::ErrorKind::UnexpectedEof);
        assert_eq!(Error::Closed.kind(), io::ErrorKind::Other);
        assert_eq!(
            Error::UnsupportedPlatform.kind(),
            io::ErrorKind::Unsupported
        );
        assert!(Error::dfs("x", libc::EEXIST, "/").is_already_exists());
        assert!(Error::dfs("x", libc::EPERM, "/").is_permission_denied());
    }

    #[test]
    fn io_error_conversion_keeps_errno() {
        let e: io::Error = Error::dfs("dfs_open", libc::ENOENT, "/x").into();
        assert_eq!(e.raw_os_error(), Some(libc::ENOENT));
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
        let e: io::Error = Error::daos("daos_init", -1001).into();
        assert_eq!(e.raw_os_error(), Some(libc::EPERM));
        let e: io::Error = Error::Closed.into();
        assert_eq!(e.raw_os_error(), None);
        assert_eq!(e.kind(), io::ErrorKind::Other);
        let e: io::Error = Error::InvalidArgument("x").into();
        assert_eq!(e.raw_os_error(), None);
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
    }
}
