//! The screen's properties, on drawn builds: a plan is a terminal's size and a sequence of
//! steps (the clock moving, the build's model with any number of units under way, lines that
//! stay, the terminal resized, tests starting and ending, the build's end), run through the
//! core and its encoder into the reference terminal ([`vt::Vt`]) with no real terminal. Hegel
//! draws the plan and shrinks a failing one; each control plants one bug in the core
//! ([`Plant`]) and expects the oracle to catch it at some seed.

mod vt;

use blaze_protocol::now::{Blocker, Now, Phase, Running, Span, Tally, Task};
use blaze_report::terminal::Terminal;
use blaze_screen::{
    BinaryEnd, Built, Caps, Command, Event, Frame, Line, Options, Plant, Screen, Test, Verdict, Why,
    encode,
};
use hegel::generators as gs;

/// Where a plan's choices come from: Hegel's draws, or a seed's.
trait Draws {
    /// A number below `n`.
    fn below(&mut self, n: u32) -> u32;
}

struct Drawn(hegel::TestCase);

impl Draws for Drawn {
    fn below(&mut self, n: u32) -> u32 {
        self.0.draw(gs::integers::<u32>().max_value(n.saturating_sub(1)))
    }
}

/// SplitMix64: a control's plans, one a seed.
struct Seeded(u64);

impl Draws for Seeded {
    fn below(&mut self, n: u32) -> u32 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        ((z ^ (z >> 31)) % u64::from(n.max(1))) as u32
    }
}

#[derive(Clone, Debug)]
enum Step {
    Tick(u64),
    Event(Event),
}

#[derive(Clone, Debug)]
struct Plan {
    cols: u16,
    rows: u16,
    command: Command,
    steps: Vec<Step>,
}

fn name(draws: &mut impl Draws, most: u32) -> String {
    let len = 1 + draws.below(most);
    (0..len).map(|at| (b'a' + ((at * 7 + len) % 26) as u8) as char).collect()
}

fn model(draws: &mut impl Draws) -> Now {
    let running = draws.below(40);
    let total = running + draws.below(400);
    let mut now = Now {
        phase: [Phase::Starting, Phase::Compiling, Phase::Linking, Phase::Planning][draws.below(4) as usize],
        workers: 16,
        tally: Tally { running, total, done: draws.below(total + 1), ..Tally::default() },
        quiet_ms: u64::from(draws.below(30_000)),
        ..Now::default()
    };
    if draws.below(4) == 0 {
        now.stage = name(draws, 120);
    }
    if draws.below(6) == 0 {
        now.memory.held_back = 1 + draws.below(4);
        now.memory.over = Some(name(draws, 20));
    }
    for id in 0..u64::from(running) {
        let kind = ["names", "bodies", "codegen", "link", "script-run"][draws.below(5) as usize];
        // Two tasks of one unit now and then: they are one row.
        let unit = match draws.below(5) {
            0 => format!("p{} 1.0.0 lib (host)", id.saturating_sub(1)),
            1 => format!("{} 0.1.0 build-script (host)", name(draws, 60)),
            _ => format!("{} 1.2.3 lib (host)", name(draws, 30)),
        };
        let span = (draws.below(2) == 0).then(|| Span {
            name: "infer".to_owned(),
            item: (draws.below(2) == 0).then(|| format!("{}::<impl#0 in macro call 3ca1.0 in macro call 3ca1.0>::{}", name(draws, 80), name(draws, 12))),
            krate: None,
            awaits: (draws.below(4) == 0).then(|| name(draws, 90)),
            for_ms: u64::from(draws.below(9000)),
        });
        let task = Task { id, kind: kind.to_owned(), name: unit };
        if draws.below(2) == 0 {
            now.blockers.push(Blocker {
                task: task.clone(),
                running: true,
                for_ms: 0,
                span: None,
                holds: draws.below(500),
                chain: 0,
                behind: Vec::new(),
            });
        }
        now.running.push(Running {
            task,
            for_ms: u64::from(draws.below(600_000)),
            thread: None,
            span,
            path: Vec::new(),
        });
    }
    now
}

fn plan(draws: &mut impl Draws) -> Plan {
    let cols = 4 + draws.below(220) as u16;
    let rows = 2 + draws.below(70) as u16;
    let testing = draws.below(3) == 0;
    let mut steps = Vec::new();
    let mut binaries = 0;
    for _ in 0..draws.below(40) {
        let step = match draws.below(10) {
            0 | 1 => Step::Tick(u64::from(draws.below(5000))),
            2..=4 => Step::Event(Event::Now(Box::new(model(draws)))),
            5 => {
                let lines: Vec<String> =
                    (0..1 + draws.below(4)).map(|_| name(draws, u32::from(cols) * 2)).collect();
                Step::Event(Event::Said(lines.join("\n") + "\n"))
            }
            6 if draws.below(3) == 0 => {
                Step::Event(Event::Waits((draws.below(3) != 0).then(|| name(draws, 300))))
            }
            6 => Step::Event(Event::Diagnosed { error: draws.below(2) == 0, unit: Some(name(draws, 20)) }),
            7 => Step::Event(Event::Resized {
                cols: 4 + draws.below(220) as u16,
                rows: 2 + draws.below(70) as u16,
            }),
            _ if testing => {
                binaries += 1;
                let binary = binaries;
                steps.push(Step::Event(Event::Test(Test::Queued {
                    binary,
                    label: name(draws, 50),
                    rerun: "blaze test".to_owned(),
                })));
                steps.push(Step::Event(Event::Test(Test::Started { binary })));
                steps.push(Step::Event(Event::Test(Test::Total { binary, tests: 3 })));
                steps.push(Step::Event(Event::Test(Test::TestStarted { binary, name: name(draws, 90) })));
                steps.push(Step::Tick(u64::from(draws.below(4000))));
                let verdict = [Verdict::Passed, Verdict::Failed, Verdict::Ignored][draws.below(3) as usize];
                Step::Event(Event::Test(Test::TestEnded {
                    binary,
                    name: name(draws, 90),
                    verdict,
                    took_ms: Some(3),
                    output: "left: 1\nright: 2\n".to_owned(),
                }))
            }
            _ => Step::Event(Event::Unit { name: name(draws, 30), fresh: draws.below(2) == 0 }),
        };
        steps.push(step);
    }
    // The end: the build's, then the tests' or the program's.
    let ok = draws.below(3) != 0;
    steps.push(Step::Event(Event::Built(Built {
        ok,
        compiled: u64::from(draws.below(3)),
        cached: 30,
        tier: Some("tier 0".to_owned()),
        why_follows: ok,
        tests_follow: testing,
        program: Some(format!("/w/target/debug/{}", name(draws, 200))),
        ..Built::default()
    })));
    if ok {
        steps.push(Step::Event(Event::Why(vec![Why {
            wasted: draws.below(2) == 0,
            cause: blaze_report::say!(blaze_report::say::At("/w/src/main.rs"), " changed"),
            headline: blaze_report::say!(
                "recomputed ",
                blaze_report::say::Count(2),
                " steps because ",
                blaze_report::say::At("/w/src/main.rs"),
                " changed; run ",
                blaze_report::say::Cmd("blaze why"),
            ),
            functions: vec!["hello::add".to_owned()],
            more: Some("blaze explain why:input".to_owned()),
        }])));
    }
    for binary in 1..=binaries {
        if draws.below(2) == 0 {
            steps.push(Step::Event(Event::Test(Test::Ended {
                binary,
                end: BinaryEnd::Failed { status: "exit status: 101".to_owned() },
                output: String::new(),
            })));
        }
    }
    steps.push(Step::Event(match testing {
        true => Event::Test(Test::Over),
        false => Event::Clear,
    }));
    let command = match testing {
        true => Command::Test,
        false => [Command::Build, Command::Run, Command::Check][draws.below(3) as usize],
    };
    Plan { cols, rows, command, steps }
}

/// A terminal with colour, links, a progress report and notifications: every sequence the
/// encoder has is written.
fn terminal() -> Terminal {
    let var = |name: &str| match name {
        "TERM_PROGRAM" => Some("ghostty".to_owned()),
        "TERM_PROGRAM_VERSION" => Some("1.2.0".to_owned()),
        _ => None,
    };
    blaze_report::terminal::detect(&blaze_report::terminal::Config::DEFAULT, var, true, Some(80))
}

fn texts(lines: &[Line]) -> Vec<String> {
    lines.iter().map(Line::text).collect()
}

/// The oracle. `plan` on a terminal, with `plant` in the core:
///
/// - the region is never taller than the terminal less its last row, and no row of it is
///   wider than the terminal less its last column;
/// - after every frame the terminal holds exactly the lines that stay, in order, then the
///   region: no row of an earlier region is left, across resizes too;
/// - after the last frame it holds the lines that stay and nothing else, its cursor shown and
///   its progress report cleared.
///
/// One thing a terminal does that no program can undo is left out by name: a resize that
/// wraps the region to more rows than the terminal has pushes rows of it into the scrollback,
/// where no cursor reaches. From such a resize on, only what is still on screen is checked.
fn on_a_terminal(plan: &Plan, plant: Plant) -> Result<(), String> {
    let caps = Caps { live: true, cols: plan.cols, rows: plan.rows, unicode: true };
    let mut screen = Screen::new(Options::of(plan.command), caps).planted(plant);
    let mut vt = vt::Vt::new(plan.cols, plan.rows);
    let terminal = terminal();
    let (mut cols, mut rows) = (plan.cols, plan.rows);
    let mut now = 0;
    let mut stay: Vec<String> = Vec::new();
    let mut region: Vec<String> = Vec::new();
    let mut lost = false;
    let mut bytes = String::new();
    for (at, step) in plan.steps.iter().enumerate() {
        match step {
            Step::Tick(ms) => now += ms,
            Step::Event(event) => {
                if let Event::Resized { cols: to_cols, rows: to_rows } = event {
                    (cols, rows) = (*to_cols, *to_rows);
                    vt.resize(cols, rows);
                    let wrapped: usize =
                        region.iter().map(|it| it.chars().count().div_ceil(usize::from(cols)).max(1)).sum();
                    lost |= wrapped >= usize::from(rows);
                }
                screen.take(event.clone());
            }
        }
        screen.advance_clock(now);
        let Some(frame) = screen.frame() else { continue };
        if frame.region.len() > usize::from(rows).saturating_sub(1) {
            return Err(format!(
                "step {at}: a region of {} rows on a terminal of {rows}",
                frame.region.len()
            ));
        }
        if let Some(wide) = frame.region.iter().find(|it| it.width() > usize::from(cols).saturating_sub(1)) {
            return Err(format!(
                "step {at}: a row {} columns wide on a terminal of {cols}: {:?}",
                wide.width(),
                wide.text()
            ));
        }
        bytes.clear();
        encode(&frame, &terminal, &mut bytes);
        vt.write(&bytes);
        stay.extend(texts(&frame.above));
        region = texts(&frame.region);
        if lost {
            // What scrolled away stays there, and a row of it may be the start of the line
            // the region's first row now ends; what the terminal still shows ends with the
            // region, whole.
            let held = vt.held();
            let tail = &held[held.len().saturating_sub(region.len())..];
            let ends = tail.len() == region.len()
                && tail.iter().zip(&region).all(|(held, row)| held.trim_end().ends_with(row.trim_end()));
            if !ends {
                return Err(format!("step {at}: the terminal ends with {tail:#?}, the region is {region:#?}"));
            }
            continue;
        }
        let want: Vec<&str> = stay.iter().chain(&region).map(|it| it.trim_end()).collect();
        // A blank line that stays at the very end is not told from a row erased.
        let want = &want[..want.iter().rposition(|it| !it.is_empty()).map_or(0, |at| at + 1)];
        if vt.held().iter().map(|it| it.trim_end()).ne(want.iter().copied()) {
            return Err(format!(
                "step {at}: the terminal ({cols} by {rows}) holds {:#?}\nand should hold {want:#?}",
                vt.held()
            ));
        }
    }
    if !region.is_empty() {
        return Err(format!("the last frame left a region: {region:#?}"));
    }
    if vt.cursor_hidden {
        return Err("the cursor is left hidden".to_owned());
    }
    if vt.synchronized {
        return Err("a synchronized update is left open".to_owned());
    }
    match vt.progress() {
        None | Some("0;0") => Ok(()),
        Some(left) => Err(format!("the terminal's progress report is left at {left}")),
    }
}

/// The oracle off a terminal: `plan` to a pipe writes no escape byte, no carriage return, and
/// only whole lines.
fn to_a_pipe(plan: &Plan, plant: Plant) -> Result<(), String> {
    let mut screen = Screen::new(Options::of(plan.command), Caps::PIPE).planted(plant);
    let mut bytes = String::new();
    let mut now = 0;
    for step in &plan.steps {
        match step {
            Step::Tick(ms) => now += ms,
            // A pipe has no size.
            Step::Event(Event::Resized { .. }) => {}
            Step::Event(event) => screen.take(event.clone()),
        }
        screen.advance_clock(now);
        if let Some(frame) = screen.frame() {
            encode(&frame, &Terminal::PIPE, &mut bytes);
        }
    }
    if let Some(at) = bytes.find(['\x1b', '\r', '\x07']) {
        let from = bytes[..at].rfind('\n').map_or(0, |it| it + 1);
        return Err(format!("a pipe was written an escape or a carriage return: {:?}", &bytes[from..at + 1]));
    }
    if !bytes.is_empty() && !bytes.ends_with('\n') {
        return Err("a pipe was written a line with no end".to_owned());
    }
    Ok(())
}

/// The invariant: on a terminal of any size the live region fits (never taller than the
/// terminal less a row, never wider than it less a column), every frame leaves exactly the
/// lines that stay and the region with no stale row, a resize in the middle included, and the
/// last frame leaves the lines that stay alone, the cursor shown and the progress report
/// cleared.
#[hegel::test(test_cases = 300)]
fn the_region_fits_and_leaves_nothing_behind(tc: hegel::TestCase) {
    let plan = plan(&mut Drawn(tc));
    if let Err(why) = on_a_terminal(&plan, Plant::None) {
        panic!("{why}\n{plan:#?}");
    }
}

/// The invariant: what is written to a pipe holds no escape byte and no carriage return.
#[hegel::test(test_cases = 300)]
fn a_pipe_is_written_no_escape(tc: hegel::TestCase) {
    let plan = plan(&mut Drawn(tc));
    if let Err(why) = to_a_pipe(&plan, Plant::None) {
        panic!("{why}\n{plan:#?}");
    }
}

/// The seeds a control tries.
const CONTROL_SEEDS: u64 = 400;

/// `plant` is caught by `oracle` at some seed, with what it said; no plant is caught at any.
fn control(plant: Plant, oracle: fn(&Plan, Plant) -> Result<(), String>, says: &str) {
    for seed in 0..CONTROL_SEEDS {
        let plan = plan(&mut Seeded(seed));
        if let Err(why) = oracle(&plan, Plant::None) {
            panic!("seed {seed} fails with no plant: {why}");
        }
        if let Err(why) = oracle(&plan, plant) {
            assert!(why.contains(says), "Plant::{plant:?} was caught at seed {seed}, as something else: {why}");
            return;
        }
    }
    panic!("the oracle caught Plant::{plant:?} at none of seeds 0..{CONTROL_SEEDS}: it, or the draws, are too weak");
}

#[test]
fn the_oracle_catches_a_region_taller_than_the_terminal() {
    control(Plant::Tall, on_a_terminal, "a region of");
}

#[test]
fn the_oracle_catches_a_row_wider_than_the_terminal() {
    control(Plant::Wide, on_a_terminal, "columns wide");
}

#[test]
fn the_oracle_catches_a_region_left_at_the_end() {
    control(Plant::KeepRegion, on_a_terminal, "holds");
}

#[test]
fn the_oracle_catches_rows_left_by_a_resize() {
    control(Plant::StaleResize, on_a_terminal, "the terminal");
}

#[test]
fn the_oracle_catches_a_terminals_frame_sent_to_a_pipe() {
    control(Plant::LiveWhenPiped, to_a_pipe, "escape");
}

/// What puts a terminal back when the command is ended from outside: the region's rows
/// erased, the cursor shown, the progress report cleared, in one write.
#[test]
fn an_interrupt_puts_the_terminal_back() {
    let caps = Caps { live: true, cols: 80, rows: 24, unicode: true };
    let mut screen = Screen::new(Options::of(Command::Build), caps);
    let mut vt = vt::Vt::new(80, 24);
    let terminal = terminal();
    screen.take(Event::Said("warning: unused\n".to_owned()));
    screen.take(Event::Now(Box::new(model(&mut Seeded(7)))));
    let frame = screen.frame().expect("a frame is due");
    let rows = frame.region.len() as u16;
    assert!(rows > 0);
    let mut bytes = String::new();
    encode(&frame, &terminal, &mut bytes);
    vt.write(&bytes);
    assert!(vt.cursor_hidden);
    bytes.clear();
    encode(&Frame::restore(rows), &terminal, &mut bytes);
    vt.write(&bytes);
    assert_eq!(vt.held(), ["warning: unused"]);
    assert_eq!(vt.shown(), "warning: unused");
    assert!(!vt.cursor_hidden);
    assert_eq!(vt.progress(), Some("0;0"));
}

/// A build's end is a notification only when the build was long: a failure that took a
/// moment is on screen already.
#[test]
fn only_a_long_build_ends_with_a_notification() {
    let notified = |took_ms: u64| {
        let caps = Caps { live: true, cols: 80, rows: 24, unicode: true };
        let mut screen = Screen::new(Options::of(Command::Build), caps);
        screen.advance_clock(took_ms);
        screen.take(Event::Built(Built { ok: false, ..Built::default() }));
        let mut bytes = String::new();
        encode(&screen.frame().expect("the build's line is due"), &terminal(), &mut bytes);
        let mut vt = vt::Vt::new(80, 24);
        vt.write(&bytes);
        vt.osc.iter().any(|it| it.starts_with("777;notify;"))
    };
    assert!(!notified(5));
    assert!(notified(11_000));
}

/// What the terminal shows after a program `blaze run` started wrote `before` and ended as
/// `ran`.
fn footer(before: &str, ran: blaze_screen::Ran) -> String {
    let caps = Caps { live: true, cols: 80, rows: 10, unicode: true };
    let mut screen = Screen::new(Options::of(Command::Run), caps);
    let mut vt = vt::Vt::new(80, 10);
    vt.write(before);
    screen.take(Event::Ran(ran));
    if let Some(frame) = screen.frame() {
        let mut bytes = String::new();
        encode(&frame, &terminal(), &mut bytes);
        vt.write(&bytes);
    }
    vt.shown()
}

/// The footer of a `run`: nothing after a quick clean run; the time and the peak after a long
/// one; the code or the signal always; and on a line of its own when the program left the
/// cursor in the middle of one.
#[test]
fn a_run_ends_with_a_footer_only_when_it_has_something_to_say() {
    use blaze_screen::Ran;
    let clean = |wall_ms| Ran { code: Some(0), wall_ms, peak_bytes: Some(14_200_000), ..Ran::default() };
    assert_eq!(footer("hello 3\n", clean(12)), "hello 3");
    assert_eq!(footer("hello 3\n", clean(2100)), "hello 3\n✓ exit 0 · 2.10s · peak 14.2 MB");
    assert_eq!(
        footer("hello 3\n", Ran { stats: true, cpu_user_ms: Some(9), cpu_sys_ms: Some(2), ..clean(12) }),
        "hello 3\n✓ exit 0 · 12ms · peak 14.2 MB · cpu 9ms user, 2ms system"
    );
    assert_eq!(footer("", Ran { code: Some(3), wall_ms: 40, ..Ran::default() }), "✗ exit 3 · 40ms");
    assert_eq!(
        footer("", Ran { signal: Some("SIGSEGV".to_owned()), core_dumped: true, wall_ms: 40, ..Ran::default() }),
        "✗ killed by SIGSEGV (core dumped) · 40ms"
    );
    // The program's last line has no end: the footer does not run on from it.
    assert_eq!(
        footer("no newline", Ran { code: Some(3), wall_ms: 40, mid_line: true, ..Ran::default() }),
        "no newline%\n✗ exit 3 · 40ms"
    );
    // At the start of a line the same bytes leave nothing.
    assert_eq!(
        footer("a line\n", Ran { code: Some(3), wall_ms: 40, mid_line: true, ..Ran::default() }),
        "a line\n✗ exit 3 · 40ms"
    );
}
