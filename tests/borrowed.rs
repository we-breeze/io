use std::io::{self, BufRead, Read, Write};

use brz_ds::EphemeralBytesArena;
use brz_io::{Reader, Writer};

fn reader(data: &[u8], segment: usize) -> Reader {
    let arena = EphemeralBytesArena::new(segment);
    let mut output = Writer::with_limit(&arena, data.len());
    output.write_all(data).unwrap();
    output.into_reader()
}

#[test]
fn borrowed_reads_advance_and_keep_multiple_results_alive() {
    let input = reader(b"abcDEFGhijkLMNOP", 3);
    let first = input.read_str(3).unwrap();
    let second = input.read_str(4).unwrap();
    let third = input.read_bytes(4).unwrap();
    let fourth = input.read_str(5).unwrap();
    assert_eq!(
        (first, second, third, fourth),
        ("abc", "DEFG", &b"hijk"[..], "LMNOP")
    );
    assert_eq!(input.position(), 16);
    assert_eq!(input.len(), 0);
}

#[test]
fn in_segment_reads_borrow_original_storage() {
    let mut input = reader(b"abcdef", 6);
    let base = input.fill_buf().unwrap().as_ptr();
    input.skip(1).unwrap();
    let value = input.read_str(3).unwrap();
    assert_eq!(value.as_ptr(), base.wrapping_add(1));
    assert_eq!(value, "bcd");
}

#[test]
fn peek_and_failed_reads_leave_cursor_unchanged() {
    let input = reader(b"abc\xffdef", 2);
    assert_eq!(input.peek_bytes(1, 4).unwrap(), b"bc\xffd");
    assert_eq!(input.position(), 0);
    assert_eq!(
        input.read_str(4).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
    assert_eq!(input.position(), 0);
    assert_eq!(
        input.read_bytes(8).unwrap_err().kind(),
        io::ErrorKind::UnexpectedEof
    );
    assert!(input.peek_bytes(usize::MAX, 2).is_err());
    assert!(input.skip(usize::MAX).is_err());
    assert_eq!(input.position(), 0);
    assert_eq!(input.read_bytes(7).unwrap(), b"abc\xffdef");
}

#[test]
fn retained_storage_and_standard_io_share_cursor_safely() {
    let mut input = reader(b"abcdefghijkl", 2);
    let a = input.read_str(3).unwrap();
    let b = input
        .store_bytes_with(4, |bytes| bytes.write_all(b"ABCD"))
        .unwrap();
    let c = input.read_str(4).unwrap();
    assert_eq!((a, b, c), ("abc", &b"ABCD"[..], "defg"));
    let mut tail = String::new();
    input.read_to_string(&mut tail).unwrap();
    assert_eq!(tail, "hijkl");
    assert_eq!(input.position(), 12);
    assert!(input.read_str(0).unwrap().is_empty());
}

#[test]
fn zero_copy_and_merged_references_survive_storage_growth() {
    let input = reader(&[b'x'; 8192], 3);
    let first = input.read_str(2).unwrap();
    let merged = input.read_str(5).unwrap();
    for _ in 0..1000 {
        input.read_str(7).unwrap();
    }
    assert_eq!(first, "xx");
    assert_eq!(merged, "xxxxx");
}

#[test]
fn views_keep_their_bounds_when_the_source_cursor_advances() {
    let mut input = reader(b"xxabcdefgh", 3);
    input.skip(2).unwrap();
    let view = input.view();
    let first = view.peek_bytes(0, 4).unwrap();
    input.skip(6).unwrap();
    assert_eq!(view.len(), 8);
    assert_eq!(view.peek_byte(0), Some(b'a'));
    assert_eq!(view.peek_bytes(0, 8).unwrap(), b"abcdefgh");
    assert_eq!(view.peek_bytes(8, 0).unwrap(), b"");
    assert_eq!(view.peek_byte(8), None);
    assert!(view.peek_bytes(usize::MAX, 1).is_err());
    assert!(view.peek_bytes(7, 2).is_err());
    assert_eq!(first, b"abcd");
    let mut remaining = String::new();
    input.read_to_string(&mut remaining).unwrap();
    assert_eq!(remaining, "gh");
}

#[test]
fn contiguous_cache_survives_shared_reads_and_resets_after_exclusive_reads() {
    let mut input = reader(b"abcdefghijkl", 3);
    let all = input.as_slice();
    assert_eq!(all, b"abcdefghijkl");
    assert_eq!(input.as_slice().as_ptr(), all.as_ptr());
    input.skip(2).unwrap();
    assert_eq!(input.as_slice(), b"cdefghijkl");
    assert_eq!(input.as_slice().as_ptr(), all.as_ptr().wrapping_add(2));
    assert_eq!(all, b"abcdefghijkl");
    input.read_exact(&mut [0; 3]).unwrap();
    assert_eq!(input.as_slice(), b"fghijkl");
    input.skip(7).unwrap();
    assert!(input.as_slice().is_empty());
}

#[test]
fn contiguous_slice_in_one_segment_borrows_original_storage() {
    let mut input = reader(b"abcdef", 6);
    let original = input.fill_buf().unwrap().as_ptr();
    assert_eq!(input.as_slice().as_ptr(), original);
    assert!(reader(b"", 1).as_slice().is_empty());
}
