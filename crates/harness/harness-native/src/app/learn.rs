//! Learn, the app's home (scene/widgets/learn.flecs): what the main card shows while no session is selected.
//!
//! A person says what they want to learn; a tutor (a chat session of an agent, asked aside so its conversation stays
//! empty) writes the course's outline, then each lesson's steps when the lesson is opened; a lesson is walked one screen
//! at a time; what was learned is remembered and comes due again. The lessons, the memory and the walk are
//! `harness-learn`'s state machines; this file is their host: it draws the screen the state says, turns keys and presses
//! into inputs, asks the tutor, and keeps everything in `$HARNESS_HOME/learn.json`.

use super::*;
use harness_learn::{Course, Input, Lesson, Mark, Progress, Run, Step, tutor};
use harness_native_ui_layout_api::{Size, SizeKind, Sizing};

/// A tutor's answer, or the session it runs in, come from the link's threads.
enum Arrived {
    /// The tutor's session was started.
    Tutor(u64),
    /// The outline asked for in generation `ask`.
    Outline { ask: u64, answer: Result<String, String> },
    /// The steps of `lesson` of `course` asked for in generation `ask`.
    Steps { ask: u64, course: String, lesson: String, answer: Result<String, String> },
}

/// What the tutor is being asked for.
#[derive(Clone)]
enum Want {
    Outline { topic: String },
    Steps { course: String, lesson: String },
}

/// Which screen shows.
enum Screen {
    Home,
    /// A tutor is writing: what, in words.
    Making { title: String, sub: String },
    /// A lesson or a review is walked ([`Learn::run`]).
    Step,
    /// A run is over.
    Done { kicker: String, title: String, score: String, next: String, course: String },
}

/// What is kept between launches.
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct Saved {
    #[serde(default)]
    courses: Vec<Course>,
    #[serde(default)]
    progress: Progress,
    /// The tutor's session, reused while the daemon still has it.
    #[serde(default)]
    tutor: Option<u64>,
}

pub struct Learn {
    /// A shot's fixed state ([`App::shot_learn`]): nothing is read from or written to disk.
    shot: bool,
    loaded: bool,
    saved: Saved,
    screen: Screen,
    run: Option<Run>,
    /// The words typed into the home's field.
    draft: String,
    /// Why the last ask of the tutor failed, shown on the home.
    note: String,
    /// What the tutor is asked for now, and its generation: an answer of an older one (cancelled) is dropped.
    want: Option<Want>,
    ask: u64,
    inbox: Arc<Mutex<Vec<Arrived>>>,
}

impl Default for Learn {
    fn default() -> Self {
        Self {
            shot: false,
            loaded: false,
            saved: Saved::default(),
            screen: Screen::Home,
            run: None,
            draft: String::new(),
            note: String::new(),
            want: None,
            ask: 0,
            inbox: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

fn path() -> std::path::PathBuf {
    crate::link::home().join("learn.json")
}

/// Today, as days since the Unix epoch in local time (so a day turns at the person's midnight).
fn today() -> harness_learn::Day {
    let now = model::now_ms() / 1000;
    // SAFETY: localtime_r writes only the `tm` it is given.
    let off = unsafe {
        let t = now as libc::time_t;
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        tm.tm_gmtoff
    };
    ((now as i64 + off as i64).max(0) / 86_400) as harness_learn::Day
}

impl Learn {
    fn load(&mut self) {
        if self.shot || std::mem::replace(&mut self.loaded, true) {
            return;
        }
        match std::fs::read(path()) {
            Ok(bytes) => match serde_json::from_slice::<Saved>(&bytes) {
                Ok(saved) => self.saved = saved,
                // Refused, never read as empty: the next save would write over what the person learned.
                Err(e) => {
                    self.note = format!("{} is not Learn's saved state ({e}); nothing is saved until it is moved away", path().display());
                    self.loaded = false;
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                self.note = format!("reading {}: {e}", path().display());
                self.loaded = false;
            }
        }
    }

    /// Write the saved state (through a temporary file and a rename). A failure is said on the home.
    fn save(&mut self) {
        if !self.loaded {
            return;
        }
        let p = path();
        let tmp = p.with_extension("json.tmp");
        let done = serde_json::to_vec_pretty(&self.saved)
            .map_err(|e| e.to_string())
            .and_then(|b| std::fs::write(&tmp, b).map_err(|e| e.to_string()))
            .and_then(|()| std::fs::rename(&tmp, &p).map_err(|e| e.to_string()));
        if let Err(e) = done {
            self.note = format!("saving {}: {e}", p.display());
        }
    }

    fn course(&self, id: &str) -> Option<&Course> {
        self.saved.courses.iter().find(|c| c.id == id)
    }

    /// Start walking `lesson` of `course` if its steps are written; whether it did.
    fn start_lesson(&mut self, course: &str, lesson: &str) -> bool {
        let Some(c) = self.course(course) else { return false };
        let Some(run) = c.lesson(lesson).and_then(|l| Run::lesson(c, l, model::now_ms())) else { return false };
        self.run = Some(run);
        self.screen = Screen::Step;
        true
    }

    /// The run in hand ended (its last step was left): the Done screen, and the progress saved.
    fn finish(&mut self) {
        let Some(run) = self.run.take() else { return };
        let Some(sum) = run.summary() else {
            self.run = Some(run);
            return;
        };
        let course = run.course_id().to_owned();
        let c = self.course(&course);
        let lesson = run.lesson_id().and_then(|id| c.and_then(|c| c.lesson(id)));
        let kicker = match lesson {
            Some(l) => l.title.clone(),
            None => "Review".to_owned(),
        };
        let title = match (sum.asked, sum.right_first) {
            (a, r) if a == r => "All of it, first try.".to_owned(),
            (a, r) if r * 2 >= a => "Done. That is learned.".to_owned(),
            _ => "Done. It will come back.".to_owned(),
        };
        let score = format!("{} of {} right the first time.", sum.right_first, sum.asked);
        let more = c.is_some_and(|c| self.saved.progress.next_lesson(c).is_some());
        let next = match (sum.missed.len(), more) {
            (0, true) => "Each of these comes back tomorrow, then further apart as you keep getting it.".to_owned(),
            (0, false) => "That was the course's last lesson. Reviews keep it: each comes back tomorrow, then further apart.".to_owned(),
            (n, _) => format!("{n} you missed {} due again today; the rest tomorrow.", if n == 1 { "is" } else { "are" }),
        };
        self.screen = Screen::Done { kicker, title, score, next, course };
        self.save();
    }
}

/// Typed text as a field takes it: no control characters, no AppKit function-key code points.
fn typed(text: &str) -> String {
    text.chars().filter(|c| !c.is_control() && !('\u{F700}'..='\u{F8FF}').contains(c)).collect()
}

fn percent(v: View<'_>, share: f32) {
    let want = Size { w: Sizing { kind: SizeKind::Percent, value: share.clamp(0.0, 1.0), min: 0.0, max: 0.0 }, h: Sizing::GROW };
    if v.entity().try_cloned::<&Size>() != Some(want) {
        v.set(want);
    }
}

/// One keyed child of `slot`: the primary button saying `label` that does `action`, or its faint stand-in when there
/// is nothing to do (`action` empty).
fn button(slot: View<'_>, label: &str, action: &str) {
    let (key, prefab) = if action.is_empty() { ("off", "hn.LearnButtonOff") } else { ("on", "hn.LearnButton") };
    view::for_each_of(slot, [(key, prefab, ())], |v, (), _| {
        if let Some(l) = v.part("label") {
            l.text(label);
        }
        if !action.is_empty() {
            v.action(action);
        }
    });
}

impl App {
    /// Whether Learn has the keyboard: the focused pane shows no object and nothing is open over it.
    pub(super) fn learn_focused(&self) -> bool {
        self.palette.is_none()
            && !self.tree_focus
            && self.focused_pane().is_some_and(|p| model::shows(self.world.world_ref(), p).is_none())
    }

    /// Take what the link's threads brought since the last frame.
    fn learn_arrivals(&self) {
        let mut l = self.learn.borrow_mut();
        let arrived: Vec<Arrived> = std::mem::take(&mut *l.inbox.lock().unwrap_or_else(|e| e.into_inner()));
        for a in arrived {
            match a {
                Arrived::Tutor(id) => {
                    l.saved.tutor = Some(id);
                    l.save();
                    self.link.rename(id, "Tutor");
                    self.learn_ask(&mut l);
                }
                Arrived::Outline { ask, answer } if ask == l.ask => {
                    l.want = None;
                    match answer.map_err(|e| e.to_string()).and_then(|t| Course::parse(&t).map_err(|e| e.to_string())) {
                        Ok(mut course) => {
                            // A second course on the same topic keeps its own progress.
                            let base = course.id.clone();
                            let mut n = 2;
                            while l.course(&course.id).is_some() {
                                course.id = format!("{base}-{n}");
                                n += 1;
                            }
                            let (cid, first) = (course.id.clone(), course.lessons[0].id.clone());
                            l.saved.courses.insert(0, course);
                            l.draft.clear();
                            l.save();
                            self.learn_open(&mut l, &cid, &first);
                        }
                        Err(e) => {
                            l.note = e;
                            l.screen = Screen::Home;
                        }
                    }
                }
                Arrived::Steps { ask, course, lesson, answer } if ask == l.ask => {
                    l.want = None;
                    let made = answer.and_then(|t| {
                        let c = l.course(&course).ok_or_else(|| format!("course {course:?} is gone"))?;
                        let outline = c.lesson(&lesson).ok_or_else(|| format!("lesson {lesson:?} of course {course:?} is gone"))?;
                        outline.with_steps(&t).map_err(|e| e.to_string())
                    });
                    match made {
                        Ok(written) => {
                            if let Some(slot) = l.saved.courses.iter_mut().find(|c| c.id == course).and_then(|c| c.lessons.iter_mut().find(|x| x.id == lesson)) {
                                *slot = written;
                            }
                            l.save();
                            if !l.start_lesson(&course, &lesson) {
                                l.screen = Screen::Home;
                            }
                        }
                        Err(e) => {
                            l.note = e;
                            l.screen = Screen::Home;
                        }
                    }
                }
                // An answer to an ask the person cancelled.
                Arrived::Outline { .. } | Arrived::Steps { .. } => {}
            }
        }
    }

    /// Ask the tutor for what is wanted, starting its session first when there is none (the ask is sent when the
    /// session arrives).
    fn learn_ask(&self, l: &mut Learn) {
        let Some(want) = l.want.clone() else { return };
        // The daemon still has the tutor's session, and it runs.
        let alive = l.saved.tutor.filter(|&sid| self.entity_of(sid).is_some_and(|e| self.status_of(e) == "running"));
        let Some(tutor) = alive else {
            let inbox = l.inbox.clone();
            self.link.spawn("claude", None, None, None, 100, 30, move |id| {
                inbox.lock().unwrap_or_else(|e| e.into_inner()).push(Arrived::Tutor(id));
            });
            return;
        };
        let (inbox, ask) = (l.inbox.clone(), l.ask);
        match want {
            Want::Outline { topic } => {
                self.link.chat_aside(tutor, &tutor::outline(&topic), move |answer| {
                    inbox.lock().unwrap_or_else(|e| e.into_inner()).push(Arrived::Outline { ask, answer });
                });
            }
            Want::Steps { course, lesson } => {
                let Some(c) = l.course(&course) else { return };
                let Some(outline) = c.lesson(&lesson) else { return };
                let prompt = tutor::steps(c, outline, &l.saved.progress.shaky(c));
                self.link.chat_aside(tutor, &prompt, move |answer| {
                    inbox.lock().unwrap_or_else(|e| e.into_inner()).push(Arrived::Steps { ask, course, lesson, answer });
                });
            }
        }
    }

    /// Open `lesson` of `course`: walk it if it is written, else ask the tutor to write it.
    fn learn_open(&self, l: &mut Learn, course: &str, lesson: &str) {
        l.note.clear();
        if l.start_lesson(course, lesson) {
            return;
        }
        let Some((ct, lt)) = l.course(course).and_then(|c| Some((c.title.clone(), c.lesson(lesson)?.title.clone()))) else { return };
        l.ask += 1;
        l.want = Some(Want::Steps { course: course.to_owned(), lesson: lesson.to_owned() });
        l.screen = Screen::Making { title: format!("Writing \u{201c}{lt}\u{201d}"), sub: format!("{ct} \u{b7} a few short steps, mostly questions. About half a minute.") };
        self.learn_ask(l);
    }

    /// Continue `course` (the home's card): what is due first, as a review, then its next lesson.
    fn learn_continue(&self, l: &mut Learn, course: &str) {
        let Some(c) = l.course(course) else { return };
        let due = l.saved.progress.due(c, today());
        if let Some(run) = Run::review(c, &due, model::now_ms()) {
            l.run = Some(run);
            l.screen = Screen::Step;
            return;
        }
        let next = l.saved.progress.next_lesson(c).map(|x| x.id.clone());
        match next {
            Some(lesson) => self.learn_open(l, course, &lesson),
            None => l.note = format!("\u{201c}{}\u{201d} is finished and nothing is due today.", c.title),
        }
    }

    /// A press in Learn (`learn:<what>`).
    pub(super) fn learn_action(&mut self, what: &str) {
        self.tree_focus = false;
        if what == "show" {
            // The sidebar's house: nothing selected, so the main card shows Learn.
            self.select(None);
            return;
        }
        {
            let me = &*self;
            let mut l = me.learn.borrow_mut();
            let l = &mut *l;
            let (name, arg) = what.split_once(':').unwrap_or((what, ""));
            match name {
                "next" => me.learn_next(l),
                "back" | "home" => me.learn_leave(l),
                "course" => me.learn_continue(l, arg),
                "pick" => {
                    if let (Some(run), Ok(i)) = (l.run.as_mut(), arg.parse::<usize>()) {
                        run.step(Input::Choose(i), &mut l.saved.progress, today());
                    }
                }
                "up" | "down" => {
                    if let (Some(run), Ok(i)) = (l.run.as_mut(), arg.parse::<usize>()) {
                        let to = if name == "up" { i.saturating_sub(1) } else { i + 1 };
                        run.step(Input::Move { from: i, to }, &mut l.saved.progress, today());
                    }
                }
                _ => eprintln!("harness-native: learn: no action {what:?}"),
            }
        }
        self.poke();
    }

    /// Return: start the course typed on the home, check the answer in hand, or go on.
    fn learn_next(&self, l: &mut Learn) {
        match &l.screen {
            Screen::Home => {
                let topic = l.draft.trim().to_owned();
                if topic.is_empty() {
                    return;
                }
                l.load();
                l.note.clear();
                l.ask += 1;
                l.want = Some(Want::Outline { topic: topic.clone() });
                l.screen = Screen::Making { title: "Planning your course".to_owned(), sub: format!("{topic} \u{b7} five to seven short lessons. About half a minute.") };
                self.learn_ask(l);
            }
            Screen::Making { .. } => {}
            Screen::Step => {
                let Some(run) = l.run.as_mut() else { return };
                run.step(Input::Next, &mut l.saved.progress, today());
                if run.view().is_none() {
                    l.finish();
                } else {
                    // A check's first answer is remembered the moment it is marked.
                    l.save();
                }
            }
            Screen::Done { course, .. } => {
                let course = course.clone();
                l.screen = Screen::Home;
                self.learn_continue(l, &course);
            }
        }
    }

    /// Escape: out of a lesson or a wait, back to the home (what was answered stays remembered).
    fn learn_leave(&self, l: &mut Learn) {
        l.ask += 1;
        l.want = None;
        l.run = None;
        l.screen = Screen::Home;
        l.save();
    }

    /// A key of ctx.Learn (scene/20-keys.flecs).
    pub(super) fn learn_key(&mut self, key: &str) {
        {
            let me = &*self;
            let mut l = me.learn.borrow_mut();
            let l = &mut *l;
            match key {
                "next" => me.learn_next(l),
                // The arrow goes on only where nothing is left to answer: past a read step, a marked check, a finished
                // run. It never submits and never starts a course.
                "forward" => {
                    let free = match (&l.screen, l.run.as_ref().and_then(Run::view)) {
                        (Screen::Step, Some(v)) => v.mark.is_some() || matches!(v.step, Step::Read { .. }),
                        (Screen::Done { .. }, _) => true,
                        _ => false,
                    };
                    if free {
                        me.learn_next(l);
                    }
                }
                "back" => match l.screen {
                    Screen::Home => l.draft.clear(),
                    _ => me.learn_leave(l),
                },
                "erase" => match (&l.screen, l.run.as_mut()) {
                    (Screen::Home, _) => {
                        l.draft.pop();
                    }
                    (Screen::Step, Some(run)) => {
                        if let Some(mut t) = run.view().map(|v| v.typed.to_owned()) {
                            t.pop();
                            run.step(Input::Type(t), &mut l.saved.progress, today());
                        }
                    }
                    _ => {}
                },
                _ => eprintln!("harness-native: learn: no key {key:?}"),
            }
        }
        self.poke();
    }

    /// Text typed (a key no binding takes, an input method, a paste) while Learn has the keyboard: into the home's
    /// field or a fill's; a digit picks a choice's option. Whether Learn took it.
    pub(super) fn learn_text(&mut self, text: &str) -> bool {
        if !self.learn_focused() {
            return false;
        }
        let text = typed(text.lines().next().unwrap_or(""));
        if text.is_empty() {
            return true;
        }
        {
            let mut l = self.learn.borrow_mut();
            let l = &mut *l;
            match (&l.screen, l.run.as_mut()) {
                (Screen::Home, _) => l.draft.push_str(&text),
                (Screen::Step, Some(run)) => {
                    let Some(v) = run.view() else { return true };
                    let input = match v.step {
                        Step::Choice { options, .. } => text.parse::<usize>().ok().filter(|n| (1..=options.len()).contains(n)).map(|n| Input::Choose(n - 1)),
                        Step::Fill { .. } => Some(Input::Type(format!("{}{text}", v.typed))),
                        Step::Read { .. } | Step::Order { .. } => None,
                    };
                    if let Some(input) = input {
                        run.step(input, &mut l.saved.progress, today());
                    }
                }
                _ => {}
            }
        }
        self.poke();
        true
    }

    /// Draw Learn into `pane` (hn.LearnPane): the one screen the state says.
    pub(super) fn fill_learn_pane(&self, pane: u64) {
        self.learn.borrow_mut().load();
        self.learn_arrivals();
        let w = self.world.world_ref();
        let Some(page) = View::of(w.entity_from_id(pane)).part("page") else { return };
        let l = self.learn.borrow();
        let l = &*l;
        let (key, prefab) = match (&l.screen, &l.run) {
            (Screen::Step, Some(_)) => ("step", "hn.LearnStep"),
            (Screen::Making { .. }, _) => ("making", "hn.LearnMaking"),
            (Screen::Done { .. }, _) => ("done", "hn.LearnDone"),
            _ => ("home", "hn.LearnHome"),
        };
        view::for_each_of(page, [(key, prefab, ())], |v, (), _| match (&l.screen, &l.run) {
            (Screen::Step, Some(run)) => fill_step(v, l, run),
            (Screen::Making { title, sub }, _) => {
                set_text(v, "title", title);
                set_text(v, "sub", sub);
            }
            (Screen::Done { kicker, title, score, next, course }, _) => {
                set_text(v, "kicker", kicker);
                set_text(v, "title", title);
                set_text(v, "score", score);
                set_text(v, "next", next);
                let more = l.course(course).is_some_and(|c| {
                    l.saved.progress.next_lesson(c).is_some() || !l.saved.progress.due(c, today()).is_empty()
                });
                if let Some(go) = v.part("row").and_then(|r| r.part("go")) {
                    if more {
                        button(go, "Keep going", "learn:next");
                    } else {
                        view::for_each_of(go, std::iter::empty::<(&str, &str, ())>(), |_, (), _| {});
                    }
                }
            }
            _ => fill_home(v, l),
        });
    }
}

fn set_text(v: View<'_>, part: &str, s: &str) {
    if let Some(p) = v.part(part) {
        p.text(s);
    }
}

fn fill_home(v: View<'_>, l: &Learn) {
    let day = today();
    if let Some(f) = v.part("field") {
        set_text(f, "text", &l.draft);
        set_text(f, "hint", if l.draft.is_empty() { "A topic, a tool, a paper, a codebase\u{2026}" } else { "" });
        if let Some(go) = f.part("go") {
            let (key, prefab) = if l.draft.trim().is_empty() { ("off", "hn.ChatSendOff") } else { ("on", "hn.ChatSend") };
            view::for_each_of(go, [(key, prefab, ())], |b, (), _| {
                if key == "on" {
                    b.action("learn:next");
                }
            });
        }
    }
    set_text(v, "note", &l.note);
    set_text(v, "label", if l.saved.courses.is_empty() { "" } else { "Your courses" });
    if let Some(list) = v.part("courses") {
        let rows = l.saved.courses.iter().take(5).map(|c| (c.id.as_str(), "hn.LearnCourse", c));
        view::for_each_of(list, rows, |row, c, _| {
            row.action(&format!("learn:course:{}", c.id));
            if let Some(head) = row.part("head") {
                set_text(head, "title", &c.title);
                let due = l.saved.progress.due(c, day).len();
                set_text(head, "due", &match due {
                    0 => String::new(),
                    n => format!("{n} to review"),
                });
            }
            let done = c.lessons.iter().filter(|x| l.saved.progress.finished(&c.id, &x.id)).count();
            let next = match l.saved.progress.next_lesson(c) {
                Some(x) => format!("Lesson {} of {} \u{b7} {}", done + 1, c.lessons.len(), x.title),
                None => format!("All {} lessons done \u{b7} {}", c.lessons.len(), c.goal),
            };
            set_text(row, "next", &next);
            if let Some(fill) = row.part("meter").and_then(|m| m.part("fill")) {
                percent(fill, l.saved.progress.mastery(c));
            }
        });
    }
    let streak = l.saved.progress.streak.on(day);
    set_text(v, "streak", &match streak {
        0 => String::new(),
        1 => "1 day in a row".to_owned(),
        n => format!("{n} days in a row"),
    });
}

fn fill_step(v: View<'_>, l: &Learn, run: &Run) {
    let Some(s) = run.view() else { return };
    if let Some(fill) = v.part("head").and_then(|h| h.part("bar")).and_then(|b| b.part("fill")) {
        percent(fill, s.progress);
    }
    let lesson: Option<&Lesson> = run.lesson_id().and_then(|id| l.course(run.course_id()).and_then(|c| c.lesson(id)));
    let kicker = match (s.again, lesson) {
        (true, _) => "Again".to_owned(),
        (false, Some(x)) => x.title.clone(),
        (false, None) => "Review".to_owned(),
    };
    set_text(v, "kicker", &kicker);
    let (title, body, code) = match s.step {
        Step::Read { title, body, code } => (title.as_str(), body.as_str(), code.as_str()),
        Step::Choice { prompt, code, .. } => (prompt.as_str(), "", code.as_str()),
        Step::Fill { prompt, .. } | Step::Order { prompt, .. } => (prompt.as_str(), "", ""),
    };
    set_text(v, "title", title);
    set_text(v, "body", body);
    if let Some(slot) = v.part("code") {
        view::for_each_of(slot, (!code.trim().is_empty()).then_some(("code", "hn.LearnCode", ())), |c, (), _| set_text(c, "text", code));
    }
    if let Some(list) = v.part("answers") {
        match s.step {
            Step::Read { .. } => view::for_each_of(list, std::iter::empty::<(String, &str, ())>(), |_, (), _| {}),
            Step::Choice { options, answer, .. } => {
                let rows = options.iter().enumerate().map(|(i, words)| {
                    let look = match (s.mark, s.picked == Some(i), i == *answer) {
                        (None, true, _) => "hn.LearnOptionPicked",
                        (None, false, _) => "hn.LearnOption",
                        (Some(_), _, true) => "hn.LearnOptionRight",
                        (Some(_), true, false) => "hn.LearnOptionWrong",
                        (Some(_), false, false) => "hn.LearnOptionDim",
                    };
                    // The key names the look too, so an option that changes look is a new node of that prefab.
                    (format!("{i}:{look}"), look, (i, words))
                });
                view::for_each_of(list, rows, |row, (i, words), _| {
                    set_text(row, "words", words);
                    if let Some(k) = row.part("key") {
                        set_text(k, "label", &(i + 1).to_string());
                    }
                    if s.mark.is_none() {
                        row.action(&format!("learn:pick:{i}"));
                    }
                });
            }
            Step::Fill { answer, .. } => {
                view::for_each_of(list, [("field".to_owned(), "hn.LearnField", ())], |f, (), _| {
                    set_text(f, "text", s.typed);
                    let hint = match (s.mark, s.typed.is_empty()) {
                        (Some(Mark::Wrong), _) => format!("   \u{2192} {answer}"),
                        (None, true) => "Type the missing part".to_owned(),
                        _ => String::new(),
                    };
                    set_text(f, "hint", &hint);
                });
            }
            Step::Order { items, .. } => {
                let n = s.order.len();
                let rows = s.order.iter().enumerate().map(|(at, &item)| {
                    let look = match s.mark {
                        None => "hn.LearnItem",
                        Some(_) if at == item => "hn.LearnItemRight",
                        Some(_) => "hn.LearnItemWrong",
                    };
                    (format!("{item}:{look}"), look, (at, item))
                });
                view::for_each_of(list, rows, |row, (at, item), _| {
                    set_text(row, "words", &items[item]);
                    if let Some(k) = row.part("key") {
                        set_text(k, "label", &(at + 1).to_string());
                    }
                    if let Some(up) = row.part("up") {
                        up.action(if at > 0 { format!("learn:up:{at}") } else { String::new() }.as_str());
                    }
                    if let Some(down) = row.part("down") {
                        down.action(if at + 1 < n { format!("learn:down:{at}") } else { String::new() }.as_str());
                    }
                });
            }
        }
    }
    let (verdict, why) = match s.mark {
        Some(Mark::Right) => ("Right.", s.step.why()),
        Some(Mark::Wrong) => ("Not quite.", s.step.why()),
        None => ("", ""),
    };
    set_text(v, "verdict", verdict);
    set_text(v, "why", why);
    if let Some(foot) = v.part("foot") {
        let read = matches!(s.step, Step::Read { .. });
        let (label, action) = match (read, s.mark, s.can_submit) {
            (true, _, _) | (false, Some(_), _) => ("Continue", "learn:next"),
            (false, None, true) => ("Check", "learn:next"),
            (false, None, false) => ("Check", ""),
        };
        let hint = match (s.step, s.mark) {
            (Step::Choice { .. }, None) => "1\u{2013}9 to pick \u{b7} return to check",
            _ => "return",
        };
        set_text(foot, "hint", hint);
        if let Some(go) = foot.part("go") {
            button(go, label, action);
        }
    }
}

/// A small course written by hand, for shots (`HARNESS_NATIVE_SHOT_LEARN`): the screens are drawn without a tutor.
const DEMO: &str = r#"{"id": "rust-borrowing", "title": "Borrowing in Rust", "goal": "Read a borrow error and know what it wants",
"lessons": [
 {"id": "owner", "title": "One owner", "about": "Every value has exactly one owner", "steps": [
  {"kind": "read", "title": "Every value has one owner", "body": "In Rust a value belongs to exactly one variable at a time: its owner. When the owner goes out of scope, the value is dropped. Assigning it to another variable moves it: the old name can no longer be used.", "code": "let a = String::from(\"hi\");\nlet b = a; // moved: `a` is gone"},
  {"kind": "choice", "id": "c1", "prompt": "After this runs, which names can still be used?", "code": "let s = vec![1, 2, 3];\nlet t = s;", "options": ["Both s and t", "Only s", "Only t", "Neither"], "answer": 2, "why": "Assigning a Vec moves it. `t` is the owner now; using `s` is a compile error, not a copy."},
  {"kind": "fill", "id": "f1", "prompt": "When its owner goes out of scope, a value is ___.", "answer": "dropped", "accept": ["freed", "destroyed"], "why": "Rust runs the value's drop at the end of the owner's scope; there is no garbage collector."},
  {"kind": "order", "id": "o1", "prompt": "Put a String's life in order.", "items": ["Created by its owner", "Moved to a new owner", "New owner goes out of scope", "Dropped"], "why": "A move changes who owns it; only the last owner's scope ending drops it."}
 ]},
 {"id": "borrow", "title": "Shared borrows", "about": "A reference reads without owning"},
 {"id": "mut", "title": "One writer", "about": "A mutable borrow is exclusive"},
 {"id": "lifetimes", "title": "Lifetimes", "about": "A borrow cannot outlive its value"}
]}"#;

impl App {
    /// A shot's Learn (`HARNESS_NATIVE_SHOT_LEARN=<screen>`, src/shot.rs): the demo course at one of its screens,
    /// nothing read from or written to disk. Screens: `home` (a course in progress), `new` (nothing yet), `read`,
    /// `choice`, `picked`, `right`, `wrong`, `fill`, `order`, `done`, `making`.
    pub fn shot_learn(&mut self, screen: &str) -> Result<(), String> {
        let course = Course::parse(DEMO).map_err(|e| e.to_string())?;
        let mut l = self.learn.borrow_mut();
        let l = &mut *l;
        l.shot = true;
        let day = today();
        let mut p = Progress::default();
        let walk = |run: &mut Run, p: &mut Progress, inputs: &[Input]| inputs.iter().for_each(|i| run.step(i.clone(), p, day));
        let mut run = Run::lesson(&course, &course.lessons[0], 7).ok_or("the demo's first lesson has no steps")?;
        let to_choice = [Input::Next];
        let to_fill = [Input::Next, Input::Choose(2), Input::Submit, Input::Next];
        match screen {
            "new" => {}
            "home" => {
                p.finish_lesson(&course.id, "owner", day.saturating_sub(1));
                for c in ["c1", "f1", "o1"] {
                    p.record(&format!("{}/owner/{c}", course.id), true, day.saturating_sub(1));
                }
                l.draft = "how tcp congestion control works".to_owned();
            }
            "making" => l.screen = Screen::Making { title: "Planning your course".into(), sub: "how tcp congestion control works \u{b7} five to seven short lessons. About half a minute.".into() },
            "read" => {}
            "choice" => walk(&mut run, &mut p, &to_choice),
            "picked" => walk(&mut run, &mut p, &[Input::Next, Input::Choose(2)]),
            "right" => walk(&mut run, &mut p, &[Input::Next, Input::Choose(2), Input::Submit]),
            "wrong" => walk(&mut run, &mut p, &[Input::Next, Input::Choose(0), Input::Submit]),
            "fill" => {
                walk(&mut run, &mut p, &to_fill);
                walk(&mut run, &mut p, &[Input::Type("drop".into())]);
            }
            "order" => {
                walk(&mut run, &mut p, &to_fill);
                walk(&mut run, &mut p, &[Input::Type("dropped".into()), Input::Submit, Input::Next]);
            }
            "done" => {
                walk(&mut run, &mut p, &to_fill);
                walk(&mut run, &mut p, &[Input::Type("gone".into()), Input::Submit, Input::Next]);
                for want in 0..4 {
                    let from = run.view().and_then(|v| v.order.iter().position(|&x| x == want)).unwrap_or(want);
                    run.step(Input::Move { from, to: want }, &mut p, day);
                }
                walk(&mut run, &mut p, &[Input::Submit, Input::Next, Input::Type("dropped".into()), Input::Submit, Input::Next]);
            }
            other => return Err(format!("HARNESS_NATIVE_SHOT_LEARN {other:?}: not a screen (new, home, making, read, choice, picked, right, wrong, fill, order, done)")),
        }
        l.saved = Saved { courses: if screen == "new" { Vec::new() } else { vec![course] }, progress: p, tutor: None };
        if matches!(screen, "read" | "choice" | "picked" | "right" | "wrong" | "fill" | "order" | "done") {
            l.run = Some(run);
            l.screen = Screen::Step;
            if screen == "done" {
                l.finish();
            }
        }
        Ok(())
    }
}
