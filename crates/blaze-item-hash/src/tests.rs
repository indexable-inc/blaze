//! Tests of docs/v2/plan/design/item-hash.md section 9: [`perturb`] changes each input of an
//! identity and of each fact alone; [`classify`] checks every read of every step is covered, a
//! fact, or counted as refused; [`anon`] checks renames and moves keep an identity and
//! change its presentation.

mod anon;
mod classify;
mod perturb;
