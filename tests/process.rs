// Copyright (c) 2026 Jihyeon Gim
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Fork and process-global cases over the in-memory backend (feature
//! `mock`). Each case runs in its own subprocess.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use daos::mock::MemDfs;
use daos::{Dfs, Error};

fn is_forked<T>(r: Result<T, Error>) -> bool {
    matches!(r, Err(Error::Forked { .. }))
}

fn fork_refuses_inherited_handles() {
    let mem = Arc::new(MemDfs::new());
    let dfs = Dfs::connect_mock(mem.clone()).unwrap();
    dfs.write("/f", b"abc").unwrap();
    let file = dfs.open("/f").unwrap();
    let rd = dfs.read_dir("/").unwrap();
    let w = dfs.writer("/w").unwrap();
    let m = mem.clone();
    let code = fork_and_wait(move || {
        let ops = [
            "dfs_release",
            "dfs_umount",
            "daos_cont_close",
            "daos_pool_disconnect",
            "dfs_lookup",
            "dfs_stat",
            "dfs_read",
        ];
        let before: Vec<u64> = ops.iter().map(|o| m.calls(o)).collect();
        let mut ok = true;
        ok &= is_forked(Dfs::connect_mock(m.clone()));
        ok &= is_forked(dfs.metadata("/f"));
        ok &= is_forked(dfs.create_dir("/d"));
        ok &= is_forked(file.read_at(&mut [0u8; 3], 0));
        ok &= is_forked(file.size());
        ok &= is_forked(dfs.close(Duration::from_secs(1)));
        let mut rd = rd;
        ok &= is_forked(rd.next().unwrap());
        let mut w = w;
        ok &= is_forked(w.write_all(b"x"));
        ok &= is_forked(file.close());
        ok &= dfs.is_closed();
        drop((rd, w, dfs));
        let after: Vec<u64> = ops.iter().map(|o| m.calls(o)).collect();
        ok &= before == after;
        ok &= m.open_mounts() == 1;
        i32::from(!ok)
    });
    assert_eq!(code, 0);
    // The parent is unaffected.
    assert_eq!(
        Dfs::connect_mock(mem.clone()).unwrap().read("/f").unwrap(),
        b"abc"
    );
}

fn fork_child_with_parent_thread_inside_a_call() {
    let mem = Arc::new(MemDfs::new());
    let dfs = Dfs::connect_mock(mem.clone()).unwrap();
    dfs.write("/f", b"abc").unwrap();
    // A parent thread blocks inside dfs_get_size, holding the mount gate
    // shared; MemDfs's own mutexes are free.
    let pause = mem.pause_next("dfs_get_size");
    let f = dfs.open("/f").unwrap();
    let t = std::thread::spawn(move || f.size());
    assert!(pause.wait_entered(Duration::from_secs(5)));
    let d = dfs.clone();
    let code = fork_and_wait(move || {
        let start = std::time::Instant::now();
        let ok = is_forked(d.metadata("/f")) && is_forked(d.close(Duration::from_secs(5)));
        drop(d);
        i32::from(!(ok && start.elapsed() < Duration::from_millis(500)))
    });
    pause.release();
    assert_eq!(t.join().unwrap().unwrap(), 3);
    assert_eq!(code, 0);
}

fn fork_before_connect_is_fine() {
    let code = fork_and_wait(|| {
        let mem = Arc::new(MemDfs::new());
        let dfs = Dfs::connect_mock(mem).unwrap();
        dfs.write("/x", b"1").unwrap();
        i32::from(dfs.read("/x").unwrap() != b"1")
    });
    assert_eq!(code, 0);
}

/// Every new type refuses use in a fork child, and drops there make no
/// backend call.
fn fork_refuses_libdaos_handles() {
    use daos::cont::ContainerOptions;
    use daos::obj::{ClassHints, ObjectClass, ObjectType, OpenMode};
    use daos::pool::PoolOptions;
    let mem = Arc::new(MemDfs::new());
    let pool = daos::Pool::connect_mock(mem.clone(), &PoolOptions::new("p")).unwrap();
    let c = daos::Container::open_mock(mem.clone(), &ContainerOptions::new("p", "c")).unwrap();
    let ids = c.oid_allocator();
    let new = |t| ids.next(t, ObjectClass::S1, ClassHints::default()).unwrap();
    let o = c
        .object(new(ObjectType::MultiHashed), OpenMode::ReadWrite)
        .unwrap();
    o.put_single(None, b"d", b"a", b"v").unwrap();
    let kv = c
        .kv(new(ObjectType::KvHashed), OpenMode::ReadWrite)
        .unwrap();
    let a = c.array_create(new(ObjectType::Array), 1, 64).unwrap();
    let it = o.dkeys().unwrap();
    let m = mem.clone();
    let code = fork_and_wait(move || {
        let ops = [
            "daos_obj_close",
            "daos_kv_close",
            "daos_array_close",
            "daos_cont_close",
            "daos_pool_disconnect",
            "daos_obj_fetch",
            "daos_tx_open",
        ];
        let before: Vec<u64> = ops.iter().map(|o| m.calls(o)).collect();
        let mut ok = true;
        ok &= is_forked(pool.query());
        ok &= is_forked(c.query());
        ok &= is_forked(c.tx(daos::tx::TxMode::ReadWrite));
        ok &= is_forked(o.get_single_vec(None, b"d", b"a"));
        ok &= is_forked(kv.get(None, "k"));
        ok &= is_forked(a.size(None));
        // Empty requests are checked before the no-op.
        ok &= is_forked(a.read_at(None, 0, &mut []));
        ok &= is_forked(a.write_at(None, 0, &[]));
        ok &= is_forked(a.punch(None, &[]));
        ok &= is_forked(o.punch_dkeys(None, &[]));
        ok &= is_forked(o.punch_akeys(None, b"d", &[]));
        ok &= is_forked(c.close(Duration::from_secs(1)));
        ok &= is_forked(pool.close(Duration::from_secs(1)));
        ok &= is_forked(daos::Pool::connect_mock(m.clone(), &PoolOptions::new("p")));
        let mut it = it;
        ok &= is_forked(it.next().unwrap());
        drop((it, a, kv, o, c, pool));
        let after: Vec<u64> = ops.iter().map(|o| m.calls(o)).collect();
        ok &= before == after;
        i32::from(!ok)
    });
    assert_eq!(code, 0);
}

/// A child forked while parent threads hold the registry lock (a connect in
/// progress) and the allocator's range lock (inside `daos_cont_alloc_oids`)
/// gets `Forked` from `destroy_container` and `OidAllocator::next` at once.
fn fork_child_does_not_hang_on_registry_or_allocator_locks() {
    use daos::cont::ContainerOptions;
    use daos::obj::{ClassHints, ObjectClass, ObjectType};
    use daos::pool::PoolOptions;
    let mem = Arc::new(MemDfs::new());
    let pool = daos::Pool::connect_mock(mem.clone(), &PoolOptions::new("p")).unwrap();
    let c = daos::Container::open_mock(mem.clone(), &ContainerOptions::new("p", "c")).unwrap();
    let ids = Arc::new(c.oid_allocator());
    let next = |ids: &daos::obj::OidAllocator| {
        ids.next(
            ObjectType::MultiHashed,
            ObjectClass::S1,
            ClassHints::default(),
        )
    };
    let pause = mem.pause_next("daos_cont_alloc_oids");
    let ids2 = ids.clone();
    let t = std::thread::spawn(move || next(&ids2).map(|_| ()));
    assert!(pause.wait_entered(Duration::from_secs(5)));
    // This thread holds the registry lock across the fork.
    let hold = mem.hold_registry_lock();
    let code = fork_and_wait(|| {
        let start = std::time::Instant::now();
        let ok = is_forked(next(&ids)) && is_forked(pool.destroy_container("other", false));
        i32::from(!(ok && start.elapsed() < Duration::from_millis(500)))
    });
    drop(hold);
    pause.release();
    t.join().unwrap().unwrap();
    assert_eq!(code, 0);
    next(&ids).unwrap();
}

fn main() {
    let cases = [
        case(
            "fork_child_does_not_hang_on_registry_or_allocator_locks",
            fork_child_does_not_hang_on_registry_or_allocator_locks,
        ),
        case("fork_refuses_libdaos_handles", fork_refuses_libdaos_handles),
        case(
            "fork_refuses_inherited_handles",
            fork_refuses_inherited_handles,
        ),
        case(
            "fork_child_with_parent_thread_inside_a_call",
            fork_child_with_parent_thread_inside_a_call,
        ),
        case("fork_before_connect_is_fine", fork_before_connect_is_fine),
    ];
    run_main(&cases, Vec::new);
}
