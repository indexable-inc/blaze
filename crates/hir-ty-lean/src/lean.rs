//! The Lean 4 this crate writes: terms, declarations and modules, and their printer.
//!
//! The printer is a function of the tree alone (no maps, no clock), so the same module prints
//! to the same bytes (`deterministic-output`). It prints only forms whose reading does not
//! depend on a user's notation: applications of named functions, `let`, `do`, `if`, `match`,
//! structure updates, tuples, and the few infix operators of [`Infix`], each operand in
//! parentheses unless it is an atom or an application. A name is escaped component by
//! component (`«at»`) when it is a Lean keyword or not a plain identifier.

/// A term, a type or a proposition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Term {
    /// A name, local or global (`x`, `Blaze.Scalar.add`): escaped when printed.
    Name(String),
    /// Text printed as it is: a numeral, `()`, `True`, `.U64`.
    Atom(String),
    App(Box<Term>, Vec<Term>),
    /// `e.field`; a tuple's `e.1`, `e.2`.
    Proj(Box<Term>, String),
    Tuple(Vec<Term>),
    /// `(e : T)`.
    Typed(Box<Term>, Box<Term>),
    /// `{ e with f := v, .. }`.
    Update(Box<Term>, Vec<(String, Term)>),
    /// `{ f := v, .. }`.
    Structure(Vec<(String, Term)>),
    /// `!e` on `Bool`.
    BoolNot(Box<Term>),
    /// `¬ e` on `Prop`.
    Not(Box<Term>),
    Infix(Infix, Box<Term>, Box<Term>),
    If(Box<Term>, Box<Term>, Box<Term>),
    Match(Box<Term>, Vec<(Pat, Term)>),
    /// `let p := v` then the body.
    Let(Pat, Box<Term>, Box<Term>),
    Do(Vec<DoElem>),
    Forall(Vec<Binder>, Box<Term>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Infix {
    And,
    Or,
    Implies,
    Eq,
    Ne,
    Lt,
    Le,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    BoolAnd,
    BoolOr,
    BoolEq,
    BoolNe,
    /// The product type `A × B`.
    Prod,
}

impl Infix {
    fn symbol(self) -> &'static str {
        match self {
            Infix::And => "∧",
            Infix::Or => "∨",
            Infix::Implies => "→",
            Infix::Eq => "=",
            Infix::Ne => "≠",
            Infix::Lt => "<",
            Infix::Le => "≤",
            Infix::Add => "+",
            Infix::Sub => "-",
            Infix::Mul => "*",
            Infix::Div => "/",
            Infix::Mod => "%",
            Infix::BoolAnd => "&&",
            Infix::BoolOr => "||",
            Infix::BoolEq => "==",
            Infix::BoolNe => "!=",
            Infix::Prod => "×",
        }
    }

    /// How tightly Lean binds it.
    fn binds(self) -> u8 {
        match self {
            Infix::Implies => 25,
            Infix::Or | Infix::BoolOr => 30,
            Infix::And | Infix::BoolAnd | Infix::Prod => 35,
            Infix::Eq | Infix::Ne | Infix::Lt | Infix::Le | Infix::BoolEq | Infix::BoolNe => 50,
            Infix::Add | Infix::Sub => 65,
            Infix::Mul | Infix::Div | Infix::Mod => 70,
        }
    }

    /// The side it associates to: `Some(true)` the right (`a ∧ (b ∧ c)` is `a ∧ b ∧ c`),
    /// `Some(false)` the left, `None` for a comparison.
    fn associates(self) -> Option<bool> {
        match self {
            Infix::And | Infix::Or | Infix::Implies | Infix::Prod => Some(true),
            Infix::Add
            | Infix::Sub
            | Infix::Mul
            | Infix::Div
            | Infix::Mod
            | Infix::BoolAnd
            | Infix::BoolOr => Some(false),
            Infix::Eq | Infix::Ne | Infix::Lt | Infix::Le | Infix::BoolEq | Infix::BoolNe => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DoElem {
    /// `let p ← e`.
    Bind(Pat, Term),
    /// `let p := e`.
    Let(Pat, Term),
    Term(Term),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pat {
    Name(String),
    Wild,
    Tuple(Vec<Pat>),
    /// A constructor applied to patterns: `Result.ok r`, `.Some x`.
    Ctor(String, Vec<Pat>),
    /// Text printed as it is: `0`, `fuel + 1`.
    Atom(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Binder {
    pub name: String,
    pub ty: Term,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decl {
    Structure { name: String, fields: Vec<Binder> },
    /// Each constructor's name and fields.
    Inductive { name: String, constructors: Vec<(String, Vec<Binder>)> },
    Def { name: String, params: Vec<Binder>, ty: Term, body: Term },
    /// `theorem name : statement := proof`, both by name.
    Theorem { name: String, statement: String, proof: String },
    /// `#print axioms name`.
    PrintAxioms(String),
}

/// A Lean file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Module {
    /// The comment at its head, one line each.
    pub about: Vec<String>,
    pub imports: Vec<String>,
    pub decls: Vec<Decl>,
}

pub fn name(text: impl Into<String>) -> Term {
    Term::Name(text.into())
}

pub fn atom(text: impl Into<String>) -> Term {
    Term::Atom(text.into())
}

pub fn app(function: &str, args: Vec<Term>) -> Term {
    if args.is_empty() { name(function) } else { Term::App(Box::new(name(function)), args) }
}

pub fn infix(op: Infix, left: Term, right: Term) -> Term {
    Term::Infix(op, Box::new(left), Box::new(right))
}

/// Lean's keywords and the command and term words a name must not be read as.
const KEYWORDS: &[&str] = &[
    "abbrev", "at", "attribute", "axiom", "break", "by", "calc", "catch", "class", "continue",
    "deriving", "def", "do", "else", "end", "example", "export", "extends", "finally", "for",
    "forall", "from", "fun", "have", "if", "import", "in", "include", "inductive", "infix",
    "infixl", "infixr", "instance", "let", "local", "macro", "match", "mut", "mutual",
    "namespace", "nomatch", "nofun", "noncomputable", "notation", "omit", "opaque", "open",
    "partial", "postfix", "prefix", "private", "protected", "public", "rec", "return", "scoped",
    "section", "set_option", "show", "structure", "suffices", "syntax", "then", "theorem",
    "try", "unless", "universe", "unsafe", "using", "variable", "where", "while", "with",
    "Prop", "Sort", "Type",
];

/// `text` as one component of a Lean name.
fn component(text: &str) -> String {
    let mut chars = text.chars();
    let plain = match chars.next() {
        Some(first) => {
            (first.is_ascii_alphabetic() || first == '_')
                && chars.all(|it| it.is_ascii_alphanumeric() || it == '_' || it == '\'')
                && text != "_"
        }
        None => false,
    };
    if plain && !KEYWORDS.contains(&text) {
        return text.to_owned();
    }
    // `»` cannot be written inside `«»`; a Rust name never has one.
    format!("«{}»", text.replace('»', "_"))
}

/// A possibly dotted name, each component escaped.
pub fn ident(text: &str) -> String {
    text.split('.').map(component).collect::<Vec<_>>().join(".")
}

const WIDTH: usize = 100;

fn pat(pattern: &Pat) -> String {
    match pattern {
        Pat::Name(text) => ident(text),
        Pat::Wild => "_".to_owned(),
        Pat::Tuple(items) => match items.as_slice() {
            [] => "()".to_owned(),
            [one] => pat(one),
            _ => format!("({})", items.iter().map(pat).collect::<Vec<_>>().join(", ")),
        },
        Pat::Ctor(ctor, args) => {
            let head = match ctor.strip_prefix('.') {
                Some(rest) => format!(".{}", ident(rest)),
                None => ident(ctor),
            };
            args.iter().fold(head, |text, arg| {
                let arg = match arg {
                    Pat::Ctor(_, inner) if !inner.is_empty() => format!("({})", pat(arg)),
                    _ => pat(arg),
                };
                format!("{text} {arg}")
            })
        }
        Pat::Atom(text) => text.clone(),
    }
}

/// Whether `term` prints as one token or a bracketed group: usable as an argument as it is.
fn is_atomic(term: &Term) -> bool {
    match term {
        Term::Name(_) | Term::Tuple(_) | Term::Typed(..) | Term::Update(..) | Term::Structure(_) => {
            true
        }
        Term::Atom(text) => !text.starts_with('-') && !text.contains(' '),
        // `(f x).field` is one group as it prints.
        Term::Proj(..) => true,
        _ => false,
    }
}

/// `term` as an argument of an application or the base of a projection.
fn argument(term: &Term) -> String {
    if is_atomic(term) { inline(term) } else { format!("({})", inline(term)) }
}

/// `term` as an operand of `op`, on the right when `right`: in parentheses unless Lean reads
/// it the same without (an application, or an operator that binds tighter, or the same
/// operator on the side it associates to).
fn operand(term: &Term, op: Infix, right: bool) -> String {
    match term {
        Term::App(..) => inline(term),
        Term::Infix(inner, ..)
            if inner.binds() > op.binds()
                || (*inner == op && op.associates() == Some(right)) =>
        {
            inline(term)
        }
        _ => argument(term),
    }
}

fn binders(all: &[Binder]) -> String {
    all.iter().map(|it| format!("({} : {})", ident(&it.name), inline(&it.ty))).collect::<Vec<_>>().join(" ")
}

fn fields(all: &[(String, Term)]) -> String {
    all.iter().map(|(field, value)| format!("{} := {}", ident(field), inline(value))).collect::<Vec<_>>().join(", ")
}

/// `term` on one line. Every term has such a form (`let` and `do` with `;`), which
/// [`lines`] uses where it fits.
pub fn inline(term: &Term) -> String {
    match term {
        Term::Name(text) => ident(text),
        Term::Atom(text) => text.clone(),
        Term::App(function, args) => args
            .iter()
            .fold(argument(function), |text, arg| format!("{text} {}", argument(arg))),
        Term::Proj(base, field) => {
            // A tuple's `.1` and `.2` are not names.
            let field = if field.chars().all(|it| it.is_ascii_digit()) {
                field.clone()
            } else {
                ident(field)
            };
            format!("{}.{field}", argument(base))
        }
        Term::Tuple(items) => match items.as_slice() {
            [] => "()".to_owned(),
            [one] => argument(one),
            _ => format!("({})", items.iter().map(inline).collect::<Vec<_>>().join(", ")),
        },
        Term::Typed(value, ty) => format!("({} : {})", inline(value), inline(ty)),
        Term::Update(base, changed) => format!("{{ {} with {} }}", inline(base), fields(changed)),
        Term::Structure(all) => format!("{{ {} }}", fields(all)),
        Term::BoolNot(inner) => format!("!{}", argument(inner)),
        Term::Not(inner) => format!("¬ {}", argument(inner)),
        Term::Infix(op, left, right) => {
            format!("{} {} {}", operand(left, *op, false), op.symbol(), operand(right, *op, true))
        }
        Term::If(cond, then, otherwise) => {
            format!("if {} then {} else {}", inline(cond), inline(then), inline(otherwise))
        }
        Term::Match(scrutinee, arms) => arms.iter().fold(
            format!("match {} with", inline(scrutinee)),
            |text, (pattern, body)| format!("{text} | {} => {}", pat(pattern), argument(body)),
        ),
        Term::Let(pattern, value, body) => {
            format!("let {} := {}; {}", pat(pattern), inline(value), inline(body))
        }
        Term::Do(elems) => {
            let elems: Vec<String> = elems
                .iter()
                .map(|elem| match elem {
                    DoElem::Bind(pattern, value) => {
                        format!("let {} ← {}", pat(pattern), inline(value))
                    }
                    DoElem::Let(pattern, value) => {
                        format!("let {} := {}", pat(pattern), inline(value))
                    }
                    DoElem::Term(term) => argument(term),
                })
                .collect();
            format!("do {}", elems.join("; "))
        }
        Term::Forall(bound, body) => format!("∀ {}, {}", binders(bound), inline(body)),
    }
}

/// Whether `term` is laid out over lines whatever its width.
fn is_block(term: &Term) -> bool {
    match term {
        Term::Let(..) | Term::Do(_) | Term::Match(..) => true,
        Term::If(_, then, otherwise) => is_block(then) || is_block(otherwise),
        Term::Forall(_, body) => is_block(body),
        Term::Infix(Infix::Implies, _, right) => is_block(right),
        _ => false,
    }
}

fn fits(text: &str, indent: usize) -> bool {
    indent + text.chars().count() <= WIDTH
}

struct Printer {
    out: Vec<String>,
}

impl Printer {
    fn line(&mut self, indent: usize, text: &str) {
        self.out.push(format!("{}{text}", " ".repeat(indent)));
    }

    /// `head` then `term`: on the same line where `term` fits there, else `term` below it,
    /// two columns in; a `do` block puts its `do` on the head's line.
    fn after(&mut self, indent: usize, head: &str, term: &Term) {
        if let Term::Do(elems) = term {
            self.line(indent, &format!("{head} do"));
            self.elems(indent + 2, elems);
            return;
        }
        let one = format!("{head} {}", inline(term));
        if !is_block(term) && fits(&one, indent) {
            self.line(indent, &one);
        } else {
            self.line(indent, head);
            self.term(indent + 2, term);
        }
    }

    fn elems(&mut self, indent: usize, elems: &[DoElem]) {
        for elem in elems {
            match elem {
                DoElem::Bind(pattern, value) => {
                    self.after(indent, &format!("let {} ←", pat(pattern)), value);
                }
                DoElem::Let(pattern, value) => {
                    self.after(indent, &format!("let {} :=", pat(pattern)), value);
                }
                DoElem::Term(term) => self.term(indent, term),
            }
        }
    }

    /// `term` from the start of a line at `indent`.
    fn term(&mut self, indent: usize, term: &Term) {
        match term {
            Term::Let(pattern, value, body) => {
                self.after(indent, &format!("let {} :=", pat(pattern)), value);
                self.term(indent, body);
            }
            Term::Do(elems) => {
                self.line(indent, "do");
                self.elems(indent + 2, elems);
            }
            Term::If(cond, then, otherwise) if is_block(term) || !fits(&inline(term), indent) => {
                self.after(indent, &format!("if {} then", inline(cond)), then);
                self.after(indent, "else", otherwise);
            }
            Term::Match(scrutinee, arms) => {
                self.line(indent, &format!("match {} with", inline(scrutinee)));
                for (pattern, body) in arms {
                    self.after(indent, &format!("| {} =>", pat(pattern)), body);
                }
            }
            Term::Forall(bound, body) if is_block(term) || !fits(&inline(term), indent) => {
                self.after(indent, &format!("∀ {},", binders(bound)), body);
            }
            Term::Infix(Infix::Implies, left, right)
                if is_block(term) || !fits(&inline(term), indent) =>
            {
                self.line(indent, &format!("{} →", operand(left, Infix::Implies, false)));
                self.term(indent + 2, right);
            }
            _ => self.line(indent, &inline(term)),
        }
    }

    fn decl(&mut self, decl: &Decl) {
        match decl {
            Decl::Structure { name, fields } => {
                self.line(0, &format!("structure {} where", ident(name)));
                for field in fields {
                    self.line(2, &format!("{} : {}", ident(&field.name), inline(&field.ty)));
                }
            }
            Decl::Inductive { name, constructors } => {
                self.line(0, &format!("inductive {} where", ident(name)));
                for (constructor, fields) in constructors {
                    let fields = binders(fields);
                    let space = if fields.is_empty() { "" } else { " " };
                    self.line(2, &format!("| {}{space}{fields}", ident(constructor)));
                }
            }
            Decl::Def { name, params, ty, body } => {
                let params = binders(params);
                let space = if params.is_empty() { "" } else { " " };
                let head = format!("def {}{space}{params} : {} :=", ident(name), inline(ty));
                self.after(0, &head, body);
            }
            Decl::Theorem { name, statement, proof } => self.line(
                0,
                &format!("theorem {} : {} := {}", ident(name), ident(statement), ident(proof)),
            ),
            Decl::PrintAxioms(name) => self.line(0, &format!("#print axioms {}", ident(name))),
        }
    }
}

impl Module {
    /// The file's text.
    pub fn print(&self) -> String {
        let mut printer = Printer { out: Vec::new() };
        for line in &self.about {
            printer.line(0, &format!("-- {line}"));
        }
        for import in &self.imports {
            printer.line(0, &format!("import {import}"));
        }
        printer.line(0, "");
        printer.line(0, "open Blaze");
        printer.line(0, "set_option linter.unusedVariables false");
        for decl in &self.decls {
            printer.line(0, "");
            printer.decl(decl);
        }
        let mut text = printer.out.join("\n");
        text.push('\n');
        text
    }
}

/// `term` laid out from column 0, for tests.
#[cfg(test)]
pub(crate) fn lines(term: &Term) -> String {
    let mut printer = Printer { out: Vec::new() };
    printer.term(0, term);
    printer.out.join("\n")
}
