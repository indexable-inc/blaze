//! A builtin type read by a constant's evaluation has no text and no crate, so it is no cell of
//! a head's read list (engine_head_consts.rs `text_cell`, `has_no_text`). Found on real
//! projects (2026-11-17: clap, serde, thiserror and a plain crate): every head whose constants'
//! evaluation named a primitive (`the item of key 6275696c74696e0008..`: the bytes `builtin\0`
//! and the type's index) was refused as "of no crate of its scope", and every body that read
//! such a head with it.
//!
//! Expected (`cargo test -p blaze-cli --bin blaze -- engine_builtin_reads`): 1 passed. The
//! effect itself is judged by the real run (`engine-check` of a project whose heads name a
//! primitive's constant): its heads freeze.

use crate::engine_head_consts::has_no_text;

/// Every named builtin type's key has no text; an item's key (a crate's hashed prefix) has.
#[test]
fn engine_builtin_reads_are_no_cell() {
    for name in blaze_scope::BUILTIN_TYPES {
        let def = blaze_scope::builtin_type(name).unwrap_or_else(|| panic!("`{name}` is a builtin type"));
        assert!(has_no_text(def.key), "`{name}`'s key is a builtin's: {:x?}", def.key.0.0);
    }
    // The control (rule 23): a key under a crate's prefix is some crate's item, with a text.
    let mut bytes = [0u8; 32];
    bytes[..8].copy_from_slice(&[0x4e, 0x22, 0x45, 0x37, 0x2e, 0x38, 0x03, 0x90]);
    bytes[8] = 8;
    assert!(!has_no_text(blaze_id::ItemKey(blaze_id::ContentId(bytes))), "a crate's item has a text");
    println!("engine builtin reads: {} builtin types have no text cell; a crate's item has one", blaze_scope::BUILTIN_TYPES.len());
}
