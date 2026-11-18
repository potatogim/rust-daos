// Copyright (c) 2026 Jihyeon Gim
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Key-value walk: `kv <pool> <container>`.
//!
//! Creates a key-value object, writes a few keys, updates one in a
//! transaction, lists the keys and destroys the object. The container must
//! exist. Exit status 1 on error.

use std::process::ExitCode;

use daos::obj::{ClassHints, ObjectClass, ObjectType, OpenMode};
use daos::tx::TxMode;

fn run() -> Result<(), daos::Error> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() != 2 {
        eprintln!("usage: kv <pool> <container>");
        std::process::exit(2);
    }
    // SAFETY: first DAOS call of main, before any other thread is started.
    let report = unsafe { daos::init(&daos::InitOptions::default())? };
    for w in &report.warnings {
        eprintln!("warning: {w}");
    }
    let cont = daos::Container::open(&args[0], &args[1])?;
    let id = cont.oid_allocator().next(
        ObjectType::KvHashed,
        ObjectClass::UNKNOWN,
        ClassHints::default(),
    )?;
    let kv = cont.kv(id, OpenMode::ReadWrite)?;
    for (k, v) in [("alpha", "1"), ("beta", "2"), ("gamma", "3")] {
        kv.put(None, k, v.as_bytes())?;
    }
    cont.transaction(TxMode::ReadWrite, |tx| {
        let old = kv.get(Some(tx), "beta")?.unwrap_or_default();
        let mut new = old.clone();
        new.extend_from_slice(b"0");
        kv.put(Some(tx), "beta", &new)
    })?;
    let mut keys = Vec::new();
    for k in kv.keys()? {
        keys.push(k?);
    }
    keys.sort();
    for k in keys {
        let v = kv.get(None, &k)?.unwrap_or_default();
        println!("{k} = {}", String::from_utf8_lossy(&v));
    }
    kv.destroy(None)?;
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
