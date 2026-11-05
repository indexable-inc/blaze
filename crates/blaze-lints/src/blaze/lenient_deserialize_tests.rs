//! `lenient_deserialize`: what it finds among serde's and blaze_serde's derives, what it leaves
//! alone (the attribute already there, shapes with no field names, `flatten`, `transparent`,
//! `untagged`), and its fix, which leaves the file clean of it.

use base_db::FileId;
use hir_expand::fixture;

use crate::{
    check::{Setup, check},
    conf::Conf,
    fix::fix,
    level::CommandLine,
    lints::disallowed_tests::give_derives,
    tests::lint_given,
};

const FLAGS: &str = "-A clippy::all --warn=blaze::lenient_deserialize";

/// `serde` and `blaze_serde`, each re-exporting a derive crate's `Serialize` and `Deserialize`
/// (which make nothing, [`give_derives`]) that take `#[serde(..)]` as their helper attribute.
const CRATES: &str = r#"
//- minicore: derive
//- /serde_derive/lib.rs crate:proc-macro:serde_derive
#[proc_macro_derive(Serialize, attributes(serde))]
pub fn serialize(item: TokenStream) -> TokenStream {
    item
}
#[proc_macro_derive(Deserialize, attributes(serde))]
pub fn deserialize(item: TokenStream) -> TokenStream {
    item
}
//- /serde/lib.rs crate:serde deps:serde_derive
pub use serde_derive::{Deserialize, Serialize};
//- /blaze_serde_derive/lib.rs crate:proc-macro:blaze_serde_derive
#[proc_macro_derive(Serialize, attributes(serde))]
pub fn serialize(item: TokenStream) -> TokenStream {
    item
}
#[proc_macro_derive(Deserialize, attributes(serde))]
pub fn deserialize(item: TokenStream) -> TokenStream {
    item
}
//- /blaze_serde/lib.rs crate:blaze_serde deps:blaze_serde_derive
pub use blaze_serde_derive::{Deserialize, Serialize};
//- /src/lib.rs crate:main deps:serde,blaze_serde
"#;

fn fixture_of(main: &str) -> String {
    format!("{}{}", CRATES.trim_start_matches('\n'), main.trim_start_matches('\n'))
}

const MESSAGE: &str = "an unknown field in its input is silently dropped";

/// A bare derive through `use`, a path form beside another serde attribute, and an enum with a
/// struct variant are found; the attribute already there, a tuple struct, `transparent`,
/// `flatten`, an enum with no struct variant, `untagged`, a derive of another trait, and test
/// code are not.
#[test]
fn lenient_deserialize_finds_derives_that_drop_unknown_fields() {
    let main = r#"
use serde::Deserialize;

#[derive(Deserialize)]
pub struct Lenient {
    pub a: u32,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Renamed {
    pub a: u32,
}

#[derive(blaze_serde::Deserialize)]
pub enum Message {
    Ping,
    Data { a: u32 },
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct Strict {
    pub a: u32,
}

#[derive(Deserialize)]
pub struct Tuple(pub u32);

#[derive(Deserialize)]
#[serde(transparent)]
pub struct Wrapper {
    pub a: u32,
}

#[derive(Deserialize)]
pub struct Flat {
    pub a: u32,
    #[serde(flatten)]
    pub rest: Strict,
}

#[derive(Deserialize)]
pub enum Plain {
    A,
    B(u32),
}

#[derive(Deserialize)]
#[serde(untagged)]
pub enum Either {
    A { a: u32 },
}

#[derive(Clone)]
pub struct NotSerde {
    pub a: u32,
}

#[cfg(test)]
mod tests {
    #[derive(serde::Deserialize)]
    pub struct InTest {
        pub a: u32,
    }
}
"#;
    let out = lint_given(&fixture_of(main), FLAGS, Conf::default(), give_derives);
    assert_eq!(out.matches(MESSAGE).count(), 3, "{out}");
    assert!(out.contains("struct `Lenient` derives `Deserialize`"), "{out}");
    assert!(out.contains("struct `Renamed` derives `Deserialize`"), "{out}");
    assert!(out.contains("enum `Message` derives `Deserialize`"), "{out}");
    for name in ["Strict", "Tuple", "Wrapper", "Flat", "Plain", "Either", "NotSerde", "InTest"] {
        assert!(!out.contains(&format!("`{name}` derives")), "`{name}` is not found:\n{out}");
    }
}

/// The fix writes `#[serde(deny_unknown_fields)]` under the derive at its indentation, and the
/// fixed file has nothing left for the lint.
#[test]
fn lenient_deserialize_fix_writes_the_attribute_under_the_derive() {
    let main = r#"
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub a: u32,
}

pub mod inner {
    #[derive(blaze_serde::Deserialize)]
    #[serde(rename_all = "kebab-case")]
    pub enum Kind {
        One { a: u32 },
    }
}
"#;
    let want = r#"
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub a: u32,
}

pub mod inner {
    #[derive(blaze_serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    #[serde(rename_all = "kebab-case")]
    pub enum Kind {
        One { a: u32 },
    }
}
"#;
    let mut fx = fixture::db(&fixture_of(main));
    give_derives(&mut fx);
    let file = *fx.files().last().expect("src/lib.rs");
    let mut db = fx.into_db();
    let krate = blaze_query::db::attach(&db, || {
        hir::Crate::all(&db)
            .into_iter()
            .find(|it| it.display_name(&db).is_some_and(|it| it.to_string() == "main"))
            .expect("the fixture has `main`")
    });
    let file = file.file_id(&db);
    let name_of = |it: FileId| (it == file).then(|| "src/lib.rs".to_owned());
    let flags: Vec<String> = FLAGS.split_whitespace().map(str::to_owned).collect();
    let setup = Setup {
        command_line: CommandLine::parse(&flags).expect("the flags read"),
        conf: Conf::default(),
    };
    fix(&mut db, krate, &setup, &name_of).unwrap_or_else(|error| panic!("{error}"));
    let text = db.file_text(file).text(&db).to_string();
    assert_eq!(text, want.trim_start_matches('\n'));
    blaze_query::db::attach(&db, || {
        let found = check(&db, krate, &setup, &name_of).expect("the file is named");
        let left: Vec<&str> = found
            .iter()
            .map(|it| it.diagnostic.message.as_str())
            .filter(|it| it.contains(MESSAGE))
            .collect();
        assert!(
            left.is_empty(),
            "the fixed file has nothing left for the lint:\n{text}\n{left:#?}"
        );
    });
}
