//! An effect's kind as the meter counts it (blaze-meter `Call`, D169): one table, read where
//! every effect is stamped on its asker's thread (bstd `io/fast.rs` `stamp`).

use blaze_meter::Call;

use crate::{AboveOp, Effect};

impl Effect {
    /// The meter's kind of this effect: the file calls by name, the rest as [`Call::Other`].
    pub fn metered(&self) -> Call {
        match self {
            Effect::Stat { .. } | Effect::Lstat { .. } | Effect::Stamp { .. } => Call::Stat,
            Effect::Entries { .. } | Effect::List { .. } => Call::Attributes,
            Effect::ReadAll { .. } => Call::Read,
            Effect::Read { .. } | Effect::ReadInto { .. } | Effect::ReadMany { .. } | Effect::ReadLink { .. } => Call::Pread,
            Effect::Write { .. }
            | Effect::WriteUnsynced { .. }
            | Effect::WriteFrom { .. }
            | Effect::WriteAt { .. }
            | Effect::Append { .. }
            | Effect::Truncate { .. } => Call::Write,
            Effect::Map { .. } | Effect::MapWrite { .. } => Call::Mmap,
            Effect::Sync { .. } | Effect::Barrier { .. } | Effect::WriteBack { .. } | Effect::SyncDir { .. } => Call::Fsync,
            Effect::Rename { .. } | Effect::RenameUnsynced { .. } => Call::Rename,
            Effect::Watch { .. } | Effect::Journal { .. } | Effect::Unwatch { .. } | Effect::Flush { .. } => Call::FileEvents,
            Effect::Above { op, .. } => match op {
                AboveOp::Stat => Call::Stat,
                AboveOp::Read => Call::Read,
                AboveOp::List => Call::Attributes,
            },
            _ => Call::Other,
        }
    }
}
