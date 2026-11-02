// The contract's check, in a process of its own: a multithreaded call
// after initialize() alone panics, in every build, saying what the caller
// must do. (Its first line names initialize, so tools/call_initialize.py
// leaves it as it is.)

#[test]
#[should_panic(expected = "call blake3_servil::initialize_multithreaded(None) once, before any multithreaded call or stream")]
fn a_multithreaded_call_before_initialize_multithreaded_panics() {
    blake3_servil::initialize();
    // Deliberately without initialize_multithreaded(None): this test is the contract's check.
    blake3_servil::performance::hash_multithreaded(blake3_servil::Mode::Plain, &[], 0);
}
