#![feature(mapped_lock_guards)]
#![doc = include_str!("../README.md")]

use core::sync::atomic::{AtomicBool, Ordering};
use std::{
	future::Future,
	marker::PhantomData,
	pin::Pin,
	sync::Arc,
	task::{Context, Poll}
};

use http::{Method, Request, Uri};
use options::{CacheOptions, Predicate};
use pin_project_lite::pin_project;
use tower_layer::Layer;
use tower_service::Service;

pub mod cache;
pub mod invalidator;
pub mod options;

// ok idea for invalidation:
// - Nothing exists on startup or layer/service creation, basically. But every service that is
//	 created gets given an `Arc<Mutex<Trie>>` (or similar data structure) that they clone and store
// - The first time each service gets a request, they will service the request, but also check that
//	 data structure and insert some data into it - specifically, they'll make `AtomicBool`s for
//	 each section of their path and set them to `false` to indicate that the cache for that path
//	 hasn't yet been invalidated. If a bool already exists for another section of their path,
//	 they'll just add another one into the vec (maybe use like `SmallVec` to indicate that multiple
//	 services are listening on that path.
// - They will then clone those atomic bools (they'll be wrapped in Arcs) and store them to check
//	 them later.
// - They will then drop the `Arc` of the initial data structure, since they've inserted their data
//	 into it and it won't change anymore.
// - Then, each time they service a request, they check if any of their atomic bools has been set
//	 to `true`. If so, they invalidate their cache and set the bools back to `false` to indicate
//	 their cache has been invalidated.
//
// issues:
//	- this doesn't work with multi-host systems. assumes everything's on the same host.
//
// ok actually, just do a `Vec` that stores each path along with like an `AtomicU64` and each time
// you invalidate a cache at a given path, just clear the cache of everything that starts with the
// given path.

/// The [`tower_layer::Layer`] implementor that enables cache functionality.
///
/// # Generics
///
/// - `Cache`: implementor of [`cache::Cache`] which will be created on a per-service basis to
///   cache the response per-service.
/// - `Resp`, `RespPred`, `Req`, `ReqPred`: See the [`options::CacheOptions`] documentation
pub struct CacheLayer<Cache, Resp, RespPred, Req, ReqPred>
where
	RespPred: Predicate<Resp>,
	ReqPred: Predicate<Req>
{
	options: CacheOptions<Resp, RespPred, Req, ReqPred>,
	_tys: PhantomData<(Req, Cache)>
}

impl<Cache, Resp, RespPred, Req, ReqPred> CacheLayer<Cache, Resp, RespPred, Req, ReqPred>
where
	RespPred: Predicate<Resp>,
	ReqPred: Predicate<Req>
{
	/// Create a new [`CacheLayer`] with the given options
	pub fn new(options: CacheOptions<Resp, RespPred, Req, ReqPred>) -> Self {
		Self {
			options,
			_tys: PhantomData
		}
	}
}

impl<Cache, RespPred, Req, ReqPred, Svc> Layer<Svc>
	for CacheLayer<Cache, Svc::Response, RespPred, Req, ReqPred>
where
	Svc: Service<Req>,
	Svc::Response: Clone,
	Cache: cache::Cache<CacheKey, Svc::Response>,
	RespPred: Predicate<Svc::Response>,
	ReqPred: Predicate<Req>
{
	type Service = CacheService<Cache, RespPred, Req, ReqPred, Svc>;

	fn layer(&self, inner: Svc) -> Self::Service {
		CacheService {
			inner,
			cache: Cache::default(),
			registered_with_invalidator: Vec::new(),
			options: self.options.clone()
		}
	}
}

/// The implementor of [`tower_service::Service`]; handles evaluating the stored predicates and
/// actually storing the responses of the underlying Service in its [`cache::Cache`]
///
/// # Generics
///
/// - `Cache`: implementor of [`cache::Cache`] which actually stores the responses within this
///   struct
/// - `Svc`: Underlying [`tower_service::Service`] which this struct will call into to get a
///   response which it may or may not cache.
/// - `RespPred`, `Req`, `ReqPred`: See [`options::CacheOptions`] documentation. The missing `Resp`
///   type is just [`Svc::Response`](tower_service::Service::Response)
pub struct CacheService<Cache, RespPred, Req, ReqPred, Svc>
where
	RespPred: Predicate<Svc::Response>,
	ReqPred: Predicate<Req>,
	Svc: Service<Req>
{
	inner: Svc,
	cache: Cache,
	registered_with_invalidator: Vec<(CacheKey, Arc<AtomicBool>)>,
	options: CacheOptions<Svc::Response, RespPred, Req, ReqPred>
}

impl<Cache, RespPred, Req, ReqPred, Svc> Service<Request<Req>>
	for CacheService<Cache, RespPred, Request<Req>, ReqPred, Svc>
where
	Svc: Service<Request<Req>>,
	Svc::Response: Clone,
	Svc::Error: Clone,
	Svc::Future: Future,
	Cache: cache::Cache<CacheKey, Svc::Response>,
	RespPred: Predicate<Svc::Response>,
	ReqPred: Predicate<Request<Req>>
{
	type Response = Svc::Response;
	type Error = Svc::Error;
	type Future = CachingFut<Cache, Svc::Error, Svc::Future, Svc::Response, RespPred>;

	fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
		self.inner.poll_ready(cx)
	}

	fn call(&mut self, req: Request<Req>) -> Self::Future {
		let key = (req.method().clone(), req.uri().clone());

		let invalidated = self
			.registered_with_invalidator
			.iter()
			.find(|(k, _)| *k == key)
			.map(|(_, valid)| valid);

		match invalidated {
			Some(valid) =>
				if !valid.load(Ordering::Relaxed) {
					self.cache.remove(&key);
					valid.store(true, Ordering::Relaxed);
				},
			None => {
				let valid = self.options.invalidator.insert(key.clone());
				self.registered_with_invalidator.push((key.clone(), valid));
			}
		}

		let fulfilled_req_pred = self
			.options
			.req_pred
			.as_ref()
			.is_none_or(|p| p.fulfills(&req));

		CachingFut {
			key,
			fut: self.inner.call(req),
			cache: self.cache.clone(),
			resp_pred: self.options.resp_pred.clone(),
			fulfilled_req_pred,
			_phantom: PhantomData
		}
	}
}

type CacheKey = (Method, Uri);

pin_project! {
	/// The [`Future`] returned by [`<CacheService as Service>::call`]
	pub struct CachingFut<Cache, Err, Fut, Resp, RespPred>
	where
		Fut: Future<Output = Result<Resp, Err>>,
		Resp: Clone,
		Cache: cache::Cache<CacheKey, Resp>,
		RespPred: Predicate<Resp>,
	{
		key: CacheKey,
		#[pin]
		fut: Fut,
		cache: Cache,
		resp_pred: Option<Arc<RespPred>>,
		fulfilled_req_pred: bool,
		_phantom: PhantomData<Err>
	}
}

impl<Cache, Err, Fut, Resp, RespPred> Future for CachingFut<Cache, Err, Fut, Resp, RespPred>
where
	Fut: Future<Output = Result<Resp, Err>>,
	Resp: Clone,
	Cache: cache::Cache<CacheKey, Resp>,
	RespPred: Predicate<Resp>
{
	type Output = Fut::Output;

	fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
		if let Some(resp) = self.cache.get(&self.key) {
			return Poll::Ready(Ok((*resp).clone()));
		}

		let proj = self.project();

		let Poll::Ready(resp) = proj.fut.poll(cx) else {
			return Poll::Pending;
		};

		if let Ok(ref r) = resp {
			if *proj.fulfilled_req_pred && proj.resp_pred.as_ref().is_none_or(|p| p.fulfills(r)) {
				proj.cache.insert_if_not_exists(proj.key.clone(), r.clone());
			}
		}

		Poll::Ready(resp)
	}
}
