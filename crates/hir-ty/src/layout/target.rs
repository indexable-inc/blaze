//! Target dependent parameters needed for layouts

use base_db::{Db, Crate, target::TargetLoadError};
use hir_def::layout::TargetDataLayout;
use rustc_abi::{AddressSpace, AlignFromBytesError, TargetDataLayoutError};


#[blaze_query::tracked(returns(as_ref), mem_tag = Infer)]
pub fn target_data_layout_query(
    db: &Db,
    krate: Crate,
) -> Result<TargetDataLayout, TargetLoadError> {
    match &krate.workspace_data(db).target {
        Ok(target) => match TargetDataLayout::parse_from_llvm_datalayout_string(&target.data_layout, AddressSpace::ZERO) {
            Ok(it) => Ok(it),
            Err(e) => {
                Err(match e {
                    TargetDataLayoutError::InvalidAddressSpace { addr_space, cause, err } => {
                        format!( // alloc: error text, once per crate whose data layout does not parse
                            r#"invalid address space `{addr_space}` for `{cause}` in "data-layout": {err}"#
                        )
                    }
                    TargetDataLayoutError::InvalidBits { kind, bit, cause, err } => format!(r#"invalid {kind} `{bit}` for `{cause}` in "data-layout": {err}"#), // alloc: as above
                    TargetDataLayoutError::MissingAlignment { cause } => format!(r#"missing alignment for `{cause}` in "data-layout""#), // alloc: as above
                    TargetDataLayoutError::InvalidAlignment { cause, err } => {
                        let (align, err_kind) = match err {
                            AlignFromBytesError::NotPowerOfTwo(align) => (align, "not a power of two"),
                            AlignFromBytesError::TooLarge(align) => (align, "too large"),
                        };
                        format!(r#"invalid alignment for `{cause}` in "data-layout": `{align}` is {err_kind}"#) // alloc: as above
                    },
                    TargetDataLayoutError::InconsistentTargetArchitecture { dl, target } => {
                        format!(r#"inconsistent target specification: "data-layout" claims architecture is {dl}-endian, while "target-endian" is `{target}`"#) // alloc: as above
                    }
                    TargetDataLayoutError::InconsistentTargetPointerWidth {
                        pointer_size,
                        target,
                    } => format!(r#"inconsistent target specification: "data-layout" claims pointers are {pointer_size}-bit, while "target-pointer-width" is `{target}`"#), // alloc: as above
                    TargetDataLayoutError::InvalidBitsSize { err } => err,
                    TargetDataLayoutError::UnknownPointerSpecification { err } => format!(r#"use of unknown pointer specifier in "data-layout": {err}"#), // alloc: as above
                }.into())
            }
        },
        Err(e) => Err(e.clone()), // clone: the load error, once per crate whose target did not load
    }
}
