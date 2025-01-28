#![feature(mapped_lock_guards)]
#![doc = include_str!("../README.md")]

use core::{
	future::Future,
	marker::PhantomData,
	pin::Pin,
	sync::atomic::{AtomicBool, Ordering},
	task::{Context, Poll}
};
use std::sync::{atomic::AtomicU32, Arc, PoisonError, RwLock};

use body::{CacheStreamBody, CachedBody, MaybeCachedBody, NoOpBody};
use bytes::BytesMut;
use http::{HeaderMap, Method, Request, Response, Uri};
use options::{CacheOptions, Predicate};
use pin_project_lite::pin_project;
use tower_layer::Layer;
use tower_service::Service;

pub mod body;
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
pub struct CacheLayer<Body, Cache, RespPred, Req, ReqPred>
where
	RespPred: Predicate<Resp<Body>>,
	ReqPred: Predicate<Req>
{
	options: CacheOptions<Resp<Body>, RespPred, Req, ReqPred>,
	_tys: PhantomData<(Req, Cache)>
}

impl<Body, Cache, RespPred, Req, ReqPred> Clone for CacheLayer<Body, Cache, RespPred, Req, ReqPred>
where
	RespPred: Predicate<Resp<Body>>,
	ReqPred: Predicate<Req>
{
	fn clone(&self) -> Self {
		Self {
			options: self.options.clone(),
			_tys: PhantomData
		}
	}
}

impl<Body, Cache, RespPred, Req, ReqPred> CacheLayer<Body, Cache, RespPred, Req, ReqPred>
where
	RespPred: Predicate<Resp<Body>>,
	ReqPred: Predicate<Req>
{
	/// Create a new [`CacheLayer`] with the given options
	pub fn new(options: CacheOptions<Resp<Body>, RespPred, Req, ReqPred>) -> Self {
		Self {
			options,
			_tys: PhantomData
		}
	}
}

impl<Body, Cache, RespPred, Req, ReqPred, Svc> Layer<Svc>
	for CacheLayer<Body, Cache, RespPred, Req, ReqPred>
where
	Svc: Service<Req>,
	Cache: cache::Cache<CacheKey, CachedResp>,
	RespPred: Predicate<Resp<Body>>,
	ReqPred: Predicate<Req>
{
	type Service = CacheService<Body, Cache, RespPred, Req, ReqPred, Svc>;

	fn layer(&self, inner: Svc) -> Self::Service {
		CacheService {
			inner,
			cache: Cache::default(),
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
pub struct CacheService<Body, Cache, RespPred, Req, ReqPred, Svc>
where
	RespPred: Predicate<Resp<Body>>,
	ReqPred: Predicate<Req>,
	Svc: Service<Req>
{
	inner: Svc,
	cache: Cache,
	options: CacheOptions<Resp<Body>, RespPred, Req, ReqPred>
}

impl<Body, Cache, RespPred, Req, ReqPred, Svc> Clone
	for CacheService<Body, Cache, RespPred, Req, ReqPred, Svc>
where
	RespPred: Predicate<Resp<Body>>,
	ReqPred: Predicate<Req>,
	Svc: Service<Req> + Clone,
	Cache: Clone
{
	fn clone(&self) -> Self {
		Self {
			inner: self.inner.clone(),
			cache: self.cache.clone(),
			options: self.options.clone()
		}
	}
}

impl<Body, Cache, RespPred, Req, ReqPred, Svc> Service<Request<Req>>
	for CacheService<Body, Cache, RespPred, Request<Req>, ReqPred, Svc>
where
	Svc: Service<Request<Req>, Response = http::Response<Body>>,
	Body: http_body::Body,
	Svc::Error: Clone,
	Svc::Future: Future,
	Cache: cache::Cache<CacheKey, CachedResp>,
	RespPred: Predicate<Resp<Body>>,
	ReqPred: Predicate<Request<Req>>
{
	type Response = Resp<Body>;

	type Error = Svc::Error;
	type Future = CachingFut<Body, Svc::Error, Svc::Future, RespPred>;

	fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
		self.inner.poll_ready(cx)
	}

	fn call(&mut self, req: Request<Req>) -> Self::Future {
		let key = (req.method().clone(), req.uri().clone());

		// First, we need to see if it's been invalidated since we last got a request to this
		// handler. To do that, we need to check if we've already handled a request wth this key.
		// 'Cause if we haven't handled a request with this key, there's nothing to invalidate
		// anyways.
		let entry = {
			let invalidated = self.cache.get_mut(&key);

			// So. If we did get the bool that indicates whether it's been invaidated or not, we want
			// to check if and remove the cached response if it's been invalidated.
			match invalidated {
				Some(guard) => {
					let mut response = (**guard).write().unwrap_or_else(PoisonError::into_inner);
					if !response.valid.load(Ordering::Relaxed) {
						response.response = MaybeCompleteResponse::Nothing;
						response.valid.store(true, Ordering::Relaxed);
					}
					guard.clone()
				}
				// If we haven't yet received a request with this key, though, we need to indicate that
				// we now have so it can be invalidated in the future.
				None => {
					drop(invalidated);
					let valid = self.options.invalidator.insert(key.clone());
					let entry = Arc::new(RwLock::new(CachedRespInner::new(valid)));
					self.cache.insert_if_not_exists(key.clone(), entry.clone());
					entry
				}
			}
		};

		// Then, after we've checked to see if it's invalidated (and cleared the cache if so), we
		// need to see if we have since computed a new response. Technically this is a race
		// condition, as we can't know exactly what happened first - if we computed this response
		// or if we told ourselves to invalidate it first. We err on the side of recomputing a new
		// response.

		// if it's been invalidated, just clear the receiver so that there aren't messages sitting
		// in there that will never be read

		// If it's not valid so far, don't pass in the sender. We don't want them to have any
		// mechanism to cache this response if it doesn't fulfill the prerequisites.
		let valid_so_far = self
			.options
			.req_pred
			.as_ref()
			.is_none_or(|p| p.fulfills(&req));

		CachingFut {
			fut: self.inner.call(req),
			entry,
			resp_pred: self.options.resp_pred.clone(),
			valid_so_far,
			_phantom: PhantomData
		}
	}
}

type CacheKey = (Method, Uri);
type Resp<Body> = http::Response<MaybeCachedBody<Body>>;

type CachedResp = Arc<RwLock<CachedRespInner>>;

/// A cached response, generally used inside an Arc<RwLock<_>> (see [`CachedResp`]) to reduce lock
/// contention
pub struct CachedRespInner {
	valid: Arc<AtomicBool>,
	response: MaybeCompleteResponse
}

/// I would like to work around this but boxing stuff makes types in trait implementations
/// difficult... hmmmmmm
#[expect(clippy::large_enum_variant)]
enum MaybeCompleteResponse {
	Nothing,
	Partial(PartialResponse),
	Complete(http::Response<CachedBody>)
}

struct PartialResponse {
	resp: http::Response<NoOpBody>,
	data: BytesMut,
	trailers: HeaderMap,
	cond_var: Arc<AtomicU32>
}

impl CachedRespInner {
	fn new(valid: Arc<AtomicBool>) -> Self {
		Self {
			valid,
			response: MaybeCompleteResponse::Nothing
		}
	}
}

pin_project! {
	/// The [`Future`] returned by [`<CacheService as Service>::call`]
	pub struct CachingFut<Body, Err, Fut, RespPred>
	where
		Fut: Future<Output = Result<http::Response<Body>, Err>>,
		RespPred: Predicate<Resp<Body>>,
	{
		#[pin]
		fut: Fut,
		entry: CachedResp,
		valid_so_far: bool,
		resp_pred: Option<Arc<RespPred>>,
		_phantom: PhantomData<Err>
	}
}

impl<Body, Err, Fut, RespPred> Future for CachingFut<Body, Err, Fut, RespPred>
where
	Fut: Future<Output = Result<http::Response<Body>, Err>>,
	RespPred: Predicate<Resp<Body>>
{
	type Output = Result<Resp<Body>, Err>;

	fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
		fn clone_cached<B, E>(
			resp: &Response<CachedBody>
		) -> Poll<Result<Response<MaybeCachedBody<B>>, E>> {
			let (parts, CachedBody { data, trailers }) = resp.clone().into_parts();
			Poll::Ready(Ok(Response::from_parts(parts, MaybeCachedBody::Cached {
				data: Some(data),
				trailers: Some(trailers)
			})))
		}

		{
			let maybe_complete = self.entry.read().unwrap_or_else(PoisonError::into_inner);
			if let MaybeCompleteResponse::Complete(ref resp) = maybe_complete.response {
				// If we have a response, then just clone it and return it. Woohoo!
				return clone_cached(resp);
			}
		}

		let proj = self.project();

		match proj.fut.poll(cx) {
			Poll::Ready(Ok(resp)) => {
				let (parts, inner) = resp.into_parts();

				// We're looping through this so that if we see that there's already a partial
				// response that someone else is working on, we just wait for that to be ready and
				// once we see that it is ready (or at least wee see that the flag has changed, so
				// we should check again)
				loop {
					// This is safe to unwrap 'cause we verify that up above
					let mut resp = proj.entry.write().unwrap_or_else(PoisonError::into_inner);
					match resp.response {
						// If there's still nothing in the response, then just continue with the
						// streaming body and such.
						MaybeCompleteResponse::Nothing => {
							resp.response = MaybeCompleteResponse::Partial(PartialResponse {
								resp: Response::from_parts(parts.clone(), NoOpBody),
								data: BytesMut::new(),
								trailers: HeaderMap::new(),
								cond_var: Arc::new(AtomicU32::new(0))
							});

							drop(resp);

							let stream_body = CacheStreamBody {
								inner,
								cache_entry: proj.entry.clone()
							};
							let new_resp =
								Response::from_parts(parts, MaybeCachedBody::New(stream_body));
							return Poll::Ready(Ok(new_resp));
						}
						MaybeCompleteResponse::Partial(PartialResponse {
							ref cond_var, ..
						}) => {
							let var = cond_var.clone();
							drop(resp);

							atomic_wait::wait(&var, 0);
						}
						MaybeCompleteResponse::Complete(ref resp) => return clone_cached(resp)
					}
				}
			}
			Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
			Poll::Pending => Poll::Pending
		}
	}
}
