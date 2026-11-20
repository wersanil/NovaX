//! Bitmap-based physical frame allocator.
//!
//! One bit per 4 KiB frame; bit set = in use. The bitmap itself lives
//! in the first USABLE region large enough to hold it, accessed through
//! the HHDM.

use crate::{HhdmOffset, MemoryError, MemoryRegion, MemoryResult, RegionKind};

/// Size of one physical frame (4 KiB).
pub const FRAME_SIZE: u64 = 4096;

/// A physical 4 KiB frame, identified by its frame number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct PhysFrame {
    /// Frame number (`physical address / FRAME_SIZE`).
    pub number: u64,
}

impl PhysFrame {
    /// Frame containing the given physical address.
    #[inline]
    pub const fn containing(phys: u64) -> Self {
        Self {
            number: phys / FRAME_SIZE,
        }
    }
    /// Physical start address of this frame.
    #[inline]
    pub const fn start_address(self) -> u64 {
        self.number * FRAME_SIZE
    }
}

/// Bitmap frame allocator; one bit per 4 KiB frame.
pub struct FrameAllocator {
    bitmap: &'static mut [u64],
    total_frames: u64,
    next_free_hint: u64,
    hhdm: HhdmOffset,
    free_count: u64,
}

impl FrameAllocator {
    /// Construct the allocator from the bootloader-supplied memory map.
    ///
    /// # Safety
    ///
    /// - `hhdm` must be the real HHDM offset covering the whole physical
    ///   address space described by `regions`.
    /// - No other code may alias the physical memory that will back the
    ///   bitmap. We place the bitmap in the first USABLE region; callers
    ///   must not touch that physical range afterwards except through this
    ///   allocator.
    pub unsafe fn new(hhdm: HhdmOffset, regions: &[MemoryRegion]) -> MemoryResult<Self> {
        let max_phys = regions
            .iter()
            .filter(|r| r.kind == RegionKind::Usable)
            .map(|r| r.base.saturating_add(r.length))
            .max()
            .ok_or(MemoryError::OutOfFrames)?;
        let total_frames = max_phys.div_ceil(FRAME_SIZE);
        let bitmap_words = (total_frames as usize).div_ceil(64);
        let bitmap_bytes = bitmap_words * 8;

        let host = regions
            .iter()
            .find(|r| r.kind == RegionKind::Usable && r.length >= bitmap_bytes as u64)
            .ok_or(MemoryError::NoUsableRegionLargeEnough {
                needed_frames: (bitmap_bytes as u64).div_ceil(FRAME_SIZE) as usize,
            })?;

        // WHY unsafe: constructing a `&'static mut [u64]` from a raw physical
        // address. Safety is delegated to the caller per the doc comment.
        let ptr = (hhdm.as_u64() + host.base) as *mut u64;
        let bitmap: &'static mut [u64] =
            unsafe { core::slice::from_raw_parts_mut(ptr, bitmap_words) };

        // Mark everything used, then free USABLE regions.
        for w in bitmap.iter_mut() {
            *w = !0u64;
        }

        let mut free_count: u64 = 0;
        for r in regions.iter().filter(|r| r.kind == RegionKind::Usable) {
            let start = r.base / FRAME_SIZE;
            let end = (r.base + r.length) / FRAME_SIZE;
            for f in start..end {
                if test_bit(bitmap, f) {
                    clear_bit(bitmap, f);
                    free_count += 1;
                }
            }
        }

        // Reserve frames covering the bitmap itself.
        let bm_start = host.base / FRAME_SIZE;
        let bm_frames = (bitmap_bytes as u64).div_ceil(FRAME_SIZE);
        for f in bm_start..bm_start + bm_frames {
            if !test_bit(bitmap, f) {
                set_bit(bitmap, f);
                free_count -= 1;
            }
        }

        Ok(Self {
            bitmap,
            total_frames,
            next_free_hint: bm_start + bm_frames,
            hhdm,
            free_count,
        })
    }

    /// Allocate one frame, returning `None` if no free frame remains.
    pub fn alloc(&mut self) -> Option<PhysFrame> {
        let words = self.bitmap.len();
        let start_word = (self.next_free_hint / 64) as usize;

        for w in start_word..words {
            let word = self.bitmap[w];
            if word != !0u64 {
                let bit = (!word).trailing_zeros() as u64;
                let frame_num = (w as u64) * 64 + bit;
                if frame_num >= self.total_frames {
                    break;
                }
                self.bitmap[w] |= 1u64 << bit;
                self.next_free_hint = frame_num + 1;
                self.free_count -= 1;
                return Some(PhysFrame { number: frame_num });
            }
        }
        for w in 0..start_word {
            let word = self.bitmap[w];
            if word != !0u64 {
                let bit = (!word).trailing_zeros() as u64;
                let frame_num = (w as u64) * 64 + bit;
                if frame_num >= self.total_frames {
                    break;
                }
                self.bitmap[w] |= 1u64 << bit;
                self.next_free_hint = frame_num + 1;
                self.free_count -= 1;
                return Some(PhysFrame { number: frame_num });
            }
        }
        None
    }

    /// Return a frame to the allocator. Must not be called twice for the
    /// same frame (debug builds assert on double free).
    pub fn free(&mut self, frame: PhysFrame) {
        debug_assert!(frame.number < self.total_frames, "free: out of range");
        debug_assert!(
            test_bit(self.bitmap, frame.number),
            "double free of frame {}",
            frame.number
        );
        clear_bit(self.bitmap, frame.number);
        self.free_count += 1;
        if frame.number < self.next_free_hint {
            self.next_free_hint = frame.number;
        }
    }

    /// Reserve a single frame that was free. Used to protect kernel-owned
    /// physical memory from accidental allocation.
    pub fn reserve(&mut self, frame: PhysFrame) {
        if frame.number >= self.total_frames {
            return;
        }
        if !test_bit(self.bitmap, frame.number) {
            set_bit(self.bitmap, frame.number);
            self.free_count -= 1;
        }
    }

    /// Reserve all frames intersecting the half-open `[start, end)` range.
    pub fn reserve_range(&mut self, start: u64, end: u64) {
        let f0 = start / FRAME_SIZE;
        let f1 = end.div_ceil(FRAME_SIZE);
        for f in f0..f1 {
            if f < self.total_frames {
                self.reserve(PhysFrame { number: f });
            }
        }
    }

    /// Whether `frame` is currently free.
    #[inline]
    pub fn is_free(&self, frame: PhysFrame) -> bool {
        !test_bit(self.bitmap, frame.number)
    }
    /// Number of frames currently free.
    #[inline]
    pub fn free_count(&self) -> u64 {
        self.free_count
    }
    /// Total number of frames tracked by the bitmap.
    #[inline]
    pub fn total_frames(&self) -> u64 {
        self.total_frames
    }
    /// HHDM offset used to access physical memory.
    #[inline]
    pub fn hhdm(&self) -> HhdmOffset {
        self.hhdm
    }
}

#[inline]
fn test_bit(bitmap: &[u64], n: u64) -> bool {
    bitmap[(n / 64) as usize] & (1u64 << (n % 64)) != 0
}
#[inline]
fn set_bit(bitmap: &mut [u64], n: u64) {
    bitmap[(n / 64) as usize] |= 1u64 << (n % 64);
}
#[inline]
fn clear_bit(bitmap: &mut [u64], n: u64) {
    bitmap[(n / 64) as usize] &= !(1u64 << (n % 64));
}
