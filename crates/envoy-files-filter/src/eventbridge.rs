use std::sync::{Arc, Mutex};

enum Inner<T> {
    Closed,
    Empty,
    Single(T),
    Multiple(Vec<T>),
}

/// A mailbox for events flowing from I/O threads to the Envoy worker thread.
///
/// The Envoy SDK cannot deliver a payload with a scheduled event - producers
/// push the event here and then call `scheduler.commit`, and `on_scheduled`
/// drains the mailbox on the worker thread. Optimized for the common case of
/// a single pending event (no allocation). Adapted from pyvoy's EventBridge.
pub(crate) struct EventBridge<T> {
    inner: Arc<Mutex<Inner<T>>>,
}

impl<T> Clone for EventBridge<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<T> EventBridge<T> {
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner::Empty)),
        }
    }

    /// Sends an event on the bridge. Fails once the bridge is closed, which
    /// is the cancellation fence for aborted streams: the caller drops the
    /// returned event (and any owned buffers in it) on its own thread.
    pub(crate) fn send(&self, event: T) -> Result<(), T> {
        let mut inner = self.inner.lock().unwrap();
        match *inner {
            Inner::Closed => {
                return Err(event);
            }
            Inner::Empty => {
                *inner = Inner::Single(event);
            }
            Inner::Single(_) => {
                let Inner::Single(first) = std::mem::replace(&mut *inner, Inner::Empty) else {
                    unreachable!()
                };
                *inner = Inner::Multiple(vec![first, event]);
            }
            Inner::Multiple(ref mut events) => {
                events.push(event);
            }
        }
        Ok(())
    }

    /// Drains all pending events, calling `f` for each. The lock is released
    /// before `f` runs so producers are never blocked on processing.
    pub(crate) fn process(&self, mut f: impl FnMut(T)) {
        let mut inner = self.inner.lock().unwrap();
        match std::mem::replace(&mut *inner, Inner::Empty) {
            Inner::Closed | Inner::Empty => {}
            Inner::Single(event) => {
                drop(inner);
                f(event);
            }
            Inner::Multiple(events) => {
                drop(inner);
                for event in events {
                    f(event);
                }
            }
        }
    }

    /// Closes the bridge. Further `send` calls fail.
    pub(crate) fn close(&self) {
        let mut inner = self.inner.lock().unwrap();
        *inner = Inner::Closed;
    }
}
