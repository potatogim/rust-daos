// Copyright (c) 2026 Jihyeon Gim
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Copy a DFS file to stdout: `cat <pool> <container> <path>`.
//!
//! Reads in 8 MiB pieces straight into one reused buffer.

use std::io::Write;
use std::process::ExitCode;

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() != 3 {
        eprintln!("usage: cat <pool> <container> <path>");
        std::process::exit(2);
    }
    // SAFETY: first DAOS call of main, before any other thread is started.
    let report = unsafe { daos::init(&daos::InitOptions::default())? };
    for w in &report.warnings {
        eprintln!("warning: {w}");
    }
    let dfs = daos::Dfs::connect(&args[0], &args[1])?;
    let file = dfs.open(&args[2])?;
    let size = file.size()?;
    let mut buf = vec![0u8; 8 << 20];
    let mut out = std::io::stdout().lock();
    let mut off = 0u64;
    while off < size {
        let n = file.read_at(&mut buf, off)?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n])?;
        off += n as u64;
    }
    out.flush()?;
    file.close()?;
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
