//! Benchmarks for `delete` latency
//! Run using: `taskset -c 2,3,4,5 cargo bench --bench delete`

use frozen_core::utils::BufferSize;
use hdrhistogram::Histogram;
use kosa::{Kosa, KosaCfg};
use std::{sync, thread, time};
use tempfile::tempdir;

const THREADS: usize = 4;
const OPS: usize = 0x40_000;
const OPS_PER_THREAD: usize = OPS / THREADS;

const PAYLOAD_SIZE: usize = 0x20;
const BATCH_SIZE: usize = 0x8000;
const INITIAL_AVAILABLE_BUFFERS: usize = 0x400_000;

#[derive(Debug)]
struct BenchResult {
    hist: Histogram<u64>,
}

#[inline]
fn prep_init() -> (tempfile::TempDir, KosaCfg) {
    let dir = tempdir().unwrap();
    let cfg = KosaCfg {
        path: dir.path().to_path_buf(),
        buffer_size: BufferSize::S32,
        initial_available_buffers: INITIAL_AVAILABLE_BUFFERS,
        flush_duration: time::Duration::from_millis(2),
        max_memory: 0x400 * 0x400 * 0x40, // 64 MB
    };

    (dir, cfg)
}

fn populate_engine(engine: &Kosa, ops: usize) -> Vec<u64> {
    let mut slots = Vec::with_capacity(ops);
    let mut last_ticket = None;

    let payload = vec![0xAB; PAYLOAD_SIZE];

    for i in 1..=ops {
        let (ticket, slot, _) = engine.write(&payload).unwrap();
        slots.push(slot);

        if i % BATCH_SIZE == 0 {
            ticket.wait().unwrap();
        }

        last_ticket = Some(ticket);
    }

    if let Some(ticket) = last_ticket {
        let _ = ticket.wait();
    }

    slots
}

#[inline(always)]
fn record_bench(engine: &Kosa, slots: &[u64], required: usize) -> BenchResult {
    let mut hist = Histogram::<u64>::new(3).unwrap();

    for &slot in slots {
        let start = time::Instant::now();

        engine.delete(slot, required).unwrap();

        hist.record(start.elapsed().as_nanos() as u64).unwrap();
    }

    BenchResult { hist }
}

fn single_tx_delete_latency() -> BenchResult {
    let (_dir, cfg) = prep_init();
    let engine = Kosa::new(cfg).unwrap();

    println!("-> Populating single-thread data ({} ops)...", OPS);
    let slots = populate_engine(&engine, OPS);

    let payload_capacity = PAYLOAD_SIZE - 8;
    let required = PAYLOAD_SIZE.div_ceil(payload_capacity).max(1);

    println!("-> Running single-thread delete benchmark...");
    record_bench(&engine, &slots, required)
}

fn multi_tx_delete_latency() -> BenchResult {
    let (_dir, cfg) = prep_init();
    let engine = sync::Arc::new(Kosa::new(cfg).unwrap());

    println!("-> Populating multi-thread data ({} ops)...", OPS);
    let slots = populate_engine(&engine, OPS);

    let slots_shared = sync::Arc::new(slots);
    let barrier = sync::Arc::new(sync::Barrier::new(THREADS));

    let payload_capacity = PAYLOAD_SIZE - 8;
    let required = PAYLOAD_SIZE.div_ceil(payload_capacity).max(1);

    println!("-> Running multi-thread delete benchmark...");
    let mut handles = Vec::with_capacity(THREADS);

    for tid in 0..THREADS {
        let eng = sync::Arc::clone(&engine);
        let barrier = sync::Arc::clone(&barrier);
        let slots_ref = sync::Arc::clone(&slots_shared);

        handles.push(thread::spawn(move || {
            let start_idx = tid * OPS_PER_THREAD;
            let end_idx = start_idx + OPS_PER_THREAD;
            let thread_slots = &slots_ref[start_idx..end_idx];

            barrier.wait();
            let result = record_bench(&eng, thread_slots, required);
            barrier.wait();

            result
        }));
    }

    let mut hist = Histogram::<u64>::new(3).unwrap();
    for handle in handles {
        let result = handle.join().unwrap();
        hist.add(&result.hist).unwrap();
    }

    BenchResult { hist }
}

fn print_results(single: &BenchResult, multi: &BenchResult) {
    println!();
    println!("| Metric  | Single TX (µs) | Multi TX (µs) |");
    println!("|:--------|:---------------|:--------------|");
    println!(
        "| P50     | {:>14.4} | {:>13.4} |",
        single.hist.value_at_quantile(0.50) as f64 / 1000.0,
        multi.hist.value_at_quantile(0.50) as f64 / 1000.0,
    );
    println!(
        "| P90     | {:>14.4} | {:>13.4} |",
        single.hist.value_at_quantile(0.90) as f64 / 1000.0,
        multi.hist.value_at_quantile(0.90) as f64 / 1000.0,
    );
    println!(
        "| P99     | {:>14.4} | {:>13.4} |",
        single.hist.value_at_quantile(0.99) as f64 / 1000.0,
        multi.hist.value_at_quantile(0.99) as f64 / 1000.0,
    );
    println!(
        "| MEAN    | {:>14.4} | {:>13.4} |",
        single.hist.mean() as f64 / 1000.0,
        multi.hist.mean() as f64 / 1000.0,
    );
    println!(
        "| MAX     | {:>14.4} | {:>13.4} |",
        single.hist.max() as f64 / 1000.0,
        multi.hist.max() as f64 / 1000.0,
    );
    println!();
}

fn main() {
    let single = single_tx_delete_latency();
    let multi = multi_tx_delete_latency();

    print_results(&single, &multi);
}
