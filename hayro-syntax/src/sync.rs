#[cfg(feature = "std")]
pub(crate) use std::collections::HashMap;

#[cfg(not(feature = "std"))]
pub(crate) use alloc::collections::BTreeMap as HashMap;

#[cfg(feature = "std")]
pub(crate) use rustc_hash::{FxHashMap, FxHashSet};

#[cfg(not(feature = "std"))]
pub(crate) use alloc::collections::BTreeMap as FxHashMap;

#[cfg(not(feature = "std"))]
pub(crate) use alloc::collections::BTreeSet as FxHashSet;

// Keep in sync with the implementation in `page`.
#[cfg(feature = "std")]
pub(crate) use std::sync::Arc;

#[cfg(not(feature = "std"))]
pub(crate) use alloc::rc::Rc as Arc;

#[cfg(feature = "std")]
pub(crate) use std::sync::OnceLock;

#[cfg(not(feature = "std"))]
pub(crate) use core::cell::OnceCell as OnceLock;

#[cfg(feature = "std")]
pub(crate) use std::sync::Mutex;

#[cfg(not(feature = "std"))]
pub(crate) use core::cell::RefCell as Mutex;

#[cfg(feature = "std")]
pub(crate) type MutexGuard<'a, T> = std::sync::MutexGuard<'a, T>;

#[cfg(not(feature = "std"))]
pub(crate) type MutexGuard<'a, T> = core::cell::RefMut<'a, T>;

pub(crate) trait MutexExt<T> {
    fn get(&self) -> MutexGuard<'_, T>;
}

#[cfg(feature = "std")]
impl<T> MutexExt<T> for Mutex<T> {
    fn get(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap()
    }
}

#[cfg(not(feature = "std"))]
impl<T> MutexExt<T> for Mutex<T> {
    fn get(&self) -> MutexGuard<'_, T> {
        self.borrow_mut()
    }
}
