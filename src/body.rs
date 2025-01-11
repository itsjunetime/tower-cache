use std::{
	collections::VecDeque,
	convert::Infallible,
	pin::Pin,
	sync::mpsc::Sender,
	task::{Context, Poll}
};

use bytes::Buf;
use http::HeaderMap;
use http_body::{Body, Frame};

pin_project_lite::pin_project! {
	pub struct CacheStreamBody<B> {
		#[pin]
		pub inner: B,
		pub sender: Option<Sender<BodyMessage>>
	}
}

impl<B: Body> Body for CacheStreamBody<B> {
	type Data = MaybeOwnedBuf<B::Data>;
	type Error = B::Error;

	fn poll_frame(
		self: Pin<&mut Self>,
		cx: &mut Context<'_>
	) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
		// If any of the `.send()` calls fail in this function, just let them fail. They only fail
		// if there's no receiving end, and if that's the case, there's nothing to cache our data
		// and nobody who would even be able to serve our cached data so it's fine to just let it
		// disappear.

		let proj = self.project();
		match proj.inner.poll_frame(cx) {
			Poll::Ready(None) => {
				if let Some(ref sender) = proj.sender {
					drop(sender.send(BodyMessage::Done(sender.clone())));
				}
				Poll::Ready(None)
			}
			Poll::Ready(Some(Ok(mut frame))) => {
				if let Some(ref sender) = proj.sender {
					frame = match frame.into_data() {
						Ok(mut data) => {
							let mut vec = VecDeque::with_capacity(data.remaining());
							while data.remaining() > 0 {
								let chunk = data.chunk();
								vec.extend(chunk);
								data.advance(chunk.len());
							}

							drop(sender.send(BodyMessage::Data(vec.clone())));
							return Poll::Ready(Some(Ok(Frame::data(MaybeOwnedBuf::Owned(vec)))));
						}
						Err(frame) => {
							if let Some(trailers) = frame.trailers_ref() {
								drop(sender.send(BodyMessage::Trailers(trailers.clone())));
							}
							frame
						}
					}
				}

				Poll::Ready(Some(Ok(match frame.into_data() {
					Ok(data) => Frame::data(MaybeOwnedBuf::MaybeUnowned(data)),
					Err(frame) => Frame::trailers(frame.into_trailers().ok().unwrap())
				})))
			}
			Poll::Pending => Poll::Pending,
			Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(e)))
		}
	}
}

pub(crate) enum BodyMessage {
	Shell(http::Response<NoOpBody>),
	Data(VecDeque<u8>),
	Trailers(HeaderMap),
	Done(Sender<Self>)
}

pub enum MaybeOwnedBuf<B: Buf> {
	Owned(VecDeque<u8>),
	MaybeUnowned(B)
}

impl<B: Buf> Buf for MaybeOwnedBuf<B> {
	fn remaining(&self) -> usize {
		match self {
			Self::Owned(v) => v.remaining(),
			Self::MaybeUnowned(b) => b.remaining()
		}
	}

	fn chunk(&self) -> &[u8] {
		match self {
			Self::Owned(v) => v.chunk(),
			Self::MaybeUnowned(b) => b.chunk()
		}
	}

	fn advance(&mut self, cnt: usize) {
		match self {
			Self::Owned(v) => v.advance(cnt),
			Self::MaybeUnowned(b) => b.advance(cnt)
		}
	}
}

impl<B: Buf> AsRef<[u8]> for MaybeOwnedBuf<B> {
	fn as_ref(&self) -> &[u8] {
		self.chunk()
	}
}

#[derive(Clone)]
pub struct CachedBody {
	pub(crate) data: VecDeque<u8>,
	pub(crate) trailers: HeaderMap
}

pub enum MaybeCachedBody<B> {
	Cached {
		data: Option<VecDeque<u8>>,
		trailers: Option<HeaderMap>
	},
	New(CacheStreamBody<B>)
}

impl<B: Body> Body for MaybeCachedBody<B> {
	type Data = MaybeOwnedBuf<B::Data>;
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
					return Poll::Ready(Some(Ok(Frame::data(MaybeOwnedBuf::Owned(data)))));
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

#[cfg(feature = "axum")]
impl<B> axum::response::IntoResponse for MaybeCachedBody<B>
where
	B: Body + Send + 'static,
	B::Data: 'static,
	<MaybeCachedBody<B> as Body>::Error: Send + Sync + core::error::Error + 'static,
	<MaybeCachedBody<B> as Body>::Data: Send + 'static
{
	fn into_response(self) -> axum::response::Response {
		axum::response::Response::new(axum::body::Body::from_stream(self))
	}
}

#[cfg(feature = "axum")]
impl<B: Body> futures_core::stream::Stream for MaybeCachedBody<B>
where
	MaybeCachedBody<B>: Body,
	<MaybeCachedBody<B> as Body>::Data: Send + AsRef<[u8]>
{
	type Item = Result<bytes::Bytes, <MaybeCachedBody<B> as Body>::Error>;

	fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
		use bytes::Bytes;

		match <Self as Body>::poll_frame(self, cx) {
			Poll::Pending => Poll::Pending,
			Poll::Ready(None) => Poll::Ready(None),
			Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(e))),
			Poll::Ready(Some(Ok(frame))) => Poll::Ready(Some(Ok(match frame.into_data() {
				// Unfortunately, we do have to do this 'copy_from_slice' because, even though we
				// know `data` is `MaybeOwnedBuf<B::Data>`, we don't know if `B::Data` is 'static
				// or not, and also VecDeque that it might contain (if it's already owned) is not
				// necessarily contiguous in memory, so we need to copy it over anyways.
				Ok(data) => Bytes::copy_from_slice(data.as_ref()),
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
}

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
