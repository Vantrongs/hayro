//! Work counted as it is done, so that whoever asked for it can stop it.

use core::cell::Cell;

/// The work, in bytes, after which a [`Meter`] polls its hook again.
pub const POLL_BYTES: usize = 64 * 1024;

/// Counts the work a task does, in bytes read, produced or converted, and polls a
/// hook each time another [`POLL_BYTES`] of it are done. Every loop that works on
/// data counts each step, so that a task stops within a bounded amount of work of
/// its hook asking it to, whatever the data.
pub struct Meter<'a> {
    hook: Option<&'a dyn Fn() -> bool>,
    /// The work left before the next poll; 0 once the hook asked to stop.
    left: Cell<usize>,
    cancelled: Cell<bool>,
}

#[cfg(all(debug_assertions, feature = "std"))]
std::thread_local! {
    /// The work every meter on this thread was told of, after a stop too: what tests
    /// bound the work of a stopped task by. Debug builds only.
    #[doc(hidden)]
    pub static TOLD: Cell<usize> = const { Cell::new(0) };
}

#[cfg(all(debug_assertions, feature = "std"))]
std::thread_local! {
    /// The bytes of objects' bodies read on this thread, in the file, headers and
    /// all, or in object streams, and around streams' data found, with the file
    /// once where `endstream` is looked for in it: what tests bound the work of
    /// reading objects by. Debug builds only.
    #[doc(hidden)]
    pub static BODIES_READ: Cell<usize> = const { Cell::new(0) };
}

#[cfg(all(debug_assertions, feature = "std"))]
std::thread_local! {
    /// The bytes of arrays' items and dictionaries' values read from their bytes on
    /// this thread, which no read of an object's body does, as it reads what the
    /// body was parsed into once: what tests bound the work of reading what objects
    /// hold by, with [`BODIES_READ`]. Debug builds only.
    #[doc(hidden)]
    pub static LEXED: Cell<usize> = const { Cell::new(0) };
}

/// Counts `bytes` of arrays' items or dictionaries' values read ([`LEXED`]).
#[inline]
pub(crate) fn lexed(_bytes: usize) {
    #[cfg(all(debug_assertions, feature = "std"))]
    LEXED.set(LEXED.get() + _bytes);
}

/// Counts `bytes` of objects' bodies read ([`BODIES_READ`]).
#[inline]
pub(crate) fn read_bodies(_bytes: usize) {
    #[cfg(all(debug_assertions, feature = "std"))]
    BODIES_READ.set(BODIES_READ.get() + _bytes);
}

/// The hook of a [`Meter`] asked to stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cancelled;

impl<'a> Meter<'a> {
    /// A meter polling `hook`, which returns true to stop; with `None`, the work is
    /// counted but never stopped.
    pub fn new(hook: Option<&'a dyn Fn() -> bool>) -> Self {
        Self {
            hook,
            left: Cell::new(POLL_BYTES),
            cancelled: Cell::new(false),
        }
    }

    /// Polls the hook now; once it asked to stop, every later poll and count fails
    /// without polling it again.
    pub fn poll(&self) -> Result<(), Cancelled> {
        if self.cancelled.get() {
            return Err(Cancelled);
        }
        if self.hook.is_some_and(|hook| hook()) {
            self.cancelled.set(true);
            self.left.set(0);
            return Err(Cancelled);
        }
        self.left.set(POLL_BYTES);
        Ok(())
    }

    /// Counts `bytes` of work done, and polls the hook when it is due.
    #[inline]
    pub fn work(&self, bytes: usize) -> Result<(), Cancelled> {
        #[cfg(all(debug_assertions, feature = "std"))]
        TOLD.set(TOLD.get() + bytes);
        let left = self.left.get();
        if bytes < left {
            self.left.set(left - bytes);
            Ok(())
        } else {
            self.poll()
        }
    }

    /// The work that can be done before the hook is polled again; 0 once it asked to
    /// stop.
    #[cfg(feature = "unsafe")]
    pub(crate) fn until_poll(&self) -> usize {
        self.left.get()
    }

    /// Whether the hook asked to stop, for work that reports stopping as failure.
    pub fn cancelled(&self) -> bool {
        self.cancelled.get()
    }
}

impl Default for Meter<'_> {
    fn default() -> Self {
        Self::new(None)
    }
}

#[cfg(feature = "images")]
impl zune_jpeg::progress::Progress for Meter<'_> {
    fn advance(&self, work: usize) -> bool {
        self.work(work).is_err()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hook_is_polled_after_each_poll_bytes_and_a_stop_sticks() {
        let polls = Cell::new(0);
        let hook = || {
            polls.set(polls.get() + 1);
            polls.get() == 3
        };
        let meter = Meter::new(Some(&hook));
        meter.work(POLL_BYTES - 1).unwrap();
        assert_eq!(polls.get(), 0);
        meter.work(1).unwrap();
        assert_eq!(polls.get(), 1);
        meter.work(3 * POLL_BYTES).unwrap();
        assert_eq!(polls.get(), 2);
        assert_eq!(meter.work(POLL_BYTES), Err(Cancelled));
        assert!(meter.cancelled());
        assert_eq!(meter.work(0), Err(Cancelled));
        assert_eq!(meter.poll(), Err(Cancelled));
        assert_eq!(polls.get(), 3);
    }
}
