//! The plan of a proof check, a sans-IO core (docs/v2/plan/design/proofs-for-sans-io.md,
//! section 5). From the theorems of one crate, as emitted, and the text of the crate's proof
//! files: the files to write and the runs of Lean to make ([`plan`]), each run keyed by
//! everything its verdict depends on; and from what each run said, each theorem's verdict
//! ([`conclude`]).
//!
//! It reads no file, starts no process and reads no clock. Its driver writes
//! [`Plan::writes`] under a directory blaze owns, then makes each [`Step`]'s run of Lean once
//! the steps it [`needs`](Step::needs) were accepted: in blaze's sandbox, with a deadline,
//! reading and writing only under that directory. The proof files are among the writes, from
//! the texts the plan was asked with: Lean reads exactly the bytes a step's key covers, never
//! the checkout, which may have moved on. A step whose [`key`](Step::key) the driver already
//! holds a verdict for is not run again: the key covers the Lean release, the module's text
//! and the key of every step it imports, so an unchanged tree runs Lean not at all.
//!
//! The first driver, deleted for this on 2026-11-13, ran Lean itself, one theorem at a time,
//! with no sandbox and no deadline: the library was compiled once per theorem, and a proof
//! file that proves two theorems imports two models, of which only the one being checked had
//! been written.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};

use base_db::ContentId;

use crate::{
    Emitted, LIBRARY, TRANSLATOR,
    lean::{Decl, Module, ident},
};

/// The Lean release blaze proves with: `lean/lean-toolchain` pins the same. Part of every
/// step's key, so of every [`ProofKey`]. A driver runs no other: a proof is only as good as
/// the checker it names.
pub const LEAN_VERSION: &str = "4.35.0";

/// The axioms a proof may rest on: Lean's three. `sorryAx` (a `sorry`) and
/// `Lean.ofReduceBool` (native evaluation: `native_decide`, `bv_decide`) are not among them.
/// blaze's library states no axiom of its own.
pub const ALLOWED_AXIOMS: &[&str] = &["propext", "Classical.choice", "Quot.sound"];

/// Which prover stands behind a proof.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Prover {
    /// The Lean kernel, at [`LEAN_VERSION`].
    Lean,
}

/// The identity of a Lean proof: how it was proved, and the key of the step that checked it
/// ([`Step::key`]), a blake3 over everything the verdict depends on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ProofKey {
    pub how: Prover,
    pub id: ContentId,
}

/// A clause of a function, for a caller that uses a proved fact
/// (docs/v2/plan/design/contracts.md, part 4).
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

/// What is asked: the theorems of one crate and its proof files.
#[derive(Clone, Copy, Debug)]
pub struct Ask<'a> {
    /// Each theorem's module as emitted: every one has a statement.
    pub theorems: &'a [Emitted],
    /// Every Lean file under the crate's `lean/`, by module (`BlazeCore.Busy`), with its text.
    pub proofs: &'a BTreeMap<String, String>,
}

/// Which of the two directories a module is in. Both are blaze's, and both are on Lean's
/// search path for every step.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Root {
    /// The library, the models and the checks: one directory for every crate, since a
    /// model's module is named by its crate and the library is everyone's.
    Out,
    /// The crate's proof files, a directory of its own for each crate: two crates may both
    /// have a `Lemmas.lean`.
    Proofs,
}

/// What a step's module is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    /// A module of blaze's Lean library ([`LIBRARY`]).
    Library,
    /// A generated model, or a type's module.
    Model,
    /// A file of the crate's `lean/`.
    Proof,
    /// The module that states a theorem by its proof and prints its axioms.
    Check,
}

impl Kind {
    fn said(self) -> &'static str {
        match self {
            Kind::Library => "a module of blaze's Lean library",
            Kind::Model => "a generated model",
            Kind::Proof => "the proof file",
            Kind::Check => "the check that the proof file proves the generated statement",
        }
    }
}

/// One run of Lean: `lean --root=. [-o <olean>] <file>` in the directory of its [`Root`],
/// with `LEAN_PATH` the two directories and nothing else of the environment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    pub kind: Kind,
    pub module: String,
    pub root: Root,
    /// Whether Lean keeps the module's compiled form, for the steps that import it.
    pub keep: bool,
    /// Over the Lean release, the module and its text and the key of each step it needs (and,
    /// for a check, the translator and the axioms allowed): everything its verdict is a
    /// function of.
    pub key: ContentId,
    /// The steps whose modules this one imports, each before it in [`Plan::steps`].
    pub needs: Vec<usize>,
}

impl Step {
    /// The module's file under its root.
    pub fn file(&self) -> String {
        format!("{}.lean", self.module.replace('.', "/"))
    }

    /// Where its compiled form goes under its root, when it is kept.
    pub fn olean(&self) -> Option<String> {
        self.keep.then(|| format!("{}.olean", self.module.replace('.', "/")))
    }
}

/// A file the driver writes before any step runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Write {
    pub root: Root,
    /// Under the root's directory, `/` between its parts.
    pub path: String,
    pub text: String,
}

/// A theorem, planned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Planned {
    /// The function, `krate::module::name`.
    pub function: String,
    /// `<name>.spec.statement`.
    pub statement: String,
    /// `<name>.spec.checked`: the theorem the check states, by the proof file's
    /// `<name>.spec.proof`.
    pub theorem: String,
    /// The module its proof is looked for in ([`proof_module`]).
    pub proof_module: String,
    /// Its model's file under [`Root::Out`].
    pub model: String,
    /// The step that compiles its model: run whether or not there is a proof file, so no
    /// answer is given of a theorem whose model Lean has not seen ([`wanted`]).
    pub model_step: usize,
    /// The step that checks it, or why no run of Lean can.
    pub check: Result<usize, Why>,
}

/// What to write and what to run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    /// By root, then path: the module of every step.
    pub writes: Vec<Write>,
    /// Each after the steps it needs: the library, the models, the proof files, the checks.
    pub steps: Vec<Step>,
    /// In the order asked.
    pub theorems: Vec<Planned>,
}

/// What blaze got wrong, which no proof file can mend: named, never worked around.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlanError {
    /// A theorem was emitted without a statement.
    NoStatement { function: String },
    /// Two modules of one name with two texts.
    TwoTexts { module: String },
    /// A library module or a model imports a module that is none of the library's, no model
    /// listed before it and none of Lean's own.
    Import { module: String, imported: String },
    /// Lean refused a module of blaze's library, or did not finish it.
    Library { module: String, output: String },
    /// The driver gave no outcome for a model or a check though every step it needs was
    /// accepted.
    NotRun { module: String },
}

impl fmt::Display for PlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PlanError::NoStatement { function } => write!(
                f,
                "`{function}` was given to the proof check with no statement \
                 (`<name>.spec.statement`): only a function with clauses or `#[blaze::theorem]` \
                 has one"
            ),
            PlanError::TwoTexts { module } => write!(
                f,
                "the Lean module `{module}` was emitted with two texts in one check: one name \
                 is one module (rule keys-cover-every-input)"
            ),
            PlanError::Import { module, imported } => write!(
                f,
                "the generated Lean module `{module}` imports `{imported}`, which is not a \
                 module of blaze's library, not a model emitted before it and not Lean's own: \
                 a bug in blaze's translator"
            ),
            PlanError::Library { module, output } => write!(
                f,
                "Lean {LEAN_VERSION} did not accept `{module}` of blaze's own Lean library, so \
                 no proof was checked:\n{output}"
            ),
            PlanError::NotRun { module } => write!(
                f,
                "`{module}` was not run though Lean accepted every module it imports: the \
                 proof check's driver runs every step a theorem needs, its model whether or not \
                 it has a proof file and its check where it has one (`plan::wanted`)"
            ),
        }
    }
}

impl std::error::Error for PlanError {}

/// Why a theorem is not proved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Why {
    /// The crate's `lean/` has no file for the proof's module.
    NoProofFile,
    /// The proof file `module` (the theorem's, or one it imports) imports `imported`, which is
    /// no file of the crate's `lean/`, no model of a theorem being checked, none of blaze's
    /// library and none of Lean's own.
    UnknownImport { module: String, imported: String },
    /// The proof file `module` imports itself, through others or directly.
    Cycle { module: String },
    /// Lean refused `module`: what it printed, whole (the goals left are in it).
    Rejected { kind: Kind, module: String, output: String },
    /// Lean did not finish `module` by its deadline.
    TimedOut { kind: Kind, module: String },
    /// The proof has a `sorry`.
    Sorry,
    /// The proof rests on an axiom that is not allowed.
    Axiom { axiom: String },
    /// Lean accepted the check and did not print the theorem's axioms.
    NoAxioms { output: String },
}

impl fmt::Display for Why {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Why::NoProofFile => write!(
                f,
                "there is no proof file: Lean checks the proof a person or an agent wrote, and \
                 searches for none"
            ),
            Why::UnknownImport { module, imported } => write!(
                f,
                "`{module}` imports `{imported}`, which is no file of the crate's `lean/`, not \
                 the model of a theorem being checked or of a function one calls, not a module \
                 of blaze's library (`Blaze.*`) and not Lean's own (`Init`, `Std`, `Lean`)"
            ),
            Why::Cycle { module } => write!(f, "`{module}` imports itself"),
            Why::Rejected { kind: Kind::Model, module, output } => write!(
                f,
                "Lean refused the generated model `{module}`: a bug in blaze's translator, not \
                 in the proof:\n{output}"
            ),
            Why::Rejected { kind, module, output } => {
                write!(f, "Lean refused {} (`{module}`):\n{output}", kind.said())
            }
            Why::TimedOut { kind, module } => write!(
                f,
                "Lean did not finish {} (`{module}`) by its deadline and was stopped (rule \
                 bounded-progress)",
                kind.said()
            ),
            Why::Sorry => write!(f, "the proof has a `sorry` (rule: proved means no `sorry`)"),
            Why::Axiom { axiom } => write!(
                f,
                "the proof rests on the axiom `{axiom}`, which is not one of {}",
                ALLOWED_AXIOMS.join(", ")
            ),
            Why::NoAxioms { output } => {
                write!(f, "Lean did not say which axioms the proof rests on:\n{output}")
            }
        }
    }
}

/// The module a function's proof is looked for in: one file per Rust module, each segment in
/// upper camel case, `Lib` for the crate's root. `blaze_graph::ledger::fnv1a_from` is proved
/// in `BlazeGraph.Ledger`.
pub fn proof_module(function: &str) -> String {
    let camel = |segment: &str| -> String {
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
    let path: Vec<&str> = function.split("::").collect();
    let modules = &path[..path.len().saturating_sub(1)];
    let mut segments: Vec<String> = modules.iter().map(|it| camel(it)).collect();
    if segments.len() <= 1 {
        segments.push("Lib".to_owned());
    }
    segments.join(".")
}

/// The modules `text` imports: Lean's header, the `import`s before its first command.
pub fn imports_of(text: &str) -> Vec<String> {
    let mut rest = text;
    let mut out = Vec::new();
    loop {
        rest = skip_blank(rest);
        let (word, after) = word(rest);
        match word {
            // What may stand before an `import` in a header.
            "module" | "prelude" | "public" | "private" | "meta" => rest = after,
            "import" => {
                let (mut name, mut after) = self::word(skip_blank(after));
                if matches!(name, "all" | "runtime") {
                    (name, after) = self::word(skip_blank(after));
                }
                if name.is_empty() {
                    return out;
                }
                out.push(name.replace(['«', '»'], ""));
                rest = after;
            }
            _ => return out,
        }
    }
}

/// `text` after its leading white space and comments. A documentation comment (`/--`, `/-!`)
/// belongs to a command and is left.
fn skip_blank(text: &str) -> &str {
    let mut rest = text;
    loop {
        rest = rest.trim_start();
        if rest.starts_with("--") {
            rest = rest.find('\n').map_or("", |at| &rest[at..]);
        } else if rest.starts_with("/-") && !rest.starts_with("/--") && !rest.starts_with("/-!") {
            // Block comments nest.
            let bytes = rest.as_bytes();
            let (mut at, mut depth) = (2, 1u32);
            while depth > 0 && at + 1 < bytes.len() {
                match (bytes[at], bytes[at + 1]) {
                    (b'/', b'-') => (at, depth) = (at + 2, depth + 1),
                    (b'-', b'/') => (at, depth) = (at + 2, depth - 1),
                    _ => at += 1,
                }
            }
            if depth > 0 {
                return "";
            }
            rest = &rest[at..];
        } else {
            return rest;
        }
    }
}

/// `text`'s first word (up to white space) and what follows it.
fn word(text: &str) -> (&str, &str) {
    let end = text.find(char::is_whitespace).unwrap_or(text.len());
    text.split_at(end)
}

/// Whether `module` is one the Lean release ships: its meaning is the release's.
fn of_lean(module: &str) -> bool {
    matches!(module.split('.').next(), Some("Init" | "Std" | "Lean" | "Lake"))
}

/// A safeguard of [`plan`] a test removes, to see that its oracle catches the removal.
#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Plant {
    None,
    /// A step's key leaves out the keys of the steps it needs.
    KeyWithoutNeeds,
    /// A theorem's model is compiled only when its check needs it (the plan before
    /// 2026-11-14): a theorem with no proof file is answered before Lean saw its model.
    NoModelCompile,
}

struct Planner<'a> {
    ask: Ask<'a>,
    writes: BTreeMap<(Root, String), String>,
    steps: Vec<Step>,
    /// The step of each module planned.
    by_module: BTreeMap<String, usize>,
    /// Each proof file asked for: its step, or why it has none.
    proofs: BTreeMap<String, Result<usize, Why>>,
    /// The proof files being planned, outermost first.
    visiting: Vec<String>,
    #[cfg(test)]
    plant: Plant,
}

impl Planner<'_> {
    fn key(&self, kind: Kind, module: &str, text: &str, needs: &[usize]) -> ContentId {
        let mut hasher = blaze_ids::Hasher::new("blaze v2 lean step 1");
        hasher.part(LEAN_VERSION.as_bytes()).part(&[kind as u8]).part(module.as_bytes());
        hasher.part(text.as_bytes());
        if kind == Kind::Check {
            hasher.part(TRANSLATOR.as_bytes());
            hasher.part(&(ALLOWED_AXIOMS.len() as u64).to_le_bytes());
            for axiom in ALLOWED_AXIOMS {
                hasher.part(axiom.as_bytes());
            }
        }
        #[cfg(test)]
        {
            if self.plant == Plant::KeyWithoutNeeds {
                return hasher.finish();
            }
        }
        hasher.part(&(needs.len() as u64).to_le_bytes());
        for need in needs {
            hasher.id(&self.steps[*need].key);
        }
        hasher.finish()
    }

    fn push(&mut self, kind: Kind, module: &str, text: &str, needs: Vec<usize>) -> usize {
        let key = self.key(kind, module, text, &needs);
        let root = if kind == Kind::Proof { Root::Proofs } else { Root::Out };
        self.steps.push(Step {
            kind,
            module: module.to_owned(),
            root,
            keep: kind != Kind::Check,
            key,
            needs,
        });
        self.writes.insert((root, format!("{}.lean", module.replace('.', "/"))), text.to_owned());
        self.by_module.insert(module.to_owned(), self.steps.len() - 1);
        self.steps.len() - 1
    }

    /// A generated module (the library's or a model): each import is a step planned before
    /// it or Lean's own.
    fn generated(&mut self, kind: Kind, module: &str, text: &str) -> Result<usize, PlanError> {
        if let Some(&step) = self.by_module.get(module) {
            return match self.writes.get(&(Root::Out, self.steps[step].file())) {
                Some(written) if written == text => Ok(step),
                _ => Err(PlanError::TwoTexts { module: module.to_owned() }),
            };
        }
        let mut needs = Vec::new();
        for imported in imports_of(text) {
            match self.by_module.get(&imported) {
                Some(&step) => needs.push(step),
                None if of_lean(&imported) => {}
                None => return Err(PlanError::Import { module: module.to_owned(), imported }),
            }
        }
        Ok(self.push(kind, module, text, needs))
    }

    /// The proof file `module`, after the proof files it imports.
    fn proof(&mut self, module: &str) -> Result<usize, Why> {
        if let Some(planned) = self.proofs.get(module) {
            // clone: a reason is given to each theorem it holds up
            return planned.clone();
        }
        if self.visiting.iter().any(|it| it == module) {
            return Err(Why::Cycle { module: module.to_owned() });
        }
        let proofs = self.ask.proofs;
        let Some(text) = proofs.get(module) else { return Err(Why::NoProofFile) };
        self.visiting.push(module.to_owned());
        let mut needs = Vec::new();
        let mut planned = Ok(());
        for imported in imports_of(text) {
            if let Some(&step) = self.by_module.get(&imported) {
                needs.push(step);
            } else if proofs.contains_key(&imported) {
                match self.proof(&imported) {
                    Ok(step) => needs.push(step),
                    Err(why) => {
                        planned = Err(why);
                        break;
                    }
                }
            } else if !of_lean(&imported) {
                planned = Err(Why::UnknownImport { module: module.to_owned(), imported });
                break;
            }
        }
        self.visiting.pop();
        let planned = planned.map(|()| self.push(Kind::Proof, module, text, needs));
        // clone: kept for the next theorem of the file
        self.proofs.insert(module.to_owned(), planned.clone());
        planned
    }
}

/// The plan of `ask`.
pub fn plan(ask: Ask<'_>) -> Result<Plan, PlanError> {
    plan_with(
        ask,
        #[cfg(test)]
        Plant::None,
    )
}

fn plan_with(ask: Ask<'_>, #[cfg(test)] plant: Plant) -> Result<Plan, PlanError> {
    let mut planner = Planner {
        ask,
        writes: BTreeMap::new(),
        steps: Vec::new(),
        by_module: BTreeMap::new(),
        proofs: BTreeMap::new(),
        visiting: Vec::new(),
        #[cfg(test)]
        plant,
    };
    for (module, text) in LIBRARY {
        planner.generated(Kind::Library, module, text)?;
    }
    // Every model before any proof file: a proof file may import the model of any theorem.
    let mut models = Vec::with_capacity(ask.theorems.len());
    for theorem in ask.theorems {
        if theorem.statement.is_none() {
            // clone: error path, once
            return Err(PlanError::NoStatement { function: theorem.function.clone() });
        }
        let mut model = 0;
        for emitted in theorem.imports.iter().chain([theorem]) {
            model = planner.generated(Kind::Model, &emitted.module, &emitted.text)?;
        }
        models.push(model);
    }
    let mut theorems = Vec::with_capacity(ask.theorems.len());
    for (emitted, model) in ask.theorems.iter().zip(models) {
        let statement = emitted.statement.as_deref().expect("checked above");
        // Not `<name>.spec`: with that name a proof file that lacks `<name>.spec.proof` is
        // told `proof` is no field of the theorem being stated, where Lean should say the
        // proof is not there.
        let theorem = format!("{}.spec.checked", emitted.name);
        let proof_module = proof_module(&emitted.function);
        let check = planner.proof(&proof_module).map(|proof| {
            let module = format!("BlazeCheck.{}", emitted.name);
            let text = check_module(emitted, statement, &theorem, &proof_module);
            planner.push(Kind::Check, &module, &text, vec![model, proof])
        });
        theorems.push(Planned {
            // clone: the plan owns its names
            function: emitted.function.clone(),
            statement: statement.to_owned(),
            theorem,
            proof_module,
            model: emitted.path(),
            model_step: model,
            check,
        });
    }
    let writes = planner
        .writes
        .into_iter()
        .map(|((root, path), text)| Write { root, path, text })
        .collect();
    Ok(Plan { writes, steps: planner.steps, theorems })
}

/// The check module of `emitted`: the theorem, by the proof its proof file gives, and its
/// axioms. The proof file names the statement and never restates it, so a statement that
/// changed is an elaboration error here, naming the theorem.
fn check_module(emitted: &Emitted, statement: &str, theorem: &str, proof_module: &str) -> String {
    Module {
        about: vec![format!(
            "Generated by blaze ({TRANSLATOR}): `{}`'s proof proves its generated statement.",
            emitted.function
        )],
        imports: vec![ident(&emitted.module), ident(proof_module)],
        decls: vec![
            Decl::Theorem {
                name: theorem.to_owned(),
                statement: statement.to_owned(),
                proof: format!("{}.spec.proof", emitted.name),
            },
            Decl::PrintAxioms(theorem.to_owned()),
        ],
    }
    .print()
}

/// What a step's run of Lean came to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ran {
    /// Lean accepted the module: what it printed.
    Accepted(String),
    /// Lean refused it: what it printed.
    Rejected(String),
    /// Lean did not end by the step's deadline and was stopped.
    TimedOut,
    /// A step it needs was not accepted, or no theorem with a check needs it: not run.
    NotRun,
}

/// A theorem's verdict.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    Proved {
        function: String,
        theorem: String,
        key: ProofKey,
        /// The axioms the proof rests on, as Lean lists them.
        axioms: Vec<String>,
        /// The model's file under [`Root::Out`].
        model: String,
    },
    Open {
        function: String,
        statement: String,
        proof_module: String,
        why: Why,
    },
    /// Lean refused the theorem's generated model, or a model it imports: blaze's translator
    /// emitted Lean that does not build, which no proof file can mend. Given whether or not
    /// there is a proof file.
    ModelBroken {
        function: String,
        /// The module Lean refused.
        module: String,
        /// Where Lean's first error is, `<file>:<line>:<column>` with the file under
        /// [`Root::Out`] as Lean printed it; the module's file where Lean printed no error
        /// with a place.
        at: String,
        /// Lean's first error after its place, as Lean printed it (`error: ...`,
        /// `error(<code>): ...`), with the lines that belong to it.
        error: String,
    },
}

/// Lean's first error in `output`: where (`<file>:<line>:<column>`) and the rest of it as Lean
/// printed it (`error(lean.redundantMatchAlt): Redundant alternative: ...`, the lines under it
/// up to the next message Lean printed). `None` where no line is an error with a place.
pub fn first_error(output: &str) -> Option<(String, String)> {
    // `<file>.lean:<line>:<column>: <severity>[(<code>)]: <message>`: where, severity, and the
    // rest after the place.
    fn located(line: &str) -> Option<(&str, &str, &str)> {
        let (file, rest) = line.split_once(".lean:")?;
        if file.is_empty() || file.contains(char::is_whitespace) {
            return None;
        }
        let (row, rest) = rest.split_once(':')?;
        let (column, rest) = rest.split_once(": ")?;
        if !row.bytes().all(|it| it.is_ascii_digit()) || !column.bytes().all(|it| it.is_ascii_digit()) {
            return None;
        }
        let severity = rest.split(':').next().unwrap_or("");
        let severity = severity.split('(').next().unwrap_or("");
        Some((&line[..file.len() + ".lean:".len() + row.len() + 1 + column.len()], severity, rest))
    }
    let mut lines = output.lines();
    let (at, message) = lines.find_map(|line| match located(line) {
        Some((at, "error", rest)) => Some((at, rest)),
        _ => None,
    })?;
    let mut error = message.to_owned();
    for line in lines.take_while(|line| located(line).is_none()) {
        error.push('\n');
        error.push_str(line);
    }
    Some((at.to_owned(), error.trim().to_owned()))
}

impl Verdict {
    /// What a [`Verdict::ModelBroken`] says, in one place for every client: the function, that
    /// its model does not build, whose bug that is, and Lean's first error.
    pub fn model_broken_said(function: &str, module: &str, at: &str, error: &str) -> String {
        format!(
            "the generated Lean model of `{function}` does not build: Lean {LEAN_VERSION} refused \
             `{module}`, a bug in blaze's translator (hir-ty-lean) that no proof file can fix:\n\
             {at}: {error}"
        )
    }
}

/// The steps a driver runs, in order: every theorem's model and its check, where it has one,
/// each after what it needs. A theorem with no proof file has its model compiled all the
/// same: its answer says the model builds. A library module none of them imports is not run.
pub fn wanted(plan: &Plan) -> Vec<usize> {
    wanted_with(
        plan,
        #[cfg(test)]
        Plant::None,
    )
}

fn wanted_with(plan: &Plan, #[cfg(test)] plant: Plant) -> Vec<usize> {
    let mut wanted = BTreeSet::new();
    #[cfg(test)]
    let models = plant != Plant::NoModelCompile;
    #[cfg(not(test))]
    let models = true;
    let mut stack: Vec<usize> = plan
        .theorems
        .iter()
        .flat_map(|it| it.check.as_ref().ok().copied().into_iter().chain(models.then_some(it.model_step)))
        .collect();
    while let Some(step) = stack.pop() {
        if wanted.insert(step) {
            stack.extend(plan.steps[step].needs.iter().copied());
        }
    }
    wanted.into_iter().collect()
}

/// The steps `from` needs, itself among them, in the plan's order.
fn closure(plan: &Plan, from: usize) -> BTreeSet<usize> {
    let mut needed = BTreeSet::new();
    let mut stack = vec![from];
    while let Some(step) = stack.pop() {
        if needed.insert(step) {
            stack.extend(plan.steps[step].needs.iter().copied());
        }
    }
    needed
}

/// Each theorem's verdict, from what each step of `plan` came to (`ran[i]` for step `i`).
/// First its model: Lean must have accepted it and every model it imports, or the answer is
/// [`Verdict::ModelBroken`] (or open for a deadline), proof file or none. Then proved means:
/// Lean accepted every module the check needs and the check, and the axioms it printed for
/// the theorem are among [`ALLOWED_AXIOMS`].
pub fn conclude(plan: &Plan, ran: &[Ran]) -> Result<Vec<Verdict>, PlanError> {
    conclude_with(
        plan,
        ran,
        #[cfg(test)]
        Plant::None,
    )
}

fn conclude_with(plan: &Plan, ran: &[Ran], #[cfg(test)] plant: Plant) -> Result<Vec<Verdict>, PlanError> {
    assert_eq!(ran.len(), plan.steps.len(), "one outcome for each step of the plan");
    #[cfg(test)]
    let models = plant != Plant::NoModelCompile;
    #[cfg(not(test))]
    let models = true;
    let mut verdicts = Vec::with_capacity(plan.theorems.len());
    'theorems: for theorem in &plan.theorems {
        let open = |why: Why| Verdict::Open {
            // clone: the verdict owns its names
            function: theorem.function.clone(),
            statement: theorem.statement.clone(),
            proof_module: theorem.proof_module.clone(),
            why,
        };
        // The model, before anything else is said of the theorem: no answer is given before
        // Lean has seen it (agents/annoyances/proofs-1113-8.json).
        if models {
            for at in closure(plan, theorem.model_step) {
                let step = &plan.steps[at];
                match &ran[at] {
                    Ran::Accepted(_) => {}
                    // clone: error path, once
                    Ran::Rejected(output) if step.kind == Kind::Library => {
                        return Err(PlanError::Library { module: step.module.clone(), output: output.clone() });
                    }
                    Ran::TimedOut if step.kind == Kind::Library => {
                        return Err(PlanError::Library {
                            module: step.module.clone(),
                            output: "Lean did not finish it by its deadline".to_owned(),
                        });
                    }
                    Ran::Rejected(output) => {
                        let (at, error) = first_error(output)
                            .unwrap_or_else(|| (step.file(), output.trim_end().to_owned()));
                        verdicts.push(Verdict::ModelBroken {
                            // clone: the verdict owns its names
                            function: theorem.function.clone(),
                            module: step.module.clone(),
                            at,
                            error,
                        });
                        continue 'theorems;
                    }
                    Ran::TimedOut => {
                        // clone: the verdict owns its names
                        verdicts.push(open(Why::TimedOut { kind: step.kind, module: step.module.clone() }));
                        continue 'theorems;
                    }
                    // Every step before it in the closure was accepted (a refusal ended the
                    // walk), so a model not run is the driver's bug, named: never an answer
                    // given without it.
                    // clone: error path, once
                    Ran::NotRun => return Err(PlanError::NotRun { module: step.module.clone() }),
                }
            }
        }
        let check = match &theorem.check {
            Ok(check) => *check,
            Err(why) => {
                // clone: the plan keeps its reason
                verdicts.push(open(why.clone()));
                continue;
            }
        };
        // The first step it needs, in the plan's order, that Lean did not accept.
        let needed = closure(plan, check);
        let mut held_up = None;
        for &at in &needed {
            let step = &plan.steps[at];
            let why = match &ran[at] {
                Ran::Accepted(_) => continue,
                Ran::NotRun => continue,
                // clone: Lean's words, given to each theorem the module holds up
                Ran::Rejected(output) if step.kind == Kind::Library => {
                    return Err(PlanError::Library {
                        module: step.module.clone(),
                        output: output.clone(),
                    });
                }
                Ran::TimedOut if step.kind == Kind::Library => {
                    return Err(PlanError::Library {
                        module: step.module.clone(),
                        output: "Lean did not finish it by its deadline".to_owned(),
                    });
                }
                Ran::Rejected(output) => Why::Rejected {
                    kind: step.kind,
                    module: step.module.clone(),
                    output: output.clone(),
                },
                Ran::TimedOut => Why::TimedOut { kind: step.kind, module: step.module.clone() },
            };
            held_up = Some(why);
            break;
        }
        if let Some(why) = held_up {
            verdicts.push(open(why));
            continue;
        }
        let Ran::Accepted(output) = &ran[check] else {
            // clone: error path, once
            return Err(PlanError::NotRun { module: plan.steps[check].module.clone() });
        };
        let Some(axioms) = printed_axioms(output, &theorem.theorem) else {
            // clone: Lean's words
            verdicts.push(open(Why::NoAxioms { output: output.clone() }));
            continue;
        };
        if axioms.iter().any(|it| it == "sorryAx") {
            verdicts.push(open(Why::Sorry));
            continue;
        }
        if let Some(axiom) = axioms.iter().find(|it| !ALLOWED_AXIOMS.contains(&it.as_str())) {
            // clone: the axiom's name
            verdicts.push(open(Why::Axiom { axiom: axiom.clone() }));
            continue;
        }
        verdicts.push(Verdict::Proved {
            // clone: the verdict owns its names
            function: theorem.function.clone(),
            theorem: theorem.theorem.clone(),
            key: ProofKey { how: Prover::Lean, id: plan.steps[check].key },
            axioms,
            model: theorem.model.clone(),
        });
    }
    Ok(verdicts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::translate::Shape;

    fn module(function: &str, statement: bool, imports: Vec<Emitted>, body: &str) -> Emitted {
        let name = function.replace("::", ".");
        let mut text = String::from("-- Generated by blaze: a test's model.\nimport Blaze\n");
        for import in &imports {
            text.push_str(&format!("import {}\n", import.module));
        }
        text.push_str(body);
        Emitted {
            function: function.to_owned(),
            module: format!("BlazeModel.{name}"),
            statement: statement.then(|| format!("{name}.spec.statement")),
            name,
            text,
            clauses: Vec::new(),
            shape: Shape { fuel: false, monadic: false, shared: false, mut_params: Vec::new(), returns_unit: false },
            imports,
        }
    }

    /// Two theorems of one Rust module, `k::m::a` (which calls `k::m::helper`) and `k::m::b`,
    /// proved in one file, `K.M`, which imports a file of lemmas, `K.Lemmas`.
    fn two(body_of_a: &str) -> (Vec<Emitted>, BTreeMap<String, String>) {
        let helper = module("k::m::helper", false, Vec::new(), "def k.m.helper := 1\n");
        let a = module("k::m::a", true, vec![helper], body_of_a);
        let b = module("k::m::b", true, Vec::new(), "def k.m.b := true\n");
        let proofs = BTreeMap::from([
            (
                "K.M".to_owned(),
                "-- Hand-written.\n/- a comment /- nested -/ -/\nimport Blaze.Machine\nimport \
                 BlazeModel.k.m.a\nimport BlazeModel.k.m.b\nimport K.Lemmas\nimport Std.Data.HashMap\n\n\
                 theorem k.m.a.spec.proof : k.m.a.spec.statement := sorry\n"
                    .to_owned(),
            ),
            ("K.Lemmas".to_owned(), "import Blaze\ntheorem one : 1 = 1 := rfl\n".to_owned()),
        ]);
        (vec![a, b], proofs)
    }

    fn planned(theorems: &[Emitted], proofs: &BTreeMap<String, String>, plant: Plant) -> Plan {
        plan_with(Ask { theorems, proofs }, plant).unwrap_or_else(|err| panic!("{err}"))
    }

    fn step<'a>(plan: &'a Plan, module: &str) -> (usize, &'a Step) {
        let at = plan.steps.iter().position(|it| it.module == module);
        let at = at.unwrap_or_else(|| panic!("no step for `{module}`"));
        (at, &plan.steps[at])
    }

    #[test]
    fn a_header_is_its_imports_and_nothing_after_them() {
        assert_eq!(
            imports_of("-- c\n/- b /- n -/ -/ import A.B\nimport «C».d\n\n/-- doc -/\ndef x := 1\nimport E\n"),
            ["A.B", "C.d"]
        );
        assert_eq!(imports_of("module\npublic import A\nimport all B\nmeta import C\n"), ["A", "B", "C"]);
        assert_eq!(imports_of("def x := 1\n"), [] as [&str; 0]);
        assert_eq!(imports_of("/- never closed"), [] as [&str; 0]);
    }

    #[test]
    fn a_proof_is_looked_for_by_its_rust_module() {
        assert_eq!(proof_module("blaze_graph::ledger::fnv1a_from"), "BlazeGraph.Ledger");
        assert_eq!(proof_module("blaze_core::busy::next_is_bounded"), "BlazeCore.Busy");
        assert_eq!(proof_module("theorems::bump_keeps_wf"), "Theorems.Lib");
    }

    /// The library once, every model before every proof file, one step for a proof file
    /// however many theorems it proves, and each step after what it needs.
    #[test]
    fn two_theorems_of_one_proof_file_share_its_step_and_the_library() {
        let (theorems, proofs) = two("def k.m.a := true\n");
        let plan = planned(&theorems, &proofs, Plant::None);
        for (at, step) in plan.steps.iter().enumerate() {
            assert!(step.needs.iter().all(|need| *need < at), "{} needs a later step", step.module);
        }
        let modules: Vec<&str> = plan.steps.iter().map(|it| it.module.as_str()).collect();
        let library: Vec<&str> = LIBRARY.iter().map(|it| it.0).collect();
        assert_eq!(modules[..library.len()], library);
        assert_eq!(
            modules[library.len()..],
            [
                "BlazeModel.k.m.helper",
                "BlazeModel.k.m.a",
                "BlazeModel.k.m.b",
                "K.Lemmas",
                "K.M",
                "BlazeCheck.k.m.a",
                "BlazeCheck.k.m.b",
            ]
        );
        let (proof, file) = step(&plan, "K.M");
        assert_eq!((file.kind, file.root, file.keep), (Kind::Proof, Root::Proofs, true));
        let needs: Vec<&str> = file.needs.iter().map(|it| plan.steps[*it].module.as_str()).collect();
        // `Std.Data.HashMap` is Lean's own: no step.
        assert_eq!(needs, ["Blaze.Machine", "BlazeModel.k.m.a", "BlazeModel.k.m.b", "K.Lemmas"]);
        for (theorem, model) in plan.theorems.iter().zip(["BlazeModel.k.m.a", "BlazeModel.k.m.b"]) {
            let check = &plan.steps[*theorem.check.as_ref().unwrap()];
            assert_eq!((check.kind, check.keep, check.olean()), (Kind::Check, false, None));
            assert_eq!(check.needs, [step(&plan, model).0, proof]);
        }
        // Every step's module is written, the proof files in their own directory: Lean
        // reads what the plan keyed.
        let written: Vec<(Root, &str)> = plan.writes.iter().map(|it| (it.root, it.path.as_str())).collect();
        assert_eq!(written.len(), plan.steps.len());
        for path in ["BlazeModel/k/m/helper.lean", "BlazeCheck/k/m/a.lean"] {
            assert!(written.contains(&(Root::Out, path)), "{path} is not written");
        }
        let proof_files: Vec<&str> =
            written.iter().filter(|it| it.0 == Root::Proofs).map(|it| it.1).collect();
        assert_eq!(proof_files, ["K/Lemmas.lean", "K/M.lean"]);
        // What is run is what a check needs: every model, proof file and check here, and of
        // the library what they import. `Blaze.Auto`, which no file of this crate imports, is
        // not compiled for it.
        let run: Vec<&str> = wanted(&plan).iter().map(|it| plan.steps[*it].module.as_str()).collect();
        for step in plan.steps.iter().filter(|it| it.kind != Kind::Library) {
            assert!(run.contains(&step.module.as_str()), "{} is not run", step.module);
        }
        assert!(run.contains(&"Blaze.Machine") && !run.contains(&"Blaze.Auto"), "{run:?}");
        // Planned again, it is the same plan (`deterministic-output`).
        assert_eq!(plan, planned(&theorems, &proofs, Plant::None));
    }

    /// The keys of the steps that differ between the plans of `two` with two bodies of `a`.
    fn moved(plant: Plant) -> Vec<String> {
        let (theorems, proofs) = two("def k.m.a := true\n");
        let (edited, _) = two("def k.m.a := false\n");
        let before = planned(&theorems, &proofs, plant);
        let after = planned(&edited, &proofs, plant);
        let mut moved = Vec::new();
        for (was, is) in before.steps.iter().zip(&after.steps) {
            assert_eq!(was.module, is.module);
            if was.key != is.key {
                // clone: a test's list of names
                moved.push(was.module.clone());
            }
        }
        moved
    }

    /// The invariant: a step's key moves exactly when its own module's text moves or the key
    /// of a step it needs does. After an edit of one model, the keys that move are that
    /// model's, the proof file's that imports it and the checks' that need either; the
    /// library's, the other models' and the lemma file's stay, so their runs are not made
    /// again. Its control is `the_oracle_catches_a_key_without_what_it_imports`.
    #[test]
    fn a_steps_key_moves_with_its_text_and_with_what_it_needs() {
        assert_eq!(moved(Plant::None), ["BlazeModel.k.m.a", "K.M", "BlazeCheck.k.m.a", "BlazeCheck.k.m.b"]);
    }

    #[test]
    fn the_oracle_catches_a_key_without_what_it_imports() {
        // With the plant only the edited module's own key moves: a proof checked against the
        // old model would be taken for a proof of the new one.
        assert_eq!(moved(Plant::KeyWithoutNeeds), ["BlazeModel.k.m.a"]);
    }

    #[test]
    fn what_a_proof_file_lacks_holds_up_only_its_theorems() {
        let (theorems, mut proofs) = two("def k.m.a := true\n");
        let other = module("k::other::c", true, Vec::new(), "def k.other.c := true\n");
        let theorems = [theorems, vec![other]].concat();
        // `k::other::c` has no proof file; `K.M` imports a module nobody has.
        proofs.insert("K.Lemmas".to_owned(), "import Mathlib.Tactic\n".to_owned());
        let plan = planned(&theorems, &proofs, Plant::None);
        let unknown = Why::UnknownImport {
            module: "K.Lemmas".to_owned(),
            imported: "Mathlib.Tactic".to_owned(),
        };
        // clone: compared twice
        assert_eq!(plan.theorems[0].check, Err(unknown.clone()));
        assert_eq!(plan.theorems[1].check, Err(unknown));
        assert_eq!(plan.theorems[2].check, Err(Why::NoProofFile));
        assert_eq!(plan.theorems[2].proof_module, "K.Other");
        // Nothing has a check, so what is run is the models (and the library they import):
        // no proof file and no check.
        let run = wanted(&plan);
        for theorem in &plan.theorems {
            assert!(run.contains(&theorem.model_step), "{} is not run", theorem.model);
        }
        assert!(run.iter().all(|it| matches!(plan.steps[*it].kind, Kind::Library | Kind::Model)), "{run:?}");
        proofs.insert("K.Lemmas".to_owned(), "import K.M\n".to_owned());
        let plan = planned(&theorems, &proofs, Plant::None);
        assert_eq!(plan.theorems[0].check, Err(Why::Cycle { module: "K.M".to_owned() }));
    }

    #[test]
    fn one_module_with_two_texts_is_refused_by_name() {
        let (mut theorems, proofs) = two("def k.m.a := true\n");
        let clash = module("k::m::helper", false, Vec::new(), "def k.m.helper := 2\n");
        theorems[1].imports.push(clash);
        let err = plan(Ask { theorems: &theorems, proofs: &proofs }).unwrap_err();
        assert_eq!(err, PlanError::TwoTexts { module: "BlazeModel.k.m.helper".to_owned() });
    }

    #[test]
    fn a_verdict_is_proved_only_on_the_allowed_axioms() {
        let (theorems, proofs) = two("def k.m.a := true\n");
        let plan = planned(&theorems, &proofs, Plant::None);
        let mut ran = vec![Ran::Accepted(String::new()); plan.steps.len()];
        let (a, _) = step(&plan, "BlazeCheck.k.m.a");
        let (b, _) = step(&plan, "BlazeCheck.k.m.b");
        ran[a] = Ran::Accepted("'k.m.a.spec.checked' depends on axioms: [propext, Quot.sound]\n".to_owned());
        ran[b] = Ran::Accepted("'k.m.b.spec.checked' depends on axioms: [propext, sorryAx]\n".to_owned());
        let verdicts = conclude(&plan, &ran).unwrap();
        let Verdict::Proved { key, axioms, model, .. } = &verdicts[0] else { panic!("{verdicts:?}") };
        assert_eq!((key.id, model.as_str()), (plan.steps[a].key, "BlazeModel/k/m/a.lean"));
        assert_eq!(axioms, &["propext", "Quot.sound"]);
        assert!(matches!(&verdicts[1], Verdict::Open { why: Why::Sorry, .. }), "{verdicts:?}");
        ran[b] = Ran::Accepted("'k.m.b.spec.checked' depends on axioms: [Lean.ofReduceBool]\n".to_owned());
        let verdicts = conclude(&plan, &ran).unwrap();
        assert!(matches!(&verdicts[1], Verdict::Open { why: Why::Axiom { axiom }, .. } if axiom == "Lean.ofReduceBool"));
        ran[b] = Ran::Accepted("no axioms printed".to_owned());
        assert!(matches!(&conclude(&plan, &ran).unwrap()[1], Verdict::Open { why: Why::NoAxioms { .. }, .. }));
    }

    #[test]
    fn a_refused_proof_file_leaves_each_of_its_theorems_open_with_leans_words() {
        let (theorems, proofs) = two("def k.m.a := true\n");
        let plan = planned(&theorems, &proofs, Plant::None);
        let mut ran = vec![Ran::Accepted(String::new()); plan.steps.len()];
        let (proof, _) = step(&plan, "K.M");
        ran[proof] = Ran::Rejected("K/M.lean:9:2: error: unsolved goals\n⊢ False".to_owned());
        for check in ["BlazeCheck.k.m.a", "BlazeCheck.k.m.b"] {
            ran[step(&plan, check).0] = Ran::NotRun;
        }
        let verdicts = conclude(&plan, &ran).unwrap();
        for verdict in &verdicts {
            let Verdict::Open { why: why @ Why::Rejected { kind: Kind::Proof, module, output }, .. } = verdict
            else {
                panic!("{verdict:?}")
            };
            assert_eq!(module, "K.M");
            assert!(output.contains("unsolved goals") && why.to_string().contains("⊢ False"));
        }
        // A model Lean refuses is said to be blaze's, a deadline is named, and a library
        // module refused is no theorem's verdict: the whole check stops on it.
        ran[proof] = Ran::Accepted(String::new());
        let (model, _) = step(&plan, "BlazeModel.k.m.a");
        ran[model] = Ran::TimedOut;
        let verdicts = conclude(&plan, &ran).unwrap();
        assert!(matches!(&verdicts[0], Verdict::Open { why: Why::TimedOut { kind: Kind::Model, .. }, .. }));
        ran[0] = Ran::Rejected("Blaze/Result.lean:1:0: error".to_owned());
        assert!(matches!(conclude(&plan, &ran), Err(PlanError::Library { module, .. }) if module == LIBRARY[0].0));
    }
    /// A stand-in for Lean over `wanted`, in order: a step whose needs were not all accepted
    /// is not run; `refused` is refused with `output`; a check prints its theorem's axioms.
    fn lean_over(plan: &Plan, wanted: &[usize], refused: Option<(&str, &str)>) -> Vec<Ran> {
        let mut ran = vec![Ran::NotRun; plan.steps.len()];
        for &at in wanted {
            let step = &plan.steps[at];
            if step.needs.iter().any(|need| !matches!(ran[*need], Ran::Accepted(_))) {
                continue;
            }
            ran[at] = match (refused, step.module.strip_prefix("BlazeCheck.")) {
                (Some((module, output)), _) if module == step.module => Ran::Rejected(output.to_owned()),
                (_, Some(name)) => Ran::Accepted(format!("'{name}.spec.checked' depends on axioms: [propext]\n")),
                (_, None) => Ran::Accepted(String::new()),
            };
        }
        ran
    }

    /// `two`'s theorems and `k::other::c`, which has no proof file.
    fn three() -> (Vec<Emitted>, BTreeMap<String, String>) {
        let (theorems, proofs) = two("def k.m.a := true\n");
        let c = module("k::other::c", true, Vec::new(), "def k.other.c : Nat := match (0 : Nat) with\n  | _ => 0\n  | 0 => 1\n");
        ([theorems, vec![c]].concat(), proofs)
    }

    /// What Lean printed of c's model, as the stand-in says it: an error, a line under it and
    /// a warning after.
    const REFUSED_C: &str = "BlazeModel/k/other/c.lean:5:4: error: Redundant alternative\n  | 0 => 1\n\
                             BlazeModel/k/other/c.lean:6:0: warning: unused\n";

    /// The verdicts of [`three`] with c's model refused, the safeguard `plant` removed.
    fn c_refused(plant: Plant) -> Result<Vec<Verdict>, PlanError> {
        let (theorems, proofs) = three();
        let plan = planned(&theorems, &proofs, plant);
        let ran = lean_over(&plan, &wanted_with(&plan, plant), Some(("BlazeModel.k.other.c", REFUSED_C)));
        conclude_with(&plan, &ran, plant)
    }

    /// The oracle of a theorem with no proof file whose model Lean refuses: it is answered
    /// [`Verdict::ModelBroken`], naming the function and the module, with Lean's first error
    /// and where it is, never `Open`.
    fn c_is_said_broken(verdicts: &[Verdict]) -> Result<(), String> {
        match &verdicts[2] {
            Verdict::ModelBroken { function, module, at, error } => {
                assert_eq!((function.as_str(), module.as_str()), ("k::other::c", "BlazeModel.k.other.c"));
                assert_eq!((at.as_str(), error.as_str()), ("BlazeModel/k/other/c.lean:5:4", "error: Redundant alternative\n  | 0 => 1"));
                Ok(())
            }
            other => Err(format!("a model Lean refuses was answered {other:?}")),
        }
    }

    /// The invariant: no theorem is answered before Lean has compiled its model. With no
    /// proof file, a model Lean refuses is [`Verdict::ModelBroken`] (blaze's bug, Lean's first
    /// error and its place) and a model it accepts is open for want of the file; with a proof
    /// file, a refused model is the same `ModelBroken`, and the theorems whose proof file
    /// imports it are open naming it, as before. Its control is
    /// `the_oracle_catches_an_answer_given_before_the_model_was_compiled`.
    #[test]
    fn a_plan_compiles_every_model_and_a_refused_one_is_blazes_bug() {
        let verdicts = c_refused(Plant::None).unwrap_or_else(|err| panic!("{err}"));
        c_is_said_broken(&verdicts).unwrap_or_else(|err| panic!("{err}"));
        let Verdict::ModelBroken { function, module, at, error } = &verdicts[2] else { unreachable!() };
        let said = Verdict::model_broken_said(function, module, at, error);
        for words in ["`k::other::c`", "does not build", "bug in blaze's translator", "no proof file can fix", "c.lean:5:4: error: Redundant alternative"] {
            assert!(said.contains(words), "{said}");
        }
        // The theorems with a proof file are unchanged by it.
        assert!(matches!(&verdicts[0], Verdict::Proved { .. }) && matches!(&verdicts[1], Verdict::Proved { .. }), "{verdicts:?}");
        // The same theorem with a model Lean accepts: open, there being no proof file.
        let (theorems, proofs) = three();
        let plan = planned(&theorems, &proofs, Plant::None);
        let ran = lean_over(&plan, &wanted(&plan), None);
        assert!(matches!(ran[plan.theorems[2].model_step], Ran::Accepted(_)), "its model was compiled");
        let verdicts = conclude(&plan, &ran).unwrap();
        assert!(matches!(&verdicts[2], Verdict::Open { why: Why::NoProofFile, .. }), "{verdicts:?}");
        // With a proof file: a's model refused is a's `ModelBroken`; b, whose proof file
        // imports a's model, is open naming it.
        let ran = lean_over(&plan, &wanted(&plan), Some(("BlazeModel.k.m.a", "BlazeModel/k/m/a.lean:3:0: error: unknown constant\n")));
        let verdicts = conclude(&plan, &ran).unwrap();
        assert!(matches!(&verdicts[0], Verdict::ModelBroken { function, at, .. } if function == "k::m::a" && at == "BlazeModel/k/m/a.lean:3:0"), "{verdicts:?}");
        assert!(matches!(&verdicts[1], Verdict::Open { why: Why::Rejected { kind: Kind::Model, module, .. }, .. } if module == "BlazeModel.k.m.a"), "{verdicts:?}");
        // Lean's first error, with the lines under it; none where no line has a place.
        assert_eq!(first_error("BlazeModel/W/lands.lean:37:6: error: Unknown constant `x`\n"), Some(("BlazeModel/W/lands.lean:37:6".to_owned(), "error: Unknown constant `x`".to_owned())));
        assert_eq!(first_error("a.lean:1:0: warning: w\nLean was ended by signal 9\n"), None);
        // Lean 4.35 names the kind of error in parentheses.
        let coded = "x.lean:2:0: warning(lean.unused): w\nM.lean:21:4: error(lean.redundantMatchAlt): Redundant alternative: Any\n  0\nwill match\n";
        assert_eq!(first_error(coded), Some(("M.lean:21:4".to_owned(), "error(lean.redundantMatchAlt): Redundant alternative: Any\n  0\nwill match".to_owned())));
        // A model the driver did not run is no answer either: named as the driver's bug.
        let (theorems, proofs) = three();
        let plan = planned(&theorems, &proofs, Plant::None);
        let ran = lean_over(&plan, &wanted_with(&plan, Plant::NoModelCompile), None);
        assert_eq!(conclude(&plan, &ran), Err(PlanError::NotRun { module: "BlazeModel.k.other.c".to_owned() }));
    }

    #[test]
    fn the_oracle_catches_an_answer_given_before_the_model_was_compiled() {
        // With the plant c's model is not compiled, and c is open for want of a proof file:
        // the answer of 2026-11-14 (agents/annoyances/proofs-1113-8.json).
        let verdicts = c_refused(Plant::NoModelCompile).unwrap_or_else(|err| panic!("{err}"));
        assert!(matches!(&verdicts[2], Verdict::Open { why: Why::NoProofFile, .. }), "{verdicts:?}");
        let caught = c_is_said_broken(&verdicts).unwrap_err();
        assert!(caught.contains("a model Lean refuses was answered Open"), "{caught}");
    }
}
