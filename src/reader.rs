use std::collections::VecDeque;
use std::io::{self, BufRead, IoSliceMut, Read};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use brz_ds::{EphemeralBytes, EphemeralBytesArena, EphemeralBytesMut};
use elsa::sync::FrozenVec;

#[derive(Debug)]
struct Segment {
    start: usize,
    bytes: EphemeralBytes,
}

/// An owned, segmented byte reader supporting both standard IO and borrowed reads.
///
/// `read_bytes` and `read_str` advance a shared cursor while their references
/// remain valid. Source segments and extra allocations remain pinned until
/// exclusive access resumes or the reader is dropped. Standard mutable IO
/// reads reclaim fully consumed segments; `fill_buf` returns the current piece.
#[derive(Debug)]
pub struct Reader {
    arena: EphemeralBytesArena,
    segments: VecDeque<Segment>,
    cursor: AtomicUsize,
    end: usize,
    retained: FrozenVec<Box<EphemeralBytes>>,
    contiguous: OnceLock<EphemeralBytes>,
}

impl Reader {
    pub(crate) fn new(
        arena: EphemeralBytesArena,
        segments: VecDeque<EphemeralBytes>,
        len: usize,
    ) -> Self {
        let mut start = 0;
        let segments = segments
            .into_iter()
            .map(|bytes| {
                let segment = Segment { start, bytes };
                start += segment.bytes.len();
                segment
            })
            .collect();
        Self {
            arena,
            segments,
            cursor: AtomicUsize::new(0),
            end: len,
            retained: FrozenVec::new(),
            contiguous: OnceLock::new(),
        }
    }

    /// Current byte position, measured from the original start of this reader.
    pub fn position(&self) -> usize {
        self.cursor.load(Ordering::Relaxed)
    }

    /// Number of unread bytes.
    pub fn len(&self) -> usize {
        self.end - self.position()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Borrow all unread bytes without advancing. A single remaining segment
    /// is borrowed directly; otherwise retained segments are merged once and
    /// cached until exclusive IO resumes. Shared reads preserve this cache.
    pub fn as_slice(&self) -> &[u8] {
        let position = self.position();
        if position == self.end {
            return &[];
        }
        let index = self.locate(position);
        let segment = &self.segments[index];
        if index + 1 == self.segments.len() {
            return &segment.bytes[position - segment.start..];
        }
        // Cache from the first retained segment, independent of the shared
        // cursor. Concurrent callers can therefore safely reuse the same copy.
        let start = self.segments[0].start;
        let bytes = self.contiguous.get_or_init(|| {
            let mut output = self.arena.alloc(self.end - start);
            for segment in &self.segments {
                output.extend_from_slice(&segment.bytes);
            }
            output.freeze()
        });
        &bytes[position - start..]
    }

    fn locate(&self, position: usize) -> usize {
        self.segments
            .partition_point(|segment| segment.start <= position)
            .saturating_sub(1)
    }

    /// Inspect one byte relative to the current cursor without advancing it.
    pub fn peek_byte(&self, offset: usize) -> Option<u8> {
        let position = self.position().checked_add(offset)?;
        self.byte_at(position)
    }

    fn byte_at(&self, position: usize) -> Option<u8> {
        if position >= self.end {
            return None;
        }
        let segment = &self.segments[self.locate(position)];
        Some(segment.bytes[position - segment.start])
    }

    /// Snapshot the unread range without moving the cursor. The view keeps its
    /// original bounds even if shared reads subsequently advance this reader.
    pub fn view(&self) -> ReaderView<'_> {
        ReaderView {
            reader: self,
            start: self.position(),
        }
    }

    fn range(&self, position: usize, len: usize) -> io::Result<&[u8]> {
        if position.checked_add(len).is_none_or(|end| end > self.end) {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        if len == 0 {
            return Ok(&[]);
        }
        let index = self.locate(position);
        let first = &self.segments[index];
        let offset = position - first.start;
        if len <= first.bytes.len() - offset {
            return Ok(&first.bytes[offset..offset + len]);
        }
        self.store_bytes_with(len, |output| {
            let mut remaining = len;
            for (i, segment) in self.segments.iter().enumerate().skip(index) {
                let start = if i == index { offset } else { 0 };
                let count = remaining.min(segment.bytes.len() - start);
                output.extend_from_slice(&segment.bytes[start..start + count]);
                remaining -= count;
                if remaining == 0 {
                    break;
                }
            }
            Ok(())
        })
    }

    /// Borrow a range relative to the current cursor without consuming it.
    /// Cross-segment ranges are copied into a retained arena allocation.
    pub fn peek_bytes(&self, offset: usize, len: usize) -> io::Result<&[u8]> {
        let position = self
            .position()
            .checked_add(offset)
            .ok_or(io::ErrorKind::UnexpectedEof)?;
        self.range(position, len)
    }

    /// Read exactly `len` bytes, advancing the cursor on success.
    /// Earlier references remain valid across subsequent shared reads.
    pub fn read_bytes(&self, len: usize) -> io::Result<&[u8]> {
        let position = self.reserve(len)?;
        self.range(position, len)
    }

    /// Read exactly `len` bytes as UTF-8. Invalid UTF-8 or short input leaves
    /// the cursor unchanged. Previously returned strings remain valid.
    pub fn read_str(&self, len: usize) -> io::Result<&str> {
        loop {
            let position = self.position();
            let bytes = self.range(position, len)?;
            let value = std::str::from_utf8(bytes)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            if self
                .cursor
                .compare_exchange(
                    position,
                    position + len,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                )
                .is_ok()
            {
                return Ok(value);
            }
        }
    }

    fn reserve(&self, len: usize) -> io::Result<usize> {
        self.cursor
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |position| {
                position.checked_add(len).filter(|end| *end <= self.end)
            })
            .map_err(|_| io::ErrorKind::UnexpectedEof.into())
    }

    /// Consume exactly `len` bytes without copying; short input does not advance.
    pub fn skip(&self, len: usize) -> io::Result<()> {
        self.reserve(len).map(|_| ())
    }

    /// Encode derived bytes directly into stable arena storage without moving
    /// the cursor. Useful for protocol decoders, including string unescaping.
    /// The closure must not exceed `capacity`; all successful allocations remain
    /// live across shared reads. Failed encodings release their allocation.
    pub fn store_bytes_with<E>(
        &self,
        capacity: usize,
        encode: impl FnOnce(&mut EphemeralBytesMut) -> Result<(), E>,
    ) -> Result<&[u8], E> {
        let mut bytes = self.arena.alloc(capacity);
        encode(&mut bytes)?;
        Ok(self.retained.push_get(Box::new(bytes.freeze())).as_slice())
    }

    pub(crate) fn chunk(&self) -> &[u8] {
        let position = self.position();
        if position == self.end {
            return &[];
        }
        let segment = &self.segments[self.locate(position)];
        &segment.bytes[position - segment.start..]
    }

    pub(crate) fn advance(&mut self, count: usize) {
        let end = self.end;
        let cursor = self.cursor.get_mut();
        *cursor += count.min(end - *cursor);
        while self
            .segments
            .front()
            .is_some_and(|segment| segment.start + segment.bytes.len() <= *cursor)
        {
            self.segments.pop_front();
        }
        // Exclusive access proves no borrowed result is still in use.
        self.retained.as_mut().clear();
        self.contiguous.take();
    }
}

/// An immutable borrowed range of a reader, independent of its cursor.
/// Holding a view prevents exclusive reads from reclaiming its segments.
#[derive(Clone, Copy, Debug)]
pub struct ReaderView<'a> {
    reader: &'a Reader,
    start: usize,
}

impl<'a> ReaderView<'a> {
    pub fn len(&self) -> usize {
        self.reader.end - self.start
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn peek_byte(&self, offset: usize) -> Option<u8> {
        self.reader.byte_at(self.start.checked_add(offset)?)
    }

    /// Borrow bytes relative to the view's start, merging only this range when
    /// it crosses segments. Returned bytes live as long as the source reader.
    pub fn peek_bytes(&self, offset: usize, len: usize) -> io::Result<&'a [u8]> {
        let position = self
            .start
            .checked_add(offset)
            .ok_or(io::ErrorKind::UnexpectedEof)?;
        self.reader.range(position, len)
    }

    /// Store decoded bytes in the source reader's arena.
    pub fn store_bytes_with<E>(
        &self,
        capacity: usize,
        encode: impl FnOnce(&mut EphemeralBytesMut) -> Result<(), E>,
    ) -> Result<&'a [u8], E> {
        self.reader.store_bytes_with(capacity, encode)
    }
}

impl Read for Reader {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        self.advance(0);
        let count = output.len().min(self.len());
        let mut written = 0;
        while written < count {
            let chunk = self.chunk();
            let len = chunk.len().min(count - written);
            output[written..written + len].copy_from_slice(&chunk[..len]);
            self.advance(len);
            written += len;
        }
        Ok(count)
    }

    fn read_vectored(&mut self, buffers: &mut [IoSliceMut<'_>]) -> io::Result<usize> {
        let mut total = 0;
        for buffer in buffers {
            let count = self.read(buffer)?;
            total += count;
            if count < buffer.len() {
                break;
            }
        }
        Ok(total)
    }
}

impl BufRead for Reader {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        self.advance(0);
        Ok(self.chunk())
    }
    fn consume(&mut self, count: usize) {
        self.advance(count);
    }
}
