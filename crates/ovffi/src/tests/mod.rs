//! Tests that drive the crate through its C entry points, as the iOS shell
//! does.
//!
//! The test code hands the entry points pointers it owns for the duration of
//! each call, so its many `unsafe` blocks are not annotated one by one.
#![allow(clippy::undocumented_unsafe_blocks)]

mod direct;
mod ffi;
mod header;
mod lan;
mod pin;
mod relay;
