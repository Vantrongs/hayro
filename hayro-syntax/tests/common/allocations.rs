//! A global allocator that measures what a closure allocates on its own thread, and
//! refuses any single request over 1 GiB so that code sizing a buffer by declared
//! dimensions aborts the test instead of exhausting the machine.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

/// What a measured closure allocated.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Allocations {
    /// Most bytes it held at once.
    pub(crate) peak: usize,
    /// The largest single request.
    pub(crate) largest: usize,
    /// The requests, reallocations included.
    pub(crate) count: usize,
}

#[derive(Clone, Copy, Default)]
struct Tally {
    live: isize,
    total: Allocations,
}

thread_local! {
    static TALLY: Cell<Option<Tally>> = const { Cell::new(None) };
}

fn record(allocated: usize, freed: usize) {
    TALLY.with(|slot| {
        if let Some(mut t) = slot.get() {
            t.live += allocated as isize - freed as isize;
            t.total.peak = t.total.peak.max(t.live.max(0) as usize);
            t.total.largest = t.total.largest.max(allocated);
            t.total.count += usize::from(allocated != 0);
            slot.set(Some(t));
        }
    });
}

/// Runs `f` and reports what it allocated on this thread.
pub(crate) fn measure<T>(f: impl FnOnce() -> T) -> (T, Allocations) {
    TALLY.with(|slot| slot.set(Some(Tally::default())));
    let result = f();
    let tally = TALLY.with(|slot| slot.replace(None)).expect("measuring");
    (result, tally.total)
}

const REFUSED: usize = 1 << 30;

struct Measuring;

// SAFETY: forwards to `System`, only counting and refusing oversized requests.
unsafe impl GlobalAlloc for Measuring {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if layout.size() > REFUSED {
            return std::ptr::null_mut();
        }
        // SAFETY: the caller's contract is `System`'s.
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            record(layout.size(), 0);
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if layout.size() > REFUSED {
            return std::ptr::null_mut();
        }
        // SAFETY: as for `alloc`.
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            record(layout.size(), 0);
        }
        ptr
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        if size > REFUSED {
            return std::ptr::null_mut();
        }
        // SAFETY: as for `alloc`; `ptr` came from this allocator.
        let new = unsafe { System.realloc(ptr, layout, size) };
        if !new.is_null() {
            record(size, layout.size());
        }
        new
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        record(0, layout.size());
        // SAFETY: `ptr` came from this allocator with `layout`.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Measuring = Measuring;
