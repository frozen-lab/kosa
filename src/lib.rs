//! Kośa (कोश) is a reliable page-based storage engine with fire-and-forget durability semantics

#![deny(missing_docs)]
#![deny(unused_must_use)]
#![allow(unsafe_op_in_unsafe_fn)]
#![allow(unused)]

use frozen_core::{bufpool, crc32, error, ffile, utils, wpipe};
use std::{mem, path, ptr, sync, time};

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
        let data_path = cfg.path.with_extension("data");
        let bmap_path = cfg.path.with_extension("bmap");

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
            bmap,
            pool,
            crc32c: crc32::Crc32C::new(),
            buf_size: cfg.buffer_size as usize,
        })
    }

    ///
    #[inline(always)]
    pub fn write(&self, src: &[u8]) -> error::FrozenResult<(wpipe::WriteRequest, u64)> {
        const CRC_SIZE: usize = mem::size_of::<u32>();
        const LEN_SIZE: usize = mem::size_of::<u32>();
        const HEADER_SIZE: usize = CRC_SIZE + LEN_SIZE; // 8 bytes aligned!

        let payload_size = self.buf_size - HEADER_SIZE;
        let required = src.len().div_ceil(payload_size);

        let mut allocation = self.pool.allocate(required);
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

        let slot_index = self.bmap.allocate(required)?.unwrap();
        let req = wpipe::WriteRequest { allocation, slot_index };

        Ok((req, slot_index as u64))
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

    const TEST_FLUSH_DUR: time::Duration = time::Duration::from_millis(100);
    const TEST_MAX_MEM: usize = 1024 * 1024 * 10; // 10MB

    fn setup_engine(path: &path::Path) -> Kosa {
        let cfg = KosaCfg {
            path: path.to_path_buf(),
            buffer_size: BufferSize::S4096,
            initial_available_buffers: 64,
            flush_duration: TEST_FLUSH_DUR,
            max_memory: TEST_MAX_MEM,
        };
        Kosa::new(cfg).unwrap()
    }

    #[test]
    fn ok_write_single_block() {
        let dir = tempdir().expect("Failed to create temp dir");
        let engine = setup_engine(dir.path().join("single_block.db").as_path());

        let payload = b"hello world, fire and forget. Go";
        let (req, slot_index) = engine.write(payload).expect("Write failed");

        assert_eq!(req.allocation.length(), 1);
        assert_eq!(slot_index, 0);

        let buf = req.allocation.first();
        let src = unsafe { std::slice::from_raw_parts(buf, engine.buf_size) };
        let stored_crc = u32::from_le_bytes(src[..4].try_into().unwrap());

        let mut expected_crc_engine = crc32::Crc32C::new();
        let expected_crc = expected_crc_engine.crc(&src[8..]);

        assert_eq!(stored_crc, expected_crc, "CRC checksum mismatch in write buffer");
    }

    #[test]
    fn ok_delete_lifecycle() {
        let dir = tempdir().expect("Failed to create temp dir");
        let engine = setup_engine(dir.path().join("delete.db").as_path());

        let payload = vec![0x01; 0x100];
        let (_req1, slot1) = engine.write(&payload).expect("First write failed");
        assert_eq!(slot1, 0);

        assert!(engine.delete(slot1, 1).is_ok());
    }

    #[test]
    fn ok_read_empty_required() {
        let dir = tempdir().expect("Failed to create temp dir");
        let engine = setup_engine(dir.path().join("read_empty.db").as_path());

        let result = engine.read(0, 0).expect("Read failed");
        assert_eq!(result, Some(Vec::new()), "Expected empty vector for 0 required blocks");
    }
}
