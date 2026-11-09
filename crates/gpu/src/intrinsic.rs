//! What only a GPU can do: derivatives across a fragment quad, sampling a texture, discarding a
//! fragment. The device backend lowers each of these functions by its path, from one table in
//! blaze-shader (docs/future/gpu/shaders.md, "What the compiler adds"); a call it has no lowering
//! for is an error naming the function. Their host bodies are what the host can do: nothing yet,
//! since a derivative needs the quad of fragments around this one, which plan.md's CPU backend
//! will run, and a texture only exists on the device. So each panics naming itself, never
//! returning a made-up value.
use crate::entry::{Sampler, Texture2d};
use crate::vec::{IVec2, Vec2, Vec4};

#[cold]
#[inline(never)]
#[track_caller]
pub(crate) fn device_only(what: &str) -> ! {
    panic!("gpu::{what} runs only on a GPU: the host has no fragment quad, texture, device buffer or acceleration structure to give it")
}

/// How fast `v` changes across the quad, in x: Slang's `ddx`.
#[inline(never)]
pub fn dpdx<T: Copy>(v: T) -> T {
    let _ = v;
    device_only("intrinsic::dpdx")
}

/// How fast `v` changes across the quad, in y: Slang's `ddy`.
#[inline(never)]
pub fn dpdy<T: Copy>(v: T) -> T {
    let _ = v;
    device_only("intrinsic::dpdy")
}

/// `abs(dpdx(v)) + abs(dpdy(v))`: Slang's `fwidth`.
#[inline(never)]
pub fn fwidth<T: Copy>(v: T) -> T {
    let _ = v;
    device_only("intrinsic::fwidth")
}

/// Drop this fragment: Slang's `discard`.
#[inline(never)]
pub fn discard() -> ! {
    device_only("intrinsic::discard")
}

impl Texture2d {
    /// The texture filtered at `uv` (0 to 1), its level chosen from the quad's derivatives:
    /// Slang's `Sample`.
    #[inline(never)]
    pub fn sample(&self, smp: &Sampler, uv: Vec2) -> Vec4 {
        let _ = (smp, uv);
        device_only("Texture2d::sample")
    }

    /// The texture filtered at `uv` at mip `level`: Slang's `SampleLevel`.
    #[inline(never)]
    pub fn sample_level(&self, smp: &Sampler, uv: Vec2, level: f32) -> Vec4 {
        let _ = (smp, uv, level);
        device_only("Texture2d::sample_level")
    }

    /// The texel at `at` of mip `level`, unfiltered: Slang's `Load`.
    #[inline(never)]
    pub fn load(&self, at: IVec2, level: u32) -> Vec4 {
        let _ = (at, level);
        device_only("Texture2d::load")
    }
}
