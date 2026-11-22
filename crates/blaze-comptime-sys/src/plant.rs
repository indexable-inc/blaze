//! The bugs a control can plant in the layer (D165, rule 78). Shipped code passes `NoPlant`.

#![blaze::sans_io]

use blaze_own::planted::Plants;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SysPlant {
    /// The layer as it is.
    None,
    /// A relative path crosses without the working directory joined.
    CwdIgnored,
    /// `read` gives one byte past a file's end.
    ReadPastEnd,
    /// `dup` copies the description, so the two numbers stop sharing an offset.
    DupCopies,
    /// `getenv` asks the host again for a name it has an answer for (or the run set itself).
    EnvAskedTwice,
}

impl Plants for SysPlant {}
