//! The start page and lessons as a web page (crates/harness/web, its README the contract): the harness browser draws it
//! as one `ui.Browser` filling the main pane; this file sends it the screen and does what it says the person did.

use super::{App, Learn, Screen, today};
use harness_learn::{Input, Mark, Run, Step};
use serde_json::{Value, json};

/// The page to load: `$HARNESS_WEB_URL` (the dev server, while designing), else the build in crates/harness/web/dist.
fn page_url() -> String {
    if let Ok(url) = std::env::var("HARNESS_WEB_URL") {
        return url;
    }
    // Resolved: Chromium resolves `..` in the url it loads, and a UI view stays on the url it was opened at.
    let built = concat!(env!("CARGO_MANIFEST_DIR"), "/../web/dist/index.html");
    match std::fs::canonicalize(built) {
        Ok(p) => super::super::browser::file_url(&p.to_string_lossy()),
        Err(e) => panic!("the app's page {built} is not built ({e}): cd crates/harness/web && bun install && bun run build"),
    }
}

/// The whole screen as the page draws it (`{t: "state", screen}`).
fn screen(l: &Learn) -> Value {
    let day = today();
    let p = &l.saved.progress;
    let screen = match (&l.screen, &l.run) {
        (Screen::Making { kicker, title, sub }, _) => json!({"kind": "making", "kicker": kicker, "title": title, "sub": sub}),
        (Screen::Step, Some(run)) => step(l, run),
        (Screen::Course { course }, _) if l.course(course).is_some() => {
            let c = l.course(course).expect("checked above");
            let next = p.next_lesson(c).map(|x| x.id.as_str());
            let lessons: Vec<Value> = c
                .lessons
                .iter()
                .map(|x| {
                    let state = if next == Some(x.id.as_str()) {
                        "next"
                    } else if p.finished(&c.id, &x.id) {
                        "done"
                    } else {
                        "later"
                    };
                    json!({"id": x.id, "title": x.title, "about": x.about, "state": state, "mastery": p.lesson_mastery(&c.id, x)})
                })
                .collect();
            json!({"kind": "course", "id": c.id, "title": c.title, "goal": c.goal, "mastery": p.mastery(c),
                   "due": p.due(c, day).len(), "lessons": lessons})
        }
        (Screen::Done { kicker, title, score, next: _, course }, _) => {
            let next = l.course(course).and_then(|c| p.next_lesson(c)).map_or(String::new(), |x| x.title.clone());
            json!({"kind": "done", "kicker": kicker, "title": title, "score": score, "next": next})
        }
        _ => {
            let rows: Vec<Value> = l
                .saved
                .courses
                .iter()
                .take(3)
                .map(|c| json!({"id": c.id, "title": c.title, "due": p.due(c, day).len(), "mastery": p.mastery(c)}))
                .collect();
            json!({"kind": "start", "note": l.note, "continue": rows})
        }
    };
    json!({"t": "state", "screen": screen})
}

/// A lesson's step in front.
fn step(l: &Learn, run: &Run) -> Value {
    let Some(s) = run.view() else { return json!({"kind": "start", "note": "", "continue": []}) };
    let course = l.course(run.course_id());
    let lesson = run.lesson_id().and_then(|id| course.and_then(|c| c.lesson(id))).map_or("Review", |x| x.title.as_str());
    let (shown, answer) = match s.step {
        Step::Read { title, body, code } => (json!({"kind": "read", "title": title, "body": body, "code": code}), String::new()),
        Step::Choice { prompt, code, options, answer, .. } => (
            json!({"kind": "choice", "prompt": prompt, "code": code, "options": options}),
            options.get(*answer).cloned().unwrap_or_default(),
        ),
        Step::Fill { prompt, answer, .. } => (json!({"kind": "fill", "prompt": prompt}), answer.clone()),
        Step::Order { prompt, items, .. } => (json!({"kind": "order", "prompt": prompt, "items": items}), items.join(" \u{2192} ")),
    };
    let mark = s.mark.map(|m| json!({"right": m == Mark::Right, "answer": answer, "why": s.step.why()}));
    json!({"kind": "step", "course": course.map_or("", |c| c.title.as_str()), "lesson": lesson,
           "progress": s.progress, "again": s.again, "step": shown, "picked": s.picked, "typed": s.typed,
           "order": s.order, "mark": mark, "can_submit": s.can_submit})
}

/// What the page said, as the contract has it.
#[derive(serde::Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
enum Up {
    Ready,
    Ask { text: String, course: bool },
    Open { course: String },
    Lesson { course: String, lesson: String },
    Pick { i: usize },
    Type { text: String },
    Order { order: Vec<usize> },
    Submit,
    Next,
    Back,
    Cancel,
    Link { url: String },
}

impl App {
    /// Each frame Learn is in front: the page's view opened, given the keys while the pane has them, sent the screen
    /// when it changed, and what it said done.
    pub(in crate::app) fn learn_page(&mut self, pane_focused: bool) {
        let Some(view) = self.ui_view("learn", &page_url()) else {
            // Refused or closed by the host: said once, in the log and on the pane's plate (browser.rs).
            let why = self.ui_failed("learn").map(str::to_owned);
            let mut l = self.learn.borrow_mut();
            if let Some(why) = why.filter(|w| *w != l.page_sent) {
                eprintln!("harness-native: learn: the page {} could not be shown: {why}", page_url());
                l.page_sent = why;
            }
            return;
        };
        self.learn.borrow().page_view.set(view);
        for (from, msg) in self.ui_messages() {
            if from != view {
                eprintln!("harness-native: learn: a message from view {from}, not the page's ({view}): {msg}");
                continue;
            }
            match serde_json::from_str::<Up>(&msg) {
                Ok(up) => self.learn_up(up),
                Err(e) => eprintln!("harness-native: learn: the page sent {msg:?}, which is not a message of web/README.md: {e}"),
            }
        }
        if pane_focused {
            self.learn_page_focus();
        }
        let text = {
            self.learn_arrivals();
            screen(&self.learn.borrow()).to_string()
        };
        let mut l = self.learn.borrow_mut();
        if l.page_sent != text {
            l.page_sent.clone_from(&text);
            drop(l);
            self.ui_post(view, &text);
        }
    }

    /// One message of the page's.
    fn learn_up(&mut self, up: Up) {
        match up {
            // A page (re)loaded holds nothing: send it all again.
            Up::Ready => self.learn.borrow_mut().page_sent.clear(),
            Up::Ask { text, course } => {
                self.learn.borrow_mut().draft = super::line_draft(&text);
                self.learn_start_ask(course);
            }
            Up::Open { course } => self.learn_action(&format!("course:{course}")),
            Up::Lesson { course, lesson } => self.learn_action(&format!("lesson:{course}/{lesson}")),
            Up::Pick { i } => self.learn_action(&format!("pick:{i}")),
            Up::Type { text } => self.learn_input(Input::Type(text)),
            Up::Order { order } => {
                // The new order as moves: each place in turn takes the item the page put there.
                let now = self.learn.borrow().run.as_ref().and_then(|r| r.view().map(|v| v.order.to_vec())).unwrap_or_default();
                let mut now = now;
                for (to, want) in order.iter().enumerate() {
                    let Some(from) = now.iter().position(|x| x == want) else {
                        eprintln!("harness-native: learn: the page's order {order:?} names item {want}, not in {now:?}");
                        return;
                    };
                    if from != to {
                        let x = now.remove(from);
                        now.insert(to, x);
                        self.learn_input(Input::Move { from, to });
                    }
                }
            }
            Up::Submit | Up::Next => {
                let me = &*self;
                me.learn_next(&mut me.learn.borrow_mut());
            }
            Up::Back => {
                let me = &*self;
                let mut l = me.learn.borrow_mut();
                // Out of a lesson, a wait or its end to the course's page; out of a course's page to the start.
                let course = match (&l.screen, &l.run) {
                    (Screen::Step, Some(run)) => Some(run.course_id().to_owned()),
                    (Screen::Done { course, .. }, _) => Some(course.clone()),
                    _ => None,
                };
                me.learn_leave(&mut l);
                if let Some(course) = course.filter(|c| l.course(c).is_some()) {
                    l.screen = Screen::Course { course };
                }
            }
            Up::Cancel => {
                let me = &*self;
                me.learn_leave(&mut me.learn.borrow_mut());
            }
            Up::Link { url } => self.open_target(&url, false),
        }
        self.poke();
    }

    /// An input to the run in hand, and the progress it changed kept.
    fn learn_input(&mut self, input: Input) {
        let mut l = self.learn.borrow_mut();
        let l = &mut *l;
        if let Some(run) = l.run.as_mut() {
            run.step(input, &mut l.saved.progress, today());
        }
    }
}
