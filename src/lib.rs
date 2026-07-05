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
        let file_cfg = ffile::FrozenFileCfg {
            path: cfg.path.clone(),
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
        let bmap = bitmap::BitMap::new(cfg.path, init_pages, cfg.flush_duration)?;

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

        let payload_size = self.buf_size - CRC_SIZE;
        let required = src.len().div_ceil(payload_size);

        let mut allocation = self.pool.allocate(required);
        for (idx, buf) in allocation.iter().enumerate() {
            let start = idx * payload_size;
            let end = (start + payload_size).min(src.len());
            let chunk = &src[start..end];

            let dst = unsafe { std::slice::from_raw_parts_mut(buf, self.buf_size) };
            let checksum = self.crc32c.crc(chunk).to_le_bytes();

            dst[..CRC_SIZE].copy_from_slice(&checksum);
            dst[CRC_SIZE..CRC_SIZE + chunk.len()].copy_from_slice(chunk);
            dst[CRC_SIZE + chunk.len()..].fill(0);
        }

        let slot_index = self.bmap.allocate(required)?.unwrap();
        let req = wpipe::WriteRequest { allocation, slot_index };

        Ok((req, slot_index as u64))
    }
}
