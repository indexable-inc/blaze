# libzstd-rs-sys

This repository contains a Rust implementation of the zstd file format. It aims to provide excellent performance while introducing memory safety.

## Current state

This codebase has three components:

- the decoder has undergone significant cleanup work and is mostly safe
- the dict builder has been cleaned up, but has some lingering unsafe when calling into the encoder
- the encoder is functional, but still mostly just the raw c2rust translation and hence full of unsafe code

## How to use

Our decoder is ready for experimental use, but is not yet battle-tested.

The primary way to use this library right now is as a static library that is linked into C projects, e.g.

```
> cargo build --features=export-symbols -p libzstd-rs-sys --release --lib

> ls target/release/*.a
target/release/liblibzstd_rs_sys.a
```

In the future we hope to hook into `zstd-rs` for more convenient usage from rust itself.

## Performance

We track our performance at https://trifectatechfoundation.github.io/libzstd-rs-sys-bench/.

## Acknowledgments

This project is based on the [Zstandard reference implementation](https://github.com/facebook/zstd).

## About

libzstd-rs-sys is part of Trifecta Tech Foundation's [Data compression initiative](https://trifectatech.org/initiatives/data-compression/).
