//! `BLAZE_CODEGEN_CODE`: the code identity of the code generator this library runs, which its
//! templates are keyed by (`blaze_codegen_code`).

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    blaze_codegen_code::emit();
}
