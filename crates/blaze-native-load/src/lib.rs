//! The in-process load of a program's native image (docs/v2/plan/design/native.md, "Loading").
//!
//! blaze-native reads the C, C++ and assembly objects a program's build scripts made, resolves
//! them and lays them out into ONE image at image-relative addresses (base 0), every
//! pc-relative relocation already applied ([`blaze_native::layout::lay`]). What is left for the
//! moment the program starts is small and fixed: where the image goes, the 64-bit cells that
//! hold absolute addresses, the system names it imports, its thread-local descriptors, its
//! initializers and its unwind sections. This crate computes exactly that, in two pure steps:
//!
//! | step | in | out |
//! |---|---|---|
//! | [`plan::plan`] | an [`Image`](blaze_native::image::Image) | a [`plan::LoadPlan`]: base-independent, holds nothing of the machine (rule 25), so a record may hold it beside the image's bytes ([`plan::LoadPlan::encode`]) |
//! | [`bind::bind`] | a plan and a [`bind::Site`]: the base the driver reserved, the address of each import the driver looked up, the Rust words, the launcher's thread-local thunks | [`bind::Writes`]: segments at absolute addresses, the one copy, every 64-bit store sorted by address, the initializers in order, the unwind sections |
//!
//! [`a64`] decodes the instruction forms a relocation patches back into the address they name:
//! the validator's half (rule 74), used by the tests to check every relocated branch lands on
//! its symbol after the image moved to a base.
//!
//! A refusal names the cell, the name or the region, and happens before the driver maps
//! anything: an import no library defines is [`bind::BindError::Unresolved`] up front, never a
//! null call later (rule 3).
//!
//! The driver (not here: blaze-launch-rt) only performs the effects, in the order
//! [`bind::Writes`] documents: reserve, map, copy, store, protect, flush, register unwind,
//! publish exports, run the initializers on the first thread before the entry.

#![cfg_attr(not(test), no_std)]
#![feature(allocator_api, register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![deny(blaze::sans_io_core)]
#![forbid(unsafe_code)]

pub mod a64;
pub mod bind;
pub mod plan;
