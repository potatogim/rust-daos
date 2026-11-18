// Copyright (c) 2026 Jihyeon Gim
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Byte array walk: `array <pool> <container>`.
//!
//! Writes two pieces of a byte array with a hole between them, reads across
//! the hole and past the end, and prints the read counters. The container
//! must exist. Exit status 1 on error.

use std::process::ExitCode;

use daos::obj::{ClassHints, ObjectClass, ObjectType, OpenMode};

fn run() -> Result<(), daos::Error> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() != 2 {
        eprintln!("usage: array <pool> <container>");
        std::process::exit(2);
    }
    // SAFETY: first DAOS call of main, before any other thread is started.
    unsafe { daos::init(&daos::InitOptions::default())? };
    let cont = daos::Container::open(&args[0], &args[1])?;
    let id = cont.oid_allocator().next(
        ObjectType::ArrayByte,
        ObjectClass::UNKNOWN,
        ClassHints::default(),
    )?;
    // Byte arrays do not store their cell and chunk size: always open this
    // one with the same values.
    let arr = cont.array_open_with_attr(id, OpenMode::ReadWrite, 1, 1 << 20)?;
    arr.write_at(None, 0, b"head")?;
    arr.write_at(None, 100, b"tail")?;
    let mut buf = vec![0xffu8; 128];
    let read = arr.read_at(None, 0, &mut buf)?;
    println!(
        "size {} cells, counters {:?}",
        arr.size(None)?,
        read.counts()
    );
    println!("bytes 0..8 {:?}, 100..104 {:?}", &buf[..8], &buf[100..104]);
    arr.destroy(None)?;
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
