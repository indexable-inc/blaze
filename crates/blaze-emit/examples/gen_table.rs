//! Makes the arm64 template table (backend-next.md D99; the backend wave's piece T1) and its pin:
//! crates/blaze-emit/table/arm64.bztab, the bytes `blaze_emit::table_gen::generate` writes, and
//! crates/blaze-emit/table/arm64.json, their BLAKE3 digest, which build.rs holds the committed
//! table to. Run from the workspace's root (the files are under the directory the command
//! started in): `cargo run -p blaze-emit --example gen_table`; commit both files.
//!
//! It is one of blaze's own tools (batch B1, D104): its `fn main` takes the OS root and hands
//! it to `bstd::entry::run_with`, which makes the native world (it writes under the directory
//! it started in, on a start with no run spec); the world is not a `main` parameter, which
//! D104 bars. COUNTED DEBT (D104): `write_table` still holds a whole `Io` where it uses two
//! capabilities (files, read-write, under `crates/blaze-emit/table`, and standard output); the
//! end state is `#[bstd::main] fn main(cx: &Cx, table: Dir<ReadWrite>, out: Out)`, once an
//! ordinary start can grant a folder (wiring/blaze-emit/d104-B-gen_table.md, HOLD).
//!
//! The table is made for arm64 on macOS with no optional features; its templates are the same on
//! Linux (only the head's os byte differs).

use blaze_emit::table_gen::generate;
use blaze_own::{Alloc, Vec};
use blaze_tier::{Arch, Machine, Os};
use bstd::fs::{Content, Durable};
use bstd::{
    Cx, Io,
    entry::{Error, Exit},
};

const MACHINE: Machine = Machine { arch: Arch::Arm64, os: Os::MacOs, features: 0 };

const HEX: &[u8; 16] = b"0123456789abcdef";

fn main() -> Exit {
    bstd::entry::run_with(bstd::entry::Os::process(), |cx, io| write_table(cx, io))
}

/// The table and its pin written under `crates/blaze-emit/table`, and one line saying so.
fn write_table(cx: &Cx, io: Io) -> Result<(), Error> {
    // The program's top names the allocator it hands down (blaze-own, `Alloc::global`).
    let alloc = Alloc::global();
    let mut table = Vec::new_in(alloc);
    let head = generate(MACHINE, &mut table, alloc)?;
    let mut hex = [0u8; 64];
    for (at, byte) in head.digest.into_iter().enumerate() {
        hex[2 * at] = HEX[usize::from(byte >> 4)];
        hex[2 * at + 1] = HEX[usize::from(byte & 0xf)];
    }
    let mut pin = Vec::new_in(alloc);
    pin.extend_from_slice(b"{\"table\": \"arm64.bztab\", \"blake3\": \"");
    pin.extend_from_slice(&hex);
    pin.extend_from_slice(b"\"}\n");

    let dir = io.need_fs()?.open_dir(cx, &bstd::path!("crates/blaze-emit/table"))?;
    dir.publish(cx, &bstd::path!("arm64.bztab"), Content::bytes(&table), Durable::No)?;
    dir.publish(cx, &bstd::path!("arm64.json"), Content::bytes(&pin), Durable::No)?;
    let out = io.need_out()?;
    let hex = core::str::from_utf8(&hex)?;
    out.line(cx, format_args!("wrote crates/blaze-emit/table/arm64.bztab: {} templates, blake3 {hex}", head.count))?;
    Ok(())
}
