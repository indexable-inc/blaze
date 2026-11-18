//! A course as data: what a tutor writes (as JSON) and what a run walks.

use serde::{Deserialize, Serialize};

/// The most words a [`Step::Read`] may tell before the lesson must ask something: a screen, read in under half a minute.
pub const READ_WORDS_MAX: usize = 90;
/// The most options a [`Step::Choice`] offers, and the most items a [`Step::Order`] orders.
pub const OPTIONS_MAX: usize = 6;
/// A lesson's steps, at most: short enough to finish in a few minutes.
pub const STEPS_MAX: usize = 16;

/// A path of lessons toward one goal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Course {
    /// Stable for the course's life: progress is keyed by it.
    pub id: String,
    pub title: String,
    /// What the person will be able to do at its end, one line.
    pub goal: String,
    pub lessons: Vec<Lesson>,
}

/// One sitting: a few ideas told and asked back.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lesson {
    /// Stable within its course.
    pub id: String,
    pub title: String,
    /// The one thing it teaches, one line (the course map shows it under the title).
    pub about: String,
    /// Empty until the tutor has written it: a course's outline comes first, each lesson's steps when it is opened.
    #[serde(default)]
    pub steps: Vec<Step>,
}

/// One screen of a lesson.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Step {
    /// One small idea, told: a title and a few sentences of markdown (at most [`READ_WORDS_MAX`] words), with an
    /// optional block of code or a worked example under it.
    Read {
        title: String,
        body: String,
        #[serde(default)]
        code: String,
    },
    /// Pick one: a question (with optional code to read first), two to [`OPTIONS_MAX`] options, which one is right
    /// and why.
    Choice {
        /// Stable within its lesson: the check's memory is keyed by it.
        id: String,
        prompt: String,
        #[serde(default)]
        code: String,
        options: Vec<String>,
        answer: usize,
        why: String,
    },
    /// Type the missing word or short phrase. `accept` lists other spellings that count; matching ignores case and
    /// the blanks around it.
    Fill {
        id: String,
        prompt: String,
        answer: String,
        #[serde(default)]
        accept: Vec<String>,
        why: String,
    },
    /// Put these in order. `items` is the right order; the run shows them shuffled.
    Order {
        id: String,
        prompt: String,
        items: Vec<String>,
        why: String,
    },
}

impl Step {
    /// A check's id; `None` for a [`Step::Read`], which asks nothing.
    pub fn check_id(&self) -> Option<&str> {
        match self {
            Step::Read { .. } => None,
            Step::Choice { id, .. } | Step::Fill { id, .. } | Step::Order { id, .. } => Some(id),
        }
    }

    /// Why the right answer is right; empty for a [`Step::Read`].
    pub fn why(&self) -> &str {
        match self {
            Step::Read { .. } => "",
            Step::Choice { why, .. } | Step::Fill { why, .. } | Step::Order { why, .. } => why,
        }
    }
}

/// Why a course (or one lesson's steps) from a tutor is refused. Each names the thing it is about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CourseError {
    /// Not the JSON of a course or of a lesson's steps.
    Json { what: &'static str, error: String },
    Empty { what: &'static str, at: String },
    DuplicateId { id: String, at: String },
    TooManySteps { lesson: String, steps: usize },
    /// A lesson that only tells: it must ask at least once.
    NoCheck { lesson: String },
    ReadTooLong { lesson: String, title: String, words: usize },
    Options { lesson: String, id: String, options: usize },
    AnswerOutOfRange { lesson: String, id: String, answer: usize, options: usize },
}

impl std::fmt::Display for CourseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Json { what, error } => write!(f, "the tutor's {what} is not its JSON: {error}"),
            Self::Empty { what, at } => write!(f, "{at}: its {what} is empty"),
            Self::DuplicateId { id, at } => write!(f, "{at}: the id {id:?} is used twice"),
            Self::TooManySteps { lesson, steps } => write!(f, "lesson {lesson:?} has {steps} steps, more than {STEPS_MAX}"),
            Self::NoCheck { lesson } => write!(f, "lesson {lesson:?} asks nothing: a lesson must check what it tells"),
            Self::ReadTooLong { lesson, title, words } => {
                write!(f, "lesson {lesson:?}: {title:?} tells {words} words, more than {READ_WORDS_MAX}")
            }
            Self::Options { lesson, id, options } => {
                write!(f, "lesson {lesson:?}: check {id:?} has {options} options or items, not 2 to {OPTIONS_MAX}")
            }
            Self::AnswerOutOfRange { lesson, id, answer, options } => {
                write!(f, "lesson {lesson:?}: check {id:?} says option {answer} is right, of {options}")
            }
        }
    }
}

impl std::error::Error for CourseError {}

/// The JSON object in `text`: a tutor's answer is often its JSON inside a fenced block or after a sentence, so this is
/// the span from its first `{` to its last `}`.
fn json_span(text: &str) -> &str {
    match (text.find('{'), text.rfind('}')) {
        (Some(a), Some(b)) if a < b => &text[a..=b],
        _ => text,
    }
}

impl Course {
    /// A course's outline from a tutor's answer: its title, goal and lessons (their steps may be empty, written later).
    pub fn parse(text: &str) -> Result<Self, CourseError> {
        let course: Course =
            serde_json::from_str(json_span(text)).map_err(|e| CourseError::Json { what: "course", error: e.to_string() })?;
        course.check()?;
        Ok(course)
    }

    /// Whether this course can be walked: named, with lessons whose ids differ, each well formed.
    pub fn check(&self) -> Result<(), CourseError> {
        let at = || format!("course {:?}", self.id);
        for (what, s) in [("id", &self.id), ("title", &self.title)] {
            if s.trim().is_empty() {
                return Err(CourseError::Empty { what, at: at() });
            }
        }
        if self.lessons.is_empty() {
            return Err(CourseError::Empty { what: "list of lessons", at: at() });
        }
        for (i, l) in self.lessons.iter().enumerate() {
            if l.id.trim().is_empty() || l.title.trim().is_empty() {
                return Err(CourseError::Empty { what: "id or title", at: format!("lesson {} of {}", i + 1, at()) });
            }
            if self.lessons[..i].iter().any(|o| o.id == l.id) {
                return Err(CourseError::DuplicateId { id: l.id.clone(), at: at() });
            }
            if !l.steps.is_empty() {
                l.check()?;
            }
        }
        Ok(())
    }

    pub fn lesson(&self, id: &str) -> Option<&Lesson> {
        self.lessons.iter().find(|l| l.id == id)
    }
}

/// A lesson's steps as a tutor answers them: `{"steps": [...]}`.
#[derive(Deserialize)]
struct StepsDoc {
    steps: Vec<Step>,
}

impl Lesson {
    /// This lesson with the steps in a tutor's answer (`{"steps": [...]}`), checked.
    pub fn with_steps(&self, text: &str) -> Result<Self, CourseError> {
        let doc: StepsDoc = serde_json::from_str(json_span(text))
            .map_err(|e| CourseError::Json { what: "lesson's steps", error: e.to_string() })?;
        let lesson = Lesson { steps: doc.steps, ..self.clone() };
        lesson.check()?;
        Ok(lesson)
    }

    /// Whether its steps can be walked: short, asking at least once, every check answerable.
    pub fn check(&self) -> Result<(), CourseError> {
        let lesson = || self.id.clone();
        if self.steps.len() > STEPS_MAX {
            return Err(CourseError::TooManySteps { lesson: lesson(), steps: self.steps.len() });
        }
        if !self.steps.iter().any(|s| s.check_id().is_some()) {
            return Err(CourseError::NoCheck { lesson: lesson() });
        }
        let mut seen: Vec<&str> = Vec::with_capacity(self.steps.len());
        for s in &self.steps {
            if let Some(id) = s.check_id() {
                if id.trim().is_empty() {
                    return Err(CourseError::Empty { what: "check id", at: format!("lesson {:?}", self.id) });
                }
                if seen.contains(&id) {
                    return Err(CourseError::DuplicateId { id: id.to_owned(), at: format!("lesson {:?}", self.id) });
                }
                seen.push(id);
            }
            match s {
                Step::Read { title, body, .. } => {
                    let words = body.split_whitespace().count();
                    if words > READ_WORDS_MAX {
                        return Err(CourseError::ReadTooLong { lesson: lesson(), title: title.clone(), words });
                    }
                    if body.trim().is_empty() {
                        return Err(CourseError::Empty { what: "body", at: format!("lesson {:?}: {title:?}", self.id) });
                    }
                }
                Step::Choice { id, options, answer, .. } => {
                    if !(2..=OPTIONS_MAX).contains(&options.len()) {
                        return Err(CourseError::Options { lesson: lesson(), id: id.clone(), options: options.len() });
                    }
                    if *answer >= options.len() {
                        return Err(CourseError::AnswerOutOfRange {
                            lesson: lesson(),
                            id: id.clone(),
                            answer: *answer,
                            options: options.len(),
                        });
                    }
                }
                Step::Fill { id, answer, .. } => {
                    if answer.trim().is_empty() {
                        return Err(CourseError::Empty { what: "answer", at: format!("lesson {:?}: check {id:?}", self.id) });
                    }
                }
                Step::Order { id, items, .. } => {
                    if !(2..=OPTIONS_MAX).contains(&items.len()) {
                        return Err(CourseError::Options { lesson: lesson(), id: id.clone(), options: items.len() });
                    }
                }
            }
        }
        Ok(())
    }

    /// The ids of its checks, in order.
    pub fn checks(&self) -> impl Iterator<Item = &str> {
        self.steps.iter().filter_map(Step::check_id)
    }
}

/// The key a check's memory is kept under: its course, lesson and own id.
pub fn check_key(course: &str, lesson: &str, check: &str) -> String {
    format!("{course}/{lesson}/{check}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const LESSON: &str = r#"Here you go:
```json
{"steps": [
  {"kind": "read", "title": "A borrow", "body": "A reference borrows a value without owning it."},
  {"kind": "choice", "id": "c1", "prompt": "Who owns `s` after `let r = &s;`?", "options": ["r", "s"], "answer": 1, "why": "A borrow never moves ownership."},
  {"kind": "fill", "id": "f1", "prompt": "A `&mut T` is a ___ borrow.", "answer": "mutable", "accept": ["unique", "exclusive"], "why": "Only one may exist at a time."},
  {"kind": "order", "id": "o1", "prompt": "Order of a value's life", "items": ["created", "borrowed", "dropped"], "why": "A borrow cannot outlive its value."}
]}
```"#;

    fn outline() -> Lesson {
        Lesson { id: "l1".into(), title: "Borrows".into(), about: "What a reference is".into(), steps: Vec::new() }
    }

    #[test]
    fn a_tutors_fenced_answer_parses_into_a_lesson() {
        let l = outline().with_steps(LESSON).unwrap();
        assert_eq!(l.steps.len(), 4);
        assert_eq!(l.checks().collect::<Vec<_>>(), ["c1", "f1", "o1"]);
    }

    #[test]
    fn a_lesson_that_asks_nothing_is_refused() {
        let text = r#"{"steps": [{"kind": "read", "title": "t", "body": "words"}]}"#;
        assert_eq!(outline().with_steps(text), Err(CourseError::NoCheck { lesson: "l1".into() }));
    }

    #[test]
    fn an_answer_past_the_options_is_refused_by_name() {
        let text = r#"{"steps": [{"kind": "choice", "id": "c", "prompt": "p", "options": ["a", "b"], "answer": 2, "why": "w"}]}"#;
        let e = outline().with_steps(text).unwrap_err();
        assert_eq!(e, CourseError::AnswerOutOfRange { lesson: "l1".into(), id: "c".into(), answer: 2, options: 2 });
        assert!(e.to_string().contains("\"c\""));
    }

    #[test]
    fn a_course_outline_needs_lessons_with_distinct_ids() {
        let text = r#"{"id": "rust-borrows", "title": "Borrowing", "goal": "Read borrow errors", "lessons": [
            {"id": "a", "title": "One", "about": "x"}, {"id": "a", "title": "Two", "about": "y"}]}"#;
        assert!(matches!(Course::parse(text), Err(CourseError::DuplicateId { .. })));
    }
}
