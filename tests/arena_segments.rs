use brz_io::{EphemeralBytesArena, Writer};
use std::io::{BufRead, Read, Write};

#[test]
fn exact_reservation_is_not_a_length_limit_and_does_not_relocate() {
    let arena = EphemeralBytesArena::new(64 * 1024);
    let mut writer = Writer::with_limit(&arena, 8192);
    writer.reserve_exact(0).unwrap();
    assert_eq!(writer.segment_count(), 0);
    writer.write_all(b"prefix").unwrap();
    writer.reserve_exact(3000).unwrap();
    assert_eq!(writer.len(), 6);
    assert_eq!(writer.segment_count(), 2);
    writer.write_all(&[b'z'; 3000]).unwrap();
    assert!(writer.reserve_exact(8192).is_err());
    assert_eq!(writer.len(), 3006);
    let mut reader = writer.into_reader();
    let first = reader.view().peek_bytes(0, 6).unwrap().as_ptr();
    assert_eq!(reader.view().chunk_at(0), b"prefix");
    let range = reader.view().peek_bytes(4, 6).unwrap();
    assert_eq!(range, b"ixzzzz");
    let merged = range.as_ptr();
    assert_eq!(reader.view().peek_bytes(4, 6).unwrap().as_ptr(), merged);
    assert_eq!(reader.view().peek_bytes(0, 6).unwrap().as_ptr(), first);
    let body = reader.view().slice(6..3006).unwrap();
    assert_eq!(body.len(), 3000);
    assert_eq!(body.chunk_at(2999), b"z");
    assert_eq!(body.peek_byte(3000), None);
    assert!(body.peek_bytes(2999, 2).is_err());
    assert!(body.slice(0..3001).is_err());
    reader.consume(6);
    assert_eq!(reader.segment_count(), 1);
    assert_eq!(reader.position(), 6);
    reader.consume(3000);
    assert!(reader.is_empty());
    assert_eq!(reader.segment_count(), 0);
    reader.clear();
    assert_eq!(reader.position(), 0);
}

#[test]
fn standard_io_and_many_segments_preserve_wire_order() {
    let arena = EphemeralBytesArena::new(7);
    let mut writer = Writer::new(&arena);
    let original: Vec<u8> = (0..=255).cycle().take(1025).collect();
    for bytes in original.chunks(11) {
        writer.write_all(bytes).unwrap();
    }
    let mut reader = writer.into_reader();
    assert!(reader.segment_count() > 2);
    let mut received = Vec::new();
    reader.read_to_end(&mut received).unwrap();
    assert_eq!(received, original);
    assert_eq!(reader.position(), 1025);
    assert_eq!(reader.segment_count(), 0);
}

#[cfg(feature = "tokio")]
mod asynchronous {
    use super::*;
    use std::io;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::io::{AsyncRead, ReadBuf};

    struct Fragmented<'a> {
        bytes: &'a [u8],
        maximum: usize,
        pending: bool,
        first_address: Option<usize>,
    }
    impl AsyncRead for Fragmented<'_> {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            out: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            if self.pending {
                self.pending = false;
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
            let n = self.bytes.len().min(self.maximum).min(out.remaining());
            let address = out.initialize_unfilled_to(n).as_ptr() as usize;
            if self.first_address.is_none() {
                self.first_address = Some(address);
            }
            out.put_slice(&self.bytes[..n]);
            self.bytes = &self.bytes[n..];
            self.pending = true;
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn direct_receive_preserves_pointer_across_reserve_and_partial_reads() {
        let arena = EphemeralBytesArena::new(64 * 1024);
        let data = [b'a'; 5000];
        let mut source = Fragmented {
            bytes: &data,
            maximum: 300,
            pending: true,
            first_address: None,
        };
        let mut reader = Writer::new(&arena).into_reader();
        let n = reader.read_from(&mut source, data.len()).await.unwrap();
        assert_eq!(n, 300);
        let prefix_address = reader.view().peek_bytes(0, n).unwrap().as_ptr() as usize;
        assert_eq!(
            Some(prefix_address),
            source.first_address,
            "read target is final arena storage"
        );
        reader.reserve_exact(data.len() - n).unwrap();
        assert_eq!(reader.len(), n, "reserve must not publish bytes");
        while reader.len() < data.len() {
            let remaining = data.len() - reader.len();
            assert!(reader.read_from(&mut source, remaining).await.unwrap() > 0);
        }
        assert_eq!(reader.segment_count(), 2);
        assert_eq!(
            reader.view().peek_bytes(0, n).unwrap().as_ptr() as usize,
            prefix_address
        );
        assert_eq!(reader.view().peek_bytes(290, 20).unwrap(), &[b'a'; 20]);
        assert_eq!(reader.len(), 5000);
        reader.consume(5000);
        reader.clear();
        assert_eq!(reader.segment_count(), 0);
    }

    #[tokio::test]
    async fn bounded_append_preserves_pipelined_suffix_and_cached_header() {
        let arena = EphemeralBytesArena::new(4096);
        let mut writer = Writer::with_initial_segment_size(&arena, 4);
        writer.write_all(b"HEADbodyNEXT").unwrap();
        let mut reader = writer.into_reader();
        let header = reader.view().peek_bytes(0, 6).unwrap().as_ptr();
        let mut more = &b"tail"[..];
        reader.reserve_exact(4).unwrap();
        assert_eq!(reader.read_from(&mut more, 4).await.unwrap(), 4);
        assert_eq!(
            reader.view().peek_bytes(0, 6).unwrap().as_ptr(),
            header,
            "append preserves immutable cached ranges"
        );
        let body = reader.view().slice(4..8).unwrap();
        assert_eq!(body.as_slice(), b"body");
        assert_eq!(body.peek_byte(4), None);
        reader.consume(8);
        assert_eq!(reader.view().as_slice(), b"NEXTtail");
    }
}

#[test]
fn impossible_reservation_preserves_empty_tail_and_initialized_prefix() {
    let arena = EphemeralBytesArena::new(64);
    let mut writer = Writer::new(&arena);
    writer.reserve_exact(8).unwrap();
    assert!(writer.reserve_exact(usize::MAX).is_err());
    assert_eq!(writer.segment_count(), 1);
    writer.write_all(b"abc").unwrap();
    let mut reader = writer.into_reader();
    reader.reserve_exact(64).unwrap();
    assert_eq!(reader.segment_count(), 2);
    assert!(reader.reserve_exact(isize::MAX as usize + 1).is_err());
    assert_eq!(reader.segment_count(), 2);
    assert_eq!(reader.as_slice(), b"abc");
    reader.consume(3);
    assert!(reader.is_empty());
    assert_eq!(reader.view().chunk_at(0), b"");
}

#[test]
fn shared_cached_ranges_remain_stable_under_concurrent_reads() {
    let arena = EphemeralBytesArena::new(4096);
    let mut writer = Writer::with_initial_segment_size(&arena, 4);
    writer.write_all(b"abcdefghijklmnopqrstuvwxyz").unwrap();
    let reader = writer.into_reader();
    let first = reader.peek_bytes(2, 10).unwrap();
    std::thread::scope(|scope| {
        for _ in 0..8 {
            scope.spawn(|| {
                for _ in 0..100 {
                    assert_eq!(reader.peek_bytes(2, 10).unwrap(), b"cdefghijkl");
                    assert_eq!(reader.peek_bytes(2, 10).unwrap().as_ptr(), first.as_ptr());
                    assert_eq!(reader.peek_bytes(10, 12).unwrap(), b"klmnopqrstuv");
                }
            });
        }
    });
    assert_eq!(first, b"cdefghijkl");
}
