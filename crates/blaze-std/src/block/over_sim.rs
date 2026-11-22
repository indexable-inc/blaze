//! [`OverSim`]: blaze-core's simulated block device (`blaze_core::sim::block`) under the block
//! grant, its facts drawn by the simulation. A durable write is a forced-unit-access write, as
//! `RWF_DSYNC` sends one; `Durable::Started` is `No`: a device has no write-back to begin, a
//! completed write is in its cache already.

use core::alloc::Allocator;
use core::num::NonZeroU32;

use blaze_core::FileName;
use blaze_core::planted::Planted;
use blaze_core::sim::block::{BlockModelPlant, ShapeError, SimBlock, SimBlockError};

use super::{BlockKind, BlockSize, Blocks, Device, DeviceFacts, DeviceFactsOpts, Durable, Lba};
use crate::cx::Cx;

/// The simulated device, as a block grant's device.
pub struct OverSim<A: Allocator + Clone, P: Planted<BlockModelPlant>> {
    device: SimBlock<A, P>,
    facts: DeviceFacts,
    name: FileName,
}

impl<A: Allocator + Clone, P: Planted<BlockModelPlant>> OverSim<A, P> {
    /// `device` under the grant, named `name` in errors; its shape as facts.
    pub fn new(device: SimBlock<A, P>, name: FileName) -> Result<OverSim<A, P>, BlockKind> {
        let shape = device.shape();
        let (logical, physical) = (shape.logical, shape.physical);
        // A device's shape was checked at its making; these hold the same bounds by type.
        let facts = DeviceFacts::new(DeviceFactsOpts {
            logical: BlockSize::new(logical).ok_or(BlockKind::Shape(ShapeError::Logical { logical }))?,
            physical: BlockSize::new(physical).ok_or(BlockKind::Shape(ShapeError::Physical { logical, physical }))?,
            atomic_write: Blocks(u64::from(shape.atomic)),
            queue_depth: NonZeroU32::new(shape.queue_depth)
                .ok_or(BlockKind::Shape(ShapeError::Queue { queue_depth: shape.queue_depth }))?,
            volatile_cache: shape.volatile_cache,
            size: Blocks(shape.blocks),
        })
        .map_err(BlockKind::Facts)?;
        Ok(OverSim { device, facts, name })
    }

    /// The device, for a test to crash ([`SimBlock::crash`]) or read whole.
    pub fn device(&mut self) -> &mut SimBlock<A, P> {
        &mut self.device
    }
}

fn sim(error: SimBlockError) -> BlockKind {
    match error {
        SimBlockError::Unreadable { lba } => BlockKind::Unreadable { lba: Lba(lba) },
        error => BlockKind::Sim(error),
    }
}

impl<A: Allocator + Clone, P: Planted<BlockModelPlant>> Device for OverSim<A, P> {
    fn facts(&self) -> DeviceFacts {
        self.facts
    }

    fn name(&self) -> &FileName {
        &self.name
    }

    fn read(&mut self, _cx: &Cx, at: Lba, into: &mut [u8]) -> Result<(), BlockKind> {
        self.device.read(at.0 * self.facts.logical.bytes(), into).map_err(sim)
    }

    fn write(&mut self, _cx: &Cx, at: Lba, bytes: &[u8], durable: Durable, _atomic: bool) -> Result<(), BlockKind> {
        // The device tears by its atomic unit only, which `Block::write_atomic` kept the write
        // within: nothing more to ask.
        let fua = durable == Durable::Yes;
        self.device.write(at.0 * self.facts.logical.bytes(), bytes, fua).map_err(sim)
    }

    fn flush(&mut self, _cx: &Cx) -> Result<(), BlockKind> {
        self.device.flush();
        Ok(())
    }

    fn discard(&mut self, _cx: &Cx, at: Lba, len: Blocks) -> Result<(), BlockKind> {
        let logical = self.facts.logical.bytes();
        self.device.discard(at.0 * logical, len.0 * logical).map_err(sim)
    }
}
