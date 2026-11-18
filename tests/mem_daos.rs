// Copyright (c) 2026 Jihyeon Gim
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Pools, containers, objects, key-value objects, arrays and transactions
//! over the in-memory backend (feature `mock`). No DAOS needed.

use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

use daos::array::ArrayRange;
use daos::cont::{ContainerCreate, ContainerOptions, Layout, RedundancyLevel};
use daos::kv::KV_AKEY;
use daos::mock::MemDaos;
use daos::obj::{
    key_u64, u64_key, ClassHints, Cond, Extreme, KeyLevel, KeyQuery, ObjectClass, ObjectId,
    ObjectType, OpenMode, Recx,
};
use daos::pool::{Access, PoolOptions};
use daos::tx::{TxMode, TxOptions};
use daos::{ConnectOptions, Container, Dfs, Error, Pool};

const T: Duration = Duration::from_secs(5);

fn backend() -> Arc<MemDaos> {
    Arc::new(MemDaos::new())
}

fn cont(mem: &Arc<MemDaos>, name: &str) -> Container {
    Container::open_mock(mem.clone(), &ContainerOptions::new("pool", name)).unwrap()
}

fn oid(c: &Container, ty: ObjectType) -> ObjectId {
    c.oid_allocator()
        .next(ty, ObjectClass::UNKNOWN, ClassHints::default())
        .unwrap()
}

// -- pools and containers -------------------------------------------------------

#[test]
fn pool_query_and_targets() {
    let mem = backend();
    let pool = Pool::connect_mock(mem.clone(), &PoolOptions::new("pool")).unwrap();
    assert_eq!(pool.name(), "pool");
    assert!(!pool.uuid().is_nil());
    assert_eq!(pool.access(), Access::ReadWrite);
    let info = pool.query().unwrap();
    assert_eq!(info.uuid, pool.uuid());
    assert_eq!(info.targets, 8);
    assert!(info.space.nvme.total >= info.space.nvme.free);
    assert!(!info.rebuild.in_progress());
    let t = pool.query_target(0, 0).unwrap();
    assert_eq!(t.state, daos::pool::TargetState::UpIn);
    assert!(pool.query_target(0, 99).unwrap_err().is_not_found());
    assert_ne!(pool.as_raw(), 0);
    let e = Pool::connect_mock(mem.clone(), &PoolOptions::new("missing")).unwrap_err();
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
    assert_eq!(
        e.to_string(),
        "daos_pool_connect(\"missing\") failed: DER_NONEXIST(-1005): The specified entity does not exist"
    );
}

#[test]
fn container_create_routes_and_properties() {
    let mem = backend();
    let pool = Pool::connect_mock(mem.clone(), &PoolOptions::new("pool")).unwrap();
    // Four creation routes.
    let posix_l = pool
        .create_container(
            &ContainerCreate::new()
                .label("posix-l")
                .layout(Layout::Posix)
                .chunk_size(1 << 20)
                .redundancy_factor(0),
        )
        .unwrap();
    let posix_u = pool
        .create_container(&ContainerCreate::new().layout(Layout::Posix))
        .unwrap();
    let plain_l = pool
        .create_container(
            &ContainerCreate::new()
                .label("objs-l")
                .redundancy_factor(1)
                .redundancy_level(RedundancyLevel::Rank),
        )
        .unwrap();
    let plain_u = pool.create_container(&ContainerCreate::new()).unwrap();
    for (op, n) in [
        ("dfs_cont_create_with_label", 1),
        ("dfs_cont_create", 1),
        ("daos_cont_create_with_label", 1),
        ("daos_cont_create2", 1),
    ] {
        assert_eq!(mem.calls(op), n, "{op}");
    }
    assert_eq!(mem.open_container_handles(), 0, "creation does not open");
    // Duplicate label.
    let e = pool
        .create_container(&ContainerCreate::new().label("objs-l"))
        .unwrap_err();
    assert!(e.is_already_exists(), "{e}");
    // Invalid label and POSIX-only fields are refused before any call.
    let before = mem.calls("daos_cont_create_with_label");
    assert!(matches!(
        pool.create_container(&ContainerCreate::new().label("a b")),
        Err(Error::InvalidArgument(_))
    ));
    assert!(matches!(
        pool.create_container(&ContainerCreate::new().chunk_size(1)),
        Err(Error::InvalidArgument(_))
    ));
    assert_eq!(mem.calls("daos_cont_create_with_label"), before);
    // Open by label and by UUID string.
    let c = pool.open_container("objs-l").unwrap();
    assert_eq!(c.uuid(), plain_l);
    assert_eq!(c.name(), "objs-l");
    let by_uuid = pool.open_container(&plain_u.to_string()).unwrap();
    assert_eq!(by_uuid.uuid(), plain_u);
    let props = c.properties().unwrap();
    assert_eq!(props.label.as_deref(), Some("objs-l"));
    assert_eq!(props.layout, Some(Layout::Unknown));
    assert_eq!(props.redundancy_factor, Some(1));
    assert_eq!(props.redundancy_level, Some(1));
    assert_eq!(props.healthy, Some(true));
    assert_eq!(
        props.snapshot_max, None,
        "entries flagged as not set are skipped"
    );
    assert_eq!(mem.open_property_lists(), 0, "every query list is freed");
    let p = pool.open_container("posix-l").unwrap();
    assert_eq!(p.properties().unwrap().layout, Some(Layout::Posix));
    assert_eq!(p.uuid(), posix_l);
    // The unlabelled POSIX container mounts by UUID string.
    let dfs = pool
        .open_container(&posix_u.to_string())
        .unwrap()
        .mount_dfs()
        .unwrap();
    dfs.write("/x", b"1").unwrap();
    // List.
    let names: Vec<String> = pool
        .list_containers()
        .unwrap()
        .into_iter()
        .map(|e| e.label)
        .collect();
    assert!(names.contains(&"posix-l".to_string()));
    assert!(names.contains(&"objs-l".to_string()));
    // Destroy: refused while this process has it open, without a DAOS call.
    let destroys = mem.calls("daos_cont_destroy2");
    match pool.destroy_container("objs-l", true) {
        Err(Error::Busy {
            in_flight,
            open_connections,
            ..
        }) => assert_eq!((in_flight, open_connections), (0, 1)),
        other => panic!("{other:?}"),
    }
    assert_eq!(mem.calls("daos_cont_destroy2"), destroys);
    c.close(T).unwrap();
    drop(c);
    pool.destroy_container("objs-l", false).unwrap();
    assert!(pool.open_container("objs-l").unwrap_err().is_not_found());
    // Non-POSIX container: dfs_mount refuses, nothing stays open.
    let before = mem.open_container_handles();
    let e = pool.open_container(&plain_u.to_string()).unwrap();
    assert!(matches!(
        e.mount_dfs(),
        Err(Error::Dfs {
            op: "dfs_mount",
            ..
        })
    ));
    drop(e);
    drop(by_uuid);
    assert!(mem.open_container_handles() <= before);
}

#[test]
fn shared_connection_between_dfs_and_raw_handles() {
    let mem = backend();
    let dfs = Dfs::connect_mock_with(mem.clone(), &ConnectOptions::new("pool", "data")).unwrap();
    let c = cont(&mem, "data");
    let p = Pool::connect_mock(mem.clone(), &PoolOptions::new("pool")).unwrap();
    assert_eq!(mem.open_pool_handles(), 1);
    assert_eq!(mem.open_container_handles(), 1);
    assert_eq!(dfs.container_handle().unwrap().as_raw(), c.as_raw());
    assert_eq!(dfs.pool_handle().unwrap().as_raw(), p.as_raw());
    assert_eq!(c.query().unwrap().open_handles, 1);
    // The container handle of a mount and a container mount of the same key
    // are the same mount.
    let again = c.mount_dfs().unwrap();
    assert_eq!(mem.calls("dfs_mount"), 1);
    again.write("/shared", b"1").unwrap();
    assert_eq!(dfs.read("/shared").unwrap(), b"1");
    // Read-only: separate handles.
    let ro = Container::open_mock(
        mem.clone(),
        &ContainerOptions::new("pool", "data").access(Access::ReadOnly),
    )
    .unwrap();
    assert_eq!(mem.open_pool_handles(), 2);
    assert_eq!(c.query().unwrap().open_handles, 2);
    drop(ro);
    // Pool close refuses while a container is open.
    match p.close(Duration::from_millis(50)) {
        Err(Error::Busy {
            in_flight,
            open_connections,
            ..
        }) => assert_eq!((in_flight, open_connections), (0, 1)),
        other => panic!("{other:?}"),
    }
    // Container close unmounts the DFS mount of the container.
    c.close(T).unwrap();
    assert!(dfs.is_closed());
    assert!(matches!(dfs.read("/shared"), Err(Error::Closed)));
    assert!(matches!(dfs.container_handle(), Err(Error::Closed)));
    p.close(T).unwrap();
    assert!(p.is_closed());
    assert!(matches!(p.query(), Err(Error::Closed)));
    assert_eq!(mem.open_mounts(), 0);
    assert_eq!(mem.open_pool_handles(), 0);
}

#[test]
fn snapshots_unnamed() {
    let mem = backend();
    let c = cont(&mem, "snaps");
    let a = c.create_snapshot().unwrap();
    let b = c.create_snapshot().unwrap();
    assert!(b.epoch > a.epoch);
    let list = c.list_snapshots().unwrap();
    assert_eq!(list, vec![a, b]);
    assert_eq!(c.query().unwrap().snapshots, 2);
    c.destroy_snapshot(a.epoch).unwrap();
    assert_eq!(c.list_snapshots().unwrap(), vec![b]);
    assert!(c.destroy_snapshot(a.epoch).unwrap_err().is_not_found());
}

// -- objects ------------------------------------------------------------------

#[test]
fn single_values_probe_and_too_small() {
    let mem = backend();
    let c = cont(&mem, "objs");
    let o = c
        .object(oid(&c, ObjectType::MultiHashed), OpenMode::ReadWrite)
        .unwrap();
    assert_eq!(o.get_single_vec(None, b"d", b"a").unwrap(), None, "absent");
    assert_eq!(o.single_size(None, b"d", b"a").unwrap(), None);
    o.put_single(None, b"d", b"a", b"hello").unwrap();
    assert_eq!(o.single_size(None, b"d", b"a").unwrap(), Some(5));
    let mut big = [0u8; 16];
    assert_eq!(o.get_single(None, b"d", b"a", &mut big).unwrap(), Some(5));
    assert_eq!(&big[..5], b"hello");
    let mut small = [0u8; 2];
    match o.get_single(None, b"d", b"a", &mut small) {
        Err(Error::BufferTooSmall { needed, .. }) => assert_eq!(needed, 5),
        other => panic!("{other:?}"),
    }
    // Empty destination is a size probe.
    let fetches = mem.calls("daos_obj_fetch");
    match o.get_single(None, b"d", b"a", &mut []) {
        Err(Error::BufferTooSmall { needed, .. }) => assert_eq!(needed, 5),
        other => panic!("{other:?}"),
    }
    assert_eq!(o.get_single(None, b"d", b"zz", &mut []).unwrap(), None);
    assert_eq!(mem.calls("daos_obj_fetch") - fetches, 2);
    let big_value = vec![7u8; 1 << 20];
    o.put_single(None, b"d", b"big", &big_value).unwrap();
    assert_eq!(
        o.get_single_vec(None, b"d", b"big").unwrap().unwrap(),
        big_value
    );
    // Empty keys and empty values are refused before any call.
    let updates = mem.calls("daos_obj_update");
    assert!(matches!(
        o.put_single(None, b"", b"a", b"x"),
        Err(Error::InvalidArgument(_))
    ));
    assert!(matches!(
        o.put_single(None, b"d", b"a", b""),
        Err(Error::InvalidArgument(_))
    ));
    assert_eq!(mem.calls("daos_obj_update"), updates);
}

#[test]
fn multi_akey_records_and_conditions() {
    let mem = backend();
    let c = cont(&mem, "objs");
    let o = c
        .object(oid(&c, ObjectType::MultiHashed), OpenMode::ReadWrite)
        .unwrap();
    let data: Vec<u8> = (0..64u8).collect();
    let recxs = [Recx::new(0, 8), Recx::new(100, 8)];
    o.update(b"dk")
        .single(b"meta", b"m1")
        .records(b"rec", 4, &recxs, &data)
        .cond(Cond::DKEY_INSERT)
        .execute()
        .unwrap();
    // Insert again: DER_EXIST.
    let e = o
        .update(b"dk")
        .single(b"meta", b"m2")
        .cond(Cond::DKEY_INSERT)
        .execute()
        .unwrap_err();
    assert!(e.is_already_exists(), "{e}");
    let mut meta = [0u8; 8];
    let mut back = vec![0u8; 64];
    let out = o
        .fetch(b"dk")
        .single(b"meta", &mut meta)
        .records(b"rec", 4, &recxs, &mut back)
        .execute()
        .unwrap();
    assert_eq!(out.sizes(), &[2, 4]);
    assert_eq!(&meta[..2], b"m1");
    assert_eq!(back, data);
    // Holes keep the buffer's content.
    let mut hole = vec![0xAAu8; 16];
    o.read_records(None, b"dk", b"rec", 4, 50, &mut hole)
        .unwrap();
    assert!(hole.iter().all(|b| *b == 0xAA));
    // Conditional fetch of a missing dkey.
    let mut x = [0u8; 4];
    let e = o
        .fetch(b"none")
        .single(b"meta", &mut x)
        .cond(Cond::DKEY_FETCH)
        .execute()
        .unwrap_err();
    assert!(e.is_not_found());
    // Size mismatch is refused before the call.
    assert!(matches!(
        o.update(b"dk")
            .records(b"rec", 4, &recxs, &data[..63])
            .execute(),
        Err(Error::InvalidArgument(_))
    ));
    // Mixing empty and non-empty destinations is refused.
    let mut e1: [u8; 0] = [];
    let mut e2 = [0u8; 4];
    assert!(matches!(
        o.fetch(b"dk")
            .single(b"meta", &mut e1)
            .single(b"meta", &mut e2)
            .execute(),
        Err(Error::InvalidArgument(_))
    ));
    // Punch akeys, dkeys, object.
    o.punch_akeys(None, b"dk", &[b"meta"]).unwrap();
    assert_eq!(o.single_size(None, b"dk", b"meta").unwrap(), None);
    o.punch_dkeys(None, &[b"dk"]).unwrap();
    assert_eq!(o.dkeys().unwrap().count(), 0);
    o.put_single(None, b"a", b"b", b"c").unwrap();
    o.punch(None).unwrap();
    assert_eq!(o.dkeys().unwrap().count(), 0);
}

#[test]
fn integer_keys_from_odd_aligned_slices() {
    let mem = backend();
    let c = cont(&mem, "objs");
    let o = c
        .object(oid(&c, ObjectType::MultiUint64), OpenMode::ReadWrite)
        .unwrap();
    let mut storage = [0u8; 32];
    for k in 1..=5u64 {
        storage[1..9].copy_from_slice(&u64_key(k));
        storage[11..19].copy_from_slice(&u64_key(k * 10));
        // The backend asserts that integer keys arrive aligned.
        o.put_single(None, &storage[1..9], &storage[11..19], b"v")
            .unwrap();
        o.write_records(None, &storage[1..9], &u64_key(7), 1, k * 3, b"r")
            .unwrap();
    }
    storage[3..11].copy_from_slice(&u64_key(3));
    assert_eq!(
        o.get_single_vec(None, &storage[3..11], &u64_key(30))
            .unwrap(),
        Some(b"v".to_vec())
    );
    o.punch_akeys(None, &storage[3..11], &[&storage[11..19]])
        .unwrap();
    assert!(matches!(
        o.put_single(None, b"short", &u64_key(1), b"v"),
        Err(Error::InvalidArgument(_))
    ));
    let mut keys: Vec<u64> = o
        .dkeys()
        .unwrap()
        .map(|k| key_u64(&k.unwrap()).unwrap())
        .collect();
    keys.sort();
    assert_eq!(keys, [1, 2, 3, 4, 5]);
    assert_eq!(o.max_dkey_u64(None).unwrap(), Some(5));
    let r = o
        .query_key(None, KeyQuery::dkey(Extreme::Min).akey(KeyLevel::Find))
        .unwrap()
        .unwrap();
    assert_eq!(r.dkey, 1);
    assert_eq!(r.akey, Some(7));
    assert_eq!(o.max_recx(None, 5, 7).unwrap(), Some(Recx::new(15, 1)));
    // akeys of an odd-aligned integer dkey, with and without a transaction.
    storage[5..13].copy_from_slice(&u64_key(4));
    let mut akeys: Vec<u64> = o
        .akeys(&storage[5..13])
        .unwrap()
        .map(|k| key_u64(&k.unwrap()).unwrap())
        .collect();
    akeys.sort();
    assert_eq!(akeys, [7, 40]);
    let tx = c.tx(TxMode::ReadOnly).unwrap();
    assert_eq!(o.akeys_in(&tx, &storage[5..13]).unwrap().count(), 2);
    drop(tx);
    // punch_dkeys with odd-aligned keys.
    storage[13..21].copy_from_slice(&u64_key(2));
    storage[21..29].copy_from_slice(&u64_key(5));
    o.punch_dkeys(None, &[&storage[13..21], &storage[21..29]])
        .unwrap();
    let mut keys: Vec<u64> = o
        .dkeys()
        .unwrap()
        .map(|k| key_u64(&k.unwrap()).unwrap())
        .collect();
    keys.sort();
    assert_eq!(keys, [1, 3, 4]);
    // Hashed object types refuse query_key.
    let h = c
        .object(oid(&c, ObjectType::MultiHashed), OpenMode::ReadWrite)
        .unwrap();
    assert!(matches!(
        h.max_dkey_u64(None),
        Err(Error::InvalidArgument(_))
    ));
    let empty = c
        .object(oid(&c, ObjectType::DkeyUint64), OpenMode::ReadWrite)
        .unwrap();
    assert_eq!(empty.max_dkey_u64(None).unwrap(), None);
}

#[test]
fn enumeration_batches_growth_and_limits() {
    let mem = backend();
    let c = cont(&mem, "objs");
    let o = c
        .object(oid(&c, ObjectType::MultiHashed), OpenMode::ReadWrite)
        .unwrap();
    for i in 0..1000 {
        o.put_single(None, format!("key-{i:04}").as_bytes(), b"a", b"v")
            .unwrap();
    }
    let lists = mem.calls("daos_obj_list_dkey");
    let mut keys: Vec<Vec<u8>> = o
        .dkeys()
        .unwrap()
        .batch_size(7, 64)
        .map(Result::unwrap)
        .collect();
    keys.sort();
    assert_eq!(keys.len(), 1000);
    assert_eq!(keys[999], b"key-0999");
    assert!(mem.calls("daos_obj_list_dkey") - lists >= 143);
    // next_batch without per-key allocation.
    let mut it = o.dkeys().unwrap().batch_size(100, 4096);
    let mut n = 0;
    while let Some(batch) = it.next_batch().unwrap() {
        n += batch.count();
    }
    assert_eq!(n, 1000);
    // A key longer than the buffer: the buffer grows.
    let long = vec![b'x'; 5000];
    o.put_single(None, &long, b"a", b"v").unwrap();
    let found = o
        .dkeys()
        .unwrap()
        .batch_size(4, 16)
        .filter(|k| k.as_ref().unwrap().len() == 5000)
        .count();
    assert_eq!(found, 1);
    // Exact fit KEY2BIG once: grow and finish.
    mem.inject_key2big(1, None);
    assert_eq!(o.akeys(b"key-0001").unwrap().count(), 1);
    // KEY2BIG forever: stop after a fixed number of calls.
    mem.inject_key2big(1000, Some(1));
    let before = mem.calls("daos_obj_list_dkey");
    let r: Vec<_> = o.dkeys().unwrap().collect();
    assert_eq!(r.len(), 1);
    assert!(r[0].is_err());
    assert_eq!(mem.calls("daos_obj_list_dkey") - before, 9);
    mem.inject_key2big(0, None);
    // Limit of the key buffer.
    let r: Vec<_> = o
        .dkeys()
        .unwrap()
        .batch_size(4, 16)
        .max_key_buffer(1024)
        .collect();
    assert!(r
        .iter()
        .any(|k| matches!(k, Err(Error::BufferTooSmall { .. }))));
    // Erasure-code classes are not enumerable, before any call.
    let ec = c
        .generate_oid(
            1,
            0,
            ObjectType::MultiHashed,
            ObjectClass::EC_2P1G1,
            ClassHints::default(),
        )
        .unwrap();
    assert!(ec.is_erasure_coded());
    let eo = c.object(ec, OpenMode::ReadWrite).unwrap();
    let lists = mem.calls("daos_obj_list_dkey");
    assert!(matches!(eo.dkeys(), Err(Error::Unsupported { .. })));
    assert!(matches!(eo.akeys(b"d"), Err(Error::Unsupported { .. })));
    assert_eq!(mem.calls("daos_obj_list_dkey"), lists);
    let ekv = c
        .generate_oid(
            2,
            0,
            ObjectType::KvHashed,
            ObjectClass::EC_4P1GX,
            ClassHints::default(),
        )
        .unwrap();
    assert!(matches!(
        c.kv(ekv, OpenMode::ReadWrite).unwrap().keys(),
        Err(Error::Unsupported { .. })
    ));
    // Update and fetch on EC objects are offered.
    eo.put_single(None, b"d", b"a", b"v").unwrap();
}

#[test]
fn oid_generation_and_allocation() {
    let mem = backend();
    let c = cont(&mem, "objs");
    let first = c.alloc_oids(10).unwrap();
    let second = c.alloc_oids(1).unwrap();
    assert!(second >= first + 10);
    let a = c.oid_allocator().batch_size(2);
    let ids: Vec<ObjectId> = (0..5)
        .map(|_| {
            a.next(ObjectType::Array, ObjectClass::S1, ClassHints::default())
                .unwrap()
        })
        .collect();
    let mut lo: Vec<u64> = ids.iter().map(|i| i.lo).collect();
    lo.dedup();
    assert_eq!(lo.len(), 5);
    assert!(ids
        .iter()
        .all(|i| i.object_type() == Some(ObjectType::Array)));
    assert!(ids.iter().all(|i| i.class() == ObjectClass::S1));
    let g = c
        .generate_oid(
            5,
            0xABCD,
            ObjectType::KvHashed,
            ObjectClass::UNKNOWN,
            ClassHints::default(),
        )
        .unwrap();
    assert_eq!(g.lo, 5);
    assert_eq!(g.hi & 0xFFFF_FFFF, 0xABCD);
    assert_eq!(g.object_type(), Some(ObjectType::KvHashed));
}

// -- KV --------------------------------------------------------------------------

#[test]
fn kv_api() {
    let mem = backend();
    let c = cont(&mem, "kv");
    let kv = c
        .kv(oid(&c, ObjectType::KvHashed), OpenMode::ReadWrite)
        .unwrap();
    assert_eq!(kv.get(None, "a").unwrap(), None);
    kv.put(None, "a", b"1").unwrap();
    kv.put(None, "b", b"22").unwrap();
    assert_eq!(kv.get(None, "b").unwrap().as_deref(), Some(&b"22"[..]));
    assert_eq!(kv.size(None, "b").unwrap(), Some(2));
    assert!(!kv.put_if_absent(None, "a", b"x").unwrap());
    assert!(kv.put_if_absent(None, "c", b"3").unwrap());
    let mut buf = [0u8; 1];
    match kv.get_into(None, "b", &mut buf) {
        Err(Error::BufferTooSmall { needed, .. }) => assert_eq!(needed, 2),
        other => panic!("{other:?}"),
    }
    match kv.get_into(None, "b", &mut []) {
        Err(Error::BufferTooSmall { needed, .. }) => assert_eq!(needed, 2),
        other => panic!("{other:?}"),
    }
    assert_eq!(kv.get_into(None, "zz", &mut []).unwrap(), None);
    let mut keys: Vec<String> = kv.keys().unwrap().map(Result::unwrap).collect();
    keys.sort();
    assert_eq!(keys, ["a", "b", "c"]);
    kv.remove(None, "a").unwrap();
    assert_eq!(kv.get(None, "a").unwrap(), None);
    assert!(matches!(
        kv.put(None, "", b"x"),
        Err(Error::InvalidArgument(_))
    ));
    assert!(matches!(
        kv.put(None, "a\0b", b"x"),
        Err(Error::InvalidArgument(_))
    ));
    assert!(matches!(
        kv.put(None, "e", b""),
        Err(Error::InvalidArgument(_))
    ));
    // KvUint64 goes through the object API with KV_AKEY.
    let u = oid(&c, ObjectType::KvUint64);
    assert!(matches!(
        c.kv(u, OpenMode::ReadWrite),
        Err(Error::InvalidArgument(_))
    ));
    let o = c.object(u, OpenMode::ReadWrite).unwrap();
    o.put_single(None, &u64_key(42), KV_AKEY, b"answer")
        .unwrap();
    assert_eq!(
        o.get_single_vec(None, &u64_key(42), KV_AKEY)
            .unwrap()
            .as_deref(),
        Some(&b"answer"[..])
    );
    // Destroy.
    kv.clone().destroy(None).unwrap();
    assert_eq!(kv.get(None, "b").unwrap(), None, "destroyed");
    // A multi-level object is not a KV.
    assert!(matches!(
        c.kv(oid(&c, ObjectType::MultiHashed), OpenMode::ReadWrite),
        Err(Error::InvalidArgument(_))
    ));
}

// -- arrays ----------------------------------------------------------------------

#[test]
fn arrays_per_type() {
    let mem = backend();
    let c = cont(&mem, "arr");
    // Ordinary array: counters are not interpreted, holes keep content.
    let id = oid(&c, ObjectType::Array);
    let a = c.array_create(id, 4, 16).unwrap();
    assert_eq!((a.cell_size(), a.chunk_size()), (4, 16));
    a.write_at(None, 0, &[1u8; 8]).unwrap();
    a.write_at(None, 10, &[2u8; 4]).unwrap();
    assert_eq!(a.size(None).unwrap(), 11);
    let mut buf = vec![0xEEu8; 44];
    let r = a.read_at(None, 0, &mut buf).unwrap();
    assert_eq!(r.counts(), None, "the backend wrote garbage counters");
    assert_eq!(&buf[..8], &[1u8; 8]);
    assert!(buf[8..40].iter().all(|b| *b == 0xEE), "hole untouched");
    assert_eq!(&buf[40..44], &[2u8; 4]);
    assert!(matches!(
        a.read_at_to_end(None, 0, &mut buf),
        Err(Error::InvalidArgument(_))
    ));
    let reopened = c.array_open(id, OpenMode::ReadOnly).unwrap();
    assert_eq!(reopened.cell_size(), 4);
    assert!(matches!(
        c.array_open(oid(&c, ObjectType::ArrayByte), OpenMode::ReadOnly),
        Err(Error::InvalidArgument(_))
    ));
    // Zero-cell requests make no call.
    let calls = mem.calls("daos_array_read")
        + mem.calls("daos_array_write")
        + mem.calls("daos_array_punch");
    a.write_at(None, 3, &[]).unwrap();
    assert_eq!(a.read_at(None, 3, &mut []).unwrap().counts(), None);
    a.write_ranges(None, &[], &[]).unwrap();
    a.read_ranges(None, &[ArrayRange::new(9, 0)], &mut [])
        .unwrap();
    a.punch(None, &[]).unwrap();
    assert_eq!(
        mem.calls("daos_array_read")
            + mem.calls("daos_array_write")
            + mem.calls("daos_array_punch"),
        calls
    );
    // Mixed zero and non-zero ranges.
    a.write_ranges(
        None,
        &[ArrayRange::new(5, 0), ArrayRange::new(2, 1)],
        &[9u8; 4],
    )
    .unwrap();
    assert!(matches!(
        a.write_at(None, 0, &[1u8; 3]),
        Err(Error::InvalidArgument(_))
    ));
    a.punch(None, &[ArrayRange::new(0, 2)]).unwrap();
    a.set_size(None, 3).unwrap();
    assert_eq!(a.stat(None).unwrap().size, 3);
    // Byte array: counters exact, holes zero-filled.
    let b = c
        .array_open_with_attr(
            oid(&c, ObjectType::ArrayByte),
            OpenMode::ReadWrite,
            1,
            1 << 20,
        )
        .unwrap();
    b.write_at(None, 0, b"ab").unwrap();
    b.write_at(None, 6, b"z").unwrap();
    let mut buf = [0xEEu8; 10];
    let r = b.read_at(None, 0, &mut buf).unwrap();
    let counts = r.counts().unwrap();
    assert_eq!(counts.cells_read, 7);
    assert_eq!(counts.short_read_cells, 3);
    assert_eq!(&buf[..7], b"ab\0\0\0\0z");
    assert_eq!(b.read_at_to_end(None, 0, &mut buf).unwrap(), 7);
    // A read-only handle cannot destroy; the read-write one can.
    assert!(reopened
        .clone()
        .destroy(None)
        .unwrap_err()
        .is_permission_denied());
    reopened.close().unwrap();
    a.destroy(None).unwrap();
}

// -- transactions -----------------------------------------------------------------

#[test]
fn transactions_commit_abort_restart() {
    let mem = backend();
    let c = cont(&mem, "tx");
    let kv = c
        .kv(oid(&c, ObjectType::KvHashed), OpenMode::ReadWrite)
        .unwrap();
    // Commit makes it visible; abort does not.
    let mut tx = c.tx(TxMode::ReadWrite).unwrap();
    kv.put(Some(&tx), "k", b"1").unwrap();
    assert_eq!(
        kv.get(None, "k").unwrap(),
        None,
        "not visible before commit"
    );
    tx.commit().unwrap();
    tx.close().unwrap();
    assert_eq!(kv.get(None, "k").unwrap().as_deref(), Some(&b"1"[..]));
    let mut tx = c.tx(TxMode::ReadWrite).unwrap();
    kv.put(Some(&tx), "k", b"2").unwrap();
    tx.abort().unwrap();
    assert!(matches!(
        kv.put(Some(&tx), "k", b"3"),
        Err(Error::InvalidArgument(_))
    ));
    drop(tx);
    assert_eq!(kv.get(None, "k").unwrap().as_deref(), Some(&b"1"[..]));
    // Closure API with restarts.
    mem.fail_nth("daos_tx_commit", 0, -2025, 2);
    let mut runs = 0;
    c.transaction(TxMode::ReadWrite, |tx| {
        runs += 1;
        kv.put(Some(tx), "k", b"9")
    })
    .unwrap();
    assert_eq!(runs, 3);
    assert_eq!(mem.calls("daos_tx_restart"), 2);
    assert_eq!(kv.get(None, "k").unwrap().as_deref(), Some(&b"9"[..]));
    // Bounded restarts return the restart error.
    mem.fail_nth("daos_tx_commit", 0, -2025, 5);
    let e = c
        .transaction_with(TxMode::ReadWrite, &TxOptions::new().max_restarts(1), |tx| {
            kv.put(Some(tx), "k", b"0")
        })
        .unwrap_err();
    assert!(e.is_tx_restart(), "{e}");
    mem.clear_faults();
    // Other errors abort and are returned.
    let e = c
        .transaction(TxMode::ReadWrite, |_| -> daos::Result<()> {
            Err(Error::InvalidArgument("stop"))
        })
        .unwrap_err();
    assert!(matches!(e, Error::InvalidArgument("stop")));
    assert_eq!(mem.open_transactions(), 0);
    // Read-only tx refuses writes before the call.
    let ro = c.tx(TxMode::ReadOnly).unwrap();
    assert!(matches!(
        kv.put(Some(&ro), "k", b"x"),
        Err(Error::InvalidArgument(_))
    ));
    drop(ro);
    // A tx of another container connection is refused.
    let other = cont(&mem, "tx-other");
    let tx2 = other.tx(TxMode::ReadWrite).unwrap();
    assert!(matches!(
        kv.put(Some(&tx2), "k", b"x"),
        Err(Error::InvalidArgument(_))
    ));
    drop(tx2);
    // Iteration inside a transaction.
    let tx = c.tx(TxMode::ReadOnly).unwrap();
    assert_eq!(kv.keys_in(&tx).unwrap().count(), 1);
    drop(tx);
}

#[test]
fn snapshot_transaction_reads_old_value() {
    let mem = backend();
    let c = cont(&mem, "snap");
    let o = c
        .object(oid(&c, ObjectType::MultiHashed), OpenMode::ReadWrite)
        .unwrap();
    o.put_single(None, b"d", b"a", b"old").unwrap();
    let s = c.create_snapshot().unwrap();
    o.put_single(None, b"d", b"a", b"new").unwrap();
    let tx = c.snapshot_tx(s.epoch).unwrap();
    assert!(tx.is_read_only());
    assert_eq!(
        o.get_single_vec(Some(&tx), b"d", b"a").unwrap().as_deref(),
        Some(&b"old"[..])
    );
    assert!(matches!(
        o.put_single(Some(&tx), b"d", b"a", b"x"),
        Err(Error::InvalidArgument(_))
    ));
    assert_eq!(o.dkeys_in(&tx).unwrap().count(), 1);
    tx.close().unwrap();
    assert_eq!(
        o.get_single_vec(None, b"d", b"a").unwrap().as_deref(),
        Some(&b"new"[..])
    );
}

#[test]
fn container_close_with_live_transaction_is_busy() {
    let mem = backend();
    let c = cont(&mem, "busy");
    let (to_main, from_tx) = mpsc::channel();
    let (to_tx, from_main) = mpsc::channel::<()>();
    let c2 = c.clone();
    let t = thread::spawn(move || {
        let tx = c2.tx(TxMode::ReadWrite).unwrap();
        to_main.send(()).unwrap();
        from_main.recv().unwrap();
        tx.close().unwrap();
    });
    from_tx.recv().unwrap();
    let aborts = mem.calls("daos_tx_abort") + mem.calls("daos_tx_close");
    match c.close(Duration::from_millis(100)) {
        Err(Error::Busy { transactions, .. }) => assert_eq!(transactions, 1),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        mem.calls("daos_tx_abort") + mem.calls("daos_tx_close"),
        aborts,
        "the closing thread makes no tx call"
    );
    assert!(!c.is_closed());
    to_tx.send(()).unwrap();
    t.join().unwrap();
    c.close(T).unwrap();
}

#[test]
fn container_close_closes_every_handle() {
    let mem = backend();
    let c = cont(&mem, "close");
    let o = c
        .object(oid(&c, ObjectType::MultiHashed), OpenMode::ReadWrite)
        .unwrap();
    let kv = c
        .kv(oid(&c, ObjectType::KvHashed), OpenMode::ReadWrite)
        .unwrap();
    let a = c.array_create(oid(&c, ObjectType::Array), 1, 1024).unwrap();
    let it = o.dkeys().unwrap();
    assert_eq!(mem.open_object_handles(), 3);
    c.close(T).unwrap();
    assert_eq!(mem.open_object_handles(), 0);
    assert_eq!(mem.open_container_handles(), 0);
    assert!(matches!(
        o.put_single(None, b"d", b"a", b"v"),
        Err(Error::Closed)
    ));
    assert!(matches!(kv.get(None, "k"), Err(Error::Closed)));
    assert!(matches!(a.size(None), Err(Error::Closed)));
    let closes = mem.calls("daos_obj_close");
    drop((o, kv, a, it));
    assert_eq!(
        mem.calls("daos_obj_close"),
        closes,
        "drops after close make no call"
    );
    assert!(matches!(
        c.object(ObjectId::new(1, 0), OpenMode::ReadOnly),
        Err(Error::Closed)
    ));
}

#[test]
fn objects_shared_between_threads() {
    let mem = backend();
    let c = cont(&mem, "mt");
    let o = c
        .object(oid(&c, ObjectType::MultiHashed), OpenMode::ReadWrite)
        .unwrap();
    let hs: Vec<_> = (0..8u32)
        .map(|t| {
            let o = o.clone();
            thread::spawn(move || {
                for i in 0..50u32 {
                    let d = format!("t{t}-{i}");
                    o.put_single(None, d.as_bytes(), b"a", &i.to_le_bytes())
                        .unwrap();
                    let v = o.get_single_vec(None, d.as_bytes(), b"a").unwrap().unwrap();
                    assert_eq!(v, i.to_le_bytes());
                }
            })
        })
        .collect();
    for h in hs {
        h.join().unwrap();
    }
    assert_eq!(o.dkeys().unwrap().count(), 400);
    o.close().unwrap();
    assert_eq!(mem.open_object_handles(), 0);
}

#[test]
fn missing_group_is_reported_at_first_use_only_by_the_real_loader() {
    // The in-memory backend has every group; the lazy MissingSymbol path is
    // covered by tests/dlopen_mock.rs with a library that lacks daos_kv_*.
    let mem = backend();
    let c = cont(&mem, "g");
    assert!(c
        .kv(oid(&c, ObjectType::KvHashed), OpenMode::ReadWrite)
        .is_ok());
}

// -- close, fork and argument checks ----------------------------------------------

/// `Container::close` while a DFS call of its mount runs on another thread:
/// `Busy`, and nothing changed (no object, mount or container close); a
/// second close while still busy is `Busy` again; once the call ends a close
/// closes everything.
#[test]
fn container_close_with_dfs_call_in_flight_is_busy() {
    let mem = backend();
    let c = cont(&mem, "inflight");
    let dfs = c.mount_dfs().unwrap();
    dfs.write("/f", b"data").unwrap();
    let o = c
        .object(oid(&c, ObjectType::MultiHashed), OpenMode::ReadWrite)
        .unwrap();
    o.put_single(None, b"d", b"a", b"v").unwrap();
    let pause = mem.pause_next("dfs_get_size");
    let f = dfs.open("/f").unwrap();
    let t = thread::spawn(move || f.size());
    assert!(pause.wait_entered(T));
    let ops = [
        "daos_cont_close",
        "dfs_umount",
        "daos_obj_close",
        "dfs_release",
    ];
    let before: Vec<u64> = ops.iter().map(|op| mem.calls(op)).collect();
    let start = Instant::now();
    match c.close(Duration::from_millis(100)) {
        Err(Error::Busy { in_flight, .. }) => assert_eq!(in_flight, 1),
        other => panic!("{other:?}"),
    }
    // One wait, not one for the container and another for the mount.
    assert!(
        start.elapsed() < Duration::from_millis(190),
        "{:?}",
        start.elapsed()
    );
    let after: Vec<u64> = ops.iter().map(|op| mem.calls(op)).collect();
    assert_eq!(before, after, "a Busy close changes nothing");
    assert!(!c.is_closed());
    assert!(!dfs.is_closed());
    assert_eq!(
        o.get_single_vec(None, b"d", b"a").unwrap().as_deref(),
        Some(&b"v"[..])
    );
    assert!(dfs.metadata("/f").unwrap().is_file());
    assert!(matches!(
        c.close(Duration::from_millis(20)),
        Err(Error::Busy { .. })
    ));
    pause.release();
    assert_eq!(t.join().unwrap().unwrap(), 4);
    c.close(T).unwrap();
    assert!(c.is_closed());
    assert!(dfs.is_closed());
    assert!(matches!(
        o.get_single_vec(None, b"d", b"a"),
        Err(Error::Closed)
    ));
    assert!(matches!(dfs.metadata("/f"), Err(Error::Closed)));
    assert_eq!(mem.open_mounts(), 0);
    assert_eq!(mem.open_object_handles(), 0);
    assert_eq!(mem.open_container_handles(), 0);
    assert_eq!(mem.open_pool_handles(), 0);
    c.close(T).unwrap(); // idempotent
}

/// A mount made while a container close waits is listed before the close
/// proceeds, so the close unmounts it.
#[test]
fn mount_during_container_close_is_unmounted() {
    let mem = backend();
    let c = cont(&mem, "mnt-close");
    let pause = mem.pause_next("dfs_mount");
    let c2 = c.clone();
    let m = thread::spawn(move || c2.mount_dfs());
    assert!(pause.wait_entered(T));
    let c3 = c.clone();
    let closer = thread::spawn(move || c3.close(T));
    thread::sleep(Duration::from_millis(50));
    pause.release();
    let dfs = m.join().unwrap().unwrap();
    closer.join().unwrap().unwrap();
    assert!(dfs.is_closed());
    assert_eq!(mem.open_mounts(), 0);
    assert_eq!(mem.open_container_handles(), 0);
}

/// `Dfs::connect` while a close of the shared container connection waits:
/// it waits for the outcome; after `Busy` it mounts on the live connection.
#[test]
fn dfs_connect_during_pending_container_close() {
    let mem = backend();
    let c = cont(&mem, "pending");
    let (to_main, from_tx) = mpsc::channel();
    let (to_tx, from_main) = mpsc::channel::<()>();
    let c2 = c.clone();
    let t = thread::spawn(move || {
        let tx = c2.tx(TxMode::ReadWrite).unwrap();
        to_main.send(()).unwrap();
        from_main.recv().unwrap();
        tx.close().unwrap();
    });
    from_tx.recv().unwrap();
    let c3 = c.clone();
    let closer = thread::spawn(move || c3.close(Duration::from_millis(300)));
    thread::sleep(Duration::from_millis(50));
    let dfs = Dfs::connect_mock_with(mem.clone(), &ConnectOptions::new("pool", "pending")).unwrap();
    assert!(matches!(
        closer.join().unwrap(),
        Err(Error::Busy {
            transactions: 1,
            ..
        })
    ));
    dfs.write("/x", b"1").unwrap();
    assert_eq!(dfs.container_handle().unwrap().as_raw(), c.as_raw());
    to_tx.send(()).unwrap();
    t.join().unwrap();
    c.close(T).unwrap();
    assert!(dfs.is_closed());
    assert_eq!(mem.open_container_handles(), 0);
}

/// libdaos state rules (abort only while open, restart only after a
/// restart error, close always): `close` and `abort` after a restart error
/// or a failed commit succeed without a `daos_tx_abort` call.
#[test]
fn transaction_state_rules() {
    let mem = backend();
    let c = cont(&mem, "txstate");
    let kv = c
        .kv(oid(&c, ObjectType::KvHashed), OpenMode::ReadWrite)
        .unwrap();
    // Restart error from the commit, then close.
    mem.fail_next("daos_tx_commit", -2025);
    let mut tx = c.tx(TxMode::ReadWrite).unwrap();
    kv.put(Some(&tx), "a", b"1").unwrap();
    assert!(tx.commit().unwrap_err().is_tx_restart());
    assert!(tx.needs_restart());
    let aborts = mem.calls("daos_tx_abort");
    tx.close().unwrap();
    assert_eq!(mem.calls("daos_tx_abort"), aborts);
    // Restart error from a data call, then abort: no call, no error.
    let mut tx = c.tx(TxMode::ReadWrite).unwrap();
    mem.fail_next("daos_kv_put", -2025);
    assert!(kv.put(Some(&tx), "a", b"1").unwrap_err().is_tx_restart());
    tx.abort().unwrap();
    assert_eq!(mem.calls("daos_tx_abort"), aborts);
    assert!(matches!(tx.restart(), Err(Error::InvalidArgument(_))));
    assert!(matches!(
        kv.put(Some(&tx), "a", b"1"),
        Err(Error::InvalidArgument(_))
    ));
    tx.close().unwrap();
    // A commit that fails with another error: libdaos aborted it.
    mem.fail_next("daos_tx_commit", -2001);
    let mut tx = c.tx(TxMode::ReadWrite).unwrap();
    kv.put(Some(&tx), "b", b"2").unwrap();
    assert!(tx.commit().is_err());
    assert!(!tx.needs_restart());
    assert!(matches!(
        kv.put(Some(&tx), "b", b"2"),
        Err(Error::InvalidArgument(_))
    ));
    assert!(matches!(tx.restart(), Err(Error::InvalidArgument(_))));
    tx.close().unwrap();
    assert_eq!(mem.calls("daos_tx_abort"), aborts);
    assert_eq!(kv.get(None, "b").unwrap(), None);
    // Restart of an open transaction is refused before any call.
    let restarts = mem.calls("daos_tx_restart");
    let mut tx = c.tx(TxMode::ReadWrite).unwrap();
    assert!(matches!(tx.restart(), Err(Error::InvalidArgument(_))));
    assert_eq!(mem.calls("daos_tx_restart"), restarts);
    // Abort of an open transaction calls daos_tx_abort once.
    tx.abort().unwrap();
    tx.abort().unwrap();
    assert_eq!(mem.calls("daos_tx_abort"), aborts + 1);
    tx.close().unwrap();
    assert_eq!(mem.open_transactions(), 0);
}

#[test]
fn transaction_with_edge_cases() {
    let mem = backend();
    let c = cont(&mem, "txedge");
    let kv = c
        .kv(oid(&c, ObjectType::KvHashed), OpenMode::ReadWrite)
        .unwrap();
    // The closure swallows a restart error of its transaction: restart.
    mem.fail_next("daos_kv_put", -2025);
    let mut runs = 0;
    c.transaction(TxMode::ReadWrite, |tx| {
        runs += 1;
        let _ = kv.put(Some(tx), "s", b"1");
        Ok(())
    })
    .unwrap();
    assert_eq!(runs, 2);
    assert_eq!(kv.get(None, "s").unwrap().as_deref(), Some(&b"1"[..]));
    // ... and with the restart limit reached: a restart error.
    mem.fail_next("daos_kv_put", -2025);
    let e = c
        .transaction_with(TxMode::ReadWrite, &TxOptions::new().max_restarts(0), |tx| {
            let _ = kv.put(Some(tx), "s", b"2");
            Ok(())
        })
        .unwrap_err();
    assert!(e.is_tx_restart(), "{e}");
    // A restart error that did not come from this transaction is returned
    // as it is, without a restart.
    let restarts = mem.calls("daos_tx_restart");
    mem.fail_next("daos_kv_put", -2025);
    let e = c
        .transaction(TxMode::ReadWrite, |_tx| kv.put(None, "z", b"1"))
        .unwrap_err();
    assert!(e.is_tx_restart(), "{e}");
    assert_eq!(mem.calls("daos_tx_restart"), restarts);
    assert_eq!(mem.open_transactions(), 0);
}

/// Zero-cell array requests and empty punch lists go through the fork,
/// handle and transaction checks before the no-op.
#[test]
fn empty_requests_are_checked_first() {
    let mem = backend();
    let c = cont(&mem, "empty");
    let a = c.array_create(oid(&c, ObjectType::Array), 1, 64).unwrap();
    let o = c
        .object(oid(&c, ObjectType::MultiHashed), OpenMode::ReadWrite)
        .unwrap();
    let mut tx = c.tx(TxMode::ReadWrite).unwrap();
    tx.commit().unwrap();
    let not_open = |r: daos::Result<()>| matches!(r, Err(Error::InvalidArgument(_)));
    assert!(not_open(a.write_at(Some(&tx), 0, &[])));
    assert!(not_open(a.read_at(Some(&tx), 0, &mut []).map(|_| ())));
    assert!(not_open(a.write_ranges(Some(&tx), &[], &[])));
    assert!(not_open(a.punch(Some(&tx), &[])));
    assert!(not_open(o.punch_dkeys(Some(&tx), &[])));
    assert!(not_open(o.punch_akeys(Some(&tx), b"d", &[])));
    tx.close().unwrap();
    // Still no call for a valid empty request.
    a.write_at(None, 0, &[]).unwrap();
    o.punch_dkeys(None, &[]).unwrap();
    assert_eq!(mem.calls("daos_array_write"), 0);
    assert_eq!(mem.calls("daos_obj_punch_dkeys"), 0);
    c.close(T).unwrap();
    let closed = |r: daos::Result<()>| matches!(r, Err(Error::Closed));
    assert!(closed(a.write_at(None, 0, &[])));
    assert!(closed(a.read_at(None, 0, &mut []).map(|_| ())));
    assert!(closed(a.read_ranges(None, &[], &mut []).map(|_| ())));
    assert!(closed(a.punch(None, &[])));
    assert!(closed(o.punch_dkeys(None, &[])));
    assert!(closed(o.punch_akeys(None, b"d", &[])));
}

/// `batch_size` after the first key: the keys of the batch already fetched
/// are still returned, the new sizes apply from the next refill.
#[test]
fn batch_size_after_iteration_started() {
    let mem = backend();
    let c = cont(&mem, "batch");
    let o = c
        .object(oid(&c, ObjectType::MultiHashed), OpenMode::ReadWrite)
        .unwrap();
    let mut want: Vec<Vec<u8>> = (0..40)
        .map(|i| format!("key-{i:04}").into_bytes())
        .collect();
    for k in &want {
        o.put_single(None, k, b"a", b"v").unwrap();
    }
    let mut it = o.dkeys().unwrap().batch_size(10, 4096);
    let mut got: Vec<Vec<u8>> = it.by_ref().take(3).map(Result::unwrap).collect();
    let lists = mem.calls("daos_obj_list_dkey");
    assert_eq!(lists, 1);
    let it = it.batch_size(2, 64);
    got.extend(it.map(Result::unwrap));
    got.sort();
    want.sort();
    assert_eq!(got, want);
    // The 7 keys left of the first batch, then 30 keys at 2 per refill.
    assert!(mem.calls("daos_obj_list_dkey") - lists >= 15);
}

#[test]
fn object_class_names_through_the_container() {
    let mem = backend();
    let c = cont(&mem, "class");
    assert_eq!(c.class_from_name("S1").unwrap(), ObjectClass::S1);
    assert_eq!(c.class_name(ObjectClass::S1).unwrap(), "S1");
    assert!(matches!(
        c.class_from_name("NO_SUCH_CLASS"),
        Err(Error::InvalidArgument(_))
    ));
}

#[test]
fn kv_key_that_is_not_utf8() {
    let mem = backend();
    let c = cont(&mem, "utf8");
    let id = oid(&c, ObjectType::KvHashed);
    let kv = c.kv(id, OpenMode::ReadWrite).unwrap();
    kv.put(None, "ok", b"1").unwrap();
    let o = c.object(id, OpenMode::ReadWrite).unwrap();
    o.put_single(None, b"\xff\xfe", KV_AKEY, b"2").unwrap();
    let mut bad = Vec::new();
    let mut good = Vec::new();
    for k in kv.keys().unwrap() {
        match k {
            Ok(s) => good.push(s),
            Err(e @ Error::NotUtf8 { .. }) => {
                assert_eq!(e.kind(), std::io::ErrorKind::InvalidData);
                if let Error::NotUtf8 { bytes, .. } = e {
                    bad.push(bytes);
                }
            }
            Err(e) => panic!("{e}"),
        }
    }
    assert_eq!(good, ["ok"]);
    assert_eq!(bad, [b"\xff\xfe".to_vec()]);
    // The object API reads it.
    assert_eq!(
        o.get_single_vec(None, &bad[0], KV_AKEY).unwrap().as_deref(),
        Some(&b"2"[..])
    );
}
