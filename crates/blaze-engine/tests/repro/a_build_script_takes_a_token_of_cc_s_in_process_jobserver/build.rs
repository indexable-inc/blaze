include!("src/jobs.rs");

fn main() {
    // As cc's `cargo_env_var_os("NUM_JOBS")`, which cargo and blaze set for a build script.
    let jobs = std::env::var_os("NUM_JOBS").and_then(|it| it.to_str().map(str::to_owned));
    println!("cargo:rustc-env=SCRIPT_SAID={}", report(jobs.as_deref().unwrap_or("missing")));
}
