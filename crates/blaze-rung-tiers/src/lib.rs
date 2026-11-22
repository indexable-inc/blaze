//! Every ladder rung at every tier, compared (backend-next.md D97 point 1; D197 root 6, piece
//! (c)): the test is tests/rung_tiers.rs. This crate has no library code. It is a crate of its
//! own because the test needs blaze-ladder (the record walk that builds a rung's LIR),
//! blaze-emit (the tiers) and blaze-rungs (the programs), and blaze-emit sits below blaze-ladder:
//! a test in either one would close a cycle or widen that crate's dependencies for one test.
//!
//! What the test proves, and how, is in the test file's documentation.

#![no_std]
