use std::io::{self, BufRead, IoSlice, IoSliceMut, Read, Write};

use brz_ds::EphemeralBytesArena;
use brz_io::{Reader, Writer};

fn writer(limit: usize, segment: usize) -> Writer {
    Writer::with_limit(&EphemeralBytesArena::new(segment), limit)
}

#[test]
fn incremental_writes_and_reads_cross_many_segments() {
    let data: Vec<u8> = (0..251).cycle().take(4097).collect();
    for segment in [1, 3, 16, 64] {
        let mut output = writer(data.len(), segment);
        for part in data.chunks(11) {
            output.write_all(part).unwrap();
        }
        assert_eq!(output.len(), data.len());
        assert_eq!(output.remaining(), 0);
        let mut input = output.into_reader();
        let mut actual = Vec::new();
        let mut buffer = [0; 37];
        loop {
            let count = input.read(&mut buffer).unwrap();
            if count == 0 {
                break;
            }
            actual.extend_from_slice(&buffer[..count]);
            assert_eq!(input.len(), data.len() - actual.len());
        }
        assert_eq!(actual, data);
        assert!(input.is_empty());
        assert_eq!(input.read(&mut buffer).unwrap(), 0);
        assert!(input.fill_buf().unwrap().is_empty());
    }
}

#[test]
fn fill_buf_borrows_current_segment_and_consume_advances() {
    let mut output = writer(10, 3);
    output.write_all(b"abcdefgh").unwrap();
    let mut input = output.into_reader();
    let original = input.fill_buf().unwrap().as_ptr();
    assert_eq!(input.fill_buf().unwrap(), b"abc");
    input.consume(1);
    assert_eq!(input.fill_buf().unwrap(), b"bc");
    assert_eq!(input.fill_buf().unwrap().as_ptr(), original.wrapping_add(1));
    input.consume(2);
    assert_eq!(input.fill_buf().unwrap(), b"def");
    let mut rest = Vec::new();
    input.read_to_end(&mut rest).unwrap();
    assert_eq!(rest, b"defgh");
}

#[test]
fn lines_and_utf8_work_across_single_byte_segments() {
    let text = "你好\r\nworld\n末尾";
    let mut output = writer(text.len(), 1);
    output.write_all(text.as_bytes()).unwrap();
    let mut input = output.into_reader();
    let mut line = String::new();
    assert_eq!(input.read_line(&mut line).unwrap(), "你好\r\n".len());
    assert_eq!(line, "你好\r\n");
    let mut rest = String::new();
    input.read_to_string(&mut rest).unwrap();
    assert_eq!(rest, "world\n末尾");
}

#[test]
fn read_exact_handles_boundaries_and_reports_short_input() {
    let mut output = writer(8, 3);
    output
        .write_all(&0x0102030405060708u64.to_be_bytes())
        .unwrap();
    let mut input = output.into_reader();
    let mut bytes = [0; 8];
    input.read_exact(&mut bytes).unwrap();
    assert_eq!(u64::from_be_bytes(bytes), 0x0102030405060708);
    assert_eq!(
        input.read_exact(&mut [0]).unwrap_err().kind(),
        io::ErrorKind::UnexpectedEof
    );
}

#[test]
fn capacity_exhaustion_uses_standard_short_write_semantics() {
    let mut output = writer(5, 3);
    assert_eq!(output.write(b"abc").unwrap(), 3);
    assert_eq!(output.write(b"defg").unwrap(), 2);
    assert_eq!(output.write(b"x").unwrap(), 0);
    assert_eq!(
        output.write_all(b"x").unwrap_err().kind(),
        io::ErrorKind::WriteZero
    );
    output.write_all(b"").unwrap();
    output.flush().unwrap();
    let mut bytes = Vec::new();
    output.into_reader().read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"abcde");
}

#[test]
fn bounded_copy_stops_without_consuming_the_next_byte() {
    for limit in [0, 1, 5, 10, 20] {
        let mut source = io::Cursor::new(b"0123456789");
        let mut output = writer(limit, 3);
        let count = io::copy(&mut source.by_ref().take(limit as u64), &mut output).unwrap();
        assert_eq!(count, limit.min(10) as u64);
        assert_eq!(source.position(), count);
        let mut bytes = Vec::new();
        output.into_reader().read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, &b"0123456789"[..count as usize]);
    }
}

#[test]
fn vectored_operations_cover_empty_slices_and_boundaries() {
    let mut output = writer(7, 2);
    let inputs = [
        IoSlice::new(b""),
        IoSlice::new(b"abc"),
        IoSlice::new(b"defghi"),
    ];
    assert_eq!(output.write_vectored(&inputs).unwrap(), 7);
    let mut input = output.into_reader();
    let mut a = [0; 2];
    let mut b = [0xff; 7];
    let mut buffers = [
        IoSliceMut::new(&mut []),
        IoSliceMut::new(&mut a),
        IoSliceMut::new(&mut b),
    ];
    assert_eq!(input.read_vectored(&mut buffers).unwrap(), 7);
    assert_eq!(&a, b"ab");
    assert_eq!(&b, b"cdefg\xff\xff");
}

#[test]
fn empty_operations_do_not_consume_data() {
    let mut output = writer(3, 2);
    output.write_all(b"abc").unwrap();
    assert_eq!(output.write(&[]).unwrap(), 0);
    assert_eq!(output.write_vectored(&[]).unwrap(), 0);
    let mut input = output.into_reader();
    assert_eq!(input.read(&mut []).unwrap(), 0);
    assert_eq!(input.read_vectored(&mut []).unwrap(), 0);
    assert_eq!(input.len(), 3);
    input.consume(0);
    assert_eq!(input.fill_buf().unwrap(), b"ab");
    let mut empty = writer(0, 1).into_reader();
    assert_eq!(empty.read(&mut [0]).unwrap(), 0);
}

#[test]
fn retained_reader_survives_arena_reuse_and_handle_drop() {
    let arena = EphemeralBytesArena::new(4);
    let mut first = Writer::with_limit(&arena, 12);
    first.write_all(b"hello world!").unwrap();
    let mut input = first.into_reader();
    for _ in 0..20 {
        let mut other = Writer::new(&arena);
        other.write_all(&[0xff; 32]).unwrap();
    }
    drop(arena);
    let mut text = String::new();
    input.read_to_string(&mut text).unwrap();
    assert_eq!(text, "hello world!");
}

#[test]
fn consuming_a_segment_releases_its_arena_ticket() {
    let arena = EphemeralBytesArena::new(16);
    let mut output = Writer::new(&arena);
    output.write_all(&[0x5a; 32]).unwrap();
    let mut input = output.into_reader();
    // Freeze both full chunks while the reader holds their allocations.
    assert!(arena.alloc(16).is_heap_allocated());
    input.consume(16);
    let mut reused = arena.alloc(16);
    assert!(!reused.is_heap_allocated());
    reused.write_all(&[0xff; 16]).unwrap();
    let mut tail = Vec::new();
    input.read_to_end(&mut tail).unwrap();
    assert_eq!(tail, [0x5a; 16]);
}

#[test]
fn copy_retries_interrupted_input_and_retains_data_on_error() {
    struct Source(usize);
    impl Read for Source {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            self.0 += 1;
            match self.0 {
                1 => Err(io::ErrorKind::Interrupted.into()),
                2 => {
                    output[..3].copy_from_slice(b"abc");
                    Ok(3)
                }
                _ => Err(io::ErrorKind::ConnectionReset.into()),
            }
        }
    }
    let mut output = writer(10, 2);
    assert_eq!(
        io::copy(&mut Source(0), &mut output).unwrap_err().kind(),
        io::ErrorKind::ConnectionReset
    );
    let mut bytes = Vec::new();
    output.into_reader().read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"abc");
}

#[test]
fn reader_and_writer_can_move_between_threads() {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<Reader>();
    send_sync::<Writer>();
    let mut output = writer(8, 3);
    output.write_all(b"thread").unwrap();
    let bytes = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        output.into_reader().read_to_end(&mut bytes).unwrap();
        bytes
    })
    .join()
    .unwrap();
    assert_eq!(bytes, b"thread");
}

#[test]
fn default_segments_double_until_64k_across_write_sizes() {
    let data: Vec<u8> = (0..251).cycle().take(257 * 1024).collect();
    for write_size in [1, 1000, data.len()] {
        let arena = EphemeralBytesArena::new(512 * 1024);
        let mut output = Writer::new(&arena);
        output.write_all(&[]).unwrap();
        for part in data.chunks(write_size) {
            output.write_all(part).unwrap();
        }
        let mut input = output.into_reader();
        let mut offset = 0;
        for size in [2, 4, 8, 16, 32, 64, 64, 64, 3].map(|kib| kib * 1024) {
            assert_eq!(input.fill_buf().unwrap(), &data[offset..offset + size]);
            input.consume(size);
            offset += size;
        }
        assert!(input.is_empty());
    }
}

#[test]
fn default_policy_respects_arena_capacity_and_explicit_byte_limit() {
    for (chunk_size, limit, expected_segments) in [
        (32768, 0, vec![]),
        (32768, 100, vec![100]),
        (32768, 7000, vec![2048, 4096, 856]),
        (3000, 7000, vec![2048, 3000, 1952]),
        (10000, 25000, vec![2048, 4096, 8192, 10000, 664]),
        (1024, 2500, vec![1024, 1024, 452]),
    ] {
        let arena = EphemeralBytesArena::new(chunk_size);
        let mut output = Writer::with_limit(&arena, limit);
        assert_eq!(output.write(&vec![7; limit + 1]).unwrap(), limit);
        assert_eq!(output.remaining(), 0);
        assert_eq!(
            output.write_all(b"x").unwrap_err().kind(),
            io::ErrorKind::WriteZero
        );
        let mut input = output.into_reader();
        for size in expected_segments {
            assert_eq!(input.fill_buf().unwrap(), vec![7; size]);
            input.consume(size);
        }
        assert!(input.is_empty());
    }
}
