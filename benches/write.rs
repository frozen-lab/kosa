//! Benchmarks for `write` latency
//! Run using: `taskset -c 2,3,4,5 cargo bench --bench write`

use frozen_core::utils::BufferSize;
use hdrhistogram::Histogram;
use kosa::{Kosa, KosaCfg};
use std::{sync, thread, time};
use tempfile::tempdir;

const THREADS: usize = 4;
const OPS: usize = 0x100_000;

const WARMUP_OPS: usize = OPS >> 0x0A;
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
    let dir = tempdir().expect("failed to create temp dir");
    let cfg = KosaCfg {
        path: dir.path().to_path_buf(),
        buffer_size: BufferSize::S32,
        initial_available_buffers: INITIAL_AVAILABLE_BUFFERS,
        flush_duration: time::Duration::from_millis(2),
        max_memory: 0x400 * 0x400 * 0x40, // 64 MB
    };

    (dir, cfg)
}

#[inline(always)]
fn record_bench(engine: &Kosa, ops: usize) -> BenchResult {
    let mut hist = Histogram::<u64>::new(3).expect("new histogram");
    let payload = vec![0xAB; PAYLOAD_SIZE];

    let mut last_ticket = None;
    for i in 1..=ops {
        let start = time::Instant::now();
        let (ticket, _slot, _) = engine.write(&payload).expect("write failed");

        if i % BATCH_SIZE == 0 {
            ticket.wait().expect("wait failed");
        }

        last_ticket = Some(ticket);
        hist.record(start.elapsed().as_nanos() as u64).expect("record latency");
    }

    if let Some(ticket) = last_ticket {
        let _ = ticket.wait();
    }

    BenchResult { hist }
}

fn single_tx_write_latency() -> BenchResult {
    let (_dir, cfg) = prep_init();
    let engine = Kosa::new(cfg).expect("new Kosa");

    // warmup
    let warmup_payload = vec![0x00; PAYLOAD_SIZE];
    for _ in 0..WARMUP_OPS {
        let (_ticket, _, _) = engine.write(&warmup_payload).expect("warmup write");
    }

    record_bench(&engine, OPS)
}

fn multi_tx_write_latency() -> BenchResult {
    let (_dir, cfg) = prep_init();
    let engine = sync::Arc::new(Kosa::new(cfg).expect("new Kosa"));
    let barrier = sync::Arc::new(sync::Barrier::new(THREADS));

    let mut handles = Vec::with_capacity(THREADS);
    for _tid in 0..THREADS {
        let eng = sync::Arc::clone(&engine);
        let barrier = sync::Arc::clone(&barrier);

        handles.push(thread::spawn(move || {
            let warmup_payload = vec![0x00; PAYLOAD_SIZE];

            // warmup
            for _ in 0..WARMUP_OPS {
                let _ = eng.write(&warmup_payload).expect("warmup write");
            }

            barrier.wait();

            let result = record_bench(&eng, OPS_PER_THREAD);
            barrier.wait();

            result
        }));
    }

    let mut hist = Histogram::<u64>::new(3).expect("new histogram");
    for handle in handles {
        let result = handle.join().expect("worker should join");
        hist.add(&result.hist).expect("merge histogram");
    }

    BenchResult { hist }
}

fn print_results(single: &BenchResult, multi: &BenchResult) {
    print!("Total measured operations: {OPS} (Batched Sync every {BATCH_SIZE} ops)");
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
    let single = single_tx_write_latency();
    let multi = multi_tx_write_latency();

    print_results(&single, &multi);
}
