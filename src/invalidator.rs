//! Module for supporting manual invalidation of various cache responses

use core::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, PoisonError, RwLock};

use segvec::SegVec;

use crate::CacheKey;

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

	pub fn invalidate(&self, key: &CacheKey) {
		self.invalidate_all_with_pred(|k| k == key)
	}

	pub fn invalidate_all_for(&self, path: &http::Uri) {
		// [TODO] check if it just contains it instead of matches exactly
		self.invalidate_all_with_pred(|key| key.1 == *path);
	}

	pub fn invalidate_all_with_pred(&self, pred: impl Fn(&CacheKey) -> bool) {
		let valids = self.valids.read().unwrap_or_else(PoisonError::into_inner);
		for (_, entry) in valids.iter().filter(|(k, _)| pred(k)) {
			entry.store(false, Ordering::Relaxed);
		}
	}
}

#[cfg(feature = "axum-core")]
impl<T> axum_core::extract::FromRequestParts<T> for Invalidator
where
	Invalidator: axum_core::extract::FromRef<T>
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
		let new = <Invalidator as axum_core::extract::FromRef<T>>::from_ref(state);
		Box::pin(async move { Ok(new) })
	}
}
