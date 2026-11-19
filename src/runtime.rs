// Copyright (c) 2026 Jihyeon Gim
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Process-wide DAOS runtime: library loading and initialisation.
//!
//! [`init`] runs `daos_init()` then `dfs_init()` once per process. The crate
//! never calls `dfs_fini()` or `daos_fini()`: libdaos keeps a per-thread
//! private event for blocking calls whose "initialised" flag is thread local,
//! and finalisation clears it only on the calling thread. A finalised and
//! re-initialised library would let other threads use events that point at a
//! destroyed network context. The runtime therefore lives until the process
//! exits; closing mounts only disconnects them.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use crate::error::{Error, Result};
use crate::fork::{Stamp, OWNER};
use crate::sys::api::Raw;

/// Environment variable naming libdfs (absolute path or soname).
pub const LIBDFS_ENV: &str = "RUST_DAOS_LIBDFS";
/// Name given to the loader when nothing else is configured.
pub const DEFAULT_LIBDFS: &str = "libdfs.so";

/// What the loaded client library offers.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct LibraryInfo {
    /// Name or path that dlopen accepted.
    pub path: String,
    /// `daos_reinit` is present (DAOS 2.6.4 or newer). Information only: the
    /// crate never calls it.
    pub has_reinit: bool,
    /// `dfs_connect_snap` is present (DAOS 2.8 or newer). Information only.
    pub has_connect_snap: bool,
    /// Functions found only under their plain header name, without the
    /// `*2` export the headers map them to (for example
    /// `"daos_pool_connect"` when `daos_pool_connect2` is absent).
    pub plain_names: Vec<&'static str>,
    /// First missing symbol of each optional group (pool and container
    /// management, objects, key-value, arrays, transactions). Calls of such a
    /// group return [`Error::MissingSymbol`](crate::Error::MissingSymbol).
    pub missing_symbols: Vec<&'static str>,
}

/// Options of [`init`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct InitOptions {
    /// Explicit path or soname of libdfs. `None` = `RUST_DAOS_LIBDFS`, then
    /// `"libdfs.so"` through the normal loader search.
    pub library_path: Option<PathBuf>,
}

impl InitOptions {
    /// Default options.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set [`InitOptions::library_path`].
    pub fn library_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.library_path = Some(path.into());
        self
    }
}

/// What [`init`] observed. Nothing here is a guarantee about libdaos internals.
///
/// The environment values reflect the process environment, not necessarily
/// what an earlier DAOS initialisation in this process (by another library
/// such as pydaos) applied.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct InitReport {
    /// The loaded library.
    pub library: LibraryInfo,
    /// `CRT_MRC_ENABLE` as found in the environment before `daos_init`
    /// (`None` = unset). This is what the launcher set.
    pub crt_mrc_enable_before: Option<String>,
    /// `CRT_MRC_ENABLE` as found after `daos_init` (server-provided client
    /// variables are applied during initialisation without overwriting).
    pub crt_mrc_enable_after: Option<String>,
    /// `FI_MR_CACHE_MAX_COUNT` as found after `daos_init`.
    pub fi_mr_cache_max_count_after: Option<String>,
    /// Human-readable warnings for the caller to log (the crate has no logger).
    pub warnings: Vec<String>,
}

/// Warning text for an MR cache that appears enabled.
const MR_CACHE_WARNING: &str =
    "provider MR cache appears enabled; multi-threaded processes without MPI have \
     crashed in this configuration; set CRT_MRC_ENABLE=0 in the launcher";

/// Compute the warnings of an [`InitReport`] from the observed values.
pub(crate) fn mr_cache_warnings(before: Option<&str>, fi_after: Option<&str>) -> Vec<String> {
    let mut w = Vec::new();
    match before {
        None => {
            if fi_after != Some("0") {
                w.push(format!(
                    "CRT_MRC_ENABLE was not set before daos::init and FI_MR_CACHE_MAX_COUNT is {}: {}",
                    fi_after.map_or_else(|| "unset".to_string(), |v| format!("{v:?}")),
                    MR_CACHE_WARNING
                ));
            }
        }
        Some(v) if v.trim() != "0" => {
            w.push(format!(
                "CRT_MRC_ENABLE={v:?} was set before daos::init: {MR_CACHE_WARNING}"
            ));
        }
        Some(_) => {}
    }
    w
}

/// Outcome of the init sequence on a backend.
#[derive(Debug)]
pub(crate) enum InitFailure {
    /// `daos_init` failed; nothing was initialised; a later call may retry.
    Retryable(Error),
    /// `dfs_init` failed after `daos_init` succeeded. The runtime is unusable
    /// for the rest of the process (no `daos_fini` is called, see module docs).
    Fatal(Error),
}

/// The init sequence: `daos_init`, then `dfs_init`. Never calls a fini.
///
/// `getenv` reads the environment; it is a parameter so that unit tests can
/// run without touching the process environment.
pub(crate) fn run_init_sequence(
    backend: &dyn Raw,
    library: LibraryInfo,
    getenv: &dyn Fn(&str) -> Option<String>,
) -> std::result::Result<InitReport, InitFailure> {
    let before = getenv("CRT_MRC_ENABLE");
    // SAFETY: daos_init has no arguments; the caller of init guarantees that
    // no other thread uses the environment while it runs.
    let rc = unsafe { backend.daos_init() };
    if rc != 0 {
        return Err(InitFailure::Retryable(Error::daos("daos_init", rc)));
    }
    // SAFETY: as above; daos_init succeeded.
    let rc = unsafe { backend.dfs_init() };
    if rc != 0 {
        return Err(InitFailure::Fatal(Error::Dfs {
            op: "dfs_init",
            errno: rc,
            path: None,
        }));
    }
    let after = getenv("CRT_MRC_ENABLE");
    let fi_after = getenv("FI_MR_CACHE_MAX_COUNT");
    let warnings = mr_cache_warnings(before.as_deref(), fi_after.as_deref());
    Ok(InitReport {
        library,
        crt_mrc_enable_before: before,
        crt_mrc_enable_after: after,
        fi_mr_cache_max_count_after: fi_after,
        warnings,
    })
}

/// The initialised runtime.
pub(crate) struct Runtime {
    pub(crate) backend: Arc<dyn Raw>,
    pub(crate) report: InitReport,
    pub(crate) stamp: Stamp,
    requested_path: Option<PathBuf>,
}

// Results are published in cells that are read without a lock, so that no
// reader waits for an init running in another thread (or, after a fork, in a
// thread that no longer exists).
static RUNTIME: OnceLock<Runtime> = OnceLock::new();
/// The error of a failed `dfs_init`: DAOS is unusable in this process.
static FATAL: OnceLock<Error> = OnceLock::new();
/// Serialises init. Taken only after [`OWNER`] was claimed and while
/// `OWNER.enter_init()` counts the caller.
static INIT_LOCK: Mutex<()> = Mutex::new(());

/// The runtime, if [`init`] completed. Checks process ownership and the fork
/// stamp first.
pub(crate) fn runtime() -> Result<&'static Runtime> {
    OWNER.claim()?;
    match RUNTIME.get() {
        Some(rt) => {
            rt.stamp.check_cold()?;
            Ok(rt)
        }
        None => Err(Error::NotInitialized),
    }
}

/// Load libdfs and initialise DAOS for this process (`daos_init`, then
/// `dfs_init`). Idempotent: later calls return the first report.
///
/// # Safety
///
/// `daos_init` modifies the process environment (it applies server-provided
/// client variables and provider settings). The caller must guarantee that no
/// other thread reads or writes the process environment while this call runs.
/// The simplest way is to call it before any other thread is started.
///
/// # Errors
///
/// - [`Error::LibraryNotFound`], [`Error::MissingSymbol`],
///   [`Error::UnsupportedPlatform`] when libdfs cannot be used.
/// - [`Error::Daos`] when `daos_init` fails (for example `DER_AGENT_COMM` when
///   no agent runs). A later call may retry.
/// - [`Error::Dfs`] when `dfs_init` fails. DAOS is then unusable in this
///   process and every later call returns the same error.
/// - [`Error::AlreadyInitialized`] when called again with a different
///   [`InitOptions::library_path`].
/// - [`Error::Forked`] in a fork child of a process that had initialised DAOS
///   or whose `init` failed for good (the child gets `Forked`, not the stored
///   `dfs_init` error), or in which another thread was inside `init` or the
///   library load at the moment of the fork. The child does not wait for locks that a parent
///   thread held. Also where the process id cannot be read and the kernel
///   cannot clear the crate's token page in children; nothing is loaded then
///   and the call can be repeated.
///
/// # Example
///
/// ```no_run
/// fn main() -> Result<(), daos::Error> {
///     // First statement of main, before any thread exists.
///     let report = unsafe { daos::init(&daos::InitOptions::default())? };
///     for w in &report.warnings {
///         eprintln!("daos: {w}");
///     }
///     Ok(())
/// }
/// ```
pub unsafe fn init(options: &InitOptions) -> Result<&'static InitReport> {
    init_with(options, |path| {
        let lib = loader::load(path)?;
        let info = loader::info_of(&lib);
        let backend: Arc<dyn Raw> = lib;
        Ok((backend, info))
    })
}

/// `init` over a given loader (unit tests pass their own backend).
fn init_with(
    options: &InitOptions,
    load: impl FnOnce(Option<&PathBuf>) -> Result<(Arc<dyn Raw>, LibraryInfo)>,
) -> Result<&'static InitReport> {
    // Before any lock: a fork child must not wait for the init lock or the
    // loader lock that a parent thread held at the fork.
    OWNER.claim()?;
    if let Some(rt) = RUNTIME.get() {
        rt.stamp.check_cold()?;
        if rt.requested_path != options.library_path {
            return Err(Error::AlreadyInitialized);
        }
        return Ok(&rt.report);
    }
    if let Some(e) = FATAL.get() {
        return Err(e.clone());
    }
    // Counted from before the lock is taken until after it is released
    // (declaration order: `_serial` drops first).
    let _active = OWNER.enter_init();
    let _serial = INIT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(rt) = RUNTIME.get() {
        if rt.requested_path != options.library_path {
            return Err(Error::AlreadyInitialized);
        }
        return Ok(&rt.report);
    }
    if let Some(e) = FATAL.get() {
        return Err(e.clone());
    }
    // Before anything is loaded or initialised: a runtime stored with a
    // stamp that fails its own check would refuse this very process for
    // good.
    let stamp = Stamp::fresh()?;
    let (backend, info) = load(options.library_path.as_ref())?;
    let getenv = |k: &str| std::env::var_os(k).map(|v| v.to_string_lossy().into_owned());
    match run_init_sequence(backend.as_ref(), info, &getenv) {
        Ok(report) => {
            let rt = Runtime {
                backend,
                report,
                stamp,
                requested_path: options.library_path.clone(),
            };
            let _ = RUNTIME.set(rt);
            OWNER.mark_init_done();
            Ok(&RUNTIME.get().expect("runtime was just set").report)
        }
        Err(InitFailure::Retryable(e)) => Err(e),
        Err(InitFailure::Fatal(e)) => {
            let _ = FATAL.set(e.clone());
            OWNER.mark_init_done();
            Err(e)
        }
    }
}

/// Report of the completed [`init`], if any.
pub fn init_report() -> Option<&'static InitReport> {
    RUNTIME.get().map(|rt| &rt.report)
}

/// Load libdfs if it is not loaded yet and report what it offers. Never
/// initialises DAOS.
///
/// When a library is already loaded (by [`init`] or an earlier call), its
/// information is returned. Otherwise `RUST_DAOS_LIBDFS`, then `"libdfs.so"`
/// is loaded.
///
/// # Errors
///
/// The load errors of [`init`], and [`Error::Forked`] in a fork child of a
/// process in which another thread was inside [`init`] or the library load
/// at the moment of the fork (the child does not wait for their locks).
/// A fork child of a process whose [`init`] failed for good (`dfs_init`
/// failed) also gets [`Error::Forked`], from this function and from [`init`],
/// instead of the error stored in the parent.
pub fn library_info() -> Result<LibraryInfo> {
    if let Some(rt) = RUNTIME.get() {
        return Ok(rt.report.library.clone());
    }
    OWNER.claim()?;
    let lib = loader::load_any()?;
    Ok(loader::info_of(&lib))
}

/// True when libdfs can be loaded. Never panics, never initialises DAOS.
/// False where [`library_info`] returns an error, also [`Error::Forked`].
pub fn is_available() -> bool {
    matches!(std::panic::catch_unwind(library_info), Ok(Ok(_)))
}

#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
mod loader {
    use super::*;
    use crate::sys::lib_dfs::LibDfs;

    static LOADED: Mutex<Option<(String, Arc<LibDfs>)>> = Mutex::new(None);

    fn resolve_name(requested: Option<&PathBuf>) -> String {
        if let Some(p) = requested {
            return p.to_string_lossy().into_owned();
        }
        match std::env::var_os(LIBDFS_ENV) {
            Some(v) if !v.is_empty() => v.to_string_lossy().into_owned(),
            _ => DEFAULT_LIBDFS.to_string(),
        }
    }

    /// Load the requested library. A library already loaded under another
    /// name is a [`Error::Conflict`]: the crate never loads two libdfs copies.
    pub(super) fn load(requested: Option<&PathBuf>) -> Result<Arc<LibDfs>> {
        let name = resolve_name(requested);
        // Counted from before the lock is taken until after it is released
        // (declaration order: `loaded` drops first).
        let _active = OWNER.enter_loader();
        let mut loaded = LOADED.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((existing, lib)) = loaded.as_ref() {
            if *existing == name {
                return Ok(lib.clone());
            }
            return Err(Error::Conflict {
                what: "library",
                existing: existing.clone(),
                requested: name,
            });
        }
        let lib = Arc::new(LibDfs::load(&name)?);
        *loaded = Some((name, lib.clone()));
        Ok(lib)
    }

    /// The loaded library, or load the default one.
    pub(super) fn load_any() -> Result<Arc<LibDfs>> {
        {
            let _active = OWNER.enter_loader();
            let loaded = LOADED.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((_, lib)) = loaded.as_ref() {
                return Ok(lib.clone());
            }
        }
        load(None)
    }

    pub(super) fn info_of(lib: &LibDfs) -> LibraryInfo {
        LibraryInfo {
            path: lib.path.clone(),
            has_reinit: lib.has_reinit,
            has_connect_snap: lib.has_connect_snap,
            plain_names: lib.fns.plain_names.clone(),
            missing_symbols: lib.fns.missing.iter().flatten().copied().collect(),
        }
    }
}

#[cfg(not(all(target_os = "linux", target_pointer_width = "64")))]
mod loader {
    use super::*;

    /// Never constructed: there is no loader on this platform.
    pub(super) enum NoLib {}

    // SAFETY: NoLib has no values, so no method can ever be called.
    unsafe impl Raw for NoLib {}

    pub(super) fn load(_: Option<&PathBuf>) -> Result<Arc<NoLib>> {
        Err(Error::UnsupportedPlatform)
    }

    pub(super) fn load_any() -> Result<Arc<NoLib>> {
        Err(Error::UnsupportedPlatform)
    }

    pub(super) fn info_of(lib: &NoLib) -> LibraryInfo {
        match *lib {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::c_int;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Counting fake: records daos_init and dfs_init; any other call panics.
    struct Counting {
        daos_rc: c_int,
        dfs_rc: c_int,
        daos_calls: AtomicUsize,
        dfs_calls: AtomicUsize,
    }

    impl Counting {
        fn new(daos_rc: c_int, dfs_rc: c_int) -> Self {
            Counting {
                daos_rc,
                dfs_rc,
                daos_calls: AtomicUsize::new(0),
                dfs_calls: AtomicUsize::new(0),
            }
        }
    }

    // SAFETY: test fake, no pointers are touched; every other method keeps
    // its default body, which panics.
    unsafe impl Raw for Counting {
        unsafe fn daos_init(&self) -> c_int {
            self.daos_calls.fetch_add(1, Ordering::SeqCst);
            self.daos_rc
        }
        unsafe fn dfs_init(&self) -> c_int {
            self.dfs_calls.fetch_add(1, Ordering::SeqCst);
            self.dfs_rc
        }
    }

    fn info() -> LibraryInfo {
        LibraryInfo {
            path: "fake".into(),
            has_reinit: true,
            has_connect_snap: false,
            plain_names: Vec::new(),
            missing_symbols: Vec::new(),
        }
    }

    fn env(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |k| pairs.iter().find(|p| p.0 == k).map(|p| p.1.to_string())
    }

    #[test]
    fn init_sequence_order_and_success() {
        let fake = Counting::new(0, 0);
        let r = run_init_sequence(
            &fake,
            info(),
            &env(&[("CRT_MRC_ENABLE", "0"), ("FI_MR_CACHE_MAX_COUNT", "0")]),
        )
        .unwrap();
        assert_eq!(fake.daos_calls.load(Ordering::SeqCst), 1);
        assert_eq!(fake.dfs_calls.load(Ordering::SeqCst), 1);
        assert_eq!(r.crt_mrc_enable_before.as_deref(), Some("0"));
        assert!(r.warnings.is_empty());
    }

    #[test]
    fn failed_daos_init_is_retryable_and_skips_dfs_init() {
        let fake = Counting::new(-2034, 0);
        match run_init_sequence(&fake, info(), &env(&[])) {
            Err(InitFailure::Retryable(Error::Daos { op, code, .. })) => {
                assert_eq!(op, "daos_init");
                assert_eq!(code, crate::DerCode::AGENT_COMM);
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(fake.dfs_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn failed_dfs_init_is_fatal() {
        let fake = Counting::new(0, libc::EIO);
        match run_init_sequence(&fake, info(), &env(&[])) {
            Err(InitFailure::Fatal(Error::Dfs { op, errno, .. })) => {
                assert_eq!(op, "dfs_init");
                assert_eq!(errno, libc::EIO);
            }
            other => panic!("unexpected {other:?}"),
        }
        // No fini exists on the backend trait; the counting fake saw exactly
        // the two init calls.
        assert_eq!(fake.daos_calls.load(Ordering::SeqCst), 1);
        assert_eq!(fake.dfs_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn mr_cache_warning_rules() {
        assert!(mr_cache_warnings(Some("0"), None).is_empty());
        assert!(mr_cache_warnings(Some("0"), Some("1000")).is_empty());
        assert!(mr_cache_warnings(None, Some("0")).is_empty());
        let w = mr_cache_warnings(None, None);
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("set CRT_MRC_ENABLE=0 in the launcher"));
        let w = mr_cache_warnings(None, Some("1024"));
        assert!(w[0].contains("\"1024\""));
        let w = mr_cache_warnings(Some("1"), Some("0"));
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("CRT_MRC_ENABLE=\"1\""));
    }

    /// A loader that hands out `fake` and counts its calls.
    fn loader_of<'a>(
        fake: &Arc<Counting>,
        loads: &'a AtomicUsize,
    ) -> impl Fn(Option<&PathBuf>) -> Result<(Arc<dyn Raw>, LibraryInfo)> + 'a {
        let fake = fake.clone();
        move |_| {
            loads.fetch_add(1, Ordering::SeqCst);
            let backend: Arc<dyn Raw> = fake.clone();
            Ok((backend, info()))
        }
    }

    #[test]
    fn init_stores_no_runtime_without_a_pid() {
        use crate::fork::testing::{force_plain, rerun_alone, with_raw_pid_after};
        if rerun_alone("runtime::tests::init_stores_no_runtime_without_a_pid") {
            return;
        }
        force_plain();
        let fake = Arc::new(Counting::new(0, 0));
        let loads = AtomicUsize::new(0);
        let load = loader_of(&fake, &loads);
        // 1: the entry claim reads a pid, every later read fails.
        // 2: the claim inside the stamp succeeds as well, its check fails.
        for n in [1, 2] {
            let r = with_raw_pid_after(n, -1, || init_with(&InitOptions::default(), &load));
            assert!(matches!(r, Err(Error::Forked { .. })), "after {n}: {r:?}");
            assert_eq!(loads.load(Ordering::SeqCst), 0);
            assert_eq!(fake.daos_calls.load(Ordering::SeqCst), 0);
            assert_eq!(fake.dfs_calls.load(Ordering::SeqCst), 0);
            assert!(RUNTIME.get().is_none());
            assert!(init_report().is_none());
        }
        assert!(init_with(&InitOptions::default(), &load).is_ok());
        assert_eq!(loads.load(Ordering::SeqCst), 1);
        let rt = runtime().unwrap();
        assert_ne!(rt.stamp.pid(), 0);
        assert!(rt.stamp.check_cold().is_ok());
    }

    #[test]
    fn fork_child_after_init_is_refused_by_the_owner() {
        use crate::fork::testing::{fork_child, rerun_alone, wait_child};
        if rerun_alone("runtime::tests::fork_child_after_init_is_refused_by_the_owner") {
            return;
        }
        let fake = Arc::new(Counting::new(0, 0));
        let loads = AtomicUsize::new(0);
        let load = loader_of(&fake, &loads);
        assert!(init_with(&InitOptions::default(), &load).is_ok());
        let child = fork_child(|| {
            // Asked directly, before any stamp of the runtime is looked at.
            if !matches!(OWNER.claim(), Err(Error::Forked { .. })) {
                return 1;
            }
            if !matches!(runtime(), Err(Error::Forked { .. })) {
                return 2;
            }
            if !matches!(
                init_with(&InitOptions::default(), &load),
                Err(Error::Forked { .. })
            ) {
                return 3;
            }
            i32::from(loads.load(Ordering::SeqCst) != 1)
        });
        assert_eq!(wait_child(child, std::time::Duration::from_secs(30)), 0);
        assert!(runtime().is_ok());
    }

    #[test]
    fn connect_before_init_is_not_initialized() {
        // Unit tests never call init, so the global runtime is absent here.
        assert!(matches!(runtime(), Err(Error::NotInitialized)));
        assert!(init_report().is_none());
    }
}
