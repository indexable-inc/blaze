//! The native backend's reads of its host's facts: the one place outside `rt` that asks the
//! system who and where the process is. Each fact never changes within a run, so it is read
//! once, when the world is opened, and kept in the world (docs/v2/plan/design/std.md, "Every
//! reach goes through the driver"); a record keeps them as one entry, and a replay supplies
//! them.

use alloc::sync::Arc;
use core::time::Duration;

use blaze_core::cores::CoresError;

use crate::{clock::Born, host::HostFacts};

/// The host's facts, read now: the process's id, its cores, when it began against the
/// driver's clock (`driver_now`, read just before), and the running image's build id. `name`
/// is the host's name, as the launcher was given it.
pub(crate) fn host_facts(name: Arc<str>, driver_now: Duration) -> Result<HostFacts, CoresError> {
    let cores = blaze_core::cores::machine()?;
    // The process's age and the driver's clock, read together: the driver began this long
    // after the process did.
    let born = match blaze_core::wait::process_age() {
        Ok(age) => Born::Before(age.saturating_sub(driver_now)),
        // alloc: once per process, why its start is not known.
        Err(error) => Born::Unknown(Arc::from(alloc::string::ToString::to_string(&error))),
    };
    Ok(HostFacts { pid: crate::rt::process_id(), cores, name, born, build_id: crate::rt::image::build_id() })
}
