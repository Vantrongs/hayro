//! The maps and sets of every crate of hayro: the one way to make one.
//!
//! A file chooses most keys, names and numbers alike, and a hash function whose
//! keys are fixed lets it choose many that hash alike, which makes each insertion
//! and lookup cost as much as all the colliding keys before it. So every map is
//! hashed with keys drawn at random for each map ([`foldhash::fast::RandomState`]),
//! which a file, written before the process ran, cannot know. Without `std`, where
//! no randomness is at hand, maps are ordered ones, whose cost no key can raise.
//!
//! The order a hashed map iterates in differs from map to map and run to run, so
//! what is read or written in its order is sorted first.

/// A map of keys to values.
#[cfg(feature = "std")]
#[allow(clippy::disallowed_types, reason = "the one definition of a map")]
pub type Map<K, V> = std::collections::HashMap<K, V, State>;

/// A set of keys.
#[cfg(feature = "std")]
#[allow(clippy::disallowed_types, reason = "the one definition of a set")]
pub type Set<K> = std::collections::HashSet<K, State>;

/// What a [`Map`] or [`Set`] hashes with: a hasher keyed at random for each value,
/// also for hashing data outside a map, such as a key's part
/// ([`core::hash::BuildHasher::hash_one`]).
#[cfg(feature = "std")]
pub type State = foldhash::fast::RandomState;

/// A map of keys to values.
#[cfg(not(feature = "std"))]
pub type Map<K, V> = alloc::collections::BTreeMap<K, V>;

/// A set of keys.
#[cfg(not(feature = "std"))]
pub type Set<K> = alloc::collections::BTreeSet<K>;

/// An entry of a [`Map`].
#[cfg(feature = "std")]
pub(crate) use std::collections::hash_map::Entry;

/// An entry of a [`Map`].
#[cfg(not(feature = "std"))]
pub(crate) use alloc::collections::btree_map::Entry;
