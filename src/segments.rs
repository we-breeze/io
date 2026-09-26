//! Two inline segment descriptors. Only the uncommon third segment allocates
//! descriptor storage. Payloads always use EphemeralBytesArena's own policy.

use std::collections::VecDeque;
use std::io;
use std::ops::Index;

use brz_ds::{EphemeralBytesArena, EphemeralBytesMut};

#[derive(Debug)]
pub(crate) struct Segment {
    pub(crate) start: usize,
    pub(crate) bytes: EphemeralBytesMut,
}

#[derive(Debug)]
pub(crate) struct Segments {
    inline: [Option<Segment>; 2],
    overflow: VecDeque<Segment>,
    len: usize,
}

impl Segments {
    pub(crate) fn new() -> Self {
        Self {
            inline: [None, None],
            overflow: VecDeque::new(),
            len: 0,
        }
    }
    pub(crate) fn len(&self) -> usize {
        self.len
    }
    pub(crate) fn iter(&self) -> impl DoubleEndedIterator<Item = &Segment> {
        self.inline.iter().flatten().chain(self.overflow.iter())
    }
    pub(crate) fn get(&self, index: usize) -> Option<&Segment> {
        if index < 2 {
            self.inline[index].as_ref()
        } else {
            self.overflow.get(index - 2)
        }
    }
    pub(crate) fn front(&self) -> Option<&Segment> {
        self.inline[0].as_ref()
    }
    pub(crate) fn back(&self) -> Option<&Segment> {
        self.len.checked_sub(1).and_then(|i| self.get(i))
    }
    pub(crate) fn back_mut(&mut self) -> Option<&mut Segment> {
        match self.len {
            0 => None,
            1..=2 => self.inline[self.len - 1].as_mut(),
            _ => self.overflow.back_mut(),
        }
    }
    pub(crate) fn push_back(&mut self, segment: Segment) {
        if self.len < 2 {
            self.inline[self.len] = Some(segment);
        } else {
            self.overflow.push_back(segment);
        }
        self.len += 1;
    }
    pub(crate) fn pop_front(&mut self) -> Option<Segment> {
        let first = self.inline[0].take()?;
        self.inline[0] = self.inline[1].take();
        self.inline[1] = self.overflow.pop_front();
        self.len -= 1;
        Some(first)
    }
    pub(crate) fn pop_back(&mut self) -> Option<Segment> {
        let value = match self.len {
            0 => return None,
            1..=2 => self.inline[self.len - 1].take(),
            _ => self.overflow.pop_back(),
        };
        self.len -= 1;
        value
    }
    pub(crate) fn clear(&mut self) {
        self.inline = [None, None];
        self.overflow.clear();
        self.len = 0;
    }
    pub(crate) fn partition_point(&self, predicate: impl Fn(&Segment) -> bool) -> usize {
        let (mut lo, mut hi) = (0, self.len);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if predicate(&self[mid]) {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }
    /// Reserve a contiguous tail WITHOUT relocating previously initialized bytes.
    /// An insufficient partially-filled tail is abandoned, not copied or extended.
    pub(crate) fn reserve_exact(
        &mut self,
        arena: &EphemeralBytesArena,
        end: usize,
        additional: usize,
    ) -> io::Result<()> {
        if additional > isize::MAX as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "reservation exceeds maximum allocation size",
            ));
        }
        end.checked_add(additional)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "byte length overflow"))?;
        if additional == 0
            || self
                .back()
                .is_some_and(|s| s.bytes.remaining() >= additional)
        {
            return Ok(());
        }
        if self.back().is_some_and(|s| s.bytes.is_empty()) {
            self.pop_back();
        }
        self.push_back(Segment {
            start: end,
            bytes: arena.alloc(additional),
        });
        Ok(())
    }
    pub(crate) fn ensure_tail(
        &mut self,
        arena: &EphemeralBytesArena,
        end: usize,
        maximum: usize,
        next_size: &mut usize,
    ) -> io::Result<()> {
        if maximum == 0 || self.back().is_some_and(|s| s.bytes.remaining() > 0) {
            return Ok(());
        }
        let size = (*next_size).min(maximum);
        self.reserve_exact(arena, end, size)?;
        *next_size = (*next_size)
            .saturating_mul(2)
            .min((64 * 1024).min(arena.chunk_capacity()));
        Ok(())
    }
}
impl Index<usize> for Segments {
    type Output = Segment;
    fn index(&self, index: usize) -> &Segment {
        self.get(index).expect("segment index")
    }
}
