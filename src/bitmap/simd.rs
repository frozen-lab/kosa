use crate::bitmap::{FULL_WORD, Slot};

#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::*;

#[cfg(target_arch = "aarch64")]
use std::arch::aarch64::*;

#[derive(Debug)]
pub(in crate::bitmap) struct SIMD {
    isa: ISA,
}

impl SIMD {
    pub(in crate::bitmap) fn new() -> Self {
        #[cfg(target_arch = "x86_64")]
        {
            if std::is_x86_feature_detected!("avx2") {
                return Self { isa: ISA::AVX2 };
            }

            return Self { isa: ISA::SSE2 };
        }

        #[cfg(target_arch = "aarch64")]
        return Self { isa: ISA::NEON };
    }

    #[inline(always)]
    pub(in crate::bitmap) unsafe fn is_slot_full(&self, slot: &Slot) -> bool {
        match self.isa {
            #[cfg(target_arch = "x86_64")]
            ISA::SSE2 => unsafe { Self::is_slot_full_sse2(slot) },

            #[cfg(target_arch = "x86_64")]
            ISA::AVX2 => unsafe { Self::is_slot_full_avx2(slot) },

            #[cfg(target_arch = "aarch64")]
            ISA::NEON => unsafe { Self::is_slot_full_neon(slot) },
        }
    }

    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "sse2")]
    unsafe fn is_slot_full_sse2(slot: &Slot) -> bool {
        let full = _mm_set1_epi64x(FULL_WORD as i64);
        let ptr = slot.as_ptr() as *const __m128i;

        let lo = _mm_loadu_si128(ptr);
        let hi = _mm_loadu_si128(ptr.add(1));

        let cmp_lo = _mm_cmpeq_epi32(lo, full);
        let cmp_hi = _mm_cmpeq_epi32(hi, full);

        let lo_full = _mm_movemask_epi8(cmp_lo) == 0xFFFF;
        let hi_full = _mm_movemask_epi8(cmp_hi) == 0xFFFF;

        lo_full && hi_full
    }

    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx2")]
    unsafe fn is_slot_full_avx2(slot: &Slot) -> bool {
        let full = _mm256_set1_epi64x(FULL_WORD as i64);
        let ymm = _mm256_loadu_si256(slot.as_ptr() as *const __m256i);
        let cmp = _mm256_cmpeq_epi64(ymm, full);

        _mm256_movemask_epi8(cmp) == -1
    }

    #[cfg(target_arch = "aarch64")]
    #[target_feature(enable = "neon")]
    unsafe fn is_slot_full_neon(slot: &Slot) -> bool {
        let ptr = slot.as_ptr();
        let lo = vld1q_u64(ptr);
        let hi = vld1q_u64(ptr.add(2));

        let anded = vandq_u64(lo, hi);
        vgetq_lane_u64(anded, 0) == FULL_WORD && vgetq_lane_u64(anded, 1) == FULL_WORD
    }
}

#[derive(Debug)]
enum ISA {
    #[cfg(target_arch = "x86_64")]
    SSE2,

    #[cfg(target_arch = "x86_64")]
    AVX2,

    #[cfg(target_arch = "aarch64")]
    NEON,
}
