//! Work counted as it is done, so that whoever asked for it can stop it.

use alloc::vec::Vec;
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
    /// The work every meter on this thread was told of, after a stop too, and the
    /// bytes worked on outside a meter and a reader: decrypted, or parsed as entries of
    /// a cross-reference section ([`told`]). With [`EXAMINED`], which counts what
    /// readers lex, this is the work done on data, decoding, decrypting, and what a
    /// writer hashes, compares and writes counted too: what tests bound the work of a
    /// stopped task by, and the work of reading and copying a file. Debug builds only.
    #[doc(hidden)]
    pub static TOLD: Cell<usize> = const { Cell::new(0) };
}

#[cfg(all(debug_assertions, feature = "std"))]
std::thread_local! {
    /// The bytes of objects' bodies read on this thread, in the file, headers and
    /// all, or in object streams, and around streams' data found, with the file
    /// once where `endstream` is looked for in it, and the decoded data of object
    /// streams: what tests bound the work of reading objects by. Debug builds only.
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

#[cfg(all(debug_assertions, feature = "std"))]
std::thread_local! {
    /// The entries of object streams' headers read on this thread: what tests bound
    /// the work of reading object streams by, with [`BODIES_READ`], which counts
    /// their decoded data. Debug builds only.
    #[doc(hidden)]
    pub static OBJECT_STREAM_ENTRIES: Cell<usize> = const { Cell::new(0) };
}

#[cfg(all(debug_assertions, feature = "std"))]
std::thread_local! {
    /// The bytes every [`crate::reader::Reader`] on this thread looked at, one by one,
    /// a byte as often as it was looked at, by reads that failed too, whatever the
    /// bytes: the file, an object stream's data, a content stream. Counted by the
    /// reader itself, so no read is left out: what tests bound the work of lexing by.
    /// A slice a reader takes whole, unread, as a stream's data, is not counted.
    /// Debug builds only.
    #[doc(hidden)]
    pub static EXAMINED: Cell<usize> = const { Cell::new(0) };
}

#[cfg(all(debug_assertions, feature = "std"))]
std::thread_local! {
    /// The bytes of decoders' states copied on this thread to save where a reader is
    /// or to go back there ([`Meter::copying`]): what tests find a copy's polls by.
    /// Debug builds only.
    #[doc(hidden)]
    pub static STATE_COPIED: Cell<usize> = const { Cell::new(0) };
}

#[cfg(all(debug_assertions, feature = "std"))]
std::thread_local! {
    /// The bytes of decoders' states made on this thread ([`Meter::making_state`]):
    /// what tests tell the work of making decoders from that of the data by. Debug
    /// builds only.
    #[doc(hidden)]
    pub static STATE_MADE: Cell<usize> = const { Cell::new(0) };
    /// How many decoders' states [`STATE_MADE`] counts. Debug builds only.
    #[doc(hidden)]
    pub static STATES_MADE: Cell<usize> = const { Cell::new(0) };
}

#[cfg(all(debug_assertions, feature = "std"))]
std::thread_local! {
    /// The bytes of the room made on this thread by [`Meter::room`] and
    /// [`Meter::reserve`], which is filled under a meter, a counted piece at a time:
    /// what tests tell an allocation whose filling is counted from one that copies or
    /// makes its bytes uncounted by.
    /// Debug builds only.
    #[doc(hidden)]
    pub static RESERVED: Cell<usize> = const { Cell::new(0) };
}

#[cfg(all(debug_assertions, feature = "std"))]
std::thread_local! {
    /// The steps of loops that work on data done on this thread, counted where they
    /// are taken, apart from the meter they count on: a layer of a filter chain
    /// reached, a byte a decoder took or made, a row's bytes reversed. With
    /// [`hayro_ccitt::OPERATIONS`] ([`operations`]), what tests find work done
    /// between two polls by that allocates nothing. Debug builds only.
    #[doc(hidden)]
    pub static OPERATIONS: Cell<usize> = const { Cell::new(0) };
}

/// The steps counted on this thread so far ([`OPERATIONS`]), those of the CCITT
/// decoder included. Debug builds only.
#[cfg(all(debug_assertions, feature = "std"))]
#[doc(hidden)]
pub fn operations() -> usize {
    #[cfg(feature = "images")]
    let ccitt = hayro_ccitt::OPERATIONS.get();
    #[cfg(not(feature = "images"))]
    let ccitt = 0;
    OPERATIONS.get() + ccitt
}

/// The bytes of room made on this thread to be filled under a meter so far
/// ([`RESERVED`]), and those the CCITT decoder made for a line's colour changes,
/// filled a change per code read. Debug builds only.
#[cfg(all(debug_assertions, feature = "std"))]
#[doc(hidden)]
pub fn reserved() -> usize {
    #[cfg(feature = "images")]
    let ccitt = hayro_ccitt::RESERVED.get();
    #[cfg(not(feature = "images"))]
    let ccitt = 0;
    RESERVED.get() + ccitt
}

/// Counts `steps` of a loop that works on data ([`OPERATIONS`]).
#[inline(always)]
pub(crate) fn operation(_steps: usize) {
    #[cfg(all(debug_assertions, feature = "std"))]
    OPERATIONS.set(OPERATIONS.get() + _steps);
}

/// Counts a byte a reader looked at ([`EXAMINED`]).
#[inline(always)]
pub(crate) fn examined() {
    #[cfg(all(debug_assertions, feature = "std"))]
    EXAMINED.set(EXAMINED.get() + 1);
}

/// Counts `bytes` worked on outside a meter and a reader ([`TOLD`]).
#[inline]
pub(crate) fn told(_bytes: usize) {
    #[cfg(all(debug_assertions, feature = "std"))]
    TOLD.set(TOLD.get() + _bytes);
}

/// Counts `entries` of an object stream's header read ([`OBJECT_STREAM_ENTRIES`]).
#[inline]
pub(crate) fn object_stream_entries(_entries: usize) {
    #[cfg(all(debug_assertions, feature = "std"))]
    OBJECT_STREAM_ENTRIES.set(OBJECT_STREAM_ENTRIES.get() + _entries);
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

    /// Counts a decoder's state of `bytes` about to be made as the work of making it,
    /// so that making any number of decoders, over however little data, polls the
    /// hook.
    #[cfg(feature = "unsafe")]
    pub(crate) fn making_state(&self, bytes: usize) -> Result<(), Cancelled> {
        self.work(bytes)?;
        #[cfg(all(debug_assertions, feature = "std"))]
        {
            STATE_MADE.set(STATE_MADE.get() + bytes);
            STATES_MADE.set(STATES_MADE.get() + 1);
        }
        Ok(())
    }

    /// Counts the copy of a decoder's state of `bytes`, copied whole, at most
    /// [`POLL_BYTES`] so that the copy stays within the work between two polls; to
    /// be called before the copy, which a stop then saves.
    #[cfg(any(feature = "unsafe", feature = "images"))]
    pub(crate) fn copying(&self, bytes: usize) -> Result<(), Cancelled> {
        debug_assert!(bytes <= POLL_BYTES, "a state of {bytes} bytes copied whole");
        self.work(bytes)?;
        #[cfg(all(debug_assertions, feature = "std"))]
        STATE_COPIED.set(STATE_COPIED.get() + bytes);
        Ok(())
    }

    /// A copy of `from`, with no more room than it needs, made a piece of at most
    /// [`POLL_BYTES`] at a time, each counted ([`Self::copying`]) before it is
    /// copied: so a decoder's state of any size is copied within the work between two
    /// polls a piece, and a stop leaves the rest uncopied.
    #[cfg(any(feature = "unsafe", feature = "images"))]
    pub(crate) fn copied<T: Copy>(&self, from: &[T]) -> Result<Vec<T>, Cancelled> {
        let mut to = self.room(from.len());
        for piece in from.chunks((POLL_BYTES / size_of::<T>().max(1)).max(1)) {
            self.copying(size_of_val(piece))?;
            to.extend_from_slice(piece);
        }
        Ok(to)
    }

    /// Room in `buf` for `additional` more items, as [`Vec::reserve`] makes it, to be
    /// filled under this meter a counted piece at a time, as [`Self::room`]'s is. Where
    /// the items must move, the allocator moves them as it can: glibc remaps a block
    /// above its mmap threshold and grows one at the top of its heap in place, so no
    /// move is counted as work, and none holds the old block besides the new across a
    /// poll. The one way a buffer that grows with the data grows.
    pub fn reserve<T>(&self, buf: &mut Vec<T>, additional: usize) {
        let before = buf.capacity();
        buf.reserve(additional);
        self.grown((buf.capacity() - before) * size_of::<T>());
    }

    /// [`Self::reserve`], with room for `additional` more items and no more.
    pub fn reserve_exact<T>(&self, buf: &mut Vec<T>, additional: usize) {
        let before = buf.capacity();
        buf.reserve_exact(additional);
        self.grown((buf.capacity() - before) * size_of::<T>());
    }

    /// [`Self::reserve_exact`], or `Err` without allocating where the room cannot be
    /// made: for room sized from what a file declares, which may be past what a
    /// vector holds or memory can give.
    pub fn try_reserve_exact<T>(
        &self,
        buf: &mut Vec<T>,
        additional: usize,
    ) -> Result<(), alloc::collections::TryReserveError> {
        let before = buf.capacity();
        buf.try_reserve_exact(additional)?;
        self.grown((buf.capacity() - before) * size_of::<T>());
        Ok(())
    }

    /// Notes room of `bytes` made ([`RESERVED`]).
    #[allow(clippy::unused_self)]
    fn grown(&self, bytes: usize) {
        #[cfg(all(debug_assertions, feature = "std"))]
        RESERVED.set(RESERVED.get() + bytes);
        #[cfg(not(all(debug_assertions, feature = "std")))]
        let _ = bytes;
    }

    /// An empty vector with room for `capacity` items, to be filled under this meter a
    /// counted piece at a time: the room is mapped as it is written, so allocating it
    /// is no work of its own ([`RESERVED`]). The one way a buffer whose size grows
    /// with the data is allocated before it is filled.
    pub fn room<T>(&self, capacity: usize) -> Vec<T> {
        let room = Vec::with_capacity(capacity);
        self.grown(room.capacity() * size_of::<T>());
        room
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
