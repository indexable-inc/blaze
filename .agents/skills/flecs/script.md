# Flecs script, the forms the site uses

Full manual: `docs/FlecsScript.md` in the Flecs repo (https://www.flecs.dev/flecs/md_docs_2FlecsScript.html).

```
using flecs.meta                         // struct, enum come from flecs.meta
struct P(text: string)                   // a component type, fields name: type
Ul {}                                    // a tag (an entity used as an id)
Ul { (With, flecs.core.OrderedChildren) }// a trait: adding Ul also adds OrderedChildren
Status { Exclusive }                     // one target per entity

prefab Page { OrderedChildren }          // a prefab
prefab Rfc : Page { (Status, Draft) }    // a prefab inheriting a prefab

site {                                   // reopen or create site
  rfcs {                                 // child: site.rfcs
    "linear-types" : Rfc {               // string name (hyphen), IsA Rfc
      Title: {"Values that must be used"}// component value, positional
      Number: {2}
      (DependsOn, site.charter)          // pair to an existing entity
      Section motivation(title: "Why") { // kind syntax with a named field
        P why("Prose with `code`, [[koka]] and [a link](https://x.dev).")
        P cited("Another.") { (Cites, refs.koka) }
        Code ex(lang: "rust", text: `fn main() \{
            println!("hi");
        }`)                              // backtick string: multiline, indent stripped by gen
      }
      Issue syntax(title: "Syntax") { (Status, Open); P q("...") }
      References refs                    // a widget tag as a block
    }
  }
}
```

- Comments are `//` to end of line.
- Entity paths use `.`: `site.rfcs.capabilities`, `refs.koka`.
- A component written in a scope without a value (`Title`) default-constructs it; with kind syntax a value is always assigned.
- `with Tag { a {} b {} }` adds Tag to each entity in the block; useful for a run of similar blocks.
- `const x = 10` and `$x` are variables; `for i in 0..3 {}` and `if` generate entities at script time; `template` makes parameterised prefabs. The site uses none of these yet.
