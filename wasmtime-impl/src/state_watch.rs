//! A tiny state watch: the latest value plus a version counter, with
//! poll-based change notification.
//!
//! Backs the `state-changes` streams: producers are demand-driven
//! (`poll_changed` registers the caller's waker), so an element reflects the
//! state at the time it is produced — the coalescing-watch contract the WIT
//! specifies. Once a terminal value is set, later sets are ignored, so no
//! state can ever be observed after a terminal one.

use std::sync::Mutex;
use std::task::{Context, Poll, Waker};

/// The latest value of a state machine, observable by version.
pub(crate) struct StateWatch<T> {
    inner: Mutex<Inner<T>>,
    /// Whether a value is terminal: once set, the watch stops changing.
    is_terminal: fn(&T) -> bool,
}

struct Inner<T> {
    value: T,
    version: u64,
    wakers: Vec<Waker>,
}

impl<T: Copy + PartialEq> StateWatch<T> {
    /// A watch starting at `initial`; `is_terminal` marks the values after
    /// which the watch stops changing.
    pub(crate) fn new(initial: T, is_terminal: fn(&T) -> bool) -> Self {
        Self {
            inner: Mutex::new(Inner {
                value: initial,
                version: 0,
                wakers: Vec::new(),
            }),
            is_terminal,
        }
    }

    /// Set the current value, waking watchers. No-ops when the value is
    /// unchanged or the current value is terminal.
    pub(crate) fn set(&self, value: T) {
        let mut inner = self.inner.lock().unwrap();
        if inner.value == value || (self.is_terminal)(&inner.value) {
            return;
        }
        inner.value = value;
        inner.version += 1;
        for waker in inner.wakers.drain(..) {
            waker.wake();
        }
    }

    /// The current value and its version.
    pub(crate) fn current(&self) -> (T, u64) {
        let inner = self.inner.lock().unwrap();
        (inner.value, inner.version)
    }

    /// Resolve with the current `(value, version)` once the version differs
    /// from `seen`; otherwise register the caller's waker and stay pending.
    pub(crate) fn poll_changed(&self, seen: u64, cx: &mut Context<'_>) -> Poll<(T, u64)> {
        let mut inner = self.inner.lock().unwrap();
        if inner.version != seen {
            return Poll::Ready((inner.value, inner.version));
        }
        if !inner.wakers.iter().any(|w| w.will_wake(cx.waker())) {
            inner.wakers.push(cx.waker().clone());
        }
        Poll::Pending
    }

    /// Whether the current value is terminal.
    pub(crate) fn is_terminal_now(&self) -> bool {
        (self.is_terminal)(&self.inner.lock().unwrap().value)
    }
}
