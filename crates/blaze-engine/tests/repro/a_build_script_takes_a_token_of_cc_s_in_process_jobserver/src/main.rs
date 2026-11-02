include!("jobs.rs");

fn main() {
    println!("script: {}", env!("SCRIPT_SAID"));
    println!("main: {}", report("18"));
}
