fn main() {
    std::panic::set_hook(Box::new(|_| {}));
    println!("outside {}", klib::outside());
    println!("inside {}", klib::inside());
    println!("generated {}", klib::generated());
    println!("script_env {}", klib::script_env());
    println!("script_cfg {}", klib::script_cfg());
    println!("feature {}", klib::feature());
    println!("process_env {}", klib::process_env());
    println!("config_env {}", klib::config_env());
    println!("flag_cfg {}", klib::flag_cfg());
    println!("assertions {}", klib::assertions());
    println!("overflows {}", klib::overflows(255));
    println!("scaled {}", klib::scaled(5));
    println!("shape {}", klib::shape());
    println!("described {}", klib::described());
    println!("identity {}", klib::identity());
    println!("checks of the library {:?}", klib::checks(255));
    println!("checks of the program {:?}", checks(255));
}

/// Which of the sums of the dependency panic when they overflow, called from here: the
/// generic ones with other types than the library calls them with, so that these are not
/// the library's copies, and then one with the library's types, which without optimisation
/// is the library's copy.
fn checks(x: u8) -> [bool; 6] {
    let caught = |f: &dyn Fn() -> u8| std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).is_err();
    [
        caught(&|| kdep::stepped(x)),
        caught(&|| kdep::sum(u16::from(x) << 8 | 255, std::hint::black_box(1)) as u8),
        caught(&|| kdep::widened(x != 0)),
        caught(&|| kdep::bumped(x)),
        caught(&|| kdep::small(std::hint::black_box(x))),
        caught(&|| kdep::sum(x, std::hint::black_box(1))),
    ]
}
