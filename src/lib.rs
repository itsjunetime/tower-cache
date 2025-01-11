#![feature(mapped_lock_guards)]
#![doc = include_str!("../README.md")]

use core::{
	future::Future,
	marker::PhantomData,
	pin::Pin,
	sync::atomic::{AtomicBool, Ordering},
	task::{Context, Poll}
};
use std::{
	collections::VecDeque,
	sync::{
		mpsc::{channel, Receiver, Sender, TryRecvError},
		Arc
	}
};

use body::{BodyMessage, CacheStreamBody, CachedBody, MaybeCachedBody, NoOpBody};
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
		let (sender, receiver) = channel();
		CacheService {
			inner,
			cache: Cache::default(),
			registered_with_invalidator: Vec::new(),
			sender: Some(sender),
			receiver,
			in_progress_resp: None,
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
	registered_with_invalidator: Vec<(CacheKey, Arc<AtomicBool>)>,
	sender: Option<Sender<BodyMessage>>,
	receiver: Receiver<BodyMessage>,
	in_progress_resp: Option<(http::Response<NoOpBody>, VecDeque<u8>, HeaderMap)>,
	options: CacheOptions<Resp<Body>, RespPred, Req, ReqPred>
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
	type Future = CachingFut<Body, Cache, Svc::Error, Svc::Future, RespPred>;

	fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
		self.inner.poll_ready(cx)
	}

	fn call(&mut self, req: Request<Req>) -> Self::Future {
		let key = (req.method().clone(), req.uri().clone());

		// First, we need to see if it's been invalidated since we last got a request to this
		// handler. To do that, we need to check if we've already handled a request wth this key.
		// 'Cause if we haven't handled a request with this key, there's nothing to invalidate
		// anyways.
		let invalidated = self
			.registered_with_invalidator
			.iter()
			.find(|(k, _)| *k == key)
			.map(|(_, valid)| valid);

		// So. If we did get the bool that indicates whether it's been invaidated or not, we want
		// to check if and remove the cached response if it's been invalidated.
		match invalidated {
			Some(valid) =>
				if !valid.load(Ordering::Relaxed) {
					self.cache.remove(&key);
					valid.store(true, Ordering::Relaxed);
				},
			// If we haven't yet received a request with this key, though, we need to indicate that
			// we now have so it can be invalidated in the future.
			None => {
				let valid = self.options.invalidator.insert(key.clone());
				self.registered_with_invalidator.push((key.clone(), valid));
			}
		}

		let next_msg = match self.receiver.try_recv() {
			Ok(msg) => match msg {
				BodyMessage::Shell(shell) => {
					self.in_progress_resp =
						Some((shell, VecDeque::default(), HeaderMap::default()));
					None
				}
				msg => Some(msg)
			},
			Err(TryRecvError::Empty) => None,
			// This shouldn't happen ever. There should always either be a Sender in this struct,
			// in a Fut, or within the channel (being sent back to this struct)
			Err(TryRecvError::Disconnected) => unreachable!()
		};

		let mut cache_partial_as_done = false;
		if let Some((_, ref mut partial_data, ref mut partial_trailers)) =
			self.in_progress_resp.as_mut()
		{
			let mut handle_msg = |msg: BodyMessage| {
				match msg {
					// If we handle a Shell here, that's a bug - we either didn't handle it up
					// above as we should've, or we loaned a sender out to two futs at the same
					// time and they're sending us response data at the same time (we don't share
					// it with two futs at the same time 'cause we don't know how to handle that),
					// so just panic here.
					BodyMessage::Shell(_) => unreachable!(),
					BodyMessage::Data(data) => partial_data.extend(data),
					BodyMessage::Trailers(trailers) => partial_trailers.extend(trailers),
					BodyMessage::Done(sender) => {
						self.sender = Some(sender);
						cache_partial_as_done = true;
					}
				}
			};

			if let Some(last_msg) = next_msg {
				handle_msg(last_msg);
			}

			while let Ok(msg) = self.receiver.try_recv() {
				handle_msg(msg);
			}
		}

		if cache_partial_as_done {
			// Don't like the take + unwrap, but I can't think of a way to 'upgrade' a ref mut (as
			// we have above) into a take that returns the wrapped value, so we have to do this.
			let (resp, data, trailers) = self.in_progress_resp.take().unwrap();
			let (parts, NoOpBody) = resp.into_parts();
			let body = CachedBody { data, trailers };

			self.cache.remove(&key);
			self.cache
				.insert_if_not_exists(key.clone(), Response::from_parts(parts, body));
		}

		// If it's not valid so far, don't pass in the sender. We don't want them to have any
		// mechanism to cache this response if it doesn't fulfill the prerequisites.
		let valid_so_far = self
			.options
			.req_pred
			.as_ref()
			.is_none_or(|p| p.fulfills(&req));

		CachingFut {
			key,
			fut: self.inner.call(req),
			cache: self.cache.clone(),
			resp_pred: self.options.resp_pred.clone(),
			sender: valid_so_far.then(|| self.sender.take()).flatten(),
			_phantom: PhantomData
		}
	}
}

type CacheKey = (Method, Uri);
type Resp<Body> = http::Response<MaybeCachedBody<Body>>;
type CachedResp = http::Response<CachedBody>;

pin_project! {
	/// The [`Future`] returned by [`<CacheService as Service>::call`]
	pub struct CachingFut<Body, Cache, Err, Fut, RespPred>
	where
		Fut: Future<Output = Result<http::Response<Body>, Err>>,
		Cache: cache::Cache<CacheKey, CachedResp>,
		RespPred: Predicate<Resp<Body>>,
	{
		key: CacheKey,
		#[pin]
		fut: Fut,
		cache: Cache,
		sender: Option<Sender<BodyMessage>>,
		resp_pred: Option<Arc<RespPred>>,
		_phantom: PhantomData<Err>
	}
}

impl<Body, Cache, Err, Fut, RespPred> Future for CachingFut<Body, Cache, Err, Fut, RespPred>
where
	Fut: Future<Output = Result<http::Response<Body>, Err>>,
	Cache: cache::Cache<CacheKey, CachedResp>,
	RespPred: Predicate<Resp<Body>>
{
	type Output = Result<Resp<Body>, Err>;

	fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
		if let Some(resp) = self.cache.get(&self.key) {
			let (parts, CachedBody { data, trailers }) = (*resp).clone().into_parts();
			return Poll::Ready(Ok(Response::from_parts(parts, MaybeCachedBody::Cached {
				data: Some(data),
				trailers: Some(trailers)
			})));
		}

		let proj = self.project();

		match proj.fut.poll(cx) {
			Poll::Ready(Ok(resp)) => {
				let (parts, inner) = resp.into_parts();
				if let Some(sender) = proj.sender {
					sender.send(BodyMessage::Shell(Response::from_parts(
						parts.clone(),
						NoOpBody
					)));
				}

				let stream_body = CacheStreamBody {
					inner,
					sender: proj.sender.take()
				};
				let new_resp = Response::from_parts(parts, MaybeCachedBody::New(stream_body));
				Poll::Ready(Ok(new_resp))
			}
			Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
			Poll::Pending => Poll::Pending
		}
	}
}
