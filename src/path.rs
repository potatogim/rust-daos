// Copyright (c) 2026 Jihyeon Gim
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Path rules for DFS paths.
//!
//! DFS paths are absolute, `/`-separated byte strings inside one container.
//! [`normalize`] turns user input into the canonical form that every API of
//! this crate passes to libdfs.

use crate::error::{Error, Result};

/// Longest component DFS accepts (`DFS_MAX_NAME`, which is `NAME_MAX`).
pub const MAX_NAME: usize = 255;
/// `DFS_MAX_PATH` (`PATH_MAX`); a normalised path must be shorter than this.
/// The longest accepted normalised path is `MAX_PATH - 1` bytes, as in
/// `dfs_lookup`.
pub const MAX_PATH: usize = 4096;

/// Normalise a DFS path.
///
/// - `""`, `"."` and `"/"` give `"/"`.
/// - A leading `/` is added; repeated slashes collapse; `.` components are
///   dropped; a trailing `/` is dropped.
/// - `..` resolves lexically and never climbs above `/`.
/// - Rejected with [`Error::InvalidPath`]: an interior NUL byte, a component
///   longer than 255 bytes, and a result of 4096 bytes or more.
///
/// ```
/// assert_eq!(daos::path::normalize("a//b/./c/").unwrap(), "/a/b/c");
/// assert_eq!(daos::path::normalize("/a/../../b").unwrap(), "/b");
/// assert_eq!(daos::path::normalize("").unwrap(), "/");
/// ```
pub fn normalize(path: &str) -> Result<String> {
    let invalid = |reason: &'static str| Error::InvalidPath {
        path: path.to_string(),
        reason,
    };
    if path.as_bytes().contains(&0) {
        return Err(invalid("contains a NUL byte"));
    }
    let mut parts: Vec<&str> = Vec::new();
    for comp in path.split('/') {
        match comp {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            name => {
                if name.len() > MAX_NAME {
                    return Err(invalid("a component is longer than 255 bytes"));
                }
                parts.push(name);
            }
        }
    }
    let mut out = String::with_capacity(path.len() + 1);
    for p in &parts {
        out.push('/');
        out.push_str(p);
    }
    if out.is_empty() {
        out.push('/');
    }
    if out.len() >= MAX_PATH {
        return Err(invalid("path is 4096 bytes or longer"));
    }
    Ok(out)
}

/// Split a normalised path into parent directory and last component.
///
/// Returns `None` for the root `/`.
///
/// ```
/// assert_eq!(daos::path::split_parent("/a/b"), Some(("/a", "b")));
/// assert_eq!(daos::path::split_parent("/a"), Some(("/", "a")));
/// assert_eq!(daos::path::split_parent("/"), None);
/// ```
pub fn split_parent(normalized: &str) -> Option<(&str, &str)> {
    if normalized == "/" || normalized.is_empty() {
        return None;
    }
    let i = normalized.rfind('/')?;
    let parent = if i == 0 { "/" } else { &normalized[..i] };
    Some((parent, &normalized[i + 1..]))
}

/// File operations refuse a path that names a directory by its syntax: a
/// trailing `/`, or a last component of `.` or `..`.
pub(crate) fn reject_dir_syntax(raw: &str, normalized: &str) -> Result<()> {
    if normalized == "/" {
        return Ok(());
    }
    let trimmed = raw.trim_end_matches('/');
    if trimmed.len() != raw.len()
        || trimmed.ends_with("/.")
        || trimmed.ends_with("/..")
        || trimmed == "."
        || trimmed == ".."
    {
        return Err(Error::InvalidPath {
            path: raw.to_string(),
            reason: "a file path must not end with '/', '.' or '..'",
        });
    }
    Ok(())
}

/// Join a normalised directory path and a name.
pub(crate) fn join(dir: &str, name: &str) -> String {
    if dir == "/" {
        format!("/{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// True when `path` is `dir` or lies below it (both normalised).
pub(crate) fn is_at_or_below(path: &str, dir: &str) -> bool {
    if dir == "/" {
        return true;
    }
    path == dir || (path.starts_with(dir) && path.as_bytes().get(dir.len()) == Some(&b'/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `prefix` followed by components of at most 200 bytes, `n` bytes long
    /// in total (`n` at least `prefix.len() + 2`).
    fn path_of_len(prefix: &str, n: usize) -> String {
        let mut p = prefix.to_string();
        while p.len() < n {
            let room = n - p.len();
            let take = if room <= 201 {
                room - 1
            } else {
                200.min(room - 3)
            };
            p.push('/');
            p.push_str(&"x".repeat(take));
        }
        p
    }

    #[test]
    fn normalisation_table() {
        let cases = [
            ("", "/"),
            (".", "/"),
            ("/", "/"),
            ("//", "/"),
            ("a", "/a"),
            ("/a/", "/a"),
            ("//a//b/", "/a/b"),
            ("a/./b/.", "/a/b"),
            ("a/../b", "/b"),
            ("/../..", "/"),
            ("/a/b/../../..", "/"),
            ("/a/b/../c", "/a/c"),
            ("..a/b..", "/..a/b.."),
        ];
        for (input, want) in cases {
            assert_eq!(normalize(input).unwrap(), want, "input {input:?}");
        }
    }

    #[test]
    fn rejects_bad_input() {
        assert!(matches!(normalize("/a\0b"), Err(Error::InvalidPath { .. })));
        let long = "x".repeat(256);
        assert!(matches!(normalize(&long), Err(Error::InvalidPath { .. })));
        assert!(normalize(&"x".repeat(255)).is_ok());
        let comp = "y".repeat(200);
        let mut p = String::new();
        while p.len() < MAX_PATH {
            p.push('/');
            p.push_str(&comp);
        }
        assert!(matches!(normalize(&p), Err(Error::InvalidPath { .. })));
        // Exactly at the limit, counting the leading slash.
        let at = path_of_len("", MAX_PATH - 1);
        assert_eq!(at.len(), 4095);
        assert_eq!(normalize(&at).unwrap(), at);
        let over = path_of_len("", MAX_PATH);
        assert_eq!(over.len(), 4096);
        assert!(matches!(normalize(&over), Err(Error::InvalidPath { .. })));
        // A trailing slash is dropped before the length is checked.
        assert_eq!(normalize(&format!("{at}/")).unwrap(), at);
        // A long input that normalises to something short is accepted.
        let back = format!("{}z", "a/../".repeat(2000));
        assert_eq!(normalize(&back).unwrap(), "/z");
    }

    #[test]
    fn split_and_join() {
        assert_eq!(split_parent("/"), None);
        assert_eq!(split_parent("/a"), Some(("/", "a")));
        assert_eq!(split_parent("/a/b/c"), Some(("/a/b", "c")));
        assert_eq!(join("/", "a"), "/a");
        assert_eq!(join("/a", "b"), "/a/b");
    }

    #[test]
    fn dir_syntax_is_rejected_for_files() {
        for bad in ["/a/", "a//", "/a/b/.", "/a/b/.."] {
            let n = normalize(bad).unwrap();
            assert!(reject_dir_syntax(bad, &n).is_err(), "{bad}");
        }
        for good in ["/a", "a/b", "/a/./b", "/a/../b"] {
            let n = normalize(good).unwrap();
            assert!(reject_dir_syntax(good, &n).is_ok(), "{good}");
        }
    }

    #[test]
    fn below() {
        assert!(is_at_or_below("/a", "/a"));
        assert!(is_at_or_below("/a/b", "/a"));
        assert!(!is_at_or_below("/ab", "/a"));
        assert!(is_at_or_below("/x", "/"));
    }
}
