//! What a tutor (an agent) is asked, as plain text: a course's outline, then each lesson's steps when it is opened.
//!
//! The rules in [`steps`] are the design of the lessons: they are what makes a course here active (asked, not told),
//! short, and bent toward what this person keeps missing.

use crate::course::{Course, Lesson, OPTIONS_MAX, READ_WORDS_MAX, STEPS_MAX, Step};

/// Ask for a course on `topic` with its first lesson written: its title, goal and three to six lessons, the first with
/// its steps, so the learner starts answering after one wait; the rest are written when opened ([`steps`]).
pub fn outline(topic: &str) -> String {
    format!(
        "You are a tutor. A learner asked about:\n\n{topic}\n\n\
Plan a short, active course for them: 3 to 6 lessons that build on each other, each teaching ONE idea that fits in \
about three minutes. The first lesson must stand alone: someone who stops after it has understood the heart of what \
they asked. Start where a newcomer to exactly this topic has to start, and end with them able to do something real.\n\n\
Write the FIRST lesson's steps now; the others get only a title and their one idea.\n\n{rules}\n\n\
Answer with JSON only, no other words:\n\
{{\"id\": \"kebab-case-id\", \"title\": \"at most 5 words\", \"goal\": \"one line: what they can do at the end\", \
\"lessons\": [{{\"id\": \"kebab-case\", \"title\": \"at most 5 words\", \"about\": \"one line: the one idea\", \
\"steps\": {shape}}}, {{\"id\": \"...\", \"title\": \"...\", \"about\": \"...\"}}]}}\n\
Every check's `id` is short and unique in the lesson. Do not use tools; answer from what you know.",
        rules = rules(),
        shape = SHAPE,
    )
}

/// The shape of a lesson's steps in the JSON asked for: one of each kind.
const SHAPE: &str = "[\n\
  {\"kind\": \"read\", \"title\": \"...\", \"body\": \"...\", \"code\": \"\"},\n\
  {\"kind\": \"choice\", \"id\": \"c1\", \"prompt\": \"...\", \"code\": \"\", \"options\": [\"...\"], \"answer\": 0, \"why\": \"...\"},\n\
  {\"kind\": \"fill\", \"id\": \"f1\", \"prompt\": \"... ___ ...\", \"answer\": \"...\", \"accept\": [], \"why\": \"...\"},\n\
  {\"kind\": \"order\", \"id\": \"o1\", \"prompt\": \"...\", \"items\": [\"...\"], \"why\": \"...\"}\n\
]";

/// How a lesson is written: the design of every lesson, in the tutor's words. People learn by answering, not by
/// reading.
fn rules() -> String {
    format!(
        "Write a lesson as steps. People learn by answering, not by reading, so follow every rule:\n\
1. 8 to 12 steps (never more than {STEPS_MAX}). Most are checks. Never two `read` steps in a row.\n\
2. A `read` tells ONE small idea in at most 60 words (hard limit {READ_WORDS_MAX}), plain and concrete, with one tiny \
example. Put code in `code`, not in `body`.\n\
3. Every idea a `read` tells is asked back by the very next step, in a new example, so it takes thinking and not \
matching words.\n\
4. Checks get harder through the lesson: recall it, then apply it, then predict what happens or what breaks. The last \
two checks mix ideas from the whole lesson.\n\
5. `choice`: 3 or 4 options (at most {OPTIONS_MAX}). The wrong options are the mistakes a learner really makes. \
`answer` is the 0-based index of the right one; vary where it is.\n\
6. `fill`: the answer is one word or one short token; list other spellings that should count in `accept`.\n\
7. `order`: 3 to 5 `items`, given in the correct order (they are shown shuffled).\n\
8. `why`: one or two sentences: why the right answer is right, and why the tempting wrong one is wrong.\n\
9. No trick questions, no \"all of the above\", no questions about wording."
    )
}

/// One earlier check the learner missed, as the tutor is told it.
fn missed_line(course: &Course, lesson: &str, check: &str) -> Option<String> {
    let step = course.lesson(lesson)?.steps.iter().find(|s| s.check_id() == Some(check))?;
    let prompt = match step {
        Step::Choice { prompt, .. } | Step::Fill { prompt, .. } | Step::Order { prompt, .. } => prompt,
        Step::Read { .. } => return None,
    };
    Some(format!("- {prompt}"))
}

/// Ask for `lesson`'s steps. `shaky` are earlier checks of `course` the learner keeps missing
/// ([`crate::Progress::shaky`]): the lesson asks each again in a new way, so the course bends toward them.
pub fn steps(course: &Course, lesson: &Lesson, shaky: &[(&str, &str)]) -> String {
    let at = course.lessons.iter().position(|l| l.id == lesson.id).map_or(0, |i| i + 1);
    let mut before = String::new();
    for l in course.lessons.iter().take(at.saturating_sub(1)) {
        before.push_str(&format!("- {}: {}\n", l.title, l.about));
    }
    if before.is_empty() {
        before.push_str("(none: this is the first lesson)\n");
    }
    let mut missed = String::new();
    for (l, c) in shaky.iter().take(4) {
        if let Some(line) = missed_line(course, l, c) {
            missed.push_str(&line);
            missed.push('\n');
        }
    }
    let missed = if missed.is_empty() {
        String::new()
    } else {
        format!(
            "\nThe learner missed these earlier questions. Ask each idea again here in a NEW example (never the same \
wording), early in the lesson:\n{missed}"
        )
    };
    format!(
        "You are a tutor writing one lesson of the course \"{title}\" (goal: {goal}).\n\n\
Lesson {at} of {n}: \"{lt}\". Its one idea: {about}\n\n\
Lessons the learner already finished:\n{before}{missed}\n\
{rules}\n\n\
Answer with JSON only, no other words:\n\
{{\"steps\": {shape}}}\n\
Every check's `id` is short and unique in the lesson. Do not use tools; answer from what you know.",
        title = course.title,
        goal = course.goal,
        rules = rules(),
        shape = SHAPE,
        n = course.lessons.len(),
        lt = lesson.title,
        about = lesson.about,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lessons_prompt_names_what_came_before_and_what_was_missed() {
        let first = Lesson {
            id: "a".into(),
            title: "Owning".into(),
            about: "One owner".into(),
            steps: vec![Step::Fill { id: "f".into(), prompt: "A value has one ___".into(), answer: "owner".into(), accept: Vec::new(), why: "w".into() }],
        };
        let second = Lesson { id: "b".into(), title: "Borrowing".into(), about: "References".into(), steps: Vec::new() };
        let c = Course { id: "c".into(), title: "Ownership".into(), goal: "Read errors".into(), lessons: vec![first, second.clone()] };
        let p = steps(&c, &second, &[("a", "f")]);
        assert!(p.contains("Lesson 2 of 2: \"Borrowing\""));
        assert!(p.contains("- Owning: One owner"));
        assert!(p.contains("- A value has one ___"));
        assert!(outline("rust lifetimes").contains("rust lifetimes"));
    }
}
