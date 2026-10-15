//! What reading lost: an object the table of a file lists but that cannot be read.
//!
//! A reference to such an object reads as one to nothing, `null`, as viewers read it,
//! however it is read: what the object held is lost, not absent. Every read of a
//! reference goes through the table ([`XRef`]), which tells the computation being
//! watched ([`XRef::watch`]) each such object it meets, so that a computation whose
//! result must not lose what the file holds, such as a copy of a page, can refuse
//! a result read without it, whichever way its reads were made.
//!
//! What is computed once and kept for later computations, as a cache does, tells
//! each of them what computing it lost, as if it had been read again ([`Memo`],
//! [`XRef::note`]).

use crate::object::ObjectIdentifier;
use crate::sync::FxHashMap;
use crate::xref::XRef;
use alloc::vec::Vec;

/// The first object a computation met that the file lists but that cannot be read,
/// if it met one.
pub type Lost = Option<ObjectIdentifier>;

/// Values computed once for the keys asked for, each with what computing it lost,
/// which every later ask is told as the first was ([`XRef::note`]): a value read
/// without an object is never taken for whole because it was read before.
pub struct Memo<K, V>(FxHashMap<K, (V, Lost)>);

impl<K, V> Default for Memo<K, V> {
    fn default() -> Self {
        Self(FxHashMap::default())
    }
}

impl<K: core::hash::Hash + Ord, V> Memo<K, V> {
    /// The value for `key`, computed by `compute` the first time, watched in `xref`;
    /// what computing it lost is told to the computation asking, now and every time.
    pub fn get(&mut self, xref: &XRef, key: K, compute: impl FnOnce() -> V) -> &V {
        let (value, lost) = self.0.entry(key).or_insert_with(|| xref.watch(compute));
        xref.note(*lost);
        value
    }
}

/// The computations being watched, innermost last, each with what it lost so far.
///
/// With `std` they are those of the thread, as reads are made on the thread that
/// asks for them; without, the document's, which is not shared between threads.
#[derive(Default)]
pub(crate) struct Watched {
    #[cfg(not(feature = "std"))]
    stack: core::cell::RefCell<Vec<Lost>>,
}

#[cfg(feature = "std")]
std::thread_local! {
    static WATCHED: core::cell::RefCell<Vec<Lost>> =
        const { core::cell::RefCell::new(Vec::new()) };
}

impl Watched {
    fn with<T>(&self, f: impl FnOnce(&mut Vec<Lost>) -> T) -> T {
        #[cfg(feature = "std")]
        return WATCHED.with_borrow_mut(f);
        #[cfg(not(feature = "std"))]
        return f(&mut self.stack.borrow_mut());
    }

    /// Runs `f` as a computation of its own, and returns what it lost; where
    /// `around`, the computation around it, if any, is told too.
    pub(crate) fn watch<T>(&self, around: bool, f: impl FnOnce() -> T) -> (T, Lost) {
        /// Ends the computation if `f` unwinds.
        struct End<'a>(&'a Watched);
        impl Drop for End<'_> {
            fn drop(&mut self) {
                self.0.with(|stack| stack.pop());
            }
        }

        self.with(|stack| stack.push(None));
        let end = End(self);
        let value = f();
        core::mem::forget(end);
        let lost = self.with(|stack| stack.pop()).flatten();
        if around {
            self.note(lost);
        }
        (value, lost)
    }

    /// Tells the innermost computation that it lost `lost`.
    pub(crate) fn note(&self, lost: Lost) {
        if let Some(id) = lost {
            self.with(|stack| {
                if let Some(top) = stack.last_mut() {
                    top.get_or_insert(id);
                }
            });
        }
    }
}
