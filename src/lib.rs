//! Kośa (कोश) is a reliable page-based storage engine with fire-and-forget durability semantics

#![deny(missing_docs)]
#![deny(unused_must_use)]
#![allow(unsafe_op_in_unsafe_fn)]

use frozen_core::{ack, bufpool, crc32, error, ffile, utils, wpipe};
use std::{mem, path, sync, time};

mod bitmap;

pub use utils::BufferSize;
pub use wpipe::WriteRequest;

/// Module ID used in [`frozen_core::error::FrozenError`]
pub(crate) const MODULE_ID: u8 = 0x01;

///
#[derive(Debug)]
pub struct KosaCfg {
    ///
    pub path: path::PathBuf,

    ///
    pub buffer_size: utils::BufferSize,

    ///
    pub initial_available_buffers: usize,

    ///
    pub flush_duration: time::Duration,

    ///
    pub max_memory: usize,
}

/// Kośa (कोश) is a reliable page-based storage engine with fire-and-forget durability semantics
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
    ///
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

    ///
    #[inline(always)]
    pub fn write(&self, src: &[u8]) -> error::FrozenResult<(ack::AckTicket, u64)> {
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

        Ok((ticket, slot_index as u64))
    }

    ///
    #[inline(always)]
    pub fn read(&self, slot_index: u64, required: usize) -> error::FrozenResult<Option<Vec<u8>>> {
        const CRC_SIZE: usize = mem::size_of::<u32>();
        const LEN_SIZE: usize = mem::size_of::<u32>();
        const HEADER_SIZE: usize = CRC_SIZE + LEN_SIZE;

        if required == 0 {
            return Ok(Some(Vec::new()));
        }

        let allocation = self.pool.allocate(required);
        self.file.pread(allocation.first(), slot_index as usize)?;

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

    ///
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
        let (_ticket, slot_index) = engine.write(payload).unwrap();

        assert_eq!(slot_index, 0);
    }

    #[test]
    fn ok_delete_lifecycle() {
        let dir = tempdir().unwrap();
        let engine = setup_engine(dir.path());

        let payload = vec![0x01; 0x100];
        let (_req1, slot1) = engine.write(&payload).unwrap();
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
            let (ticket, slot_index) = engine.write(payload).unwrap();

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

            let payload = vec![0x42; 5000];
            let (ticket, slot_index) = engine.write(&payload).unwrap();

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
            let (ticket1, slot1) = engine.write(payload1).unwrap();

            ticket1.wait().unwrap();
            assert_eq!(slot1, 0);

            engine.delete(slot1, 1).unwrap();

            let payload2 = b"second payload overwriting";
            let (ticket2, slot2) = engine.write(payload2).unwrap();

            ticket2.wait().unwrap();

            let header_size = std::mem::size_of::<u32>() * 2;
            let payload_capacity = engine.buf_size - header_size;
            let required = payload2.len().div_ceil(payload_capacity).max(1);

            let read_result = engine.read(slot2, required).unwrap();
            let read_data = read_result.expect("Failed to read valid data");

            assert_eq!(payload2.as_slice(), read_data.as_slice());
        }
    }

    mod stress {
        use super::*;

        #[test]
        fn stress_concurrent_read_write() {
            use std::sync::Arc;
            use std::thread;

            let dir = tempdir().unwrap();
            let engine = Arc::new(setup_engine(dir.path()));

            let thread_count = 0x0A;
            let ops_per_thread = 0x64;

            let mut handles = vec![];
            for t_idx in 0..thread_count {
                let eng = Arc::clone(&engine);

                handles.push(thread::spawn(move || {
                    let header_size = std::mem::size_of::<u32>() * 2;
                    let payload_capacity = 4096 - header_size;

                    for op_idx in 0..ops_per_thread {
                        let payload_str = format!(
                            "Thread {} - Op {} - Data must survive the concurrent storm.",
                            t_idx, op_idx
                        );
                        let payload = payload_str.as_bytes();

                        let required = payload.len().div_ceil(payload_capacity).max(1);
                        let (ticket, slot) = eng.write(payload).expect("Concurrent write failed");

                        ticket.wait().unwrap();

                        let read_result = eng.read(slot, required).expect("Concurrent read failed");
                        let read_data = read_result.expect("CRC mismatch or data missing");

                        assert_eq!(
                            payload, read_data,
                            "Data corruption detected on thread {} op {}",
                            t_idx, op_idx
                        );
                    }
                }));
            }

            for handle in handles {
                handle.join().expect("A thread panicked during stress testing");
            }
        }
    }
}
