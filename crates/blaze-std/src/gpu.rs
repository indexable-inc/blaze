//! `Gpu`, the capability to use a graphics device (docs/v2/plan/design/std.md, "Gpu, video and
//! images"), as a placeholder: it can be had only from [`Io::gpu`](crate::io::Io::gpu) and does
//! nothing yet but stand for the device, so that what needs one (the hardware video encoder,
//! [`crate::video::Encoder::hardware`]) says so in its signature. The device, buffers in a
//! caller's allocator, launches through the one wait, offscreen targets, `Gpu::cpu()` and the
//! Metal, Vulkan and CUDA providers are the GPU plan's, still to come.

/// A graphics device this program was handed: from [`Io::gpu`](crate::io::Io::gpu) only.
/// Cloning it copies authority it holds and adds none.
#[derive(Clone, Debug)]
pub struct Gpu {
    _private: (),
}

impl Gpu {
    /// What `Io::gpu` gives: a device on a native macOS world (whose media engine the hardware
    /// encoder runs on); none in the simulation, which has no device, nor on other targets
    /// until their providers exist.
    #[blaze::pure]
    pub(crate) fn offered(native: bool) -> Option<Gpu> {
        if native && cfg!(target_os = "macos") { Some(Gpu { _private: () }) } else { None }
    }
}
