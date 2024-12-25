//! Options for configuring how the [`crate::CacheLayer`] and [`crate::CacheService`] should work

use std::{marker::PhantomData, ops::Range, sync::Arc};

use crate::invalidator::Invalidator;

/// Options to pass in to [`crate::CacheLayer::new`] to define how the layer should handle caching
/// (at time of writing, this specifically only restrict when responses should be cached based on
/// the given predicates).
///
/// # Generics
///
/// - `Resp`: The Response type that this is generic over, probably going to resolve to
///   [`http::Response`] (but we're generic to be safe). This is stored in a [`PhantomData`] since
///   it's not directly used, but necessary to make the `RespPred` bound work.
/// - `RespPred`: The [`Predicate`] over `Resp`, used for [resp_pred](CacheOptions::resp_pred).
/// - `Req`: The Request type that this is generic over - this will probably be [`http::Request`],
///   but that is also generic over another requet type, so that will depend on what other
///   libraries you're using
/// - `ReqPred`: The [`Predicate`] over `Req`, used for [req_pred](CacheOptions::req_pred).
pub struct CacheOptions<Resp, RespPred, Req, ReqPred>
where
	RespPred: Predicate<Resp>,
	ReqPred: Predicate<Req>
{
	/// A predicate that, if it is [`Some`], will restrict what responses are cached based on this
	/// predicate, which will operate on the response itself. See the [`Predicate`] trait for more
	/// information.
	///
	/// If [`Self::req_pred`] fails on a given request, [`Self::resp_pred`] will not be evaluated
	/// on that request's response since there is no way for it to succeed.
	///
	/// The 'Response' mentioned here is specifically the [`tower_service::Service::Response`] that
	/// is returned from [`crate::CacheService`]'s underlying service.
	pub resp_pred: Option<Arc<RespPred>>,
	/// A predicate that, if it is [`Some`], will restrict what responses are cached based on this
	/// predicate, which will operate on the request which was given to generate that response. See
	/// the [`Predicate`] trait for more information.
	pub req_pred: Option<Arc<ReqPred>>,
	pub(crate) invalidator: Invalidator,
	_response: PhantomData<(Resp, Req)>
}

impl<Resp, RespPred, Req, ReqPred> Clone for CacheOptions<Resp, RespPred, Req, ReqPred>
where
	RespPred: Predicate<Resp>,
	ReqPred: Predicate<Req>
{
	fn clone(&self) -> Self {
		Self {
			resp_pred: self.resp_pred.clone(),
			req_pred: self.req_pred.clone(),
			invalidator: self.invalidator.clone(),
			_response: PhantomData
		}
	}
}

impl<Resp, RespPred, Req, ReqPred> CacheOptions<Resp, RespPred, Req, ReqPred>
where
	RespPred: Predicate<Resp>,
	ReqPred: Predicate<Req>
{
	/// Create a new [`CacheOptions`] with the given `resp_pred` and `req_pred`
	pub fn new(resp_pred: Option<RespPred>, req_pred: Option<ReqPred>) -> Self {
		Self {
			resp_pred: resp_pred.map(Arc::new),
			req_pred: req_pred.map(Arc::new),
			invalidator: Invalidator::default(),
			_response: PhantomData
		}
	}
}

/// A simple trait to facilitate caching conditions. Specifically, values implementing this trait
/// can be passed into [`CacheOptions::new`] to declare when responses should be cached.
pub trait Predicate<Value> {
	/// This should return [`true`] when this predicate is fulfilled. In the case of this
	/// function's usage with [`CacheOptions`], this function returning true means that the
	/// relevant response should be cached. For example, look at the implementation of
	/// [`http::StatusCode`] below: It returns true (meaning that the response should be cached)
	/// when the repsonse has the given status code
	///
	/// This is also used to place bounds caching responses based on the requests that generated
	/// them (in the case of [`CacheOptions::req_pred`]
	fn fulfills(&self, val: &Value) -> bool;
}

impl<Value, Func> Predicate<Value> for Func
where
	Func: Fn(&Value) -> bool
{
	fn fulfills(&self, response: &Value) -> bool {
		(self)(response)
	}
}

impl<Resp> Predicate<http::Response<Resp>> for http::StatusCode {
	fn fulfills(&self, response: &http::Response<Resp>) -> bool {
		response.status() == *self
	}
}

impl<Resp> Predicate<http::Response<Resp>> for Range<http::StatusCode> {
	fn fulfills(&self, response: &http::Response<Resp>) -> bool {
		self.contains(&response.status())
	}
}

impl<Req> Predicate<http::Request<Req>> for http::Method {
	fn fulfills(&self, req: &http::Request<Req>) -> bool {
		req.method() == *self
	}
}
