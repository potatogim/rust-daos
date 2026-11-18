// Copyright (c) 2026 Jihyeon Gim
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Transactions: [`Tx`] and [`Container::transaction`](crate::Container::transaction).
//!
//! A transaction lives on the thread that opened it: `Tx` is neither `Send`
//! nor `Sync`, because DAOS does not support the operations of one
//! transaction from several threads ("Invoking operations of one TX with
//! events from multiple event queues, including with NULL events from
//! multiple threads, is not currently supported", `daos_api.h`). Iterators
//! and builders that borrow a `Tx` inherit this.
//!
//! The crate never opens a transaction with `DAOS_TF_ZERO_COPY`: without it
//! libdaos copies the data of an update during the call, which is what makes
//! borrowed buffers sound.
//!
//! ```compile_fail,E0277
//! fn send<T: Send>(_: T) {}
//! fn f(c: &daos::Container) {
//!     let tx = c.tx(daos::tx::TxMode::ReadWrite).unwrap();
//!     send(tx);
//! }
//! ```
//!
//! ```compile_fail,E0277
//! fn f(c: &daos::Container) {
//!     let tx = c.tx(daos::tx::TxMode::ReadWrite).unwrap();
//!     std::thread::scope(|s| {
//!         s.spawn(|| tx.epoch());
//!     });
//! }
//! ```

use std::cell::Cell;
use std::fmt;
use std::marker::PhantomData;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use crate::conn::{require, ContConn};
use crate::cont::Container;
use crate::error::{Error, Result};
use crate::sys::api::Group;
use crate::sys::{self, daos_handle_t};

/// Mode of a transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum TxMode {
    /// Reads and writes.
    #[default]
    ReadWrite,
    /// Reads only (`DAOS_TF_RDONLY`).
    ReadOnly,
}

/// Options of [`Container::transaction_with`](crate::Container::transaction_with).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct TxOptions {
    /// Restarts after which the last `DER_TX_RESTART` error is returned;
    /// `None` retries without a limit (restart is the normal conflict path
    /// in DAOS).
    pub max_restarts: Option<u32>,
}

impl TxOptions {
    /// Default options (no restart limit).
    pub fn new() -> Self {
        Self::default()
    }

    /// Bound the number of restarts.
    pub fn max_restarts(mut self, n: u32) -> Self {
        self.max_restarts = Some(n);
        self
    }
}

/// State of a transaction as the crate tracks it. libdaos (`obj_tx.c`, the
/// same in 2.6.4 and 2.8.1) allows `daos_tx_abort` and `daos_tx_commit` only
/// in its `TX_OPEN` state, `daos_tx_restart` only in `TX_FAILED` (after
/// `DER_TX_RESTART`), and `daos_tx_close` in every state; a commit that fails
/// with another error leaves the transaction aborted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TxState {
    /// `TX_OPEN`.
    Open,
    /// `TX_COMMITTED`.
    Committed,
    /// Aborted by the caller, or given up after a restart error or a failed
    /// commit.
    Aborted,
    /// A call returned `DER_TX_RESTART` (`TX_FAILED`): only restart or close.
    NeedsRestart,
    /// The commit failed with an error other than `DER_TX_RESTART`
    /// (`TX_ABORTED` in libdaos): only close.
    Failed,
}

/// An open transaction on a container. See the [module docs](self).
///
/// Every data call of the object, key-value and array API takes
/// `tx: Option<&Tx>`; `None` is `DAOS_TX_NONE` (an independent operation).
/// Dropping an open transaction that was not committed aborts it.
pub struct Tx<'c> {
    pub(crate) cont: Arc<ContConn>,
    th: daos_handle_t,
    state: Cell<TxState>,
    read_only: bool,
    snapshot: bool,
    closed: Cell<bool>,
    _container: PhantomData<&'c Container>,
    /// Not `Send`, not `Sync`.
    _thread: PhantomData<*const ()>,
}

impl fmt::Debug for Tx<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tx")
            .field("state", &self.state.get())
            .field("read_only", &self.read_only)
            .field("snapshot", &self.snapshot)
            .finish()
    }
}

impl<'c> Tx<'c> {
    pub(crate) fn open(cont: &'c Container, mode: TxMode) -> Result<Tx<'c>> {
        let conn = cont.conn.clone();
        require(conn.backend.as_ref(), Group::Tx)?;
        let _pass = conn.enter()?;
        let flags = match mode {
            TxMode::ReadWrite => 0,
            TxMode::ReadOnly => sys::DAOS_TF_RDONLY,
        };
        debug_assert_eq!(flags & sys::DAOS_TF_ZERO_COPY, 0);
        let mut th = daos_handle_t::default();
        // SAFETY: live container handle (gate held); out-pointer to a local;
        // flags never contain DAOS_TF_ZERO_COPY; ev NULL.
        let rc = unsafe {
            conn.backend
                .daos_tx_open(conn.coh, &mut th, flags, std::ptr::null_mut())
        };
        if rc != 0 {
            return Err(Error::daos_at("daos_tx_open", rc, conn.target()));
        }
        conn.transactions.fetch_add(1, Ordering::SeqCst);
        drop(_pass);
        Ok(Tx {
            cont: conn,
            th,
            state: Cell::new(TxState::Open),
            read_only: mode == TxMode::ReadOnly,
            snapshot: false,
            closed: Cell::new(false),
            _container: PhantomData,
            _thread: PhantomData,
        })
    }

    pub(crate) fn open_snapshot(cont: &'c Container, epoch: u64) -> Result<Tx<'c>> {
        let conn = cont.conn.clone();
        require(conn.backend.as_ref(), Group::Tx)?;
        let _pass = conn.enter()?;
        let mut th = daos_handle_t::default();
        // SAFETY: live container handle; out-pointer to a local; ev NULL.
        let rc = unsafe {
            conn.backend
                .daos_tx_open_snap(conn.coh, epoch, &mut th, std::ptr::null_mut())
        };
        if rc != 0 {
            return Err(Error::daos_at("daos_tx_open_snap", rc, conn.target()));
        }
        conn.transactions.fetch_add(1, Ordering::SeqCst);
        drop(_pass);
        Ok(Tx {
            cont: conn,
            th,
            state: Cell::new(TxState::Open),
            read_only: true,
            snapshot: true,
            closed: Cell::new(false),
            _container: PhantomData,
            _thread: PhantomData,
        })
    }

    /// True for a read-only or snapshot transaction.
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// True after `commit` succeeded.
    pub fn is_committed(&self) -> bool {
        self.state.get() == TxState::Committed
    }

    /// True when a call of this transaction returned `DER_TX_RESTART`;
    /// [`Tx::restart`] reopens it.
    pub fn needs_restart(&self) -> bool {
        self.state.get() == TxState::NeedsRestart
    }

    fn call(&self, op: &'static str, f: impl FnOnce(daos_handle_t) -> i32) -> Result<()> {
        if self.closed.get() {
            return Err(Error::InvalidArgument("the transaction is closed"));
        }
        self.cont.stamp.check()?;
        // Lifecycle calls are allowed while a container close waits: that
        // close waits for this transaction to end.
        let _pass = self.cont.gate.enter(true)?;
        let rc = f(self.th);
        if rc != 0 {
            let e = Error::daos(op, rc);
            if e.is_tx_restart() {
                self.state.set(TxState::NeedsRestart);
            }
            return Err(e);
        }
        Ok(())
    }

    /// Commit (`daos_tx_commit`). An error with
    /// [`Error::is_tx_restart`] means: call [`Tx::restart`] and run the
    /// operations again. A snapshot transaction cannot be committed.
    pub fn commit(&mut self) -> Result<()> {
        if self.snapshot {
            return Err(Error::InvalidArgument(
                "a snapshot transaction is not committed",
            ));
        }
        if self.state.get() != TxState::Open {
            return Err(Error::InvalidArgument("the transaction is not open"));
        }
        let backend = self.cont.backend.clone();
        // SAFETY: an open transaction handle of this thread; ev NULL.
        let r = self.call("daos_tx_commit", |th| unsafe {
            backend.daos_tx_commit(th, std::ptr::null_mut())
        });
        match &r {
            Ok(()) => self.state.set(TxState::Committed),
            // `call` has set NeedsRestart.
            Err(e) if e.is_tx_restart() => {}
            // libdaos has aborted the transaction (or never opened the
            // commit); only close is left.
            Err(Error::Daos { .. }) => self.state.set(TxState::Failed),
            // Refused before the call (fork, closed handle): unchanged.
            Err(_) => {}
        }
        r
    }

    /// Restart after `DER_TX_RESTART` (`daos_tx_restart`); the transaction
    /// is open again and empty. Only after a call of this transaction
    /// returned `DER_TX_RESTART` ([`Tx::needs_restart`]); libdaos refuses
    /// it in every other state.
    pub fn restart(&mut self) -> Result<()> {
        if self.snapshot {
            return Err(Error::InvalidArgument(
                "a snapshot transaction is not restarted",
            ));
        }
        match self.state.get() {
            TxState::NeedsRestart => {}
            TxState::Committed => {
                return Err(Error::InvalidArgument("the transaction is committed"))
            }
            TxState::Open | TxState::Aborted | TxState::Failed => {
                return Err(Error::InvalidArgument(
                    "the transaction does not need a restart",
                ))
            }
        }
        let backend = self.cont.backend.clone();
        // SAFETY: as in commit.
        self.call("daos_tx_restart", |th| unsafe {
            backend.daos_tx_restart(th, std::ptr::null_mut())
        })?;
        self.state.set(TxState::Open);
        Ok(())
    }

    /// Abort: nothing of the transaction becomes visible. Calls
    /// `daos_tx_abort` only for an open transaction; after a restart error
    /// or a failed commit nothing was made visible and libdaos refuses the
    /// abort, so the transaction is only marked aborted.
    pub fn abort(&mut self) -> Result<()> {
        self.abort_inner()
    }

    fn abort_inner(&self) -> Result<()> {
        if self.snapshot {
            return Ok(());
        }
        match self.state.get() {
            TxState::Committed => {
                return Err(Error::InvalidArgument("the transaction is committed"))
            }
            TxState::Aborted => return Ok(()),
            TxState::NeedsRestart | TxState::Failed => {
                self.state.set(TxState::Aborted);
                return Ok(());
            }
            TxState::Open => {}
        }
        let backend = self.cont.backend.clone();
        // SAFETY: as in commit.
        let r = self.call("daos_tx_abort", |th| unsafe {
            backend.daos_tx_abort(th, std::ptr::null_mut())
        });
        match r {
            Ok(()) => {}
            // libdaos no longer has the transaction open (DER_NO_PERM) or
            // has aborted it already (DER_ALREADY): nothing to undo.
            Err(Error::Daos { code, .. })
                if code == crate::DerCode::NO_PERM || code == crate::DerCode::ALREADY => {}
            Err(e) => return Err(e),
        }
        self.state.set(TxState::Aborted);
        Ok(())
    }

    /// The epoch of the transaction (`daos_tx_hdl2epoch`); an error until
    /// the transaction has done its first operation.
    pub fn epoch(&self) -> Result<u64> {
        let mut epoch = 0u64;
        let backend = self.cont.backend.clone();
        // SAFETY: an open transaction handle; out-pointer to a local.
        self.call("daos_tx_hdl2epoch", |th| unsafe {
            backend.daos_tx_hdl2epoch(th, &mut epoch)
        })?;
        Ok(epoch)
    }

    /// Close the handle (`daos_tx_close`); an open, uncommitted transaction
    /// is aborted first. Dropping does the same and ignores errors.
    pub fn close(self) -> Result<()> {
        // Drop runs afterwards and finds the handle closed.
        self.finish()
    }

    fn finish(&self) -> Result<()> {
        if self.closed.get() {
            return Ok(());
        }
        // Only an open transaction is aborted; libdaos refuses the abort in
        // every other state, and close alone is enough there.
        let aborted = if self.state.get() == TxState::Open {
            self.abort_inner()
        } else {
            Ok(())
        };
        let backend = self.cont.backend.clone();
        // SAFETY: an open transaction handle of this thread, closed once.
        let closed = self.call("daos_tx_close", |th| unsafe {
            backend.daos_tx_close(th, std::ptr::null_mut())
        });
        self.closed.set(true);
        self.cont.transactions.fetch_sub(1, Ordering::SeqCst);
        aborted.and(closed)
    }

    /// The handle to pass to a data call, after checking that the
    /// transaction belongs to `cont`, is open, and allows writes when
    /// `write` is set.
    pub(crate) fn handle_for(&self, cont: &Arc<ContConn>, write: bool) -> Result<daos_handle_t> {
        if !Arc::ptr_eq(&self.cont, cont) {
            return Err(Error::InvalidArgument(
                "the transaction belongs to another container connection",
            ));
        }
        if self.closed.get() {
            return Err(Error::InvalidArgument("the transaction is closed"));
        }
        match self.state.get() {
            TxState::Open => {}
            TxState::NeedsRestart => {
                return Err(Error::InvalidArgument("the transaction needs a restart"))
            }
            TxState::Committed | TxState::Aborted | TxState::Failed => {
                return Err(Error::InvalidArgument("the transaction is not open"))
            }
        }
        if write && self.read_only {
            return Err(Error::InvalidArgument("the transaction is read-only"));
        }
        Ok(self.th)
    }

    /// Record the result of a data call made with this transaction.
    pub(crate) fn note(&self, rc: i32) {
        if rc == -(crate::DerCode::TX_RESTART.value()) {
            self.state.set(TxState::NeedsRestart);
        }
    }
}

impl Drop for Tx<'_> {
    fn drop(&mut self) {
        if self.cont.stamp.is_foreign() {
            return; // fork child: leak
        }
        let _ = self.finish();
    }
}

/// Transaction handle for a data call: `DAOS_TX_NONE` without a
/// transaction.
pub(crate) fn th(tx: Option<&Tx<'_>>, cont: &Arc<ContConn>, write: bool) -> Result<daos_handle_t> {
    match tx {
        None => Ok(sys::DAOS_TX_NONE),
        Some(t) => t.handle_for(cont, write),
    }
}

/// Note the result of a data call on the transaction, if any.
pub(crate) fn note(tx: Option<&Tx<'_>>, rc: i32) {
    if let Some(t) = tx {
        t.note(rc);
    }
}

impl Container {
    /// Open a transaction (`daos_tx_open`). See [`Tx`].
    pub fn tx(&self, mode: TxMode) -> Result<Tx<'_>> {
        Tx::open(self, mode)
    }

    /// Open a read-only transaction that reads the snapshot at `epoch`
    /// (`daos_tx_open_snap`). Writes through it are refused before any
    /// DAOS call; it needs no commit.
    pub fn snapshot_tx(&self, epoch: u64) -> Result<Tx<'_>> {
        Tx::open_snapshot(self, epoch)
    }

    /// Run `f` in a transaction and commit it. On `DER_TX_RESTART` (from `f`
    /// or from the commit) the transaction is restarted and `f` runs again,
    /// without a limit; any other error aborts and is returned.
    ///
    /// A restart happens only when a call of this transaction returned
    /// `DER_TX_RESTART` ([`Tx::needs_restart`]): a restart error of another
    /// transaction returned by `f` is returned like any other error. When `f`
    /// returns `Ok` although a call of the transaction needs a restart (`f`
    /// swallowed the error), the transaction is restarted as well; if the
    /// restart limit is reached then, the error returned is a
    /// `DER_TX_RESTART` error with op `daos_tx_commit`.
    ///
    /// ```no_run
    /// # fn f(cont: &daos::Container, kv: &daos::kv::Kv) -> daos::Result<()> {
    /// use daos::tx::TxMode;
    /// cont.transaction(TxMode::ReadWrite, |tx| {
    ///     let n = kv.get(Some(tx), "counter")?.map_or(0, |v| v[0]);
    ///     kv.put(Some(tx), "counter", &[n + 1])
    /// })?;
    /// # Ok(()) }
    /// ```
    pub fn transaction<T>(&self, mode: TxMode, f: impl FnMut(&Tx<'_>) -> Result<T>) -> Result<T> {
        self.transaction_with(mode, &TxOptions::default(), f)
    }

    /// As [`Container::transaction`], with options (a restart limit).
    pub fn transaction_with<T>(
        &self,
        mode: TxMode,
        options: &TxOptions,
        mut f: impl FnMut(&Tx<'_>) -> Result<T>,
    ) -> Result<T> {
        let mut tx = self.tx(mode)?;
        let mut restarts: u32 = 0;
        loop {
            let err = match f(&tx) {
                Ok(v) if !tx.needs_restart() => match tx.commit() {
                    Ok(()) => {
                        tx.close()?;
                        return Ok(v);
                    }
                    Err(e) => e,
                },
                // `f` swallowed a restart error of this transaction.
                Ok(_) => Error::daos("daos_tx_commit", -crate::DerCode::TX_RESTART.value()),
                Err(e) => e,
            };
            let restart = err.is_tx_restart() && tx.needs_restart();
            if !restart || options.max_restarts.is_some_and(|m| restarts >= m) {
                let _ = tx.abort();
                drop(tx);
                return Err(err);
            }
            restarts += 1;
            tx.restart()?;
        }
    }
}
