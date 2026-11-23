use core::alloc::Allocator;

use blaze_own::{String as OwnString, Vec as OwnVec, format_in, testing::heap};
use blaze_sim::{Broke, Config, Tape};

use super::*;

fn dp_edit_distance<T: Eq, A: Allocator + Copy>(a: &[T], b: &[T], alloc: A) -> usize {
    let n = a.len();
    let m = b.len();
    let stride = m + 1;
    let mut dp = OwnVec::new_in(alloc);
    for _ in 0..((n + 1) * stride) {
        dp.push(0usize);
    }
    for i in 0..=n {
        dp[i * stride] = i;
    }
    for j in 0..=m {
        dp[j] = j;
    }
    for i in 1..=n {
        for j in 1..=m {
            dp[i * stride + j] = if a[i - 1] == b[j - 1] {
                dp[(i - 1) * stride + (j - 1)]
            } else {
                1 + dp[(i - 1) * stride + j].min(dp[i * stride + (j - 1)])
            };
        }
    }
    dp[n * stride + m]
}

fn reconstruct_slices<T: Copy, A: Allocator + Copy>(
    changes: &[Change<'_, T>],
    alloc: A,
) -> (OwnVec<T, A>, OwnVec<T, A>) {
    let mut old = OwnVec::new_in(alloc);
    let mut new = OwnVec::new_in(alloc);
    for ch in changes {
        match ch {
            Change::Equal(s) => {
                for &x in *s {
                    old.push(x);
                    new.push(x);
                }
            }
            Change::Delete(s) => {
                for &x in *s {
                    old.push(x);
                }
            }
            Change::Insert(s) => {
                for &x in *s {
                    new.push(x);
                }
            }
        }
    }
    (old, new)
}

fn reconstruct_text<A: Allocator + Copy>(
    changes: &[TextChange<'_>],
    alloc: A,
) -> (OwnString<A>, OwnString<A>) {
    let mut old = OwnString::new_in(alloc);
    let mut new = OwnString::new_in(alloc);
    for ch in changes {
        match ch {
            TextChange::Equal(s) => {
                old.push_str(s);
                new.push_str(s);
            }
            TextChange::Delete(s) => old.push_str(s),
            TextChange::Insert(s) => new.push_str(s),
        }
    }
    (old, new)
}

fn draw_text<A: Allocator>(tape: &Tape, label: &'static str, alloc: A) -> OwnString<A> {
    const LINE_POOL: &[&str] = &[
        "",
        "alpha",
        "beta",
        "gamma",
        "  \"start\": \"bun index.ts\",",
        "  \"test\": \"bun test\",",
        "{",
        "}",
        "fn main() {}",
        "café ☕",
        "line with\rcrlf",
    ];
    let n = usize::try_from(tape.int_in(0..=14, label)).unwrap_or(0);
    let mut out = OwnString::new_in(alloc);
    for i in 0..n {
        let pick = usize::try_from(tape.below(LINE_POOL.len() as u64, "line")).unwrap_or(0);
        out.push_str(LINE_POOL[pick]);
        if i + 1 < n || tape.coin("trailing newline") {
            out.push('\n');
        }
    }
    out
}

#[test]
fn bun_diff_screenshot_examples() {
    let h = heap();
    let a = "{\n  \"name\": \"my-app\",\n  \"type\": \"module\",\n  \"scripts\": {\n    \"start\": \"node index.js\",\n    \"test\": \"jest\",\n    \"lint\": \"eslint .\"\n  }\n}\n";
    let b = "{\n  \"name\": \"my-app\",\n  \"type\": \"module\",\n  \"scripts\": {\n    \"start\": \"bun index.ts\",\n    \"test\": \"bun test\",\n    \"lint\": \"eslint .\"\n  }\n}\n";

    let patch = create_patch(
        a,
        b,
        PatchOpts {
            algorithm: Algorithm::Myers,
            context: 4,
            old_name: None,
            new_name: None,
            budget: DEFAULT_BUDGET,
            alloc: h.alloc(),
        },
    )
    .expect("patch created");
    assert!(patch.as_str().contains("@@ -1,9 +1,9 @@"));
    assert!(patch.as_str().contains("-    \"start\": \"node index.js\","));
    assert!(patch.as_str().contains("+    \"start\": \"bun index.ts\","));

    let applied = apply_patch_text(
        a,
        patch.as_str(),
        ParsePatchLimits::USUAL,
        ApplyOpts { max_offset: 0, alloc: h.alloc() },
    )
    .expect("patch applies");
    assert_eq!(applied.as_str(), b);

    let parsed = parse_patch(patch.as_str(), ParsePatchLimits::USUAL, h.alloc()).expect("parses");
    assert_eq!(parsed.hunks.len(), 1);
    assert_eq!(parsed.stats(), DiffStats { equal: 7, inserted: 2, deleted: 2 });

    let reversed =
        apply_patch_reverse(b, &parsed, ApplyOpts { max_offset: 0, alloc: h.alloc() })
            .expect("reverse applies");
    assert_eq!(reversed.as_str(), a);

    let word_diff = words(
        "node index.js",
        "bun index.ts",
        DiffOpts { algorithm: Algorithm::Myers, budget: DEFAULT_BUDGET, alloc: h.alloc() },
    )
    .expect("words diff");
    assert_eq!(
        word_diff.as_slice(),
        &[
            TextChange::Delete("node"),
            TextChange::Insert("bun"),
            TextChange::Equal(" index."),
            TextChange::Delete("js"),
            TextChange::Insert("ts"),
        ]
    );

    let char_diff = chars(
        "color",
        "colour",
        DiffOpts { algorithm: Algorithm::Myers, budget: DEFAULT_BUDGET, alloc: h.alloc() },
    )
    .expect("chars diff");
    assert_eq!(
        char_diff.as_slice(),
        &[TextChange::Equal("colo"), TextChange::Insert("u"), TextChange::Equal("r")]
    );

    let arr_diff = arrays(
        &[1, 2, 3],
        &[1, 3, 4],
        DiffOpts { algorithm: Algorithm::Myers, budget: DEFAULT_BUDGET, alloc: h.alloc() },
    )
    .expect("arrays diff");
    assert_eq!(
        arr_diff.as_slice(),
        &[
            Change::Equal(&[1]),
            Change::Delete(&[2]),
            Change::Equal(&[3]),
            Change::Insert(&[4]),
        ]
    );

    let colored = format_patch_ansi(patch.as_str(), h.alloc()).expect("ansi patch");
    assert!(crate::ansi::has_ansi(colored.as_str()));
    let stripped = crate::ansi::strip_in(colored.as_str(), h.alloc()).expect("strip ansi");
    assert_eq!(stripped.as_str(), patch.as_str());
}

#[test]
fn multi_file_patch_and_fuzz_offset() {
    let h = heap();
    let multi = "\
diff --git a/one.txt b/one.txt
index 111..222 100644
--- a/one.txt
+++ b/one.txt
@@ -1 +1 @@
-old
+new
--- a/two.txt
+++ b/two.txt
@@ -1,2 +1,3 @@ fn heading()
 a
+inserted
 b
";
    let patches = parse_patches(multi, ParsePatchLimits::USUAL, h.alloc()).expect("multi parse");
    assert_eq!(patches.len(), 2);
    assert_eq!(patches[0].old_name, Some("a/one.txt"));
    assert_eq!(patches[1].new_name, Some("b/two.txt"));
    assert_eq!(patches[1].hunks[0].header, Some("fn heading()"));

    let shifted_base = "header1\nheader2\na\nb\n";
    let strict =
        apply_patch(shifted_base, &patches[1], ApplyOpts { max_offset: 0, alloc: h.alloc() });
    assert!(matches!(strict, Err(ApplyPatchError::HunkMismatch { hunk: 0, .. })));

    let fuzzy =
        apply_patch(shifted_base, &patches[1], ApplyOpts { max_offset: 4, alloc: h.alloc() })
            .expect("fuzzy applies");
    assert_eq!(fuzzy.as_str(), "header1\nheader2\na\ninserted\nb\n");
}

#[test]
fn budget_exceeded_is_typed_error() {
    let h = heap();
    let mut a = OwnVec::new_in(h.alloc());
    let mut b = OwnVec::new_in(h.alloc());
    for i in 0u32..100 {
        a.push(i);
        b.push(i + 100);
    }
    let res = slices(a.as_slice(), b.as_slice(), 50, h.alloc());
    assert_eq!(res, Err(DiffError::BudgetExceeded { steps: 50 }));
}

#[test]
fn property_slices_match_dp_oracle() {
    let h = heap();
    blaze_sim::check(Config::cases(256), |tape| {
        let n = usize::try_from(tape.int_in(0..=18, "n")).unwrap_or(0);
        let m = usize::try_from(tape.int_in(0..=18, "m")).unwrap_or(0);
        let alpha = u64::try_from(tape.int_in(2..=6, "alphabet")).unwrap_or(2);
        let mut a = OwnVec::new_in(h.alloc());
        for _ in 0..n {
            a.push(u8::try_from(tape.below(alpha, "a")).unwrap_or(0));
        }
        let mut b = OwnVec::new_in(h.alloc());
        for _ in 0..m {
            b.push(u8::try_from(tape.below(alpha, "b")).unwrap_or(0));
        }

        let expected_dist = dp_edit_distance(a.as_slice(), b.as_slice(), h.alloc());
        let myers_dist =
            edit_distance(a.as_slice(), b.as_slice(), DEFAULT_BUDGET, h.alloc()).expect("dist");
        assert_eq!(myers_dist, expected_dist, "edit_distance disagrees with DP oracle");

        let changes = slices(a.as_slice(), b.as_slice(), DEFAULT_BUDGET, h.alloc()).expect("slices");
        let (rec_a, rec_b) = reconstruct_slices(changes.as_slice(), h.alloc());
        assert_eq!(rec_a.as_slice(), a.as_slice());
        assert_eq!(rec_b.as_slice(), b.as_slice());

        let actual_edits: usize = changes
            .as_slice()
            .iter()
            .map(|c| match c {
                Change::Equal(_) => 0,
                Change::Delete(s) | Change::Insert(s) => s.len(),
            })
            .sum();
        assert_eq!(actual_edits, expected_dist, "Myers script is not shortest");

        let pat_changes = arrays(
            a.as_slice(),
            b.as_slice(),
            DiffOpts { algorithm: Algorithm::Patience, budget: DEFAULT_BUDGET, alloc: h.alloc() },
        )
        .expect("patience");
        let (pat_a, pat_b) = reconstruct_slices(pat_changes.as_slice(), h.alloc());
        assert_eq!(pat_a.as_slice(), a.as_slice());
        assert_eq!(pat_b.as_slice(), b.as_slice());

        let sim =
            similarity(a.as_slice(), b.as_slice(), DEFAULT_BUDGET, h.alloc()).expect("similarity");
        assert!((0.0..=1.0).contains(&sim));
        if a.as_slice() == b.as_slice() {
            assert_eq!(sim, 1.0);
        }
    });
}

fn check_patch_round_trip(
    a: &str,
    b: &str,
    context: usize,
    algo: Algorithm,
    plant: DiffPlant,
) -> Result<(), Broke> {
    let h = heap();
    let patch_text = create_patch_planted(
        a,
        b,
        PatchOpts {
            algorithm: algo,
            context,
            old_name: Some("a/file.txt"),
            new_name: Some("b/file.txt"),
            budget: DEFAULT_BUDGET,
            alloc: h.alloc(),
        },
        plant,
    )
    .map_err(|e| Broke::Oracle(format_in!(h.alloc(), "create_patch failed: {e}").as_str().into()))?;

    let parsed = parse_patch(patch_text.as_str(), ParsePatchLimits::USUAL, h.alloc()).map_err(
        |e| Broke::Oracle(format_in!(h.alloc(), "parse_patch failed: {e}").as_str().into()),
    )?;

    let forward = apply_patch(a, &parsed, ApplyOpts { max_offset: 0, alloc: h.alloc() }).map_err(
        |e| Broke::Oracle(format_in!(h.alloc(), "apply_patch failed: {e}").as_str().into()),
    )?;
    if forward.as_str() != b {
        return Err(Broke::Oracle(
            format_in!(h.alloc(), "forward apply mismatch: expected {b:?}, got {:?}", forward.as_str())
                .as_str()
                .into(),
        ));
    }

    let backward = apply_patch_reverse(b, &parsed, ApplyOpts { max_offset: 0, alloc: h.alloc() })
        .map_err(|e| {
            Broke::Oracle(format_in!(h.alloc(), "apply_patch_reverse failed: {e}").as_str().into())
        })?;
    if backward.as_str() != a {
        return Err(Broke::Oracle(
            format_in!(
                h.alloc(),
                "reverse apply mismatch: expected {a:?}, got {:?}",
                backward.as_str()
            )
            .as_str()
            .into(),
        ));
    }

    Ok(())
}

/// The invariant: unified diff patches and text diffs reconstruct the exact original and target texts.
#[test]
fn property_patch_and_text_diffs_round_trip() {
    let h = heap();
    blaze_sim::check(Config::cases(256), |tape| {
        let a = draw_text(tape, "a_lines", h.alloc());
        let b = draw_text(tape, "b_lines", h.alloc());
        let context = usize::try_from(tape.int_in(0..=4, "context")).unwrap_or(0);
        let algo = if tape.coin("patience") { Algorithm::Patience } else { Algorithm::Myers };

        check_patch_round_trip(a.as_str(), b.as_str(), context, algo, DiffPlant::None)?;

        for ch in [
            lines(
                a.as_str(),
                b.as_str(),
                DiffOpts { algorithm: algo, budget: DEFAULT_BUDGET, alloc: h.alloc() },
            )
            .expect("lines"),
            words(
                a.as_str(),
                b.as_str(),
                DiffOpts { algorithm: algo, budget: DEFAULT_BUDGET, alloc: h.alloc() },
            )
            .expect("words"),
            chars(
                a.as_str(),
                b.as_str(),
                DiffOpts { algorithm: algo, budget: DEFAULT_BUDGET, alloc: h.alloc() },
            )
            .expect("chars"),
        ] {
            let (ra, rb) = reconstruct_text(ch.as_slice(), h.alloc());
            if ra.as_str() != a.as_str() || rb.as_str() != b.as_str() {
                return Err(Broke::Oracle(
                    format_in!(h.alloc(), "text reconstruction failed").as_str().into(),
                ));
            }
        }
        Ok(())
    });
}

#[test]
fn the_oracle_catches_skipped_no_newline_marker() {
    let h = heap();
    blaze_sim::control(
        Config::cases(256),
        DiffPlant::None,
        DiffPlant::SkipNoNewlineMarker,
        "",
        |tape, plant| {
            let a = draw_text(tape, "a_lines", h.alloc());
            let b = draw_text(tape, "b_lines", h.alloc());
            let context = usize::try_from(tape.int_in(0..=3, "context")).unwrap_or(0);
            check_patch_round_trip(a.as_str(), b.as_str(), context, Algorithm::Myers, plant)
        },
    );
}

#[test]
fn the_oracle_catches_off_by_one_hunk_count() {
    let h = heap();
    blaze_sim::control(
        Config::cases(256),
        DiffPlant::None,
        DiffPlant::OffByOneHunkCount,
        "",
        |tape, plant| {
            let a = draw_text(tape, "a_lines", h.alloc());
            let b = draw_text(tape, "b_lines", h.alloc());
            let context = usize::try_from(tape.int_in(0..=3, "context")).unwrap_or(0);
            check_patch_round_trip(a.as_str(), b.as_str(), context, Algorithm::Myers, plant)
        },
    );
}

#[test]
fn property_parse_patch_hostile_input_never_panics() {
    let h = heap();
    const FRAGS: &[&str] = &[
        "--- a\n",
        "+++ b\n",
        "@@ -1,1 +1,1 @@\n",
        "@@ -0,0 +1 @@\n",
        "@@ bad @@\n",
        " ctx\n",
        "-del\n",
        "+add\n",
        "\\ No newline at end of file\n",
        "\n",
        "garbage",
        "diff --git a/x b/x\n",
    ];
    blaze_sim::check(Config::cases(256), |tape| {
        let count = usize::try_from(tape.int_in(0..=16, "frags")).unwrap_or(0);
        let mut text = OwnString::new_in(h.alloc());
        for _ in 0..count {
            let idx = usize::try_from(tape.below(FRAGS.len() as u64, "frag")).unwrap_or(0);
            text.push_str(FRAGS[idx]);
        }
        let limits = ParsePatchLimits { most_bytes: 512, most_hunks: 4, most_lines: 16 };
        let _ = parse_patches(text.as_str(), limits, h.alloc());
    });
}

#[test]
fn merge3_clean_convergent_and_conflict() {
    let h = heap();
    let base = "line1\nline2\nline3\nline4\n";
    let ours = "LINE1\nline2\nline3\nline4\n";
    let theirs = "line1\nline2\nline3\nLINE4\n";

    // Disjoint edits merge cleanly.
    let res = merge3(
        base,
        ours,
        theirs,
        MergeOpts {
            algorithm: Algorithm::Myers,
            budget: DEFAULT_BUDGET,
            style: ConflictStyle::Standard,
            base_label: "base",
            ours_label: "ours",
            theirs_label: "theirs",
            alloc: h.alloc(),
        },
    )
    .expect("merge3");
    assert!(res.is_clean());
    let merged = res
        .render_in(ConflictStyle::Standard, "base", "ours", "theirs", h.alloc())
        .expect("render");
    assert_eq!(merged.as_str(), "LINE1\nline2\nline3\nLINE4\n");

    // Identical edits on both sides converge with zero conflicts.
    let same = merge3(
        base,
        ours,
        ours,
        MergeOpts {
            algorithm: Algorithm::Myers,
            budget: DEFAULT_BUDGET,
            style: ConflictStyle::Standard,
            base_label: "base",
            ours_label: "ours",
            theirs_label: "theirs",
            alloc: h.alloc(),
        },
    )
    .expect("convergent");
    assert!(same.is_clean());
    assert_eq!(
        same.render_in(ConflictStyle::Standard, "base", "ours", "theirs", h.alloc())
            .expect("render")
            .as_str(),
        ours
    );

    // Divergent edits to the same line produce a conflict with diff3 markers.
    let conf = merge3(
        "a\nb\nc\n",
        "a\nOURS\nc\n",
        "a\nTHEIRS\nc\n",
        MergeOpts {
            algorithm: Algorithm::Myers,
            budget: DEFAULT_BUDGET,
            style: ConflictStyle::Diff3,
            base_label: "base",
            ours_label: "ours",
            theirs_label: "theirs",
            alloc: h.alloc(),
        },
    )
    .expect("conflict");
    assert!(conf.has_conflicts());
    assert_eq!(conf.conflict_count, 1);
    let txt = conf
        .render_in(ConflictStyle::Diff3, "base", "ours", "theirs", h.alloc())
        .expect("render");
    assert_eq!(
        txt.as_str(),
        "a\n<<<<<<< ours\nOURS\n||||||| base\nb\n=======\nTHEIRS\n>>>>>>> theirs\nc\n"
    );
}

#[test]
fn inline_ansi_patch_strips_back_identically() {
    let h = heap();
    let a = "fn main() {\n    let x = \"node index.js\";\n}\n";
    let b = "fn main() {\n    let x = \"bun index.ts\";\n}\n";
    let patch = create_patch(
        a,
        b,
        PatchOpts {
            algorithm: Algorithm::Myers,
            context: 3,
            old_name: Some("a/main.rs"),
            new_name: Some("b/main.rs"),
            budget: DEFAULT_BUDGET,
            alloc: h.alloc(),
        },
    )
    .expect("patch");
    let inline = format_patch_ansi_inline(patch.as_str(), h.alloc()).expect("inline ansi");
    assert!(crate::ansi::has_ansi(inline.as_str()));
    let stripped = crate::ansi::strip_in(inline.as_str(), h.alloc()).expect("strip");
    assert_eq!(stripped.as_str(), patch.as_str());
}

#[test]
fn value_diff_and_rfc6902_patch_round_trip() {
    let h = heap();
    let old = crate::json::read_in(
        br#"{"name":"app","scripts":{"start":"node","test":"jest"},"tags":[1,2,3],"a/b~c":true}"#,
        crate::json::Limits::SHIPPED,
        h.alloc(),
    )
    .expect("old json");
    let new = crate::json::read_in(
        br#"{"name":"app","scripts":{"start":"bun","lint":"oxlint"},"tags":[1,4],"a/b~c":false,"extra":null}"#,
        crate::json::Limits::SHIPPED,
        h.alloc(),
    )
    .expect("new json");
    let patch_ops = diff_values(&old, &new, h.alloc()).expect("diff values");
    assert!(!patch_ops.is_empty());
    let applied = apply_value_patch(&old, patch_ops.as_slice(), h.alloc()).expect("apply");
    assert_eq!(applied, new);
}
