use super::{COUNTERS, Descriptor, log};

unsafe extern "C" {
    fn __blaze_exec_slots() -> u32;
    fn __blaze_exec_arm(region: *mut u8, len: usize) -> i32;
    fn blaze_live_counters() -> *mut u64;
}

/// The run side a test binary's recording fork server and tiered code call by name is the dev
/// runtime's own, so a dev executable that depends on neither blaze-live nor blaze-inproc has it
/// (starthook2: `edition` and `stdx` failed to link, naming `__blaze_exec_arm`). This binary
/// links the runtime alone; it is not linked live, so there is no log to arm.
#[test]
fn a_binary_with_only_the_dev_runtime_resolves_the_execution_logs_exports() {
    // SAFETY: the runtime's export, as the fork server declares it, takes nothing.
    let slots = unsafe { __blaze_exec_slots() };
    // SAFETY: as above; with no descriptor nothing is armed, so the null region is never
    // touched.
    let armed = unsafe { __blaze_exec_arm(std::ptr::null_mut(), 0) };
    assert_eq!((slots, armed), (0, log::NO_LOG));
    // SAFETY: the runtime's export takes nothing and returns its counters' address.
    let counters = unsafe { blaze_live_counters() };
    assert!(!counters.is_null() && counters.is_aligned(), "{COUNTERS} counters at {counters:?}");
}

/// The descriptor reads back as the linker writes it, and a short or foreign one is none.
#[test]
fn a_descriptor_round_trips_and_a_short_one_is_none() {
    let descriptor = Descriptor {
        magic: super::DESCRIPTOR_MAGIC,
        version: super::DESCRIPTOR_VERSION,
        arch: super::HOST_ARCH,
        id: [7; 16],
        generation: 3,
        this: 0x1000,
        table: 0x2000,
        slots: 9,
        patch_code: [0x3000, 0x4000],
        patch_data: [0x5000, 0x6000],
    };
    let bytes = descriptor.encode();
    assert_eq!(bytes.len(), Descriptor::SIZE);
    assert_eq!(Descriptor::decode(&bytes), Some(descriptor));
    assert_eq!(Descriptor::decode(&bytes[..Descriptor::SIZE - 1]), None);
    let mut foreign = bytes.clone(); // clone: test
    foreign[0] ^= 1;
    assert_eq!(Descriptor::decode(&foreign), None);
}
