mod simd;

use frozen_core::fmmap;

pub(in crate::bitmap) type Word = u64;
pub(in crate::bitmap) type Slot = [Word; 4];
pub(in crate::bitmap) type MMap = fmmap::FrozenMMap<Slot>;

pub(in crate::bitmap) const FULL_WORD: Word = Word::MAX;
