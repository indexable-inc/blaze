//! One sitting, as a state machine: a lesson walked step by step, or a review of what is due.
//!
//! The host shows [`Run::view`] and sends [`Input`]s. A check is answered, then marked (with its `why`), then left with
//! [`Input::Next`]; a missed one comes back [`AGAIN_AFTER`] steps later until it is answered right. Only a check's first
//! answer in a run is written to [`Progress`]: getting it right on the second look is not a recall.

use crate::course::{Course, Lesson, Step, check_key};
use crate::memory::{Day, Progress};
use std::collections::VecDeque;

/// How many steps later a missed check comes back.
pub const AGAIN_AFTER: usize = 3;

/// What the person did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Input {
    /// Past a read step, or past a marked check. On an unmarked check with an answer in hand it submits.
    Next,
    /// Pick option `i` of a choice (again to change it), before it is marked.
    Choose(usize),
    /// The words typed so far into a fill.
    Type(String),
    /// Move an order's item from one place to another.
    Move { from: usize, to: usize },
    /// Check the answer in hand.
    Submit,
}

/// How a check was answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mark {
    Right,
    Wrong,
}

/// What the host draws.
#[derive(Clone, Debug, PartialEq)]
pub struct View<'a> {
    pub step: &'a Step,
    /// The option picked (a choice), the words typed (a fill), the items as they are ordered now (an order: indexes
    /// into the step's `items`).
    pub picked: Option<usize>,
    pub typed: &'a str,
    pub order: &'a [usize],
    /// `None` until the check is submitted; a read step is never marked.
    pub mark: Option<Mark>,
    /// Whether [`Input::Submit`] would do anything now (an answer is in hand and unmarked).
    pub can_submit: bool,
    /// How far along the run is, 0 to 1: steps passed over steps in all (a missed check does not count until it is
    /// answered right).
    pub progress: f32,
    /// This check came back after a miss.
    pub again: bool,
}

/// How a finished run went.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    /// Checks asked, and how many were right the first time.
    pub asked: usize,
    pub right_first: usize,
    /// The checks missed at least once, by key.
    pub missed: Vec<String>,
}

#[derive(Clone, Debug)]
struct Item {
    lesson: String,
    step: Step,
    /// How its first answer in this run went.
    first: Option<Mark>,
    passed: bool,
    seen: u32,
}

/// What the run is of: a lesson (finished, it is marked so), or a review.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Of {
    Lesson(String),
    Review,
}

#[derive(Clone, Debug)]
pub struct Run {
    course: String,
    of: Of,
    items: Vec<Item>,
    queue: VecDeque<usize>,
    picked: Option<usize>,
    typed: String,
    order: Vec<usize>,
    mark: Option<Mark>,
    seed: u64,
    summary: Option<Summary>,
}

/// The next value of a small deterministic generator (SplitMix64): the host passes the seed, so a run is a function of
/// its inputs.
fn next(seed: &mut u64) -> u64 {
    *seed = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *seed;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// `0..n` shuffled, never in order when `n > 1` (an order check shown already solved asks nothing).
fn shuffled(n: usize, seed: &mut u64) -> Vec<usize> {
    let mut v: Vec<usize> = (0..n).collect();
    for i in (1..n).rev() {
        v.swap(i, (next(seed) % (i as u64 + 1)) as usize);
    }
    if n > 1 && v.iter().enumerate().all(|(i, &x)| i == x) {
        v.rotate_left(1);
    }
    v
}

/// A typed answer as it is compared: lower case, blanks trimmed and runs of them one space.
fn plain(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

impl Run {
    /// A run of `lesson`'s steps in order. `None` if its steps are not written yet.
    pub fn lesson(course: &Course, lesson: &Lesson, seed: u64) -> Option<Run> {
        if lesson.steps.is_empty() {
            return None;
        }
        let steps = lesson.steps.iter().map(|s| (lesson.id.clone(), s.clone()));
        Some(Self::of(course, Of::Lesson(lesson.id.clone()), steps, seed))
    }

    /// A review of `due` (from [`Progress::due`]): those checks alone, no reading. `None` when nothing is due.
    pub fn review(course: &Course, due: &[(&str, &str)], seed: u64) -> Option<Run> {
        let steps: Vec<(String, Step)> = due
            .iter()
            .filter_map(|(l, c)| {
                let step = course.lesson(l)?.steps.iter().find(|s| s.check_id() == Some(c))?;
                Some(((*l).to_owned(), step.clone()))
            })
            .collect();
        if steps.is_empty() {
            return None;
        }
        Some(Self::of(course, Of::Review, steps.into_iter(), seed))
    }

    fn of(course: &Course, of: Of, steps: impl Iterator<Item = (String, Step)>, seed: u64) -> Run {
        let items: Vec<Item> = steps.map(|(lesson, step)| Item { lesson, step, first: None, passed: false, seen: 0 }).collect();
        let queue = (0..items.len()).collect();
        let mut run = Run {
            course: course.id.clone(),
            of,
            items,
            queue,
            picked: None,
            typed: String::new(),
            order: Vec::new(),
            mark: None,
            seed,
            summary: None,
        };
        run.arrive();
        run
    }

    /// The step at the front of the queue was just reached: its answer starts empty (an order's items shuffled).
    fn arrive(&mut self) {
        self.picked = None;
        self.typed.clear();
        self.mark = None;
        self.order.clear();
        if let Some(&i) = self.queue.front() {
            self.items[i].seen += 1;
            if let Step::Order { items, .. } = &self.items[i].step {
                self.order = shuffled(items.len(), &mut self.seed);
            }
        }
    }

    /// The lesson this run walks; `None` for a review.
    pub fn lesson_id(&self) -> Option<&str> {
        match &self.of {
            Of::Lesson(id) => Some(id),
            Of::Review => None,
        }
    }

    pub fn course_id(&self) -> &str {
        &self.course
    }

    /// How it went, once it is over.
    pub fn summary(&self) -> Option<&Summary> {
        self.summary.as_ref()
    }

    /// What to draw now; `None` once the run is over ([`Run::summary`] then says how it went).
    pub fn view(&self) -> Option<View<'_>> {
        let item = &self.items[*self.queue.front()?];
        let passed = self.items.iter().filter(|i| i.passed).count();
        let has_answer = match &item.step {
            Step::Read { .. } => false,
            Step::Choice { .. } => self.picked.is_some(),
            Step::Fill { .. } => !self.typed.trim().is_empty(),
            Step::Order { .. } => true,
        };
        Some(View {
            step: &item.step,
            picked: self.picked,
            typed: &self.typed,
            order: &self.order,
            mark: self.mark,
            can_submit: has_answer && self.mark.is_none(),
            progress: passed as f32 / self.items.len() as f32,
            again: item.seen > 1,
        })
    }

    /// Whether the answer in hand is the right one.
    fn right(&self, step: &Step) -> bool {
        match step {
            Step::Read { .. } => true,
            Step::Choice { answer, .. } => self.picked == Some(*answer),
            Step::Fill { answer, accept, .. } => {
                let typed = plain(&self.typed);
                std::iter::once(answer).chain(accept).any(|a| plain(a) == typed)
            }
            Step::Order { .. } => self.order.iter().enumerate().all(|(i, &x)| i == x),
        }
    }

    /// Take one input; `progress` is written when a check is first answered and when the run ends, as of `today`.
    /// An input that means nothing now (a choice on a read step, anything after the end) changes nothing.
    pub fn step(&mut self, input: Input, progress: &mut Progress, today: Day) {
        let Some(&i) = self.queue.front() else { return };
        let is_read = matches!(self.items[i].step, Step::Read { .. });
        match input {
            Input::Next if is_read || self.mark.is_some() => self.leave(i, progress, today),
            Input::Next | Input::Submit => self.submit(i, progress, today),
            Input::Choose(k) if self.mark.is_none() => {
                if let Step::Choice { options, .. } = &self.items[i].step {
                    if k < options.len() {
                        self.picked = Some(k);
                    }
                }
            }
            Input::Type(s) if self.mark.is_none() && matches!(self.items[i].step, Step::Fill { .. }) => self.typed = s,
            Input::Move { from, to } if self.mark.is_none() && from < self.order.len() && to < self.order.len() => {
                let x = self.order.remove(from);
                self.order.insert(to, x);
            }
            Input::Choose(_) | Input::Type(_) | Input::Move { .. } => {}
        }
    }

    fn submit(&mut self, i: usize, progress: &mut Progress, today: Day) {
        let can = self.view().is_some_and(|v| v.can_submit);
        if !can {
            return;
        }
        let right = self.right(&self.items[i].step);
        let mark = if right { Mark::Right } else { Mark::Wrong };
        self.mark = Some(mark);
        let item = &mut self.items[i];
        if item.first.is_none() {
            item.first = Some(mark);
            if let Some(id) = item.step.check_id() {
                progress.record(&check_key(&self.course, &item.lesson, id), right, today);
            }
        }
    }

    /// Leave the step at the front: passed if it was read or answered right, else back in the queue a few steps on.
    fn leave(&mut self, i: usize, progress: &mut Progress, today: Day) {
        self.queue.pop_front();
        if self.mark == Some(Mark::Wrong) {
            self.queue.insert(AGAIN_AFTER.min(self.queue.len()), i);
        } else {
            self.items[i].passed = true;
        }
        if self.queue.is_empty() {
            let checks = || self.items.iter().filter(|it| it.step.check_id().is_some());
            let summary = Summary {
                asked: checks().count(),
                right_first: checks().filter(|it| it.first == Some(Mark::Right)).count(),
                missed: checks()
                    .filter(|it| it.first == Some(Mark::Wrong))
                    .filter_map(|it| Some(check_key(&self.course, &it.lesson, it.step.check_id()?)))
                    .collect(),
            };
            match &self.of {
                Of::Lesson(lesson) => progress.finish_lesson(&self.course, lesson, today),
                Of::Review => progress.streak = progress.streak.after(today),
            }
            self.summary = Some(summary);
        }
        self.arrive();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn course() -> Course {
        let steps = vec![
            Step::Read { title: "Idea".into(), body: "One idea.".into(), code: String::new() },
            Step::Choice { id: "c".into(), prompt: "?".into(), code: String::new(), options: vec!["a".into(), "b".into()], answer: 1, why: "b".into() },
            Step::Fill { id: "f".into(), prompt: "_".into(), answer: "Mutable".into(), accept: vec!["unique".into()], why: "w".into() },
            Step::Order { id: "o".into(), prompt: "order".into(), items: vec!["1".into(), "2".into(), "3".into()], why: "w".into() },
        ];
        Course {
            id: "k".into(),
            title: "K".into(),
            goal: "g".into(),
            lessons: vec![Lesson { id: "l".into(), title: "L".into(), about: "a".into(), steps }],
        }
    }

    /// Put an order's items right, one move at a time.
    fn sort(run: &mut Run, p: &mut Progress) {
        for want in 0..3 {
            let from = run.view().unwrap().order.iter().position(|&x| x == want).unwrap();
            run.step(Input::Move { from, to: want }, p, 0);
        }
    }

    #[test]
    fn a_lesson_walked_right_is_finished_with_every_check_recalled() {
        let c = course();
        let mut p = Progress::default();
        let mut run = Run::lesson(&c, &c.lessons[0], 7).unwrap();
        run.step(Input::Choose(0), &mut p, 0); // nothing to choose on a read step
        run.step(Input::Next, &mut p, 0);
        assert!(!run.view().unwrap().can_submit);
        run.step(Input::Submit, &mut p, 0); // nothing picked: nothing is marked
        assert_eq!(run.view().unwrap().mark, None);
        run.step(Input::Choose(1), &mut p, 0);
        run.step(Input::Next, &mut p, 0); // an answer in hand: Next submits
        assert_eq!(run.view().unwrap().mark, Some(Mark::Right));
        run.step(Input::Next, &mut p, 0);
        run.step(Input::Type("  mutable ".into()), &mut p, 0);
        run.step(Input::Submit, &mut p, 0);
        assert_eq!(run.view().unwrap().mark, Some(Mark::Right), "case and blanks do not matter");
        run.step(Input::Next, &mut p, 0);
        assert_ne!(run.view().unwrap().order, [0, 1, 2], "an order check is never shown solved");
        sort(&mut run, &mut p);
        run.step(Input::Submit, &mut p, 0);
        run.step(Input::Next, &mut p, 0);
        assert!(run.view().is_none());
        assert_eq!(run.summary(), Some(&Summary { asked: 3, right_first: 3, missed: Vec::new() }));
        assert!(p.finished("k", "l"));
        assert_eq!(p.checks.len(), 3);
        assert_eq!(p.streak.days, 1);
    }

    #[test]
    fn a_missed_check_comes_back_and_only_its_first_answer_is_remembered() {
        let c = course();
        let mut p = Progress::default();
        let mut run = Run::lesson(&c, &c.lessons[0], 1).unwrap();
        run.step(Input::Next, &mut p, 3);
        run.step(Input::Choose(0), &mut p, 3);
        run.step(Input::Submit, &mut p, 3);
        assert_eq!(run.view().unwrap().mark, Some(Mark::Wrong));
        run.step(Input::Choose(1), &mut p, 3); // marked: the answer cannot be changed
        assert_eq!(run.view().unwrap().picked, Some(0));
        run.step(Input::Next, &mut p, 3);
        // The fill and the order, then the choice again (it went to the end of a queue shorter than AGAIN_AFTER).
        run.step(Input::Type("unique".into()), &mut p, 3);
        run.step(Input::Submit, &mut p, 3);
        run.step(Input::Next, &mut p, 3);
        sort(&mut run, &mut p);
        run.step(Input::Submit, &mut p, 3);
        run.step(Input::Next, &mut p, 3);
        let v = run.view().unwrap();
        assert!(v.again && matches!(v.step, Step::Choice { .. }));
        assert!(v.progress < 1.0, "a missed check does not count until it is answered right");
        run.step(Input::Choose(1), &mut p, 3);
        run.step(Input::Submit, &mut p, 3);
        run.step(Input::Next, &mut p, 3);
        assert_eq!(run.summary().unwrap().missed, ["k/l/c"]);
        let m = p.checks["k/l/c"];
        assert_eq!((m.recalled, m.missed, m.due), (0, 1, 3), "right on the second look is not a recall");
        assert_eq!(p.due(&c, 3), [("l", "c")]);
    }

    #[test]
    fn a_review_asks_only_what_is_due() {
        let c = course();
        let mut p = Progress::default();
        p.finish_lesson("k", "l", 0);
        p.record("k/l/c", true, 0);
        p.record("k/l/f", true, 0);
        assert!(Run::review(&c, &p.due(&c, 0), 1).is_none(), "nothing is due the day it was learned");
        let due = p.due(&c, 1);
        let mut run = Run::review(&c, &due, 1).unwrap();
        assert!(run.lesson_id().is_none());
        for _ in 0..2 {
            match run.view().unwrap().step.clone() {
                Step::Choice { answer, .. } => run.step(Input::Choose(answer), &mut p, 1),
                Step::Fill { answer, .. } => run.step(Input::Type(answer), &mut p, 1),
                other => panic!("a review asks checks, not {other:?}"),
            }
            run.step(Input::Submit, &mut p, 1);
            run.step(Input::Next, &mut p, 1);
        }
        assert!(run.view().is_none());
        assert!(p.due(&c, 1).is_empty());
        assert_eq!(p.streak.days, 2);
    }
}
