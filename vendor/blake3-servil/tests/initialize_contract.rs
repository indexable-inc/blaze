// The contract's check, in a process of its own: a call before initialize()
// panics, in every build, saying what the caller must do. (Its first line
// names initialize, so tools/call_initialize.py leaves it as it is.)

#[test]
#[should_panic(expected = "call blake3_servil::initialize() once, before any other call")]
fn a_call_before_initialize_panics() {
    // Deliberately without initialize(): this test is the contract's check.
    blake3_servil::convenience::hash(b"");
}
