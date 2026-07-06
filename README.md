[![Latest Version](https://img.shields.io/crates/v/kosa.svg)](https://crates.io/crates/kosa)
[![License](https://img.shields.io/github/license/frozen-lab/kosa?logo=open-source-initiative&logoColor=white)](https://github.com/frozen-lab/kosa/blob/master/LICENSE)
[![Tests](https://github.com/frozen-lab/kosa/actions/workflows/tests.yaml/badge.svg)](https://github.com/frozen-lab/kosa/actions/workflows/tests.yaml)

# Kośa (कोश)

A reliable page-based storage engine with fire-and-forget durability semantics

## Usage

Add following to your `Cargo.toml`,

```toml
[dependencies]
kosa = { version = "0.0.1" }
```

> [!NOTE]
> Current version of `kosa` requires Rust 1.86 or later.

## Design

Kośa (कोश) is designed for ultra-low latency I/O operations by offloading durability to a
background asynchronous write pipeline (`WritePipe`).

## Benchmarks

Environment used for benching,

* OS: NixOS (WSL2)
* Architecture: x86_64
* Memory: 8 GiB RAM (DDR4)
* Rust: rustc 1.86.0 w/ cargo 1.86.0
* Kernel: Linux 6.6.87.2-microsoft-standard-WSL2
* CPU: Intel® Core™ i5-10300H @ 2.50GHz (4C / 8T)

**Write Latency:**

Observed measurements for 1,048,576 batched operations,

| Metric  | 1 Thread (µs) | 4 Threads (µs) |
|:--------|:--------------|:---------------|
| P50     |         0.200 |          0.642 |
| P90     |         0.500 |          1.559 |
| P99     |         1.000 |         11.095 |
| Mean    |         1.867 |          7.510 |
| Max     |     10051.583 |      30965.759 |

**Read Latency:**

Observed measurements for 262,144 operations,

| Metric  | 1 Thread (µs) | 4 Threads (µs) |
|:--------|:--------------|:---------------|
| P50     |         0.642 |          0.825 |
| P90     |         0.733 |          1.009 |
| P99     |         1.008 |          1.558 |
| Mean    |         0.653 |          0.834 |
| Max     |        29.711 |         78.399 |

**Delete Latency:**

Observed measurements for 262,144 operations,

| Metric  | 1 Thread (µs) | 4 Threads (µs) |
|:--------|:--------------|:---------------|
| P50     |         0.095 |          0.382 |
| P90     |         0.096 |          0.574 |
| P99     |         0.096 |          0.765 |
| Mean    |         0.094 |          0.666 |
| Max     |      1255.423 |       4698.111 |

## Example

```rs
use frozen_core::utils::BufferSize;
use kosa::{Kosa, KosaCfg};
use std::time::Duration;

let dir = tempfile::tempdir().unwrap();
let cfg = KosaCfg {
    path: dir.path().to_path_buf(),
    buffer_size: BufferSize::S64,
    initial_available_buffers: 0x1000,
    flush_duration: Duration::from_millis(2),
    max_memory: 0x400 * 0x400 * 0x40, // 64 MB
};

let engine = Kosa::new(cfg).unwrap();

let payload = b"hello world, fire and forget semantics!";
let (ticket, slot_index) = engine.write(payload).unwrap();

ticket.wait().unwrap();

let header_size = std::mem::size_of::<u32>() * 2;
let payload_capacity = 0x40 - header_size;
let required_blocks = payload.len().div_ceil(payload_capacity).max(1);

let read_result = engine.read(slot_index, required_blocks).unwrap();
let data = read_result.unwrap();

assert_eq!(payload.as_slice(), data.as_slice());
engine.delete(slot_index, required_blocks).unwrap();
```

## Etymology

Kośa (कोश), pronounced as _KOH-shuh_, is a Sanskrit word which means *repository*, a place where
valuable things are kept safe.

