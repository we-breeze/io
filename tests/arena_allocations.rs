//! Counts the changed IO hot path only; excludes arena construction and Tokio tasks.
#![allow(unsafe_code)]
use brz_io::{EphemeralBytesArena, Writer};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::io::Write;

thread_local! {
    static ALLOCATIONS: Cell<Option<usize>> = const { Cell::new(None) };
}

fn allocated() {
    let _ = ALLOCATIONS.try_with(|slot| {
        if let Some(count) = slot.get() {
            slot.set(Some(count + 1));
        }
    });
}

struct Counted;
// SAFETY: All operations preserve their arguments and forward ownership to the
// same System allocator. The counter never allocates and tolerates TLS teardown.
unsafe impl GlobalAlloc for Counted {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        allocated();
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        allocated();
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        allocated();
        unsafe { System.realloc(pointer, layout, size) }
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counted = Counted;

fn count<T>(work: impl FnOnce() -> T) -> (T, usize) {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            ALLOCATIONS.with(|slot| slot.set(None));
        }
    }
    ALLOCATIONS.with(|slot| {
        assert!(slot.get().is_none(), "measurement must not be nested");
        slot.set(Some(0));
    });
    let reset = Reset;
    let value = work();
    let allocations = ALLOCATIONS.with(|slot| slot.get().unwrap());
    drop(reset);
    (value, allocations)
}

#[test]
fn counter_has_a_positive_control() {
    let (_, allocations) = count(|| std::hint::black_box(vec![1_u8; 4096]));
    assert!(allocations > 0);
}

#[test]
fn two_segments_and_one_cross_segment_range_need_no_descriptor_heap_allocations() {
    let arena = EphemeralBytesArena::new(1024 * 1024);
    let (_, allocations) = count(|| {
        let mut writer = Writer::with_initial_segment_size(&arena, 8);
        writer.write_all(b"abcdefgh").unwrap();
        writer.reserve_exact(64).unwrap();
        writer.write_all(&[b'z'; 64]).unwrap();
        let reader = writer.into_reader();
        assert_eq!(reader.segment_count(), 2);
        assert_eq!(reader.view().peek_bytes(4, 8).unwrap(), b"efghzzzz");
        assert_eq!(reader.view().peek_bytes(4, 8).unwrap(), b"efghzzzz");
    });
    assert_eq!(
        allocations, 0,
        "arena hit and inline metadata should not call the global allocator"
    );
}

#[test]
fn derived_bytes_can_use_the_single_inline_slot_and_ranges_still_cache() {
    let arena = EphemeralBytesArena::new(1024 * 1024);
    let mut writer = Writer::with_initial_segment_size(&arena, 8);
    writer.write_all(b"abcdefgh").unwrap();
    writer.reserve_exact(8).unwrap();
    writer.write_all(b"ijklmnop").unwrap();
    let reader = writer.into_reader();

    let (decoded, allocations) = count(|| {
        reader
            .store_bytes_with(4, |bytes| bytes.write_all(b"a\nb\t"))
            .unwrap()
    });
    assert_eq!(allocations, 0, "derived bytes can use the inline slot");
    let (first, allocations) = count(|| reader.peek_bytes(4, 8).unwrap());
    assert!(allocations > 0, "decoding already occupied the only slot");
    let (second, allocations) = count(|| reader.peek_bytes(5, 8).unwrap());
    assert!(allocations > 0, "a second range needs a heap owner");
    for _ in 0..32 {
        reader
            .store_bytes_with(8, |bytes| bytes.write_all(b"derived!"))
            .unwrap();
    }
    let (_, allocations) = count(|| {
        assert_eq!(reader.peek_bytes(4, 8).unwrap().as_ptr(), first.as_ptr());
        assert_eq!(reader.peek_bytes(5, 8).unwrap().as_ptr(), second.as_ptr());
    });
    assert_eq!(allocations, 0, "both range caches survive owner growth");
    assert_eq!(
        (decoded, first, second),
        (&b"a\nb\t"[..], &b"efghijkl"[..], &b"fghijklm"[..])
    );
}

#[test]
fn range_occupies_the_shared_slot_until_clear() {
    let arena = EphemeralBytesArena::new(1024 * 1024);
    let mut writer = Writer::with_initial_segment_size(&arena, 4);
    writer.write_all(b"abcd").unwrap();
    writer.reserve_exact(4).unwrap();
    writer.write_all(b"efgh").unwrap();
    let mut reader = writer.into_reader();
    let (range, allocations) = count(|| reader.read_str(6).unwrap());
    assert_eq!(allocations, 0);
    let (decoded, allocations) = count(|| {
        reader
            .store_bytes_with(2, |bytes| bytes.write_all(b"a\n"))
            .unwrap()
    });
    assert!(
        allocations > 0,
        "the cross-segment string occupied the slot"
    );
    assert_eq!((range, decoded), ("abcdef", &b"a\n"[..]));

    reader.clear();
    let (decoded, allocations) = count(|| {
        reader
            .store_bytes_with(2, |bytes| bytes.write_all(b"b\n"))
            .unwrap()
    });
    assert_eq!(allocations, 0, "clear makes the inline slot reusable");
    assert_eq!(decoded, b"b\n");
}

#[test]
fn explicit_overflow_is_still_supported() {
    let arena = EphemeralBytesArena::new(1024 * 1024);
    let (_, allocations) = count(|| {
        let mut writer = Writer::with_initial_segment_size(&arena, 2);
        writer.write_all(b"ab").unwrap();
        writer.reserve_exact(2).unwrap();
        writer.write_all(b"cd").unwrap();
        writer.reserve_exact(2).unwrap();
        writer.write_all(b"ef").unwrap();
        assert_eq!(writer.segment_count(), 3);
        assert_eq!(writer.into_reader().as_slice(), b"abcdef");
    });
    assert!(
        allocations > 0,
        "third descriptor is a documented fallback, not zero-allocation"
    );
}

#[test]
fn repeated_cross_segment_body_after_inline_cache_overflow_does_not_reallocate() {
    let arena = EphemeralBytesArena::new(1024 * 1024);
    let mut writer = Writer::with_initial_segment_size(&arena, 8);
    writer.write_all(b"abcdefgh").unwrap();
    writer.reserve_exact(64).unwrap();
    writer.write_all(&[b'z'; 64]).unwrap();
    let reader = writer.into_reader();
    for offset in [4, 5, 6, 7] {
        reader.peek_bytes(offset, 8).unwrap();
    }
    let body = reader.view().slice(2..72).unwrap();
    let first = body.as_slice();
    let (_, allocations) = count(|| {
        for _ in 0..10 {
            assert_eq!(body.as_slice().as_ptr(), first.as_ptr());
        }
    });
    assert_eq!(
        allocations, 0,
        "overflow cache must reuse the first merged body"
    );
}
