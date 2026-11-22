//! An ordinary start's grants (defaults.rs): `--grant` first, then the attribute's default,
//! then the refusal; and the one parser's spellings. Compile-time refusals are fixture pairs
//! under crates/blaze-std/refusals/ (`main_default_no_param`, `main_default_wrong_spelling`).

use super::{OrdinaryError, Shape, Spelling, ordinary_grants};
use crate::{
    env::Env,
    fs::{Dir, ReadOnly},
    grant::{Kind, LaunchAlloc, Manifest, Param, Prepared, prepare},
    net::Net,
    spec::{RunSpec, Scope},
};

fn alloc() -> LaunchAlloc {
    blaze_own::testing::heap().shared()
}

fn bare(a: LaunchAlloc) -> RunSpec<LaunchAlloc> {
    RunSpec::defaults(blaze_own::Vec::new_in(a), blaze_own::Vec::new_in(a), blaze_own::String::from_str_in(".", a), a)
}

/// The scope `spec` grants `name`, as `(kind, its text)`: `dir`'s path, `env`'s names joined.
fn granted(spec: &RunSpec<LaunchAlloc>, name: &str) -> Option<(&'static str, std::string::String)> {
    let granted = spec.grant(name)?;
    let text = match &granted.scope {
        Scope::Dir { path, write } => std::format!("{}{}", path.as_str(), if *write { " rw" } else { "" }),
        Scope::Net { at, listen } => std::format!("{}{}", if *listen { "listen:" } else { "" }, at.as_str()),
        Scope::Env { names } => names.iter().map(|it| it.as_str()).collect::<std::vec::Vec<_>>().join(","),
        _ => std::string::String::from("?"),
    };
    Some((granted.scope.kind_name(), text))
}

const PARAMS: [Param; 3] = [Param::new("data", Kind::Dir { write: false }), Param::new("env", Kind::Env), Param::new("api", Kind::Net)];

static DEFAULTS: Manifest =
    Manifest::new(&PARAMS).with_defaults(&crate::__grant_spellings!(data = dir("."), env(HOME, PATH), api = connect("example.com:443")));

/// The spellings: each becomes its `--grant` value, and the bare `env(..)` is the parameter
/// `env`'s.
/// Expected: `test grant::defaults::tests::each_spelling_is_its_grant_value ... ok`.
#[test]
fn each_spelling_is_its_grant_value() {
    let spelt = crate::__grant_spellings!(
        data = dir("a/b"), env(HOME, PATH), vars = env(CC), api = connect("h:1"), port = listen("h:2"), mic = whole, cam = device("front"),
    );
    assert_eq!(
        spelt,
        [
            Spelling::new("data", Shape::Dir, "a/b"),
            Spelling::new("env", Shape::Env, "HOME,PATH"),
            Spelling::new("vars", Shape::Env, "CC"),
            Spelling::new("api", Shape::Net, "h:1"),
            Spelling::new("port", Shape::Net, "listen:h:2"),
            Spelling::new("mic", Shape::Whole, "whole"),
            Spelling::new("cam", Shape::Whole, "device:front"),
        ]
    );
    let none: [Spelling; 0] = crate::__grant_spellings!();
    assert_eq!(none, []);
}

/// With no `--grant`, each parameter takes the attribute's default, and no argument is taken.
/// Control: a manifest with no defaults grants nothing.
/// Expected: `test grant::defaults::tests::a_default_fills_what_no_grant_names ... ok`.
#[test]
fn a_default_fills_what_no_grant_names() {
    let a = alloc();
    let mut spec = bare(a);
    assert_eq!(ordinary_grants(&DEFAULTS, &["prog", "x"], &mut spec, a), Ok(0));
    assert_eq!(granted(&spec, "data"), Some(("dir", ".".into())));
    assert_eq!(granted(&spec, "env"), Some(("env", "HOME,PATH".into())));
    assert_eq!(granted(&spec, "api"), Some(("net", "example.com:443".into())));

    static PLAIN: Manifest = Manifest::new(&PARAMS);
    let mut spec = bare(a);
    assert_eq!(ordinary_grants(&PLAIN, &["prog"], &mut spec, a), Ok(0));
    assert!(spec.grants.is_empty());
}

/// A leading `--grant` wins over the default and is taken from the command line (both
/// spellings, and a `--` that ends them); an argument after the first other one is `main`'s.
/// Expected: `test grant::defaults::tests::a_grant_argument_wins_over_the_default ... ok`.
#[test]
fn a_grant_argument_wins_over_the_default() {
    let a = alloc();
    let mut spec = bare(a);
    let args = ["prog", "--grant", "data=fixtures", "--grant=env=CC", "--", "--grant", "api=other:1"];
    assert_eq!(ordinary_grants(&DEFAULTS, &args, &mut spec, a), Ok(4));
    assert_eq!(granted(&spec, "data"), Some(("dir", "fixtures".into())));
    assert_eq!(granted(&spec, "env"), Some(("env", "CC".into())));
    // After `--`, `--grant` is `main`'s argument: `api` keeps its default.
    assert_eq!(granted(&spec, "api"), Some(("net", "example.com:443".into())));

    let mut spec = bare(a);
    let twice = ordinary_grants(&DEFAULTS, &["prog", "--grant", "data=a", "--grant", "data=b"], &mut spec, a);
    assert_eq!(twice, Err(OrdinaryError::Twice { grant: "data=b" }));
    let mut spec = bare(a);
    assert_eq!(ordinary_grants(&DEFAULTS, &["prog", "--grant"], &mut spec, a), Err(OrdinaryError::NoValue));
}

static BAD_DEFAULT: Manifest = Manifest::new(&[Param::new("api", Kind::Net)]).with_defaults(&crate::__grant_spellings!(api = connect("no port")));

/// A default whose value does not read as its kind's scope is refused at the start, naming the
/// attribute and the `--grant` line it is; the control: a `--grant` for it is read instead, and
/// the bad default is never read.
/// Expected: `test grant::defaults::tests::a_default_that_does_not_read_is_refused_naming_the_attribute ... ok`.
#[test]
fn a_default_that_does_not_read_is_refused_naming_the_attribute() {
    let a = alloc();
    let mut spec = bare(a);
    let refused = ordinary_grants(&BAD_DEFAULT, &["prog"], &mut spec, a).expect_err("the default does not read");
    let said = std::format!("{refused}");
    assert!(said.contains("the default of `api` in `#[bstd::main(..)]` (`--grant api=no port`)"), "{said}");
    let mut spec = bare(a);
    assert_eq!(ordinary_grants(&BAD_DEFAULT, &["prog", "--grant", "api=example.com:1"], &mut spec, a), Ok(2));
}

static HERE: Manifest = Manifest::new(&[Param::new("data", Kind::Dir { write: false })]).with_defaults(&crate::__grant_spellings!(data = dir(".")));

fn takes_here(_cx: &crate::cx::Cx, data: Dir<ReadOnly>) -> crate::entry::Status {
    let _ = data;
    crate::entry::Status(11)
}

/// `dir(".")` grants the start folder itself: the program starts with it (11). Controls: the
/// `.` part inside a longer path is still no folder grant (`--grant data=a/./b`), and the spec
/// reads a `.` folder back.
/// Expected: `test grant::defaults::tests::the_start_folder_is_granted_as_dot ... ok`.
#[test]
fn the_start_folder_is_granted_as_dot() {
    let a = alloc();
    let mut said = std::string::String::new();
    let body = crate::launch_body!(takes_here; data: Dir<ReadOnly>);
    let status = crate::entry::launch_in(&HERE, None, crate::io::Io::sim(1), &mut said, body);
    assert_eq!((status, said.as_str()), (crate::entry::Status(11), ""));
    let mut spec = bare(a);
    assert!(matches!(ordinary_grants(&HERE, &["prog", "--grant", "data=a/./b"], &mut spec, a), Err(OrdinaryError::Argument { .. })));
    // A spec read back needs an absolute start folder (`bare`'s is `.`, which the reader refuses).
    let mut spec = RunSpec::defaults(blaze_own::Vec::new_in(a), blaze_own::Vec::new_in(a), blaze_own::String::from_str_in("/start", a), a);
    assert_eq!(ordinary_grants(&HERE, &["prog"], &mut spec, a), Ok(0));
    // `RunSpec<A>`'s derived `PartialEq` needs `A: PartialEq`, which a launch allocator is not.
    let written = spec.write(a);
    let back = RunSpec::read(written.as_bytes(), a).expect("a `.` folder reads back");
    assert_eq!(back.write(a).as_str(), written.as_str(), "the spec reads back equal");
}

/// Through the launcher's decision: an ordinary start reads `--grant` and the defaults, and
/// says how many arguments it took; with a run spec on file descriptor 3 neither is read (the
/// spec is the whole of what is granted). `Dir`, `Env` and `Net` are named so the parameters'
/// kinds stay the types'.
/// Expected: `test grant::defaults::tests::the_ordinary_start_takes_its_grants_before_main ... ok`.
#[test]
fn the_ordinary_start_takes_its_grants_before_main() {
    let _types: Option<(Dir<ReadOnly>, Env, Net)> = None;
    let a = alloc();
    match prepare(&DEFAULTS, None, || bare(a), &["prog", "--grant", "data=f", "rest"], a) {
        Prepared::Run { spec, taken, .. } => {
            assert_eq!(taken, 2);
            assert_eq!(granted(spec.get(), "data"), Some(("dir", "f".into())));
            assert_eq!(granted(spec.get(), "env"), Some(("env", "HOME,PATH".into())));
        }
        Prepared::Manifest(_) | Prepared::Refuse(_) => panic!("an ordinary start with good grants runs"),
    }
    match prepare(&BAD_DEFAULT, None, || bare(a), &["prog"], a) {
        Prepared::Refuse(said) => assert!(said.as_str().contains("#[bstd::main(..)]"), "{}", said.as_str()),
        Prepared::Run { .. } | Prepared::Manifest(_) => panic!("a default that does not read is refused"),
    }
}
