//! The proof check: pipeline step 4, and the one `proved` interface ([`proved`], [`ProofKey`],
//! [`Clause`]) since the Z3 tier (`hir-ty-verus`) was retired on 2026-11-10.
//!
//! The pinned Lean is the executable `BLAZE_LEAN_PROVER` names, an absolute path: there is no
//! other place it is looked for (no `PATH`, no elan). Its `--version` must say
//! [`LEAN_VERSION`]: a proof is only as good as the checker it names. Lean is run with an
//! empty environment but `LEAN_PATH`, in a directory blaze owns (`Config::out`), where the
//! library, the models and the check module are written; the proof file is read where the
//! crate keeps it and never written.
//!
//! Lean is not run in blaze's sandbox yet, and it runs what the proof file says (a tactic may do IO), so the command that runs this for code blaze did not
//! write must put it in the sandbox first (`sandbox-everything`).

use std::{
    fmt,
    path::{Path, PathBuf},
    process::Command,
};

use base_db::{ContentId, Db};
use hir_def::FunctionId;
use hir_ty::next_solver::StoredGenericArgs;

use crate::{
    Emitted, LIBRARY, NotTranslated, TRANSLATOR,
    lean::{Decl, Module, ident},
};

/// The Lean release blaze proves with: `lean/lean-toolchain` pins the same. Part of every
/// [`ProofKey`].
pub const LEAN_VERSION: &str = "4.20.0";

/// The axioms a proof may rest on: Lean's three. `sorryAx` (a `sorry`) and
/// `Lean.ofReduceBool` (native evaluation: `native_decide`, `bv_decide`) are not among them.
/// v0's library states no axiom of its own.
pub const ALLOWED_AXIOMS: &[&str] = &["propext", "Classical.choice", "Quot.sound"];

/// Which prover stands behind a proof.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Prover {
    /// The Lean kernel, at [`LEAN_VERSION`].
    Lean,
}

/// The identity of a Lean proof: how it was proved, and a blake3 over everything the verdict
/// depends on ([`proof_key`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ProofKey {
    pub how: Prover,
    pub id: ContentId,
}

/// A crate's proof file for a function: `lean/<Module/Path>.lean` beside its `src/`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProofFile {
    /// The directory module names are relative to: the crate's `lean/`.
    pub root: PathBuf,
    /// The module, `BlazeGraph.Ledger`.
    pub module: String,
}

impl ProofFile {
    pub fn path(&self) -> PathBuf {
        self.root.join(format!("{}.lean", self.module.replace('.', "/")))
    }
}

/// How a function is checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// The `lean` executable. Its version must be [`LEAN_VERSION`].
    pub lean: PathBuf,
    /// A directory blaze owns, where generated modules and what Lean compiles them to go.
    pub out: PathBuf,
}

impl Config {
    /// Lean from `BLAZE_LEAN_PROVER`. There is no other place it is looked for.
    pub fn from_env(out: PathBuf) -> Result<Config, LeanError> {
        match std::env::var_os("BLAZE_LEAN_PROVER") {
            Some(lean) => {
                let lean = PathBuf::from(lean);
                if !lean.is_absolute() {
                    return Err(LeanError::NoLean(format!(
                        "`BLAZE_LEAN_PROVER` is `{}`, which is not an absolute path",
                        lean.display()
                    )));
                }
                Ok(Config { lean, out })
            }
            None => Err(LeanError::NoLean(format!(
                "`BLAZE_LEAN_PROVER` is not set: blaze checks proofs with Lean {LEAN_VERSION}, \
                 the `lean` executable of the toolchain `lean/lean-toolchain` pins"
            ))),
        }
    }
}

/// Why a function is not proved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LeanError {
    NotTranslated(NotTranslated),
    /// The function has no `requires` or `ensures`: there is nothing to prove.
    NoClauses { function: String },
    /// Lean is not set, not there, or not the pinned version.
    NoLean(String),
    /// The proof file is not there.
    NoProof { function: String, path: PathBuf },
    /// A file could not be written or read: the path and the error.
    Io { path: PathBuf, error: String },
    /// Lean refused a module. `step` says which: the library, a model, the proof file, or
    /// the check that the proof proves the generated statement (a proof of a statement that
    /// has since changed fails here).
    Rejected { function: String, step: String, module: String, output: String },
    /// The proof has a `sorry`.
    Sorry { function: String, theorem: String },
    /// The proof rests on an axiom that is not allowed.
    Axiom { function: String, theorem: String, axiom: String },
    /// Lean accepted the check module and did not print the theorem's axioms.
    NoAxioms { function: String, output: String },
}

impl fmt::Display for LeanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LeanError::NotTranslated(it) => write!(f, "{it}"),
            LeanError::NoClauses { function } => {
                write!(f, "`{function}` has no `requires` or `ensures` to prove")
            }
            LeanError::NoLean(why) => write!(f, "{why}"),
            LeanError::NoProof { function, path } => write!(
                f,
                "`{function}` has no proof: the proof file {} is not there",
                path.display()
            ),
            LeanError::Io { path, error } => write!(f, "{}: {error}", path.display()),
            LeanError::Rejected { function, step, module, output } => {
                write!(f, "`{function}` is not proved: Lean refused {step} (`{module}`):\n{output}")
            }
            LeanError::Sorry { function, theorem } => write!(
                f,
                "`{function}` is not proved: the proof of `{theorem}` has a `sorry` (rule: proved \
                 means no `sorry`)"
            ),
            LeanError::Axiom { function, theorem, axiom } => write!(
                f,
                "`{function}` is not proved: the proof of `{theorem}` rests on the axiom \
                 `{axiom}`, which is not one of {}",
                ALLOWED_AXIOMS.join(", ")
            ),
            LeanError::NoAxioms { function, output } => write!(
                f,
                "`{function}` is not proved: Lean did not say which axioms its proof rests \
                 on:\n{output}"
            ),
        }
    }
}

impl std::error::Error for LeanError {}

/// A function whose clauses Lean proved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Proved {
    /// The function, `krate::module::name`.
    pub function: String,
    /// The theorem, `<name>.spec`.
    pub theorem: String,
    pub key: ProofKey,
    /// The axioms the proof rests on, as Lean lists them.
    pub axioms: Vec<String>,
    /// The generated model's file.
    pub model: PathBuf,
}

/// The check module of `emitted`: the theorem, by the proof the proof file gives, and its
/// axioms.
fn check_module(emitted: &Emitted, statement: &str, proof: &ProofFile) -> (String, String) {
    let theorem = format!("{}.spec", emitted.name);
    let module = Module {
        about: vec![format!(
            "Generated by blaze ({TRANSLATOR}): `{}`'s proof proves its generated statement.",
            emitted.function
        )],
        imports: vec![ident(&emitted.module), ident(&proof.module)],
        decls: vec![
            Decl::Theorem {
                name: theorem.clone(),
                statement: statement.to_owned(),
                proof: format!("{}.spec.proof", emitted.name),
            },
            Decl::PrintAxioms(theorem.clone()),
        ],
    };
    (theorem, module.print())
}

/// The key of `emitted`'s proof by the proof file's `proof` text: over the Lean version, the
/// translator's version, the axioms allowed, the library, every model the proof can see, the
/// check module and the proof file. Lean's `maxHeartbeats` is its default, which the Lean
/// version fixes; a proof file that sets its own says so in its text.
pub fn proof_key(emitted: &Emitted, check: &str, proof: &str) -> ProofKey {
    let mut hasher = blaze_ids::Hasher::new("blaze v2 lean proof 1");
    hasher.part(LEAN_VERSION.as_bytes()).part(TRANSLATOR.as_bytes());
    hasher.part(&(ALLOWED_AXIOMS.len() as u64).to_le_bytes());
    for axiom in ALLOWED_AXIOMS {
        hasher.part(axiom.as_bytes());
    }
    hasher.part(&(LIBRARY.len() as u64).to_le_bytes());
    for (module, text) in LIBRARY {
        hasher.part(module.as_bytes()).part(text.as_bytes());
    }
    hasher.part(&(emitted.imports.len() as u64).to_le_bytes());
    for model in emitted.imports.iter().chain([emitted]) {
        hasher.part(model.module.as_bytes()).part(model.text.as_bytes());
    }
    hasher.part(check.as_bytes()).part(proof.as_bytes());
    ProofKey { how: Prover::Lean, id: hasher.finish() }
}

/// The axioms `#print axioms` listed in `output` for `theorem`: `None` when it did not.
pub(crate) fn printed_axioms(output: &str, theorem: &str) -> Option<Vec<String>> {
    let none = format!("'{theorem}' does not depend on any axioms");
    if output.contains(&none) {
        return Some(Vec::new());
    }
    let head = format!("'{theorem}' depends on axioms: [");
    let start = output.find(&head)? + head.len();
    let end = start + output[start..].find(']')?;
    Some(
        output[start..end]
            .split(',')
            .map(|it| it.trim().to_owned())
            .filter(|it| !it.is_empty())
            .collect(),
    )
}

/// Whether the axioms a proof rests on make it a proof.
pub(crate) fn judge(function: &str, theorem: &str, axioms: &[String]) -> Result<(), LeanError> {
    for axiom in axioms {
        if axiom == "sorryAx" {
            return Err(LeanError::Sorry {
                function: function.to_owned(),
                theorem: theorem.to_owned(),
            });
        }
        if !ALLOWED_AXIOMS.contains(&axiom.as_str()) {
            return Err(LeanError::Axiom {
                function: function.to_owned(),
                theorem: theorem.to_owned(),
                axiom: axiom.clone(),
            });
        }
    }
    Ok(())
}

fn write(path: &Path, text: &str) -> Result<(), LeanError> {
    let io = |error: std::io::Error| LeanError::Io { path: path.to_owned(), error: error.to_string() };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(io)?;
    }
    std::fs::write(path, text).map_err(io)
}

struct Lean<'a> {
    config: &'a Config,
    function: &'a str,
}

impl Lean<'_> {
    /// Runs Lean on `file`, a module under `root`, writing its `.olean` under the output
    /// directory when `keep`. What it printed, when it accepted the file.
    fn run(&self, step: &str, module: &str, root: &Path, keep: bool) -> Result<String, LeanError> {
        let relative = format!("{}.lean", module.replace('.', "/"));
        let mut command = Command::new(&self.config.lean);
        command.env_clear().env("LEAN_PATH", &self.config.out).current_dir(root).arg("--root=.");
        if keep {
            let olean = self.config.out.join(format!("{}.olean", module.replace('.', "/")));
            if let Some(dir) = olean.parent() {
                std::fs::create_dir_all(dir).map_err(|error| LeanError::Io {
                    path: dir.to_owned(),
                    error: error.to_string(),
                })?;
            }
            command.arg("-o").arg(olean);
        }
        command.arg(&relative);
        let output = command.output().map_err(|error| {
            LeanError::NoLean(format!("{} did not start: {error}", self.config.lean.display()))
        })?;
        let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&output.stderr));
        if output.status.success() {
            Ok(text)
        } else {
            Err(LeanError::Rejected {
                function: self.function.to_owned(),
                step: step.to_owned(),
                module: module.to_owned(),
                output: text,
            })
        }
    }
}

/// Checks that `proof` proves the statement of `func`'s clauses: emits the model, runs the
/// pinned Lean on the library, the models, the proof file and the check module, and holds the
/// theorem's axioms to [`ALLOWED_AXIOMS`].
pub fn check(
    db: &Db,
    func: FunctionId,
    proof: &ProofFile,
    config: &Config,
) -> Result<Proved, LeanError> {
    let emitted = crate::emit(db, func).map_err(LeanError::NotTranslated)?;
    check_emitted(&emitted, proof, config)
}

/// [`check`] for a module already emitted.
pub fn check_emitted(
    emitted: &Emitted,
    proof: &ProofFile,
    config: &Config,
) -> Result<Proved, LeanError> {
    let function = emitted.function.as_str();
    let Some(statement) = &emitted.statement else {
        return Err(LeanError::NoClauses { function: function.to_owned() });
    };
    let proof_path = proof.path();
    let proof_text = match std::fs::read_to_string(&proof_path) {
        Ok(text) => text,
        Err(_) => {
            return Err(LeanError::NoProof { function: function.to_owned(), path: proof_path });
        }
    };
    let (theorem, check) = check_module(emitted, statement, proof);
    let key = proof_key(emitted, &check, &proof_text);

    let version = Command::new(&config.lean).env_clear().arg("--version").output().map_err(
        |error| LeanError::NoLean(format!("{} did not start: {error}", config.lean.display())),
    )?;
    let version = String::from_utf8_lossy(&version.stdout).into_owned();
    if !version.contains(&format!("version {LEAN_VERSION}")) {
        return Err(LeanError::NoLean(format!(
            "{} is `{}`, not Lean {LEAN_VERSION}",
            config.lean.display(),
            version.trim()
        )));
    }

    let lean = Lean { config, function };
    for (module, text) in LIBRARY {
        write(&config.out.join(format!("{}.lean", module.replace('.', "/"))), text)?;
        lean.run("the library", module, &config.out, true)?;
    }
    for model in emitted.imports.iter().chain([emitted]) {
        write(&config.out.join(model.path()), &model.text)?;
        lean.run("a generated model", &model.module, &config.out, true)?;
    }
    lean.run("the proof file", &proof.module, &proof.root, true)?;
    let check_name = format!("BlazeCheck.{}", emitted.name);
    write(&config.out.join(format!("{}.lean", check_name.replace('.', "/"))), &check)?;
    let output = lean.run(
        "the check that the proof file proves the generated statement",
        &check_name,
        &config.out,
        false,
    )?;
    let Some(axioms) = printed_axioms(&output, &theorem) else {
        return Err(LeanError::NoAxioms { function: function.to_owned(), output });
    };
    judge(function, &theorem, &axioms)?;
    Ok(Proved {
        function: function.to_owned(),
        theorem,
        key,
        axioms,
        model: config.out.join(emitted.path()),
    })
}

/// A clause of a function, for [`proved`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Clause {
    /// The function's `requires`, holding at every call site.
    Requires,
    /// The function's `ensures`, given its `requires`.
    Ensures,
}

impl Clause {
    pub fn keyword(self) -> &'static str {
        match self {
            Clause::Requires => "requires",
            Clause::Ensures => "ensures",
        }
    }
}

/// A clause that is not proved, and why: never a pass, never another prover's answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotProved {
    /// The function, `krate::module::name`.
    pub function: String,
    pub clause: Clause,
    pub why: String,
}

impl fmt::Display for NotProved {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "`{}`'s `{}` is not proved: {}", self.function, self.clause.keyword(), self.why)
    }
}

impl std::error::Error for NotProved {}

/// Where a crate's proofs are and where blaze may write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Proofs {
    /// The crate's `lean/`, beside its `src/`.
    pub root: PathBuf,
    /// A directory blaze owns ([`Config::out`]).
    pub out: PathBuf,
}

impl Proofs {
    /// The proof file of the function at `path` (the crate, the modules, the name): one file
    /// per Rust module, each segment in upper camel case, `Lib` for the crate's root:
    /// `blaze_graph::ledger::fnv1a_from` is proved in `BlazeGraph.Ledger`.
    pub fn file_of(&self, path: &[String]) -> ProofFile {
        let camel = |segment: &String| -> String {
            segment
                .split('_')
                .map(|word| {
                    let mut chars = word.chars();
                    match chars.next() {
                        Some(first) => first.to_uppercase().chain(chars).collect::<String>(),
                        None => String::new(),
                    }
                })
                .collect()
        };
        let modules = &path[..path.len().saturating_sub(1)];
        let mut segments: Vec<String> = modules.iter().map(camel).collect();
        if segments.len() <= 1 {
            segments.push("Lib".to_owned());
        }
        ProofFile { root: self.root.clone(), module: segments.join(".") }
    }
}

/// Whether `clause` of `func<args>` is proved in Lean, and the key of that proof: the one
/// interface a caller that uses a proved fact reads (docs/v2/plan/design/contracts.md, part
/// 4). Only a proof the pinned Lean checked answers `Ok`; everything else says why not.
///
/// Lean v0 proves what a proof file proves. It runs no solver of its own, so a clause with no
/// proof in the crate's `lean/` is not proved, whatever it says. A `requires` is proved at
/// each call site, which v0 does not lower; a generic function is not translated.
pub fn proved(
    db: &Db,
    func: FunctionId,
    args: StoredGenericArgs,
    clause: Clause,
    proofs: &Proofs,
) -> Result<ProofKey, NotProved> {
    let path = crate::body::function_path(db, func);
    let not = |why: String| NotProved { function: path.join("::"), clause, why };
    if !args.as_ref().is_empty() {
        return Err(not("a generic function is not translated to Lean in v0".to_owned()));
    }
    if clause == Clause::Requires {
        return Err(not(
            "a `requires` is proved where the function is called, and v0 lowers no call site's              obligation"
                .to_owned(),
        ));
    }
    let config = Config::from_env(proofs.out.clone()).map_err(|err| not(err.to_string()))?;
    let proof = proofs.file_of(&path);
    match check(db, func, &proof, &config) {
        Ok(proved) => Ok(proved.key),
        Err(LeanError::NoProof { path, .. }) => Err(not(format!(
            "there is no proof file {}: Lean checks the proof a person or an agent wrote and              searches for none itself",
            path.display()
        ))),
        Err(err) => Err(not(err.to_string())),
    }
}
