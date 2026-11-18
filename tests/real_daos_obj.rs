// Copyright (c) 2026 Jihyeon Gim
// SPDX-License-Identifier: MIT OR Apache-2.0

//! The pool, container, object, key-value, array and transaction API against
//! a real DAOS system. Skipped unless `DAOS_POOL` is set (optional
//! `DAOS_SYS`); with `RUST_DAOS_REQUIRE_REAL=1` a missing variable is a
//! failure.
//!
//! The suite creates containers in `DAOS_POOL`, labelled
//! `rust-daos-test-<pid>-<case>`, and destroys them at the end of each case,
//! also when the case fails. The first case removes leftovers whose label
//! starts with `rust-daos-test-`; no other container is touched.
//!
//! ```text
//! CRT_MRC_ENABLE=0 DAOS_POOL=tank cargo test --test real_daos_obj
//! cargo test --test real_daos_obj -- --ignored     # also the multi-engine cases
//! ```

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use common::*;
use daos::array::ArrayRange;
use daos::cont::{ContainerCreate, Layout, RedundancyLevel};
use daos::kv::KV_AKEY;
use daos::obj::{
    key_u64, u64_key, ClassHints, Cond, Extreme, KeyLevel, KeyQuery, ObjectClass, ObjectId,
    ObjectType, OpenMode, Recx,
};
use daos::pool::{Access, PoolOptions};
use daos::tx::TxMode;
use daos::{Container, Error, InitOptions, Pool};

const PREFIX: &str = "rust-daos-test-";
const T: Duration = Duration::from_secs(60);

struct Env {
    pool: String,
    sys: Option<String>,
}

fn env() -> Env {
    match std::env::var("DAOS_POOL") {
        Ok(pool) => Env {
            pool,
            sys: std::env::var("DAOS_SYS").ok(),
        },
        Err(_) => {
            if std::env::var("RUST_DAOS_REQUIRE_REAL").as_deref() == Ok("1") {
                panic!("RUST_DAOS_REQUIRE_REAL=1 but DAOS_POOL is not set");
            }
            skip("DAOS_POOL is not set");
        }
    }
}

/// Init (first statement of every case, before threads) and connect.
fn setup() -> (Env, Pool) {
    let e = env();
    // SAFETY: called first in a single-threaded test subprocess.
    unsafe { daos::init(&InitOptions::default()) }.expect("daos::init");
    let pool = connect(&e, Access::ReadWrite);
    (e, pool)
}

fn connect(e: &Env, access: Access) -> Pool {
    let mut o = PoolOptions::new(&e.pool).access(access);
    if let Some(s) = &e.sys {
        o = o.sys_name(s);
    }
    Pool::connect_with(&o).expect("pool connect")
}

/// A test container that is destroyed when dropped (also on panic).
struct TestCont {
    pool: Pool,
    label: String,
}

impl TestCont {
    fn create(pool: &Pool, suffix: &str, spec: ContainerCreate) -> TestCont {
        let case = std::env::var("RUST_DAOS_CASE").unwrap_or_default();
        let label = format!("{PREFIX}{}-{case}{suffix}", std::process::id());
        pool.create_container(&spec.label(label.clone()))
            .unwrap_or_else(|e| panic!("create {label}: {e}"));
        TestCont {
            pool: pool.clone(),
            label,
        }
    }

    fn open(&self) -> Container {
        self.pool.open_container(&self.label).unwrap()
    }
}

impl Drop for TestCont {
    fn drop(&mut self) {
        if let Err(e) = self.pool.destroy_container(&self.label, true) {
            eprintln!("cleanup of {} failed: {e}", self.label);
        }
    }
}

fn oid(c: &Container, t: ObjectType, class: ObjectClass) -> ObjectId {
    c.oid_allocator()
        .next(t, class, ClassHints::default())
        .unwrap()
}

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u32).wrapping_mul(2_654_435_761).to_le_bytes()[2] ^ seed)
        .collect()
}

// ---------------------------------------------------------------------------

fn sweep_leftovers() {
    let (_e, pool) = setup();
    for c in pool.list_containers().unwrap() {
        if c.label.starts_with(PREFIX) {
            match pool.destroy_container(&c.label, true) {
                Ok(()) => println!("removed leftover {}", c.label),
                Err(e) => println!("could not remove {}: {e}", c.label),
            }
        }
    }
}

/// Every planned symbol resolves through the libdfs handle; which names the
/// loader used. With `RUST_DAOS_SYMBOLS_OUT=<file>` the lines are written
/// there too (abi/symbols-installed-<ver>.txt).
fn symbols() {
    let _ = env();
    let r = unsafe { daos::init(&InitOptions::default()) }.unwrap();
    println!("library: {:?}", r.library);
    assert!(
        r.library.plain_names.is_empty(),
        "{:?}",
        r.library.plain_names
    );
    assert!(
        r.library.missing_symbols.is_empty(),
        "{:?}",
        r.library.missing_symbols
    );
    let lib = unsafe {
        libloading::os::unix::Library::open(
            Some("libdfs.so"),
            libloading::os::unix::RTLD_NOW | libloading::os::unix::RTLD_LOCAL,
        )
    }
    .unwrap();
    let names = include_str!("../abi/symbols.txt");
    let mut out = String::new();
    let mut missing = Vec::new();
    for name in names
        .lines()
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
    {
        let found = unsafe { lib.get::<*mut libc::c_void>(name.as_bytes()) }.is_ok();
        out.push_str(&format!(
            "{name} {}\n",
            if found { "found" } else { "absent" }
        ));
        if !found {
            missing.push(name);
        }
    }
    print!("{out}");
    if let Ok(p) = std::env::var("RUST_DAOS_SYMBOLS_OUT") {
        std::fs::write(p, &out).unwrap();
    }
    assert!(missing.is_empty(), "absent: {missing:?}");
}

fn pool_query() {
    let (e, pool) = setup();
    let info = pool.query().unwrap();
    println!("pool info: {info:#?}");
    assert!(info.targets > 0);
    assert!(info.nodes > 0);
    assert!(info.space.scm.total > 0 || info.space.nvme.total > 0);
    if !pool.uuid().is_nil() {
        assert_eq!(info.uuid, pool.uuid());
    }
    let by_uuid = Pool::connect_with(&PoolOptions::new(info.uuid.to_string())).unwrap();
    assert_eq!(by_uuid.query().unwrap().uuid, info.uuid);
    let t = pool.query_target(0, 0).unwrap();
    println!("rank 0 target 0: {t:?}");
    let list = pool.list_containers().unwrap();
    println!("{} containers", list.len());
    if let Ok(c) = std::env::var("DAOS_CONT") {
        assert!(list.iter().any(|x| x.label == c), "{c} not listed");
    }
    let _ = e;
}

fn container_lifecycle() {
    let (_e, pool) = setup();
    let generic = TestCont::create(
        &pool,
        "-g",
        ContainerCreate::new()
            .redundancy_factor(0)
            .redundancy_level(RedundancyLevel::Rank),
    );
    let posix = TestCont::create(&pool, "-p", ContainerCreate::new().layout(Layout::Posix));
    // Unlabelled routes: destroyed by UUID string.
    let anon_g = pool.create_container(&ContainerCreate::new()).unwrap();
    let anon_p = pool
        .create_container(&ContainerCreate::new().layout(Layout::Posix))
        .unwrap();
    let c = generic.open();
    let info = c.query().unwrap();
    assert_eq!(info.uuid, c.uuid());
    assert_eq!(info.open_handles, 1);
    let by_uuid = pool.open_container(&c.uuid().to_string()).unwrap();
    assert_eq!(by_uuid.uuid(), c.uuid());
    let props = c.properties().unwrap();
    println!(
        "properties: {props:#?} checksum {:?}",
        props.checksum_name()
    );
    assert_eq!(props.label.as_deref(), Some(generic.label.as_str()));
    assert_eq!(props.layout, Some(Layout::Unknown));
    assert_eq!(props.redundancy_factor, Some(0));
    assert_eq!(props.redundancy_level, Some(1));
    let p = posix.open();
    assert_eq!(p.properties().unwrap().layout, Some(Layout::Posix));
    // Destroy with an open handle under the same strings: Busy, no call.
    assert!(matches!(
        pool.destroy_container(&generic.label, true),
        Err(Error::Busy { .. })
    ));
    // Under another string DAOS itself refuses without force.
    let e = pool
        .destroy_container(&c.uuid().to_string(), false)
        .unwrap_err();
    println!("destroy by uuid with an open handle: {e}");
    c.close(T).unwrap();
    by_uuid.close(T).unwrap();
    p.close(T).unwrap();
    let d = pool
        .open_container(&anon_p.to_string())
        .unwrap()
        .mount_dfs()
        .unwrap();
    d.write("/x", b"1").unwrap();
    d.close(T).unwrap();
    drop(d);
    pool.destroy_container(&anon_g.to_string(), false).unwrap();
    pool.destroy_container(&anon_p.to_string(), false).unwrap();
    assert!(pool
        .open_container(&anon_g.to_string())
        .unwrap_err()
        .is_not_found());
}

fn posix_container_mount_and_objects() {
    let (_e, pool) = setup();
    let tc = TestCont::create(&pool, "", ContainerCreate::new().layout(Layout::Posix));
    let c = tc.open();
    let dfs = c.mount_dfs().unwrap();
    dfs.write("/dir/f", b"dfs").unwrap();
    let names: Vec<String> = dfs
        .read_dir("/")
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(names, ["/dir"]);
    let h = dfs.container_handle().unwrap();
    assert_eq!(h.as_raw(), c.as_raw());
    let kv = h
        .kv(
            oid(&h, ObjectType::KvHashed, ObjectClass::S1),
            OpenMode::ReadWrite,
        )
        .unwrap();
    kv.put(None, "k", b"v").unwrap();
    assert_eq!(kv.get(None, "k").unwrap().as_deref(), Some(&b"v"[..]));
    let names: Vec<String> = dfs
        .read_dir("/")
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(names, ["/dir"], "DFS listing unaffected");
    drop(kv);
    c.close(T).unwrap();
    assert!(dfs.is_closed());
}

fn shared_connection_handles() {
    let (_e, pool) = setup();
    let tc = TestCont::create(&pool, "", ContainerCreate::new().layout(Layout::Posix));
    let c = tc.open();
    let dfs = c.mount_dfs().unwrap();
    let again = daos::Dfs::connect(pool.name(), &tc.label).unwrap();
    assert_eq!(again.container_handle().unwrap().as_raw(), c.as_raw());
    assert_eq!(
        c.query().unwrap().open_handles,
        1,
        "one handle for Dfs and Container"
    );
    let ro = daos::Container::open_with(
        &daos::cont::ContainerOptions::new(pool.name(), &tc.label).access(Access::ReadOnly),
    )
    .unwrap();
    assert_eq!(
        c.query().unwrap().open_handles,
        2,
        "read-only is its own handle"
    );
    ro.close(T).unwrap();
    dfs.close(T).unwrap();
    drop(again);
    c.close(T).unwrap();
}

fn objects() {
    let (_e, pool) = setup();
    let tc = TestCont::create(&pool, "", ContainerCreate::new());
    let c = tc.open();
    let o = c
        .object(
            oid(&c, ObjectType::MultiHashed, ObjectClass::UNKNOWN),
            OpenMode::ReadWrite,
        )
        .unwrap();
    println!(
        "UNKNOWN resolved to {:?} ({:?})",
        o.id().class(),
        o.id().class().name()
    );
    // Single values of several sizes.
    assert_eq!(o.get_single_vec(None, b"d", b"absent").unwrap(), None);
    for (akey, len) in [
        (&b"one"[..], 1usize),
        (b"mib", 1 << 20),
        (b"16mib", 16 << 20),
    ] {
        let v = pattern(len, len as u8);
        o.put_single(None, b"d", akey, &v).unwrap();
        assert_eq!(o.single_size(None, b"d", akey).unwrap(), Some(len as u64));
        assert!(o.get_single_vec(None, b"d", akey).unwrap().unwrap() == v);
    }
    let mut small = [0u8; 4];
    match o.get_single(None, b"d", b"mib", &mut small) {
        Err(Error::BufferTooSmall { needed, .. }) => {
            println!("too small: needed {needed}");
            assert!(needed == 1 << 20 || needed == 0);
        }
        other => panic!("{other:?}"),
    }
    // Records with several extents and several akeys in one call.
    let data = pattern(4 * 300, 3);
    let recxs = [
        Recx::new(0, 100),
        Recx::new(1000, 100),
        Recx::new(5000, 100),
    ];
    o.update(b"r")
        .single(b"meta", b"m")
        .records(b"rec", 4, &recxs, &data)
        .execute()
        .unwrap();
    let mut back = vec![0u8; data.len()];
    let mut meta = [0u8; 8];
    let out = o
        .fetch(b"r")
        .single(b"meta", &mut meta)
        .records(b"rec", 4, &recxs, &mut back)
        .execute()
        .unwrap();
    assert_eq!(out.sizes(), &[1, 4]);
    assert!(back == data);
    let mut hole = vec![0xAAu8; 40];
    o.read_records(None, b"r", b"rec", 4, 200, &mut hole)
        .unwrap();
    println!("hole bytes after fetch: {:?}", &hole[..8]);
    // Conditions.
    assert!(o
        .update(b"r")
        .single(b"meta", b"x")
        .cond(Cond::DKEY_INSERT)
        .execute()
        .unwrap_err()
        .is_already_exists());
    assert!(o
        .update(b"new")
        .single(b"meta", b"x")
        .cond(Cond::DKEY_UPDATE)
        .execute()
        .unwrap_err()
        .is_not_found());
    let mut x = [0u8; 2];
    assert!(o
        .fetch(b"none")
        .single(b"a", &mut x)
        .cond(Cond::DKEY_FETCH)
        .execute()
        .unwrap_err()
        .is_not_found());
    // Punch.
    o.punch_akeys(None, b"r", &[b"meta"]).unwrap();
    assert_eq!(o.single_size(None, b"r", b"meta").unwrap(), None);
    o.punch_dkeys(None, &[b"d"]).unwrap();
    let left: Vec<Vec<u8>> = o.dkeys().unwrap().map(Result::unwrap).collect();
    assert_eq!(left, vec![b"r".to_vec()]);
    o.punch(None).unwrap();
    assert_eq!(o.dkeys().unwrap().count(), 0);
}

/// Correctness of key enumeration on a populated object. Inserts run on
/// several threads sharing one `Object`, which also covers concurrent updates
/// on one handle. `RUST_DAOS_ENUM_KEYS` raises the key count (default 2,000;
/// the long variant used on the VM is 20,000). Each listing pass is timed.
fn enumeration() {
    const THREADS: usize = 8;
    let (_e, pool) = setup();
    let tc = TestCont::create(&pool, "", ContainerCreate::new());
    let c = tc.open();
    let n: usize = std::env::var("RUST_DAOS_ENUM_KEYS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2_000);
    // Formatted outside the timed region; zero padded, so already sorted.
    let want: Vec<Vec<u8>> = (0..n).map(|i| format!("k{i:07}").into_bytes()).collect();
    for class in [ObjectClass::S1, ObjectClass::SX] {
        let o = c
            .object(oid(&c, ObjectType::MultiHashed, class), OpenMode::ReadWrite)
            .unwrap();
        let t0 = Instant::now();
        thread::scope(|s| {
            for part in want.chunks(n.div_ceil(THREADS).max(1)) {
                let o = &o;
                s.spawn(move || {
                    for k in part {
                        o.put_single(None, k, b"a", b"v").unwrap();
                    }
                });
            }
        });
        println!(
            "{class:?}: {n} puts on {THREADS} threads in {:.2}s",
            t0.elapsed().as_secs_f64()
        );
        let t0 = Instant::now();
        let mut keys: Vec<Vec<u8>> = o
            .dkeys()
            .unwrap()
            .batch_size(97, 2048)
            .map(Result::unwrap)
            .collect();
        println!(
            "{class:?}: listing at 97 keys per call in {:.3}s",
            t0.elapsed().as_secs_f64()
        );
        keys.sort();
        assert!(keys == want, "set of keys for {class:?}");
        // A key longer than the batch buffer.
        let long = vec![b'L'; 10_000];
        o.put_single(None, &long, b"a", b"v").unwrap();
        let t0 = Instant::now();
        let found = o
            .dkeys()
            .unwrap()
            .batch_size(8, 256)
            .filter(|k| k.as_ref().unwrap().len() == 10_000)
            .count();
        println!(
            "{class:?}: listing at 8 keys and 256 bytes per call in {:.3}s",
            t0.elapsed().as_secs_f64()
        );
        assert_eq!(found, 1);
        o.put_single(None, b"akeys", b"a1", b"1").unwrap();
        o.put_single(None, b"akeys", b"a2", b"2").unwrap();
        let mut ak: Vec<Vec<u8>> = o.akeys(b"akeys").unwrap().map(Result::unwrap).collect();
        ak.sort();
        assert_eq!(ak, [b"a1".to_vec(), b"a2".to_vec()]);
        o.punch(None).unwrap();
    }
}

fn integer_keys_and_types() {
    let (_e, pool) = setup();
    let tc = TestCont::create(&pool, "", ContainerCreate::new());
    let c = tc.open();
    let o = c
        .object(
            oid(&c, ObjectType::MultiUint64, ObjectClass::S1),
            OpenMode::ReadWrite,
        )
        .unwrap();
    let mut storage = [0u8; 32];
    for k in [5u64, 1, 9, 3] {
        storage[1..9].copy_from_slice(&u64_key(k));
        storage[13..21].copy_from_slice(&u64_key(k + 100));
        o.put_single(None, &storage[1..9], &storage[13..21], b"v")
            .unwrap();
        o.write_records(None, &storage[1..9], &u64_key(7), 1, k * 10, b"r")
            .unwrap();
    }
    let mut ks: Vec<u64> = o
        .dkeys()
        .unwrap()
        .map(|k| key_u64(&k.unwrap()).unwrap())
        .collect();
    ks.sort();
    assert_eq!(ks, [1, 3, 5, 9]);
    assert_eq!(o.max_dkey_u64(None).unwrap(), Some(9));
    let r = o
        .query_key(None, KeyQuery::dkey(Extreme::Min).akey(KeyLevel::Find))
        .unwrap()
        .unwrap();
    println!("min dkey/akey: {r:?}");
    assert_eq!(r.dkey, 1);
    let rx = o.max_recx(None, 9, 7).unwrap().unwrap();
    println!("max recx of 9/7: {rx:?}");
    assert_eq!(rx.index + rx.count - 1, 90);
    // Every object type: open, write, read.
    for t in [
        ObjectType::MultiHashed,
        ObjectType::DkeyUint64,
        ObjectType::AkeyUint64,
        ObjectType::MultiUint64,
        ObjectType::DkeyLexical,
        ObjectType::AkeyLexical,
        ObjectType::MultiLexical,
        ObjectType::KvHashed,
        ObjectType::KvUint64,
        ObjectType::KvLexical,
    ] {
        let o = c
            .object(oid(&c, t, ObjectClass::S1), OpenMode::ReadWrite)
            .unwrap();
        let d = if t.dkey_is_u64() {
            u64_key(3).to_vec()
        } else {
            b"dk".to_vec()
        };
        let a = if t.akey_is_u64() {
            u64_key(4).to_vec()
        } else {
            b"ak".to_vec()
        };
        o.put_single(None, &d, &a, b"val").unwrap();
        assert_eq!(
            o.get_single_vec(None, &d, &a).unwrap().as_deref(),
            Some(&b"val"[..]),
            "{t:?}"
        );
        assert_eq!(o.dkeys().unwrap().count(), 1, "{t:?}");
    }
}

fn kv_and_arrays() {
    let (_e, pool) = setup();
    let tc = TestCont::create(&pool, "", ContainerCreate::new());
    let c = tc.open();
    let kv = c
        .kv(
            oid(&c, ObjectType::KvHashed, ObjectClass::S1),
            OpenMode::ReadWrite,
        )
        .unwrap();
    println!(
        "absent key: get={:?} size={:?}",
        kv.get(None, "nope"),
        kv.size(None, "nope")
    );
    println!("remove of an absent key: {:?}", kv.remove(None, "nope"));
    for i in 0..500 {
        kv.put(None, &format!("key{i}"), format!("value{i}").as_bytes())
            .unwrap();
    }
    assert_eq!(
        kv.get(None, "key7").unwrap().as_deref(),
        Some(&b"value7"[..])
    );
    assert!(!kv.put_if_absent(None, "key7", b"x").unwrap());
    assert!(kv.put_if_absent(None, "fresh", b"x").unwrap());
    let mut buf = [0u8; 3];
    assert!(matches!(
        kv.get_into(None, "key7", &mut buf),
        Err(Error::BufferTooSmall { .. })
    ));
    match kv.get_into(None, "key7", &mut []) {
        Err(Error::BufferTooSmall { needed, .. }) => assert_eq!(needed, 6),
        other => panic!("{other:?}"),
    }
    let mut keys: Vec<String> = kv
        .keys()
        .unwrap()
        .batch_size(50, 512)
        .map(Result::unwrap)
        .collect();
    keys.sort();
    assert_eq!(keys.len(), 501);
    kv.remove(None, "key7").unwrap();
    assert_eq!(kv.get(None, "key7").unwrap(), None);
    // Integer KV through the object API, the layout daos_kv_* uses.
    let ku = c
        .object(
            oid(&c, ObjectType::KvUint64, ObjectClass::S1),
            OpenMode::ReadWrite,
        )
        .unwrap();
    ku.put_single(None, &u64_key(42), KV_AKEY, b"answer")
        .unwrap();
    assert_eq!(
        ku.get_single_vec(None, &u64_key(42), KV_AKEY)
            .unwrap()
            .as_deref(),
        Some(&b"answer"[..])
    );

    // Ordinary array: holes keep the buffer content, no counters.
    let id = oid(&c, ObjectType::Array, ObjectClass::S1);
    let a = c.array_create(id, 4, 1024).unwrap();
    a.write_at(None, 0, &[1u8; 16]).unwrap();
    a.write_at(None, 100, &[2u8; 4]).unwrap();
    assert_eq!(a.size(None).unwrap(), 101);
    let mut b = vec![0xEEu8; 404];
    let r = a.read_at(None, 0, &mut b).unwrap();
    assert_eq!(r.counts(), None);
    assert_eq!(&b[..16], &[1u8; 16]);
    assert!(
        b[16..400].iter().all(|x| *x == 0xEE),
        "ordinary array hole untouched"
    );
    assert_eq!(&b[400..], &[2u8; 4]);
    let reopened = c.array_open(id, OpenMode::ReadOnly).unwrap();
    assert_eq!((reopened.cell_size(), reopened.chunk_size()), (4, 1024));
    a.punch(None, &[ArrayRange::new(0, 2)]).unwrap();
    a.set_size(None, 50).unwrap();
    assert_eq!(a.stat(None).unwrap().size, 50);
    // Byte array: counters and zero-filled holes.
    let bid = oid(&c, ObjectType::ArrayByte, ObjectClass::S1);
    let ba = c
        .array_open_with_attr(bid, OpenMode::ReadWrite, 1, 1 << 20)
        .unwrap();
    ba.write_at(None, 0, b"head").unwrap();
    ba.write_at(None, 3 << 20, b"tail").unwrap();
    let mut b = vec![0xEEu8; 64];
    let r = ba.read_at(None, (3 << 20) - 60, &mut b).unwrap();
    let counts = r.counts().unwrap();
    assert_eq!(counts.cells_read, 64);
    assert!(b[..60].iter().all(|x| *x == 0), "hole zero-filled");
    assert_eq!(&b[60..], b"tail");
    let mut past = vec![0xEEu8; 32];
    let r = ba.read_at(None, (3 << 20) + 2, &mut past).unwrap();
    println!("past the end: {:?}, buffer {:?}", r.counts(), &past[..8]);
    assert_eq!(r.counts().unwrap().cells_read, 2);
    assert_eq!(
        ba.read_at_to_end(None, (3 << 20) + 2, &mut past).unwrap(),
        2
    );
    // Zero cells make no call (checked in the mocks), and succeed here.
    ba.read_at(None, 0, &mut []).unwrap();
    ba.write_at(None, 0, &[]).unwrap();
    reopened.close().unwrap();
    a.destroy(None).unwrap();
}

fn transactions() {
    let (_e, pool) = setup();
    let tc = TestCont::create(&pool, "", ContainerCreate::new());
    let c = tc.open();
    let id = oid(&c, ObjectType::KvHashed, ObjectClass::S1);
    let kv = c.kv(id, OpenMode::ReadWrite).unwrap();
    let mut tx = c.tx(TxMode::ReadWrite).unwrap();
    kv.put(Some(&tx), "a", b"1").unwrap();
    tx.commit().unwrap();
    tx.close().unwrap();
    assert_eq!(kv.get(None, "a").unwrap().as_deref(), Some(&b"1"[..]));
    let mut tx = c.tx(TxMode::ReadWrite).unwrap();
    kv.put(Some(&tx), "b", b"2").unwrap();
    tx.abort().unwrap();
    drop(tx);
    assert_eq!(kv.get(None, "b").unwrap(), None);
    // Two threads increment one counter: conflicts restart and converge.
    kv.put(None, "counter", &0u64.to_le_bytes()).unwrap();
    let rounds = 20;
    let barrier = Arc::new(Barrier::new(2));
    let hs: Vec<_> = (0..2)
        .map(|_| {
            let c = c.clone();
            let kv = kv.clone();
            let barrier = barrier.clone();
            thread::spawn(move || {
                barrier.wait();
                let mut restarts = 0;
                for _ in 0..rounds {
                    c.transaction(TxMode::ReadWrite, |tx| {
                        let v = kv.get(Some(tx), "counter")?.unwrap();
                        let n = u64::from_le_bytes(v[..8].try_into().unwrap());
                        let r = kv.put(Some(tx), "counter", &(n + 1).to_le_bytes());
                        if matches!(&r, Err(e) if e.is_tx_restart()) {
                            restarts += 1;
                        }
                        r
                    })
                    .unwrap();
                }
                restarts
            })
        })
        .collect();
    let restarts: Vec<u32> = hs.into_iter().map(|h| h.join().unwrap()).collect();
    let v = kv.get(None, "counter").unwrap().unwrap();
    println!("restarts seen in the closures: {restarts:?}");
    assert_eq!(
        u64::from_le_bytes(v[..8].try_into().unwrap()),
        2 * rounds as u64
    );
    // The same by hand: a thread that sees a restart error closes its
    // transaction and opens a new one. libdaos refuses daos_tx_abort after
    // DER_TX_RESTART, so close must not report an error then.
    kv.put(None, "manual", &0u64.to_le_bytes()).unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let hs: Vec<_> = (0..2)
        .map(|_| {
            let c = c.clone();
            let kv = kv.clone();
            let barrier = barrier.clone();
            thread::spawn(move || {
                barrier.wait();
                let mut gave_up = 0u32;
                let mut done = 0;
                while done < rounds {
                    let mut tx = c.tx(TxMode::ReadWrite).unwrap();
                    let r = kv.get(Some(&tx), "manual").and_then(|v| {
                        let n = u64::from_le_bytes(v.unwrap()[..8].try_into().unwrap());
                        kv.put(Some(&tx), "manual", &(n + 1).to_le_bytes())?;
                        tx.commit()
                    });
                    match r {
                        Ok(()) => done += 1,
                        Err(e) if e.is_tx_restart() => {
                            assert!(tx.needs_restart());
                            gave_up += 1;
                        }
                        Err(e) => panic!("{e}"),
                    }
                    tx.close().unwrap();
                }
                gave_up
            })
        })
        .collect();
    let gave_up: Vec<u32> = hs.into_iter().map(|h| h.join().unwrap()).collect();
    println!("transactions closed after a restart error: {gave_up:?}");
    let v = kv.get(None, "manual").unwrap().unwrap();
    assert_eq!(
        u64::from_le_bytes(v[..8].try_into().unwrap()),
        2 * rounds as u64
    );
    // Snapshots and a snapshot transaction.
    kv.put(None, "s", b"old").unwrap();
    let snap = c.create_snapshot().unwrap();
    kv.put(None, "s", b"new").unwrap();
    let st = c.snapshot_tx(snap.epoch).unwrap();
    assert_eq!(
        kv.get(Some(&st), "s").unwrap().as_deref(),
        Some(&b"old"[..])
    );
    assert!(matches!(
        kv.put(Some(&st), "s", b"x"),
        Err(Error::InvalidArgument(_))
    ));
    st.close().unwrap();
    assert!(c.list_snapshots().unwrap().contains(&snap));
    c.destroy_snapshot(snap.epoch).unwrap();
    assert!(!c.list_snapshots().unwrap().contains(&snap));
    // Container::close from another thread while a transaction is open.
    let tx = c.tx(TxMode::ReadWrite).unwrap();
    let c2 = c.clone();
    let r = thread::spawn(move || c2.close(Duration::from_millis(200)))
        .join()
        .unwrap();
    assert!(
        matches!(
            r,
            Err(Error::Busy {
                transactions: 1,
                ..
            })
        ),
        "{r:?}"
    );
    tx.close().unwrap();
    c.close(T).unwrap();
}

fn threads_and_close() {
    let (_e, pool) = setup();
    let tc = TestCont::create(&pool, "", ContainerCreate::new());
    let c = tc.open();
    let o = c
        .object(
            oid(&c, ObjectType::MultiHashed, ObjectClass::S1),
            OpenMode::ReadWrite,
        )
        .unwrap();
    let kv = c
        .kv(
            oid(&c, ObjectType::KvHashed, ObjectClass::S1),
            OpenMode::ReadWrite,
        )
        .unwrap();
    let a = c
        .array_open_with_attr(
            oid(&c, ObjectType::ArrayByte, ObjectClass::S1),
            OpenMode::ReadWrite,
            1,
            1 << 16,
        )
        .unwrap();
    let hs: Vec<_> = (0..8u32)
        .map(|t| {
            let (o, kv, a) = (o.clone(), kv.clone(), a.clone());
            thread::spawn(move || {
                for i in 0..100u32 {
                    let k = format!("t{t}-{i}");
                    let v = pattern(1000 + i as usize, t as u8);
                    o.put_single(None, k.as_bytes(), b"a", &v).unwrap();
                    assert!(o.get_single_vec(None, k.as_bytes(), b"a").unwrap().unwrap() == v);
                    kv.put(None, &k, &v).unwrap();
                    assert!(kv.get(None, &k).unwrap().unwrap() == v);
                    let off = u64::from(t) * 100_000 + u64::from(i) * 1000;
                    a.write_at(None, off, &v[..1000]).unwrap();
                    let mut b = vec![0u8; 1000];
                    a.read_at(None, off, &mut b).unwrap();
                    assert!(b == v[..1000]);
                }
            })
        })
        .collect();
    for h in hs {
        h.join().unwrap();
    }
    // Close while workers run: workers see Closed, nobody crashes.
    let (tx, rx) = mpsc::channel();
    let workers: Vec<_> = (0..4u32)
        .map(|t| {
            let (o, tx) = (o.clone(), tx.clone());
            thread::spawn(move || loop {
                match o.put_single(None, format!("w{t}").as_bytes(), b"a", b"v") {
                    Ok(()) => {}
                    Err(Error::Closed) => {
                        tx.send(()).unwrap();
                        return;
                    }
                    Err(e) => panic!("{e}"),
                }
            })
        })
        .collect();
    thread::sleep(Duration::from_millis(200));
    c.close(T).unwrap();
    for w in workers {
        w.join().unwrap();
    }
    assert_eq!(rx.try_iter().count(), 4);
}

/// `Container::close` while DFS reads of its mount run on another thread:
/// each close is `Busy` (the container and its objects stay usable) or
/// closes everything; never a `daos_cont_close` error with the mount left
/// behind.
fn container_close_with_dfs_io() {
    let (_e, pool) = setup();
    let tc = TestCont::create(&pool, "", ContainerCreate::new().layout(Layout::Posix));
    let c = tc.open();
    let dfs = c.mount_dfs().unwrap();
    let data = pattern(8 << 20, 3);
    dfs.write("/big", &data).unwrap();
    let o = c
        .object(
            oid(&c, ObjectType::MultiHashed, ObjectClass::S1),
            OpenMode::ReadWrite,
        )
        .unwrap();
    o.put_single(None, b"d", b"a", b"v").unwrap();
    let started = Arc::new(Barrier::new(2));
    let stop = Arc::new(AtomicBool::new(false));
    let reader = {
        let (dfs, started, stop) = (dfs.clone(), started.clone(), stop.clone());
        thread::spawn(move || {
            let f = dfs.open("/big").unwrap();
            let mut buf = vec![0u8; 8 << 20];
            let mut reads = 0u64;
            started.wait();
            while !stop.load(Ordering::SeqCst) {
                match f.read_exact_at(&mut buf, 0) {
                    Ok(()) => reads += 1,
                    Err(Error::Closed) => break,
                    Err(e) => panic!("{e}"),
                }
            }
            reads
        })
    };
    started.wait();
    let mut busy = 0;
    let mut closed = false;
    for _ in 0..20 {
        match c.close(Duration::from_millis(1)) {
            Err(Error::Busy { .. }) => {
                busy += 1;
                assert!(!c.is_closed());
                assert!(!dfs.is_closed());
                assert_eq!(
                    o.get_single_vec(None, b"d", b"a").unwrap().as_deref(),
                    Some(&b"v"[..])
                );
            }
            Ok(()) => {
                closed = true;
                break;
            }
            Err(e) => panic!("close during DFS reads: {e}"),
        }
    }
    stop.store(true, Ordering::SeqCst);
    let reads = reader.join().unwrap();
    println!("Busy closes: {busy}, closed during reads: {closed}, reads: {reads}");
    c.close(T).unwrap();
    assert!(c.is_closed());
    assert!(dfs.is_closed());
    assert!(matches!(
        o.get_single_vec(None, b"d", b"a"),
        Err(Error::Closed)
    ));
    drop((o, dfs, c));
    // The old handle is really closed: a new open is the only one.
    let again = tc.open();
    assert_eq!(again.query().unwrap().open_handles, 1);
    again.close(T).unwrap();
}

/// MR cache stress on the object and array read paths (see real_daos.rs
/// mr_stress); run with and without CRT_MRC_ENABLE=0.
fn mr_stress_objects() {
    let (_e, pool) = setup();
    let tc = TestCont::create(&pool, "", ContainerCreate::new());
    let c = tc.open();
    let o = c
        .object(
            oid(&c, ObjectType::MultiHashed, ObjectClass::S1),
            OpenMode::ReadWrite,
        )
        .unwrap();
    let a = c
        .array_open_with_attr(
            oid(&c, ObjectType::ArrayByte, ObjectClass::S1),
            OpenMode::ReadWrite,
            1,
            1 << 20,
        )
        .unwrap();
    let size = 16 << 20;
    let data = pattern(size, 5);
    o.write_records(None, b"d", b"a", 1, 0, &data).unwrap();
    a.write_at(None, 0, &data).unwrap();
    let iters: usize = std::env::var("RUST_DAOS_MR_STRESS_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8);
    let hs: Vec<_> = (0..8)
        .map(|_| {
            let (o, a, check) = (o.clone(), a.clone(), data[..4096].to_vec());
            thread::spawn(move || {
                for _ in 0..iters {
                    let mut b = vec![0u8; size];
                    o.read_records(None, b"d", b"a", 1, 0, &mut b).unwrap();
                    assert!(b[..4096] == check[..]);
                    drop(b);
                    let mut b = vec![0u8; size];
                    a.read_at(None, 0, &mut b).unwrap();
                    assert!(b[..4096] == check[..]);
                }
            })
        })
        .collect();
    for h in hs {
        h.join().unwrap();
    }
}

/// A closed object cookie used through a raw call returns DER_NO_HDL: the
/// gate orders calls, it is not the only protection.
fn stale_handle() {
    let (_e, pool) = setup();
    let tc = TestCont::create(&pool, "", ContainerCreate::new());
    let c = tc.open();
    let o = c
        .object(
            oid(&c, ObjectType::MultiHashed, ObjectClass::S1),
            OpenMode::ReadWrite,
        )
        .unwrap();
    let cookie = o.as_raw();
    o.close().unwrap();
    let lib = unsafe {
        libloading::os::unix::Library::open(
            Some("libdfs.so"),
            libloading::os::unix::RTLD_NOW | libloading::os::unix::RTLD_LOCAL,
        )
    }
    .unwrap();
    // SAFETY: daos_obj_close(daos_handle_t, daos_event_t *); the handle is a
    // u64 cookie passed by value.
    let close: libloading::os::unix::Symbol<unsafe extern "C" fn(u64, *mut libc::c_void) -> i32> =
        unsafe { lib.get(b"daos_obj_close") }.unwrap();
    let rc = unsafe { close(cookie, std::ptr::null_mut()) };
    println!("daos_obj_close on a closed cookie: {rc}");
    assert_eq!(rc, -1002);
}

fn defaults_record() {
    let (_e, pool) = setup();
    let tc = TestCont::create(&pool, "", ContainerCreate::new());
    let c = tc.open();
    for t in [
        ObjectType::MultiHashed,
        ObjectType::Array,
        ObjectType::KvHashed,
    ] {
        let id = c
            .generate_oid(7, 0, t, ObjectClass::UNKNOWN, ClassHints::default())
            .unwrap();
        println!(
            "{t:?} with OC_UNKNOWN: {:?} {:?}",
            id.class(),
            id.class().name()
        );
    }
    let p = c.properties().unwrap();
    println!(
        "default container: checksum {:?} ({:?}), rf {:?}, level {:?}",
        p.checksum,
        p.checksum_name(),
        p.redundancy_factor,
        p.redundancy_level
    );
    let posix = TestCont::create(&pool, "-p", ContainerCreate::new().layout(Layout::Posix));
    let pp = posix.open().properties().unwrap();
    println!("default POSIX container: {pp:?}");
}

// -- multi-engine cases (run with --ignored) ---------------------------------------
//
// Every container here uses redundancy at rank level: on a single host the
// default level (node) has one fault domain. A case whose class needs more
// engines than the pool spans is skipped with the reason, not passed.

/// Skip the case unless the pool spans at least `n` engines.
fn require_engines(pool: &Pool, n: u32, what: &str) {
    let have = pool.query().unwrap().nodes;
    if have < n {
        skip(&format!("{what} needs {n} engines, the pool has {have}"));
    }
}

/// A container with `rd_fac` 1 at rank level.
fn rank_level_container(pool: &Pool) -> TestCont {
    TestCont::create(
        pool,
        "",
        ContainerCreate::new()
            .redundancy_factor(1)
            .redundancy_level(RedundancyLevel::Rank),
    )
}

/// Two replicas (2 engines): update, fetch, punch, enumeration, KV, array.
fn multi_engine_rp2_objects() {
    replicated(2, &[ObjectClass::RP_2G1, ObjectClass::RP_2GX]);
}

/// Three replicas (3 engines).
fn multi_engine_rp3_objects() {
    replicated(3, &[ObjectClass::RP_3G1, ObjectClass::RP_3GX]);
}

fn replicated(width: u32, classes: &[ObjectClass]) {
    let (_e, pool) = setup();
    require_engines(&pool, width, &format!("{classes:?}"));
    let tc = rank_level_container(&pool);
    let c = tc.open();
    for &class in classes {
        let o = c
            .object(oid(&c, ObjectType::MultiHashed, class), OpenMode::ReadWrite)
            .unwrap();
        // "GX" classes come back with the concrete group count.
        println!("{class:?} -> {:?}", o.id().class());
        assert_eq!(o.id().class().id() >> 24, class.id() >> 24);
        for i in 0..100 {
            o.put_single(
                None,
                format!("k{i}").as_bytes(),
                b"a",
                &pattern(4096, i as u8),
            )
            .unwrap();
        }
        assert!(o.get_single_vec(None, b"k7", b"a").unwrap().unwrap() == pattern(4096, 7));
        assert_eq!(o.dkeys().unwrap().count(), 100, "{class:?}");
        o.punch_dkeys(None, &[b"k1"]).unwrap();
        assert_eq!(o.dkeys().unwrap().count(), 99, "{class:?}");
        o.punch(None).unwrap();
        let kv = c
            .kv(oid(&c, ObjectType::KvHashed, class), OpenMode::ReadWrite)
            .unwrap();
        kv.put(None, "k", b"v").unwrap();
        assert_eq!(kv.keys().unwrap().count(), 1);
        let a = c
            .array_open_with_attr(
                oid(&c, ObjectType::ArrayByte, class),
                OpenMode::ReadWrite,
                1,
                1 << 20,
            )
            .unwrap();
        let v = pattern(3 << 20, 9);
        a.write_at(None, 0, &v).unwrap();
        let mut b = vec![0u8; v.len()];
        a.read_at(None, 0, &mut b).unwrap();
        assert!(b == v, "{class:?}");
    }
}

/// `EC_2P1G1`: 3 engines (2 data, 1 parity).
fn multi_engine_ec_objects_ec_2p1() {
    ec_objects(ObjectClass::EC_2P1G1, 3);
}

/// `EC_4P1G1`: 5 engines (4 data, 1 parity).
fn multi_engine_ec_objects_ec_4p1() {
    ec_objects(ObjectClass::EC_4P1G1, 5);
}

/// Update and fetch on an erasure-code object, KV and array. Key
/// enumeration of such objects is not offered in this release: both the
/// object and the KV listing must return `Unsupported`.
fn ec_objects(class: ObjectClass, width: u32) {
    let (_e, pool) = setup();
    require_engines(&pool, width, &format!("{class:?}"));
    let tc = rank_level_container(&pool);
    let c = tc.open();
    let o = c
        .object(oid(&c, ObjectType::MultiHashed, class), OpenMode::ReadWrite)
        .unwrap();
    println!("{class:?} -> {:?}", o.id().class());
    let v = pattern(1 << 20, 1);
    o.put_single(None, b"d", b"a", &v).unwrap();
    assert!(o.get_single_vec(None, b"d", b"a").unwrap().unwrap() == v);
    assert!(matches!(o.dkeys(), Err(Error::Unsupported { .. })));
    let kv = c
        .kv(oid(&c, ObjectType::KvHashed, class), OpenMode::ReadWrite)
        .unwrap();
    kv.put(None, "k", b"v").unwrap();
    assert!(kv.get(None, "k").unwrap().unwrap() == b"v");
    assert!(matches!(kv.keys(), Err(Error::Unsupported { .. })));
    let a = c
        .array_open_with_attr(
            oid(&c, ObjectType::ArrayByte, class),
            OpenMode::ReadWrite,
            1,
            1 << 20,
        )
        .unwrap();
    let v = pattern(3 << 20, 5);
    a.write_at(None, 0, &v).unwrap();
    let mut b = vec![0u8; v.len()];
    a.read_at(None, 0, &mut b).unwrap();
    assert!(b == v, "{class:?}");
}

/// `rd_fac` 1 at rank level (2 engines).
fn multi_engine_container_redundancy_rank() {
    let (_e, pool) = setup();
    require_engines(&pool, 2, "rd_fac 1 at rank level");
    container_redundancy(&pool, 1, RedundancyLevel::Rank);
}

/// `rd_fac` 2 at node level: 3 engines on 3 different nodes. The engine
/// count does not show the node count, so the case runs only when
/// `RUST_DAOS_TEST_NODES` says the system has at least 3 nodes.
fn multi_engine_container_redundancy_node() {
    let nodes: u32 = std::env::var("RUST_DAOS_TEST_NODES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);
    if nodes < 3 {
        skip(&format!(
            "rd_fac 2 at node level needs 3 nodes, RUST_DAOS_TEST_NODES is {nodes}"
        ));
    }
    let (_e, pool) = setup();
    require_engines(&pool, 3, "rd_fac 2 at node level");
    container_redundancy(&pool, 2, RedundancyLevel::Node);
}

fn container_redundancy(pool: &Pool, rf: u8, level: RedundancyLevel) {
    let tc = TestCont::create(
        pool,
        &format!("-rf{rf}"),
        ContainerCreate::new()
            .redundancy_factor(rf)
            .redundancy_level(level),
    );
    let c = tc.open();
    let p = c.properties().unwrap();
    assert_eq!(p.redundancy_factor, Some(u64::from(rf)));
    let id = c
        .generate_oid(
            1,
            0,
            ObjectType::MultiHashed,
            ObjectClass::UNKNOWN,
            ClassHints::default(),
        )
        .unwrap();
    println!(
        "rf {rf} at {level:?}: default class {:?}",
        id.class().name()
    );
}

fn main() {
    let cases = [
        case("sweep_leftovers", sweep_leftovers),
        case("symbols", symbols),
        case("pool_query", pool_query),
        case("container_lifecycle", container_lifecycle),
        case(
            "posix_container_mount_and_objects",
            posix_container_mount_and_objects,
        ),
        case("shared_connection_handles", shared_connection_handles),
        case("objects", objects),
        case("enumeration", enumeration),
        case("integer_keys_and_types", integer_keys_and_types),
        case("kv_and_arrays", kv_and_arrays),
        case("transactions", transactions),
        case("threads_and_close", threads_and_close),
        case("container_close_with_dfs_io", container_close_with_dfs_io),
        Case {
            name: "mr_stress_objects_launcher_disabled",
            run: mr_stress_objects,
            env: &[("CRT_MRC_ENABLE", Some("0"))],
        },
        Case {
            name: "mr_stress_objects_default_env",
            run: mr_stress_objects,
            env: &[("CRT_MRC_ENABLE", None)],
        },
        case("stale_handle", stale_handle),
        case("defaults_record", defaults_record),
        case("multi_engine_rp2_objects", multi_engine_rp2_objects),
        case("multi_engine_rp3_objects", multi_engine_rp3_objects),
        case(
            "multi_engine_ec_objects_ec_2p1",
            multi_engine_ec_objects_ec_2p1,
        ),
        case(
            "multi_engine_ec_objects_ec_4p1",
            multi_engine_ec_objects_ec_4p1,
        ),
        case(
            "multi_engine_container_redundancy_rank",
            multi_engine_container_redundancy_rank,
        ),
        case(
            "multi_engine_container_redundancy_node",
            multi_engine_container_redundancy_node,
        ),
    ];
    run_main(&cases, Vec::new);
}
