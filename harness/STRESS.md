# Stress edits

99 of 99 steps right, 0 restarts. Log: /tmp/blaze-stress-out-4kd9d848/daemon.log

| # | Edit | Reply | ms | Result |
|---|---|---|---:|---|
| 0 | body: constant in a leaf function | patch | 12 | same |
| 1 | body: in main | patch | 34 | same |
| 2 | body: generic function used from another crate | patch | 13 | same |
| 3 | body: generic method of a generic struct | patch | 22 | same |
| 4 | body: trait default method | patch | 14 | same |
| 5 | body: closure inside a function | patch | 21 | same |
| 6 | body: async fn | patch | 11 | same |
| 7 | body: async fn gets another await | patch | 20 | same |
| 8 | body: Drop impl | patch | 14 | same |
| 9 | body: match arm in trait impl | patch | 11 | same |
| 10 | blank line at top of a file with line!() and track_caller | patch | 10 | same |
| 11 | blank line above a panic site | patch | 14 | same |
| 12 | comment appended | patch | 12 | same |
| 13 | new private function, unused | patch | 12 | same |
| 14 | new public function, called from main | patch | 133 | same |
| 15 | function renamed, callers updated | patch | 61 | same |
| 16 | function removed | patch | 34 | same |
| 17 | function moved to another file of the crate | patch | 58 | same |
| 18 | signature: parameter added, callers in another crate updated | patch | 82 | same |
| 19 | signature: return type changed | patch | 58 | same |
| 20 | signature: parameter type &str to String | patch | 69 | same |
| 21 | signature: function made generic | full | 99 | same |
| 22 | signature: impl Fn return becomes Box<dyn Fn> | patch | 39 | same |
| 23 | signature: const fn body and a const that uses it | patch | 29 | same |
| 24 | signature: fn becomes async, caller awaits through block_on | patch | 44 | same |
| 25 | struct: field added, constructor updated | patch | 53 | same |
| 26 | struct: fields reordered (derived PartialOrd changes) | patch | 48 | same |
| 27 | struct: field type widened across crates | patch | 41 | same |
| 28 | struct: private field added to a generic struct | patch | 57 | same |
| 29 | enum: variant added, matches updated | patch | 56 | same |
| 30 | enum: variants reordered (discriminants move) | patch | 42 | same |
| 31 | derive added | patch | 41 | same |
| 32 | derive removed, hand-written impl instead | full | 140 | same |
| 33 | repr and type alias | patch | 39 | same |
| 34 | trait: method added with a default (vtable grows) | patch | 37 | same |
| 35 | trait: default overridden in one impl | patch | 29 | same |
| 36 | trait: required method added, all impls updated | patch | 59 | same |
| 37 | trait: associated const changed | patch | 20 | same |
| 38 | trait: new impl for a foreign-crate type | patch | 45 | same |
| 39 | trait: impl removed | patch | 39 | same |
| 40 | trait: new trait with blanket impl | patch | 37 | same |
| 41 | trait: generic bound tightened | patch | 31 | same |
| 42 | trait: associated type changed on an Iterator impl | patch | 42 | same |
| 43 | trait: operator impl added | patch | 37 | same |
| 44 | generic: new instantiation of an existing generic | patch | 50 | same |
| 45 | generic: std collection never used before | patch | 275 | same |
| 46 | generic: type parameter added to a function | patch | 76 | same |
| 47 | generic: const generic introduced | patch | 73 | same |
| 48 | macro: body of macro_rules used in another crate | patch | 53 | same |
| 49 | macro: function-making macro changed | patch | 55 | same |
| 50 | macro: new invocation makes a new function | patch | 75 | same |
| 51 | macro: new macro defined and used in the same edit | patch | 91 | same |
| 52 | macro: macro that declares a struct and impl | patch | 83 | same |
| 53 | macro: format string changed | patch | 52 | same |
| 54 | macro: matches!, vec!, assert!, write! in a new function | patch | 81 | same |
| 55 | const value changed (used in a const fn and an array length) | patch | 85 | same |
| 56 | static value changed | patch | 65 | same |
| 57 | static added and used | patch | 96 | same |
| 58 | static initial value of an atomic changed | patch | 84 | same |
| 59 | thread_local added | patch | 122 | same |
| 60 | static with a lazily made value | patch | 180 | same |
| 61 | new file and module | patch | 120 | same |
| 62 | edit to the file just added | patch | 82 | same |
| 63 | module renamed (file moved) | patch | 137 | same |
| 64 | module becomes a directory with a submodule | patch | 122 | same |
| 65 | re-export added and used | patch | 132 | same |
| 66 | inline module with nested function and impl | patch | 116 | same |
| 67 | include_str! of a new file | patch | 164 | same |
| 68 | the included file changes | patch | 96 | same |
| 69 | module removed | patch | 101 | same |
| 70 | #[inline(never)] and #[cold] added | patch | 76 | same |
| 71 | cfg(debug_assertions) branch | patch | 133 | same |
| 72 | visibility narrowed and widened | patch | 93 | same |
| 73 | BROKEN: syntax error | error | 338 | said so (error: /tmp/blaze-stress-2u_jflxh/mid/src/text.rs:17: expect) |
| 74 | fixed again, with a body change | full | 81 | same |
| 75 | BROKEN: type error in a body | error | 148 | said so (error: `main` is not compiled by blaze: unsupported: callee ) |
| 76 | BROKEN: still broken, another file edited too | error | 12 | said so (error: `main` is not compiled by blaze: unsupported: callee ) |
| 77 | fixed again | full | 97 | same |
| 78 | BROKEN: unresolved name in main | error | 65 | said so (error: `main` has an error blaze cannot say more of: fronten) |
| 79 | fixed again by adding the missing function | full | 113 | same |
| 80 | BROKEN: caller not updated after a signature change | error | 53 | said so (error: `main` has an error blaze cannot say more of: fronten) |
| 81 | fixed again: caller updated | full | 135 | same |
| 82 | BROKEN: file emptied | error | 66 | said so (error: `main` has an error blaze cannot say more of: fronten) |
| 83 | file restored and three crates edited in one build | full | 110 | same |
| 84 | thread_local of the first source given another value | full | 120 | same |
| 85 | BROKEN: struct literal without one of its fields | error | 14 | said so (error: /tmp/blaze-stress-2u_jflxh/base/src/lib.rs:109:5: mis) |
| 86 | mended: the field given | patch | 10 | same |
| 87 | BROKEN: match without one of its arms | error | 25 | said so (error: /tmp/blaze-stress-2u_jflxh/mid/src/lib.rs:199:11: mis) |
| 88 | mended: the arm given | patch | 14 | same |
| 89 | BROKEN: impl without one of its trait's methods | error | 23 | said so (error: /tmp/blaze-stress-2u_jflxh/app/src/main.rs:106:6: not) |
| 90 | mended: the method given | patch | 21 | same |
| 91 | BROKEN: method that no type has | error | 10 | said so (error: /tmp/blaze-stress-2u_jflxh/mid/src/text.rs:22:7: no m) |
| 92 | mended: the method named right | patch | 9 | same |
| 93 | BROKEN: import of a name that is not there | error | 35 | said so (error: /tmp/blaze-stress-2u_jflxh/app/src/main.rs:102:5: unr) |
| 94 | mended: the import removed | patch | 35 | same |
| 95 | whole file rewritten in another order | patch | 11 | same |
| 96 | large generated function added | patch | 155 | same |
| 97 | everything back to the first source | full | 120 | same |
| 98 | and forward again to the last source | full | 198 | same |
