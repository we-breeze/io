//! The first segment descriptor is always inline. A predictable second segment
//! shares storage with the uncommon heap deque, which is allocated only when a
//! third segment appears. Payloads always use EphemeralBytesArena's own policy.

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
enum Overflow {
    Empty,
    Inline(Segment),
    // Keep this variant, even when the deque becomes empty, so a connection
    // that once needed three segments can reuse its descriptor allocation.
    Heap(VecDeque<Segment>),
}

#[derive(Debug)]
pub(crate) struct Segments {
    first: Option<Segment>,
    overflow: Overflow,
    initial_segment_size: usize,
    next_segment_size: usize,
}

impl Segments {
    pub(crate) fn new(initial_segment_size: usize) -> Self {
        Self {
            first: None,
            overflow: Overflow::Empty,
            initial_segment_size,
            next_segment_size: initial_segment_size,
        }
    }

    pub(crate) fn len(&self) -> usize {
        usize::from(self.first.is_some())
            + match &self.overflow {
                Overflow::Empty => 0,
                Overflow::Inline(_) => 1,
                Overflow::Heap(segments) => segments.len(),
            }
    }

    pub(crate) fn iter(&self) -> impl DoubleEndedIterator<Item = &Segment> {
        let inline = match &self.overflow {
            Overflow::Inline(segment) => Some(segment),
            Overflow::Empty | Overflow::Heap(_) => None,
        };
        let heap = match &self.overflow {
            Overflow::Heap(segments) => Some(segments),
            Overflow::Empty | Overflow::Inline(_) => None,
        };
        self.first
            .iter()
            .chain(inline)
            .chain(heap.into_iter().flatten())
    }

    pub(crate) fn get(&self, index: usize) -> Option<&Segment> {
        match index {
            0 => self.first.as_ref(),
            1.. => match &self.overflow {
                Overflow::Empty => None,
                Overflow::Inline(segment) => (index == 1).then_some(segment),
                Overflow::Heap(segments) => segments.get(index - 1),
            },
        }
    }

    pub(crate) fn front(&self) -> Option<&Segment> {
        self.first.as_ref()
    }

    pub(crate) fn back(&self) -> Option<&Segment> {
        match &self.overflow {
            Overflow::Empty => self.first.as_ref(),
            Overflow::Inline(segment) => Some(segment),
            Overflow::Heap(segments) => segments.back().or(self.first.as_ref()),
        }
    }

    pub(crate) fn back_mut(&mut self) -> Option<&mut Segment> {
        match &mut self.overflow {
            Overflow::Empty => self.first.as_mut(),
            Overflow::Inline(segment) => Some(segment),
            Overflow::Heap(segments) => segments.back_mut().or(self.first.as_mut()),
        }
    }

    pub(crate) fn push_back(&mut self, segment: Segment) {
        if self.first.is_none() {
            debug_assert!(matches!(self.overflow, Overflow::Empty | Overflow::Heap(_)));
            self.first = Some(segment);
            return;
        }
        match &mut self.overflow {
            Overflow::Empty => self.overflow = Overflow::Inline(segment),
            Overflow::Inline(_) => {
                let Overflow::Inline(previous) =
                    std::mem::replace(&mut self.overflow, Overflow::Empty)
                else {
                    unreachable!();
                };
                let mut segments = VecDeque::new();
                segments.push_back(previous);
                segments.push_back(segment);
                self.overflow = Overflow::Heap(segments);
            }
            Overflow::Heap(segments) => segments.push_back(segment),
        }
    }

    pub(crate) fn pop_front(&mut self) -> Option<Segment> {
        let first = self.first.take()?;
        match &mut self.overflow {
            Overflow::Empty => {}
            Overflow::Inline(_) => {
                let Overflow::Inline(next) = std::mem::replace(&mut self.overflow, Overflow::Empty)
                else {
                    unreachable!();
                };
                self.first = Some(next);
            }
            Overflow::Heap(segments) => self.first = segments.pop_front(),
        }
        Some(first)
    }

    pub(crate) fn pop_back(&mut self) -> Option<Segment> {
        match &mut self.overflow {
            Overflow::Empty => self.first.take(),
            Overflow::Inline(_) => {
                let Overflow::Inline(segment) =
                    std::mem::replace(&mut self.overflow, Overflow::Empty)
                else {
                    unreachable!();
                };
                Some(segment)
            }
            Overflow::Heap(segments) => segments.pop_back().or_else(|| self.first.take()),
        }
    }

    pub(crate) fn clear(&mut self) {
        self.first = None;
        self.next_segment_size = self.initial_segment_size;
        match &mut self.overflow {
            Overflow::Empty => {}
            Overflow::Inline(_) => self.overflow = Overflow::Empty,
            Overflow::Heap(segments) => segments.clear(),
        }
    }

    pub(crate) fn partition_point(&self, predicate: impl Fn(&Segment) -> bool) -> usize {
        let (mut lo, mut hi) = (0, self.len());
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
    ) -> io::Result<()> {
        if maximum == 0 || self.back().is_some_and(|s| s.bytes.remaining() > 0) {
            return Ok(());
        }
        let size = self.next_segment_size.min(maximum);
        self.reserve_exact(arena, end, size)?;
        self.next_segment_size = self
            .next_segment_size
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

#[cfg(all(test, target_pointer_width = "64"))]
mod tests {
    use super::*;

    #[test]
    fn adaptive_overflow_keeps_the_descriptor_layout_compact() {
        assert_eq!(std::mem::size_of::<Segment>(), 56);
        assert_eq!(std::mem::size_of::<Overflow>(), 56);
        assert_eq!(std::mem::size_of::<Segments>(), 128);
        assert_eq!(std::mem::size_of::<crate::Reader>(), 192);
        assert_eq!(std::mem::size_of::<crate::Writer>(), 152);
    }
}
