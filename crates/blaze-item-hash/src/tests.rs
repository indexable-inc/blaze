//! Tests of docs/v2/plan/design/item-hash.md section 9: [`perturb`] changes each input of an
//! identity and of each fact alone; [`facts`] checks each fact moves with what it stands for
//! and only then; [`anon`] checks renames and moves keep an identity and change its
//! presentation; [`early`] checks the identity made before lowering.

mod anon;
mod early;
mod escaped;
mod facts;
mod perturb;
mod steps;
