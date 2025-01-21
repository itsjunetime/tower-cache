//! Module for supporting manual invalidation of various cache responses

use core::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, PoisonError, RwLock};

use segvec::SegVec;

use crate::CacheKey;

/// A struct which can be used to manually invalidate responses from the Cache produced by
/// [`CacheService`]. If the `axum` feature is enabled, this can be used as an [`
///
/// [`CacheService`]: crate::CacheService
#[derive(Default, Clone)]
pub struct Invalidator {
	valids: Arc<RwLock<SegVec<KeyIsValid>>>
}

type KeyIsValid = (CacheKey, Arc<AtomicBool>);

impl Invalidator {
	pub(crate) fn insert(&mut self, key: CacheKey) -> Arc<AtomicBool> {
		// [TODO] Theoretically SegVec guarantees static addresses for all of its elements, even if
		// growth happens. Can we find some way to transmute lifetimes from returned `last()` calls
		// here to make sure references are only valid as long as `Invalidator` lives but allowing
		// us to avoid the allocation of Arc? probably not, but something to think about
		let valid = Arc::new(AtomicBool::new(true));
		self.valids
			.write()
			.unwrap_or_else(PoisonError::into_inner)
			.push((key, valid.clone()));
		valid
	}

	/// Invalidate every response associated with the given [`CacheKey`]
	pub fn invalidate(&self, key: &CacheKey) {
		self.invalidate_all_with_pred(|k| k == key)
	}

	/// Invalidate every response which was sent as a response for the given path. Note that this
	/// (at time of writing) matches exactly - this only invalidates responses which were given for
	/// this *exact* path. If you want to match *roughly*, you would currently be better off using
	/// [`invalidate_all_with_pred`]
	///
	/// [`invalidate_all_with_pred`]: Invalidator::invalidate_all_with_pred
	pub fn invalidate_all_for(&self, path: &http::Uri) {
		// [TODO] check if it just contains it instead of matches exactly
		self.invalidate_all_with_pred(|key| key.1 == *path);
	}

	/// Invalidate responses based on a provided predicate which processes a [`CacheKey`] and
	/// returns a bool
	///
	/// [`CacheKey`]: crate::CacheKey
	pub fn invalidate_all_with_pred(&self, pred: impl Fn(&CacheKey) -> bool) {
		let valids = self.valids.read().unwrap_or_else(PoisonError::into_inner);
		for (_, entry) in valids.iter().filter(|(k, _)| pred(k)) {
			entry.store(false, Ordering::Relaxed);
		}
	}
}

#[cfg(feature = "axum")]
impl<T> axum::extract::FromRequestParts<T> for Invalidator
where
	Invalidator: axum::extract::FromRef<T>
{
	type Rejection = core::convert::Infallible;

	fn from_request_parts<'life0, 'life1, 'async_trait>(
		_parts: &'life0 mut http::request::Parts,
		state: &'life1 T
	) -> core::pin::Pin<
		Box<dyn core::future::Future<Output = Result<Self, Self::Rejection>> + Send + 'async_trait>
	>
	where
		Self: 'async_trait,
		'life0: 'async_trait,
		'life1: 'async_trait
	{
		let new = <Invalidator as axum::extract::FromRef<T>>::from_ref(state);
		Box::pin(async move { Ok(new) })
	}
}
