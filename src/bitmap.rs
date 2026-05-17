use crate::MODULE_ID;
use frozen_core::{error::FrozenRes, fmmap::FrozenMMap, hints};
use std::sync::atomic;

type WORD = u32;

/// A custom type used for [`FrozenMMap`]
///
/// Each type contains 8 * u32 slots, i.e. 256 bits. This is done to ensure that the [`SLOT`]
/// could easily fit into a single `avx2` register for best possible performance for lookup
type SLOT = [WORD; WORD_PER_SLOT];

const WORD_PER_SLOT: usize = 8;
const BITS_PER_SLOT: usize = 8 * WORD_PER_SLOT * std::mem::size_of::<u32>();

struct BitMap {
    pool: Pool,
    simd: SIMD,
    mmap: FrozenMMap<SLOT, MODULE_ID>,
}

impl BitMap {
    /// Create a new instance of [`BitMap`]
    ///
    /// NOTE: `initial_cap` must be power of 2
    pub(crate) fn new<P: AsRef<std::path::Path>>(
        path: P,
        initial_cap: u32,
        flush_duration: std::time::Duration,
    ) -> FrozenRes<Option<Self>> {
        let mmap = FrozenMMap::<SLOT, MODULE_ID>::new(
            path,
            frozen_core::fmmap::FMCfg {
                flush_duration,
                initial_count: 1 + initial_cap as usize,
            },
        )?;

        let hdr_slot = unsafe { mmap.read(Header::HEADER_SLOT_INDEX, |hdr| *hdr) }?;
        let header = Header::from_slot(&hdr_slot);

        // new init (init header)
        if header.total_slots == 0 {
            let _ = unsafe {
                mmap.write_sync(Header::HEADER_SLOT_INDEX, |hdr| {
                    let header = Header::from_slot_mut(&mut *hdr);

                    header.total_slots = initial_cap as u64;
                    header.available_bits = (initial_cap * 8) as u64;
                    header.slot_pointer = 0;
                })
            }?;
        } else {
            // we need to grow
            if header.available_bits == 0 {
                return Ok(None);
            }
        }

        let pool = Pool::new(initial_cap);

        Ok(Some(Self {
            pool,
            mmap,
            simd: SIMD::new(),
        }))
    }

    #[inline(always)]
    pub unsafe fn allocate(&self, required: usize) -> FrozenRes<Option<(usize, u64)>> {
        let hdr_slot = self.mmap.read(Header::HEADER_SLOT_INDEX, |hdr| *hdr)?;
        let header = Header::from_slot(&hdr_slot);

        // fast fail
        if hints::unlikely(header.available_bits == 0) {
            return Ok(None);
        }

        match required {
            2 => self.allocate_2(header),
            _ => unimplemented!(),
        }
    }

    #[inline(always)]
    unsafe fn allocate_2(&self, header: &Header) -> FrozenRes<Option<(usize, u64)>> {
        let total = header.total_slots as usize;
        for _ in 0..total {
            let slot_idx = match self.pool.next() {
                Some(idx) => idx,
                None => return Ok(None),
            };

            // NOTE: we must +1 as 0th slot is reserved for the header
            let mmap_slot = slot_idx + 1;

            let slot = unsafe { self.mmap.read(mmap_slot, |s| *s) }?;
            let slot_base = slot_idx * BITS_PER_SLOT;

            if unsafe { self.simd.is_full(&slot) } {
                self.pool.retire(slot_idx);
                continue;
            }

            for word_idx in 0..WORD_PER_SLOT {
                let word = slot[word_idx];
                if word == WORD::MAX {
                    continue;
                }

                let inv = !word;
                let candidate = (inv & (inv >> 1)) & 0x5555_5555;
                if candidate == 0 {
                    continue;
                }

                let bit_idx = candidate.trailing_zeros() as usize;
                let abs = slot_base + (word_idx << 5) + bit_idx;
                let mask = 0b11 << bit_idx;

                let mut tx = self.mmap.new_tx();
                unsafe {
                    tx.write(Header::HEADER_SLOT_INDEX, |slot| {
                        let hdr = Header::from_slot_mut(&mut *slot);

                        hdr.available_bits -= 2;
                        hdr.slot_pointer = slot_idx as u64;
                    })?;
                    tx.write(mmap_slot, |slot| {
                        let slot = &mut *slot;

                        // validation under lock
                        if (slot[word_idx] & mask) == 0 {
                            slot[word_idx] |= mask;
                        }
                    })?;
                }
                let epoch = tx.commit()?;

                return Ok(Some((abs, epoch)));
            }
        }

        Ok(None)
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Header {
    total_slots: u64,
    slot_pointer: u64,
    available_bits: u64,
    _reserved_space: u64,
}

const _: () = assert!(std::mem::size_of::<Header>() == std::mem::size_of::<SLOT>());

impl Header {
    const HEADER_SLOT_INDEX: usize = 0;

    #[inline(always)]
    fn from_slot(slot: &SLOT) -> &Self {
        unsafe { &*(slot as *const SLOT as *const Self) }
    }

    #[inline(always)]
    fn from_slot_mut(slot: &mut SLOT) -> &mut Self {
        unsafe { &mut *(slot as *mut SLOT as *mut Self) }
    }
}

enum ISA {
    SSE2,
    AVX2,
    NEON,
}

struct SIMD(ISA);

impl SIMD {
    fn new() -> Self {
        #[cfg(target_arch = "x86_64")]
        {
            if std::is_x86_feature_detected!("avx2") {
                return Self(ISA::AVX2);
            }

            return Self(ISA::SSE2);
        }

        #[cfg(target_arch = "aarch64")]
        return Self(ISA::NEON);
    }

    unsafe fn is_full(&self, slot: &SLOT) -> bool {
        #[cfg(target_arch = "x86_64")]
        {
            return match self.0 {
                ISA::SSE2 => self._is_full_sse2(slot),
                ISA::AVX2 => self._is_full_avx2(slot),
                _ => unreachable!(),
            };
        }

        #[cfg(target_arch = "aarch64")]
        return self._is_full_neon(slot);
    }

    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "sse2")]
    unsafe fn _is_full_sse2(&self, slot: &SLOT) -> bool {
        use std::arch::x86_64::*;

        let full = _mm_set1_epi32(-1);
        let ptr = slot.as_ptr() as *const __m128i;

        let lo = unsafe { _mm_loadu_si128(ptr) };
        let hi = unsafe { _mm_loadu_si128(ptr.add(1)) };

        let cmp_lo = _mm_cmpeq_epi32(lo, full);
        let cmp_hi = _mm_cmpeq_epi32(hi, full);

        let lo_full = _mm_movemask_epi8(cmp_lo) == 0xFFFF;
        let hi_full = _mm_movemask_epi8(cmp_hi) == 0xFFFF;

        lo_full && hi_full
    }

    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx2")]
    unsafe fn _is_full_avx2(&self, slot: &SLOT) -> bool {
        use std::arch::x86_64::*;

        let full = _mm256_set1_epi32(-1);
        let ymm = unsafe { _mm256_loadu_si256(slot.as_ptr() as *const __m256i) };

        let cmp = _mm256_cmpeq_epi32(ymm, full);
        _mm256_movemask_epi8(cmp) == -1
    }

    #[cfg(target_arch = "aarch64")]
    #[target_feature(enable = "neon")]
    unsafe fn _is_full_neon(&self, slot: &SLOT) -> bool {
        use std::arch::aarch64::*;

        let ptr = slot.as_ptr();
        let full = vdupq_n_u32(WORD::MAX);

        let lo = unsafe { vld1q_u32(ptr) };
        let hi = unsafe { vld1q_u32(ptr.add(4)) };

        let cmp_lo = vceqq_u32(lo, full);
        let cmp_hi = vceqq_u32(hi, full);

        let lo_full = unsafe { vminvq_u32(cmp_lo) } == WORD::MAX;
        let hi_full = unsafe { vminvq_u32(cmp_hi) } == WORD::MAX;

        lo_full && hi_full
    }
}

struct Pool {
    total: u32,
    cursor: atomic::AtomicU32,
    slots: Box<[atomic::AtomicU32]>,
}

impl Pool {
    const FREE: u32 = 0;
    const CLAIMED: u32 = 1;
    const FULL: u32 = 2;

    /// NOTE: `total` must be power of 2
    fn new(total: u32) -> Self {
        let mut slots = Vec::with_capacity(total as usize);

        for _ in 0..total {
            slots.push(atomic::AtomicU32::new(Self::FREE));
        }

        Self {
            total,
            slots: slots.into_boxed_slice(),
            cursor: atomic::AtomicU32::new(0),
        }
    }

    #[inline(always)]
    fn next(&self) -> Option<usize> {
        let mask = self.total - 1;

        for _ in 0..self.total {
            let idx = (self.cursor.fetch_add(1, atomic::Ordering::Relaxed) & mask) as usize;

            let slot = &self.slots[idx];

            if slot
                .compare_exchange_weak(
                    Self::FREE,
                    Self::CLAIMED,
                    atomic::Ordering::Acquire,
                    atomic::Ordering::Relaxed,
                )
                .is_ok()
            {
                return Some(idx);
            }
        }

        None
    }

    #[inline(always)]
    fn release(&self, idx: usize) {
        self.slots[idx].store(Self::FREE, atomic::Ordering::Release);
    }

    #[inline(always)]
    fn retire(&self, idx: usize) {
        self.slots[idx].store(Self::FULL, atomic::Ordering::Release);
    }
}
