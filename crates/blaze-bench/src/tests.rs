use super::*;

fn args(line: &str) -> Vec<String> {
    line.split_whitespace().map(str::to_owned).collect()
}

fn options(noise_threshold: f64, filter_outliers: bool) -> Options {
    Options {
        budget: Budget::Samples(1),
        filter: String::new(),
        noise_threshold,
        filter_outliers,
        seed: None,
    }
}

#[test]
fn ci_and_query_hit_command_lines_parse() {
    let Ok(Mode::Compare(base, it)) =
        Mode::parse(&args("--bench --color never compare /tmp/base -t 4 --noise-threshold 5 -o"))
    else {
        panic!("a comparison");
    };
    assert_eq!(base, PathBuf::from("/tmp/base"));
    assert!(matches!(it.budget, Budget::Time(time) if time == Duration::from_secs(4)));
    assert_eq!(it.noise_threshold, 5.0);
    assert!(it.filter_outliers);
    assert!(matches!(Mode::parse(&args("--bench solo -t 8")), Ok(Mode::Solo(_))));
    assert!(Mode::parse(&args("solo -t 1 -s 3")).is_err());
    assert!(Mode::parse(&args("compare")).is_err());
    assert!(Mode::parse(&args("--bench")).is_err());
}

#[test]
fn glob_stars_and_marks() {
    assert!(glob("", "db/hit/sequential"));
    assert!(glob("db/*", "db/hit/sequential"));
    assert!(glob("*/hit/*", "other/hit/scattered"));
    assert!(glob("store/?et_hit", "store/get_hit"));
    assert!(!glob("db/*", "other/hit/sequential"));
    assert!(!glob("store/get", "store/get_hit"));
}

#[test]
fn summary_is_the_sample_variance() {
    let it = Summary::of(&[2.0, 4.0, 4.0, 4.0, 5.0, 5.0, 7.0, 9.0]).unwrap();
    assert_eq!((it.n, it.min, it.max, it.mean), (8, 2.0, 9.0, 5.0));
    assert!((it.variance - 32.0 / 7.0).abs() < 1e-12);
    assert_eq!(Summary::of(&[]), None);
}

#[test]
fn a_steady_slowdown_is_significant_and_noise_is_not() {
    let slower: Vec<(u64, u64)> = (0..100).map(|i| (1_000 + i % 7, 1_100 + i % 5)).collect();
    let it = Paired::of(&slower, 1, &options(1.0, false)).unwrap();
    assert!(it.significant && it.percent > 9.0, "{it:?}");
    let it = Paired::of(&slower, 1, &options(20.0, false)).unwrap();
    assert!(!it.significant, "below the noise threshold: {it:?}");
    let even: Vec<(u64, u64)> =
        (0..100).map(|i| if i % 2 == 0 { (1_000, 1_010) } else { (1_010, 1_000) }).collect();
    assert!(!Paired::of(&even, 1, &options(0.0, false)).unwrap().significant);
}

#[test]
fn outliers_outside_the_fences_are_dropped() {
    let mut pairs: Vec<(u64, u64)> = (0..40).map(|i| (1_000, 1_000 + i % 3)).collect();
    pairs.push((1_000, 1_000_000));
    let kept = Paired::of(&pairs, 1, &options(1.0, true)).unwrap();
    let all = Paired::of(&pairs, 1, &options(1.0, false)).unwrap();
    assert!(kept.candidate_mean < 1_010.0 && all.candidate_mean > 20_000.0, "{kept:?} {all:?}");
    assert_eq!(fences(vec![1.0, 2.0]), None);
}

#[test]
fn human_times() {
    assert_eq!(Human(0.0).to_string(), "0 ns");
    assert_eq!(Human(120.0).to_string(), "120.0 ns");
    assert_eq!(Human(1_500.0).to_string(), "1.5 us");
    assert_eq!(format!("{:>8}", Human(2.5e6)), "  2.5 ms");
}
