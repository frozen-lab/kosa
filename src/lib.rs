// #![deny(missing_docs)]
// #![deny(unused_must_use)]
#![allow(unused)]
#![allow(unsafe_op_in_unsafe_fn)]
#![doc = include_str!("../README.md")]

mod bitmap;

/// Module ID used for [`FrozenGrave`] in [`FrozenErr`]
pub(crate) const MODULE_ID: u8 = 0x01;

/// A crash-safe page-based storage engine with fire-and-forget durability semantics.
#[derive(Debug)]
pub struct Grave {}
