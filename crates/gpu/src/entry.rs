//! What an entry point's signature says, as data: each parameter's kind (from its type, through
//! [`Param`]), the varyings' fields (through [`Varying`]), what it returns (through [`Output`]),
//! and the slot each resource is bound at, assigned by [`layout`], the one home of that rule.
//!
//! `#[gpu::vertex]`, `#[gpu::fragment]` and `#[gpu::kernel]` write, next to the function, a module of its name
//! holding its [`EntryDesc`] (`ENTRY`) and one [`Slot`] per resource parameter, named like the
//! parameter. The host binds through those; the device backend reads the same constants by
//! const evaluation, so the two can't number differently.
use crate::vec::{IVec2, IVec4, UVec2, UVec3, UVec4, Vec2, Vec3, Vec4};

/// Which stage an entry point is.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Stage {
    Vertex,
    Fragment,
    /// A compute kernel, run by workgroups of `x * y * z` threads (Slang's `numthreads`).
    Compute { x: u32, y: u32, z: u32 },
}

/// The scalar or vector type of a value that crosses an interface.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ValueTy {
    F32,
    Vec2,
    Vec3,
    Vec4,
    U32,
    UVec4,
    I32,
    IVec2,
}

/// How a varying is carried from the vertex stage's corners to a fragment.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Interp {
    /// Interpolated with perspective (the default, Slang's unmarked fields).
    Perspective,
    /// Interpolated in screen space (Slang's `noperspective`).
    Linear,
    /// The provoking vertex's value (Slang's `nointerpolation`).
    Flat,
}

/// What one field of a varyings struct is.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FieldKind {
    /// The clip position out of the vertex stage, the fragment's window position into the fragment
    /// stage (`SV_Position`).
    Position,
    /// A user value at the next location.
    Value { ty: ValueTy, interp: Interp },
}

/// One field of a varyings struct, with the location [`layout_fields`] gave it (`None` for
/// [`FieldKind::Position`]).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Field {
    pub name: &'static str,
    pub kind: FieldKind,
    pub location: Option<u32>,
}

/// What one parameter of an entry point is.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    /// The vertex's index in the draw (`SV_VertexID`).
    VertexIndex,
    /// The instance's index in the draw (`SV_InstanceID`).
    InstanceIndex,
    /// The interpolants, into the fragment stage.
    Varyings(&'static [Field]),
    /// A uniform buffer holding one `T` of this size (`&T`, `ConstantBuffer<T>`).
    Uniform { size: u32 },
    /// A read-only storage buffer of `T`s this far apart (`&[T]`, `StructuredBuffer<T>`).
    Storage { stride: u32 },
    /// A sampled 2D texture of four floats (`Texture2D<float4>`).
    Texture2d,
    /// A sampler (`SamplerState`).
    Sampler,
    /// The thread's identity, into a kernel (`SV_DispatchThreadID`, `SV_GroupThreadID`,
    /// `SV_GroupID`).
    Thread,
    /// A storage buffer of `T`s this far apart that a kernel reads and writes (`&RawOut<T>`,
    /// `RWStructuredBuffer<T>`).
    RawOut { stride: u32 },
    /// A top-level acceleration structure, for ray queries (`&Tlas`,
    /// `RaytracingAccelerationStructure`).
    Tlas,
}

/// What an entry point returns.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Out {
    /// A vertex stage's varyings.
    Varyings(&'static [Field]),
    /// A fragment stage's colour, into colour attachment 0 (`SV_Target`).
    Color,
    /// Nothing: a kernel's `()`.
    Nothing,
}

/// The argument table a resource is bound in: Metal's three, which Vulkan and WebGPU flatten
/// into one binding space per group.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Table {
    /// A built-in or the varyings: no slot.
    None,
    Buffer,
    Texture,
    Sampler,
}

/// Where a parameter is bound.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Slot {
    pub table: Table,
    pub index: u32,
}

impl Slot {
    /// The index, for an API that takes a `usize` (Metal's `atIndex:`).
    #[inline]
    pub const fn index(self) -> usize {
        self.index as usize
    }
}

/// One parameter: its name in the Rust signature, its kind and its slot.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ParamDesc {
    pub name: &'static str,
    pub kind: Kind,
    pub slot: Slot,
}

/// An entry point, as its signature says: the host's binding table and the device backend's
/// interface.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct EntryDesc {
    /// The function's name, which is also its name in the device library.
    pub name: &'static str,
    pub stage: Stage,
    pub params: &'static [ParamDesc],
    pub out: Out,
    /// Where the buffer of the storage buffers' sizes in bytes is bound ([`sizes_slot`]), which
    /// the device reads for a slice's length; `None` when the entry binds no storage buffer. The
    /// host writes there one `u32` per storage parameter, in parameter order.
    pub sizes: Option<Slot>,
}

impl EntryDesc {
    /// A kernel's workgroup size, `[x, y, z]`: what the host dispatches by. `None` for a graphics
    /// stage.
    pub const fn threads(&self) -> Option<[u32; 3]> {
        match self.stage {
            Stage::Compute { x, y, z } => Some([x, y, z]),
            Stage::Vertex | Stage::Fragment => None,
        }
    }

    /// The parameter named `name`, if there is one.
    pub const fn param(&self, name: &str) -> Option<&ParamDesc> {
        let mut i = 0;
        while i < self.params.len() {
            if str_eq(self.params[i].name, name) {
                return Some(&self.params[i]);
            }
            i += 1;
        }
        None
    }
}

/// The most threads a workgroup holds: Metal's `maxTotalThreadsPerThreadgroup` on every GPU
/// family, and WebGPU's `maxComputeInvocationsPerWorkgroup` limit is at most it.
pub const MAX_THREADS: u32 = 1024;

/// What a stage may take and return; `#[gpu::vertex]`, `#[gpu::fragment]` and `#[gpu::kernel]`
/// evaluate it on the entry they write, so a signature that breaks it fails to compile with the
/// rule it broke.
pub const fn check(e: &EntryDesc) {
    if let Stage::Compute { x, y, z } = e.stage {
        assert!(x > 0 && y > 0 && z > 0, "a kernel's workgroup has at least one thread on each axis");
        assert!(
            (x as u64) * (y as u64) * (z as u64) <= MAX_THREADS as u64,
            "a kernel's workgroup holds at most 1024 threads (Metal's limit on every GPU family)"
        );
    }
    let mut i = 0;
    while i < e.params.len() {
        match (e.stage, e.params[i].kind) {
            (Stage::Vertex, Kind::Varyings(_)) => panic!("a vertex stage takes no varyings: they are what it returns"),
            (Stage::Fragment, Kind::VertexIndex | Kind::InstanceIndex) => {
                panic!("a fragment stage takes no VertexIndex or InstanceIndex: carry it in a Flat varying")
            }
            (Stage::Compute { .. }, Kind::VertexIndex | Kind::InstanceIndex | Kind::Varyings(_)) => {
                panic!("a kernel takes no VertexIndex, InstanceIndex or varyings: its identity is a Thread")
            }
            (Stage::Vertex | Stage::Fragment, Kind::Thread) => panic!("only a kernel (#[gpu::kernel]) takes a Thread"),
            (Stage::Vertex | Stage::Fragment, Kind::RawOut { .. }) => {
                panic!("only a kernel (#[gpu::kernel]) writes a RawOut: a graphics stage's writes are its outputs")
            }
            _ => {}
        }
        i += 1;
    }
    match (e.stage, e.out) {
        (Stage::Vertex, Out::Varyings(fields)) => {
            let (mut i, mut positions) = (0, 0);
            while i < fields.len() {
                if let FieldKind::Position = fields[i].kind {
                    positions += 1;
                }
                i += 1;
            }
            assert!(positions == 1, "a vertex stage's varyings need exactly one Position field");
        }
        (Stage::Vertex, Out::Color) => panic!("a vertex stage returns a #[derive(gpu::Varyings)] struct, not a colour"),
        (Stage::Fragment, Out::Color) => {}
        (Stage::Fragment, Out::Varyings(_) | Out::Nothing) => panic!("a fragment stage returns a colour (Vec4)"),
        (Stage::Vertex, Out::Nothing) => panic!("a vertex stage returns a #[derive(gpu::Varyings)] struct"),
        (Stage::Compute { .. }, Out::Nothing) => {}
        (Stage::Compute { .. }, Out::Varyings(_) | Out::Color) => {
            panic!("a kernel returns nothing: what it makes it writes to a RawOut")
        }
    }
}

const fn str_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// Metal's buffer argument table holds 31 entries.
pub const MAX_BUFFERS: u32 = 31;
/// Metal's texture argument table, on the smallest GPU family.
pub const MAX_TEXTURES: u32 = 31;
/// Metal's sampler argument table.
pub const MAX_SAMPLERS: u32 = 16;

/// The slots of one entry point's parameters: each resource the next free index of its table,
/// in parameter order, counted per entry point (never across a program, as Slang does). A
/// signature with more resources than a table holds fails to compile, naming the table.
pub const fn layout<const N: usize>(names: [&'static str; N], kinds: [Kind; N]) -> [ParamDesc; N] {
    let none = Slot { table: Table::None, index: 0 };
    let mut out = [ParamDesc { name: "", kind: Kind::VertexIndex, slot: none }; N];
    let (mut buffers, mut textures, mut samplers) = (0u32, 0u32, 0u32);
    let mut i = 0;
    while i < N {
        let slot = match kinds[i] {
            Kind::VertexIndex | Kind::InstanceIndex | Kind::Varyings(_) | Kind::Thread => none,
            // Metal binds an acceleration structure in the buffer table, and wgpu's Metal backend
            // counts it with a bind group's buffers in entry order.
            Kind::Uniform { .. } | Kind::Storage { .. } | Kind::RawOut { .. } | Kind::Tlas => {
                buffers += 1;
                assert!(buffers <= MAX_BUFFERS, "an entry point binds more buffers than Metal's 31");
                Slot { table: Table::Buffer, index: buffers - 1 }
            }
            Kind::Texture2d => {
                textures += 1;
                assert!(textures <= MAX_TEXTURES, "an entry point binds more textures than Metal's 31");
                Slot { table: Table::Texture, index: textures - 1 }
            }
            Kind::Sampler => {
                samplers += 1;
                assert!(samplers <= MAX_SAMPLERS, "an entry point binds more samplers than Metal's 16");
                Slot { table: Table::Sampler, index: samplers - 1 }
            }
        };
        out[i] = ParamDesc { name: names[i], kind: kinds[i], slot };
        i += 1;
    }
    out
}

/// Where an entry point with these parameters binds the sizes of its storage buffers: the buffer
/// slot after its last buffer, when it has a storage buffer (`&[T]` or `&RawOut<T>`); the one
/// home of that rule,
/// which the device backend reads back from `EntryDesc::sizes` by const evaluation. A slice's
/// length on the device is its buffer's size over its stride, and Metal gives a shader no buffer's
/// size, so the host passes them (naga's MSL `sizes_buffer`).
pub const fn sizes_slot(params: &[ParamDesc]) -> Option<Slot> {
    let (mut storage, mut next) = (false, 0u32);
    let mut i = 0;
    while i < params.len() {
        if let Kind::Storage { .. } | Kind::RawOut { .. } = params[i].kind {
            storage = true;
        }
        if let Table::Buffer = params[i].slot.table
            && params[i].slot.index + 1 > next
        {
            next = params[i].slot.index + 1;
        }
        i += 1;
    }
    if !storage {
        return None;
    }
    assert!(next < MAX_BUFFERS, "an entry point binds storage buffers in all 31 of Metal's buffer slots, leaving none for their sizes");
    Some(Slot { table: Table::Buffer, index: next })
}

/// The locations of a varyings struct's fields: each value the next location in field order, the
/// position none. A struct with two positions fails to compile.
pub const fn layout_fields<const N: usize>(names: [&'static str; N], kinds: [FieldKind; N]) -> [Field; N] {
    let mut out = [Field { name: "", kind: FieldKind::Position, location: None }; N];
    let (mut next, mut positions) = (0u32, 0u32);
    let mut i = 0;
    while i < N {
        let location = match kinds[i] {
            FieldKind::Position => {
                positions += 1;
                assert!(positions <= 1, "a varyings struct has more than one Position");
                None
            }
            FieldKind::Value { ty, interp } => {
                // Integers are not interpolated: Metal, Vulkan and WebGPU each refuse one that isn't flat.
                if let ValueTy::U32 | ValueTy::UVec4 | ValueTy::I32 | ValueTy::IVec2 = ty {
                    assert!(
                        matches!(interp, Interp::Flat),
                        "an integer varying must be Flat<T>: integers are not interpolated"
                    );
                }
                next += 1;
                Some(next - 1)
            }
        };
        out[i] = Field { name: names[i], kind: kinds[i], location };
        i += 1;
    }
    out
}

/// A type that may be an entry point's parameter; [`Param::KIND`] says what it is.
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not a GPU entry point's parameter",
    note = "a parameter is a built-in (VertexIndex, InstanceIndex, Thread), the varyings, or a resource: \
            &T (a uniform), &[T] (read-only storage), &RawOut<T> (storage a kernel writes; &mut [T] would \
            hand every thread a &mut to one buffer), &Texture2d, &Sampler or &Tlas"
)]
pub trait Param {
    const KIND: Kind;
}

/// A type that may be a field of a varyings struct.
pub trait Varying {
    const KIND: FieldKind;
}

/// A struct of interpolants (`#[derive(gpu::Varyings)]`): its fields, with their locations.
pub trait Varyings {
    const FIELDS: &'static [Field];
}

/// A type an entry point may return.
pub trait Output {
    const OUT: Out;
}

/// Plain old data: any bit pattern of its size is a value and it has no padding, so the host may
/// write its bytes into a buffer and the device read them back (`#[derive(gpu::Pod)]`, which
/// checks the fields and the padding).
///
/// # Safety
///
/// Every bit pattern of `size_of::<Self>()` bytes is a valid `Self`, and `Self` has no padding.
pub unsafe trait Pod: Copy + 'static {}

macro_rules! pod {
    ($($t:ty),+) => {$(
        // SAFETY: a plain number or a `#[repr(C)]` struct of plain numbers of one size, so no padding.
        unsafe impl Pod for $t {}
    )+};
}
pod!(f32, u32, i32, Vec2, Vec3, Vec4, UVec2, UVec3, UVec4, IVec2, IVec4);
// SAFETY: an array of `Pod` has no padding between its elements and every bit pattern is a value.
unsafe impl<T: Pod, const N: usize> Pod for [T; N] {}

/// A value that crosses an interface by itself: its [`ValueTy`].
pub trait Value: Copy {
    const TY: ValueTy;
}

macro_rules! value {
    ($($t:ty => $v:ident),+) => {$(
        impl Value for $t {
            const TY: ValueTy = ValueTy::$v;
        }
        impl Varying for $t {
            const KIND: FieldKind = FieldKind::Value { ty: ValueTy::$v, interp: Interp::Perspective };
        }
    )+};
}
value!(f32 => F32, Vec2 => Vec2, Vec3 => Vec3, Vec4 => Vec4, u32 => U32, UVec4 => UVec4, i32 => I32, IVec2 => IVec2);

impl Output for Vec4 {
    const OUT: Out = Out::Color;
}

impl Output for () {
    const OUT: Out = Out::Nothing;
}

/// The vertex's index in the draw.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct VertexIndex(pub u32);

impl VertexIndex {
    #[inline]
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl Param for VertexIndex {
    const KIND: Kind = Kind::VertexIndex;
}

/// The instance's index in the draw.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct InstanceIndex(pub u32);

impl InstanceIndex {
    #[inline]
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl Param for InstanceIndex {
    const KIND: Kind = Kind::InstanceIndex;
}

/// A kernel thread's identity: where it is in the grid, in its workgroup, and which workgroup.
/// Only the kernel ABI makes one: it is not `Copy` or `Clone` and has no public constructor, so
/// the safe writes plan.md adds can key on it (docs/future/gpu/plan.md, "Memory safety by
/// construction").
#[derive(Debug, PartialEq)]
pub struct Thread {
    global: UVec3,
    local: UVec3,
    group: UVec3,
}

impl Thread {
    /// A Thread for this crate's tests; nothing else makes one but the kernel ABI.
    #[cfg(test)]
    pub(crate) fn new_for_tests(global: UVec3, local: UVec3, group: UVec3) -> Thread {
        Thread { global, local, group }
    }

    /// The thread's position in the whole grid (`SV_DispatchThreadID`): `group * threads + local`.
    #[inline]
    pub fn global(&self) -> UVec3 {
        self.global
    }

    /// The thread's position in its workgroup (`SV_GroupThreadID`).
    #[inline]
    pub fn local(&self) -> UVec3 {
        self.local
    }

    /// The workgroup's position in the grid (`SV_GroupID`).
    #[inline]
    pub fn group(&self) -> UVec3 {
        self.group
    }
}

impl Param for Thread {
    const KIND: Kind = Kind::Thread;
}

/// A storage buffer of `T`s that a kernel reads and writes: plan.md's raw writer. Indexing is
/// checked against [`RawOut::len`] (a failed check ends the invocation, like every panic on the
/// device); what a check can't see, two threads touching one element in one launch, is why
/// [`RawOut::read`] and [`RawOut::write`] are `unsafe`. Only the device has one, like
/// [`Texture2d`]: the methods' host bodies panic naming themselves.
pub struct RawOut<T> {
    _device_only: (),
    _of: core::marker::PhantomData<T>,
}

impl<T: Pod> RawOut<T> {
    /// How many `T`s the buffer holds.
    #[inline]
    pub fn len(&self) -> usize {
        self.count()
    }

    /// Whether the buffer holds none.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Element `i`. Past the end, the invocation ends.
    ///
    /// # Safety
    ///
    /// No other thread of this launch writes element `i`.
    #[inline]
    pub unsafe fn read(&self, i: usize) -> T {
        assert!(i < self.len(), "RawOut::read past the end");
        self.load(i)
    }

    /// Writes `v` at element `i`. Past the end, the invocation ends.
    ///
    /// # Safety
    ///
    /// No other thread of this launch reads or writes element `i`.
    #[inline]
    pub unsafe fn write(&self, i: usize, v: T) {
        assert!(i < self.len(), "RawOut::write past the end");
        self.store(i, v)
    }

    // The three the device backend lowers by path (blaze-shader's intrinsics table).

    #[inline(never)]
    fn count(&self) -> usize {
        crate::intrinsic::device_only("RawOut::len")
    }

    #[inline(never)]
    fn load(&self, i: usize) -> T {
        let _ = i;
        crate::intrinsic::device_only("RawOut::read")
    }

    #[inline(never)]
    fn store(&self, i: usize, v: T) {
        let _ = (i, v);
        crate::intrinsic::device_only("RawOut::write")
    }
}

impl<T: Pod> Param for &RawOut<T> {
    const KIND: Kind = Kind::RawOut { stride: size_of::<T>() as u32 };
}

/// `SV_Position`: written by the vertex stage as the clip position, read by the fragment stage as
/// the fragment's window position (x and y the pixel's centre in pixels, z its depth, w 1/w).
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct Position(pub Vec4);

impl Varying for Position {
    const KIND: FieldKind = FieldKind::Position;
}

/// A varying the fragment stage gets from the provoking vertex, not interpolated.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct Flat<T>(pub T);

impl<T: Value> Varying for Flat<T> {
    const KIND: FieldKind = FieldKind::Value { ty: T::TY, interp: Interp::Flat };
}

/// A varying interpolated in screen space, without perspective.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct Linear<T>(pub T);

impl<T: Value> Varying for Linear<T> {
    const KIND: FieldKind = FieldKind::Value { ty: T::TY, interp: Interp::Linear };
}

impl<T> core::ops::Deref for Flat<T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T> core::ops::Deref for Linear<T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T: Pod> Param for &T {
    const KIND: Kind = Kind::Uniform { size: size_of::<T>() as u32 };
}

impl<T: Pod> Param for &[T] {
    const KIND: Kind = Kind::Storage { stride: size_of::<T>() as u32 };
}

/// A sampled 2D texture of four floats. Only the device has one: the host can't make it, so a
/// fragment stage that samples runs only there (until plan.md's CPU backend).
pub struct Texture2d {
    _device_only: (),
}

/// A sampler. Only the device has one, like [`Texture2d`].
pub struct Sampler {
    _device_only: (),
}

impl Param for &Texture2d {
    const KIND: Kind = Kind::Texture2d;
}

impl Param for &Sampler {
    const KIND: Kind = Kind::Sampler;
}

/// The code of an entry point, for a device build to find; never called through.
#[derive(Copy, Clone, Debug)]
pub struct Code(pub *const ());

// SAFETY: the pointer is only an identity (which function), never dereferenced or called through,
// so sharing it between threads can't race.
unsafe impl Sync for Code {}

/// An entry point: its descriptor and its function.
#[derive(Copy, Clone, Debug)]
pub struct Entry {
    pub desc: &'static EntryDesc,
    pub code: Code,
}

/// A crate's entry points, the static `gpu::entries!` writes at the crate root as `GPU_ENTRIES`:
/// what a device build of the crate compiles.
#[derive(Copy, Clone, Debug)]
pub struct Entries(pub &'static [Entry]);

impl Entries {
    /// The entry named `name`, if the crate has it.
    pub fn get(&self, name: &str) -> Option<&'static EntryDesc> {
        self.0.iter().find(|e| e.desc.name == name).map(|e| e.desc)
    }
}
