mod simd;

use frozen_core::{error, fmmap, hints, reservoir};
use std::{mem, path, time};

pub(in crate::bitmap) type Word = u64;
pub(in crate::bitmap) type Row = [Word; 4];

pub(in crate::bitmap) const FULL_WORD: Word = Word::MAX;
pub(in crate::bitmap) const TOTALE_ROWS_PER_PAGE: usize = 8;
pub(in crate::bitmap) const USABLE_ROWS_PER_PAGE: usize = TOTALE_ROWS_PER_PAGE - 1;

pub(in crate::bitmap) const SLOTS_PER_ROW: usize = mem::size_of::<Row>() * mem::size_of::<u64>();
pub(in crate::bitmap) const SLOTS_PER_WORD: usize = mem::size_of::<Word>() * mem::size_of::<u64>();

pub(in crate::bitmap) const WORDS_PER_ROW: usize = mem::size_of::<Row>() / 8;
pub(in crate::bitmap) const SLOTS_PER_PAGE: usize = SLOTS_PER_ROW * (TOTALE_ROWS_PER_PAGE - 1);

#[derive(Debug)]
pub(crate) struct BitMap {
    simd: simd::SIMD,
    mmap: fmmap::FrozenMMap<Page>,
    reservoir: reservoir::Reservoir<usize>,
}

impl BitMap {
    pub(crate) fn new<P: AsRef<path::Path>>(
        path: P,
        init_pages: usize,
        flush_duration: time::Duration,
    ) -> error::FrozenResult<Self> {
        let cfg = fmmap::FrozenMMapCfg {
            flush_duration,
            initial_count: init_pages,
            immediate_durability: false,
            module_id: crate::MODULE_ID,
        };
        let mmap = fmmap::FrozenMMap::<Page>::new(path, cfg)?;
        let total_pages = mmap.total_slots();
        let reservoir = reservoir::Reservoir::new((0..total_pages).into_iter().collect());

        Ok(Self { mmap, reservoir, simd: simd::SIMD::new() })
    }

    #[inline(always)]
    pub(crate) fn allocate(&self, n: usize) -> error::FrozenResult<Option<usize>> {
        // sanity check
        debug_assert!(n <= SLOTS_PER_ROW);

        let mut slot: Option<usize> = None;
        let index = self.reservoir.acquire();

        unsafe {
            self.mmap.write(*index, |raw_page| {
                let page = &mut (*raw_page);
                if hints::unlikely(page.meta.full_rows_counter == USABLE_ROWS_PER_PAGE as u64) {
                    return;
                }

                let start_word = page.meta.current_word_ptr as usize;
                for row_idx in 0..USABLE_ROWS_PER_PAGE {
                    let row = &mut page.rows[row_idx];

                    if self.simd.is_row_full(row) {
                        continue;
                    }

                    for i in 0..WORDS_PER_ROW {
                        let word_idx = (start_word + i) & (WORDS_PER_ROW - 1);
                        let word = &mut row[word_idx];

                        if *word == FULL_WORD {
                            continue;
                        }

                        if let Some(bit) = lookup_run(*word, n) {
                            let mask = if n == 0x40 { u64::MAX } else { ((1u64 << n) - 1) << bit };
                            *word |= mask;

                            if self.simd.is_row_full(row) {
                                page.meta.full_rows_counter += 1;
                            }

                            page.meta.current_word_ptr =
                                ((word_idx + 1) & (WORDS_PER_ROW - 1)) as u64;
                            slot = Some(
                                row_idx * SLOTS_PER_ROW + word_idx * SLOTS_PER_WORD + bit as usize,
                            );

                            return;
                        }
                    }
                }
            })
        }?;

        if let Some(si) = slot {
            let start_bit_index = (*index * SLOTS_PER_PAGE) + si;
            return Ok(Some(start_bit_index));
        }

        Ok(None)
    }
}

#[inline(always)]
fn lookup_run(word: Word, n: usize) -> Option<u32> {
    let free = !word;
    for start in 0..=(0x40 - n) {
        let mask = if n == 0x40 { Word::MAX } else { ((1u64 << n) - 1) << start };
        if free & mask == mask {
            return Some(start as u32);
        }
    }

    None
}

#[repr(C)]
#[repr(align(8))]
#[derive(Debug)]
struct PageMeta {
    current_word_ptr: u64,
    full_rows_counter: u64,
    _padding: [u64; 2],
}

#[repr(C)]
#[repr(align(8))]
#[derive(Debug)]
pub(in crate::bitmap) struct Page {
    meta: PageMeta,
    rows: [Row; TOTALE_ROWS_PER_PAGE - 1],
}

const _: () = assert!(mem::size_of::<PageMeta>() == mem::size_of::<Row>());
const _: () = assert!(mem::size_of::<Page>() == mem::size_of::<Row>() * TOTALE_ROWS_PER_PAGE);
