//! Counts allocation requests and bytes on the test thread.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

/// What a measured closure allocated.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Allocations {
    /// The requests, reallocations included.
    pub(crate) count: usize,
    /// The bytes requested in all, which grows with the work that makes values.
    pub(crate) allocated: usize,
}

thread_local! {
    static TALLY: Cell<Option<Allocations>> = const { Cell::new(None) };
}

fn record(allocated: usize) {
    TALLY.with(|slot| {
        if let Some(mut t) = slot.get() {
            t.count += usize::from(allocated != 0);
            t.allocated += allocated;
            slot.set(Some(t));
        }
    });
}

/// Runs `f` and reports what it allocated on this thread.
pub(crate) fn measure<T>(f: impl FnOnce() -> T) -> (T, Allocations) {
    TALLY.with(|slot| slot.set(Some(Allocations::default())));
    let result = f();
    let tally = TALLY.with(|slot| slot.replace(None)).expect("measuring");
    (result, tally)
}

struct Measuring;

// SAFETY: forwards to `System`, counting successful requests.
unsafe impl GlobalAlloc for Measuring {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller's contract is `System`'s.
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            record(layout.size());
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: as for `alloc`.
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            record(layout.size());
        }
        ptr
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        // SAFETY: as for `alloc`; `ptr` came from this allocator.
        let new = unsafe { System.realloc(ptr, layout, size) };
        if !new.is_null() {
            record(size);
        }
        new
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` came from this allocator with `layout`.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Measuring = Measuring;
