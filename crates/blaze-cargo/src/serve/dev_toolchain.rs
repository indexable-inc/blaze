//! `BLAZE_DEV_TOOLCHAIN_AS`: a developer's setting under which a rebuilt blaze keys what it
//! stores as another blaze did, so it reads that blaze's store.
//!
//! Everything blaze stores is keyed by the code identity of what made it: the code generator's
//! (`hir_ty_codegen::v2::CodegenCode`, in `Toolchain::id` and so in every template's key), the
//! front end's ([`super::FrontEndCode`]: images, check outcomes, bodies, goals, expansions) and
//! that of what makes a unit's record ([`super::RecordCode`]). Each is a hash of whole packages
//! (`blaze_codegen_code`), worked out by blaze-engine's build script under either builder (cargo
//! or blaze), so one edit to a package of the front end misses every image and every record,
//! and a blaze built by cargo has other identities than one built by blaze from the same tree
//! (the compiler's `-vV` is an input). Working on the compiler, that is the whole world
//! compiled again for each fix.
//!
//! The setting names, part by part, the identity to key as:
//! `BLAZE_DEV_TOOLCHAIN_AS=codegen=<64 hex>,front_end=<64 hex>,record=<64 hex>`, any of the
//! three, each once. A part left out keeps the identity this blaze was built with, so what
//! that part's code makes is made again: after a fix to the front end, assuming `codegen`
//! alone keeps every stored template and makes every image, check and record again, and
//! nothing stale is served. Assuming a part whose code changed serves what the old code made.
//! `blaze debug toolchain` prints a blaze's own three, and the value that assumes all of them.
//!
//! It is the process's, read once as the program starts ([`hand_over`]), never a request's:
//! a daemon keeps front ends, memos and images in memory that are keyed by no toolchain (one
//! process is one code generator), so two requests assuming two toolchains would share them.
//! It is one of the settings a daemon is started with ([`super::settings::STARTED_WITH`]): a
//! command that sets another value than its daemon's is refused, naming both.
//!
//! Loud and bounded, since what it serves may be stale:
//! - every build and check under it says one warning naming what is assumed ([`Dev::said`]);
//! - a build that may be shipped is refused ([`Dev::refuses`]): `--release`, `-r`, a
//!   `--profile` other than `dev` or `test`, `bench`, and a `--goal max`;
//! - the store check (`BLAZE_VERIFY_STORE`) is on at a rate of at least
//!   [`VERIFY_RATE`], so a stale template is found by sampling, as an error naming it.
//!
//! Nothing in this tree writes a remote cache (the daemon's store is opened with no remote, and
//! the front end's global cache is read-through only, `hir_def::deps::serve`); whatever comes
//! to write one asks [`dev`] first and refuses.

use blaze_ids::ContentId;

/// The variable.
pub const VAR: &str = "BLAZE_DEV_TOOLCHAIN_AS";

/// The least rate the store check runs at under the setting.
pub const VERIFY_RATE: f64 = 0.05;

/// The three code identities a process keys by.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Codes {
    pub codegen: ContentId,
    pub front_end: ContentId,
    pub record: ContentId,
}

impl Codes {
    /// As [`VAR`] takes them: `codegen=<hex>,front_end=<hex>,record=<hex>`.
    pub fn spelled(&self) -> String {
        // alloc: a line said once
        format!(
            "codegen={:x},front_end={:x},record={:x}",
            self.codegen, self.front_end, self.record
        )
    }
}

/// What [`VAR`] names: each part's identity to key as, `None` for a part left out.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Assumed {
    pub codegen: Option<ContentId>,
    pub front_end: Option<ContentId>,
    pub record: Option<ContentId>,
}

impl Assumed {
    /// The setting `value` spells, or why it is none, naming the part.
    pub fn parse(value: &str) -> Result<Assumed, String> {
        // alloc: an error, once
        let refused = |why: String| {
            format!(
                "{VAR}={value:?}: {why}; it takes `codegen=<64 hex>,front_end=<64 hex>,record=<64 \
                 hex>`, any of the three, each once (`blaze debug toolchain` prints a blaze's own)"
            )
        };
        let mut assumed = Assumed::default();
        for part in value.split(',') {
            let Some((name, hex)) = part.split_once('=') else {
                return Err(refused(format!("`{part}` is no `<part>=<hex>`")));
            };
            let slot = match name.trim() {
                "codegen" => &mut assumed.codegen,
                "front_end" => &mut assumed.front_end,
                "record" => &mut assumed.record,
                other => {
                    return Err(refused(format!(
                        "`{other}` is no part of the toolchain's identity (`codegen`, `front_end`, `record`)"
                    )));
                }
            };
            if slot.is_some() {
                return Err(refused(format!("`{}` is given twice", name.trim())));
            }
            let hex = hex.trim();
            let id = id_of_hex(hex)
                .ok_or_else(|| refused(format!("`{hex}` is not 64 hex digits")))?;
            *slot = Some(id);
        }
        Ok(assumed)
    }

    /// `built` with each part this names in its place.
    pub fn over(&self, built: Codes) -> Codes {
        Codes {
            codegen: self.codegen.unwrap_or(built.codegen),
            front_end: self.front_end.unwrap_or(built.front_end),
            record: self.record.unwrap_or(built.record),
        }
    }
}

/// The content ID 64 hex digits spell.
fn id_of_hex(hex: &str) -> Option<ContentId> {
    if hex.len() != 64 || !hex.is_ascii() {
        return None;
    }
    let mut bytes = [0u8; 32];
    for (byte, at) in bytes.iter_mut().zip((0..64).step_by(2)) {
        *byte = u8::from_str_radix(&hex[at..at + 2], 16).ok()?;
    }
    Some(ContentId(bytes))
}

/// A process under [`VAR`]: what it was built as, and what it keys as.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Dev {
    pub built: Codes,
    pub assumed: Assumed,
}

impl Dev {
    /// What the process keys by.
    pub fn keyed(&self) -> Codes {
        self.assumed.over(self.built)
    }

    /// The one line every build under it says.
    pub fn said(&self) -> String {
        let part = |name: &str, assumed: Option<ContentId>, built: ContentId| match assumed {
            // alloc: a line said once a build
            Some(as_) if as_ != built => format!("{name} {as_:x} (this blaze's is {built:x})"),
            Some(_) => format!("{name} its own"), // alloc: as above
            None => format!("{name} its own"),    // alloc: as above
        };
        // alloc: as above
        format!(
            "{VAR} is set: this blaze reads and writes the store as another toolchain ({}, {}, \
             {}), so what it reuses was made by other code and its outputs may be stale; the \
             store check runs at a rate of at least {VERIFY_RATE}",
            part("codegen", self.assumed.codegen, self.built.codegen),
            part("front_end", self.assumed.front_end, self.built.front_end),
            part("record", self.assumed.record, self.built.record),
        )
    }

    /// Why a request of cargo's `command` (`build`, `bench`, ...) with `args` is refused under
    /// the setting, naming the argument: one whose outputs may be shipped.
    pub fn refuses(&self, command: &str, args: &[String]) -> Option<String> {
        let shipped = shipped(command, args)?;
        // alloc: an error, once
        Some(format!(
            "{VAR} is set, and {shipped} builds what may be shipped: a blaze that keys as another \
             toolchain may reuse what other code made, so it builds only the `dev` and `test` \
             profiles; unset {VAR} (and stop the daemon started with it, `blaze stop`) to build this"
        ))
    }
}

/// What of a request of cargo's `command` with `args` (as typed, before `--`) says its outputs
/// may be shipped: `bench`, `--release` or `-r`, a `--profile` other than `dev` or `test`, a
/// `--goal max`. `None`: a dev or test build.
fn shipped(command: &str, args: &[String]) -> Option<String> {
    if command == "bench" {
        return Some("`bench`".to_owned()); // alloc: an error, once
    }
    let mut args = args.iter().map(String::as_str);
    while let Some(arg) = args.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) => (flag, Some(value)),
            None => (arg, None),
        };
        match flag {
            "--" => return None,
            "--release" | "-r" => return Some(format!("`{flag}`")), // alloc: as above
            "--profile" | "--goal" => {
                let value = inline.or_else(|| args.next())?;
                let dev = match flag {
                    "--profile" => matches!(value, "dev" | "test"),
                    _ => value != "max",
                };
                if !dev {
                    return Some(format!("`{flag} {value}`")); // alloc: as above
                }
            }
            _ => {}
        }
    }
    None
}

#[expect(
    clippy::disallowed_types,
    reason = "whether the process keys as another toolchain, handed over once by the program's main with its code identities"
)]
static DEV: std::sync::OnceLock<Option<Dev>> = std::sync::OnceLock::new();

/// Hands over the process's three code identities, `built` as its build script worked them
/// out, with `assumed`, the value of [`VAR`] in its environment when set: the identities it
/// keys by are `built`'s with each part `assumed` names in its place. Once, before any command
/// runs, in place of the three hand-overs it makes ([`hir_ty_codegen::v2::set_code`],
/// [`super::set_front_end_code`], [`super::set_record_code`]).
pub fn hand_over(built: Codes, assumed: Option<&str>) -> Result<(), String> {
    let dev = match assumed {
        Some(value) => Some(Dev { built, assumed: Assumed::parse(value)? }),
        None => None,
    };
    let keyed = dev.map_or(built, |it| it.keyed());
    hir_ty_codegen::v2::set_code(hir_ty_codegen::v2::CodegenCode(keyed.codegen))?;
    super::set_front_end_code(super::FrontEndCode(keyed.front_end))?;
    super::set_record_code(super::RecordCode(keyed.record))?;
    match DEV.get_or_init(|| dev) {
        it if *it == dev => Ok(()),
        // alloc: an error, once
        it => Err(format!(
            "the process keys as {it:?} already, and {dev:?} was handed over: one process is one toolchain"
        )),
    }
}

/// The setting the process runs under, when it does: `None` for a blaze that keys as itself,
/// and for a process whose identities were handed over one by one (a test's).
pub fn dev() -> Option<&'static Dev> {
    DEV.get().and_then(Option::as_ref)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(byte: u8) -> ContentId {
        ContentId([byte; 32])
    }

    const BUILT: Codes = Codes {
        codegen: ContentId([1; 32]),
        front_end: ContentId([2; 32]),
        record: ContentId([3; 32]),
    };

    /// What `blaze debug toolchain` prints is what the setting takes, and a part left out
    /// keeps the built identity.
    #[test]
    fn a_blazes_own_spelling_assumes_all_of_it_and_a_part_left_out_stays_built() {
        let other = Codes { codegen: id(0xa1), front_end: id(0xb2), record: id(0xc3) };
        let all = Assumed::parse(&other.spelled()).unwrap();
        assert_eq!(all.over(BUILT), other);

        let codegen = format!("codegen={:x}", id(0xa1));
        let one = Assumed::parse(&codegen).unwrap();
        assert_eq!(one, Assumed { codegen: Some(id(0xa1)), front_end: None, record: None });
        assert_eq!(one.over(BUILT), Codes { codegen: id(0xa1), ..BUILT });
    }

    /// Each value that is no setting is refused naming the variable, the value and the part.
    #[test]
    fn a_value_that_names_no_identity_is_refused_by_part() {
        let hex = format!("{:x}", id(7));
        for (value, why) in [
            (hex.clone(), "is no `<part>=<hex>`"),
            (format!("linker={hex}"), "`linker` is no part"),
            (format!("codegen={hex},codegen={hex}"), "`codegen` is given twice"),
            ("codegen=abc".to_owned(), "`abc` is not 64 hex digits"),
            (String::new(), "is no `<part>=<hex>`"),
        ] {
            let error = Assumed::parse(&value).unwrap_err();
            assert!(error.starts_with(VAR) && error.contains(why), "{value:?}: {error}");
        }
    }

    /// The line names each part assumed with this blaze's own beside it, and the check's rate.
    #[test]
    fn the_line_names_what_is_assumed() {
        let dev = Dev {
            built: BUILT,
            assumed: Assumed { codegen: Some(id(0xa1)), front_end: None, record: None },
        };
        let said = dev.said();
        assert!(said.starts_with("BLAZE_DEV_TOOLCHAIN_AS is set"), "{said}");
        assert!(said.contains(&format!("codegen {:x} (this blaze's is {:x})", id(0xa1), id(1))));
        assert!(said.contains("front_end its own") && said.contains("record its own"), "{said}");
        assert!(said.contains("may be stale") && said.contains("at least 0.05"), "{said}");
    }

    /// Builds that may be shipped are refused, naming the argument; dev and test builds are not.
    #[test]
    fn a_build_that_may_be_shipped_is_refused_by_its_argument() {
        let dev = Dev { built: BUILT, assumed: Assumed::default() };
        let args = |words: &[&str]| words.iter().map(|it| (*it).to_owned()).collect::<Vec<_>>();
        for (command, words, named) in [
            ("build", &["--release"][..], "`--release`"),
            ("build", &["-p", "x", "-r"][..], "`-r`"),
            ("build", &["--profile", "release"][..], "`--profile release`"),
            ("build", &["--profile=bench"][..], "`--profile bench`"),
            ("build", &["--goal", "max"][..], "`--goal max`"),
            ("bench", &[][..], "`bench`"),
        ] {
            let refused = dev.refuses(command, &args(words)).unwrap();
            assert!(refused.starts_with(VAR) && refused.contains(named), "{refused}");
        }
        for (command, words) in [
            ("build", &[][..]),
            ("test", &["--profile", "test"][..]),
            ("check", &["--profile=dev", "--goal", "dev"][..]),
            ("run", &["--", "--release"][..]),
        ] {
            assert_eq!(dev.refuses(command, &args(words)), None, "{command} {words:?}");
        }
    }
}
