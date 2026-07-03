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
        // sanity checks
        debug_assert_ne!(n, 0, "`n` must be greater than 0");
        debug_assert!(n <= SLOTS_PER_ROW, "`n` must be <= {}", SLOTS_PER_ROW);

        let mut slot = None;
        let page_idx = self.reservoir.acquire();

        unsafe {
            self.mmap.write(*page_idx, |raw_page| {
                let page = &mut *raw_page;
                if hints::unlikely(page.meta.full_rows_counter == USABLE_ROWS_PER_PAGE as u64) {
                    return;
                }

                let start_word = page.meta.current_word_ptr as usize;
                for row_idx in 0..USABLE_ROWS_PER_PAGE {
                    let row = &mut page.rows[row_idx];
                    if hints::unlikely(self.simd.is_row_full(row)) {
                        continue;
                    }

                    if let Some(bit) = find_free_run(row, start_word, n) {
                        set_run(row, bit, n);

                        if self.simd.is_row_full(row) {
                            page.meta.full_rows_counter += 1;
                        }

                        page.meta.current_word_ptr =
                            (((bit / SLOTS_PER_WORD) + 1) & (WORDS_PER_ROW - 1)) as u64;

                        slot = Some(row_idx * SLOTS_PER_ROW + bit);
                        return;
                    }
                }
            })
        }?;

        Ok(slot.map(|local| (*page_idx * SLOTS_PER_PAGE) + local))
    }

    #[inline(always)]
    pub(crate) fn free(&self, index: usize, n: usize) -> error::FrozenResult<()> {
        let page_idx = index / SLOTS_PER_PAGE;

        // sanity checks
        debug_assert_ne!(n, 0, "`n` must be greater than 0");
        debug_assert!(n <= SLOTS_PER_ROW, "`n` must be <= {}", SLOTS_PER_ROW);
        debug_assert!(page_idx < self.mmap.total_slots(), "`index` is out of bounds");

        let slot = index % SLOTS_PER_PAGE;
        let row_idx = slot / SLOTS_PER_ROW;
        let bit = slot % SLOTS_PER_ROW;

        unsafe {
            self.mmap.write(page_idx, |raw_page| {
                let page = &mut *raw_page;

                let row = &mut page.rows[row_idx];
                let was_row_full = self.simd.is_row_full(row);

                clear_run(row, bit, n);

                if was_row_full {
                    page.meta.full_rows_counter -= 1;
                }

                page.meta.current_word_ptr =
                    (((bit / SLOTS_PER_WORD) + 1) & (WORDS_PER_ROW - 1)) as u64;
            })
        }?;

        Ok(())
    }
}

#[inline(always)]
fn find_free_run(row: &Row, start_word: usize, n: usize) -> Option<usize> {
    // sanity checks
    debug_assert!(start_word < WORDS_PER_ROW);
    debug_assert!((1..=SLOTS_PER_ROW).contains(&n));

    let start = start_word * SLOTS_PER_WORD;
    for bit in start..=(SLOTS_PER_ROW - n) {
        if is_run_free(row, bit, n) {
            return Some(bit);
        }
    }

    for bit in 0..start {
        if bit + n > SLOTS_PER_ROW {
            break;
        }

        if is_run_free(row, bit, n) {
            return Some(bit);
        }
    }

    None
}

#[inline(always)]
fn is_run_free(row: &Row, start: usize, mut len: usize) -> bool {
    let mut bit = start;
    while len != 0 {
        let word = bit / SLOTS_PER_WORD;
        let offset = bit % SLOTS_PER_WORD;

        let take = len.min(SLOTS_PER_WORD - offset);
        let mask = if take == 0x40 { u64::MAX } else { ((1u64 << take) - 1) << offset };

        if row[word] & mask != 0 {
            return false;
        }

        len -= take;
        bit += take;
    }

    true
}

#[inline(always)]
fn set_run(row: &mut Row, start: usize, mut len: usize) {
    let mut bit = start;
    while len != 0 {
        let word = bit / SLOTS_PER_WORD;
        let offset = bit % SLOTS_PER_WORD;

        let take = len.min(SLOTS_PER_WORD - offset);
        let mask = if take == 0x40 { u64::MAX } else { ((1u64 << take) - 1) << offset };

        row[word] |= mask;

        len -= take;
        bit += take;
    }
}

#[inline(always)]
fn clear_run(row: &mut Row, start: usize, mut len: usize) {
    let mut bit = start;
    while len != 0 {
        let word = bit / SLOTS_PER_WORD;
        let offset = bit % SLOTS_PER_WORD;

        let take = len.min(SLOTS_PER_WORD - offset);
        let mask = if take == 0x40 { u64::MAX } else { ((1u64 << take) - 1) << offset };

        row[word] &= !mask;

        len -= take;
        bit += take;
    }
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
