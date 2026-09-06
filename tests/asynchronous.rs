#![cfg(feature = "tokio")]

use std::io::{self, IoSlice};
use std::mem::MaybeUninit;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

use brz_ds::EphemeralBytesArena;
use brz_io::Writer;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};

fn writer(limit: usize, segment: usize) -> Writer {
    Writer::with_limit(&EphemeralBytesArena::new(segment), limit)
}

#[tokio::test]
async fn asynchronous_copy_accepts_a_source_that_waits_for_data() {
    let (mut sender, receiver) = tokio::io::duplex(2);
    let producer = tokio::spawn(async move {
        sender.write_all("你好\r\nworld".as_bytes()).await.unwrap();
        sender.shutdown().await.unwrap();
    });
    let mut output = writer(64, 2);
    assert_eq!(
        tokio::io::copy(&mut receiver.take(64), &mut output)
            .await
            .unwrap(),
        13
    );
    producer.await.unwrap();
    output.flush().await.unwrap();
    output.shutdown().await.unwrap();
    let mut input = output.into_reader();
    let mut line = String::new();
    input.read_line(&mut line).await.unwrap();
    assert_eq!(line, "你好\r\n");
    let mut rest = Vec::new();
    tokio::io::copy(&mut input, &mut rest).await.unwrap();
    assert_eq!(rest, b"world");
}

#[tokio::test]
async fn asynchronous_limit_and_vectored_write_match_synchronous_behavior() {
    let mut output = writer(5, 2);
    assert!(output.is_write_vectored());
    let buffers = [
        IoSlice::new(b""),
        IoSlice::new(b"abc"),
        IoSlice::new(b"def"),
    ];
    assert_eq!(output.write_vectored(&buffers).await.unwrap(), 5);
    assert_eq!(
        output.write_all(b"x").await.unwrap_err().kind(),
        io::ErrorKind::WriteZero
    );
    let mut input = output.into_reader();
    let mut bytes = [0; 5];
    input.read_exact(&mut bytes).await.unwrap();
    assert_eq!(&bytes, b"abcde");
    assert_eq!(
        input.read_exact(&mut [0]).await.unwrap_err().kind(),
        io::ErrorKind::UnexpectedEof
    );
}

#[tokio::test]
async fn take_preserves_input_after_limit_and_handles_zero_limit() {
    for limit in [0, 1, 5, 10, 20] {
        let mut source = &b"0123456789"[..];
        let mut output = writer(limit, 3);
        let count = tokio::io::copy(&mut (&mut source).take(limit as u64), &mut output)
            .await
            .unwrap();
        assert_eq!(count, limit.min(10) as u64);
        assert_eq!(source, &b"0123456789"[count as usize..]);
        assert_eq!(output.len(), count as usize);
    }
}

#[tokio::test]
async fn synchronous_and_asynchronous_access_share_one_cursor() {
    let mut output = writer(7, 2);
    std::io::Write::write_all(&mut output, b"abc").unwrap();
    output.write_all(b"defg").await.unwrap();
    let mut input = output.into_reader();
    assert_eq!(input.fill_buf().await.unwrap(), b"ab");
    input.consume(1);
    let mut bytes = [0; 3];
    std::io::Read::read_exact(&mut input, &mut bytes).unwrap();
    assert_eq!(&bytes, b"bcd");
    let mut tail = String::new();
    input.read_to_string(&mut tail).await.unwrap();
    assert_eq!(tail, "efg");
}

#[test]
fn poll_read_initializes_only_filled_bytes_and_preserves_the_prefix() {
    let mut output = writer(6, 2);
    std::io::Write::write_all(&mut output, b"abcdef").unwrap();
    let mut input = output.into_reader();
    let mut memory = [MaybeUninit::uninit(); 5];
    let mut buffer = ReadBuf::uninit(&mut memory);
    buffer.put_slice(b"!");
    let mut cx = Context::from_waker(Waker::noop());
    assert!(matches!(
        Pin::new(&mut input).poll_read(&mut cx, &mut buffer),
        Poll::Ready(Ok(()))
    ));
    assert_eq!(buffer.filled(), b"!abcd");
    assert_eq!(input.len(), 2);
    assert!(matches!(
        Pin::new(&mut input).poll_read(&mut cx, &mut buffer),
        Poll::Ready(Ok(()))
    ));
    assert_eq!(input.len(), 2);
}

#[tokio::test]
async fn cancelling_copy_keeps_bytes_already_accepted_by_writer() {
    let (mut sender, mut receiver) = tokio::io::duplex(8);
    sender.write_all(b"abc").await.unwrap();
    let mut output = writer(10, 2);
    {
        let mut copy = Box::pin(tokio::io::copy(&mut receiver, &mut output));
        let mut cx = Context::from_waker(Waker::noop());
        assert!(std::future::Future::poll(copy.as_mut(), &mut cx).is_pending());
    }
    assert_eq!(output.len(), 3);
    sender.write_all(b"def").await.unwrap();
    sender.shutdown().await.unwrap();
    tokio::io::copy(&mut receiver, &mut output).await.unwrap();
    let mut text = String::new();
    output
        .into_reader()
        .read_to_string(&mut text)
        .await
        .unwrap();
    assert_eq!(text, "abcdef");
}
