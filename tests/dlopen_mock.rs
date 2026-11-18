// Copyright (c) 2026 Jihyeon Gim
// SPDX-License-Identifier: MIT OR Apache-2.0

//! The dlopen path against the C mock pair (libdfs_mock.so linked to
//! libdaos_mock.so.2): symbol lookup through a dependency, absent library and
//! missing symbol errors, init order (no fini ever), the connect and close
//! sequence on crate-owned pool and container handles, errno and DER
//! injection, the registry rules, fork refusal, and the thread sequence of the
//! "runtime outlives mounts" rule. Each case runs in its own subprocess.

mod common;

use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use common::*;
use daos::{ConnectOptions, Dfs, Error, InitOptions};

const T: Duration = Duration::from_secs(10);

fn absent_library() {
    let missing = "/nonexistent/dir/libdfs.so";
    std::env::set_var(daos::LIBDFS_ENV, missing);
    assert!(!daos::is_available());
    match daos::library_info() {
        Err(Error::LibraryNotFound { tried, reason, .. }) => {
            assert_eq!(tried, vec![missing.to_string()]);
            assert!(reason.contains("No such file"), "{reason}");
        }
        other => panic!("{other:?}"),
    }
    // SAFETY: single-threaded subprocess.
    let e = unsafe { daos::init(&InitOptions::new().library_path(missing)) }.unwrap_err();
    let text = e.to_string();
    assert!(
        text.starts_with("DAOS client library not found: tried \"/nonexistent/dir/libdfs.so\" (")
    );
    assert!(text
        .ends_with("Install the daos-client package or set RUST_DAOS_LIBDFS / LD_LIBRARY_PATH."));
    assert_eq!(e.kind(), std::io::ErrorKind::Unsupported);
    assert!(matches!(Dfs::connect("p", "c"), Err(Error::NotInitialized)));
}

fn missing_symbol() {
    let e =
        unsafe { daos::init(&InitOptions::new().library_path(mock_lib(MockVariant::NoDfsStat))) }
            .unwrap_err();
    match e {
        Error::MissingSymbol { name, library, .. } => {
            assert_eq!(name, "dfs_stat");
            assert!(library.ends_with("libdfs_mock.so"));
        }
        other => panic!("{other:?}"),
    }
}

fn library_info_through_dependency() {
    std::env::set_var(daos::LIBDFS_ENV, mock_lib(MockVariant::Full));
    let info = daos::library_info().unwrap();
    assert!(info.path.ends_with("full/libdfs_mock.so"));
    assert!(
        info.has_reinit,
        "daos_reinit found in libdaos_mock through the libdfs handle"
    );
    assert!(!info.has_connect_snap);
    assert!(daos::is_available());
    // Loading never initialises.
    let ctl = MockCtl::open(MockVariant::Full);
    assert_eq!(ctl.calls("daos_init"), 0);
    assert!(daos::init_report().is_none());
}

fn no_reinit_variant() {
    let r = init_mock(MockVariant::NoReinit);
    assert!(!r.library.has_reinit);
    let ctl = MockCtl::open(MockVariant::NoReinit);
    assert_eq!(ctl.calls("daos_reinit"), 0);
}

fn init_order_idempotence_and_report() {
    assert!(matches!(Dfs::connect("p", "c"), Err(Error::NotInitialized)));
    let r1 = init_mock(MockVariant::Full);
    let r2 = init_mock(MockVariant::Full);
    assert!(std::ptr::eq(r1, r2), "later calls return the first report");
    assert!(std::ptr::eq(daos::init_report().unwrap(), r1));
    let ctl = MockCtl::open(MockVariant::Full);
    assert_eq!(ctl.trace(), "daos_init\ndfs_init\n");
    // Different library path: AlreadyInitialized.
    let e =
        unsafe { daos::init(&InitOptions::new().library_path("/other/libdfs.so")) }.unwrap_err();
    assert!(matches!(e, Error::AlreadyInitialized));
    let e = unsafe { daos::init(&InitOptions::default()) }.unwrap_err();
    assert!(matches!(e, Error::AlreadyInitialized));
    // Launched without CRT_MRC_ENABLE (see the case table): one warning.
    assert_eq!(r1.crt_mrc_enable_before, None);
    assert_eq!(r1.warnings.len(), 1, "{:?}", r1.warnings);
    assert!(r1.warnings[0].contains("set CRT_MRC_ENABLE=0 in the launcher"));
    assert_eq!(daos::library_info().unwrap(), r1.library);
}

fn init_report_quiet_when_launcher_disables_mr_cache() {
    let r = init_mock(MockVariant::Full);
    assert_eq!(r.crt_mrc_enable_before.as_deref(), Some("0"));
    assert_eq!(r.crt_mrc_enable_after.as_deref(), Some("0"));
    assert_eq!(r.fi_mr_cache_max_count_after.as_deref(), Some("0"));
    assert!(r.warnings.is_empty(), "{:?}", r.warnings);
}

fn init_report_warns_for_nonzero_value() {
    let r = init_mock(MockVariant::Full);
    assert_eq!(r.crt_mrc_enable_before.as_deref(), Some("1"));
    assert_eq!(r.warnings.len(), 1);
    assert!(r.warnings[0].starts_with("CRT_MRC_ENABLE=\"1\" was set before daos::init"));
}

fn daos_init_failure_is_retryable() {
    std::env::set_var(daos::LIBDFS_ENV, mock_lib(MockVariant::Full));
    daos::library_info().unwrap(); // load only
    let ctl = MockCtl::open(MockVariant::Full);
    ctl.set_fail("daos_init", -2034, 0, 1);
    let e = unsafe { daos::init(&InitOptions::default()) }.unwrap_err();
    assert_eq!(e.der(), Some(daos::DerCode::AGENT_COMM));
    assert_eq!(
        e.to_string(),
        "daos_init failed: DER_AGENT_COMM(-2034): Agent communication error"
    );
    assert_eq!(ctl.calls("dfs_init"), 0);
    unsafe { daos::init(&InitOptions::default()) }.unwrap();
    assert_eq!(ctl.trace(), "daos_init\ndaos_init\ndfs_init\n");
}

fn dfs_init_failure_is_fatal() {
    std::env::set_var(daos::LIBDFS_ENV, mock_lib(MockVariant::Full));
    daos::library_info().unwrap();
    let ctl = MockCtl::open(MockVariant::Full);
    ctl.set_fail("dfs_init", libc::EIO, 0, 1);
    let e1 = unsafe { daos::init(&InitOptions::default()) }.unwrap_err();
    assert!(matches!(
        e1,
        Error::Dfs {
            op: "dfs_init",
            errno: libc::EIO,
            ..
        }
    ));
    let e2 = unsafe { daos::init(&InitOptions::default()) }.unwrap_err();
    assert_eq!(e2.to_string(), e1.to_string());
    assert_eq!(ctl.calls("daos_init"), 1);
    assert_eq!(ctl.calls("dfs_init"), 1);
    assert_eq!(ctl.calls("daos_fini"), 0, "never finalised");
    assert!(matches!(Dfs::connect("p", "c"), Err(Error::NotInitialized)));
}

fn io_close_reconnect_never_finalises() {
    init_mock(MockVariant::Full);
    let ctl = MockCtl::open(MockVariant::Full);
    let dfs = Dfs::connect("pool", "cont").unwrap();
    dfs.write("/a/b/file", b"payload").unwrap();
    assert_eq!(dfs.read("/a/b/file").unwrap(), b"payload");
    let mut buf = [0u8; 3];
    assert_eq!(dfs.read_range_into("/a/b/file", 4, &mut buf).unwrap(), 3);
    assert_eq!(&buf, b"oad");
    assert!(dfs.metadata("/a/b").unwrap().is_dir());
    assert_eq!(dfs.metadata("/a/b/file").unwrap().len(), 7);
    let names: Vec<_> = dfs
        .read_dir("/a/b")
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(names, vec!["/a/b/file"]);
    assert_eq!(
        dfs.remove_dir("/a").unwrap_err().raw_os_error(),
        Some(libc::ENOTEMPTY)
    );
    dfs.close(T).unwrap();
    assert!(matches!(dfs.read("/a/b/file"), Err(Error::Closed)));
    let again = Dfs::connect("pool", "cont").unwrap();
    assert_eq!(again.read("/a/b/file").unwrap(), b"payload");
    // O_TRUNC overwrite with shorter content.
    again.write("/a/b/file", b"pay").unwrap();
    assert_eq!(again.read("/a/b/file").unwrap(), b"pay");
    again.write("/a/b/file", b"payload").unwrap();
    again.remove_all("/a").unwrap();
    drop(again);
    drop(dfs);
    let trace = ctl.trace();
    assert!(!trace.contains("fini"), "{trace}");
    assert_eq!(ctl.calls("daos_init"), 1);
    for op in [
        "daos_pool_connect",
        "daos_cont_open",
        "dfs_mount",
        "dfs_umount",
        "daos_cont_close",
        "daos_pool_disconnect",
    ] {
        assert_eq!(ctl.calls(op), 2, "{op}");
    }
    assert!(
        trace.contains("daos_pool_connect\ndaos_cont_open\ndfs_mount\n"),
        "{trace}"
    );
    assert!(
        trace.contains("dfs_umount\ndaos_cont_close\ndaos_pool_disconnect\n"),
        "{trace}"
    );
    // Every object from dfs_lookup/dfs_open was released, every handle closed.
    assert_eq!(ctl.live_objects(), 0);
    assert_eq!(ctl.live_conts(), 0);
    assert_eq!(ctl.live_pools(), 0);
}

fn errno_injection() {
    init_mock(MockVariant::Full);
    let ctl = MockCtl::open(MockVariant::Full);
    let dfs = Dfs::connect("pool", "cont").unwrap();
    dfs.write("/f", b"x").unwrap();
    ctl.set_fail("dfs_read", libc::EIO, 0, 1);
    let e = dfs.read("/f").unwrap_err();
    assert_eq!(
        e.to_string(),
        "dfs_read(\"/f\") failed: EIO (5): Input/output error"
    );
    ctl.set_fail("dfs_write", libc::ENOSPC, 0, 1);
    assert_eq!(
        dfs.write("/g", b"x").unwrap_err().raw_os_error(),
        Some(libc::ENOSPC)
    );
    ctl.set_fail("dfs_open", libc::EPERM, 0, 1);
    assert!(dfs.open("/f").unwrap_err().is_permission_denied());
    ctl.set_fail("dfs_get_size", libc::ETIMEDOUT, 0, 1);
    assert_eq!(
        dfs.read("/f").unwrap_err().kind(),
        std::io::ErrorKind::TimedOut
    );
    // dfs_release failure is reported once by File::close.
    let f = dfs.open("/f").unwrap();
    ctl.set_fail("dfs_release", libc::EIO, 0, 1);
    assert_eq!(f.close().unwrap_err().raw_os_error(), Some(libc::EIO));
    // Connect errors: pool, container, mount. Whatever was opened for the
    // failed connect is closed again.
    let e = Dfs::connect("missing", "c").unwrap_err();
    assert!(
        matches!(
            e,
            Error::Daos {
                op: "daos_pool_connect",
                ..
            }
        ),
        "{e}"
    );
    assert!(e.is_not_found());
    assert_eq!(e.raw_os_error(), Some(libc::ENOENT));
    assert_eq!(
        e.to_string(),
        "daos_pool_connect(\"missing\") failed: DER_NONEXIST(-1005): The specified entity does not exist"
    );
    let e = Dfs::connect("pool2", "missing-cont").unwrap_err();
    assert!(
        matches!(
            e,
            Error::Daos {
                op: "daos_cont_open",
                ..
            }
        ),
        "{e}"
    );
    assert!(e.is_not_found());
    let e = Dfs::connect("pool2", "plain-cont").unwrap_err();
    assert!(
        matches!(
            e,
            Error::Dfs {
                op: "dfs_mount",
                errno: libc::EINVAL,
                ..
            }
        ),
        "{e}"
    );
    ctl.set_fail("daos_cont_open", -1012, 0, 1);
    let e = Dfs::connect("pool2", "c").unwrap_err();
    assert_eq!(e.der(), Some(daos::DerCode::BUSY));
    // Only the mount of "pool"/"cont" holds handles.
    assert_eq!(ctl.live_conts(), 1);
    assert_eq!(ctl.live_pools(), 1);
    assert_eq!(ctl.calls("dfs_cont_create"), 0, "open existing only");
    dfs.close(T).unwrap();
    assert_eq!(ctl.live_conts(), 0);
    assert_eq!(ctl.live_pools(), 0);
}

/// dfs_mount reports ENOENT while a new container has no superblock yet; the
/// crate retries like dfs_connect (7 attempts).
fn mount_enoent_retry() {
    init_mock(MockVariant::Full);
    let ctl = MockCtl::open(MockVariant::Full);
    ctl.set_fail("dfs_mount", libc::ENOENT, 0, 3);
    let dfs = Dfs::connect("pool", "cont").unwrap();
    assert_eq!(ctl.calls("dfs_mount"), 4);
    dfs.close(T).unwrap();
    ctl.set_fail("dfs_mount", libc::ENOENT, 0, 100);
    let e = Dfs::connect("pool", "cont").unwrap_err();
    assert!(
        matches!(
            e,
            Error::Dfs {
                op: "dfs_mount",
                errno: libc::ENOENT,
                ..
            }
        ),
        "{e}"
    );
    assert_eq!(ctl.calls("dfs_mount"), 4 + 7);
    assert_eq!(ctl.live_conts(), 0);
    assert_eq!(ctl.live_pools(), 0);
    ctl.set_fail("dfs_mount", 0, 0, 0);
}

/// A library that exports only the plain names (`daos_pool_connect`,
/// `daos_cont_open`, no `*2`) works the same.
fn plain_names_variant() {
    init_mock(MockVariant::PlainNames);
    let ctl = MockCtl::open(MockVariant::PlainNames);
    let dfs = Dfs::connect("pool", "cont").unwrap();
    dfs.write("/p", b"plain").unwrap();
    assert_eq!(dfs.read("/p").unwrap(), b"plain");
    let c = dfs.container_handle().unwrap();
    let o = c
        .object(new_oid(&c, ObjectType::MultiHashed), OpenMode::ReadWrite)
        .unwrap();
    o.put_single(None, b"d", b"a", b"v").unwrap();
    drop((o, c));
    dfs.close(T).unwrap();
    assert_eq!(ctl.calls("daos_pool_connect"), 1);
    assert_eq!(ctl.calls("daos_obj_generate_oid"), 1);
    assert_eq!(ctl.live_pools(), 0);
    let info = daos::library_info().unwrap();
    assert!(info.plain_names.contains(&"daos_pool_connect"), "{info:?}");
}

fn registry_rules() {
    init_mock(MockVariant::Full);
    let ctl = MockCtl::open(MockVariant::Full);
    let a = Dfs::connect("pool", "c1").unwrap();
    let b = Dfs::connect_with(&ConnectOptions::new("pool", "c1")).unwrap();
    assert_eq!(ctl.calls("dfs_mount"), 1, "same key shares the mount");
    assert_eq!(ctl.calls("daos_pool_connect"), 1);
    assert_eq!(ctl.live_objects(), 0);
    a.write("/x", b"1").unwrap();
    assert_eq!(b.read("/x").unwrap(), b"1");
    // Another container of the same pool shares the pool handle.
    let c2 = Dfs::connect("pool", "c2").unwrap();
    assert_eq!(ctl.calls("daos_pool_connect"), 1);
    assert_eq!(ctl.live_conts(), 2);
    let e = Dfs::connect_with(&ConnectOptions::new("pool", "c3").sys_name("other")).unwrap_err();
    assert!(matches!(e, Error::Conflict { what: "system", .. }), "{e}");
    // The other access mode is a separate pool and container handle.
    let ro = Dfs::connect_with(&ConnectOptions::new("pool", "c1").read_only(true)).unwrap();
    assert!(ro.is_read_only());
    assert_eq!(ctl.calls("daos_pool_connect"), 2);
    assert_eq!(ctl.live_pools(), 2);
    assert_eq!(ctl.live_conts(), 3);
    assert_eq!(ro.read("/x").unwrap(), b"1");
    assert!(ro.write("/y", b"1").unwrap_err().is_permission_denied());
    a.close(T).unwrap();
    drop((a, b, c2, ro));
    assert_eq!(ctl.live_conts(), 0);
    assert_eq!(ctl.live_pools(), 0);
    // The system rule persists after close.
    let e = Dfs::connect_with(&ConnectOptions::new("pool", "c1").sys_name("x")).unwrap_err();
    assert!(matches!(e, Error::Conflict { what: "system", .. }));
}

/// A last close must not re-initialise the library: thread A does I/O and stays alive; thread B closes the
/// last mount and connects again; A then does I/O on a file of the new mount.
/// The mock fails A with EIO if libdaos was finalised and re-initialised.
fn last_close_no_reinit() {
    init_mock(MockVariant::Full);
    let ctl = MockCtl::open(MockVariant::Full);
    last_close_sequence(|| {});
    assert!(!ctl.trace().contains("fini"));
    assert_eq!(ctl.calls("daos_init"), 1);
}

/// Control for the case above: the same sequence with a daos_fini/daos_init
/// pair (re-initialising the library on last close) makes A fail.
fn mock_detects_fini_then_reinit() {
    init_mock(MockVariant::Full);
    let ctl = MockCtl::open(MockVariant::Full);
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        last_close_sequence(|| {
            assert_eq!(ctl.raw_daos("daos_fini"), 0);
            assert_eq!(ctl.raw_daos("daos_init"), 0);
        })
    }));
    assert!(r.is_err(), "thread A must fail with a stale event");
}

fn last_close_sequence(between: impl FnOnce()) {
    let dfs = Dfs::connect("pool", "cont").unwrap();
    dfs.write("/lastclose/a", b"first").unwrap();
    let (to_a, a_rx) = mpsc::channel::<daos::File>();
    let (to_b, b_rx) = mpsc::channel::<Result<Vec<u8>, String>>();
    let a_dfs = dfs.clone();
    let a = thread::spawn(move || {
        // A: I/O on the first mount, then park.
        let first = a_dfs.read("/lastclose/a").map_err(|e| e.to_string());
        drop(a_dfs);
        to_b.send(first).unwrap();
        let file = a_rx.recv().unwrap();
        let mut buf = vec![0u8; 6];
        let r = file
            .read_exact_at(&mut buf, 0)
            .map(|_| buf)
            .map_err(|e| e.to_string());
        to_b.send(r).unwrap();
    });
    assert_eq!(b_rx.recv().unwrap().unwrap(), b"first");
    // B: close the last mount, then connect again.
    dfs.close(T).unwrap();
    drop(dfs);
    between();
    let dfs2 = Dfs::connect("pool", "cont").unwrap();
    dfs2.write("/lastclose/b", b"second").unwrap();
    to_a.send(dfs2.open("/lastclose/b").unwrap()).unwrap();
    let r = b_rx.recv().unwrap();
    a.join().unwrap();
    assert_eq!(r.expect("thread A I/O after reconnect"), b"second");
    dfs2.remove_all("/lastclose").unwrap();
}

fn fork_child_is_refused_and_drop_is_silent() {
    init_mock(MockVariant::Full);
    let ctl = MockCtl::open(MockVariant::Full);
    let dfs = Dfs::connect("pool", "cont").unwrap();
    dfs.write("/fk/f", b"data").unwrap();
    let file = dfs.open("/fk/f").unwrap();
    let mut rd = dfs.read_dir("/fk").unwrap();
    let mut w = dfs.writer("/fk/w").unwrap();
    let parent_pid = std::process::id();
    let code = fork_and_wait(move || {
        let forked = |r: Result<(), Error>| match r {
            Err(Error::Forked {
                created_pid,
                current_pid,
                ..
            }) => created_pid == parent_pid && current_pid != parent_pid,
            _ => false,
        };
        let before: Vec<u64> = [
            "dfs_release",
            "dfs_umount",
            "dfs_lookup",
            "dfs_read",
            "daos_cont_close",
        ]
        .iter()
        .map(|o| ctl.calls(o))
        .collect();
        let mut ok = true;
        ok &= forked(Dfs::connect("pool", "cont").map(|_| ()));
        ok &= forked(Dfs::connect("pool", "other").map(|_| ()));
        ok &= forked(dfs.metadata("/fk/f").map(|_| ()));
        ok &= forked(dfs.read("/fk/f").map(|_| ()));
        ok &= forked(file.read_at(&mut [0u8; 4], 0).map(|_| ()));
        ok &= forked(w.write_all(b"x"));
        ok &= forked(rd.next().unwrap().map(|_| ()));
        ok &= forked(dfs.close(T));
        ok &= forked(
            unsafe { daos::init(&InitOptions::new().library_path(mock_lib(MockVariant::Full))) }
                .map(|_| ()),
        );
        drop(rd);
        drop(w);
        drop(file);
        drop(dfs);
        let after: Vec<u64> = [
            "dfs_release",
            "dfs_umount",
            "dfs_lookup",
            "dfs_read",
            "daos_cont_close",
        ]
        .iter()
        .map(|o| ctl.calls(o))
        .collect();
        ok &= before == after;
        if ok {
            0
        } else {
            1
        }
    });
    assert_eq!(code, 0, "child checks failed");
}

fn fork_child_does_not_hang_on_parent_locks() {
    init_mock(MockVariant::Full);
    let ctl = MockCtl::open(MockVariant::Full);
    let dfs = Dfs::connect("pool", "cont").unwrap();
    dfs.write("/h/f", b"data").unwrap();
    // A parent thread sits inside daos_cont_open (its key reserved in the
    // registry), and another inside dfs_read, holding the mount gate.
    ctl.set_delay("daos_cont_open", 1500);
    ctl.set_delay("dfs_read", 1500);
    let t1 = thread::spawn(|| Dfs::connect("pool", "other").map(|_| ()));
    let d2 = dfs.clone();
    let t2 = thread::spawn(move || d2.read("/h/f").map(|_| ()));
    thread::sleep(Duration::from_millis(300));
    let code = fork_and_wait(move || {
        let start = Instant::now();
        let a = matches!(Dfs::connect("pool", "cont"), Err(Error::Forked { .. }));
        let b = matches!(dfs.metadata("/h/f"), Err(Error::Forked { .. }));
        let c = matches!(dfs.close(T), Err(Error::Forked { .. }));
        drop(dfs);
        let quick = start.elapsed() < Duration::from_millis(500);
        if a && b && c && quick {
            0
        } else {
            1
        }
    });
    assert_eq!(code, 0);
    t1.join().unwrap().unwrap();
    t2.join().unwrap().unwrap();
}

fn fork_before_init_is_fine() {
    let path = mock_lib(MockVariant::Full);
    let code = fork_and_wait(move || {
        let r = unsafe { daos::init(&InitOptions::new().library_path(&path)) };
        if r.is_err() {
            return 2;
        }
        let dfs = Dfs::connect("pool", "cont").unwrap();
        dfs.write("/child", b"1").unwrap();
        if dfs.read("/child").unwrap() == b"1" {
            0
        } else {
            3
        }
    });
    assert_eq!(code, 0);
    init_mock(MockVariant::Full);
    Dfs::connect("pool", "cont")
        .unwrap()
        .write("/parent", b"1")
        .unwrap();
}

/// Wait until `cond` holds; false after 10 s.
fn wait_for(cond: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + T;
    while !cond() {
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(2));
    }
    true
}

/// In a fork child: `init`, `library_info`, `is_available` and a connect all
/// refuse with `Forked` naming `owner`, without waiting.
fn child_refuses_process_state(owner: u32) -> i32 {
    let start = Instant::now();
    let forked = |r: Result<(), Error>| {
        matches!(r, Err(Error::Forked { created_pid, current_pid, .. })
            if created_pid == owner && current_pid != owner)
    };
    let mut ok = true;
    ok &= forked(unsafe { daos::init(&InitOptions::default()) }.map(|_| ()));
    ok &= forked(daos::library_info().map(|_| ()));
    ok &= !daos::is_available();
    ok &= forked(Dfs::connect("pool", "cont").map(|_| ()));
    ok &= daos::init_report().is_none();
    let quick = start.elapsed() < Duration::from_secs(2);
    if !ok {
        1
    } else if !quick {
        2
    } else {
        0
    }
}

/// A child forked while a parent thread is inside `daos_init` (holding the
/// init lock) gets `Forked` at once instead of waiting for that lock.
fn fork_during_daos_init() {
    std::env::set_var(daos::LIBDFS_ENV, mock_lib(MockVariant::Full));
    daos::library_info().unwrap();
    let ctl = MockCtl::open(MockVariant::Full);
    ctl.set_delay("daos_init", 2000);
    let t = thread::spawn(|| unsafe { daos::init(&InitOptions::default()) }.map(|_| ()));
    assert!(wait_for(|| ctl.calls("daos_init") == 1));
    let parent = std::process::id();
    let code = fork_and_wait(move || child_refuses_process_state(parent));
    t.join().unwrap().unwrap();
    assert_eq!(code, 0, "child checks failed");
    ctl.set_delay("daos_init", 0);
    assert_eq!(ctl.trace(), "daos_init\ndfs_init\n");
    Dfs::connect("pool", "cont")
        .unwrap()
        .write("/after", b"1")
        .unwrap();
}

/// A child forked while a parent thread is inside the library load of a
/// pre-init `library_info` (holding the loader lock, inside dlopen) gets
/// `Forked` at once and never reaches the loader.
fn fork_during_library_load() {
    let path = mock_lib(MockVariant::Full);
    let dir = std::env::temp_dir().join(format!("rust-daos-load-pause-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::env::set_var("RUST_DAOS_MOCK_LOAD_PAUSE", &dir);
    std::env::set_var(daos::LIBDFS_ENV, &path);
    let t = thread::spawn(daos::library_info);
    let entered = dir.join("entered");
    assert!(
        wait_for(|| entered.exists()),
        "dlopen never reached the pause"
    );
    let parent = std::process::id();
    let code = fork_and_wait(move || child_refuses_process_state(parent));
    std::fs::write(dir.join("release"), b"").unwrap();
    let info = t.join().unwrap().unwrap();
    std::env::remove_var("RUST_DAOS_MOCK_LOAD_PAUSE");
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(code, 0, "child checks failed");
    assert!(info.path.ends_with("full/libdfs_mock.so"));
    init_mock(MockVariant::Full);
}

/// A child of a parent that only probed the library (no init, no thread
/// inside the loader) takes the process state over and initialises.
fn fork_after_probe_child_initialises() {
    std::env::set_var(daos::LIBDFS_ENV, mock_lib(MockVariant::Full));
    assert!(daos::is_available());
    let code = fork_and_wait(|| {
        if daos::library_info().is_err() {
            return 2;
        }
        if unsafe { daos::init(&InitOptions::default()) }.is_err() {
            return 3;
        }
        let dfs = Dfs::connect("pool", "cont").unwrap();
        dfs.write("/probe-child", b"1").unwrap();
        i32::from(dfs.read("/probe-child").unwrap() != b"1")
    });
    assert_eq!(code, 0);
    unsafe { daos::init(&InitOptions::default()) }.unwrap();
}

/// A retryable `daos_init` failure leaves nothing behind: a child forked
/// afterwards may initialise.
fn fork_after_retryable_init_failure_child_initialises() {
    std::env::set_var(daos::LIBDFS_ENV, mock_lib(MockVariant::Full));
    daos::library_info().unwrap();
    let ctl = MockCtl::open(MockVariant::Full);
    ctl.set_fail("daos_init", -2034, 0, 1);
    unsafe { daos::init(&InitOptions::default()) }.unwrap_err();
    let code = fork_and_wait(|| match unsafe { daos::init(&InitOptions::default()) } {
        Ok(_) => i32::from(Dfs::connect("pool", "cont").is_err()),
        Err(_) => 2,
    });
    assert_eq!(code, 0);
}

fn threads_on_one_file() {
    init_mock(MockVariant::Full);
    let dfs = Dfs::connect("pool", "cont").unwrap();
    let data: Vec<u8> = (0..1u32 << 20).map(|i| (i % 253) as u8).collect();
    dfs.write("/mt/f", &data).unwrap();
    let file = std::sync::Arc::new(dfs.open("/mt/f").unwrap());
    let hs: Vec<_> = (0..16usize)
        .map(|t| {
            let file = file.clone();
            let data = data.clone();
            let dfs = dfs.clone();
            thread::spawn(move || {
                let mut buf = vec![0u8; 65536];
                for k in 0..16usize {
                    let off = ((t + k) % 16) * 65536;
                    file.read_exact_at(&mut buf, off as u64).unwrap();
                    assert_eq!(&buf[..], &data[off..off + 65536]);
                }
                dfs.write(&format!("/mt/own{t}"), &buf).unwrap();
            })
        })
        .collect();
    for h in hs {
        h.join().unwrap();
    }
    assert_eq!(dfs.read_dir("/mt").unwrap().count(), 17);
}

// ---------------------------------------------------------------------------
// libdaos layer

use daos::cont::{ContainerCreate, Layout};
use daos::obj::{u64_key, ClassHints, ObjectClass, ObjectType, OpenMode};
use daos::tx::TxMode;
use daos::{Container, Pool};

fn new_oid(c: &Container, t: ObjectType) -> daos::ObjectId {
    c.oid_allocator()
        .next(t, ObjectClass::UNKNOWN, ClassHints::default())
        .unwrap()
}

/// A full walk: pool, the four creation routes, object, KV, array and a
/// transaction; then close. Checks the order of the close calls, that every
/// handle and property list was released, and that no commit came from a
/// foreign thread.
fn libdaos_walk_and_close_order() {
    init_mock(MockVariant::Full);
    let ctl = MockCtl::open(MockVariant::Full);
    let pool = Pool::connect("pool").unwrap();
    pool.create_container(&ContainerCreate::new().label("px").layout(Layout::Posix))
        .unwrap();
    pool.create_container(&ContainerCreate::new().layout(Layout::Posix))
        .unwrap();
    pool.create_container(&ContainerCreate::new().label("ob"))
        .unwrap();
    let anon = pool.create_container(&ContainerCreate::new()).unwrap();
    for op in [
        "dfs_cont_create_with_label",
        "dfs_cont_create",
        "daos_cont_create_with_label",
        "daos_cont_create",
    ] {
        assert_eq!(ctl.calls(op), 1, "{op}");
    }
    assert_eq!(ctl.containers(), 4);
    let px = pool.open_container("px").unwrap();
    let dfs = px.mount_dfs().unwrap();
    dfs.write("/f", b"posix").unwrap();
    assert_eq!(px.properties().unwrap().layout, Some(Layout::Posix));
    assert_eq!(ctl.live_props(), 0);
    let c = pool.open_container("ob").unwrap();
    assert!(pool.open_container(&anon.to_string()).is_ok());
    let o = c
        .object(new_oid(&c, ObjectType::DkeyUint64), OpenMode::ReadWrite)
        .unwrap();
    for k in 0..20u64 {
        o.put_single(None, &u64_key(k), b"a", &k.to_le_bytes())
            .unwrap();
    }
    assert_eq!(o.max_dkey_u64(None).unwrap(), Some(19));
    assert_eq!(o.dkeys().unwrap().batch_size(3, 64).count(), 20);
    let kv = c
        .kv(new_oid(&c, ObjectType::KvHashed), OpenMode::ReadWrite)
        .unwrap();
    kv.put(None, "k", b"v").unwrap();
    let a = c
        .array_create(new_oid(&c, ObjectType::Array), 1, 128)
        .unwrap();
    a.write_at(None, 0, b"array").unwrap();
    let mut buf = [0u8; 5];
    assert_eq!(a.read_at(None, 0, &mut buf).unwrap().counts(), None);
    assert_eq!(&buf, b"array");
    c.transaction(TxMode::ReadWrite, |tx| kv.put(Some(tx), "t", b"1"))
        .unwrap();
    assert_eq!(kv.get(None, "t").unwrap().as_deref(), Some(&b"1"[..]));
    assert_eq!(ctl.live_txs(), 0);
    assert_eq!(ctl.tx_cross_thread(), 0);
    assert_eq!(ctl.live_objs(), 3);
    c.close(T).unwrap();
    assert_eq!(ctl.live_objs(), 0);
    let trace = ctl.trace();
    let close = trace.rfind("daos_cont_close").unwrap();
    for op in ["daos_array_close", "daos_kv_close", "daos_obj_close"] {
        assert!(
            trace.rfind(op).unwrap() < close,
            "{op} before daos_cont_close"
        );
    }
    px.close(T).unwrap();
    assert!(dfs.is_closed());
    drop((o, kv, a, dfs, px, c));
    pool.destroy_container("ob", false).unwrap();
    pool.close(T).unwrap();
    assert_eq!(ctl.live_conts(), 0);
    assert_eq!(ctl.live_pools(), 0);
    assert_eq!(ctl.live_props(), 0);
    assert!(!trace.contains("fini"));
}

/// A library without the KV group: DFS and objects work, KV reports the
/// missing symbol at first use.
fn libdaos_lazy_missing_group() {
    init_mock(MockVariant::NoKv);
    let c = Container::open("pool", "c").unwrap();
    let o = c
        .object(new_oid(&c, ObjectType::MultiHashed), OpenMode::ReadWrite)
        .unwrap();
    o.put_single(None, b"d", b"a", b"v").unwrap();
    match c.kv(new_oid(&c, ObjectType::KvHashed), OpenMode::ReadWrite) {
        Err(Error::MissingSymbol { name, .. }) => assert_eq!(name, "daos_kv_open"),
        other => panic!("{other:?}"),
    }
    let dfs = Dfs::connect("pool", "c").unwrap();
    dfs.write("/x", b"1").unwrap();
}

/// The KEY2BIG rules against the C mock: an exact fit grows the buffer and
/// finishes; a library that never makes progress stops after 9 calls.
fn libdaos_key2big_rules() {
    init_mock(MockVariant::Full);
    let ctl = MockCtl::open(MockVariant::Full);
    let c = Container::open("pool", "c").unwrap();
    let o = c
        .object(new_oid(&c, ObjectType::MultiHashed), OpenMode::ReadWrite)
        .unwrap();
    o.put_single(None, b"one", b"a", b"v").unwrap();
    ctl.set_key2big(1, -1);
    assert_eq!(o.dkeys().unwrap().map(Result::unwrap).count(), 1);
    ctl.set_key2big(100, 1);
    let before = ctl.calls("daos_obj_list_dkey");
    let r: Vec<_> = o.dkeys().unwrap().collect();
    assert!(r.len() == 1 && r[0].is_err());
    assert_eq!(ctl.calls("daos_obj_list_dkey") - before, 9);
    ctl.set_key2big(0, 0);
}

/// Restart loop driven by DER_TX_RESTART from the mock's commit.
fn libdaos_tx_restart_loop() {
    init_mock(MockVariant::Full);
    let ctl = MockCtl::open(MockVariant::Full);
    let c = Container::open("pool", "c").unwrap();
    let kv = c
        .kv(new_oid(&c, ObjectType::KvHashed), OpenMode::ReadWrite)
        .unwrap();
    ctl.set_fail("daos_tx_commit", -2025, 0, 2);
    let mut runs = 0;
    c.transaction(TxMode::ReadWrite, |tx| {
        runs += 1;
        kv.put(Some(tx), "k", b"v")
    })
    .unwrap();
    assert_eq!(runs, 3);
    assert_eq!(ctl.calls("daos_tx_restart"), 2);
    assert_eq!(ctl.live_txs(), 0);
}

/// The C mock enforces the libdaos state rules (abort only while open,
/// restart only after a restart error): close and abort after a restart
/// error or a failed commit succeed without a daos_tx_abort call.
fn libdaos_tx_state_rules() {
    init_mock(MockVariant::Full);
    let ctl = MockCtl::open(MockVariant::Full);
    let c = Container::open("pool", "c").unwrap();
    let kv = c
        .kv(new_oid(&c, ObjectType::KvHashed), OpenMode::ReadWrite)
        .unwrap();
    ctl.set_fail("daos_tx_commit", -2025, 0, 1);
    let mut tx = c.tx(TxMode::ReadWrite).unwrap();
    kv.put(Some(&tx), "a", b"1").unwrap();
    assert!(tx.commit().unwrap_err().is_tx_restart());
    tx.close().unwrap();
    ctl.set_fail("daos_tx_commit", -2025, 0, 1);
    let mut tx = c.tx(TxMode::ReadWrite).unwrap();
    kv.put(Some(&tx), "a", b"1").unwrap();
    assert!(tx.commit().unwrap_err().is_tx_restart());
    tx.abort().unwrap();
    tx.close().unwrap();
    ctl.set_fail("daos_tx_commit", -2001, 0, 1);
    let mut tx = c.tx(TxMode::ReadWrite).unwrap();
    kv.put(Some(&tx), "b", b"2").unwrap();
    assert!(tx.commit().is_err());
    tx.close().unwrap();
    assert_eq!(ctl.calls("daos_tx_abort"), 0);
    assert_eq!(kv.get(None, "b").unwrap(), None);
    // An open transaction: one abort, then close.
    let mut tx = c.tx(TxMode::ReadWrite).unwrap();
    tx.abort().unwrap();
    tx.close().unwrap();
    assert_eq!(ctl.calls("daos_tx_abort"), 1);
    // The mock itself refuses what libdaos refuses.
    let th = ctl.raw_tx_open(c.as_raw());
    assert_eq!(ctl.raw_tx("daos_tx_restart", th), -1001);
    assert_eq!(ctl.raw_tx("daos_tx_commit", th), 0);
    assert_eq!(ctl.raw_tx("daos_tx_commit", th), -1008);
    assert_eq!(ctl.raw_tx("daos_tx_abort", th), -1001);
    assert_eq!(ctl.raw_tx("daos_tx_close", th), 0);
    let th = ctl.raw_tx_open(c.as_raw());
    assert_eq!(ctl.raw_tx("daos_tx_abort", th), 0);
    assert_eq!(ctl.raw_tx("daos_tx_abort", th), -1008);
    assert_eq!(ctl.raw_tx("daos_tx_commit", th), -1001);
    assert_eq!(ctl.raw_tx("daos_tx_close", th), 0);
    assert_eq!(ctl.live_txs(), 0);
}

/// The same with object I/O: thread A reads an object and stays alive; B closes
/// the last connection and opens again; A reads through the new handle.
fn libdaos_last_close_no_reinit() {
    init_mock(MockVariant::Full);
    let ctl = MockCtl::open(MockVariant::Full);
    let c = Container::open("pool", "c2").unwrap();
    let id = new_oid(&c, ObjectType::MultiHashed);
    let o = c.object(id, OpenMode::ReadWrite).unwrap();
    o.put_single(None, b"d", b"a", b"first").unwrap();
    let (to_a, a_rx) = mpsc::channel::<daos::obj::Object>();
    let (to_b, b_rx) = mpsc::channel::<Result<Option<Vec<u8>>, String>>();
    let a_obj = o.clone();
    let a = thread::spawn(move || {
        to_b.send(
            a_obj
                .get_single_vec(None, b"d", b"a")
                .map_err(|e| e.to_string()),
        )
        .unwrap();
        drop(a_obj);
        let o2 = a_rx.recv().unwrap();
        to_b.send(
            o2.get_single_vec(None, b"d", b"a")
                .map_err(|e| e.to_string()),
        )
        .unwrap();
    });
    assert_eq!(
        b_rx.recv().unwrap().unwrap().as_deref(),
        Some(&b"first"[..])
    );
    drop(o);
    c.close(T).unwrap();
    drop(c);
    assert_eq!(ctl.live_pools(), 0);
    let c = Container::open("pool", "c2").unwrap();
    to_a.send(c.object(id, OpenMode::ReadOnly).unwrap())
        .unwrap();
    let r = b_rx.recv().unwrap();
    a.join().unwrap();
    assert_eq!(r.unwrap().as_deref(), Some(&b"first"[..]));
    assert!(!ctl.trace().contains("fini"));
}

fn main() {
    let cases = [
        case("libdaos_walk_and_close_order", libdaos_walk_and_close_order),
        case("libdaos_lazy_missing_group", libdaos_lazy_missing_group),
        case("libdaos_key2big_rules", libdaos_key2big_rules),
        case("libdaos_tx_restart_loop", libdaos_tx_restart_loop),
        case("libdaos_tx_state_rules", libdaos_tx_state_rules),
        case("libdaos_last_close_no_reinit", libdaos_last_close_no_reinit),
        case("absent_library", absent_library),
        case("missing_symbol", missing_symbol),
        case(
            "library_info_through_dependency",
            library_info_through_dependency,
        ),
        case("no_reinit_variant", no_reinit_variant),
        Case {
            name: "init_order_idempotence_and_report",
            run: init_order_idempotence_and_report,
            env: &[("CRT_MRC_ENABLE", None), ("FI_MR_CACHE_MAX_COUNT", None)],
        },
        Case {
            name: "init_report_quiet_when_launcher_disables_mr_cache",
            run: init_report_quiet_when_launcher_disables_mr_cache,
            env: &[
                ("CRT_MRC_ENABLE", Some("0")),
                ("FI_MR_CACHE_MAX_COUNT", Some("0")),
            ],
        },
        Case {
            name: "init_report_warns_for_nonzero_value",
            run: init_report_warns_for_nonzero_value,
            env: &[("CRT_MRC_ENABLE", Some("1"))],
        },
        case(
            "daos_init_failure_is_retryable",
            daos_init_failure_is_retryable,
        ),
        case("dfs_init_failure_is_fatal", dfs_init_failure_is_fatal),
        case(
            "io_close_reconnect_never_finalises",
            io_close_reconnect_never_finalises,
        ),
        case("errno_injection", errno_injection),
        case("mount_enoent_retry", mount_enoent_retry),
        case("plain_names_variant", plain_names_variant),
        case("registry_rules", registry_rules),
        case("last_close_no_reinit", last_close_no_reinit),
        case(
            "mock_detects_fini_then_reinit",
            mock_detects_fini_then_reinit,
        ),
        case(
            "fork_child_is_refused_and_drop_is_silent",
            fork_child_is_refused_and_drop_is_silent,
        ),
        case(
            "fork_child_does_not_hang_on_parent_locks",
            fork_child_does_not_hang_on_parent_locks,
        ),
        case("fork_before_init_is_fine", fork_before_init_is_fine),
        case("fork_during_daos_init", fork_during_daos_init),
        case("fork_during_library_load", fork_during_library_load),
        case(
            "fork_after_probe_child_initialises",
            fork_after_probe_child_initialises,
        ),
        case(
            "fork_after_retryable_init_failure_child_initialises",
            fork_after_retryable_init_failure_child_initialises,
        ),
        case("threads_on_one_file", threads_on_one_file),
    ];
    run_main(&cases, prepare_mocks);
}
