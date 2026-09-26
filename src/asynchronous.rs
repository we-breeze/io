use std::io::{self, IoSlice, Write};
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncBufRead, AsyncRead, AsyncWrite, ReadBuf};

use crate::{Reader, Writer};

impl AsyncRead for Reader {
    fn poll_read(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        output: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let reader = self.get_mut();
        reader.advance(0);
        while output.remaining() > 0 && !reader.is_empty() {
            let chunk = reader.chunk();
            let count = output.remaining().min(chunk.len());
            output.put_slice(&chunk[..count]);
            reader.advance(count);
        }
        Poll::Ready(Ok(()))
    }
}

impl AsyncBufRead for Reader {
    fn poll_fill_buf(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
        let reader = self.get_mut();
        reader.advance(0);
        Poll::Ready(Ok(reader.chunk()))
    }

    fn consume(self: Pin<&mut Self>, count: usize) {
        self.get_mut().advance(count);
    }
}

impl AsyncWrite for Writer {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        Poll::Ready(Write::write(self.get_mut(), bytes))
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buffers: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Poll::Ready(Write::write_vectored(self.get_mut(), buffers))
    }

    fn is_write_vectored(&self) -> bool {
        true
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Write::flush(self.get_mut()))
    }

    // Like Tokio's Vec<u8> writer, shutdown only flushes this in-memory sink.
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_flush(cx)
    }
}

impl Writer {
    /// Read directly into the writable arena tail. No tokio::io::copy buffer.
    /// `maximum` limits this read and is additionally clamped to the Writer limit.
    pub fn poll_read_from<R: AsyncRead + ?Sized>(
        &mut self,
        cx: &mut Context<'_>,
        source: Pin<&mut R>,
        maximum: usize,
    ) -> Poll<io::Result<usize>> {
        let maximum = maximum.min(self.remaining());
        if maximum == 0 {
            return Poll::Ready(Ok(0));
        }
        if let Err(error) =
            self.segments
                .ensure_tail(&self.arena, self.len, maximum, &mut self.segment_size)
        {
            return Poll::Ready(Err(error));
        }
        let result = self
            .segments
            .back_mut()
            .expect("writable tail")
            .bytes
            .poll_read_from(cx, source, maximum);
        if let Poll::Ready(Ok(count)) = &result {
            self.len += *count;
        }
        result
    }

    pub async fn read_from<R: AsyncRead + Unpin + ?Sized>(
        &mut self,
        source: &mut R,
        maximum: usize,
    ) -> io::Result<usize> {
        std::future::poll_fn(|cx| self.poll_read_from(cx, Pin::new(&mut *source), maximum)).await
    }
}

impl Reader {
    /// Append directly into the original receive segments using exclusive access.
    /// Existing bytes/range caches are unchanged; no packet-prefix copy occurs.
    /// Bytes already consumed via BufRead are not made visible again.
    pub fn poll_read_from<R: AsyncRead + ?Sized>(
        &mut self,
        cx: &mut Context<'_>,
        source: Pin<&mut R>,
        maximum: usize,
    ) -> Poll<io::Result<usize>> {
        if maximum == 0 {
            return Poll::Ready(Ok(0));
        }
        if self.end.checked_add(maximum).is_none() {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "receive length overflow",
            )));
        }
        if let Err(error) =
            self.segments
                .ensure_tail(&self.arena, self.end, maximum, &mut self.segment_size)
        {
            return Poll::Ready(Err(error));
        }
        let result = self
            .segments
            .back_mut()
            .expect("writable tail")
            .bytes
            .poll_read_from(cx, source, maximum);
        if let Poll::Ready(Ok(count @ 1..)) = &result {
            self.end += *count;
            self.contiguous.take();
        }
        result
    }

    pub async fn read_from<R: AsyncRead + Unpin + ?Sized>(
        &mut self,
        source: &mut R,
        maximum: usize,
    ) -> io::Result<usize> {
        std::future::poll_fn(|cx| self.poll_read_from(cx, Pin::new(&mut *source), maximum)).await
    }
}
