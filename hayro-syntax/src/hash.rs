//! Document maps and sets with randomized hashing under std.

/// A map of keys to values.
#[cfg(feature = "std")]
#[allow(clippy::disallowed_types, reason = "the one definition of a map")]
pub type Map<K, V> = std::collections::HashMap<K, V, State>;

/// A set of keys.
#[cfg(feature = "std")]
#[allow(clippy::disallowed_types, reason = "the one definition of a set")]
pub type Set<K> = std::collections::HashSet<K, State>;

/// Per-map randomized hash builder.
#[cfg(feature = "std")]
pub type State = foldhash::fast::RandomState;

/// A map of keys to values.
#[cfg(not(feature = "std"))]
pub type Map<K, V> = alloc::collections::BTreeMap<K, V>;

/// A set of keys.
#[cfg(not(feature = "std"))]
pub type Set<K> = alloc::collections::BTreeSet<K>;
