// A build script as most that matter are: it reads one variable by name, and it walks its
// environment, as a script does that starts a compiler or changes a `Command`'s environment.
fn main() {
    let out = std::env::var("OUT_DIR").expect("OUT_DIR");
    let made = std::env::var("MADE_VALUE").unwrap_or_else(|_| "40".to_owned());
    let walked = std::env::vars().count();
    assert!(walked > 0, "a script is started with the variables cargo sets for it");
    std::fs::write(format!("{out}/made.rs"), format!("pub const MADE: u32 = {made};\n")).expect("writing made.rs");
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-env-changed=MADE_VALUE");
}
