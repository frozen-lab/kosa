#![deny(missing_docs)]
#![deny(unused_must_use)]
#![allow(unsafe_op_in_unsafe_fn)]
#![doc = include_str!("../README.md")]

/// A crash-safe page-based storage engine with fire-and-forget durability semantics.
#[derive(Debug)]
pub struct Grave {}
