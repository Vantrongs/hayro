//! Reporting and replaying unreadable listed objects.

use crate::hash;
use crate::object::ObjectIdentifier;
use crate::xref::XRef;
type WatchStack = smallvec::SmallVec<[(usize, Lost); 8]>;

/// The first unreadable listed object encountered during a computation.
pub type Lost = Option<ObjectIdentifier>;

/// Cached results and the first unreadable object encountered for each key.
pub struct Memo<K, V>(hash::Map<K, (V, Lost)>);

impl<K, V> Default for Memo<K, V> {
    fn default() -> Self {
        Self(hash::Map::default())
    }
}

impl<K: core::hash::Hash + Ord, V> Memo<K, V> {
    /// Compute the value once and report its recorded loss on every lookup.
    pub fn get(&mut self, xref: &XRef, key: K, compute: impl FnOnce() -> V) -> &V {
        let (value, lost) = self.0.entry(key).or_insert_with(|| xref.apart(compute));
        xref.note(*lost);
        value
    }
}

#[derive(Default)]
pub(crate) struct Watched {
    identity: u8,
    #[cfg(not(feature = "std"))]
    stack: core::cell::RefCell<WatchStack>,
}

#[cfg(feature = "std")]
std::thread_local! {
    static WATCHED: core::cell::RefCell<WatchStack> =
        core::cell::RefCell::new(WatchStack::new());
}

impl Watched {
    fn with<T>(&self, f: impl FnOnce(&mut WatchStack) -> T) -> T {
        #[cfg(feature = "std")]
        return WATCHED.with_borrow_mut(f);
        #[cfg(not(feature = "std"))]
        return f(&mut self.stack.borrow_mut());
    }

    pub(crate) fn watch<T>(&self, around: bool, f: impl FnOnce() -> T) -> (T, Lost) {
        struct End<'a>(&'a Watched);
        impl Drop for End<'_> {
            fn drop(&mut self) {
                self.0.with(|stack| stack.pop());
            }
        }

        self.with(|stack| stack.push((core::ptr::addr_of!(self.identity) as usize, None)));
        let end = End(self);
        let value = f();
        core::mem::forget(end);
        let lost = self.with(|stack| stack.pop()).and_then(|(_, lost)| lost);
        if around {
            self.note(lost);
        }
        (value, lost)
    }

    pub(crate) fn note(&self, lost: Lost) {
        if let Some(id) = lost {
            self.with(|stack| {
                if let Some((_, top)) = stack
                    .iter_mut()
                    .rev()
                    .find(|(doc, _)| *doc == core::ptr::addr_of!(self.identity) as usize)
                {
                    top.get_or_insert(id);
                }
            });
        }
    }
}
