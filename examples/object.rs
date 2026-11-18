// Copyright (c) 2026 Jihyeon Gim
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Object walk: `object <pool> <container>`.
//!
//! Writes single values and records under integer dkeys of one object, reads
//! them back in one fetch, lists the dkeys and finds the largest one, then
//! punches the object. The container must exist. Exit status 1 on error.

use std::process::ExitCode;

use daos::obj::{key_u64, u64_key, ClassHints, ObjectClass, ObjectType, OpenMode, Recx};

fn run() -> Result<(), daos::Error> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() != 2 {
        eprintln!("usage: object <pool> <container>");
        std::process::exit(2);
    }
    // SAFETY: first DAOS call of main, before any other thread is started.
    unsafe { daos::init(&daos::InitOptions::default())? };
    let cont = daos::Container::open(&args[0], &args[1])?;
    let id = cont.oid_allocator().next(
        ObjectType::DkeyUint64,
        ObjectClass::UNKNOWN,
        ClassHints::default(),
    )?;
    let obj = cont.object(id, OpenMode::ReadWrite)?;
    println!("object {id} class {:?}", id.class().name());
    for step in 0..4u64 {
        let payload = vec![step as u8; 256];
        obj.update(&u64_key(step))
            .single(b"name", format!("step-{step}").as_bytes())
            .records(b"data", 1, &[Recx::new(0, 256)], &payload)
            .execute()?;
    }
    let mut name = [0u8; 32];
    let mut data = vec![0u8; 256];
    let out = obj
        .fetch(&u64_key(2))
        .single(b"name", &mut name)
        .records(b"data", 1, &[Recx::new(0, 256)], &mut data)
        .execute()?;
    let len = out.sizes()[0] as usize;
    println!(
        "dkey 2: name {}, data[0] {}",
        String::from_utf8_lossy(&name[..len]),
        data[0]
    );
    let mut keys = Vec::new();
    for k in obj.dkeys()? {
        keys.push(key_u64(&k?).unwrap_or_default());
    }
    keys.sort();
    println!("dkeys {keys:?}, largest {:?}", obj.max_dkey_u64(None)?);
    obj.punch(None)?;
    obj.close()?;
    cont.close(std::time::Duration::from_secs(30))
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
