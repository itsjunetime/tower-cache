//! Basic traits and implementations of [`self::Cache`] for usage with each handler

use std::{
	ops::{Deref, DerefMut},
	sync::{
		Arc, MappedRwLockReadGuard, MappedRwLockWriteGuard, PoisonError, RwLock, RwLockReadGuard,
		RwLockWriteGuard
	}
};

/// A basic cache trait, with only the necessary methods and types to make our service work.
pub trait Cache<K, V>: Default + Clone + Send + 'static {
	/// Because Self must be [`Send`] (to share the cache across futures and process requests and
	/// responses), we generally need a type that represents a borrowed guard that generally will
	/// 'unlock' self when it's dropped (e.g. [`RwLockReadGuard`])
	type GetGuard<'guard>: Deref<Target = V>
	where
		V: 'guard;

	/// Get the value for `key`, returning [`None`] if the value doesn't exist in the cache.
	/// Callers should assume that `self` is locked until the return value is dropped (if it is
	/// [`Some`]), as that's how most [`Send`]-implementing map/caches work.
	fn get<'s>(&'s self, key: &K) -> Option<Self::GetGuard<'s>>;

	/// The guard type which wraps a mutable reference to a value in the cache, akin to
	/// [`RwLockWriteGuard`]. Returned from [`get_mut`]
	///
	/// [`get_mut`]: Cache::get_mut
	type GetMutGuard<'guard>: DerefMut<Target = V>
	where
		V: 'guard;

	/// Potentially get a mutable reference to entry in the cache. This will return [`None`] if
	/// such an entry doesn't exist, but will return the guard if it does exist.
	fn get_mut<'s>(&'s mut self, key: &K) -> Option<Self::GetMutGuard<'s>>;

	/// Insert the value `val` for key `key` in the cache.
	fn insert_if_not_exists(&mut self, key: K, val: V);

	/// Remove the stored value associated with the key `key`, returning it if there as one.
	fn remove(&mut self, key: &K) -> Option<V>;
}

/// A basic linear map - really just a wrapper for an [`Arc`]`<`[`RwLock`]`<`[`Vec`]`<(K, V)>>>`
pub struct SendLinearMap<K, V>(Arc<RwLock<Vec<(K, V)>>>)
where
	K: Eq;

impl<K: Eq, V> Default for SendLinearMap<K, V> {
	fn default() -> Self {
		Self(Arc::default())
	}
}

impl<K: Eq, V> Clone for SendLinearMap<K, V> {
	fn clone(&self) -> Self {
		Self(self.0.clone())
	}
}

impl<K: Eq, V> Cache<K, V> for SendLinearMap<K, V>
where
	Self: Send + 'static
{
	type GetGuard<'guard>
		= MappedRwLockReadGuard<'guard, V>
	where
		V: 'guard;

	fn get<'s>(&'s self, key: &K) -> Option<Self::GetGuard<'s>> {
		RwLockReadGuard::filter_map(
			self.0.read().unwrap_or_else(PoisonError::into_inner),
			|cache| cache.iter().find(|(k, _)| k == key).map(|(_, v)| v)
		)
		.ok()
	}

	type GetMutGuard<'guard>
		= MappedRwLockWriteGuard<'guard, V>
	where
		V: 'guard;

	fn get_mut<'s>(&'s mut self, key: &K) -> Option<Self::GetMutGuard<'s>> {
		RwLockWriteGuard::filter_map(
			self.0.write().unwrap_or_else(PoisonError::into_inner),
			|cache| cache.iter_mut().find(|(k, _)| k == key).map(|(_, v)| v)
		)
		.ok()
	}

	fn insert_if_not_exists(&mut self, key: K, val: V) {
		let mut cache = self.0.write().unwrap_or_else(PoisonError::into_inner);
		if !cache.iter().any(|(k, _)| key == *k) {
			cache.push((key, val));
		}
	}

	fn remove(&mut self, key: &K) -> Option<V> {
		let mut cache = self.0.write().unwrap_or_else(PoisonError::into_inner);
		cache
			.iter()
			.position(|(k, _)| k == key)
			.map(|idx| cache.remove(idx).1)
	}
}

#[cfg(feature = "dashmap")]
impl<K, V> Cache<K, V> for dashmap::DashMap<K, V>
where
	K: Eq + std::hash::Hash + Clone,
	V: Clone,
	Self: Send + 'static
{
	type GetGuard<'guard>
		= dashmap::mapref::one::Ref<'guard, K, V>
	where
		V: 'guard;

	fn get<'s>(&'s self, key: &K) -> Option<Self::GetGuard<'s>> {
		dashmap::DashMap::get(self, key)
	}

	type GetMutGuard<'guard>
		= dashmap::mapref::one::RefMut<'guard, K, V>
	where
		V: 'guard;

	fn get_mut<'s>(&'s mut self, key: &K) -> Option<Self::GetMutGuard<'s>> {
		dashmap::DashMap::get_mut(self, key)
	}

	fn insert_if_not_exists(&mut self, key: K, val: V) {
		if let dashmap::Entry::Vacant(entry) = self.entry(key) {
			drop(entry.insert(val));
		}
	}

	fn remove(&mut self, key: &K) -> Option<V> {
		dashmap::DashMap::remove(self, key).map(|(_, v)| v)
	}
}
