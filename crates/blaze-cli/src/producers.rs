//! The producers' table, made once at the entry (decision D105; docs/v2/plan/design/producer-id.md).
//!
//! - `Row::CodeBuilder` is keyed by [`CODE_BUILDER_SOURCE`], computed at compile time by
//!   `build.rs` via `blaze-producers` over the transitive dependency closure of every crate that
//!   declares `producer = [.., "code-builder"]` (`blaze-ladder`, `blaze-drive`, and all compiler
//!   crates they depend on — `blaze-build`, `blaze-emit`, `blaze-lir`, `blaze-typeck`,
//!   `blaze-lower`, `blaze-scope`, `blaze-surface`, `blaze-solve`, `blaze-own`, `pins/`, lock rows,
//!   plus target & `debug_assertions`), EXCLUDING `blaze-export` and `blaze-cli` CLI/export glue.
//!   Rebuilding `blaze-cli` after editing `blaze-export` or CLI glue changes the binary's `LC_UUID`
//!   without invalidating cached `CodeRecord`s in `~/.blaze/v2` (Rule 21, Rule 52).
//! - Every other row (`Expander`, `Lowering`, `BodyChecker`, `Names`, `Whole`) stays keyed by the
//!   running binary's linker id (`Host::build_id`) until its producing function moves out of
//!   `blaze-cli`.

use blaze_ids::producer::{Producers, Row};

include!(concat!(env!("OUT_DIR"), "/code_builder_producer.rs"));
include!(concat!(env!("OUT_DIR"), "/pack_file_items_producer.rs"));

/// The table: `Row::CodeBuilder` from its compile-time compiler-crate closure digest, and every
/// other row from the running binary's linker id.
pub fn table(build_id: &[u8]) -> Producers {
    Producers::of_build_with_sources(build_id, &[(Row::CodeBuilder, CODE_BUILDER_SOURCE)])
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use blaze_ids::Hasher;
    use blaze_ids::producer::{CodeBuilder, Names, Row, Whole};
    use blaze_producers::{Digest, Entry, Step, Walk};

    use super::*;

    const CODE_BUILDER_DOMAIN: &str = "blaze-cli code-builder cfg 1";
    const PACK_FILE_ITEMS_DOMAIN: &str = "blaze-cli pack-file-items cfg 1";
    const PACK_FILE_ITEMS_ROOTS: [&str; 2] = ["blaze-file-items", "blaze-drive"];
    const PACK_FILE_ITEMS_CLI_FILES: [&str; 3] = [
        "crates/blaze-cli/src/engine_bodies.rs",
        "crates/blaze-cli/src/front_load.rs",
        "crates/blaze-cli/src/sweep.rs",
    ];

    fn drive_walk_with_override(
        workspace_root: &Path,
        walk: &mut Walk<'_>,
        override_file: Option<(&str, &[u8])>,
    ) {
        loop {
            let (is_list, path) = match walk.next().expect("walk.next") {
                Step::Done => break,
                Step::Read(path) => (false, path.to_owned()),
                Step::List(path) => (true, path.to_owned()),
            };
            if !is_list {
                if let Some((ov_path, ov_bytes)) = override_file
                    && ov_path == path
                {
                    walk.read(Some(ov_bytes)).expect("walk.read override");
                    continue;
                }
                let full = workspace_root.join(&path);
                let bytes = match std::fs::read(&full) {
                    Ok(b) => Some(b),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                    Err(e) => panic!("read {path}: {e}"),
                };
                walk.read(bytes.as_deref()).expect("walk.read");
                continue;
            }
            let full = workspace_root.join(&path);
            let dir = match std::fs::read_dir(&full) {
                Ok(d) => d,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    walk.listed(None).expect("walk.listed none");
                    continue;
                }
                Err(e) => panic!("list {path}: {e}"),
            };
            let mut names: Vec<(String, bool)> = Vec::new();
            for entry in dir {
                let entry = entry.expect("dir entry");
                let Ok(name) = entry.file_name().into_string() else { continue };
                let is_dir = std::fs::metadata(entry.path()).expect("metadata").is_dir();
                names.push((name, is_dir));
            }
            let entries: Vec<Entry<'_>> =
                names.iter().map(|(n, d)| Entry { name: n.as_str(), is_dir: *d }).collect();
            walk.listed(Some(&entries)).expect("walk.listed");
        }
    }

    fn walk_workspace_code_builder(
        workspace_root: &Path,
        override_file: Option<(&str, &[u8])>,
    ) -> [u8; 32] {
        let alloc = blaze_own::testing::heap().alloc();
        let mut walk = Walk::for_row(Row::CodeBuilder, alloc).expect("for_row CodeBuilder");
        drive_walk_with_override(workspace_root, &mut walk, override_file);
        let single = walk.finish_row(Row::CodeBuilder).expect("finish_row");
        let row_digest = match single.digest {
            Digest::Source(bytes) => bytes,
            Digest::Refused(why) => panic!("CodeBuilder row refused: {why}"),
        };
        let mut hasher = Hasher::new(CODE_BUILDER_DOMAIN);
        hasher
            .part(&row_digest)
            .part(std::env::consts::ARCH.as_bytes())
            .part(std::env::consts::OS.as_bytes())
            .part(CODE_BUILDER_TARGET_ENV.as_bytes())
            .part(&[u8::from(cfg!(debug_assertions))]);
        hasher.finish().0
    }

    fn walk_workspace_pack_file_items(
        workspace_root: &Path,
        override_file: Option<(&str, &[u8])>,
    ) -> [u8; 32] {
        let alloc = blaze_own::testing::heap().alloc();
        let mut walk = Walk::for_crates(&PACK_FILE_ITEMS_ROOTS, alloc).expect("for_crates pack-file-items");
        drive_walk_with_override(workspace_root, &mut walk, override_file);
        let finished = walk.finish_crates("pack-file-items").expect("finish_crates");
        let closure_digest = match finished.digest {
            Digest::Source(bytes) => bytes,
            Digest::Refused(why) => panic!("pack-file-items closure refused: {why}"),
        };
        let mut hasher = Hasher::new(PACK_FILE_ITEMS_DOMAIN);
        hasher.part(&closure_digest);
        for rel in PACK_FILE_ITEMS_CLI_FILES {
            let bytes = match override_file {
                Some((ov_path, ov_bytes)) if ov_path == rel => ov_bytes.to_vec(),
                _ => std::fs::read(workspace_root.join(rel)).unwrap_or_else(|e| panic!("read {rel}: {e}")),
            };
            hasher.part(rel.as_bytes()).part(&bytes);
        }
        hasher
            .part(std::env::consts::ARCH.as_bytes())
            .part(std::env::consts::OS.as_bytes())
            .part(CODE_BUILDER_TARGET_ENV.as_bytes())
            .part(&[u8::from(cfg!(debug_assertions))]);
        hasher.finish().0
    }

    #[test]
    fn code_builder_producer_is_nonzero_deterministic_and_decoupled_from_build_id() {
        assert_ne!(CODE_BUILDER_SOURCE, [0u8; 32], "CODE_BUILDER_SOURCE must be non-zero");
        let t1 = table(b"mach-o-uuid-build-1");
        let t2 = table(b"mach-o-uuid-build-2");
        assert_eq!(
            t1.get::<CodeBuilder>(),
            t2.get::<CodeBuilder>(),
            "CodeBuilder must stay identical across binary LC_UUID rebuilds"
        );
        assert_eq!(table(b"mach-o-uuid-build-1"), t1, "table must be deterministic");
        assert_ne!(t1.get::<Whole>(), t2.get::<Whole>(), "Whole still tracks binary build id");
        assert_ne!(t1.get::<Names>(), t2.get::<Names>(), "Names still tracks binary build id");
    }

    #[test]
    fn code_builder_changes_on_codegen_edit_and_ignores_export_and_cli_edits() {
        let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let live = walk_workspace_code_builder(&workspace_root, None);
        assert_eq!(
            live, CODE_BUILDER_SOURCE,
            "build.rs CODE_BUILDER_SOURCE must match deterministic workspace walk"
        );

        // Editing a codegen crate (`blaze-emit`) MUST change `ProducerId<CodeBuilder>`.
        let with_emit_edit = walk_workspace_code_builder(
            &workspace_root,
            Some(("crates/blaze-emit/src/sota/mod.rs", b"// simulated codegen edit\n")),
        );
        assert_ne!(
            with_emit_edit, CODE_BUILDER_SOURCE,
            "editing blaze-emit MUST invalidate ProducerId<CodeBuilder> (Rule 21)"
        );

        // Editing `blaze-export` MUST NOT change `ProducerId<CodeBuilder>`.
        let with_export_edit = walk_workspace_code_builder(
            &workspace_root,
            Some(("crates/blaze-export/src/lib.rs", b"// simulated export edit\n")),
        );
        assert_eq!(
            with_export_edit, CODE_BUILDER_SOURCE,
            "editing blaze-export must keep ProducerId<CodeBuilder> stable"
        );

        // Editing CLI glue (`blaze-cli`) MUST NOT change `ProducerId<CodeBuilder>`.
        let with_cli_edit = walk_workspace_code_builder(
            &workspace_root,
            Some(("crates/blaze-cli/src/engine_export.rs", b"// simulated cli edit\n")),
        );
        assert_eq!(
            with_cli_edit, CODE_BUILDER_SOURCE,
            "editing blaze-cli glue must keep ProducerId<CodeBuilder> stable"
        );
    }

    /// Rule 21 / Rule 52 / Rule 54 proof for `PACK_FILE_ITEMS_SOURCE`:
    /// - Positive: editing ANY file on the pack-`FileItems` production/encoding/decoding path
    ///   (`blaze-file-items`, `blaze-surface`, `parser`, `blaze-drive`, `front_load.rs`,
    ///   `engine_bodies.rs`, `sweep.rs`) invalidates `PACK_FILE_ITEMS_SOURCE` and
    ///   `pack_file_items_key_with_producer`.
    /// - Negative: editing unrelated codegen crates (`blaze-emit`, `blaze-lir`, `blaze-ladder`,
    ///   `blaze-export`) or unrelated CLI files (`main.rs`, `engine_export.rs`) keeps
    ///   `PACK_FILE_ITEMS_SOURCE` and `pack_file_items_key_with_producer` unchanged so warm
    ///   toolchain/dependency `FileItems` CAS records stay hot across routine codegen/CLI work.
    #[test]
    fn pack_file_items_producer_covers_complete_path_and_ignores_unrelated_codegen_and_cli_edits() {
        assert_ne!(PACK_FILE_ITEMS_SOURCE, [0u8; 32], "PACK_FILE_ITEMS_SOURCE must be non-zero");
        assert_ne!(
            PACK_FILE_ITEMS_SOURCE, CODE_BUILDER_SOURCE,
            "PACK_FILE_ITEMS_SOURCE must be a dedicated producer distinct from CODE_BUILDER_SOURCE"
        );

        let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let live = walk_workspace_pack_file_items(&workspace_root, None);
        assert_eq!(
            live, PACK_FILE_ITEMS_SOURCE,
            "build.rs PACK_FILE_ITEMS_SOURCE must match deterministic workspace walk"
        );

        let sample_digest = blaze_ids::ContentId::of(
            "producers test pack file",
            b"struct S; impl S { #[cfg_attr(a, inline)] fn f() {} }",
        );
        let base_pack_key = crate::front_load::pack_file_items_key_with_producer(
            blaze_surface::Edition::E2024,
            sample_digest,
            PACK_FILE_ITEMS_SOURCE,
        );

        for rel in [
            "crates/blaze-file-items/src/names_digest.rs",
            "crates/blaze-surface/src/file_items.rs",
            "crates/parser/src/lib.rs",
            "crates/blaze-drive/src/record.rs",
            "crates/blaze-cli/src/front_load.rs",
            "crates/blaze-cli/src/engine_bodies.rs",
            "crates/blaze-cli/src/sweep.rs",
        ] {
            let edited = walk_workspace_pack_file_items(
                &workspace_root,
                Some((rel, b"// simulated pack-file-items path edit\n")),
            );
            assert_ne!(
                edited, PACK_FILE_ITEMS_SOURCE,
                "editing {rel} MUST invalidate PACK_FILE_ITEMS_SOURCE (Rule 21)"
            );
            let edited_pack_key = crate::front_load::pack_file_items_key_with_producer(
                blaze_surface::Edition::E2024,
                sample_digest,
                edited,
            );
            assert_ne!(
                edited_pack_key, base_pack_key,
                "editing {rel} MUST move pack_file_items_key (Rule 21, Rule 54)"
            );
        }

        for rel in [
            "crates/blaze-emit/src/sota/mod.rs",
            "crates/blaze-lir/src/lib.rs",
            "crates/blaze-ladder/src/lib.rs",
            "crates/blaze-export/src/lib.rs",
            "crates/blaze-cli/src/main.rs",
            "crates/blaze-cli/src/engine_export.rs",
        ] {
            let edited = walk_workspace_pack_file_items(
                &workspace_root,
                Some((rel, b"// simulated unrelated edit\n")),
            );
            assert_eq!(
                edited, PACK_FILE_ITEMS_SOURCE,
                "editing unrelated {rel} must NOT invalidate PACK_FILE_ITEMS_SOURCE (Rule 52)"
            );
            let edited_pack_key = crate::front_load::pack_file_items_key_with_producer(
                blaze_surface::Edition::E2024,
                sample_digest,
                edited,
            );
            assert_eq!(
                edited_pack_key, base_pack_key,
                "editing unrelated {rel} must keep pack_file_items_key warm (Rule 52)"
            );
        }

        // Watch list emitted by `build.rs` must include reached crate root directories (even when
        // no `build.rs` exists yet) and source subdirectories so Cargo reruns `build.rs` if a new
        // `build.rs` or `.rs` file is added inside any reached crate (Rule 21).
        let alloc = blaze_own::testing::heap().alloc();
        let mut pfi_walk = Walk::for_crates(&PACK_FILE_ITEMS_ROOTS, alloc).expect("for_crates pack-file-items");
        drive_walk_with_override(&workspace_root, &mut pfi_walk, None);
        let pfi = pfi_walk.finish_crates("pack-file-items").expect("finish_crates");
        for expected in [
            "crates/blaze-file-items",
            "crates/blaze-file-items/src",
            "crates/blaze-file-items/src/walk",
            "crates/blaze-drive",
            "crates/blaze-drive/src",
            "crates/blaze-surface",
            "crates/parser",
            "pins",
        ] {
            assert!(
                pfi.watch.iter().any(|w| w.as_str() == expected),
                "pack-file-items watch must include `{expected}` so newly added files trigger build.rs reruns"
            );
        }
        for excluded in ["crates/blaze-emit", "crates/blaze-lir", "crates/blaze-ladder"] {
            assert!(
                !pfi.watch.iter().any(|w| w.as_str() == excluded),
                "unrelated codegen crate root `{excluded}` must NOT be in pack-file-items watch"
            );
        }
    }

    /// Rule 21 guardrail for `crates/blaze-ladder/Cargo.toml`'s
    /// `[package.metadata.blaze.producer-exclude] code-builder = ["blaze-export"]`:
    /// no file on the `CodeRecord` production path in `blaze-ladder` may reference `blaze_export`.
    #[test]
    fn code_builder_never_calls_blaze_export() {
        let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let code_record_files = [
            "crates/blaze-ladder/src/code_records.rs",
            "crates/blaze-ladder/src/code_reads.rs",
            "crates/blaze-ladder/src/static_entry.rs",
            "crates/blaze-ladder/src/symbols.rs",
            "crates/blaze-ladder/src/body_ids.rs",
            "crates/blaze-ladder/src/run.rs",
            "crates/blaze-ladder/src/run/make_one.rs",
            "crates/blaze-ladder/src/run/lay_states.rs",
        ];
        for rel in code_record_files {
            let text = std::fs::read_to_string(workspace_root.join(rel))
                .unwrap_or_else(|e| panic!("read {rel}: {e}"));
            assert!(
                !text.contains("blaze_export"),
                "{rel} references `blaze_export` while `blaze-export` is excluded from `code-builder` (Rule 21)"
            );
        }
    }
}
