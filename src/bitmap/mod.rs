mod simd;

use frozen_core::{error, fmmap};
use std::{mem, path, time};

pub(in crate::bitmap) type Word = u64;
pub(in crate::bitmap) type Slot = [Word; 4];

pub(in crate::bitmap) const PAGES_AT_INIT: usize = 8;
pub(in crate::bitmap) const SLOTS_PER_PAGE: usize = 8;
pub(in crate::bitmap) const FULL_WORD: Word = Word::MAX;

#[derive(Debug)]
pub(crate) struct BitMap {
    mmap: fmmap::FrozenMMap<Page>,
}

impl BitMap {
    pub(crate) fn new<P: AsRef<path::Path>>(path: P, flush_duration: time::Duration) -> error::FrozenResult<Self> {
        let cfg = fmmap::FrozenMMapCfg {
            flush_duration,
            immediate_durability: false,
            module_id: crate::MODULE_ID,
            initial_count: PAGES_AT_INIT,
        };
        let mmap = fmmap::FrozenMMap::<Page>::new(path, cfg)?;

        Ok(Self { mmap })
    }
}

#[repr(C)]
#[repr(align(8))]
#[derive(Debug)]
struct PageMeta {
    word_ptr: u64,
    full_words: u64,
    _padding: [u64; 2],
}

#[repr(C)]
#[repr(align(8))]
#[derive(Debug)]
pub(in crate::bitmap) struct Page {
    meta: PageMeta,
    words: [Slot; SLOTS_PER_PAGE - 1],
}

const _: () = assert!(mem::size_of::<PageMeta>() == mem::size_of::<Slot>());
const _: () = assert!(mem::size_of::<Page>() == mem::size_of::<Slot>() * SLOTS_PER_PAGE);
