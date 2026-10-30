//! Statics with `#[link_section]`, which on wasm are custom sections of the module: two in
//! one section (the linker joins them, as it does those of every object), one in a section
//! of its own that nothing refers to, and one the code reads, which is in memory too; and two
//! in `.init_array`, which are no custom section but init functions; and one holding two, and
//! one of which the code reads the address.

#[unsafe(link_section = "blaze_joined")]
pub static FIRST: [u8; 5] = *b"hello";

#[unsafe(link_section = "blaze_joined")]
pub static SECOND: [u8; 3] = [1, 2, 3];

#[unsafe(link_section = "blaze_alone")]
#[used]
static ALONE: u32 = 0x0102_0304;

#[unsafe(link_section = "blaze_read")]
pub static READ: (u16, [u8; 2]) = (0xbeef, *b"ok");

pub fn read() -> u32 {
    u32::from(READ.0) + u32::from(READ.1[0]) + u32::from(FIRST[4])
}

/// What `ctor` puts on wasm: a function pointer in `.init_array`, which is no custom section but
/// an init function of the module, called from `__wasm_call_ctors`; one of the default priority
/// and one of priority 101.
#[unsafe(link_section = ".init_array")]
#[used]
static INIT: extern "C" fn() = init;

#[unsafe(link_section = ".init_array.101")]
#[used]
static EARLY: extern "C" fn() = init;

/// A static holds any number of them: two here, and its address is read, so it is data of the
/// module as well.
#[unsafe(link_section = ".init_array.102")]
#[used]
static BOTH: [extern "C" fn(); 2] = [init, init];

pub fn init_array_address() -> *const [extern "C" fn(); 2] {
    &BOTH
}

extern "C" fn init() {}
