use std::collections::VecDeque;
use std::io::{self, IoSlice, Write};

use brz_ds::{EphemeralBytesArena, EphemeralBytesMut};

use crate::Reader;

const FIRST_SEGMENT_SIZE: usize = 2 * 1024;
const MAX_SEGMENT_SIZE: usize = 64 * 1024;

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
    arena: EphemeralBytesArena,
    segments: VecDeque<EphemeralBytesMut>,
    max_segment_size: usize,
    segment_size: usize,
    limit: usize,
    len: usize,
}

impl Writer {
    /// Create a writer without an application byte limit. The first allocation
    /// is 2 KiB, doubling each time up to 64 KiB, then staying at 64 KiB.
    /// Sizes are also capped to the arena chunk capacity. Empty writes do not
    /// allocate or advance this policy.
    pub fn new(arena: &EphemeralBytesArena) -> Self {
        Self {
            arena: arena.clone(),
            segments: VecDeque::new(),
            max_segment_size: MAX_SEGMENT_SIZE.min(arena.chunk_capacity()),
            segment_size: FIRST_SEGMENT_SIZE.min(arena.chunk_capacity()),
            limit: usize::MAX,
            len: 0,
        }
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
            self.segments
                .into_iter()
                .map(EphemeralBytesMut::freeze)
                .collect(),
            self.len,
        )
    }
}

impl Write for Writer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let accepted = bytes.len().min(self.remaining());
        let mut rest = &bytes[..accepted];
        while !rest.is_empty() {
            if self
                .segments
                .back()
                .is_none_or(|segment| segment.remaining() == 0)
            {
                self.segments
                    .push_back(self.arena.alloc(self.segment_size.min(self.remaining())));
                self.segment_size = self
                    .segment_size
                    .saturating_mul(2)
                    .min(self.max_segment_size);
            }
            let segment = self.segments.back_mut().expect("writable segment");
            let len = rest.len().min(segment.remaining());
            segment.extend_from_slice(&rest[..len]);
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
