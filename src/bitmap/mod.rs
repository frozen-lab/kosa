mod simd;

use frozen_core::{error, fmmap, hints};
use std::{mem, path, sync::atomic, time};

pub(in crate::bitmap) type Row = [Word; 4];
pub(in crate::bitmap) const FULL_WORD: Word = Word::MAX;

type Word = u64;

const TOTALE_ROWS_PER_PAGE: usize = 8;
const USABLE_ROWS_PER_PAGE: usize = TOTALE_ROWS_PER_PAGE - 1;

const SLOTS_PER_ROW: usize = mem::size_of::<Row>() * mem::size_of::<u64>();
const SLOTS_PER_WORD: usize = mem::size_of::<Word>() * mem::size_of::<u64>();

const WORDS_PER_ROW: usize = mem::size_of::<Row>() / 8;
const SLOTS_PER_PAGE: usize = SLOTS_PER_ROW * (TOTALE_ROWS_PER_PAGE - 1);

#[derive(Debug)]
pub(crate) struct BitMap {
    simd: simd::SIMD,
    mmap: fmmap::FrozenMMap<Page>,
    next_page: atomic::AtomicUsize,
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
        let init_page = Self::find_init_page_idx(&mmap);

        Ok(Self { mmap, simd: simd::SIMD::new(), next_page: atomic::AtomicUsize::new(init_page) })
    }

    fn find_init_page_idx(mmap: &fmmap::FrozenMMap<Page>) -> usize {
        let mut latest = 0;
        let total = mmap.total_slots();

        for i in 0..total {
            if let Some(idx) = unsafe {
                mmap.read(i, |page| {
                    if (*page).meta.full_rows_counter < USABLE_ROWS_PER_PAGE as u64 {
                        return Some(i);
                    }

                    None
                })
            } {
                latest = i;
                break;
            }
        }

        latest
    }

    #[inline(always)]
    pub(crate) fn allocate(&self, n: usize) -> error::FrozenResult<Option<usize>> {
        // sanity checks
        debug_assert_ne!(n, 0, "`n` must be greater than 0");
        debug_assert!(n <= SLOTS_PER_ROW, "`n` must be <= {}", SLOTS_PER_ROW);

        let total_pages = self.mmap.total_slots();
        let start_page = self.next_page.load(atomic::Ordering::Relaxed);

        for i in 0..total_pages {
            let page_idx = (start_page + i) % total_pages;
            let mut slot = None;

            unsafe {
                self.mmap.write(page_idx, |raw_page| {
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

            if let Some(local_slot) = slot {
                self.next_page.store(page_idx, atomic::Ordering::Relaxed);
                return Ok(Some((page_idx * SLOTS_PER_PAGE) + local_slot));
            }
        }

        Ok(None)
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

#[cfg(test)]
mod tests {
    use super::*;

    const INIT_PAGES: usize = 0x0A;
    const FLUSH_DURATION: time::Duration = time::Duration::from_secs(1);

    #[inline]
    fn init() -> (tempfile::TempDir, BitMap) {
        let dir = tempfile::tempdir().expect("new_dir");
        let path = dir.path().join("bmap");
        let map = BitMap::new(path, INIT_PAGES, FLUSH_DURATION).expect("new bmap");

        (dir, map)
    }

    #[cfg(test)]
    mod t_find_free_run {
        use super::*;

        #[test]
        fn ok_empty_row() {
            let row = [0; WORDS_PER_ROW];

            assert_eq!(find_free_run(&row, 0, 1), Some(0));
            assert_eq!(find_free_run(&row, 0, 0x40), Some(0));
            assert_eq!(find_free_run(&row, 0, SLOTS_PER_ROW), Some(0));
        }

        #[test]
        fn ok_full_row() {
            let row = [FULL_WORD; WORDS_PER_ROW];

            assert_eq!(find_free_run(&row, 0, 1), None);
            assert_eq!(find_free_run(&row, 0, 0x40), None);
            assert_eq!(find_free_run(&row, 0, SLOTS_PER_ROW), None);
        }

        #[test]
        fn ok_exactly_fills_row() {
            let row = [0; WORDS_PER_ROW];
            assert_eq!(find_free_run(&row, 0, SLOTS_PER_ROW), Some(0));
        }

        #[test]
        fn ok_starts_at_last_word() {
            let mut row = [FULL_WORD; WORDS_PER_ROW];
            row[3] = 0;

            assert_eq!(find_free_run(&row, 3, 0x20), Some(0xC0));
        }

        #[test]
        fn ok_honors_start_word() {
            let mut row = [FULL_WORD; WORDS_PER_ROW];

            row[0] = 0;
            row[2] = 0;

            assert_eq!(find_free_run(&row, 2, 8), Some(0x80));
        }

        #[test]
        fn ok_wraps_to_beginning() {
            let mut row = [FULL_WORD; WORDS_PER_ROW];

            row[0] = 0;
            row[3] = FULL_WORD;

            assert_eq!(find_free_run(&row, 3, 8), Some(0));
        }

        #[test]
        fn ok_single_bit() {
            let mut row = [FULL_WORD; WORDS_PER_ROW];
            row[1] = !(1 << 0x11);

            assert_eq!(find_free_run(&row, 0, 1), Some(0x51));
        }

        #[test]
        fn ok_entire_row() {
            assert_eq!(find_free_run(&[0; WORDS_PER_ROW], 0, 0x100), Some(0));
        }
    }

    mod t_allocate {
        use super::*;
        use std::{collections::HashSet, sync::Arc};

        #[test]
        fn ok_cross_word() {
            let (_dir, bitmap) = init();

            let bit = bitmap.allocate(0x60).unwrap().unwrap();
            assert_eq!(bit, 0);

            bitmap.free(bit, 0x60).unwrap();
            let bit2 = bitmap.allocate(0x60).unwrap().unwrap();

            assert_eq!(bit2, 0x40);
        }

        #[test]
        fn ok_reuse_after_free() {
            let (_dir, bitmap) = init();

            let first = bitmap.allocate(0x60).unwrap().unwrap();
            bitmap.free(first, 0x60).unwrap();

            let second = bitmap.allocate(0x60).unwrap().unwrap();
            bitmap.free(second, 0x60).unwrap();

            assert_ne!(first, usize::MAX);
            assert_ne!(second, usize::MAX);
        }

        #[test]
        fn ok_multiple_sizes() {
            let (_dir, bitmap) = init();

            for size in 1..=SLOTS_PER_ROW {
                let bit = bitmap.allocate(size).unwrap().unwrap();
                bitmap.free(bit, size).unwrap();
            }
        }

        #[test]
        fn ok_random_allocate_free() {
            let (_dir, bitmap) = init();

            let mut allocs = Vec::new();
            for i in 0..0x2000 {
                let size = (i % SLOTS_PER_ROW) + 1;
                if let Some(bit) = bitmap.allocate(size).unwrap() {
                    allocs.push((bit, size));
                }

                if allocs.len() > 0x20 {
                    let (bit, size) = allocs.remove(0);
                    bitmap.free(bit, size).unwrap();
                }
            }

            while let Some((bit, size)) = allocs.pop() {
                bitmap.free(bit, size).unwrap();
            }

            assert!(bitmap.allocate(SLOTS_PER_ROW).unwrap().is_some());
        }

        #[test]
        fn ok_parallel_contention() {
            let (_dir, bitmap) = init();
            let bitmap = Arc::new(bitmap);

            std::thread::scope(|scope| {
                for tid in 0..0x10 {
                    let bitmap = Arc::clone(&bitmap);

                    scope.spawn(move || {
                        let mut owned = Vec::new();

                        for i in 0..0x61A8 {
                            let size = ((i + tid) % SLOTS_PER_ROW) + 1;
                            if let Some(bit) = bitmap.allocate(size).unwrap() {
                                owned.push((bit, size));
                            }

                            if owned.len() >= 0x40 {
                                let idx = owned.len() / 2;
                                let (bit, size) = owned.swap_remove(idx);
                                bitmap.free(bit, size).unwrap();
                            }
                        }

                        while let Some((bit, size)) = owned.pop() {
                            bitmap.free(bit, size).unwrap();
                        }
                    });
                }
            });

            assert!(bitmap.allocate(SLOTS_PER_ROW).unwrap().is_some());
        }

        #[test]
        fn ok_fragmentation() {
            let (_dir, bitmap) = init();

            let mut allocs = Vec::new();
            for _ in 0..0x40 {
                allocs.push(bitmap.allocate(4).unwrap().unwrap());
            }

            for bit in allocs.iter().step_by(2) {
                bitmap.free(*bit, 4).unwrap();
            }

            for _ in 0..0x20 {
                assert!(bitmap.allocate(4).unwrap().is_some());
            }
        }

        #[test]
        fn ok_single_bit_until_full() {
            let (_dir, bitmap) = init();
            let mut seen = HashSet::new();
            let total_capacity = SLOTS_PER_PAGE * INIT_PAGES;

            for _ in 0..total_capacity {
                let bit = bitmap.allocate(1).unwrap().unwrap();
                assert!(seen.insert(bit));
            }

            assert_eq!(bitmap.allocate(1).unwrap(), None);
        }

        #[test]
        fn ok_entire_page() {
            let (_dir, bitmap) = init();
            let total_rows = USABLE_ROWS_PER_PAGE * INIT_PAGES;

            for _ in 0..total_rows {
                assert!(bitmap.allocate(SLOTS_PER_ROW).unwrap().is_some());
            }

            assert_eq!(bitmap.allocate(1).unwrap(), None);
        }

        #[test]
        fn ok_fill_all_rows() {
            let (_dir, bitmap) = init();
            let total_rows = USABLE_ROWS_PER_PAGE * INIT_PAGES;

            for _ in 0..total_rows {
                assert!(bitmap.allocate(SLOTS_PER_ROW).unwrap().is_some());
            }

            assert_eq!(bitmap.allocate(1).unwrap(), None);
        }

        #[test]
        fn ok_unique_indices() {
            let (_dir, bitmap) = init();
            let mut seen = HashSet::new();

            while let Some(bit) = bitmap.allocate(1).unwrap() {
                assert!(seen.insert(bit), "duplicate allocation: {bit}");
            }

            assert_eq!(seen.len(), SLOTS_PER_PAGE * INIT_PAGES);
        }

        #[test]
        fn ok_fill_and_empty_page() {
            let (_dir, bitmap) = init();
            let mut allocs = Vec::new();

            // Fill all pages
            while let Some(bit) = bitmap.allocate(1).unwrap() {
                allocs.push(bit);
            }

            assert_eq!(allocs.len(), SLOTS_PER_PAGE * INIT_PAGES);

            // Free exactly one page worth of slots to test capacity recovery
            for bit in allocs.into_iter().take(SLOTS_PER_PAGE) {
                bitmap.free(bit, 1).unwrap();
            }

            assert!(bitmap.allocate(SLOTS_PER_PAGE / USABLE_ROWS_PER_PAGE).unwrap().is_some());
        }
    }

    mod t_free {
        use super::*;

        #[test]
        fn ok_single_allocation() {
            let (_dir, bitmap) = init();

            let bit = bitmap.allocate(1).unwrap().unwrap();
            bitmap.free(bit, 1).unwrap();

            assert!(bitmap.allocate(SLOTS_PER_ROW).unwrap().is_some());
        }

        #[test]
        fn ok_first_allocation() {
            let (_dir, bitmap) = init();

            let first = bitmap.allocate(8).unwrap().unwrap();
            let second = bitmap.allocate(8).unwrap().unwrap();

            bitmap.free(first, 8).unwrap();
            bitmap.free(second, 8).unwrap();

            assert!(bitmap.allocate(SLOTS_PER_ROW).unwrap().is_some());
        }

        #[test]
        fn ok_last_allocation() {
            let (_dir, bitmap) = init();

            let mut last = None;
            while let Some(bit) = bitmap.allocate(1).unwrap() {
                last = Some(bit);
            }

            let last = last.expect("expected at least one allocation");
            bitmap.free(last, 1).unwrap();

            assert!(bitmap.allocate(1).unwrap().is_some());
        }

        #[test]
        fn ok_cross_word_allocation() {
            let (_dir, bitmap) = init();

            let bit = bitmap.allocate(0x60).unwrap().unwrap();
            bitmap.free(bit, 0x60).unwrap();

            assert!(bitmap.allocate(0x60).unwrap().is_some());
        }

        #[test]
        fn ok_entire_row() {
            let (_dir, bitmap) = init();

            let mut allocs = Vec::new();
            for _ in 0..USABLE_ROWS_PER_PAGE {
                allocs.push(bitmap.allocate(SLOTS_PER_ROW).unwrap().unwrap());
            }

            for bit in allocs {
                bitmap.free(bit, SLOTS_PER_ROW).unwrap();
            }

            for _ in 0..USABLE_ROWS_PER_PAGE {
                assert!(bitmap.allocate(SLOTS_PER_ROW).unwrap().is_some());
            }
        }

        #[test]
        fn ok_allocate_after_free() {
            let (_dir, bitmap) = init();

            let bit = bitmap.allocate(0x25).unwrap().unwrap();
            bitmap.free(bit, 0x25).unwrap();

            let bit2 = bitmap.allocate(0x25).unwrap().unwrap();
            bitmap.free(bit2, 0x25).unwrap();

            assert!(bitmap.allocate(SLOTS_PER_ROW).unwrap().is_some());
        }

        #[test]
        fn ok_free_random_order() {
            let (_dir, bitmap) = init();

            let mut allocs = Vec::new();
            let mut size = 1;

            while let Some(bit) = bitmap.allocate(size).unwrap() {
                allocs.push((bit, size));
                size += 1;

                if size > SLOTS_PER_ROW {
                    size = 1;
                }
            }

            assert!(!allocs.is_empty());

            while !allocs.is_empty() {
                let idx = allocs.len() / 2;
                let (bit, size) = allocs.swap_remove(idx);
                bitmap.free(bit, size).unwrap();
            }

            while bitmap.allocate(1).unwrap().is_some() {}

            assert_eq!(bitmap.allocate(1).unwrap(), None);
        }
    }

    mod stress {
        use super::*;

        const OPS: usize = 0x20_000;

        #[test]
        fn stress_random_operations() {
            let (_dir, bitmap) = init();

            let mut rng: u64 = 0xDEADC0DECAFEBABE;
            let mut allocs = Vec::<(usize, usize)>::new();

            #[inline(always)]
            fn rand(state: &mut u64) -> u64 {
                *state ^= *state << 0x0D;
                *state ^= *state >> 7;
                *state ^= *state << 0x11;
                *state
            }

            for _ in 0..OPS {
                if allocs.is_empty() || (rand(&mut rng) % 0x64) < 0x3C {
                    let size = (rand(&mut rng) as usize % SLOTS_PER_ROW) + 1;

                    if let Some(bit) = bitmap.allocate(size).unwrap() {
                        allocs.push((bit, size));
                    }
                } else {
                    let idx = rand(&mut rng) as usize % allocs.len();
                    let (bit, size) = allocs.swap_remove(idx);

                    bitmap.free(bit, size).unwrap();
                }
            }

            while let Some((bit, size)) = allocs.pop() {
                bitmap.free(bit, size).unwrap();
            }

            // NOTE: Should be completely reusable
            let mut count = 0;
            while bitmap.allocate(1).unwrap().is_some() {
                count += 1;
            }

            assert_eq!(count, INIT_PAGES * SLOTS_PER_PAGE);
        }
    }
}
