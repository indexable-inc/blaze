include!("levels.rs");

fn main() {
    let out = std::env::var("OUT_DIR").unwrap();
    let level = option_env!("NORUSTC_LEVEL").map_or(LEVEL, |it| it.parse().unwrap());
    let unit = include_str!("unit.txt").trim();
    let said = shout::shout!(level);
    std::fs::write(
        format!("{out}/level.rs"),
        format!("pub const LEVEL: u32 = {level}; // {unit}, {said}\n"),
    )
    .unwrap();
    println!("cargo::rerun-if-changed=build.rs");
}
