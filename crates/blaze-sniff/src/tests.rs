extern crate std;

use std::string::String;

use crate::{DirMarkers, FileCategory, is_dir_label, sniff_dir, sniff_file};

#[test]
fn sniffs_magic_headers_images_binaries_and_git_worktree() {
    // 1. PNG with IHDR 1920x1080
    let mut png = [0u8; 24];
    png[..8].copy_from_slice(&[0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n']);
    png[12..16].copy_from_slice(b"IHDR");
    png[16..20].copy_from_slice(&1920u32.to_be_bytes());
    png[20..24].copy_from_slice(&1080u32.to_be_bytes());
    let r = sniff_file("screenshot.dat", &png);
    assert_eq!(r.category, FileCategory::Image);
    assert_eq!(r.label, "PNG");
    assert_eq!(r.dims, Some((1920, 1080)));
    let mut s = String::new();
    r.format_label(&mut s).unwrap();
    assert_eq!(s, "PNG 1920×1080");

    // 2. Mach-O arm64 binary
    let mut macho = [0u8; 16];
    macho[..4].copy_from_slice(&[0xCF, 0xFA, 0xED, 0xFE]);
    macho[4..8].copy_from_slice(&0x0100_000Cu32.to_le_bytes());
    let r = sniff_file("blaze-shell", &macho);
    assert_eq!(r.label, "Mach-O arm64");
    assert!(r.is_binary);

    // 3. Git worktree `.git` file
    let r = sniff_file(".git", b"gitdir: /Volumes/Projects/blaze/.git/worktrees/blaze-shell-1124\n");
    assert_eq!(r.label, "Git worktree");
    assert_eq!(r.category, FileCategory::Config);

    // 4. WebAssembly, Parquet, SQLite, Zstd
    assert_eq!(sniff_file("macro", b"\0asm\x01\0\0\0").label, "WebAssembly");
    assert_eq!(sniff_file("prof", b"PAR1xxxx").label, "Parquet");
    assert_eq!(sniff_file("data", b"SQLite format 3\0").label, "SQLite DB");
    assert_eq!(sniff_file("blob", &[0x28, 0xB5, 0x2F, 0xFD, 0x00]).label, "Zstd");
}

#[test]
fn sniffs_dotfiles_shebangs_and_directories() {
    assert_eq!(sniff_file(".clippy.toml", b"").label, "Clippy config");
    assert_eq!(sniff_file(".rustfmt.toml", b"").label, "Rustfmt config");
    assert_eq!(sniff_file(".gitignore", b"target/\n").label, "Git ignore");
    assert_eq!(sniff_file("LICENSE-APACHE", b"Apache").label, "License");
    assert_eq!(sniff_file("AGENTS.md", b"# blaze").label, "Markdown");
    assert_eq!(sniff_file("run_me", b"#!/usr/bin/env python3\nprint(1)\n").label, "Python script");
    assert_eq!(sniff_file("deploy", b"#!/bin/bash\nset -e\n").label, "Shell script");

    assert_eq!(sniff_dir(".github", DirMarkers::default()).label, "GitHub");
    assert_eq!(sniff_dir(".claude", DirMarkers::default()).label, "Agent config");
    assert_eq!(
        sniff_dir("blaze-shell", DirMarkers { has_cargo_toml: true, ..Default::default() }).label,
        "Rust crate"
    );
    assert!(is_dir_label("Rust crate"));
    assert!(is_dir_label("GitHub"));
    assert!(!is_dir_label("Markdown"));
}

#[test]
fn sim_fuzz_arbitrary_headers_never_panics_and_is_deterministic() {
    use blaze_sim::{Config, check};

    check(Config::cases(160), |tape| {
        let n = tape.int_in(0..=64, "len") as usize;
        let mut buf = std::vec::Vec::with_capacity(n);
        for _ in 0..n {
            buf.push(tape.int_in(0..=255, "b") as u8);
        }
        let names = ["", ".git", "foo.rs", "x.png", "Makefile", "noext", "a/b/c.toml"];
        let name = names[tape.below(names.len() as u64, "name") as usize];
        let a = sniff_file(name, &buf);
        let b = sniff_file(name, &buf);
        if a != b {
            return Err(std::format!("non-deterministic sniff for `{name}`"));
        }
        if a.label.is_empty() {
            return Err(std::string::String::from("empty sniff label"));
        }
        Ok::<(), std::string::String>(())
    });
}
