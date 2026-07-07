//! Kośa (कोश) is a reliable page-based storage engine with fire-and-forget durability semantics
//!
//! ## Design
//!
//! Kośa (कोश) is designed for ultra-low latency I/O operations by offloading durability to a
//! background asynchronous write pipeline (`WritePipe`).
//!
//! ## Benchmarks
//!
//! Environment used for benching,
//!
//! * OS: NixOS (WSL2)
//! * Architecture: x86_64
//! * Memory: 8 GiB RAM (DDR4)
//! * Rust: rustc 1.86.0 w/ cargo 1.86.0
//! * Kernel: Linux 6.6.87.2-microsoft-standard-WSL2
//! * CPU: Intel® Core™ i5-10300H @ 2.50GHz (4C / 8T)
//!
//! **Write Latency:**
//!
//! Observed measurements for 1,048,576 batched operations,
//!
//! | Metric  | 1 Thread (µs) | 4 Threads (µs) |
//! |:--------|:--------------|:---------------|
//! | P50     |         0.200 |          0.642 |
//! | P90     |         0.500 |          1.559 |
//! | P99     |         1.000 |         11.095 |
//! | Mean    |         1.867 |          7.510 |
//! | Max     |     10051.583 |      30965.759 |
//!
//! **Read Latency:**
//!
//! Observed measurements for 262,144 operations,
//!
//! | Metric  | 1 Thread (µs) | 4 Threads (µs) |
//! |:--------|:--------------|:---------------|
//! | P50     |         0.642 |          0.825 |
//! | P90     |         0.733 |          1.009 |
//! | P99     |         1.008 |          1.558 |
//! | Mean    |         0.653 |          0.834 |
//! | Max     |        29.711 |         78.399 |
//!
//! **Delete Latency:**
//!
//! Observed measurements for 262,144 operations,
//!
//! | Metric  | 1 Thread (µs) | 4 Threads (µs) |
//! |:--------|:--------------|:---------------|
//! | P50     |         0.095 |          0.382 |
//! | P90     |         0.096 |          0.574 |
//! | P99     |         0.096 |          0.765 |
//! | Mean    |         0.094 |          0.666 |
//! | Max     |      1255.423 |       4698.111 |
//!
//! ## Example
//!
//! ```
//! use frozen_core::utils::BufferSize;
//! use kosa::{Kosa, KosaCfg};
//! use std::time::Duration;
//!
//! let dir = tempfile::tempdir().unwrap();
//!
//! let cfg = KosaCfg {
//!     path: dir.path().to_path_buf(),
//!     buffer_size: BufferSize::S64,
//!     initial_available_buffers: 0x1000,
//!     flush_duration: Duration::from_millis(2),
//!     max_memory: 0x400 * 0x400 * 0x40, // 64 MB
//! };
//!
//! let engine = Kosa::new(cfg).unwrap();
//!
//! let payload = b"hello world, fire and forget semantics!";
//! let (ticket, slot_index, n_bufs) = engine.write(payload).unwrap();
//!
//! ticket.wait().unwrap();
//!
//! let read_result = engine.read(slot_index, n_bufs as usize).unwrap();
//! let data = read_result.unwrap();
//!
//! assert_eq!(payload.as_slice(), data.as_slice());
//! engine.delete(slot_index, n_bufs as usize).unwrap();
//! ```

#![deny(missing_docs)]
#![deny(unused_must_use)]
#![allow(unsafe_op_in_unsafe_fn)]

use frozen_core::{ack, bufpool, crc32, error, ffile, utils, wpipe};
use std::{mem, path, sync, time};

mod bitmap;

pub use ack::AckTicket;
pub use utils::BufferSize;

/// Module ID used in [`frozen_core::error::FrozenError`]
pub(crate) const MODULE_ID: u8 = 0x01;

/// All the available configurations for [`Kosa`]
///
/// ## Example
///
/// ```
/// use frozen_core::utils::BufferSize;
/// use kosa::KosaCfg;
/// use std::time::Duration;
///
/// let dir = tempfile::tempdir().unwrap();
/// let cfg = KosaCfg {
///     path: dir.path().to_path_buf(),
///     buffer_size: BufferSize::S64,
///     initial_available_buffers: 0x1000,
///     flush_duration: Duration::from_millis(2),
///     max_memory: 0x400 * 0x400 * 0x40, // 64 MB
/// };
///
/// assert!(cfg.max_memory > 0);
/// assert_eq!(cfg.buffer_size as usize, 0x40);
/// ```
#[derive(Debug)]
pub struct KosaCfg {
    /// The root directory path where database files (`data` and `bmap`) will be stored
    pub path: path::PathBuf,

    /// Size (in bytes) of an individual page/buffer unit in the storage file
    pub buffer_size: utils::BufferSize,

    /// Number of pre-allocated buffer slots in the internal bitmap tracker
    pub initial_available_buffers: usize,

    /// Time interval used by the background `WritePipe` to perform a hard sync to the OS
    pub flush_duration: time::Duration,

    /// Maximum allowed memory (in bytes) to be allocated simultaneously by the engine
    pub max_memory: usize,
}

/// Kośa (कोश) is a reliable page-based storage engine with fire-and-forget durability semantics
///
/// ## Design
///
/// Kośa (कोश) is designed for ultra-low latency I/O operations by offloading durability to a
/// background asynchronous write pipeline (`WritePipe`).
///
/// ## Example
///
/// ```
/// use frozen_core::utils::BufferSize;
/// use kosa::{Kosa, KosaCfg};
/// use std::time::Duration;
///
/// let dir = tempfile::tempdir().unwrap();
///
/// let cfg = KosaCfg {
///     path: dir.path().to_path_buf(),
///     buffer_size: BufferSize::S64,
///     initial_available_buffers: 0x1000,
///     flush_duration: Duration::from_millis(2),
///     max_memory: 0x400 * 0x400 * 0x40, // 64 MB
/// };
///
/// let engine = Kosa::new(cfg).unwrap();
///
/// let payload = b"hello world, fire and forget semantics!";
/// let (ticket, slot_index, n_bufs) = engine.write(payload).unwrap();
///
/// ticket.wait().unwrap();
///
/// let read_result = engine.read(slot_index, n_bufs as usize).unwrap();
/// let data = read_result.unwrap();
///
/// assert_eq!(payload.as_slice(), data.as_slice());
/// engine.delete(slot_index, n_bufs as usize).unwrap();
/// ```
#[derive(Debug)]
pub struct Kosa {
    file: sync::Arc<ffile::FrozenFile>,
    pipe: wpipe::WritePipe,
    bmap: bitmap::BitMap,
    crc32c: crc32::Crc32C,
    pool: bufpool::BufPool,
    buf_size: usize,
}

impl Kosa {
    /// Creates or initializes a new [`Kosa`] storage engine
    ///
    /// ## Example
    ///
    /// ```
    /// use frozen_core::utils::BufferSize;
    /// use kosa::{Kosa, KosaCfg};
    /// use std::time::Duration;
    ///
    /// let dir = tempfile::tempdir().unwrap();
    /// let cfg = KosaCfg {
    ///     path: dir.path().to_path_buf(),
    ///     buffer_size: BufferSize::S64,
    ///     initial_available_buffers: 0x10,
    ///     flush_duration: Duration::from_millis(0x0A),
    ///     max_memory: 0x400 * 0x400,
    /// };
    ///
    /// let engine = Kosa::new(cfg).unwrap();
    ///
    /// let (ticket, slot_index, n_bufs) = engine.write(b"hello, kosa!").unwrap();
    /// ticket.wait().unwrap();
    ///
    /// assert_eq!(slot_index, 0);
    /// assert_eq!(n_bufs, 1);
    /// ```
    pub fn new(cfg: KosaCfg) -> error::FrozenResult<Self> {
        let data_path = cfg.path.join("data");
        let bmap_path = cfg.path.join("bmap");

        let file_cfg = ffile::FrozenFileCfg {
            path: data_path,
            module_id: MODULE_ID,
            buffer_size: cfg.buffer_size as usize,
            initial_available_buffers: cfg.initial_available_buffers,
        };
        let file = sync::Arc::new(ffile::FrozenFile::new(file_cfg)?);

        let pipe_cfg =
            wpipe::WritePipeCfg { module_id: MODULE_ID, flush_duration: cfg.flush_duration };
        let pipe = wpipe::WritePipe::new(pipe_cfg, file.clone())?;

        let pool_cfg =
            bufpool::BufPoolCfg { buffer_size: cfg.buffer_size, max_memory: cfg.max_memory };
        let pool = bufpool::BufPool::new(pool_cfg);

        let init_pages = if cfg.initial_available_buffers < bitmap::SLOTS_PER_PAGE {
            1
        } else {
            (cfg.initial_available_buffers + bitmap::SLOTS_PER_PAGE - 1) / bitmap::SLOTS_PER_PAGE
        };
        let bmap = bitmap::BitMap::new(bmap_path, init_pages, cfg.flush_duration)?;

        Ok(Self {
            file,
            pipe,
            pool,
            bmap,
            crc32c: crc32::Crc32C::new(),
            buf_size: cfg.buffer_size as usize,
        })
    }

    /// Asynchronously writes a slice of bytes to the storage engine w/ fire-and-forget semantics
    ///
    /// ## Panics
    ///
    /// Panics if the internal `BitMap` fails to allocate the required sequential slots (i.e., the
    /// engine has exhausted its `initial_available_buffers` limit and cannot find free space).
    ///
    /// ## Example
    ///
    /// ```
    /// use frozen_core::utils::BufferSize;
    /// use kosa::{Kosa, KosaCfg};
    /// use std::time::Duration;
    ///
    /// let dir = tempfile::tempdir().unwrap();
    /// let engine = Kosa::new(KosaCfg {
    ///     path: dir.path().to_path_buf(),
    ///     buffer_size: BufferSize::S64,
    ///     initial_available_buffers: 0x10,
    ///     flush_duration: Duration::from_millis(0x0A),
    ///     max_memory: 0x400 * 0x400,
    /// })
    /// .unwrap();
    ///
    /// let payload = b"hello, kosa!";
    /// let (ticket, slot_index, n_bufs) = engine.write(payload).unwrap();
    ///
    /// ticket.wait().unwrap();
    /// assert_eq!(slot_index, 0);
    /// assert_eq!(n_bufs, 1);
    /// ```
    #[inline(always)]
    pub fn write(&self, src: &[u8]) -> error::FrozenResult<(ack::AckTicket, u64, u64)> {
        const CRC_SIZE: usize = mem::size_of::<u32>();
        const LEN_SIZE: usize = mem::size_of::<u32>();
        const HEADER_SIZE: usize = CRC_SIZE + LEN_SIZE;

        let payload_size = self.buf_size - HEADER_SIZE;
        let required = src.len().div_ceil(payload_size);

        let allocation = self.pool.allocate(required);
        for (idx, buf) in allocation.iter().enumerate() {
            let start = idx * payload_size;
            let end = (start + payload_size).min(src.len());
            let chunk = &src[start..end];

            let dst = unsafe { std::slice::from_raw_parts_mut(buf, self.buf_size) };

            dst[HEADER_SIZE..HEADER_SIZE + chunk.len()].copy_from_slice(chunk);
            dst[HEADER_SIZE + chunk.len()..].fill(0);

            let chunk_len = (chunk.len() as u32).to_le_bytes();
            dst[CRC_SIZE..HEADER_SIZE].copy_from_slice(&chunk_len);

            let checksum = self.crc32c.crc(&dst[HEADER_SIZE..]).to_le_bytes();
            dst[..CRC_SIZE].copy_from_slice(&checksum);
        }

        let slot_index_opt = self.bmap.allocate(required)?;
        if slot_index_opt.is_none() {
            panic!("Out of storage");
        }

        let slot_index = slot_index_opt.unwrap();
        let request = wpipe::WriteRequest { allocation, slot_index };
        let ticket = self.pipe.write(request)?;

        Ok((ticket, slot_index as u64, required as u64))
    }

    /// Synchronously reads a specified number of blocks from the engine starting at `slot_index`
    ///
    /// ## Why might it return `None`?
    ///
    /// Returning `None` is an expected behavior of the storage engine when a checksum validation
    /// fails.
    ///
    /// ## Example
    ///
    /// ```
    /// use frozen_core::utils::BufferSize;
    /// use kosa::{Kosa, KosaCfg};
    /// use std::time::Duration;
    ///
    /// let dir = tempfile::tempdir().unwrap();
    /// let engine = Kosa::new(KosaCfg {
    ///     path: dir.path().to_path_buf(),
    ///     buffer_size: BufferSize::S64,
    ///     initial_available_buffers: 0x10,
    ///     flush_duration: Duration::from_millis(0x0A),
    ///     max_memory: 0x400 * 0x400,
    /// })
    /// .unwrap();
    ///
    /// let payload = b"hello, kosa!";
    /// let (ticket, slot_index, n_bufs) = engine.write(payload).unwrap();
    /// ticket.wait().unwrap();
    ///
    /// let data = engine.read(slot_index, n_bufs as usize).unwrap().unwrap();
    /// assert_eq!(data, payload);
    /// ```
    #[inline(always)]
    pub fn read(&self, slot_index: u64, required: usize) -> error::FrozenResult<Option<Vec<u8>>> {
        const CRC_SIZE: usize = mem::size_of::<u32>();
        const LEN_SIZE: usize = mem::size_of::<u32>();
        const HEADER_SIZE: usize = CRC_SIZE + LEN_SIZE;

        if required == 0 {
            return Ok(Some(Vec::new()));
        }

        let allocation = self.pool.allocate(required);
        let allocs: Vec<*mut u8> = allocation.iter().collect();
        self.file.preadv(&allocs, slot_index as usize)?;

        let mut output = Vec::with_capacity(required * (self.buf_size - HEADER_SIZE));
        for buf in allocation.iter() {
            let src = unsafe { std::slice::from_raw_parts(buf, self.buf_size) };
            let stored_crc = u32::from_le_bytes(src[..CRC_SIZE].try_into().unwrap());

            let payload = &src[HEADER_SIZE..];
            let computed_crc = self.crc32c.crc(payload);

            if stored_crc != computed_crc {
                return Ok(None);
            }

            let chunk_len =
                u32::from_le_bytes(src[CRC_SIZE..HEADER_SIZE].try_into().unwrap()) as usize;
            let valid_len = chunk_len.min(self.buf_size - HEADER_SIZE);

            output.extend_from_slice(&payload[..valid_len]);
        }

        Ok(Some(output))
    }

    /// Logically deletes records from the storage engine
    ///
    /// ## Example
    ///
    /// ```
    /// use frozen_core::utils::BufferSize;
    /// use kosa::{Kosa, KosaCfg};
    /// use std::time::Duration;
    ///
    /// let dir = tempfile::tempdir().unwrap();
    /// let engine = Kosa::new(KosaCfg {
    ///     path: dir.path().to_path_buf(),
    ///     buffer_size: BufferSize::S64,
    ///     initial_available_buffers: 0x10,
    ///     flush_duration: Duration::from_millis(0x0A),
    ///     max_memory: 0x800 * 0x800,
    /// })
    /// .unwrap();
    ///
    /// let payload = b"temporary record";
    /// let (ticket, slot_index, n_bufs) = engine.write(payload).unwrap();
    /// ticket.wait().unwrap();
    ///
    /// engine.delete(slot_index, n_bufs as usize).unwrap();
    /// ```
    #[inline(always)]
    pub fn delete(&self, slot_index: u64, n: usize) -> error::FrozenResult<()> {
        self.bmap.free(slot_index as usize, n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    const TEST_FLUSH_DUR: time::Duration = time::Duration::from_millis(0x64);
    const TEST_MAX_MEM: usize = 0x400 * 0x400 * 0x0A;

    fn setup_engine(path: &path::Path) -> Kosa {
        let cfg = KosaCfg {
            path: path.to_path_buf(),
            buffer_size: BufferSize::S4096,
            initial_available_buffers: 0x40,
            flush_duration: TEST_FLUSH_DUR,
            max_memory: TEST_MAX_MEM,
        };
        Kosa::new(cfg).unwrap()
    }

    #[test]
    fn ok_write_single_block() {
        let dir = tempdir().unwrap();
        let engine = setup_engine(dir.path());

        let payload = b"hello world, fire and forget. Go";
        let (_ticket, slot_index, _) = engine.write(payload).unwrap();

        assert_eq!(slot_index, 0);
    }

    #[test]
    fn ok_delete_lifecycle() {
        let dir = tempdir().unwrap();
        let engine = setup_engine(dir.path());

        let payload = vec![0x01; 0x100];
        let (_req1, slot1, _) = engine.write(&payload).unwrap();
        assert_eq!(slot1, 0);

        assert!(engine.delete(slot1, 1).is_ok());
    }

    #[test]
    fn ok_read_empty_required() {
        let dir = tempdir().unwrap();
        let engine = setup_engine(dir.path());

        let result = engine.read(0, 0).unwrap();
        assert_eq!(result, Some(Vec::new()), "Expected empty vector for 0 required blocks");
    }

    mod crud {
        use super::*;

        #[test]
        fn ok_write_then_read_success() {
            let dir = tempdir().unwrap();
            let engine = setup_engine(dir.path());

            let payload = b"testing complete read/write lifecycle";
            let (ticket, slot_index, _) = engine.write(payload).unwrap();

            ticket.wait().unwrap();

            let header_size = std::mem::size_of::<u32>() * 2;
            let payload_capacity = engine.buf_size - header_size;
            let required = payload.len().div_ceil(payload_capacity).max(1);

            let read_result = engine.read(slot_index, required).unwrap();
            let read_data = read_result.unwrap();

            assert_eq!(payload.as_slice(), read_data.as_slice());
        }

        #[test]
        fn ok_write_multiple_blocks() {
            let dir = tempdir().unwrap();
            let engine = setup_engine(dir.path());

            let payload = vec![0x42; 0x4000];
            let (ticket, slot_index, _) = engine.write(&payload).unwrap();

            ticket.wait().unwrap();

            let header_size = std::mem::size_of::<u32>() * 2;
            let payload_capacity = engine.buf_size - header_size;
            let required = payload.len().div_ceil(payload_capacity).max(1);

            let read_result = engine.read(slot_index, required).unwrap();
            let read_data = read_result.unwrap();

            assert_eq!(payload, read_data);
        }

        #[test]
        fn ok_delete_allows_reuse() {
            let dir = tempdir().unwrap();
            let engine = setup_engine(dir.path());

            let payload1 = b"first payload";
            let (ticket1, slot1, _) = engine.write(payload1).unwrap();

            ticket1.wait().unwrap();
            assert_eq!(slot1, 0);

            engine.delete(slot1, 1).unwrap();

            let payload2 = b"second payload overwriting";
            let (ticket2, slot2, _) = engine.write(payload2).unwrap();

            ticket2.wait().unwrap();

            let header_size = std::mem::size_of::<u32>() * 2;
            let payload_capacity = engine.buf_size - header_size;
            let required = payload2.len().div_ceil(payload_capacity).max(1);

            let read_result = engine.read(slot2, required).unwrap();
            let read_data = read_result.unwrap();

            assert_eq!(payload2.as_slice(), read_data.as_slice());
        }
    }

    mod stress {
        use super::*;

        #[test]
        fn stress_concurrent_read_write() {
            let dir = tempdir().unwrap();
            let engine = sync::Arc::new(setup_engine(dir.path()));

            let thread_count = 0x0A;
            let ops_per_thread = 0x64;

            let mut handles = vec![];
            for t_idx in 0..thread_count {
                let eng = sync::Arc::clone(&engine);

                handles.push(std::thread::spawn(move || {
                    let header_size = std::mem::size_of::<u32>() * 2;
                    let payload_capacity = 0x1000 - header_size;

                    for op_idx in 0..ops_per_thread {
                        let payload_str = format!(
                            "Thread {} - Op {} - Data must survive the concurrent storm.",
                            t_idx, op_idx
                        );
                        let payload = payload_str.as_bytes();

                        let required = payload.len().div_ceil(payload_capacity).max(1);
                        let (ticket, slot, _) = eng.write(payload).unwrap();

                        ticket.wait().unwrap();

                        let read_result = eng.read(slot, required).unwrap();
                        let read_data = read_result.unwrap();

                        assert_eq!(
                            payload, read_data,
                            "Data corruption detected on thread {} op {}",
                            t_idx, op_idx
                        );
                    }
                }));
            }

            for handle in handles {
                handle.join().unwrap();
            }
        }
    }
}
