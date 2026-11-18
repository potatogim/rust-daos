// Copyright (c) 2026 Jihyeon Gim
// SPDX-License-Identifier: MIT OR Apache-2.0

//! List a DFS directory: `ls <pool> <container> [path]`.
//!
//! Prints one entry per line with its type and size. Exit status 1 on error.

use std::process::ExitCode;

fn run() -> Result<(), daos::Error> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 2 {
        eprintln!("usage: ls <pool> <container> [path]");
        std::process::exit(2);
    }
    // SAFETY: first DAOS call of main, before any other thread is started.
    let report = unsafe { daos::init(&daos::InitOptions::default())? };
    for w in &report.warnings {
        eprintln!("warning: {w}");
    }
    let dfs = daos::Dfs::connect(&args[0], &args[1])?;
    let dir = args.get(2).map_or("/", String::as_str);
    let mut entries = Vec::new();
    for entry in dfs.read_dir(dir)? {
        entries.push(entry?.path());
    }
    entries.sort();
    for p in entries {
        let md = dfs.metadata(&p)?;
        let kind = if md.is_dir() { 'd' } else { '-' };
        println!("{kind} {:>12} {p}", md.len());
    }
    dfs.close(std::time::Duration::from_secs(30))
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
