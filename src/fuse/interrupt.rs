// SPDX-License-Identifier: BSD-2-Clause
//! FUSE_INTERRUPT support — cancellable long-running operations.
//!
//! ## Design
//!
//! Every FUSE request carries a `unique` ID (the request ID). When the kernel
//! wants to cancel an in-flight operation, it sends a `FUSE_INTERRUPT`
//! message with that ID. The filesystem sets an interrupt flag for the
//! request; the operation checks the flag at checkpoints and aborts.
//!
//! This module provides:
//! - `RequestId` — a monotonically increasing u64 identifying a request
//! - `InterruptManager` — tracks in-flight requests and their interrupt flags
//! - `InterruptToken` — a lightweight handle passed to long-running code
//!
//! Long-running operations should call `token.check()? at loop iteration
//! boundaries. When interrupted, `check()` returns `StorageError::Interrupted`.
//! The FUSE adapter maps this to `EINTR` for the kernel.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::storage::StorageError;

/// Unique identifier for a FUSE request, matching the `unique` field in
/// `struct fuse_in_header`.
pub type RequestId = u64;

/// Atomic flag that is set when the kernel requests cancellation of a
/// specific FUSE operation. Shared between the interrupt handler and the
/// worker thread executing the operation.
#[derive(Clone)]
pub struct InterruptFlag(Arc<AtomicBool>);

impl InterruptFlag {
    fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    /// Signal that the operation has been interrupted.
    pub fn set(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// Returns `true` if an interrupt was requested.
    pub fn is_set(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    /// Check the flag and return an error if set.
    ///
    /// Call this at loop boundaries inside long-running operations:
    ///
    /// ```ignore
    /// for chunk in &chunks {
    ///     token.check()?;
    ///     // … process chunk …
    /// }
    /// ```
    pub fn check(&self) -> Result<(), StorageError> {
        if self.is_set() {
            Err(StorageError::Interrupted)
        } else {
            Ok(())
        }
    }
}

impl Default for InterruptFlag {
    fn default() -> Self {
        Self::new()
    }
}

/// Token passed into long-running operations so they can check for
/// cancellation. Created by `InterruptManager::register()` and passed
/// by value (it's cheap — just one `Arc`).
#[derive(Clone)]
pub struct InterruptToken {
    pub id: RequestId,
    flag: InterruptFlag,
}

impl InterruptToken {
    /// Returns `true` if the operation has been interrupted.
    pub fn is_interrupted(&self) -> bool {
        self.flag.is_set()
    }

    /// Check the flag and return `Err(Interrupted)` if set.
    pub fn check(&self) -> Result<(), StorageError> {
        self.flag.check()
    }
}

/// Manages in-flight FUSE requests and their interrupt flags.
///
/// Each FUSE request is registered with `register()`, which returns a
/// `RequestId` and an `InterruptToken`. When the kernel sends
/// `FUSE_INTERRUPT` for that request, `interrupt()` sets the flag.
pub struct InterruptManager {
    next_id: AtomicU64,
    /// Maps request_id → interrupt flag.
    flags: Mutex<HashMap<RequestId, InterruptFlag>>,
}

impl InterruptManager {
    pub fn new() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            flags: Mutex::new(HashMap::new()),
        }
    }

    /// Register a new in-flight request. Returns its ID and a token the
    /// operation can poll for cancellation.
    pub fn register(&self) -> (RequestId, InterruptToken) {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let flag = InterruptFlag::new();
        self.flags
            .lock()
            .unwrap()
            .insert(id, flag.clone());
        (id, InterruptToken { id, flag })
    }

    /// Mark a request as interrupted (called when FUSE_INTERRUPT arrives).
    /// Returns `true` if the request was found and signaled.
    pub fn interrupt(&self, id: RequestId) -> bool {
        match self.flags.lock().unwrap().get(&id) {
            Some(flag) => {
                flag.set();
                true
            }
            None => false,
        }
    }

    /// Deregister a request (called when it completes, whether or not
    /// it was interrupted).
    pub fn deregister(&self, id: RequestId) {
        self.flags.lock().unwrap().remove(&id);
    }

    /// Returns the number of currently in-flight requests.
    pub fn pending_count(&self) -> usize {
        self.flags.lock().unwrap().len()
    }
}

impl Default for InterruptManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_request_lifecycle() {
        let mgr = InterruptManager::new();
        let (id, token) = mgr.register();
        assert!(id > 0);
        assert!(!token.is_interrupted());
        token.check().unwrap();

        assert!(mgr.interrupt(id));
        assert!(token.is_interrupted());
        assert!(token.check().is_err());

        mgr.deregister(id);
        assert!(!mgr.interrupt(id)); // already deregistered
    }

    #[test]
    fn test_interrupt_unknown_request() {
        let mgr = InterruptManager::new();
        assert!(!mgr.interrupt(9999));
    }

    #[test]
    fn test_multiple_requests_isolated() {
        let mgr = InterruptManager::new();
        let (id1, token1) = mgr.register();
        let (id2, _token2) = mgr.register();

        mgr.interrupt(id1);
        assert!(token1.is_interrupted());
        // id2 should not be affected.
        let flags = mgr.flags.lock().unwrap();
        assert!(!flags.get(&id2).unwrap().is_set());
    }

    #[test]
    fn test_check_succeeds_when_not_interrupted() {
        let mgr = InterruptManager::new();
        let (_id, token) = mgr.register();
        for _ in 0..100 {
            token.check().unwrap();
        }
    }
}
