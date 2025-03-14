mod websocket {
    use std::io::{Error, ErrorKind, Result};
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use bytes::{BufMut, BytesMut};
    use futures_util::Stream;
    use pin_project::pin_project;
    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
    use worker::{EventStream, WebSocket, WebsocketEvent};

    #[pin_project]
    pub struct WebSocketStream<'a> {
        ws: &'a WebSocket,
        #[pin]
        stream: EventStream<'a>,
        buffer: BytesMut,
        closed: bool,
    }

    impl<'a> WebSocketStream<'a> {
        pub fn new(
            ws: &'a WebSocket,
            stream: EventStream<'a>,
            early_data: Option<Vec<u8>>,
        ) -> Self {
            let mut buffer = BytesMut::with_capacity(4096);
            if let Some(data) = early_data {
                buffer.put_slice(&data);
            }
            Self {
                ws,
                stream,
                buffer,
                closed: false,
            }
        }
    }

    impl<'a> AsyncRead for WebSocketStream<'a> {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<Result<()>> {
            let this = self.project();
            let remaining = buf.remaining();

            if !this.buffer.is_empty() {
                let take = std::cmp::min(this.buffer.len(), remaining);
                buf.put_slice(&this.buffer.split_to(take));
                return Poll::Ready(Ok(()));
            }

            if *this.closed {
                return Poll::Ready(Ok(()));
            }

            match this.stream.as_mut().poll_next(cx) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(Some(Ok(WebsocketEvent::Message(msg)))) => {
                    if let Some(data) = msg.bytes() {
                        this.buffer.put_slice(&data);
                        self.poll_read(cx, buf)
                    } else {
                        Poll::Pending
                    }
                }
                Poll::Ready(Some(Ok(WebsocketEvent::Close(_)))) => {
                    *this.closed = true;
                    Poll::Ready(Ok(()))
                }
                Poll::Ready(Some(Err(e))) => {
                    Poll::Ready(Err(Error::new(ErrorKind::Other, e.to_string())))
                }
                _ => Poll::Ready(Ok(())),
            }
        }
    }

    impl<'a> AsyncWrite for WebSocketStream<'a> {
        fn poll_write(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<Result<usize>> {
            let this = self.project();
            if *this.closed {
                return Poll::Ready(Err(Error::new(ErrorKind::BrokenPipe, "socket closed")));
            }
            if let Err(e) = this.ws.send_with_bytes(buf) {
                return Poll::Ready(Err(Error::new(ErrorKind::Other, e.to_string())));
            }
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
        ) -> Poll<Result<()>> {
            let this = self.project();
            if *this.closed {
                return Poll::Ready(Err(Error::new(
                    ErrorKind::BrokenPipe,
                    "socket already closed",
                )));
            }
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
        ) -> Poll<Result<()>> {
            let this = self.project();
            if !*this.closed {
                if let Err(e) = this.ws.close(None, Some("normal close")) {
                    return Poll::Ready(Err(Error::new(
                        ErrorKind::Other,
                        e.to_string(),
                    )));
                }
                *this.closed = true;
            }
            Poll::Ready(Ok(()))
        }
    }
}
