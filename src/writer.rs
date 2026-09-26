use std::io::{self, IoSlice, Write};

use crate::segments::Segments;
use brz_ds::EphemeralBytesArena;

use crate::Reader;

const FIRST_SEGMENT_SIZE: usize = 2 * 1024;

/// A grow-on-demand byte stream backed by arena allocations.
///
/// No payload storage is allocated until the first nonempty write. Writes
/// append across segments without moving existing bytes. Once an explicit limit is reached,
/// `write` returns `Ok(0)` and `write_all` reports `WriteZero`; already written
/// data remains available through [`Self::into_reader`]. To stop copying at a
/// limit successfully, also bound the source using `Read::take`.
///
/// `flush` is a no-op: every successful write is already stored in memory.
#[derive(Debug)]
pub struct Writer {
    pub(crate) arena: EphemeralBytesArena,
    pub(crate) segments: Segments,
    pub(crate) segment_size: usize,
    initial_segment_size: usize,
    pub(crate) limit: usize,
    pub(crate) len: usize,
}

impl Writer {
    /// Create a writer without an application byte limit. The first allocation
    /// is 2 KiB, doubling each time up to 64 KiB, then staying at 64 KiB.
    /// Sizes are also capped to the arena chunk capacity. Empty writes do not
    /// allocate or advance this policy.
    pub fn new(arena: &EphemeralBytesArena) -> Self {
        Self::with_initial_segment_size(arena, FIRST_SEGMENT_SIZE)
    }

    /// Sets the first implicit segment size (clamped to the arena chunk size).
    /// Does not allocate payloads or descriptor storage. `size` must be nonzero.
    pub fn with_initial_segment_size(arena: &EphemeralBytesArena, size: usize) -> Self {
        assert!(size > 0, "initial segment size must be positive");
        let size = size.min(arena.chunk_capacity());
        Self {
            arena: arena.clone(),
            segments: Segments::new(),
            segment_size: size,
            initial_segment_size: size,
            limit: usize::MAX,
            len: 0,
        }
    }

    /// Guarantees room for `additional` more bytes in the writable tail.
    /// Does NOT change len, initialize payloads, or relocate the old prefix.
    /// An insufficient tail is abandoned and one exact-sized segment is added.
    /// Unlike implicit growth, this is not capped at 64 KiB. Arena heap fallback
    /// remains possible; callers must check their resource budget before reserving.
    pub fn reserve_exact(&mut self, additional: usize) -> io::Result<()> {
        if additional > self.remaining() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "reservation exceeds writer limit",
            ));
        }
        self.segments
            .reserve_exact(&self.arena, self.len, additional)
    }

    /// Payload segments, including a currently reserved, possibly empty tail.
    pub fn segment_count(&self) -> usize {
        self.segments.len()
    }

    /// Use the default allocation policy while accepting at most `max_bytes`
    /// bytes, including zero. Allocations are capped to the remaining budget.
    pub fn with_limit(arena: &EphemeralBytesArena, max_bytes: usize) -> Self {
        let mut writer = Self::new(arena);
        writer.limit = max_bytes;
        writer
    }

    /// Number of bytes stored so far.
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Number of additional bytes this writer will accept.
    pub fn remaining(&self) -> usize {
        self.limit - self.len
    }

    /// Transfer the initialized segments into a reader without copying bytes.
    ///
    /// The returned reader owns its storage and can outlive the arena handle.
    pub fn into_reader(self) -> Reader {
        Reader::new(
            self.arena,
            self.segments,
            self.len,
            self.initial_segment_size,
            self.segment_size,
        )
    }
}

impl Write for Writer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let accepted = bytes.len().min(self.remaining());
        let mut rest = &bytes[..accepted];
        while !rest.is_empty() {
            let remaining = self.remaining();
            self.segments
                .ensure_tail(&self.arena, self.len, remaining, &mut self.segment_size)?;
            let segment = self.segments.back_mut().expect("writable segment");
            let len = rest.len().min(segment.bytes.remaining());
            segment.bytes.extend_from_slice(&rest[..len]);
            self.len += len;
            rest = &rest[len..];
        }
        Ok(accepted)
    }

    fn write_vectored(&mut self, buffers: &[IoSlice<'_>]) -> io::Result<usize> {
        let mut total = 0;
        for buffer in buffers {
            let written = self.write(buffer)?;
            total += written;
            if written < buffer.len() {
                break;
            }
        }
        Ok(total)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
