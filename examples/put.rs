// Copyright (c) 2026 Jihyeon Gim
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Write stdin to a DFS file: `put <pool> <container> <path>`.
//!
//! Parent directories are created; an existing file is truncated.

use std::io::{BufWriter, Read, Write};
use std::process::ExitCode;

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() != 3 {
        eprintln!("usage: put <pool> <container> <path>");
        std::process::exit(2);
    }
    // SAFETY: first DAOS call of main, before any other thread is started.
    let report = unsafe { daos::init(&daos::InitOptions::default())? };
    for w in &report.warnings {
        eprintln!("warning: {w}");
    }
    let dfs = daos::Dfs::connect(&args[0], &args[1])?;
    let writer = dfs.writer(&args[2])?;
    // Coalesce small stdin reads into 4 MiB writes.
    let mut out = BufWriter::with_capacity(4 << 20, writer);
    let mut input = std::io::stdin().lock();
    let mut chunk = vec![0u8; 1 << 20];
    loop {
        let n = input.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        out.write_all(&chunk[..n])?;
    }
    let writer = out.into_inner().map_err(|e| e.into_error())?;
    let written = writer.finish()?;
    eprintln!("wrote {written} bytes to {}", args[2]);
    dfs.close(std::time::Duration::from_secs(30))?;
    Ok(())
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
