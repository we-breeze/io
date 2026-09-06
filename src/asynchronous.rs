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
