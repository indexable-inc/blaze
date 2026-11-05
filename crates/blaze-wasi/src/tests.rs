use wasmtime::{Caller, Engine, Linker, Store};

use crate::{Embed, FUNCTIONS, Stop, System, W32, W64, Width, link};

/// Store data that links every call and serves none: the names are what is checked.
struct Nothing;

impl Embed for Nothing {
    fn with<R>(
        _: &mut Caller<'_, Self>,
        _: impl FnOnce(&mut [u8], &mut dyn System) -> Result<R, Stop>,
    ) -> wasmtime::Result<R> {
        Err(wasmtime::Error::msg("no guest"))
    }
}

/// The names `define` puts in a linker, sorted, each checked to be under `module`.
fn defined(module: &str, define: fn(&mut Linker<Nothing>) -> wasmtime::Result<()>) -> Vec<String> {
    let engine = Engine::default();
    let mut l = Linker::new(&engine);
    define(&mut l).unwrap();
    let mut store = Store::new(&engine, Nothing);
    let mut names: Vec<String> = l
        .iter(&mut store)
        .map(|(defined_in, name, _)| {
            assert_eq!(defined_in, module, "`{name}` is defined under `{defined_in}`");
            name.to_owned()
        })
        .collect();
    names.sort();
    names
}

fn sorted(names: &[&str]) -> Vec<String> {
    let mut names: Vec<String> = names.iter().map(|name| (*name).to_owned()).collect();
    names.sort();
    names
}

/// `FUNCTIONS` is what an embedder checks a sandboxed guest's imports against: it is exactly
/// what `link` defines at each width. (`seed::FUNCTIONS` against `seed::link`, which needs the
/// `seed` feature, is blaze-seed-run's test.)
#[test]
fn each_list_of_calls_is_exactly_what_its_link_defines() {
    assert_eq!(defined(W32::MODULE, link::<W32, Nothing>), sorted(FUNCTIONS));
    assert_eq!(defined(W64::MODULE, link::<W64, Nothing>), sorted(FUNCTIONS));
}
