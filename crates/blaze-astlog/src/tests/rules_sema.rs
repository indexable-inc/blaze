//! Two rules from the rules lane (`tools/astlog/rules-sema/`), rewritten with the language's new
//! forms and run beside their old text over the lane's fixtures. The tests run the blocks below,
//! read from this comment, so the text a reader sees is the text that ran.
//!
//! The shared library, `rules/lib/int-arms.astlog` in the view, 7 lines (5 of forms). Both rules
//! include it: before `include`, each kept its own copy of `int-arm`, `made`, `made-earlier`
//! and `generated`.
//!
//! ```lisp
//! ;; Each path that names an enum variant.
//! (rule (variant-path p d) (syntax p "PATH") (resolves p d) (def-kind d "variant"))
//! ;; `k => ..`: an arm keyed by an integer, the integer, and the first variant its expression names.
//! (rule (makes m a i p d) (match-arm m a k b) (int-value k i) (descendant b p) (variant-path p d))
//! (rule (makes-later a p) (makes m a i p d) (makes m a i q e) (precedes q p))
//! (rule (generated m a i d) (makes m a i p d) (not (makes-later a p)))
//! ```
//!
//! `variant-table-disagrees`, old: 60 lines in the lane's file, 27 of them forms. The forms:
//!
//! ```lisp
//! (rule (int-arm a k)
//!   (syntax k "LITERAL_PAT")
//!   (text-match k "^[0-9]+$")
//!   (parent a k)
//!   (kind a "MATCH_ARM"))
//! (rule (made a k p d)
//!   (syntax p "PATH")
//!   (resolves p d)
//!   (def-kind d "variant")
//!   (ancestor a p)
//!   (int-arm a k)
//!   (precedes k p))
//! (rule (made-earlier a p) (made a k p d) (made a k q e) (precedes q p))
//! (rule (generated a k d) (made a k p d) (not (made-earlier a p)))
//! (rule (counted a k p d)
//!   (syntax k "LITERAL")
//!   (text-match k "^[0-9]+$")
//!   (parent a k)
//!   (kind a "MATCH_ARM")
//!   (syntax p "PATH")
//!   (resolves p d)
//!   (def-kind d "variant")
//!   (ancestor a p)
//!   (precedes p k))
//! (rule (counted-earlier a p) (counted a k p d) (counted a k q e) (precedes q p))
//! (rule (indexed a k d) (counted a k p d) (not (counted-earlier a p)))
//! (rule (pair g gk i ik d) (generated g gk d) (indexed i ik d) (same-file g i))
//! (rule (agrees g i) (pair g gk i ik d) (same-text gk ik))
//! (rule (variant-table-disagrees g gk d ik) (pair g gk i ik d) (not (agrees g i)))
//! (lint variant-table-disagrees error "this arm makes `$d` from $gk, and its index arm counts it as $ik")
//! ```
//!
//! New: 8 lines, 7 of forms, plus the library. `match-arm` gives the arm, its pattern and its
//! expression in place of `parent`, `kind` and `ancestor`; `int-value` reads `0x6` and `0b1`
//! where `text-match "^[0-9]+$"` skipped them, so an arm written in hex is joined, and two
//! numbers agree by value where `same-text` compared spellings.
//!
//! ```lisp
//! (include "rules/lib/int-arms.astlog")
//! ;; `E::V .. => k`: an arm whose expression is an integer, and the first variant its pattern names.
//! (rule (counts a i p d) (match-arm m a q b) (int-value b i) (descendant q p) (variant-path p d))
//! (rule (counts-later a p) (counts a i p d) (counts a i q e) (precedes q p))
//! (rule (indexed a i d) (counts a i p d) (not (counts-later a p)))
//! (rule (pair g gi x xi d) (generated m g gi d) (indexed x xi d) (same-file g x))
//! (rule (variant-table-disagrees g gi d xi) (pair g gi x xi d) (not (pair g gi x gi d)))
//! (lint variant-table-disagrees error "this arm makes `$d` from $gi, and its index arm counts it as $xi")
//! ```
//!
//! `int-arm-variant-picker`, old: 33 lines in the lane's file, 15 of them forms. The forms:
//!
//! ```lisp
//! (rule (int-arm a k)
//!   (syntax k "LITERAL_PAT")
//!   (text-match k "^[0-9]+$")
//!   (parent a k)
//!   (kind a "MATCH_ARM"))
//! (rule (made a k p d)
//!   (syntax p "PATH")
//!   (resolves p d)
//!   (def-kind d "variant")
//!   (ancestor a p)
//!   (int-arm a k)
//!   (precedes k p))
//! (rule (made-earlier a p) (made a k p d) (made a k q e) (precedes q p))
//! (rule (generated a d) (made a k p d) (not (made-earlier a p)))
//! (rule (picks m a) (generated a d) (parent l a) (parent m l))
//! (rule (int-arm-variant-picker m n) (picks m a) (count n (picks m x)) (le 4 n))
//! (lint int-arm-variant-picker warning "$n arms pick a variant by a hand-kept integer: draw from the enum's own list")
//! ```
//!
//! New: 4 lines, all forms, plus the library. `match-arm` names the arm's `match`, so `picks`
//! needs no walk up through the arm list; `def-parent` and `variant-count` put the enum and how
//! many variants it has in the message, and the arms are counted per enum.
//!
//! ```lisp
//! (include "rules/lib/int-arms.astlog")
//! (rule (picks m a e) (generated m a i d) (def-parent d e))
//! (rule (int-arm-variant-picker m n e v) (picks m a e) (count n (picks m x e)) (le 4 n) (variant-count e v))
//! (lint int-arm-variant-picker warning "$n arms pick a `$e` variant of $v by a hand-kept integer: draw from the enum's own list")
//! ```

use blaze_api::query::{Cell, Rows};

use super::view;
use crate::query;

const SOURCE: &str = include_str!("rules_sema.rs");

/// The `n`th ```lisp block of this file's module comment.
fn block(n: usize) -> String {
    let comment: String = SOURCE
        .lines()
        .take_while(|line| line.starts_with("//!"))
        .map(|line| format!("{}\n", line.strip_prefix("//! ").unwrap_or(line.trim_start_matches("//!"))))
        .collect();
    let block = comment.split("```lisp\n").nth(n + 1).expect("the comment's lisp block");
    block.split("```").next().expect("a closed block").to_owned()
}

const LIBRARY: usize = 0;
const TABLE_OLD: usize = 1;
const TABLE_NEW: usize = 2;
const PICKER_OLD: usize = 3;
const PICKER_NEW: usize = 4;

/// Each finding's line and message, in the order the query gives them.
#[track_caller]
fn findings(outcome: &Rows) -> Vec<(u32, String)> {
    outcome
        .findings
        .iter()
        .map(|it| match &it.at {
            Some(Cell::Code { line: Some(line), .. }) => (*line, it.message.clone()),
            other => panic!("a finding not at code with a line: {other:?}"),
        })
        .collect()
}

/// The findings of `rules` over `lib`, with the shared library in the view.
fn run(lib: &str, rules: &str) -> Vec<(u32, String)> {
    let fixture = format!(
        "//- /lib.rs crate:lib edition:2021\n{lib}//- /rules/lib/int-arms.astlog\n{}",
        block(LIBRARY)
    );
    let (db, files) = view(&fixture, 0, &["lib.rs", "rules/lib/int-arms.astlog"]);
    let outcome = query(&db, &files, rules).unwrap_or_else(|error| panic!("{error}"));
    findings(&outcome)
}

/// The lane's `variant-table-disagrees` fixture, with `Check`'s arm written `0b1` (it agrees
/// with its index arm) and `Query`'s `0x6` (its index arm counts it as 5).
const TABLE: &str = r#"pub enum Verb {
    Compiling,
    Checking,
}
pub enum Request {
    Commit { base: u64 },
    Check,
    Build,
    Status,
    Stop,
    Query(u32),
}
pub enum Response {
    Committed { root: u64 },
    Unit(Verb),
    Done,
}
pub struct Gen {
    n: u64,
}
impl Gen {
    fn below(&mut self, n: u64) -> u64 {
        self.n % n
    }
    pub fn request(&mut self) -> Request {
        match self.below(6) {
            0 => Request::Commit { base: self.below(9) },
            0b1 => Request::Check,
            2 => Request::Status,
            3 => {
                Request::Build
            }
            0x6 => Request::Query(self.below(3) as u32),
            _ => Request::Stop,
        }
    }
    pub fn response(&mut self) -> Response {
        match self.below(3) {
            0 => Response::Committed { root: self.below(7) },
            1 => Response::Unit(match self.below(2) {
                0 => Verb::Compiling,
                _ => Verb::Checking,
            }),
            _ => Response::Done,
        }
    }
}
pub fn request_index(request: &Request) -> usize {
    match request {
        Request::Commit { .. } => 0,
        Request::Check => 1,
        Request::Build => 2,
        Request::Status => 3,
        Request::Stop => 4,
        Request::Query(_) => 5,
    }
}
pub fn response_index(response: &Response) -> usize {
    match response {
        Response::Committed { .. } => 0,
        Response::Unit(_) => 1,
        Response::Done => 2,
    }
}
pub fn width(n: u64) -> u32 {
    match n {
        0 => 8,
        1 => 16,
        _ => 32,
    }
}
"#;

/// The new `variant-table-disagrees` finds the two swapped arms the old one finds, and the arm
/// written `0x6` that the old one skipped; `0b1 => Request::Check` agrees with `=> 1`, so it is
/// not reported, where comparing spellings would have.
#[test]
fn variant_table_disagrees_rewritten_with_match_arm_and_int_value_also_joins_a_hex_arm() {
    let swapped = [
        (29, "this arm makes `lib::Request::Status` from 2, and its index arm counts it as 3".to_owned()),
        (30, "this arm makes `lib::Request::Build` from 3, and its index arm counts it as 2".to_owned()),
    ];
    assert_eq!(run(TABLE, &block(TABLE_OLD)), swapped);
    let mut new = swapped.to_vec();
    new.push((33, "this arm makes `lib::Request::Query` from 6, and its index arm counts it as 5".to_owned()));
    assert_eq!(run(TABLE, &block(TABLE_NEW)), new);
}

/// The lane's `int-arm-variant-picker` fixture.
const PICKER: &str = r#"pub enum Kind {
    Lib,
    Bin,
    Test,
    Bench,
    Example,
}
pub enum Goal {
    Check,
    Build,
}
pub fn kind(n: u64) -> Kind {
    match n % 5 {
        0 => Kind::Lib,
        1 => Kind::Bin,
        2 => Kind::Test,
        3 => Kind::Bench,
        _ => Kind::Example,
    }
}
pub fn goal(n: u64) -> Goal {
    match n % 2 {
        0 => Goal::Check,
        _ => Goal::Build,
    }
}
pub fn width(n: u64) -> u32 {
    match n % 4 {
        0 => 8,
        1 => 16,
        2 => 32,
        _ => 64,
    }
}
pub fn index(kind: &Kind) -> u64 {
    match kind {
        Kind::Lib => 0,
        Kind::Bin => 1,
        Kind::Test => 2,
        Kind::Bench => 3,
        Kind::Example => 4,
    }
}
"#;

/// The new `int-arm-variant-picker` finds the one `match` the old one finds, and names the enum
/// and its five variants beside the four arms.
#[test]
fn int_arm_variant_picker_rewritten_with_match_arm_names_the_enum_and_its_variant_count() {
    let old = run(PICKER, &block(PICKER_OLD));
    assert_eq!(old, [(13, "4 arms pick a variant by a hand-kept integer: draw from the enum's own list".to_owned())]);
    let new = run(PICKER, &block(PICKER_NEW));
    let message = "4 arms pick a `lib::Kind` variant of 5 by a hand-kept integer: draw from the enum's own list";
    assert_eq!(new, [(13, message.to_owned())]);
}
