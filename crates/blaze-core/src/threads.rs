//! A [`Driver`] on the real file system: a few threads run blocking calls, and the thread that
//! polls keeps the timers. Portable, and the default on macOS. Two appends to one file in flight
//! at once may land in either order, as with any asynchronous driver; a state machine that
//! needs their order waits for the first.

use std::{
    cmp::Reverse,
    collections::BinaryHeap,
    fs::{File, OpenOptions},
    io::{self, Write as _},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use crate::{Completion, Done, Driver, Effect, IoError, Tag};

pub struct Threads {
    start: Instant,
    work: Option<Sender<Effect>>,
    done: Receiver<Completion>,
    workers: Vec<JoinHandle<()>>,
    timers: BinaryHeap<Reverse<(Instant, Tag)>>,
    in_flight: usize,
}

impl Threads {
    /// A driver whose files are under `root`, with `threads` threads for blocking calls.
    pub fn new(root: impl Into<PathBuf>, threads: usize) -> Threads {
        let root: Arc<Path> = root.into().into();
        let (work, queue) = mpsc::channel::<Effect>();
        let queue = Arc::new(Mutex::new(queue));
        let (finished, done) = mpsc::channel();
        let workers = (0..threads.max(1))
            .map(|n| {
                let (root, queue, finished) = (root.clone(), queue.clone(), finished.clone());
                std::thread::Builder::new()
                    .name(format!("blaze-io-{n}"))
                    .spawn(move || work_on(&root, &queue, &finished))
                    .expect("starting an IO thread")
            })
            .collect();
        Threads {
            start: Instant::now(),
            work: Some(work),
            done,
            workers,
            timers: BinaryHeap::new(),
            in_flight: 0,
        }
    }

    fn fire_timers(&mut self, out: &mut Vec<Completion>) -> usize {
        let now = Instant::now();
        let mut fired = 0;
        while let Some(Reverse((at, tag))) = self.timers.peek().copied() {
            if at > now {
                break;
            }
            self.timers.pop();
            self.in_flight -= 1;
            out.push(Completion { tag, done: Done::Fired });
            fired += 1;
        }
        fired
    }
}

impl Drop for Threads {
    fn drop(&mut self) {
        self.work = None;
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

fn work_on(root: &Path, queue: &Mutex<Receiver<Effect>>, finished: &Sender<Completion>) {
    loop {
        let next = queue.lock().map(|it| it.recv());
        let Ok(Ok(effect)) = next else { return };
        let tag = effect.tag();
        let done =
            run(root, effect).unwrap_or_else(|error| Done::Failed(IoError(error.to_string())));
        if finished.send(Completion { tag, done }).is_err() {
            return;
        }
    }
}

fn run(root: &Path, effect: Effect) -> io::Result<Done> {
    match effect {
        Effect::ReadAll { file, .. } => match std::fs::read(root.join(file.as_str())) {
            Ok(bytes) => Ok(Done::Read(bytes)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Done::Read(Vec::new())),
            Err(error) => Err(error),
        },
        Effect::Read { file, at, len, .. } => {
            let file = File::open(root.join(file.as_str()))?;
            let mut bytes = vec![0; len];
            let mut read = 0;
            while read < len {
                let n = read_at(&file, &mut bytes[read..], at + read as u64)?;
                if n == 0 {
                    break;
                }
                read += n;
            }
            bytes.truncate(read);
            Ok(Done::Read(bytes))
        }
        Effect::Append { file, bytes, .. } => {
            let path = root.join(file.as_str());
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            OpenOptions::new().create(true).append(true).open(path)?.write_all(&bytes)?;
            Ok(Done::Appended)
        }
        Effect::Sync { file, .. } => {
            // `sync_all` is `F_FULLFSYNC` on Apple's systems, where `fsync` leaves the bytes in
            // the drive's cache.
            OpenOptions::new()
                .append(true)
                .create(true)
                .open(root.join(file.as_str()))?
                .sync_all()?;
            Ok(Done::Synced)
        }
        Effect::Truncate { file, len, .. } => {
            let path = root.join(file.as_str());
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            OpenOptions::new().write(true).create(true).truncate(false).open(path)?.set_len(len)?;
            Ok(Done::Truncated)
        }
        Effect::Timer { .. } => unreachable!("timers are kept by the polling thread"),
    }
}

#[cfg(unix)]
fn read_at(file: &File, buffer: &mut [u8], at: u64) -> io::Result<usize> {
    std::os::unix::fs::FileExt::read_at(file, buffer, at)
}

#[cfg(windows)]
fn read_at(file: &File, buffer: &mut [u8], at: u64) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_read(file, buffer, at)
}

impl Driver for Threads {
    fn submit(&mut self, effects: &mut Vec<Effect>) {
        for effect in effects.drain(..) {
            self.in_flight += 1;
            match effect {
                Effect::Timer { after, tag } => {
                    self.timers.push(Reverse((Instant::now() + after, tag)))
                }
                effect => {
                    let work = self.work.as_ref().expect("the driver is running");
                    work.send(effect).expect("the IO threads are running");
                }
            }
        }
    }

    fn poll(&mut self, out: &mut Vec<Completion>, wait: Duration) -> usize {
        let mut count = self.fire_timers(out);
        if count == 0 {
            let until_timer = self
                .timers
                .peek()
                .map(|Reverse((at, _))| at.saturating_duration_since(Instant::now()));
            let wait = until_timer.map_or(wait, |it| it.min(wait));
            match self.done.recv_timeout(wait) {
                Ok(completion) => {
                    self.in_flight -= 1;
                    out.push(completion);
                    count += 1;
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return count,
            }
            count += self.fire_timers(out);
        }
        while let Ok(completion) = self.done.try_recv() {
            self.in_flight -= 1;
            out.push(completion);
            count += 1;
        }
        count
    }

    fn in_flight(&self) -> usize {
        self.in_flight
    }

    fn now(&self) -> Duration {
        self.start.elapsed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FileName, run_until_idle};

    #[test]
    fn appends_sync_and_read_back() {
        let dir = std::env::temp_dir().join(format!("blaze-threads-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut driver = Threads::new(&dir, 2);
        let file = FileName::new("vfs/journal").unwrap();
        let mut effects =
            vec![Effect::Append { file: file.clone(), bytes: b"one".to_vec(), tag: Tag(1) }];
        let mut seen = Vec::new();
        run_until_idle(&mut driver, &mut effects, |done, effects| {
            seen.push(done.done.clone());
            match done.tag {
                Tag(1) => effects.push(Effect::Sync { file: file.clone(), tag: Tag(2) }),
                Tag(2) => {
                    effects.push(Effect::Timer { after: Duration::from_millis(1), tag: Tag(3) })
                }
                Tag(3) => {
                    effects.push(Effect::Read { file: file.clone(), at: 1, len: 10, tag: Tag(4) })
                }
                _ => {}
            }
        });
        assert_eq!(seen, [Done::Appended, Done::Synced, Done::Fired, Done::Read(b"ne".to_vec())]);
        let mut effects =
            vec![Effect::ReadAll { file: FileName::new("missing").unwrap(), tag: Tag(5) }];
        run_until_idle(&mut driver, &mut effects, |done, _| {
            assert_eq!(done.done, Done::Read(Vec::new()))
        });
        drop(driver);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
