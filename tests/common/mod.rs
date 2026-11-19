// Copyright (c) 2026 Jihyeon Gim
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared helpers for the test binaries, mostly for the subprocess-based ones
//! (`harness = false`).
//!
//! In those, every case that depends on process-global state (`daos::init`, the
//! environment, fork, the system and pool-mode rules) runs in its own
//! subprocess: the test binary re-executes itself with `RUST_DAOS_CASE=<name>`
//! and checks the child's exit status and output.

#![allow(dead_code)]
// c_long is i64 on the 64-bit targets DAOS runs on, not everywhere.
#![allow(clippy::unnecessary_cast)]

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Exit status a case uses to report "skipped".
pub const SKIP: i32 = 77;

/// Prefix of cases that run only with `--ignored` (they need more DAOS
/// engines than a single-engine test system has; see tests/README.md).
pub const IGNORED_PREFIX: &str = "multi_engine_";

/// One test case.
pub struct Case {
    pub name: &'static str,
    pub run: fn(),
    /// Environment changes for the child: `Some(v)` sets, `None` removes.
    pub env: &'static [(&'static str, Option<&'static str>)],
}

/// Shorthand for a case without environment changes.
pub const fn case(name: &'static str, run: fn()) -> Case {
    Case {
        name,
        run,
        env: &[],
    }
}

/// Run the hidden case `helper_<name>` of this test binary as a subprocess
/// with extra environment; returns true when it exits with status 0.
pub fn run_helper(name: &str, env: &[(&str, &str)]) -> bool {
    let exe = std::env::current_exe().expect("current_exe");
    let mut cmd = Command::new(exe);
    cmd.env("RUST_DAOS_CASE", format!("helper_{name}"));
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("spawn helper");
    if !out.status.success() {
        eprintln!(
            "helper {name} failed: {:?}\n{}{}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    out.status.success()
}

/// Mark the running case as skipped and exit.
pub fn skip(reason: &str) -> ! {
    println!("SKIPPED: {reason}");
    std::process::exit(SKIP);
}

/// Entry point of a `harness = false` test binary.
///
/// `prepare` runs once in the parent and returns environment variables passed
/// to every child (for example the directory of the built C mock).
pub fn run_main(cases: &[Case], prepare: impl FnOnce() -> Vec<(String, String)>) {
    if let Ok(name) = std::env::var("RUST_DAOS_CASE") {
        let case = cases.iter().find(|c| c.name == name).unwrap_or_else(|| {
            eprintln!("unknown case {name}");
            std::process::exit(2)
        });
        (case.run)();
        std::process::exit(0);
    }

    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--list") {
        for c in cases {
            println!("{}: test", c.name);
        }
        return;
    }
    let exact = args.iter().any(|a| a == "--exact");
    let ignored = args
        .iter()
        .any(|a| a == "--ignored" || a == "--include-ignored");
    let filters: Vec<&String> = args.iter().filter(|a| !a.starts_with('-')).collect();
    let selected: Vec<&Case> = cases
        .iter()
        // `helper_*` cases are only run by other cases as subprocesses.
        .filter(|c| !c.name.starts_with("helper_"))
        .filter(|c| ignored || !c.name.starts_with(IGNORED_PREFIX))
        .filter(|c| {
            filters.is_empty()
                || filters.iter().any(|f| {
                    if exact {
                        c.name == f.as_str()
                    } else {
                        c.name.contains(f.as_str())
                    }
                })
        })
        .collect();

    let shared_env = prepare();
    let exe = std::env::current_exe().expect("current_exe");
    let timeout = Duration::from_secs(
        std::env::var("RUST_DAOS_CASE_TIMEOUT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(600),
    );
    println!("\nrunning {} tests (one subprocess each)", selected.len());
    let (mut passed, mut failed, mut skipped) = (0, 0, 0);
    let mut failures = Vec::new();
    let mut skips: Vec<(String, &str)> = Vec::new();
    let start = Instant::now();
    for c in &selected {
        let mut cmd = Command::new(&exe);
        cmd.env("RUST_DAOS_CASE", c.name)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in &shared_env {
            cmd.env(k, v);
        }
        for (k, v) in c.env {
            match v {
                Some(v) => cmd.env(k, v),
                None => cmd.env_remove(k),
            };
        }
        let t0 = Instant::now();
        let (status, out, err) = run_with_timeout(cmd, timeout);
        let secs = t0.elapsed().as_secs_f64();
        match status {
            Some(0) => {
                passed += 1;
                println!("test {} ... ok ({secs:.2}s)", c.name);
            }
            Some(SKIP) => {
                skipped += 1;
                let reason = out
                    .lines()
                    .find(|l| l.starts_with("SKIPPED"))
                    .unwrap_or("SKIPPED");
                println!("test {} ... skipped: {reason}", c.name);
                skips.push((reason.to_string(), c.name));
            }
            other => {
                failed += 1;
                println!("test {} ... FAILED ({other:?}, {secs:.2}s)", c.name);
                failures.push((c.name, out.clone(), err.clone()));
            }
        }
        if std::env::var_os("RUST_DAOS_SHOW_OUTPUT").is_some() && !matches!(status, Some(SKIP)) {
            for l in out.lines().chain(err.lines()) {
                println!("    | {l}");
            }
        }
    }
    for (name, out, err) in &failures {
        println!("\n---- {name} stdout ----\n{out}\n---- {name} stderr ----\n{err}");
    }
    if !skips.is_empty() {
        // Skips per reason, so that a skipped width (for example a class that
        // needs more engines) is not mistaken for a pass.
        skips.sort();
        println!("\nskipped, by reason:");
        let mut i = 0;
        while i < skips.len() {
            let reason = &skips[i].0;
            let names: Vec<&str> = skips
                .iter()
                .skip(i)
                .take_while(|s| &s.0 == reason)
                .map(|s| s.1)
                .collect();
            println!("  {reason}: {}", names.join(", "));
            i += names.len();
        }
    }
    let verdict = if failed == 0 { "ok" } else { "FAILED" };
    println!(
        "\ntest result: {verdict}. {passed} passed; {failed} failed; {skipped} skipped; finished in {:.2}s\n",
        start.elapsed().as_secs_f64()
    );
    if failed > 0 {
        std::process::exit(101);
    }
}

/// Run a case process in its own process group. The leader is polled with
/// `WNOWAIT`, so it stays unreaped and its group id cannot be reused; on
/// exit or timeout the whole group is killed (a case may leave descendants
/// behind), then the leader is reaped and the output collected.
fn run_with_timeout(mut cmd: Command, timeout: Duration) -> (Option<i32>, String, String) {
    use std::os::unix::process::CommandExt;
    cmd.process_group(0);
    let mut child = cmd.spawn().expect("spawn test subprocess");
    let pid = child.id() as libc::pid_t;
    let mut so = child.stdout.take().unwrap();
    let mut se = child.stderr.take().unwrap();
    let to = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = so.read_to_string(&mut s);
        s
    });
    let te = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = se.read_to_string(&mut s);
        s
    });
    let deadline = Instant::now() + timeout;
    loop {
        // SAFETY: zeroed siginfo_t is a valid out-parameter.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: waits for our own child without reaping it.
        let r = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if r == 0 && info.si_signo == libc::SIGCHLD {
            break;
        }
        if r != 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            break;
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    // SAFETY: the group of our unreaped child; its id cannot be reused yet.
    unsafe { libc::kill(-pid, libc::SIGKILL) };
    let status = child.wait().expect("wait");
    let out = to.join().unwrap();
    let mut err = te.join().unwrap();
    let code = status.code();
    if code.is_none() {
        use std::os::unix::process::ExitStatusExt;
        err.push_str(&format!("\n[terminated by signal {:?}]", status.signal()));
    }
    (code, out, err)
}

// ---------------------------------------------------------------------------
// fork helpers

/// Wait for `pid` at most `limit`: the exit status, or minus the signal.
/// Past the deadline the child is killed and reaped. `EINTR` is retried.
pub fn wait_bounded(pid: libc::pid_t, limit: Duration) -> i32 {
    assert!(pid > 0, "no child to wait for");
    let deadline = Instant::now() + limit;
    let mut killed = false;
    loop {
        let mut status = 0;
        // SAFETY: waiting for our own child.
        let r = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if r == pid {
            if killed {
                return -libc::SIGKILL;
            }
            return if libc::WIFEXITED(status) {
                libc::WEXITSTATUS(status)
            } else {
                -libc::WTERMSIG(status)
            };
        }
        if r < 0 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            panic!("waitpid: {e}");
        }
        if !killed && Instant::now() >= deadline {
            // SAFETY: our own child, not reaped yet.
            unsafe { libc::kill(pid, libc::SIGKILL) };
            killed = true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Fork; run `child` in the child and `_exit` with its return value. Returns
/// the child's exit status in the parent (or -signal), waiting at most 30 s.
pub fn fork_and_wait(child: impl FnOnce() -> i32) -> i32 {
    // SAFETY: test helper; the child only runs `child` and then _exit.
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0, "fork failed");
    if pid == 0 {
        // A child that hangs (the failure the fork cases look for) would keep
        // the runner's output pipes open for ever; SIGALRM ends it instead.
        // SAFETY: alarm has no preconditions.
        unsafe { libc::alarm(20) };
        let code = std::panic::catch_unwind(std::panic::AssertUnwindSafe(child)).unwrap_or(99);
        // SAFETY: terminate the child without running atexit handlers or
        // destructors inherited from the parent.
        unsafe { libc::_exit(code) };
    }
    wait_bounded(pid, Duration::from_secs(30))
}

/// A child made with a raw `clone` system call (no atfork handlers run, and
/// on older C libraries `getpid()` would return the parent's id there). The
/// caller must have no other live thread: the child may allocate.
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
pub fn raw_clone_and_wait(child: impl FnOnce() -> i32) -> i32 {
    // SAFETY: a clone with only SIGCHLD: a copy of the address space, like
    // fork without the C library's handlers.
    let r = unsafe {
        libc::syscall(
            libc::SYS_clone,
            libc::SIGCHLD as libc::c_long,
            0 as libc::c_long,
            0 as libc::c_long,
            0 as libc::c_long,
            0 as libc::c_long,
        )
    };
    assert!(r >= 0, "clone: {}", std::io::Error::last_os_error());
    if r == 0 {
        // SAFETY: alarm has no preconditions.
        unsafe { libc::alarm(20) };
        let code = std::panic::catch_unwind(std::panic::AssertUnwindSafe(child)).unwrap_or(99);
        // SAFETY: as in fork_and_wait.
        unsafe { libc::_exit(code) };
    }
    wait_bounded(r as libc::pid_t, Duration::from_secs(30))
}

/// True when the kernel clears a page marked `MADV_WIPEONFORK` in a child.
pub fn kernel_wipes_on_fork() -> bool {
    #[cfg(target_os = "linux")]
    {
        const MADV_WIPEONFORK: libc::c_int = 18;
        // SAFETY: sysconf has no preconditions.
        let len = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
        // SAFETY: a new private anonymous mapping.
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if p == libc::MAP_FAILED {
            return false;
        }
        // SAFETY: `p` and `len` are the mapping just made.
        let ok = unsafe {
            libc::madvise(p, len, -1) != 0 && libc::madvise(p, len, MADV_WIPEONFORK) == 0
        };
        let wiped = ok && {
            let cell = p as *mut u64;
            // SAFETY: the page is mapped and writable.
            unsafe { cell.write_volatile(1) };
            // SAFETY: the child only reads the page.
            let read = move || i32::from(unsafe { cell.read_volatile() } != 0);
            #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
            let code = raw_clone_and_wait(read);
            #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
            let code = fork_and_wait(read);
            code == 0
        };
        // SAFETY: the mapping made above, no longer used.
        unsafe { libc::munmap(p, len) };
        wiped
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

/// `kernel_wipes_on_fork()`, failing the case when it is false and
/// `RUST_DAOS_REQUIRE_WIPE=1`.
pub fn require_wipe() -> bool {
    let wipes = kernel_wipes_on_fork();
    if !wipes && std::env::var("RUST_DAOS_REQUIRE_WIPE").as_deref() == Ok("1") {
        panic!("RUST_DAOS_REQUIRE_WIPE=1 but the kernel does not clear the page in children");
    }
    wipes
}

/// Exit code of a process whose `unshare` failed (not `SKIP`: a case
/// process that skips for another reason must not be read as this one).
pub const NS_UNSHARE_FAILED: i32 = 76;
/// Exit code of a process whose clone into a new PID namespace failed.
pub const NS_CLONE_FAILED: i32 = 78;

/// Report a namespace setup failure from a child: write the errno to `fd`
/// and exit with `code`.
pub fn ns_fail(fd: libc::c_int, code: i32) -> ! {
    let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
    let bytes = errno.to_ne_bytes();
    // SAFETY: writes four bytes from a local to our own pipe end.
    unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
    // SAFETY: leave without running anything inherited.
    unsafe { libc::_exit(code) }
}

/// A pipe for `ns_fail`: (read end, write end).
pub fn ns_pipe() -> (libc::c_int, libc::c_int) {
    let mut fds = [0; 2];
    // SAFETY: an array of two descriptors as pipe expects.
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    (fds[0], fds[1])
}

/// After the case's child exited with `code`: skip when it reported a
/// namespace setup failure (`NS_UNSHARE_FAILED` or `NS_CLONE_FAILED`) through the pipe.
pub fn ns_skip_on_failure(code: i32, read_fd: libc::c_int) {
    if code != NS_UNSHARE_FAILED && code != NS_CLONE_FAILED {
        return;
    }
    let mut bytes = [0u8; 4];
    // SAFETY: reads at most four bytes into a local.
    let n = unsafe { libc::read(read_fd, bytes.as_mut_ptr().cast(), bytes.len()) };
    let errno = if n == 4 {
        i32::from_ne_bytes(bytes)
    } else {
        -1
    };
    skip(&format!(
        "PID namespaces not available (exit {code}, errno {errno})"
    ));
}

// ---------------------------------------------------------------------------
// C mock library

/// Variants of the C mock pair.
#[derive(Clone, Copy, Debug)]
pub enum MockVariant {
    Full,
    NoReinit,
    NoDfsStat,
    /// Exports only the plain names (`daos_pool_connect`, `daos_cont_open`).
    PlainNames,
    /// Lacks the `daos_kv_*` group.
    NoKv,
}

impl MockVariant {
    fn dir_name(self) -> &'static str {
        match self {
            MockVariant::Full => "full",
            MockVariant::NoReinit => "noreinit",
            MockVariant::NoDfsStat => "nostat",
            MockVariant::PlainNames => "plain",
            MockVariant::NoKv => "nokv",
        }
    }
}

/// Build the C mock pair (libdaos_mock.so.2 and libdfs_mock.so linked to it)
/// for every variant under `root`. Returns false when no C compiler works.
pub fn build_mocks(root: &Path) -> bool {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/mock");
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".to_string());
    for v in [
        MockVariant::Full,
        MockVariant::NoReinit,
        MockVariant::NoDfsStat,
        MockVariant::PlainNames,
        MockVariant::NoKv,
    ] {
        let dir = root.join(v.dir_name());
        std::fs::create_dir_all(&dir).unwrap();
        let mut daos = Command::new(&cc);
        daos.args([
            "-shared",
            "-fPIC",
            "-O1",
            "-Wall",
            "-Werror",
            "-pthread",
            "-Wl,-soname,libdaos_mock.so.2",
        ]);
        if matches!(v, MockVariant::NoReinit) {
            daos.arg("-DNO_REINIT");
        }
        if matches!(v, MockVariant::PlainNames) {
            daos.arg("-DNO_V2_NAMES");
        }
        if matches!(v, MockVariant::NoKv) {
            daos.arg("-DNO_KV");
        }
        daos.arg("-o")
            .arg(dir.join("libdaos_mock.so.2"))
            .arg(src.join("libdaos_mock.c"));
        let mut dfs = Command::new(&cc);
        dfs.args(["-shared", "-fPIC", "-O1", "-Wall", "-Werror", "-pthread"]);
        if matches!(v, MockVariant::NoDfsStat) {
            dfs.arg("-DOMIT_DFS_STAT");
        }
        dfs.arg("-o")
            .arg(dir.join("libdfs_mock.so"))
            .arg(src.join("libdfs_mock.c"))
            .arg("-L")
            .arg(&dir)
            .arg("-l:libdaos_mock.so.2")
            .arg("-Wl,-rpath,$ORIGIN");
        for mut c in [daos, dfs] {
            match c.status() {
                Ok(s) if s.success() => {}
                Ok(s) => panic!("building the C mock failed ({s}): {c:?}"),
                Err(e) => {
                    eprintln!("no C compiler ({cc}): {e}");
                    return false;
                }
            }
        }
    }
    true
}

/// Prepare step for binaries that use the C mock: build it and pass its
/// directory to the children.
pub fn prepare_mocks() -> Vec<(String, String)> {
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("daos-c-mock");
    if build_mocks(&root) {
        vec![("RUST_DAOS_MOCK_DIR".to_string(), root.display().to_string())]
    } else if std::env::var("RUST_DAOS_REQUIRE_MOCK").as_deref() == Ok("1") {
        panic!("RUST_DAOS_REQUIRE_MOCK=1 but the C mock could not be built");
    } else {
        Vec::new()
    }
}

/// Path of `libdfs_mock.so` of a variant, or skip the case.
pub fn mock_lib(v: MockVariant) -> PathBuf {
    match std::env::var_os("RUST_DAOS_MOCK_DIR") {
        Some(d) => PathBuf::from(d).join(v.dir_name()).join("libdfs_mock.so"),
        None => skip("C mock not built (no C compiler)"),
    }
}

/// Handle on the mock's test controls (looked up through libdfs_mock.so,
/// which reaches libdaos_mock.so.2 as a dependency).
/// `daos_handle_t`.
#[repr(C)]
pub struct RawHandle {
    pub cookie: u64,
}

pub struct MockCtl {
    lib: libloading::Library,
}

impl MockCtl {
    pub fn open(v: MockVariant) -> MockCtl {
        // SAFETY: the mock's only constructor is the load pause, which does
        // nothing unless RUST_DAOS_MOCK_LOAD_PAUSE is set.
        let lib = unsafe { libloading::Library::new(mock_lib(v)) }.expect("open mock");
        MockCtl { lib }
    }

    fn sym<T: Copy>(&self, name: &str) -> T {
        // SAFETY: callers use the prototypes of libdaos_mock.c.
        unsafe { *self.lib.get::<T>(name.as_bytes()).expect("mock symbol") }
    }

    pub fn calls(&self, op: &str) -> u64 {
        let f: unsafe extern "C" fn(*const libc::c_char) -> u64 = self.sym("mock_calls");
        let c = std::ffi::CString::new(op).unwrap();
        // SAFETY: NUL-terminated name.
        unsafe { f(c.as_ptr()) }
    }

    pub fn set_fail(&self, op: &str, rc: i32, skip: u32, times: u32) {
        let f: unsafe extern "C" fn(*const libc::c_char, libc::c_int, libc::c_uint, libc::c_uint) =
            self.sym("mock_set_fail");
        let c = std::ffi::CString::new(op).unwrap();
        // SAFETY: NUL-terminated name.
        unsafe { f(c.as_ptr(), rc, skip, times) }
    }

    pub fn set_delay(&self, op: &str, ms: u32) {
        let f: unsafe extern "C" fn(*const libc::c_char, libc::c_uint) = self.sym("mock_set_delay");
        let c = std::ffi::CString::new(op).unwrap();
        // SAFETY: NUL-terminated name.
        unsafe { f(c.as_ptr(), ms) }
    }

    pub fn live_objects(&self) -> i64 {
        let f: unsafe extern "C" fn() -> i64 = self.sym("mock_live_objects");
        // SAFETY: no arguments.
        unsafe { f() }
    }

    /// Live pool handles in libdaos_mock.
    pub fn live_pools(&self) -> i64 {
        self.live_handles(1)
    }

    /// Live container handles in libdaos_mock.
    pub fn live_conts(&self) -> i64 {
        self.live_handles(2)
    }

    /// Property lists from daos_prop_alloc not yet freed.
    pub fn live_props(&self) -> i64 {
        let f: unsafe extern "C" fn() -> libc::c_long = self.sym("mock_live_props");
        // SAFETY: no arguments.
        unsafe { f() as i64 }
    }

    /// Open object, KV and array handles.
    pub fn live_objs(&self) -> i64 {
        let f: unsafe extern "C" fn() -> libc::c_long = self.sym("mock_live_objs");
        // SAFETY: no arguments.
        unsafe { f() as i64 }
    }

    /// Open transactions.
    pub fn live_txs(&self) -> i64 {
        let f: unsafe extern "C" fn() -> libc::c_long = self.sym("mock_live_txs");
        // SAFETY: no arguments.
        unsafe { f() as i64 }
    }

    /// Commits refused because they came from another thread.
    pub fn tx_cross_thread(&self) -> i64 {
        let f: unsafe extern "C" fn() -> libc::c_long = self.sym("mock_tx_cross_thread");
        // SAFETY: no arguments.
        unsafe { f() as i64 }
    }

    /// Existing containers.
    pub fn containers(&self) -> i64 {
        let f: unsafe extern "C" fn() -> libc::c_long = self.sym("mock_containers");
        // SAFETY: no arguments.
        unsafe { f() as i64 }
    }

    /// Force `times` KEY2BIG results reporting `report` (negative: exact fit).
    pub fn set_key2big(&self, times: i32, report: i64) {
        let f: unsafe extern "C" fn(libc::c_int, libc::c_long) = self.sym("mock_set_key2big");
        // SAFETY: plain integer arguments.
        unsafe { f(times, report as libc::c_long) }
    }

    fn live_handles(&self, kind: i32) -> i64 {
        let f: unsafe extern "C" fn(libc::c_int) -> libc::c_long = self.sym("mock_live_handles");
        // SAFETY: plain integer argument.
        unsafe { f(kind) as i64 }
    }

    pub fn trace(&self) -> String {
        let f: unsafe extern "C" fn(*mut libc::c_char, libc::size_t) -> libc::size_t =
            self.sym("mock_trace");
        let mut buf = vec![0u8; 1 << 20];
        // SAFETY: buffer of the given length.
        let n = unsafe { f(buf.as_mut_ptr().cast(), buf.len()) };
        buf.truncate(n);
        String::from_utf8(buf).unwrap()
    }

    /// Open a transaction on container handle `coh` directly in the mock
    /// (not through the crate); returns its cookie.
    pub fn raw_tx_open(&self, coh: u64) -> u64 {
        let f: unsafe extern "C" fn(
            RawHandle,
            *mut RawHandle,
            u64,
            *mut libc::c_void,
        ) -> libc::c_int = self.sym("daos_tx_open");
        let mut th = RawHandle { cookie: 0 };
        // SAFETY: the daos_tx_open prototype; out-pointer to a local; ev NULL.
        let rc = unsafe { f(RawHandle { cookie: coh }, &mut th, 0, std::ptr::null_mut()) };
        assert_eq!(rc, 0);
        th.cookie
    }

    /// Call `daos_tx_commit`, `_abort`, `_restart` or `_close` of the mock
    /// directly on transaction `th`.
    pub fn raw_tx(&self, op: &str, th: u64) -> i32 {
        let f: unsafe extern "C" fn(RawHandle, *mut libc::c_void) -> libc::c_int = self.sym(op);
        // SAFETY: the prototype of these four calls; ev NULL.
        unsafe { f(RawHandle { cookie: th }, std::ptr::null_mut()) }
    }

    /// Call the mock's daos_fini / daos_init directly (not through the crate).
    pub fn raw_daos(&self, name: &str) -> i32 {
        let f: unsafe extern "C" fn() -> libc::c_int = self.sym(name);
        // SAFETY: no arguments.
        unsafe { f() }
    }
}

/// Initialise the crate against a mock variant.
pub fn init_mock(v: MockVariant) -> &'static daos::InitReport {
    let opts = daos::InitOptions::new().library_path(mock_lib(v));
    // SAFETY: test subprocess, called before any thread is started.
    unsafe { daos::init(&opts) }.expect("init against the C mock")
}

/// `prefix` followed by components of at most 200 bytes, `n` bytes long
/// in total (`n` at least `prefix.len() + 2`).
pub fn path_of_len(prefix: &str, n: usize) -> String {
    let mut p = prefix.to_string();
    while p.len() < n {
        let room = n - p.len();
        let take = if room <= 201 {
            room - 1
        } else {
            200.min(room - 3)
        };
        p.push('/');
        p.push_str(&"x".repeat(take));
    }
    p
}
