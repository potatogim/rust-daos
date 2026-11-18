// Copyright (c) 2026 Jihyeon Gim
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Compiling counterparts of the `compile_fail` examples in the module docs
//! (built only for doc tests).
//!
//! Each block below is one `compile_fail` example without the line that must
//! fail. Together they show that the failing examples fail at that line and
//! not for an unrelated reason such as a renamed method. Stable rustdoc
//! ignores the error codes written on `compile_fail` blocks (only nightly
//! checks them), so these counterparts are what keeps the examples specific
//! on stable.
//!
//! `tx.rs`, moving a `Tx` to another thread:
//!
//! ```no_run
//! fn send<T: Send>(_: T) {}
//! fn f(c: &daos::Container) {
//!     let tx = c.tx(daos::tx::TxMode::ReadWrite).unwrap();
//!     send(c.clone());
//!     drop(tx);
//! }
//! ```
//!
//! `tx.rs`, sharing a `Tx` with another thread:
//!
//! ```no_run
//! fn f(c: &daos::Container) {
//!     let tx = c.tx(daos::tx::TxMode::ReadWrite).unwrap();
//!     let _ = tx.epoch();
//!     std::thread::scope(|s| {
//!         s.spawn(|| c.is_closed());
//!     });
//! }
//! ```
//!
//! `obj/list.rs`, moving a `TxKeyIter` and using it after the `Tx` closed:
//!
//! ```no_run
//! fn send<T: Send>(_: T) {}
//! fn f(c: &daos::Container, o: &daos::obj::Object) {
//!     let tx = c.tx(daos::tx::TxMode::ReadOnly).unwrap();
//!     send(o.dkeys().unwrap());
//!     let it = o.dkeys_in(&tx).unwrap();
//!     drop(it);
//!     tx.close().unwrap();
//! }
//! ```
//!
//! `kv.rs`, the same for `TxKvKeys`:
//!
//! ```no_run
//! fn send<T: Send>(_: T) {}
//! fn f(c: &daos::Container, kv: &daos::Kv) {
//!     let tx = c.tx(daos::tx::TxMode::ReadOnly).unwrap();
//!     send(kv.keys().unwrap());
//!     let keys = kv.keys_in(&tx).unwrap();
//!     drop(keys);
//!     tx.close().unwrap();
//! }
//! ```
//!
//! `obj/io.rs`, moving an `Update` builder:
//!
//! ```no_run
//! fn send<T: Send>(_: T) {}
//! fn f(o: &daos::obj::Object) {
//!     send(o.clone());
//!     o.update(b"d").single(b"a", b"v").execute().unwrap();
//! }
//! ```
