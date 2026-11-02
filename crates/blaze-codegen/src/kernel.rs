//! Codegen's steps in the kernel (`blaze_kernel`, docs/v2/plan/lanes/kernel.md).
//!
//! A dev template is the kernel's entry with no reads of [`DevTemplate`]. Its identity is the
//! instance as the driver keeps it ([`crate::driver::placed`] of [`crate::driver::stored`]: the
//! instance key, what it builds in, and the kinds of its references), the step's code, and the
//! settings. The store keeps the template's blob ([`crate::blob::encode`]) as it is, under
//! [`blaze_kernel::whole::key`]: one probe, no header.
//!
//! The driver still compiles templates in its own batches (the memo, flights, folding), so
//! `DevTemplate` is a [`Step`] the kernel keys, not a [`blaze_kernel::Compute`] it runs. The
//! driver computes and puts it.
//!
//! Its code is [`crate::settings::Toolchain::id`] (the pinned nightly and the blaze
//! executable's hash) until lane identity gives the step a code identity of its own. Once it
//! does, `Settings` drops the executable's hash, and a blaze that changed no codegen code
//! reads the templates the last one kept.

use blaze_cas::{Key, Kind};
use blaze_ids::ContentId;
use blaze_kernel::{CodeId, Step, StepId, Subject, identity, steps, whole};

use crate::{InstanceKey, Settings};

/// A function instance's dev template.
pub struct DevTemplate;

/// What a dev template is of.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TemplateOf {
    /// What the driver keeps the instance's code as: `placed(stored(key, built_in), refs)`.
    pub stored: InstanceKey,
    pub settings: Settings,
}

impl Step for DevTemplate {
    const ID: StepId = steps::DEV_TEMPLATE;
    const NAME: &'static str = "codegen dev template";
    const KIND: Kind = Kind::Code;
    type Input = TemplateOf;

    fn subject(input: &TemplateOf) -> Subject {
        Subject { of: input.stored.0, flags: input.settings.0 }
    }

    fn label(input: &TemplateOf) -> String {
        // alloc: an error's text
        format!("the instance kept as {:x}", input.stored.0)
    }
}

/// The step's code: the toolchain's id (`Spec::toolchain.id`), until lane identity gives it its own.
pub fn code(toolchain: &ContentId) -> CodeId {
    CodeId(*toolchain)
}

/// The store key of the dev template of the instance kept as `stored`, at `settings`, compiled
/// by the codegen of `toolchain` (`Spec::toolchain.id`).
pub fn template(stored: &InstanceKey, settings: &Settings, toolchain: &ContentId) -> Key {
    whole::key(&identity::<DevTemplate>(code(toolchain), &TemplateOf { stored: *stored, settings: *settings }))
}
