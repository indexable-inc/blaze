//! What a person has learned, and when each piece is due again.

use crate::course::{Course, Lesson, check_key};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A day, as a count from any fixed epoch the host chooses (it only has to keep choosing the same one).
pub type Day = u32;

/// The first gap after a check is recalled, in days; each recall after multiplies it by [`GROW`].
pub const FIRST_GAP: f32 = 1.0;
/// How much a recalled check's gap grows.
pub const GROW: f32 = 2.6;
/// What is left of the gap after a miss.
pub const SHRINK: f32 = 0.35;
/// The longest gap: about four months.
pub const GAP_MAX: f32 = 120.0;
/// A check counts as learned once its gap reaches this many days (three recalls in a row from new).
pub const LEARNED_GAP: f32 = 6.0;

/// One check's memory: how long it lasts and when it is due.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Memory {
    /// The gap, in days, the next recall is scheduled after.
    pub gap: f32,
    /// The day it is next due.
    pub due: Day,
    /// Times recalled, and times missed.
    pub recalled: u32,
    pub missed: u32,
}

impl Memory {
    /// The memory after a review on `today`: recalled first try (`right`), or missed.
    pub fn review(seen: Option<Memory>, right: bool, today: Day) -> Memory {
        let was = seen.unwrap_or(Memory { gap: 0.0, due: today, recalled: 0, missed: 0 });
        let gap = match (right, was.gap > 0.0) {
            // Recalled early (before it was due) grows the gap by the share of it that had passed, so cramming one check
            // ten times in a day does not push it out four months.
            (true, true) => {
                let waited = was.gap - (was.due.saturating_sub(today)) as f32;
                let share = (waited / was.gap).clamp(0.0, 1.0);
                (was.gap * (1.0 + (GROW - 1.0) * share)).min(GAP_MAX)
            }
            (true, false) => FIRST_GAP,
            (false, _) => was.gap * SHRINK,
        };
        // A missed check is due today again: it comes back in this run, and tomorrow's review has it.
        let due = if right { today + gap.round().max(1.0) as Day } else { today };
        Memory { gap, due, recalled: was.recalled + u32::from(right), missed: was.missed + u32::from(!right) }
    }

    pub fn learned(&self) -> bool {
        self.gap >= LEARNED_GAP
    }

    /// How well it is held, 0 to 1: the gap against the learned gap.
    pub fn strength(&self) -> f32 {
        (self.gap / LEARNED_GAP).clamp(0.0, 1.0)
    }
}

/// Days in a row with a finished run, and the last such day.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Streak {
    pub days: u32,
    pub last: Option<Day>,
}

impl Streak {
    /// The streak after a run finished on `today`.
    pub fn after(self, today: Day) -> Streak {
        let days = match self.last {
            Some(d) if d == today => self.days.max(1),
            Some(d) if d + 1 == today => self.days + 1,
            _ => 1,
        };
        Streak { days, last: Some(today) }
    }

    /// The streak as it stands on `today`: gone once a day was skipped.
    pub fn on(self, today: Day) -> u32 {
        match self.last {
            Some(d) if d + 1 >= today => self.days,
            _ => 0,
        }
    }
}

/// Everything remembered about one person's learning. Plain data: the host reads and writes it as JSON.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Progress {
    /// Each check's memory, by [`check_key`].
    #[serde(default)]
    pub checks: BTreeMap<String, Memory>,
    /// The lessons finished, as `course/lesson`, and the day each was first finished.
    #[serde(default)]
    pub finished: BTreeMap<String, Day>,
    #[serde(default)]
    pub streak: Streak,
}

impl Progress {
    /// Record one answer to a check on `today`. Only a check's first answer in a run is recorded (the run says which).
    pub fn record(&mut self, key: &str, right: bool, today: Day) {
        let next = Memory::review(self.checks.get(key).copied(), right, today);
        self.checks.insert(key.to_owned(), next);
    }

    pub fn finish_lesson(&mut self, course: &str, lesson: &str, today: Day) {
        self.finished.entry(format!("{course}/{lesson}")).or_insert(today);
        self.streak = self.streak.after(today);
    }

    pub fn finished(&self, course: &str, lesson: &str) -> bool {
        self.finished.contains_key(&format!("{course}/{lesson}"))
    }

    /// The first lesson of `course` not finished yet: where "Continue" goes. `None` once all are.
    pub fn next_lesson<'c>(&self, course: &'c Course) -> Option<&'c Lesson> {
        course.lessons.iter().find(|l| !self.finished(&course.id, &l.id))
    }

    /// A lesson's mastery, 0 to 1: the mean strength of its checks (a check never answered counts as 0). A lesson whose
    /// steps are not written yet has none.
    pub fn lesson_mastery(&self, course: &str, lesson: &Lesson) -> f32 {
        let (mut sum, mut n) = (0.0, 0);
        for id in lesson.checks() {
            sum += self.checks.get(&check_key(course, &lesson.id, id)).map_or(0.0, Memory::strength);
            n += 1;
        }
        if n == 0 { 0.0 } else { sum / n as f32 }
    }

    /// A course's mastery, 0 to 1: the share of its lessons finished, each weighted by how well its checks are held
    /// (a finished lesson counts half on finishing and half on its mastery, so progress shows at once and then deepens).
    pub fn mastery(&self, course: &Course) -> f32 {
        if course.lessons.is_empty() {
            return 0.0;
        }
        let total: f32 = course
            .lessons
            .iter()
            .map(|l| if self.finished(&course.id, &l.id) { 0.5 + 0.5 * self.lesson_mastery(&course.id, l) } else { 0.0 })
            .sum();
        total / course.lessons.len() as f32
    }

    /// The checks of `course` due on `today` or before, most overdue first: a review run's queue, as
    /// `(lesson id, check id)`. Only checks of finished lessons are due (a lesson in progress asks its own).
    pub fn due<'c>(&self, course: &'c Course, today: Day) -> Vec<(&'c str, &'c str)> {
        let mut due: Vec<(Day, &str, &str)> = Vec::new();
        for l in course.lessons.iter().filter(|l| self.finished(&course.id, &l.id)) {
            for id in l.checks() {
                if let Some(m) = self.checks.get(&check_key(&course.id, &l.id, id)) {
                    if m.due <= today {
                        due.push((m.due, &l.id, id));
                    }
                }
            }
        }
        due.sort();
        due.into_iter().map(|(_, l, c)| (l, c)).collect()
    }

    /// The checks of `course` the person missed more often than they recalled, weakest first: what a tutor is told when
    /// it writes the next lesson, so the course bends toward what is shaky.
    pub fn shaky<'c>(&self, course: &'c Course) -> Vec<(&'c str, &'c str)> {
        let mut shaky: Vec<(i64, &str, &str)> = Vec::new();
        for l in &course.lessons {
            for id in l.checks() {
                if let Some(m) = self.checks.get(&check_key(&course.id, &l.id, id)) {
                    if m.missed > 0 && m.missed >= m.recalled {
                        shaky.push((i64::from(m.recalled) - i64::from(m.missed), &l.id, id));
                    }
                }
            }
        }
        shaky.sort();
        shaky.into_iter().map(|(_, l, c)| (l, c)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recalls_on_time_space_out_and_a_miss_pulls_back() {
        let mut m = Memory::review(None, true, 10);
        assert_eq!((m.gap, m.due), (FIRST_GAP, 11));
        m = Memory::review(Some(m), true, 11);
        assert!((m.gap - 2.6).abs() < 1e-4 && m.due == 14);
        m = Memory::review(Some(m), true, 14);
        assert!(m.learned(), "three recalls in a row from new: learned");
        let before = m.gap;
        m = Memory::review(Some(m), false, m.due);
        assert!(m.gap < before * 0.5 && !m.learned());
        assert_eq!(m.missed, 1);
    }

    #[test]
    fn cramming_a_check_the_same_day_does_not_space_it_out() {
        let mut m = Memory::review(None, true, 0);
        m = Memory::review(Some(m), true, 1);
        let gap = m.gap;
        for _ in 0..10 {
            m = Memory::review(Some(m), true, 1);
        }
        assert!((m.gap - gap).abs() < 1e-4, "recalled again the day it was scheduled: nothing had passed");
    }

    #[test]
    fn a_streak_counts_days_in_a_row_and_ends_when_one_is_skipped() {
        let s = Streak::default().after(5).after(5).after(6);
        assert_eq!(s.days, 2);
        assert_eq!((s.on(6), s.on(7), s.on(8)), (2, 2, 0));
        assert_eq!(s.after(9).days, 1);
    }
}
