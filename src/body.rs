//! A collection of implementors of [`http_body::Body`] which enable streamed caching in this crate

use std::{
	collections::VecDeque,
	convert::Infallible,
	pin::Pin,
	sync::{atomic::Ordering, PoisonError},
	task::{Context, Poll}
};

use bytes::{Buf, Bytes};
use http::{HeaderMap, Response};
use http_body::{Body, Frame};

use crate::{CachedResp, CachedRespInner, MaybeCompleteResponse, PartialResponse};

pin_project_lite::pin_project! {
	/// A struct wrapping an implementor of [`Body`] which copies all data and trailers returned
	/// from the wrapped Body so that it can be cached and returned easily at a later date
	///
	/// [`Body`]: http_body::Body
	pub struct CacheStreamBody<Body> {
		#[pin]
		pub(crate) inner: Body,
		pub(crate) cache_entry: CachedResp
	}
}

impl<B: Body> Body for CacheStreamBody<B> {
	type Data = Bytes;
	type Error = B::Error;

	fn poll_frame(
		self: Pin<&mut Self>,
		cx: &mut Context<'_>
	) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
		let proj = self.project();
		match proj.inner.poll_frame(cx) {
			Poll::Ready(frame_res_opt) => {
				let mut inner = proj
					.cache_entry
					.write()
					.unwrap_or_else(PoisonError::into_inner);

				let CachedRespInner {
					response: MaybeCompleteResponse::Partial(ref mut resp),
					valid: _
				} = *inner
				else {
					panic!()
				};

				match frame_res_opt {
					Some(Err(e)) => {
						// If we run into an error, we still need to tell the cond_var that we're
						// done and reset the cache so that someone else can try to get the
						// response.
						let cond_var = resp.cond_var.clone();

						inner.response = MaybeCompleteResponse::Nothing;
						drop(inner);

						cond_var.store(1, Ordering::Release);

						Poll::Ready(Some(Err(e)))
					}
					None => {
						let PartialResponse {
							resp,
							data,
							trailers,
							cond_var
						} = resp;
						let (parts, NoOpBody) = std::mem::take(resp).into_parts();

						let data = std::mem::take(data).into();
						let trailers = std::mem::take(trailers);
						let cond_var = cond_var.clone();

						inner.response = MaybeCompleteResponse::Complete(Response::from_parts(
							parts,
							CachedBody { data, trailers }
						));
						drop(inner);

						cond_var.store(1, Ordering::Release);

						Poll::Ready(None)
					}
					Some(Ok(mut frame)) => {
						frame = match frame.into_data() {
							Ok(mut data) => {
								let mut vec = Vec::with_capacity(data.remaining());
								while data.remaining() > 0 {
									let chunk = data.chunk();
									vec.extend(chunk);
									data.advance(chunk.len());
								}

								resp.data.extend(vec.iter());
								return Poll::Ready(Some(Ok(Frame::data(Bytes::from_owner(
									vec
								)))));
							}
							Err(frame) => {
								if let Some(trailers) = frame.trailers_ref() {
									resp.trailers.extend(trailers.clone());
								}
								frame
							}
						};

						Poll::Ready(Some(Ok(match frame.into_data() {
							// we don't like copying data but since we're using built-in
							// overrideable Buf methods, if someone wants to ensure we don't have
							// to do copies, they can do so.
							Ok(mut data) => Frame::data(data.copy_to_bytes(data.remaining())),
							Err(frame) => Frame::trailers(frame.into_trailers().ok().unwrap())
						})))
					}
				}
			}
			Poll::Pending => Poll::Pending
		}
	}
}

#[derive(Clone)]
pub(crate) struct CachedBody {
	pub(crate) data: Bytes,
	pub(crate) trailers: HeaderMap
}

/// An implementor of [`Body`] that may already be cached (and thus can just be cloned
/// and returned) or may need to be polled and streamed through the general `Body` interface
///
/// [`Body`]: http_body::Body
pub enum MaybeCachedBody<B> {
	/// The variant present if this body has already been cached and we just need to return it
	Cached {
		/// The data of the body - an [`Option`] so that we can just take it out and leave [`None`]
		/// in its place after we've already returned it from [`poll_frame`]
		///
		/// [`poll_frame`]: http_body::Body::poll_frame
		data: Option<Bytes>,
		/// The trailers of the body, once again wrapped in an option so we can just [`take`] it
		/// out when we want to return it
		///
		/// [`take`]: std::mem::take
		trailers: Option<HeaderMap>
	},
	/// The variant present if the body has not been cached yet and we need to poll, process, and
	/// cache it.
	New(CacheStreamBody<B>)
}

impl<B: Body> Body for MaybeCachedBody<B> {
	type Data = Bytes;
	type Error = B::Error;

	fn poll_frame(
		self: Pin<&mut Self>,
		cx: &mut Context<'_>
	) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
		// SAFETY: This is safe because we make sure we don't overwrite or move the only
		// potentially-movable thing inside self, the `CacheStreamBody.inner`.
		match unsafe { Pin::get_unchecked_mut(self) } {
			Self::Cached { data, trailers } => {
				if let Some(data) = data.take() {
					return Poll::Ready(Some(Ok(Frame::data(data))));
				}

				if let Some(trailers) = trailers.take() {
					return Poll::Ready(Some(Ok(Frame::trailers(trailers))));
				}

				Poll::Ready(None)
			}
			// SAFETY: This is safe since we haven't violated any moving/overwriting constraints
			// since taking it out of the Pin.
			Self::New(body) => match Body::poll_frame(unsafe { Pin::new_unchecked(body) }, cx) {
				Poll::Ready(Some(Ok(f))) => match f.into_data() {
					Ok(data) => Poll::Ready(Some(Ok(Frame::data(data)))),
					Err(frame) => {
						// Don't like this unwrap but since they don't expose `Frame` as the enum
						// it clearly is, we can't do a clean match like we'd like.
						let trailers = frame.into_trailers().ok().unwrap();
						Poll::Ready(Some(Ok(Frame::trailers(trailers))))
					}
				},
				// We have to do this annoying duplication because the left and right sides are
				// technically different types
				Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(e))),
				Poll::Ready(None) => Poll::Ready(None),
				Poll::Pending => Poll::Pending
			}
		}
	}
}

/*#[cfg(feature = "axum")]
impl<B> axum::response::IntoResponse for MaybeCachedBody<B>
where
	B: Body + Send + 'static,
	B::Data: 'static,
	<Self as Body>::Error: Send + Sync + core::error::Error + 'static,
	<Self as Body>::Data: Send + 'static
{
	fn into_response(self) -> axum::response::Response {
		axum::response::Response::new(axum::body::Body::from_stream(self))
	}
}

#[cfg(feature = "axum")]
impl<B: Body> futures_core::stream::Stream for MaybeCachedBody<B>
where
	Self: Body,
	<Self as Body>::Data: Send + AsRef<[u8]>
{
	type Item = Result<bytes::Bytes, <Self as Body>::Error>;

	fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
		match <Self as Body>::poll_frame(self, cx) {
			Poll::Pending => Poll::Pending,
			Poll::Ready(None) => Poll::Ready(None),
			Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(e))),
			Poll::Ready(Some(Ok(frame))) => Poll::Ready(Some(Ok(match frame.into_data() {
				Ok(data) => data,
				Err(frame) => frame
					.into_trailers()
					.map(|trailers| {
						let mut trailers_str = Vec::new();
						for (name, value) in trailers {
							if let Some(name) = name {
								if !trailers_str.is_empty() {
									trailers_str.push(b'\n');
								}

								trailers_str.extend(name.as_str().as_bytes());
								trailers_str.extend(b": ");
							} else {
								trailers_str.push(b',');
							}

							trailers_str.extend(value.as_bytes());
						}

						Bytes::from_owner(trailers_str)
					})
					.unwrap_or_default()
			})))
		}
	}
}*/

/// A [`http_body::Body`] that contains no data and is always ready to return `None` when polled
#[derive(Default)]
pub struct NoOpBody;

impl Body for NoOpBody {
	type Data = VecDeque<u8>;
	type Error = Infallible;

	fn poll_frame(
		self: Pin<&mut Self>,
		_cx: &mut Context<'_>
	) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
		Poll::Ready(None)
	}
}
