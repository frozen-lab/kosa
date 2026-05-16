#![allow(unused)]

use crate::MODULE_ID;
use frozen_core::{error::FrozenRes, fmmap::FrozenMMap};

type WORD = u32;

/// A custom type used for [`FrozenMMap`]
///
/// Each type contains 8 * u32 slots, i.e. 256 bits. This is done to ensure that the [`SLOT`]
/// could easily fit into a single `avx2` register for best possible performance for lookup
type SLOT = [WORD; WORD_PER_SLOT];

/// Number of slots available per page.
///
/// Each page contains 16 * [`SLOT`], i.e. 512 bytes
const SLOT_PER_PAGE: u32 = 0x10;

const WORD_PER_SLOT: usize = 8;
const BITS_PER_PAGE: usize = BITS_PER_SLOT * SLOT_PER_PAGE as usize;
const BITS_PER_SLOT: usize = 8 * WORD_PER_SLOT * std::mem::size_of::<u32>();

/// Number of pages available when a new index is initialized from scratch
///
/// NOTE: First slot is reserved for [`Header`]
const INITIAL_SLOT_CAPACITY: usize = (SLOT_PER_PAGE as usize * 4) + 1;

struct BitMap {
    mmap: FrozenMMap<SLOT, MODULE_ID>,
}

impl BitMap {
    pub(crate) fn new<P: AsRef<std::path::Path>>(path: P, flush_duration: std::time::Duration) -> FrozenRes<Self> {
        let mmap = FrozenMMap::<SLOT, MODULE_ID>::new(
            path,
            frozen_core::fmmap::FMCfg {
                flush_duration,
                initial_count: INITIAL_SLOT_CAPACITY,
            },
        )?;

        Ok(Self { mmap })
    }

    fn lookup_2(&self) -> FrozenRes<Option<(usize, u32, u32)>> {
        const MASK: u32 = 0b11;

        let header = unsafe { Header(self.mmap.read(0, |hdr| *hdr)?) };

        let total_pages = header.total_pages();
        let start_page = header.current_page();
        let start_slot = header.current_slot();

        for rel_page in 0..total_pages {
            let page_idx = (start_page + rel_page) % total_pages;
            let slot_begin = if rel_page == 0 { start_slot } else { 0 };
            let page_off = 1 + (page_idx * SLOT_PER_PAGE);

            for slot_off in slot_begin..SLOT_PER_PAGE {
                let slot = unsafe { self.mmap.read((page_off + slot_off) as usize, |s| *s) }?;
                for word_idx in 0..WORD_PER_SLOT {
                    let word = slot[word_idx];
                    if word == WORD::MAX {
                        continue;
                    }

                    let inv = !word;
                    for bit_idx in 0..0x1F {
                        if ((inv >> bit_idx) & MASK) == MASK {
                            let abs = (page_idx as usize * BITS_PER_PAGE)
                                + (slot_off as usize * BITS_PER_SLOT)
                                + (word_idx * 32)
                                + bit_idx;

                            return Ok(Some((abs, page_idx, slot_off)));
                        }
                    }
                }
            }
        }
        Ok(None)
    }
}

#[repr(C)]
struct Header(SLOT);

impl Header {
    const CURRENT_PAGE_INDEX: usize = 1;
    const CURRENT_SLOT_INDEX: usize = 2;
    const TOTAL_PAGES_INDEX: usize = 0;
    const AVAILABLE_FREE_SLOTS_INDEX: usize = 3;

    #[inline]
    fn total_pages(&self) -> u32 {
        self.0[Self::TOTAL_PAGES_INDEX]
    }

    #[inline]
    fn available_free_slots(&self) -> u32 {
        self.0[Self::AVAILABLE_FREE_SLOTS_INDEX]
    }

    #[inline]
    fn current_slot(&self) -> u32 {
        self.0[Self::CURRENT_SLOT_INDEX]
    }

    #[inline]
    fn current_page(&self) -> u32 {
        self.0[Self::CURRENT_PAGE_INDEX]
    }
}
