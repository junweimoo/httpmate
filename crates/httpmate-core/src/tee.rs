//! Body teeing: stream a request/response body through unmodified while
//! copying up to a cap into a capture buffer for the recorder.
//!
//! The capture is delivered over a oneshot when the body finishes. A guard's
//! Drop delivers whatever was captured if the body is dropped early (client
//! disconnect, upstream error), so the finalize task always gets an answer.

use bytes::Bytes;
use http_body::{Body, Frame, SizeHint};
use pin_project_lite::pin_project;
use std::pin::Pin;
use std::task::{ready, Context, Poll};
use tokio::sync::oneshot;

/// What the tee captured once the body is done (or abandoned).
#[derive(Debug, Default)]
pub struct CapturedBody {
    pub bytes: Vec<u8>,
    /// Total data bytes that flowed through, including beyond the cap.
    pub total: u64,
    /// True if the body exceeded the capture cap.
    pub truncated: bool,
    /// False if the body was dropped before reaching end-of-stream.
    pub completed: bool,
}

struct CaptureGuard {
    buf: Vec<u8>,
    total: u64,
    truncated: bool,
    limit: usize,
    tx: Option<oneshot::Sender<CapturedBody>>,
}

impl CaptureGuard {
    fn push(&mut self, data: &[u8]) {
        self.total += data.len() as u64;
        let room = self.limit.saturating_sub(self.buf.len());
        if room >= data.len() {
            self.buf.extend_from_slice(data);
        } else {
            self.buf.extend_from_slice(&data[..room]);
            self.truncated = true;
        }
    }

    fn finish(&mut self, completed: bool) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(CapturedBody {
                bytes: std::mem::take(&mut self.buf),
                total: self.total,
                truncated: self.truncated,
                completed,
            });
        }
    }
}

impl Drop for CaptureGuard {
    fn drop(&mut self) {
        self.finish(false);
    }
}

pin_project! {
    pub struct TeeBody<B> {
        #[pin]
        inner: B,
        guard: CaptureGuard,
    }
}

impl<B> TeeBody<B> {
    /// Wrap `inner`, capturing at most `limit` bytes. Returns the wrapper and
    /// a receiver resolved when the body completes or is dropped.
    pub fn new(inner: B, limit: usize) -> (Self, oneshot::Receiver<CapturedBody>) {
        let (tx, rx) = oneshot::channel();
        (
            Self {
                inner,
                guard: CaptureGuard { buf: Vec::new(), total: 0, truncated: false, limit, tx: Some(tx) },
            },
            rx,
        )
    }
}

impl<B> Body for TeeBody<B>
where
    B: Body<Data = Bytes>,
{
    type Data = Bytes;
    type Error = B::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.project();
        match ready!(this.inner.poll_frame(cx)) {
            Some(Ok(frame)) => {
                if let Some(data) = frame.data_ref() {
                    this.guard.push(data);
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Some(Err(e)) => {
                this.guard.finish(false);
                Poll::Ready(Some(Err(e)))
            }
            None => {
                this.guard.finish(true);
                Poll::Ready(None)
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::{BodyExt, Full};

    #[tokio::test]
    async fn captures_full_body() {
        let body = Full::new(Bytes::from_static(b"hello world"));
        let (tee, rx) = TeeBody::new(body, 1024);
        let collected = tee.collect().await.unwrap().to_bytes();
        assert_eq!(&collected[..], b"hello world");
        let cap = rx.await.unwrap();
        assert_eq!(cap.bytes, b"hello world");
        assert_eq!(cap.total, 11);
        assert!(cap.completed);
        assert!(!cap.truncated);
    }

    #[tokio::test]
    async fn truncates_beyond_cap_but_streams_everything() {
        let payload = vec![7u8; 100];
        let body = Full::new(Bytes::from(payload.clone()));
        let (tee, rx) = TeeBody::new(body, 10);
        let collected = tee.collect().await.unwrap().to_bytes();
        assert_eq!(collected.len(), 100, "full body must still flow through");
        let cap = rx.await.unwrap();
        assert_eq!(cap.bytes.len(), 10);
        assert_eq!(cap.total, 100);
        assert!(cap.truncated);
        assert!(cap.completed);
    }

    #[tokio::test]
    async fn drop_delivers_partial_capture() {
        let body = Full::new(Bytes::from_static(b"abc"));
        let (tee, rx) = TeeBody::new(body, 1024);
        drop(tee);
        let cap = rx.await.unwrap();
        assert!(!cap.completed);
        assert!(cap.bytes.is_empty());
    }
}
