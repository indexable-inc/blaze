//! The device build of a crate (docs/future/gpu/shaders.md, "The device build"): its
//! `GPU_ENTRIES` read by const evaluation, every entry lowered into one naga module, the module
//! validated by naga, and written as MSL whose bindings are the descriptors' slots.
//!
//! The binding table is not a second decision: each entry's MSL resource map is built from the
//! slots its descriptor holds (`gpu::entry`'s `EntryDesc`, which the host binds through), and the
//! storage sizes buffer from the descriptor's `sizes` (`gpu::sizes_slot`), so the host and the
//! device can't number a slot differently (`mirrors-derive-or-check`).
//!
//! The Metal step is not here: blaze writes MSL source, and the host compiles it with Metal's
//! own `newLibraryWithSource` at run time, so no `xcrun` runs at build time and nothing leaves the
//! sandbox. A later `.metallib` step is the plan's (docs/future/gpu/shaders.md, "The Metal
//! step").
use std::collections::BTreeMap;

use base_db::{Crate, Db};
use naga::{
    back::msl::{self, BindSamplerTarget, BindTarget, EntryPointResources},
    valid::{Capabilities, ValidationFlags, Validator},
};

use crate::{
    entries::{self, Entry, Table},
    lower::Device,
    refuse::Error,
};

/// The MSL language version written: Metal 2.4 (macOS 12), the oldest with every feature the
/// lowering uses.
pub const MSL_VERSION: (u8, u8) = (2, 4);

/// What the device build of a crate makes.
#[derive(Clone, Debug)]
pub struct DeviceBuild {
    /// The MSL source of every entry, one library.
    pub msl: String,
    /// The entries, in `GPU_ENTRIES`'s order: their names in the MSL and their slots.
    pub entries: Vec<Entry>,
    /// The validated module, for the other back ends and for tests.
    pub module: naga::Module,
}

/// The device build of `krate`: every entry of its `GPU_ENTRIES`, lowered, validated and written
/// as MSL. Every refusal met in every entry is in the error, not only the first.
///
/// A refusal names types with the database in hand (`refuse::ty_name`); what it shows of the
/// front end's own values (a MIR terminator, a projection, a lowering error) is their `Debug`,
/// made while `db` is attached so that it can read it.
pub fn device_msl(db: &Db, krate: Crate) -> Result<DeviceBuild, Error> {
    blaze_query::db::attach(db, || build(db, krate))
}

fn build(db: &Db, krate: Crate) -> Result<DeviceBuild, Error> {
    let entries = entries::entries(db, krate)?;
    let mut device = Device::new(db, krate);
    let mut refusals = Vec::new(); // alloc: the build's refusals
    let mut tables = Vec::with_capacity(entries.len()); // alloc: one binding table per entry
    for entry in &entries {
        match device.lower_entry(entry) {
            Ok(table) => tables.push(table),
            Err(refused) => refusals.extend(refused),
        }
    }
    if !refusals.is_empty() {
        return Err(Error::Refused(refusals));
    }
    let module = device.module;
    let info = Validator::new(ValidationFlags::all(), capabilities(&module)).validate(&module).map_err(|e| {
        let inner = e.as_inner();
        let entry = match inner {
            naga::valid::ValidationError::EntryPoint { name, .. } => name.clone(), // clone: an error path
            _ => "the module".to_owned(),                                          // alloc: an error
        };
        Error::Invalid { entry, what: chain(inner) }
    })?;

    let mut per_entry_point_map = BTreeMap::new(); // alloc: the MSL resource map
    for (entry, table) in entries.iter().zip(&tables) {
        let mut resources = BTreeMap::new(); // alloc: one entry's resources
        for (binding, slot) in table {
            let index = u8::try_from(slot.index).map_err(|_| Error::Invalid {
                entry: entry.name.clone(), // clone: an error path
                what: format!("slot {} of the {:?} table is past Metal's 255", slot.index, slot.table), // alloc: an error
            })?;
            let mut target = BindTarget::default();
            match slot.table {
                Table::Buffer => target.buffer = Some(index),
                Table::Texture => target.texture = Some(index),
                Table::Sampler => target.sampler = Some(BindSamplerTarget::Resource(index)),
                Table::None => continue,
            }
            resources.insert(binding.clone(), target); // clone: a small binding
        }
        let sizes_buffer = match entry.sizes {
            Some(slot) => Some(u8::try_from(slot.index).map_err(|_| Error::Invalid {
                entry: entry.name.clone(), // clone: an error path
                what: format!("the sizes buffer's slot {} is past Metal's 255", slot.index), // alloc: an error
            })?),
            None => None,
        };
        // clone: the map is keyed by name
        per_entry_point_map.insert(entry.name.clone(), EntryPointResources { resources, immediates_buffer: None, sizes_buffer });
    }
    let options = msl::Options {
        lang_version: MSL_VERSION,
        per_entry_point_map,
        // A resource the map does not bind is the lowering's bug, not a slot to invent.
        fake_missing_bindings: false,
        // Bounds: Rust's own `assert`s check every index before it is used, and a failed one
        // ends the invocation, so naga adds no second check.
        ..msl::Options::default()
    };
    let (msl, translation) = msl::write_string(&module, &info, &options, &msl::PipelineOptions::default())
        .map_err(|e| Error::Msl(chain(&e)))?;
    for (entry, name) in entries.iter().zip(&translation.entry_point_names) {
        match name {
            Ok(name) if *name == entry.name => {}
            // alloc: an error
            Ok(name) => return Err(Error::Msl(format!("gpu entry `{}` was written as `{name}`, not its own name", entry.name))),
            Err(e) => return Err(Error::Msl(format!("gpu entry `{}`: {}", entry.name, chain(e)))), // alloc: an error
        }
    }
    Ok(DeviceBuild { msl, entries, module })
}

/// What the module uses past naga's baseline, and so what the validator allows: a ray query when an
/// entry traces one, and half packing (`f32tof16`, `f16tof32`) when an entry packs; Metal has both.
/// Never more than the module uses, so a construct a lowering did not mean to make still fails
/// validation.
fn capabilities(module: &naga::Module) -> Capabilities {
    let mut caps = Capabilities::empty();
    for (_, t) in module.types.iter() {
        if let naga::TypeInner::AccelerationStructure { .. } | naga::TypeInner::RayQuery { .. } = t.inner {
            caps |= Capabilities::RAY_QUERY;
        }
    }
    for e in &module.entry_points {
        for (_, x) in e.function.expressions.iter() {
            if let naga::Expression::Math { fun: naga::MathFunction::Pack2x16float | naga::MathFunction::Unpack2x16float, .. } = x {
                caps |= Capabilities::SHADER_FLOAT16_IN_FLOAT32;
            }
        }
    }
    caps
}

/// An error and its sources, as one line.
fn chain(e: &dyn std::error::Error) -> String {
    let mut out = e.to_string(); // alloc: an error
    let mut at = e.source();
    while let Some(source) = at {
        out.push_str(": ");
        out.push_str(&source.to_string());
        at = source.source();
    }
    out
}
