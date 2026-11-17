//! The layout's field-grounding refusal reaches a constant's error NAMING the field
//! ([`Refusal::FieldGround`]): the ADT, the field's row and name, and the projection that has
//! no ground form. Before it, every such refusal was "a field type with no ground form",
//! naming neither (alloc's `STATIC_INNER_SLICE`, port27/front2/list-AI.txt).
//!
//! The Reference, "Static items": "The static initializer is a constant expression evaluated
//! at compile time"; a value is laid out by its type, field by field ("Type layout").
//!
//! Line it prints (and asserts): `constant 0x9: field 1 (name 0x1) of ADT 0x1000 has no ground
//! form: the projection `<_ as 0x77>::0x5` is not normalized`.

use blaze_build::BuildError;
use blaze_build::layout::{LayoutError, Refusal as L};
use blaze_typeck::{ItemKey, Name, PTy};

use super::{Stop, stop};
use crate::error::{ConstError, Refusal};

#[test]
fn a_field_with_no_ground_form_is_named() {
    let e = LayoutError {
        ty: PTy(3),
        why: L::FieldGround { adt: ItemKey(0x1000), field: 1, name: Name(1), error: BuildError::Projection { trait_: ItemKey(0x77), name: Name(5) } },
    };
    let Stop::Refused(refusal) = stop(&e) else { panic!("a field's grounding is a refusal, not a need") };
    assert_eq!(refusal, Refusal::FieldGround { adt: ItemKey(0x1000), field: 1, name: Name(1), projection: Some((ItemKey(0x77), Name(5))) });
    let shown = std::format!("{}", ConstError { constant: ItemKey(9), at: None, refusal });
    std::eprintln!("{shown}");
    assert_eq!(shown, "constant 0x9: field 1 (name 0x1) of ADT 0x1000 has no ground form: the projection `<_ as 0x77>::0x5` is not normalized");
}
