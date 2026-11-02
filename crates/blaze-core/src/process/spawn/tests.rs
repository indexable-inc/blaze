use super::busy;
use crate::sim::Rng;

/// [`busy::next`] as a loop: the wait doubled once per try, held at the cap, cut at the bound.
fn reference(tried: u32, waited: u64) -> Option<u64> {
    if waited >= busy::MOST_NS {
        return None;
    }
    let mut wait = busy::FIRST_NS;
    for _ in 0..tried {
        wait = (wait * 2).min(busy::CAP_NS);
    }
    Some(wait.min(busy::MOST_NS - waited))
}

/// Followed from the first try, the waits start at the first, never pass the cap, never get
/// shorter but for the last, and add up to the bound exactly in a few dozen tries.
#[test]
fn busy_waits_add_up_to_the_bound() {
    let (mut tried, mut waited) = (0, 0);
    let mut last = 0;
    while let Some(wait) = busy::next(tried, waited) {
        assert!(wait > 0 && wait <= busy::CAP_NS, "try {tried}: {wait} ns");
        if tried == 0 {
            assert_eq!(wait, busy::FIRST_NS);
        }
        if waited + wait < busy::MOST_NS {
            assert!(wait >= last, "try {tried}: {wait} ns after {last} ns");
        }
        last = wait;
        tried += 1;
        waited += wait;
        assert!(tried <= 40, "{tried} tries and {waited} ns waited");
    }
    assert_eq!(waited, busy::MOST_NS);
    assert!(tried >= 20, "{tried} tries: the cap keeps the tries close together");
}

/// The schedule against its loop, from any point: random tries (past the shift's limit too)
/// and time waited (past the bound too).
#[test]
fn busy_waits_are_the_reference_schedule() {
    for seed in 0..64 {
        let mut rng = Rng::new(seed);
        for _ in 0..1000 {
            let tried = rng.below(80) as u32;
            let waited = match rng.below(4) {
                0 => busy::MOST_NS + rng.below(busy::MOST_NS),
                1 => busy::MOST_NS - rng.below(busy::CAP_NS * 2).min(busy::MOST_NS),
                _ => rng.below(busy::MOST_NS),
            };
            assert_eq!(busy::next(tried, waited), reference(tried, waited), "seed {seed}: {tried} tries, {waited} ns");
        }
    }
}

mod held {
    use std::{
        io::Read as _,
        os::unix::fs::PermissionsExt as _,
        path::{Path, PathBuf},
        time::Duration,
    };

    use crate::{Command, Sandbox};

    /// A program in a directory of the test's own, executable: a file this process wrote, as
    /// the daemon writes a program it built. On Linux a copy of the system's shell; elsewhere a
    /// script of it, since macOS kills a copy of a platform binary as it starts.
    fn written(what: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("blaze-busy-{what}-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let program = dir.join("sh");
        #[cfg(target_os = "linux")]
        std::fs::copy("/bin/sh", &program).unwrap();
        #[cfg(not(target_os = "linux"))]
        std::fs::write(&program, "#!/bin/sh\nexit 7\n").unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        (dir, program)
    }

    fn command(program: &Path) -> Box<Command> {
        Box::new(Command {
            program: program.to_path_buf(),
            args: vec!["-c".into(), "exit 7".into()],
            env: Vec::new(),
            dir: "/".into(),
            stdin: Vec::new(),
            sandbox: Sandbox::Off,
            memory: None,
        })
    }

    /// `program` held open for writing by another thread for `held`, from before the spawn.
    fn holding(program: &Path, held: Duration) -> std::thread::JoinHandle<()> {
        let (opened, open) = crossbeam_channel::bounded(0);
        let program = program.to_path_buf();
        let holder = std::thread::spawn(move || {
            let file = std::fs::OpenOptions::new().write(true).open(&program).unwrap();
            opened.send(()).unwrap();
            blaze_spin::busy(held);
            drop(file);
        });
        open.recv().unwrap();
        holder
    }

    /// A program held open for writing while it is spawned runs once the writer closes it: on
    /// Linux its exec meets `ETXTBSY` (the child's own copy of the writer's descriptor too)
    /// and the spawn is made again; where exec doesn't refuse it, it simply runs.
    #[test]
    fn a_program_held_open_for_writing_runs_once_it_is_closed() {
        let (dir, program) = written("closed");
        let holder = holding(&program, Duration::from_millis(30));
        let started = crate::process::start(Path::new("/"), command(&program), None).unwrap();
        holder.join().unwrap();
        let mut stdout = Vec::new();
        std::fs::File::from(started.stdout).read_to_end(&mut stdout).unwrap();
        let mut stderr = Vec::new();
        std::fs::File::from(started.stderr).read_to_end(&mut stderr).unwrap();
        let exit = started.finish.exited(started.pid, stdout, stderr).unwrap();
        assert_eq!((exit.code, exit.signal), (Some(7), None), "{exit:?}");
        _ = std::fs::remove_dir_all(&dir);
    }

    /// Held past the bound, the spawn fails naming the program and why, after about the
    /// bound, and runs nothing else in its place.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_program_held_open_past_the_bound_fails_naming_it() {
        let (dir, program) = written("spent");
        let holder = holding(&program, Duration::from_millis(1500));
        let began = std::time::Instant::now();
        let error = match crate::process::start(Path::new("/"), command(&program), None) {
            Ok(_) => panic!("{} ran while open for writing", program.display()),
            Err(error) => error,
        };
        let took = began.elapsed();
        holder.join().unwrap();
        let said = error.to_string();
        assert!(said.contains(&program.display().to_string()), "{said}");
        assert!(said.contains("still open for writing"), "{said}");
        assert_eq!(error.kind(), std::io::ErrorKind::ExecutableFileBusy, "{said}");
        let bound = Duration::from_nanos(super::busy::MOST_NS);
        assert!(took >= bound && took < bound + Duration::from_millis(400), "{took:?}");
        _ = std::fs::remove_dir_all(&dir);
    }
}
