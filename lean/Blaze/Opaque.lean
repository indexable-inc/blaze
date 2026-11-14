import Blaze.Result

/-!
Opaque items: step 0 of docs/v2/plan/design/proofs-for-sans-io.md, section 3. A type or function
of the standard library that a property never looks inside is declared here, or in a generated
module of its own, with no definition: a theorem about code that merely carries or calls it holds
for every meaning of it.

- **An opaque type** is `opaque <name> : Type`. Lean's `opaque` needs the type it declares a
  constant of to be inhabited, which `Type` is (by `PUnit`); the constant's value is never
  unfolded by the kernel, so no proof can use what the type is: not that it is a subsingleton,
  not that it is inhabited, nothing of its equality. It adds no axiom: `#print axioms` of a
  theorem over values of it lists none of its own. (A structure with a private `Unit` field would
  make every value equal, which is a meaning, not opacity.) The list, and why each may be opaque,
  is `OPAQUE_TYPES` in crates/hir-ty-lean/src/body.rs, which must name the same types as here.
- **An opaque function** is `opaque <name> (a0 : A0) .. : Result R` in its own generated module
  (crates/hir-ty-lean/src/lib.rs, `opaque_module`). It gives a `Result`: it may panic or run
  forever, so nothing assumes it returns. Lean needs its type inhabited, which the instance below
  gives by `Result.div` without an axiom. Being a constant, it gives one result for one set of
  arguments: `OPAQUE_FUNCTIONS` says, for each, why the Rust function is a function of them.
-/

namespace Blaze

/-- What Lean's `opaque` needs of an opaque function's type, `A → .. → Result R`: a `Result` of
any type is inhabited, by `div`. The kernel never unfolds an opaque constant, so this value says
nothing of what the function gives. -/
instance Result.instInhabitedDiv {α : Type} : Inhabited (Result α) := ⟨Result.div⟩

/-- `std::io::Error`, defined in core (`core::io::error::Error`). -/
opaque core.io.error.Error : Type

/-- `core::fmt::Error`, the error of a formatter's write. -/
opaque core.fmt.Error : Type

/-- `core::fmt::Formatter`: its value stands for the whole state of the writer it writes to. -/
opaque core.fmt.Formatter : Type

/-- `core::fmt::Arguments`, what `format_args!` makes. -/
opaque core.fmt.Arguments : Type

/-- `core::panic::Location`, a place in the source. -/
opaque core.panic.location.Location : Type

/-- `alloc::string::String`, carried only: it gets a model of its bytes later (step d). -/
opaque alloc.string.String : Type

end Blaze
