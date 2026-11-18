// Copyright (c) 2026 Jihyeon Gim
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Public API over the in-memory backend (feature `mock`). No DAOS needed.

use std::io::Write as _;
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

use daos::mock::MemDfs;
use daos::{ConnectOptions, Dfs, Error, OpenOptions};

fn mount() -> (Arc<MemDfs>, Dfs) {
    let mem = Arc::new(MemDfs::new());
    let dfs = Dfs::connect_mock(mem.clone()).unwrap();
    (mem, dfs)
}

fn errno(e: &Error) -> Option<i32> {
    e.raw_os_error()
}

#[test]
fn write_then_read_back() {
    let (_m, dfs) = mount();
    let data: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
    dfs.write("/a/b/c.bin", &data).unwrap();
    assert_eq!(dfs.read("/a/b/c.bin").unwrap(), data);
    let md = dfs.metadata("/a/b/c.bin").unwrap();
    assert!(md.is_file());
    assert_eq!(md.len(), data.len() as u64);
    assert!(dfs.metadata("/a/b").unwrap().is_dir());
    assert!(dfs.metadata("/").unwrap().is_dir());
    assert_eq!(md.mode() & 0o777, 0o644);
}

#[test]
fn reads_short_eof_and_zero_length() {
    let (m, dfs) = mount();
    dfs.write("/f", b"hello").unwrap();
    let f = dfs.open("/f").unwrap();
    let mut buf = [0u8; 10];
    assert_eq!(f.read_at(&mut buf, 0).unwrap(), 5);
    assert_eq!(&buf[..5], b"hello");
    assert_eq!(f.read_at(&mut buf, 3).unwrap(), 2);
    assert_eq!(f.read_at(&mut buf, 10).unwrap(), 0);
    match f.read_exact_at(&mut buf, 0) {
        Err(Error::ShortRead { wanted, got, .. }) => {
            assert_eq!((wanted, got), (10, 5));
        }
        other => panic!("{other:?}"),
    }
    let before = m.calls("dfs_read");
    assert_eq!(f.read_at(&mut [], 0).unwrap(), 0);
    assert_eq!(
        m.calls("dfs_read"),
        before,
        "zero-length read makes no call"
    );
    let mut out = [0u8; 3];
    assert_eq!(dfs.read_range_into("/f", 1, &mut out).unwrap(), 3);
    assert_eq!(&out, b"ell");
}

#[test]
fn sparse_and_positional_writes() {
    let (_m, dfs) = mount();
    let f = dfs.create("/s").unwrap();
    f.write_all_at(b"xy", 10).unwrap();
    assert_eq!(f.size().unwrap(), 12);
    let mut buf = [1u8; 12];
    f.read_exact_at(&mut buf, 0).unwrap();
    assert_eq!(&buf[..10], &[0u8; 10]);
    assert_eq!(&buf[10..], b"xy");
    assert_eq!(f.metadata().unwrap().len(), 12);
    f.close().unwrap();
}

#[test]
fn open_errors() {
    let (_m, dfs) = mount();
    let e = dfs.open("/missing").unwrap_err();
    assert!(e.is_not_found());
    assert_eq!(e.kind(), std::io::ErrorKind::NotFound);
    dfs.write("/x", b"1").unwrap();
    let e = dfs
        .open_with("/x", OpenOptions::new().write(true).create_new(true))
        .unwrap_err();
    assert!(e.is_already_exists());
    let e = dfs
        .open_with("/x", OpenOptions::new().create(true))
        .unwrap_err();
    assert!(matches!(e, Error::InvalidArgument(_)));
    let e = dfs.open("/").unwrap_err();
    assert_eq!(errno(&e), Some(libc::EISDIR));
    let e = dfs.open("/x/y").unwrap_err();
    assert_eq!(errno(&e), Some(libc::ENOTDIR));
    assert!(matches!(dfs.open("/a\0b"), Err(Error::InvalidPath { .. })));
    // A write-only handle cannot read; a read-only handle cannot write.
    let w = dfs.open_with("/x", OpenOptions::new().write(true)).unwrap();
    assert_eq!(
        errno(&w.read_at(&mut [0u8; 1], 0).unwrap_err()),
        Some(libc::EPERM)
    );
    let r = dfs.open("/x").unwrap();
    assert_eq!(
        errno(&r.write_all_at(b"z", 0).unwrap_err()),
        Some(libc::EPERM)
    );
}

#[test]
fn create_parents_and_mode() {
    let (_m, dfs) = mount();
    let f = dfs
        .open_with(
            "/p/q/r.txt",
            OpenOptions::new()
                .write(true)
                .create(true)
                .create_parents(true)
                .mode(0o600),
        )
        .unwrap();
    f.write_all_at(b"ok", 0).unwrap();
    drop(f);
    assert_eq!(dfs.metadata("/p/q/r.txt").unwrap().mode() & 0o777, 0o600);
    assert!(dfs.metadata("/p/q").unwrap().is_dir());
}

#[test]
fn writer_streams_at_running_offset() {
    let (_m, dfs) = mount();
    let mut w = dfs.writer("/w/out.bin").unwrap();
    w.write_all(b"abc").unwrap();
    std::io::Write::write_all(&mut w, b"def").unwrap();
    {
        let mut bw = std::io::BufWriter::new(&mut w);
        bw.write_all(b"ghi").unwrap();
        bw.flush().unwrap();
    }
    assert_eq!(w.position(), 9);
    assert_eq!(w.finish().unwrap(), 9);
    assert_eq!(dfs.read("/w/out.bin").unwrap(), b"abcdefghi");
    // A dropped writer leaves the bytes written so far.
    let mut w = dfs.writer("/w/partial").unwrap();
    w.write_all(b"12").unwrap();
    drop(w);
    assert_eq!(dfs.read("/w/partial").unwrap(), b"12");
}

#[test]
fn directories() {
    let (_m, dfs) = mount();
    dfs.create_dir("/d").unwrap();
    assert!(dfs.create_dir("/d").unwrap_err().is_already_exists());
    assert_eq!(
        errno(&dfs.create_dir("/no/such").unwrap_err()),
        Some(libc::ENOENT)
    );
    dfs.create_dir_all("/d/e/f/g/h/i/j/k").unwrap();
    dfs.create_dir_all("/d/e/f").unwrap();
    dfs.create_dir_all("/").unwrap();
    dfs.write("/d/file", b"1").unwrap();
    assert!(dfs
        .create_dir_all("/d/file")
        .unwrap_err()
        .is_already_exists());
    assert!(dfs.exists("/d/e/f/g/h/i/j/k").unwrap());
    assert!(!dfs.exists("/d/nope").unwrap());
    assert!(dfs.try_metadata("/d/file/below").unwrap().is_none());
    assert!(dfs.create_dir("/").unwrap_err().is_already_exists());
}

#[test]
fn removal_rules() {
    let (_m, dfs) = mount();
    dfs.write("/r/a/f1", b"1").unwrap();
    dfs.write("/r/f2", b"2").unwrap();
    assert_eq!(
        errno(&dfs.remove_file("/r").unwrap_err()),
        Some(libc::EISDIR)
    );
    assert_eq!(
        errno(&dfs.remove_dir("/r/f2").unwrap_err()),
        Some(libc::ENOTDIR)
    );
    assert_eq!(
        errno(&dfs.remove_dir("/r").unwrap_err()),
        Some(libc::ENOTEMPTY)
    );
    assert_eq!(
        errno(&dfs.remove_dir_all("/r/f2").unwrap_err()),
        Some(libc::ENOTDIR)
    );
    assert_eq!(errno(&dfs.remove_all("/").unwrap_err()), Some(libc::EBUSY));
    assert_eq!(
        errno(&dfs.remove_dir_all("/").unwrap_err()),
        Some(libc::EBUSY)
    );
    assert!(dfs.remove_file("/r/missing").unwrap_err().is_not_found());
    dfs.remove_file("/r/f2").unwrap();
    dfs.remove_dir_all("/r/a").unwrap();
    dfs.remove_dir("/r").unwrap();
    assert!(!dfs.exists("/r").unwrap());
    dfs.write("/z/file", b"1").unwrap();
    dfs.remove_all("/z/file").unwrap();
    dfs.remove_all("/z").unwrap();
    assert!(!dfs.exists("/z").unwrap());
}

fn names(dfs: &Dfs, dir: &str) -> Vec<String> {
    let mut v: Vec<String> = dfs
        .read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

#[test]
fn iteration_sizes() {
    for n in [0usize, 1, 1023, 1024, 1025, 5000] {
        let (m, dfs) = mount();
        let dir = format!("/it{n}");
        dfs.create_dir(&dir).unwrap();
        for i in 0..n {
            dfs.create_dir(&format!("{dir}/e{i:05}")).unwrap();
        }
        let before = m.calls("dfs_iterate");
        let got = names(&dfs, &dir);
        assert_eq!(got.len(), n, "n = {n}");
        let mut want: Vec<String> = (0..n).map(|i| format!("e{i:05}")).collect();
        want.sort();
        assert_eq!(got, want);
        let batches = m.calls("dfs_iterate") - before;
        assert_eq!(
            batches as usize,
            n.div_ceil(1024).max(1),
            "batches for n = {n}"
        );
        assert!(!got.iter().any(|s| s == "." || s == ".."));
    }
    let (_m, dfs) = mount();
    dfs.write("/root-file", b"").unwrap();
    let entries: Vec<_> = dfs
        .read_dir("/")
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(entries, vec!["/root-file".to_string()]);
}

#[test]
fn iteration_callback_panic_gives_one_error() {
    let (m, dfs) = mount();
    for i in 0..10 {
        dfs.write(&format!("/p/{i}"), b"").unwrap();
    }
    m.set_iterate_null_name(true);
    let mut it = dfs.read_dir("/p").unwrap();
    let first = it.next().unwrap();
    assert_eq!(first.unwrap_err().raw_os_error(), Some(libc::EIO));
    assert!(it.next().is_none());
}

#[test]
fn iteration_zero_progress_safety_net() {
    let (m, dfs) = mount();
    dfs.create_dir("/s").unwrap();
    m.set_iterate_stall(true);
    let mut it = dfs.read_dir("/s").unwrap();
    let before = m.calls("dfs_iterate");
    assert_eq!(
        it.next().unwrap().unwrap_err().raw_os_error(),
        Some(libc::EIO)
    );
    assert_eq!(m.calls("dfs_iterate") - before, 64);
    assert!(it.next().is_none());
}

#[test]
fn iteration_error_is_reported_once() {
    let (m, dfs) = mount();
    dfs.create_dir("/e").unwrap();
    m.fail_next("dfs_iterate", libc::EIO);
    let mut it = dfs.read_dir("/e").unwrap();
    assert!(it.next().unwrap().is_err());
    assert!(it.next().is_none());
}

#[test]
fn create_dir_all_tolerates_a_racing_creator() {
    let (m, dfs) = mount();
    // A racing creator made /race/a between our ENOENT for /race/a/b/c and
    // our own mkdir of /race/a: that step sees EEXIST and continues because
    // the entry is a directory.
    dfs.create_dir("/race").unwrap();
    dfs.create_dir("/race/a").unwrap(); // the racing creator
                                        // Our view before the race: lookups of /race/a/b and /race/a miss.
    m.fail_nth("dfs_lookup", 0, libc::ENOENT, 2);
    let calls = m.calls("dfs_mkdir");
    dfs.create_dir_all("/race/a/b/c").unwrap();
    assert!(dfs.metadata("/race/a/b/c").unwrap().is_dir());
    assert_eq!(m.calls("dfs_mkdir") - calls, 3);
    // Second step races: /race/x is created by us (step 1), the mkdir of
    // /race/x/y (step 2) reports EEXIST. The crate checks the entry with stat;
    // here nothing exists, so the error is reported instead of hidden.
    let calls = m.calls("dfs_mkdir");
    m.fail_nth("dfs_mkdir", 1, libc::EEXIST, 1);
    let e = dfs.create_dir_all("/race/x/y").unwrap_err();
    assert!(e.is_not_found(), "{e}");
    assert_eq!(m.calls("dfs_mkdir") - calls, 2);
    // Same step when the racing creator really made the directory: success.
    dfs.create_dir("/race/x/y").unwrap();
    m.fail_next("dfs_mkdir", libc::EEXIST);
    dfs.create_dir_all("/race/x/y").unwrap();
    // An existing non-directory in the path is EEXIST.
    dfs.write("/race/file", b"").unwrap();
    assert!(dfs
        .create_dir_all("/race/file")
        .unwrap_err()
        .is_already_exists());
}

#[test]
fn dir_cache_eviction_and_stale_retry() {
    let (m, dfs) = mount();
    dfs.write("/c/d/f1", b"1").unwrap();
    let lookups = m.calls("dfs_lookup");
    dfs.metadata("/c/d/f1").unwrap();
    dfs.metadata("/c/d/f1").unwrap();
    assert_eq!(
        m.calls("dfs_lookup"),
        lookups,
        "parent handle comes from the cache"
    );
    // An ordinary "not found" under a valid cached parent costs one dfs_ostat
    // and keeps the cache.
    let ostats = m.calls("dfs_ostat");
    assert!(dfs.open("/c/d/missing").unwrap_err().is_not_found());
    assert_eq!(m.calls("dfs_ostat"), ostats + 1);
    assert_eq!(m.calls("dfs_lookup"), lookups);
    // ESTALE from a call on a valid handle is reported, not retried.
    m.fail_next("dfs_stat", libc::ESTALE);
    assert_eq!(
        errno(&dfs.metadata("/c/d/f1").unwrap_err()),
        Some(libc::ESTALE)
    );
    assert_eq!(m.calls("dfs_lookup"), lookups);
    // Own remove evicts at or below the path; a recreated directory is found.
    dfs.remove_dir_all("/c/d").unwrap();
    dfs.write("/c/d/f2", b"2").unwrap();
    assert_eq!(dfs.read("/c/d/f2").unwrap(), b"2");
}

/// Two mounts over one tree: B removes and recreates a directory that A has
/// cached. MemDfs, like libdfs, accepts creates through A's stale handle and
/// hides the result, so these checks fail without the crate's validation.
#[test]
fn stale_parent_after_external_remove() {
    let mem = Arc::new(MemDfs::new());
    let a = Dfs::connect_mock_with(mem.clone(), &ConnectOptions::new("pa", "c")).unwrap();
    let b = Dfs::connect_mock_with(mem.clone(), &ConnectOptions::new("pb", "c")).unwrap();
    a.write("/d/f0", b"0").unwrap();
    a.metadata("/d/f0").unwrap(); // caches /d in A
    b.remove_all("/d").unwrap();
    b.create_dir("/d").unwrap();
    // Creates through A reach the new directory.
    a.write("/d/f1", b"1").unwrap();
    a.create_dir("/d/sub").unwrap();
    a.open_with("/d/f2", OpenOptions::new().write(true).create_new(true))
        .unwrap();
    for p in ["/d/f1", "/d/sub", "/d/f2"] {
        assert!(b.exists(p).unwrap(), "{p} not visible through B");
    }
    assert!(a.open("/d/f0").unwrap_err().is_not_found());
    // Listing through A after another external recreate shows the new state.
    a.metadata("/d/f1").unwrap();
    b.remove_all("/d").unwrap();
    b.write("/d/new", b"n").unwrap();
    assert_eq!(names(&a, "/d"), ["new"]);
    // A non-create call under a stale parent retries with a fresh lookup.
    a.metadata("/d/new").unwrap();
    b.remove_all("/d").unwrap();
    b.write("/d/g", b"2").unwrap();
    assert_eq!(a.read("/d/g").unwrap(), b"2");
    b.remove_all("/d").unwrap();
    b.create_dir("/d").unwrap();
    assert!(a.remove_file("/d/g").unwrap_err().is_not_found());
}

#[test]
fn truncate_overwrite_with_shorter_content() {
    let (_m, dfs) = mount();
    dfs.write("/t", b"a long first version").unwrap();
    dfs.write("/t", b"short").unwrap();
    assert_eq!(dfs.read("/t").unwrap(), b"short");
    let f = dfs.create("/t").unwrap();
    assert_eq!(f.size().unwrap(), 0);
    f.write_all_at(b"xy", 0).unwrap();
    drop(f);
    assert_eq!(dfs.read("/t").unwrap(), b"xy");
    // Without truncate the tail stays.
    let f = dfs.open_with("/t", OpenOptions::new().write(true)).unwrap();
    f.write_all_at(b"z", 0).unwrap();
    drop(f);
    assert_eq!(dfs.read("/t").unwrap(), b"zy");
}

#[test]
fn file_paths_with_directory_syntax_are_rejected() {
    let (_m, dfs) = mount();
    dfs.write("/x", b"1").unwrap();
    for p in ["/x/", "/x//", "/d/x/.", "/d/x/.."] {
        assert!(
            matches!(dfs.write(p, b"1"), Err(Error::InvalidPath { .. })),
            "{p}"
        );
        assert!(matches!(dfs.open(p), Err(Error::InvalidPath { .. })), "{p}");
        assert!(
            matches!(dfs.remove_file(p), Err(Error::InvalidPath { .. })),
            "{p}"
        );
    }
    // Directory operations accept a trailing slash.
    dfs.create_dir_all("/dir/").unwrap();
    assert!(dfs.metadata("/dir/").unwrap().is_dir());
    dfs.remove_dir("/dir/").unwrap();
}

#[test]
fn create_parents_costs_nothing_when_the_parent_exists() {
    let (m, dfs) = mount();
    dfs.write("/p/a", b"1").unwrap();
    let mkdirs = m.calls("dfs_mkdir");
    let lookups = m.calls("dfs_lookup");
    dfs.write("/p/b", b"2").unwrap();
    assert_eq!(m.calls("dfs_mkdir"), mkdirs);
    assert_eq!(m.calls("dfs_lookup"), lookups);
}

#[test]
fn dir_cache_disabled_and_bounded() {
    let mem = Arc::new(MemDfs::new());
    let dfs = Dfs::connect_mock_with(
        mem.clone(),
        &ConnectOptions::new("p", "c").dir_cache_capacity(0),
    )
    .unwrap();
    dfs.write("/a/f", b"1").unwrap();
    let n = mem.open_objects();
    dfs.metadata("/a/f").unwrap();
    assert_eq!(mem.open_objects(), n, "no handle is kept without a cache");
    assert_eq!(n, 0);
    let dfs2 = Dfs::connect_mock_with(
        mem.clone(),
        &ConnectOptions::new("p", "c2").dir_cache_capacity(2),
    )
    .unwrap();
    for i in 0..5 {
        dfs2.write(&format!("/d{i}/f"), b"1").unwrap();
    }
    // Capacity 2: the cache was emptied when /d2 and /d4 were added, so it
    // holds only /d4.
    assert_eq!(mem.open_objects(), 1);
}

#[test]
fn close_waits_releases_and_rejects_later_use() {
    let (m, dfs) = mount();
    dfs.write("/f", b"data").unwrap();
    dfs.write("/dir/a", b"").unwrap();
    let file = dfs.open("/f").unwrap();
    let mut rd = dfs.read_dir("/dir").unwrap();
    assert!(m.open_objects() >= 2);
    assert_eq!(m.open_mounts(), 1);

    // A blocked operation makes close time out with Busy, mount untouched.
    let pause = m.pause_next("dfs_get_size");
    let f2 = dfs.open("/f").unwrap();
    let t = thread::spawn(move || f2.size());
    assert!(pause.wait_entered(Duration::from_secs(5)));
    match dfs.close(Duration::from_millis(100)) {
        Err(Error::Busy { in_flight, .. }) => assert_eq!(in_flight, 1),
        other => panic!("{other:?}"),
    }
    assert!(!dfs.is_closed());
    // Close waits for the running operation once it finishes.
    let d2 = dfs.clone();
    let closer = thread::spawn(move || d2.close(Duration::from_secs(10)));
    thread::sleep(Duration::from_millis(50));
    pause.release();
    assert_eq!(t.join().unwrap().unwrap(), 4);
    closer.join().unwrap().unwrap();

    assert!(dfs.is_closed());
    assert_eq!(m.open_objects(), 0, "close released every handle");
    assert_eq!(m.open_mounts(), 0);
    assert_eq!(m.open_container_handles(), 0);
    assert_eq!(m.open_pool_handles(), 0);
    let releases = m.calls("dfs_release");
    assert!(matches!(file.read_at(&mut [0u8; 1], 0), Err(Error::Closed)));
    assert!(matches!(rd.next(), Some(Err(Error::Closed))));
    assert!(matches!(dfs.metadata("/f"), Err(Error::Closed)));
    drop(file);
    drop(rd);
    assert_eq!(
        m.calls("dfs_release"),
        releases,
        "drops after close make no call"
    );
    dfs.close(Duration::from_secs(1)).unwrap(); // idempotent
                                                // A new connect makes a new mount.
    let again = Dfs::connect_mock(m.clone()).unwrap();
    assert_eq!(again.read("/f").unwrap(), b"data");
}

#[test]
fn close_refuses_new_operations_while_waiting() {
    let (m, dfs) = mount();
    dfs.write("/f", b"data").unwrap();
    let pause = m.pause_next("dfs_get_size");
    let f = dfs.open("/f").unwrap();
    let t = thread::spawn(move || f.size());
    assert!(pause.wait_entered(Duration::from_secs(5)));
    let d2 = dfs.clone();
    let closer = thread::spawn(move || d2.close(Duration::from_millis(400)));
    thread::sleep(Duration::from_millis(100));
    // close is waiting: new operations are refused.
    assert!(matches!(dfs.metadata("/f"), Err(Error::Closed)));
    assert!(matches!(dfs.open("/f"), Err(Error::Closed)));
    assert!(!dfs.is_closed());
    assert!(matches!(closer.join().unwrap(), Err(Error::Busy { .. })));
    // After Busy the mount accepts operations again.
    assert!(dfs.metadata("/f").unwrap().is_file());
    pause.release();
    assert_eq!(t.join().unwrap().unwrap(), 4);
    dfs.close(Duration::from_secs(5)).unwrap();
}

#[test]
fn close_races_file_drops() {
    for round in 0..20 {
        let (m, dfs) = mount();
        dfs.write("/f", b"x").unwrap();
        let files: Vec<daos::File> = (0..64).map(|_| dfs.open("/f").unwrap()).collect();
        let mut chunks: Vec<Vec<daos::File>> = Vec::new();
        let mut it = files.into_iter();
        for _ in 0..8 {
            chunks.push(it.by_ref().take(8).collect());
        }
        let hs: Vec<_> = chunks
            .into_iter()
            .enumerate()
            .map(|(i, chunk)| {
                thread::spawn(move || {
                    for (k, f) in chunk.into_iter().enumerate() {
                        if (i + k + round) % 3 == 0 {
                            let _ = f.close();
                        } else {
                            drop(f);
                        }
                    }
                })
            })
            .collect();
        dfs.close(Duration::from_secs(10)).unwrap();
        for h in hs {
            h.join().unwrap();
        }
        assert_eq!(m.open_objects(), 0, "round {round}");
        assert_eq!(
            m.calls("dfs_release") as usize,
            m.objects_created(),
            "every object released exactly once (round {round})"
        );
    }
}

#[test]
fn registry_rules() {
    let mem = Arc::new(MemDfs::new());
    let a = Dfs::connect_mock_with(mem.clone(), &ConnectOptions::new("pool", "c1")).unwrap();
    let b = Dfs::connect_mock_with(mem.clone(), &ConnectOptions::new("pool", "c1")).unwrap();
    assert_eq!(mem.calls("dfs_mount"), 1, "same key shares the mount");
    assert_eq!(mem.open_pool_handles(), 1);
    assert_eq!(mem.open_container_handles(), 1);
    a.write("/x", b"1").unwrap();
    assert_eq!(b.read("/x").unwrap(), b"1");
    // Another system is rejected.
    let e = Dfs::connect_mock_with(
        mem.clone(),
        &ConnectOptions::new("pool", "c2").sys_name("other"),
    )
    .unwrap_err();
    assert!(matches!(e, Error::Conflict { what: "system", .. }), "{e}");
    // Another container of the same pool and mode shares the pool handle.
    let c2 = Dfs::connect_mock_with(mem.clone(), &ConnectOptions::new("pool", "c2")).unwrap();
    assert_eq!(mem.open_pool_handles(), 1);
    assert_eq!(mem.open_container_handles(), 2);
    // The other access mode on the same pool and container: separate handles.
    let ro = Dfs::connect_mock_with(
        mem.clone(),
        &ConnectOptions::new("pool", "c1").read_only(true),
    )
    .unwrap();
    assert!(ro.is_read_only());
    assert!(!a.is_read_only());
    assert_eq!(mem.open_pool_handles(), 2);
    assert_eq!(mem.open_container_handles(), 3);
    assert_eq!(mem.open_mounts(), 3);
    assert_eq!(ro.read("/x").unwrap(), b"1");
    assert!(ro.write("/y", b"1").unwrap_err().is_permission_denied());
    // No system name and the DAOS default name are the same system.
    Dfs::connect_mock_with(
        mem.clone(),
        &ConnectOptions::new("pool", "c3").sys_name("daos_server"),
    )
    .unwrap();
    // Closing one mount closes its container; the pool stays for c2.
    a.close(Duration::from_secs(1)).unwrap();
    assert!(b.is_closed());
    assert_eq!(mem.open_container_handles(), 2);
    assert_eq!(mem.open_pool_handles(), 2);
    drop((a, b, c2, ro));
    assert_eq!(mem.open_container_handles(), 0);
    assert_eq!(mem.open_pool_handles(), 0);
    // The system rule persists after close.
    let e = Dfs::connect_mock_with(
        mem.clone(),
        &ConnectOptions::new("pool", "c1").sys_name("x"),
    )
    .unwrap_err();
    assert!(matches!(e, Error::Conflict { what: "system", .. }));
    // Bad options.
    assert!(matches!(
        Dfs::connect_mock_with(mem.clone(), &ConnectOptions::new("", "c")),
        Err(Error::InvalidArgument(_))
    ));
}

#[test]
fn connect_errors_close_what_was_opened() {
    let mem = Arc::new(MemDfs::new());
    let e = Dfs::connect_mock_with(mem.clone(), &ConnectOptions::new("missing", "c")).unwrap_err();
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
    let e = Dfs::connect_mock_with(mem.clone(), &ConnectOptions::new("p", "missing")).unwrap_err();
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
    assert_eq!(e.kind(), std::io::ErrorKind::NotFound);
    let e = Dfs::connect_mock_with(mem.clone(), &ConnectOptions::new("p", "plain")).unwrap_err();
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
    assert_eq!(mem.open_pool_handles(), 0);
    assert_eq!(mem.open_container_handles(), 0);
    assert_eq!(mem.calls("daos_cont_close"), 1);
    assert_eq!(mem.calls("daos_pool_disconnect"), 2);
}

#[test]
fn mount_retries_enoent() {
    let mem = Arc::new(MemDfs::new());
    mem.fail_nth("dfs_mount", 0, libc::ENOENT, 2);
    let dfs = Dfs::connect_mock(mem.clone()).unwrap();
    assert_eq!(mem.calls("dfs_mount"), 3);
    dfs.close(Duration::from_secs(1)).unwrap();
    mem.fail_nth("dfs_mount", 0, libc::ENOENT, 50);
    let e = Dfs::connect_mock(mem.clone()).unwrap_err();
    assert_eq!(e.raw_os_error(), Some(libc::ENOENT));
    assert_eq!(mem.calls("dfs_mount"), 3 + 7);
    assert_eq!(mem.open_pool_handles(), 0);
    mem.clear_faults();
}

#[test]
fn close_reports_container_close_errors() {
    let mem = Arc::new(MemDfs::new());
    let dfs = Dfs::connect_mock(mem.clone()).unwrap();
    mem.fail_next("daos_cont_close", -1012);
    let e = dfs.close(Duration::from_secs(1)).unwrap_err();
    assert!(
        matches!(
            e,
            Error::Daos {
                op: "daos_cont_close",
                ..
            }
        ),
        "{e}"
    );
    assert_eq!(e.der(), Some(daos::DerCode::BUSY));
    assert!(dfs.is_closed());
    assert_eq!(mem.open_mounts(), 0);
}

#[test]
fn last_drop_disconnects_and_releases() {
    let mem = Arc::new(MemDfs::new());
    {
        let dfs = Dfs::connect_mock(mem.clone()).unwrap();
        dfs.write("/a/b/c", b"1").unwrap();
        let _f = dfs.open("/a/b/c").unwrap();
        let _rd = dfs.read_dir("/a").unwrap();
        assert!(mem.open_objects() > 0);
    }
    assert_eq!(mem.open_objects(), 0);
    assert_eq!(mem.open_mounts(), 0);
    assert_eq!(mem.open_container_handles(), 0);
    assert_eq!(mem.open_pool_handles(), 0);
    assert_eq!(mem.calls("dfs_umount"), 1);
    assert_eq!(mem.calls("daos_cont_close"), 1);
    assert_eq!(mem.calls("daos_pool_disconnect"), 1);
    // Init happened once, never a fini (the backend has none).
    assert_eq!(mem.calls("daos_init"), 1);
    assert_eq!(mem.calls("dfs_init"), 1);
}

#[test]
fn read_only_mount_rejects_writes() {
    let mem = Arc::new(MemDfs::new());
    let rw = Dfs::connect_mock_with(mem.clone(), &ConnectOptions::new("rw", "c")).unwrap();
    rw.write("/f", b"1").unwrap();
    let ro = Dfs::connect_mock_with(mem.clone(), &ConnectOptions::new("ro", "c").read_only(true))
        .unwrap();
    assert_eq!(ro.read("/f").unwrap(), b"1");
    assert!(ro.write("/g", b"1").unwrap_err().is_permission_denied());
    assert!(ro.create_dir("/d").unwrap_err().is_permission_denied());
    assert!(ro.remove_file("/f").unwrap_err().is_permission_denied());
    let f = ro.open("/f").unwrap();
    assert!(f.write_all_at(b"x", 0).unwrap_err().is_permission_denied());
}

#[test]
fn errors_convert_to_io_errors() {
    let (_m, dfs) = mount();
    let e: std::io::Error = dfs.open("/nope").unwrap_err().into();
    assert_eq!(e.raw_os_error(), Some(libc::ENOENT));
    let e = dfs.open("/nope").unwrap_err();
    assert_eq!(
        e.to_string(),
        "dfs_open(\"/nope\") failed: ENOENT (2): No such file or directory"
    );
}

#[test]
fn send_sync_and_threads() {
    fn assert_send_sync<T: Send + Sync>() {}
    fn assert_send<T: Send>() {}
    assert_send_sync::<Dfs>();
    assert_send_sync::<daos::File>();
    assert_send::<daos::Writer>();
    assert_send::<daos::ReadDir>();
    assert_send_sync::<Error>();

    let (_m, dfs) = mount();
    let data: Vec<u8> = (0..1u32 << 16).map(|i| (i * 7) as u8).collect();
    dfs.write("/shared", &data).unwrap();
    let file = Arc::new(dfs.open("/shared").unwrap());
    let (tx, rx) = mpsc::channel();
    let mut hs = Vec::new();
    for t in 0..16usize {
        let file = file.clone();
        let dfs = dfs.clone();
        let data = data.clone();
        let tx = tx.clone();
        hs.push(thread::spawn(move || {
            let mut buf = vec![0u8; 4096];
            for k in 0..16usize {
                let off = ((t + k) % 16) * 4096;
                file.read_exact_at(&mut buf, off as u64).unwrap();
                assert_eq!(&buf[..], &data[off..off + 4096]);
            }
            let own = format!("/own/{t}");
            dfs.write(&own, &buf).unwrap();
            assert_eq!(dfs.read(&own).unwrap(), buf);
            tx.send(()).unwrap();
        }));
    }
    drop(tx);
    for h in hs {
        h.join().unwrap();
    }
    assert_eq!(rx.iter().count(), 16);
    assert_eq!(names(&dfs, "/own").len(), 16);
}
