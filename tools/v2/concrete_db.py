"""Moves a front-end crate's code from `dyn` databases to the one concrete `base_db::Db`.

Input: `{"root": "<workspace>", "crates": ["hir-def", ...]}` (inline, a path, or `-`),
`"write": true` to change the files (without it, only what would change is printed), and
`"paths": ["crates/hir-ty/src/traits.rs", ...]` to change only the files under those. For each
crate's `.rs` files it rewrites what is mechanical (docs/v2/plan/lanes/frontend/db.md, "The
migration"):

- `dyn HirDatabase`, `dyn SourceDatabase`, `dyn salsa::Database` (any path, `+ 'a` or not, behind
  `&`, `&mut`, `*const` or as a generic argument) become `Db`;
- `.as_dyn()` is dropped;
- `SourceDatabase::` becomes `Db::`, and `SourceDatabase` leaves `use` lists, as does
  `HirDatabase` from a file that no longer calls one of its methods;
- `TestDB::default()` becomes `TestDB::for_tests()` (`Db` has no `Default`);
- a file that now names `Db` and doesn't import it gets `use base_db::Db;` (`use crate::Db;` in
  base-db).

Then it prints, per file and line of the rewritten file, what it left for a person: `impl SourceDatabase for`, generic
bounds on the database traits, casts `as &dyn`, a `dyn` database in any other form, a crate whose
manifest lacks base-db, and a file with an item of its own named `Db`. It exits 1 when it left any.
"""

import re
import sys
from dataclasses import dataclass, field
from pathlib import Path

from typed_input import parse

SCRIPT = "concrete_db.py"

TRAITS = r"(?:HirDatabase|SourceDatabase|DefDatabase|ExpandDatabase|RootQueryDb|Database)"
# A path to one of the traits: `HirDatabase`, `hir::db::HirDatabase`, `salsa::Database`.
TRAIT_PATH = rf"(?:(?:crate|self|super|[a-z_][a-z0-9_]*)::)*{TRAITS}\b"
# `dyn HirDatabase`, `(dyn HirDatabase + 'static)`, behind `&`, `*const`, or as a generic argument.
# `+ 'a`, `+ Send`, `+ Sync` go with the trait: `Db` is `Send + Sync` and has no lifetime.
BOUNDS = r"(?:\s*\+\s*(?:'[a-z_][a-z0-9_]*|(?:(?:std|core)::marker::)?(?:Send|Sync)\b))*"
DYN = re.compile(rf"\(\s*dyn\s+{TRAIT_PATH}{BOUNDS}\s*\)|\bdyn\s+{TRAIT_PATH}{BOUNDS}")
# `x as &dyn HirDatabase`: a database cast, not a closure's (`as &dyn Fn(..)`).
CAST = re.compile(rf"\bas\s+&\s*(?:'[a-z_][a-z0-9_]*\s+)?(?:mut\s+)?\(?\s*dyn\s+{TRAIT_PATH}")
FRONT_END_PATH = TRAIT_PATH.replace("|Database)", ")")
DYN_FRONT_END = re.compile(rf"\(\s*dyn\s+{FRONT_END_PATH}{BOUNDS}\s*\)|\bdyn\s+{FRONT_END_PATH}{BOUNDS}")
AS_DYN = re.compile(r"\.as_dyn\(\)")
UFCS = re.compile(r"\b(?:base_db::)?SourceDatabase::")
# `Db` has no `Default`: a test's database is `Db::for_tests()`, named so.
TEST_DEFAULT = re.compile(r"\b(TestDB|TestDb)::default\(\)")
USE = re.compile(r"^(?P<indent>[ \t]*)(?:pub(?:\([^)]*\))?\s+)?use\s[^;]*;\n?", re.MULTILINE | re.DOTALL)
NAMES_DB = re.compile(r"(?<![\w:'])Db\b")
OWN_DB = re.compile(r"^[ \t]*(?:pub(?:\([^)]*\))?\s+)?(?:(?:struct|enum|type|trait)\s+Db\b|use\s[^;]*\bDb\b)", re.MULTILINE)

LEFT = (
    (re.compile(r"\bimpl(?:<[^>]*>)?\s+(?:[\w:]+::)?SourceDatabase\s+for\b"), "a database type of its own: `type TestDB = base_db::Db;` made by `Db::for_tests()`, its helpers a crate-private trait implemented for `Db`, `executed` as `Db::log_executed`"),
    (re.compile(r"\bimpl(?:<[^>]*>)?\s+(?:[\w:]+::)?HirDatabase\s+for\b"), "an impl of `HirDatabase`: hir-ty's one impl for `Db` is the only one"),
    (re.compile(r"impl_hir_database!"), "`impl_hir_database!`: gone; the MIR providers register at link time, so link `hir` or `hir-ty-mir` (`use hir as _;`)"),
    (re.compile(r"(?:(?<!:):(?!:)|\+)\s*\??\s*(?:[\w:]+::)?(?:HirDatabase|SourceDatabase)\b(?!::)"), "a generic bound on a database trait: take `&Db`"),
    (re.compile(r"\bdyn\s+(?:salsa::)?Database\b"), "salsa's `dyn Database`, kept because a `// dyn:` justifies it: right for API only on `impl dyn Database` (`memory_usage`); a query's database parameter is `&Db`"),
    (re.compile(r"(?<![\w:])Db\s*\+"), "`Db + ..`, from an earlier run that kept a `dyn` trait's bounds: `Db` alone (it is `Send + Sync`)"),
    (CAST, "a cast to a `dyn` database: drop it and pass the `&Db` (a test's `TestDB` is `Db`)"),
    (re.compile(rf"\bdyn\s+{TRAIT_PATH}"), "a `dyn` database in another form (a `Box`, a pointer, a macro): `Db`"),
    (re.compile(r"hir_database_is_dyn_compatible"), "the dyn-compatibility test: `dyn HirDatabase` must not compile now"),
)


@dataclass(frozen=True)
class Input:
    root: str
    crates: tuple[str, ...]
    write: bool = False
    # Only the files under these paths (relative to the root): one owner's files of a shared crate.
    paths: tuple[str, ...] = ()


@dataclass
class FileResult:
    rewrites: int = 0
    left: list[str] = field(default_factory=list)


def strip_from_uses(text: str, name: str = "SourceDatabase") -> tuple[str, int]:
    """`text` with `name` out of its `use` items, and how many it took out."""
    count = 0
    # The name with its path inside the item (`db::HirDatabase`), and what separated it.
    in_use = re.compile(rf"(?<![\w:])(?:\w+::)*{name}(?:\s+as\s+_)?\b[ \t]*,?[ \t]*(?:\n[ \t]*(?=\S))?")

    def one(m: re.Match[str]) -> str:
        nonlocal count
        item = m.group(0)
        if not re.search(rf"\b{name}\b", item) or f"{name}::" in item or re.search(r"^\s*pub", item):
            return item
        new, n = in_use.subn("", item)
        count += n
        for _ in range(3):
            new = re.sub(r"\{\s*\}", "{}", new)
            new = re.sub(r",[ \t]*\}", "}", new)
            new = re.sub(r"\{[ \t]*,[ \t]*", "{", new)
            # A group emptied by it: `db::{}` goes, with the comma after it.
            new = re.sub(r"(?<![\w:])(?:\w+::)+\{\}[ \t]*,?[ \t]*", "", new)
            new = re.sub(r"\n[ \t]*\n", "\n", new)
        # Nothing left to import (`use base_db::{};`, `use {};`, `use ;`): the item goes, line and all.
        if re.fullmatch(r"\s*use\s+(?:[\w:]*(?:::)?\{\})?\s*;\n?", new) or re.fullmatch(r"\s*use\s+[\w:]*::\s*;\n?", new):
            return ""
        return new

    return USE.sub(one, text), count


def add_db_import(text: str, crate: str) -> str:
    """`text` importing `Db`: into its top-level `use base_db::...` (`use crate::...` in base-db)
    if it has one, else as a line of its own before its first `use` of another crate."""
    root = "crate" if crate == "base-db" else "base_db"
    braced = re.compile(rf"^use {root}::\{{", re.MULTILINE)
    if m := braced.search(text):
        if text[m.end() :].startswith("\n"):
            indent = re.match(r"\n([ \t]*)", text[m.end() :]).group(1)
            return text[: m.end()] + f"\n{indent}Db," + text[m.end() :]
        return text[: m.end()] + "Db, " + text[m.end() :]
    single = re.compile(rf"^use {root}::(?P<name>[\w:]+(?: as \w+)?);", re.MULTILINE)
    if m := single.search(text):
        return text[: m.start()] + f"use {root}::{{Db, {m.group('name')}}};" + text[m.end() :]
    line = f"use {root}::Db;\n"
    lines = text.splitlines(keepends=True)
    uses = [i for i, it in enumerate(lines) if re.match(r"(?:pub(?:\([^)]*\))?\s+)?use ", it)]
    if not uses:
        at = 0
        while at < len(lines) and (lines[at].startswith("//!") or lines[at].startswith("#![") or not lines[at].strip()):
            at += 1
        lines.insert(at, line + "\n")
        return "".join(lines)
    std = re.compile(r"(?:pub(?:\([^)]*\))?\s+)?use (?:std|core|alloc)::")
    at = next((i for i in uses if not std.match(lines[i])), None)
    if at is None:
        # Only `std` imports: after the last of them, ending where its `;` is.
        at = uses[-1]
        while not lines[at].rstrip().endswith(";"):
            at += 1
        lines.insert(at + 1, "\n" + line)
    else:
        lines.insert(at, line)
    return "".join(lines)


def code_only(line: str) -> str:
    return line.split("//", 1)[0]


def code_only_text(text: str) -> str:
    return "\n".join(code_only(it) for it in text.splitlines())


def hir_database_methods(root: Path) -> frozenset[str]:
    """The method names of hir-ty's `HirDatabase` trait, which a file calls only with it imported."""
    text = (root / "crates/hir-ty/src/db.rs").read_text()
    start = text.index("pub trait HirDatabase")
    end = text.index("\n}\n", start)
    return frozenset(re.findall(r"^\s*fn\s+(\w+)", text[start:end], re.MULTILINE))


def calls_any(text: str, methods: frozenset[str]) -> bool:
    return any(m.group(1) in methods for m in re.finditer(r"\.(\w+)\s*(?:::<[^>]*>)?\(", text))


DB_CALL = re.compile(r"\b(?:self\.)?db\s*\.\s*(\w+)\s*(?:::<[^>]*>)?\(")


SALSA_METHODS = frozenset({
    "trigger_lru_eviction", "synthetic_write", "trigger_cancellation", "cancellation_token",
    "report_untracked_read", "ingredient_debug_name", "unwind_if_revision_cancelled", "attach",
})
SALSA_IMPORT = re.compile(r"(?m)^[ \t]*(?:pub(?:\([^)]*\))?\s+)?use\s[^;]*\bDatabase\b")


def DB_CALL_NAMES(text: str) -> set[str]:
    return {m.group(1) for m in DB_CALL.finditer(text)}


def salsa_database_path(crate: str, manifest: Path) -> str | None:
    """How `crate`'s files name `salsa::Database`: salsa itself, or base-db's re-export."""
    deps = manifest.read_text() if manifest.is_file() else ""
    if re.search(r"^salsa\s*[.=]", deps, re.MULTILINE):
        return "salsa::Database"
    if crate == "base-db" or re.search(r"^base-db\s*[.=]", deps, re.MULTILINE):
        return "crate::salsa::Database" if crate == "base-db" else "base_db::salsa::Database"
    return None


def hir_database_path(crate: str, manifest: Path) -> str | None:
    """How `crate`'s files name `HirDatabase`: its own `db` module, or the nearest dependency's."""
    if crate in ("hir-ty", "hir"):
        return "crate::db::HirDatabase"
    deps = manifest.read_text() if manifest.is_file() else ""
    for dep, path in (("hir-ty", "hir_ty"), ("hir", "hir")):
        if re.search(rf"^{dep}\s*[.=]", deps, re.MULTILINE):
            return f"{path}::db::HirDatabase"
    return None


def migrate(text: str, crate: str, methods: frozenset[str] = frozenset(), hir_db: str | None = None, salsa_db: str | None = None) -> tuple[str, FileResult]:
    result = FileResult()
    had_own_db = bool(OWN_DB.search(text))

    new_lines: list[str] = []
    prev = ""
    for line in text.splitlines(keepends=True):
        justified = "// dyn:" in line or "// dyn:" in prev
        prev = line
        if CAST.search(line):
            new_lines.append(line)
            continue
        # Only code: a comment that names `dyn Database` says something true of its own.
        code, sep, comment = line.partition("//")
        # salsa's own `dyn Database`, justified, is kept: some of salsa's API is only on
        # `impl dyn Database` (`memory_usage`). The front end's traits never are.
        dyn = DYN if not justified else DYN_FRONT_END
        code, n1 = dyn.subn("Db", code)
        code, n2 = AS_DYN.subn("", code)
        code, n3 = UFCS.subn("Db::", code)
        code, n4 = TEST_DEFAULT.subn(r"\1::for_tests()", code)
        result.rewrites += n1 + n2 + n3 + n4
        new_lines.append(code + sep + comment)
    new = "".join(new_lines)
    new, n = strip_from_uses(new)
    result.rewrites += n
    # `HirDatabase` named the type before; now a file imports it only to call its methods.
    rest = "\n".join(it for it in new.splitlines() if not it.lstrip().startswith(("use ", "pub use ")))
    if result.rewrites and methods and not re.search(r"\bHirDatabase\b", code_only_text(rest)) and not calls_any(new, methods):
        new, n = strip_from_uses(new, "HirDatabase")
        result.rewrites += n

    names_db = any(NAMES_DB.search(code_only(it)) for it in new.splitlines() if not it.lstrip().startswith("use "))
    imports_db = re.search(r"(?m)^[ \t]*(?:pub(?:\([^)]*\))?\s+)?use\s[^;]*\bDb\b", new) is not None
    # A file migrated by an earlier run names `Db` too; one generic over a `Db` of its own doesn't
    # mean `base_db::Db`.
    generic_db = re.search(r"[<,]\s*Db\s*:", new) is not None
    if names_db and not imports_db and not generic_db:
        if had_own_db:
            result.left.append("0: an item of its own named `Db`: the rewrite's `Db` would name it; rename one")
        else:
            new = add_db_import(new, crate)
    # On `&dyn HirDatabase` its methods resolve with the trait unimported; on `&Db` they need it
    # in scope.
    imports_trait = re.search(r"(?m)^[ \t]*(?:pub(?:\([^)]*\))?\s+)?use\s[^;]*\bHirDatabase\b", new)
    called = {m.group(1) for m in DB_CALL.finditer(code_only_text(new))} & methods
    if called and not imports_trait and not re.search(r"\btrait\s+HirDatabase\b", new):
        if hir_db is None:
            result.left.append(f"0: calls `HirDatabase`'s `{min(called)}` on `&Db` with no path to the trait from this crate: import it")
        else:
            first_use = re.search(r"(?m)^use\s", new)
            at = first_use.start() if first_use else 0
            new = new[:at] + f"use {hir_db};\n" + new[at:]
    # `salsa::Database`'s own methods too: a supertrait of the `dyn` trait, now a trait to import.
    if DB_CALL_NAMES(code_only_text(new)) & SALSA_METHODS and not SALSA_IMPORT.search(new) and salsa_db:
        first_use = re.search(r"(?m)^use\s", new)
        at = first_use.start() if first_use else 0
        new = new[:at] + f"use {salsa_db} as _;\n" + new[at:]
    for i, line in enumerate(new.splitlines(), 1):
        code = code_only(line)
        for pattern, why in LEFT:
            if pattern.search(code):
                result.left.append(f"{i}: {why}")
                break
    return new, result


def depends_on_base_db(manifest: Path) -> bool:
    return manifest.name == "Cargo.toml" and re.search(r"^base-db\s*[.=]", manifest.read_text(), re.MULTILINE) is not None


def main() -> None:
    match sys.argv[1:]:
        case [text]:
            cfg = parse(Input, text, SCRIPT)
        case _:
            print(f"usage: {SCRIPT} '{{\"root\": \".\", \"crates\": [\"hir-def\"]}}'", file=sys.stderr)
            sys.exit(2)
    root = Path(cfg.root)
    trait_root = root if (root / "crates/hir-ty/src/db.rs").is_file() else Path(__file__).resolve().parents[2]
    methods = hir_database_methods(trait_root)
    left_total = 0
    for crate in cfg.crates:
        base = root / "crates" / crate
        if not base.is_dir():
            print(f"{crate}: no such crate under {root / 'crates'}", file=sys.stderr)
            sys.exit(2)
        rewrites = 0
        left: list[str] = []
        hir_db = hir_database_path(crate, base / "Cargo.toml")
        salsa_db = salsa_database_path(crate, base / "Cargo.toml")
        for path in sorted(base.rglob("*.rs")):
            if "target" in path.parts:
                continue
            rel = path.relative_to(root)
            if cfg.paths and not any(rel.as_posix().startswith(it) for it in cfg.paths):
                continue
            text = path.read_text()
            new, result = migrate(text, crate, methods, hir_db, salsa_db)
            rewrites += result.rewrites
            left.extend(f"{rel}:{it}" for it in result.left)
            if cfg.write and new != text:
                path.write_text(new)
        if rewrites and crate != "base-db" and not depends_on_base_db(base / "Cargo.toml"):
            left.append(f"crates/{crate}/Cargo.toml: no `base-db` dependency for `base_db::Db`")
        for line in left:
            print(line)
        print(f"{rewrites:6} rewritten, {len(left):4} left  {crate}", file=sys.stderr)
        left_total += len(left)
    sys.exit(1 if left_total else 0)


if __name__ == "__main__":
    main()
