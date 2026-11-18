// Copyright (c) 2026 Jihyeon Gim
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Integration tests against a real DAOS system. Skipped unless `DAOS_POOL`
//! and `DAOS_CONT` are set (optional `DAOS_SYS`); with
//! `RUST_DAOS_REQUIRE_REAL=1` a missing variable is a failure.
//!
//! ```text
//! CRT_MRC_ENABLE=0 DAOS_POOL=tank DAOS_CONT=posix cargo test --test real_daos
//! ```
//!
//! Every case runs in its own subprocess and works below
//! `/daos-crate-test/<case>-<pid>-<nanos>`, removed at the end; the last case
//! removes `/daos-crate-test` itself.

mod common;

use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use common::*;
use daos::{ConnectOptions, Dfs, Error, InitOptions, OpenOptions};

const ROOT: &str = "/daos-crate-test";
const T: Duration = Duration::from_secs(60);

struct Env {
    pool: String,
    cont: String,
    sys: Option<String>,
}

fn env() -> Env {
    let pool = std::env::var("DAOS_POOL").ok();
    let cont = std::env::var("DAOS_CONT").ok();
    match (pool, cont) {
        (Some(pool), Some(cont)) => Env {
            pool,
            cont,
            sys: std::env::var("DAOS_SYS").ok(),
        },
        _ => {
            if std::env::var("RUST_DAOS_REQUIRE_REAL").as_deref() == Ok("1") {
                panic!("RUST_DAOS_REQUIRE_REAL=1 but DAOS_POOL or DAOS_CONT is not set");
            }
            skip("DAOS_POOL and DAOS_CONT are not set");
        }
    }
}

/// Init (first statement of every case, before threads) and connect.
fn setup() -> (Env, Dfs, String) {
    let e = env();
    // SAFETY: called first in a single-threaded test subprocess.
    let report = unsafe { daos::init(&InitOptions::default()) }.expect("daos::init");
    println!("init report: {report:?}");
    let dfs = connect(&e, false);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let case = std::env::var("RUST_DAOS_CASE").unwrap();
    let base = format!("{ROOT}/{case}-{}-{nanos}", std::process::id());
    dfs.create_dir_all(&base).unwrap();
    (e, dfs, base)
}

fn connect(e: &Env, read_only: bool) -> Dfs {
    let mut o = ConnectOptions::new(&e.pool, &e.cont).read_only(read_only);
    if let Some(s) = &e.sys {
        o = o.sys_name(s);
    }
    Dfs::connect_with(&o).expect("connect")
}

fn finish(dfs: Dfs, base: &str) {
    dfs.remove_all(base).unwrap();
    assert!(!dfs.exists(base).unwrap());
    dfs.close(T).unwrap();
}

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u32).wrapping_mul(2_654_435_761).to_le_bytes()[2] ^ seed)
        .collect()
}

// ---------------------------------------------------------------------------

fn capabilities() {
    let _ = env();
    let r = unsafe { daos::init(&InitOptions::default()) }.unwrap();
    println!("report: {r:#?}");
    // daos_reinit exists from 2.6.4, dfs_connect_snap from 2.8. Set
    // RUST_DAOS_EXPECT_DAOS=2.6 or 2.8 to check the second one too.
    assert!(
        r.library.has_reinit,
        "DAOS 2.6.4 and later have daos_reinit"
    );
    match std::env::var("RUST_DAOS_EXPECT_DAOS").as_deref() {
        Ok("2.6") => assert!(!r.library.has_connect_snap, "2.6 has no dfs_connect_snap"),
        Ok("2.8") => assert!(r.library.has_connect_snap, "2.8 has dfs_connect_snap"),
        _ => println!("has_connect_snap = {}", r.library.has_connect_snap),
    }
    assert_eq!(daos::library_info().unwrap(), r.library);
    // Open item: daos_init is reachable through the libdfs handle itself.
    // SAFETY: the real client library; only a symbol lookup.
    let lib = unsafe {
        libloading::os::unix::Library::open(
            Some("libdfs.so"),
            libloading::os::unix::RTLD_NOW | libloading::os::unix::RTLD_LOCAL,
        )
    }
    .unwrap();
    for sym in ["daos_init", "daos_fini", "daos_reinit"] {
        let found = unsafe { lib.get::<unsafe extern "C" fn() -> i32>(sym.as_bytes()) }.is_ok();
        println!("dlsym(libdfs handle, {sym}) = {found}");
        assert!(found);
    }
}

fn write_read_sizes_and_sparse() {
    let (_e, dfs, base) = setup();
    for (len, name) in [(1usize, "one"), (1 << 20, "mib"), (64 << 20, "64mib")] {
        let p = format!("{base}/{name}");
        let data = pattern(len, len as u8);
        dfs.write(&p, &data).unwrap();
        let back = dfs.read(&p).unwrap();
        assert!(back == data, "pattern mismatch for {name}");
        assert_eq!(dfs.metadata(&p).unwrap().len(), len as u64);
        let f = dfs.open(&p).unwrap();
        assert_eq!(f.size().unwrap(), len as u64);
        assert_eq!(f.metadata().unwrap().len(), len as u64);
        let mut tail = vec![0u8; 100];
        let n = f.read_at(&mut tail, len as u64 - 1).unwrap();
        assert_eq!(n, 1, "short read at end of file");
        assert!(matches!(
            f.read_exact_at(&mut tail, len as u64 - 1),
            Err(Error::ShortRead { .. })
        ));
        assert_eq!(f.read_at(&mut [], 0).unwrap(), 0);
    }
    // Sparse offsets read back as zeros.
    let f = dfs
        .open_with(
            &format!("{base}/sparse"),
            OpenOptions::new().read(true).write(true).create(true),
        )
        .unwrap();
    f.write_all_at(b"tail", 10 << 20).unwrap();
    f.write_all_at(b"head", 0).unwrap();
    assert_eq!(f.size().unwrap(), (10 << 20) + 4);
    let mut mid = vec![1u8; 4096];
    f.read_exact_at(&mut mid, 5 << 20).unwrap();
    assert!(mid.iter().all(|b| *b == 0));
    let mut t = [0u8; 4];
    f.read_exact_at(&mut t, 10 << 20).unwrap();
    assert_eq!(&t, b"tail");
    f.close().unwrap();
    // Streaming writer.
    let mut w = dfs.writer(&format!("{base}/w/stream")).unwrap();
    for i in 0..10u8 {
        w.write_all(&pattern(100_000, i)).unwrap();
    }
    assert_eq!(w.finish().unwrap(), 1_000_000);
    assert_eq!(
        dfs.metadata(&format!("{base}/w/stream")).unwrap().len(),
        1_000_000
    );
    finish(dfs, &base);
}

fn errors_and_directories() {
    let (_e, dfs, base) = setup();
    let f = format!("{base}/exists");
    dfs.write(&f, b"1").unwrap();
    let e = dfs
        .open_with(&f, OpenOptions::new().write(true).create_new(true))
        .unwrap_err();
    assert!(e.is_already_exists(), "{e}");
    assert_eq!(e.kind(), std::io::ErrorKind::AlreadyExists);
    let e = dfs.open(&format!("{base}/missing")).unwrap_err();
    assert!(e.is_not_found(), "{e}");
    println!("not found: {e}");
    let deep = format!("{base}/d1/d2/d3/d4/d5/d6/d7/d8");
    dfs.create_dir_all(&deep).unwrap();
    dfs.create_dir_all(&deep).unwrap();
    let md = dfs.metadata(&deep).unwrap();
    assert!(md.is_dir(), "mkdir result is a directory");
    assert_eq!(md.mode() & 0o777, 0o755);
    assert!(dfs.metadata(&f).unwrap().is_file());
    assert_eq!(dfs.metadata(&f).unwrap().mode() & 0o777, 0o644);
    assert!(dfs.create_dir_all(&f).unwrap_err().is_already_exists());
    assert!(dfs.try_metadata(&format!("{f}/below")).unwrap().is_none());
    let e = dfs.remove_dir(&format!("{base}/d1")).unwrap_err();
    assert_eq!(e.raw_os_error(), Some(libc::ENOTEMPTY), "{e}");
    assert_eq!(
        dfs.remove_file(&format!("{base}/d1"))
            .unwrap_err()
            .raw_os_error(),
        Some(libc::EISDIR)
    );
    assert_eq!(
        dfs.remove_dir(&f).unwrap_err().raw_os_error(),
        Some(libc::ENOTDIR)
    );
    assert_eq!(
        dfs.remove_all("/").unwrap_err().raw_os_error(),
        Some(libc::EBUSY)
    );
    dfs.remove_dir_all(&format!("{base}/d1")).unwrap();
    assert!(!dfs.exists(&format!("{base}/d1")).unwrap());
    // Remove and recreate under a cached parent handle.
    dfs.write(&format!("{base}/cache/x/f"), b"1").unwrap();
    dfs.metadata(&format!("{base}/cache/x/f")).unwrap();
    dfs.remove_dir_all(&format!("{base}/cache/x")).unwrap();
    dfs.write(&format!("{base}/cache/x/g"), b"2").unwrap();
    assert_eq!(dfs.read(&format!("{base}/cache/x/g")).unwrap(), b"2");
    // O_TRUNC overwrite with shorter content.
    let t = format!("{base}/trunc");
    dfs.write(&t, &pattern(1 << 20, 9)).unwrap();
    dfs.write(&t, b"short").unwrap();
    assert_eq!(dfs.read(&t).unwrap(), b"short");
    assert_eq!(dfs.metadata(&t).unwrap().len(), 5);
    // Root metadata.
    assert!(dfs.metadata("/").unwrap().is_dir());
    finish(dfs, &base);
}

fn read_dir_3000() {
    let (_e, dfs, base) = setup();
    let dir = format!("{base}/many");
    dfs.create_dir(&dir).unwrap();
    for i in 0..3000 {
        let p = format!("{dir}/entry-{i:05}");
        if i % 2 == 0 {
            dfs.write(&p, b"").unwrap();
        } else {
            dfs.create_dir(&p).unwrap();
        }
    }
    let mut names: Vec<String> = dfs
        .read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names.len(), 3000);
    assert!(
        !names.iter().any(|n| n == "." || n == ".."),
        "no dot entries"
    );
    let want: Vec<String> = (0..3000).map(|i| format!("entry-{i:05}")).collect();
    assert_eq!(names, want);
    let empty = format!("{base}/empty");
    dfs.create_dir(&empty).unwrap();
    assert_eq!(dfs.read_dir(&empty).unwrap().count(), 0);
    finish(dfs, &base);
}

/// libdfs accepts a 4095-byte path (`DFS_MAX_PATH - 1`); the crate refuses
/// 4096 bytes before any call.
fn path_limits() {
    let (_e, dfs, base) = setup();
    let at = common::path_of_len(&base, daos::path::MAX_PATH - 1);
    assert_eq!(at.len(), 4095);
    dfs.write(&at, b"edge").unwrap();
    assert_eq!(dfs.read(&at).unwrap(), b"edge");
    assert!(dfs.metadata(&at).unwrap().is_file());
    let over = common::path_of_len(&base, daos::path::MAX_PATH);
    assert_eq!(over.len(), 4096);
    assert!(matches!(
        dfs.write(&over, b"x"),
        Err(Error::InvalidPath { .. })
    ));
    assert!(matches!(
        dfs.metadata(&over),
        Err(Error::InvalidPath { .. })
    ));
    finish(dfs, &base);
}

fn read_only_mount() {
    let e = env();
    unsafe { daos::init(&InitOptions::default()) }.unwrap();
    let ro = connect(&e, true);
    assert!(ro.is_read_only());
    assert!(ro.metadata("/").unwrap().is_dir());
    let _ = ro.read_dir("/").unwrap().count();
    let err = ro.write(&format!("{ROOT}-ro-probe"), b"1").unwrap_err();
    assert!(err.is_permission_denied(), "{err}");
    println!("write on read-only mount: {err}");
    assert!(ro
        .create_dir(&format!("{ROOT}-ro-dir"))
        .unwrap_err()
        .is_permission_denied());
    // The other mode on the same pool and container is a separate mount on
    // its own pool and container handles; both work side by side.
    let rw = connect(&e, false);
    assert!(!rw.is_read_only());
    let probe = format!("{ROOT}-ro-visible-{}", std::process::id());
    rw.write(&probe, b"seen").unwrap();
    assert_eq!(ro.read(&probe).unwrap(), b"seen");
    rw.remove_file(&probe).unwrap();
    assert!(ro.metadata("/").unwrap().is_dir());
    ro.close(T).unwrap();
    assert!(!rw.is_closed());
    rw.close(T).unwrap();
}

fn unknown_pool_and_system_conflict() {
    let e = env();
    unsafe { daos::init(&InitOptions::default()) }.unwrap();
    let err = Dfs::connect("no-such-pool-daos-crate", &e.cont).unwrap_err();
    println!("unknown pool: {err}");
    assert!(
        matches!(
            err,
            Error::Daos {
                op: "daos_pool_connect",
                ..
            }
        ),
        "{err}"
    );
    assert!(err
        .to_string()
        .starts_with("daos_pool_connect(\"no-such-pool-daos-crate\") failed: DER_"));
    let mut o = ConnectOptions::new(&e.pool, "no-such-container-daos-crate");
    if let Some(s) = &e.sys {
        o = o.sys_name(s);
    }
    let err = Dfs::connect_with(&o).unwrap_err();
    println!("unknown container: {err}");
    assert!(
        matches!(
            err,
            Error::Daos {
                op: "daos_cont_open",
                ..
            }
        ),
        "{err}"
    );
    assert!(err.is_not_found(), "{err}");
    let dfs = connect(&e, false);
    let err = Dfs::connect_with(&ConnectOptions::new(&e.pool, &e.cont).sys_name("other_system"))
        .unwrap_err();
    assert!(
        matches!(err, Error::Conflict { what: "system", .. }),
        "{err}"
    );
    // Same key: the live mount is shared.
    let again = connect(&e, false);
    again.close(T).unwrap();
    assert!(dfs.is_closed(), "clones of one mount close together");
}

fn threads_one_file_and_own_files() {
    let (_e, dfs, base) = setup();
    let data = pattern(16 << 20, 7);
    let p = format!("{base}/shared");
    dfs.write(&p, &data).unwrap();
    let file = Arc::new(dfs.open(&p).unwrap());
    let data = Arc::new(data);
    let hs: Vec<_> = (0..16usize)
        .map(|t| {
            let file = file.clone();
            let data = data.clone();
            let dfs = dfs.clone();
            let base = base.clone();
            thread::spawn(move || {
                let mut buf = vec![0u8; 1 << 20];
                for k in 0..16usize {
                    let off = ((t + k) % 16) << 20;
                    file.read_exact_at(&mut buf, off as u64).unwrap();
                    assert!(buf[..] == data[off..off + (1 << 20)]);
                }
                let own = format!("{base}/own-{t}");
                let mine = pattern(4 << 20, t as u8);
                dfs.write(&own, &mine).unwrap();
                assert!(dfs.read(&own).unwrap() == mine);
            })
        })
        .collect();
    for h in hs {
        h.join().unwrap();
    }
    drop(file);
    assert_eq!(dfs.read_dir(&base).unwrap().count(), 17);
    finish(dfs, &base);
}

/// MR cache stress: threads repeatedly allocate a 64 MiB buffer, read into
/// it, and free it. Run twice by the case table: with CRT_MRC_ENABLE=0 and
/// with the variable removed from the environment. Scale with
/// `RUST_DAOS_MR_STRESS_THREADS` (default 8) and `RUST_DAOS_MR_STRESS_ITERS` (12).
fn mr_stress() {
    let (_e, dfs, base) = setup();
    let r = daos::init_report().unwrap();
    println!(
        "CRT_MRC_ENABLE before={:?} after={:?} FI_MR_CACHE_MAX_COUNT after={:?} warnings={:?}",
        r.crt_mrc_enable_before, r.crt_mrc_enable_after, r.fi_mr_cache_max_count_after, r.warnings
    );
    let p = format!("{base}/big");
    let data = pattern(64 << 20, 3);
    dfs.write(&p, &data).unwrap();
    let file = Arc::new(dfs.open(&p).unwrap());
    let num = |k: &str, d: usize| {
        std::env::var(k)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(d)
    };
    let threads = num("RUST_DAOS_MR_STRESS_THREADS", 8);
    let iterations = num("RUST_DAOS_MR_STRESS_ITERS", 12);
    let start = Instant::now();
    let hs: Vec<_> = (0..threads)
        .map(|t| {
            let file = file.clone();
            let check = data[..4096].to_vec();
            thread::spawn(move || {
                for i in 0..iterations {
                    let mut buf = vec![0u8; 64 << 20];
                    file.read_exact_at(&mut buf, 0).unwrap();
                    assert!(buf[..4096] == check[..], "thread {t} iteration {i}");
                    drop(buf);
                }
            })
        })
        .collect();
    for h in hs {
        h.join().unwrap();
    }
    let secs = start.elapsed().as_secs_f64();
    let gib = (threads * iterations * 64) as f64 / 1024.0;
    println!(
        "mr_stress: {threads} threads x {iterations} reads of 64 MiB in {secs:.1}s ({:.2} GiB/s)",
        gib / secs
    );
    drop(file);
    finish(dfs, &base);
}

fn fork_child_refused() {
    let (_e, dfs, base) = setup();
    let p = format!("{base}/f");
    dfs.write(&p, b"parent").unwrap();
    let file = dfs.open(&p).unwrap();
    let parent = std::process::id();
    let d = dfs.clone();
    let p2 = p.clone();
    let code = fork_and_wait(move || {
        let f = |r: Result<(), Error>| matches!(r, Err(Error::Forked { created_pid, .. }) if created_pid == parent);
        let mut ok = true;
        ok &= f(Dfs::connect("x", "y").map(|_| ()));
        ok &= f(d.metadata(&p2).map(|_| ()));
        ok &= f(d.write(&p2, b"child").map(|_| ()));
        ok &= f(file.read_at(&mut [0u8; 6], 0).map(|_| ()));
        ok &= f(unsafe { daos::init(&InitOptions::default()) }.map(|_| ()));
        drop(file);
        drop(d);
        i32::from(!ok)
    });
    assert_eq!(code, 0);
    // The parent continues on the same mount.
    assert_eq!(dfs.read(&p).unwrap(), b"parent");
    finish(dfs, &base);
}

fn connect_close_connect() {
    let (e, dfs, base) = setup();
    let p = format!("{base}/f");
    dfs.write(&p, b"one").unwrap();
    let held = dfs.open(&p).unwrap();
    dfs.close(T).unwrap();
    assert!(matches!(held.read_at(&mut [0u8; 3], 0), Err(Error::Closed)));
    drop(held);
    for round in 0..3 {
        let d = connect(&e, false);
        assert_eq!(d.read(&p).unwrap(), b"one", "round {round}");
        d.close(T).unwrap();
    }
    finish(connect(&e, false), &base);
}

/// On real DAOS, a last close must not re-initialise the library: thread A does I/O and stays alive parked on a
/// channel; B closes the last mount and connects again; A then does I/O on a
/// file opened from the new mount.
fn last_close_no_reinit() {
    let (e, dfs, base) = setup();
    let p1 = format!("{base}/a");
    let p2 = format!("{base}/b");
    let d1 = pattern(1 << 20, 1);
    let d2 = pattern(1 << 20, 2);
    dfs.write(&p1, &d1).unwrap();
    let (to_a, a_rx) = mpsc::channel::<daos::File>();
    let (to_b, b_rx) = mpsc::channel::<Result<Vec<u8>, String>>();
    let a_dfs = dfs.clone();
    let p1a = p1.clone();
    let a = thread::spawn(move || {
        let first = a_dfs.read(&p1a).map_err(|e| e.to_string());
        drop(a_dfs);
        to_b.send(first).unwrap();
        let file = a_rx.recv().unwrap();
        let mut buf = vec![0u8; 1 << 20];
        let r = file
            .read_exact_at(&mut buf, 0)
            .map(|_| buf)
            .map_err(|e| e.to_string());
        to_b.send(r).unwrap();
    });
    assert!(b_rx.recv().unwrap().unwrap() == d1);
    dfs.close(T).unwrap();
    drop(dfs);
    let dfs = connect(&e, false);
    dfs.write(&p2, &d2).unwrap();
    to_a.send(dfs.open(&p2).unwrap()).unwrap();
    let r = b_rx.recv().unwrap();
    a.join().unwrap();
    assert!(r.expect("thread A I/O after reconnect") == d2);
    finish(dfs, &base);
}

/// Another process (or mount) removes and recreates a directory that this
/// mount has a cached handle for. Creates, listings and lookups through this
/// mount must then reach the new directory, as seen from a third process.
fn two_mounts_external_remove_recreate() {
    let (_e, dfs, base) = setup();
    let d = format!("{base}/d");
    dfs.write(&format!("{d}/f0"), b"0").unwrap();
    dfs.create_dir(&format!("{d}/sub0")).unwrap();
    // Cache the handle of `d` in this mount.
    assert!(dfs.metadata(&format!("{d}/f0")).unwrap().is_file());
    assert_eq!(dfs.read_dir(&d).unwrap().count(), 2);

    // Process B removes `d` and creates a new, empty `d`.
    assert!(run_helper(
        "remove_recreate",
        &[("RUST_DAOS_HELPER_PATH", d.as_str())]
    ));

    // Creates through this mount land in the new directory (the first call
    // after the external change uses the cached parent handle).
    let f1 = format!("{d}/f1");
    dfs.write(&f1, b"one").unwrap();
    let sub = format!("{d}/sub1");
    dfs.create_dir(&sub).unwrap();
    let f2 = format!("{d}/f2");
    let w = dfs
        .open_with(&f2, OpenOptions::new().write(true).create_new(true))
        .unwrap();
    w.write_all_at(b"two", 0).unwrap();
    w.close().unwrap();
    for p in [&f1, &sub, &f2] {
        assert!(
            run_helper("exists", &[("RUST_DAOS_HELPER_PATH", p.as_str())]),
            "{p} not visible from another process"
        );
    }
    // The old entries are gone.
    assert!(dfs.open(&format!("{d}/f0")).unwrap_err().is_not_found());
    let mut names: Vec<String> = dfs
        .read_dir(&d)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names, ["f1", "f2", "sub1"]);
    // Second round: external remove and recreate, then list first.
    dfs.metadata(&f1).unwrap();
    assert!(run_helper(
        "remove_recreate",
        &[("RUST_DAOS_HELPER_PATH", d.as_str())]
    ));
    assert!(run_helper(
        "create",
        &[("RUST_DAOS_HELPER_PATH", &format!("{d}/new"))]
    ));
    let names: Vec<String> = dfs
        .read_dir(&d)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["new"], "listing through a stale handle");
    dfs.write(&f1, b"one").unwrap();
    dfs.create_dir(&sub).unwrap();
    dfs.write(&f2, b"two").unwrap();
    assert_eq!(dfs.read_dir(&d).unwrap().count(), 4);
    assert_eq!(dfs.read(&f1).unwrap(), b"one");
    finish(dfs, &base);
}

/// Timing of creates, stats and reads at depth 5 with the directory cache
/// (capacity from `RUST_DAOS_BENCH_DIR_CACHE`). Prints numbers only.
fn bench_dir_cache() {
    let e = env();
    unsafe { daos::init(&InitOptions::default()) }.unwrap();
    let cap: usize = std::env::var("RUST_DAOS_BENCH_DIR_CACHE")
        .unwrap()
        .parse()
        .unwrap();
    let mut o = ConnectOptions::new(&e.pool, &e.cont).dir_cache_capacity(cap);
    if let Some(s) = &e.sys {
        o = o.sys_name(s);
    }
    let dfs = Dfs::connect_with(&o).unwrap();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let base = format!("{ROOT}/bench-{cap}-{}-{nanos}", std::process::id());
    let dir = format!("{base}/a/b/c/d");
    dfs.create_dir_all(&dir).unwrap();
    let n = 300;
    let t0 = Instant::now();
    for i in 0..n {
        dfs.write(&format!("{dir}/f{i}"), b"x").unwrap();
    }
    let create = t0.elapsed();
    let t0 = Instant::now();
    for i in 0..n {
        dfs.metadata(&format!("{dir}/f{i}")).unwrap();
    }
    let stat = t0.elapsed();
    let t0 = Instant::now();
    for i in 0..n {
        dfs.read(&format!("{dir}/f{i}")).unwrap();
    }
    let read = t0.elapsed();
    println!(
        "dir_cache_capacity={cap}: {n} creates {:.1} us/op, stats {:.1} us/op, reads {:.1} us/op",
        create.as_secs_f64() * 1e6 / n as f64,
        stat.as_secs_f64() * 1e6 / n as f64,
        read.as_secs_f64() * 1e6 / n as f64
    );
    finish(dfs, &base);
}

fn helper_remove_recreate() {
    let e = env();
    unsafe { daos::init(&InitOptions::default()) }.unwrap();
    let dfs = connect(&e, false);
    let p = std::env::var("RUST_DAOS_HELPER_PATH").unwrap();
    dfs.remove_all(&p).unwrap();
    dfs.create_dir(&p).unwrap();
    dfs.close(T).unwrap();
}

fn helper_create() {
    let e = env();
    unsafe { daos::init(&InitOptions::default()) }.unwrap();
    let dfs = connect(&e, false);
    let p = std::env::var("RUST_DAOS_HELPER_PATH").unwrap();
    dfs.write(&p, b"new").unwrap();
    dfs.close(T).unwrap();
}

fn helper_exists() {
    let e = env();
    unsafe { daos::init(&InitOptions::default()) }.unwrap();
    let dfs = connect(&e, false);
    let p = std::env::var("RUST_DAOS_HELPER_PATH").unwrap();
    let found = dfs.exists(&p).unwrap();
    dfs.close(T).unwrap();
    std::process::exit(i32::from(!found));
}

fn cleanup_root() {
    let e = env();
    unsafe { daos::init(&InitOptions::default()) }.unwrap();
    let dfs = connect(&e, false);
    match dfs.remove_all(ROOT) {
        Ok(()) => println!("removed {ROOT}"),
        Err(err) if err.is_not_found() => println!("{ROOT} already absent"),
        Err(err) => panic!("{err}"),
    }
    assert!(!dfs.exists(ROOT).unwrap());
    dfs.close(T).unwrap();
}

fn main() {
    let cases = [
        case("capabilities", capabilities),
        case("write_read_sizes_and_sparse", write_read_sizes_and_sparse),
        case("errors_and_directories", errors_and_directories),
        case("read_dir_3000", read_dir_3000),
        case("path_limits", path_limits),
        case("read_only_mount", read_only_mount),
        case(
            "unknown_pool_and_system_conflict",
            unknown_pool_and_system_conflict,
        ),
        case(
            "threads_one_file_and_own_files",
            threads_one_file_and_own_files,
        ),
        Case {
            name: "mr_stress_launcher_disabled",
            run: mr_stress,
            env: &[("CRT_MRC_ENABLE", Some("0"))],
        },
        Case {
            name: "mr_stress_default_env",
            run: mr_stress,
            env: &[("CRT_MRC_ENABLE", None)],
        },
        case("fork_child_refused", fork_child_refused),
        case("connect_close_connect", connect_close_connect),
        case("last_close_no_reinit", last_close_no_reinit),
        case(
            "two_mounts_external_remove_recreate",
            two_mounts_external_remove_recreate,
        ),
        Case {
            name: "bench_dir_cache_default",
            run: bench_dir_cache,
            env: &[("RUST_DAOS_BENCH_DIR_CACHE", Some("1024"))],
        },
        Case {
            name: "bench_dir_cache_off",
            run: bench_dir_cache,
            env: &[("RUST_DAOS_BENCH_DIR_CACHE", Some("0"))],
        },
        case("helper_remove_recreate", helper_remove_recreate),
        case("helper_exists", helper_exists),
        case("helper_create", helper_create),
        case("cleanup_root", cleanup_root),
    ];
    run_main(&cases, Vec::new);
}
