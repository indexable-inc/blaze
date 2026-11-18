//! What a person typed asks for: to understand something (a lesson) or to have something done (an agent's work).
//!
//! One field takes everything; nobody picks a mode. Asking to understand ("explain bloom filters", "how does TCP
//! congestion control work", "what is a monad", a bare topic like "bloom filters") is answered by a short lesson that
//! asks back, because a person keeps what they answer and loses what they only read. Asking for work ("fix the failing
//! test", "write a script that ...") goes to an agent.

/// What a message asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Intent {
    /// To understand `topic`: taught as a lesson.
    Learn { topic: String },
    /// Something done: an agent's work.
    Do,
}

/// The topic a learner who asks for "something" gets: the tutor picks.
pub const SURPRISE: &str = "one genuinely useful idea from computer science that most programmers never learned \
properly (your pick; name it in the course title)";

/// Openings that ask to be taught, and the words to drop after them to leave the topic.
const TEACH: [&str; 12] = [
    "teach me",
    "i want to learn",
    "i'd like to learn",
    "help me learn",
    "help me understand",
    "i want to understand",
    "explain to me",
    "explain",
    "eli5",
    "tell me about",
    "intro to",
    "introduction to",
];

/// Openings of a question about how something is: a lesson, unless the question is about the person's own work.
const QUESTION: [&str; 14] = [
    "what is",
    "what are",
    "what's",
    "whats",
    "what does",
    "how does",
    "how do",
    "how is",
    "how are",
    "why does",
    "why do",
    "why is",
    "why are",
    "difference between",
];

/// Words that make a message about work in front of the person (their code, a file, a run): an agent's, not a lesson.
const WORK: [&str; 31] = [
    "fix", "build", "make", "write", "create", "add", "run", "deploy", "install", "set up", "setup", "open", "find",
    "check", "update", "refactor", "debug", "rename", "delete", "remove", "commit", "push", "merge", "review",
    "implement", "change", "move", "test", "my ", "this ", "our ",
];

/// What `message` asks for.
#[must_use]
pub fn of(message: &str) -> Intent {
    let text = message.trim();
    let lower = text.to_lowercase();
    // A path, a URL or code is about something in front of the person.
    if text.contains(['/', '`', '{', '\n']) || lower.contains("http") {
        return teach(text, &lower).map_or(Intent::Do, |topic| Intent::Learn { topic });
    }
    if let Some(topic) = teach(text, &lower) {
        return Intent::Learn { topic };
    }
    let words = lower.split_whitespace().count();
    let starts_work = WORK.iter().any(|w| lower.starts_with(w) || lower.starts_with(&format!("please {w}")));
    let about_mine = [" my ", " this ", " our ", " the repo", " the build", " the test"].iter().any(|w| lower.contains(w));
    if let Some(q) = QUESTION.iter().find(|q| lower.starts_with(*q)) {
        if !about_mine {
            return Intent::Learn { topic: clean(&text[q.len()..]).to_owned() };
        }
        return Intent::Do;
    }
    // A bare topic: a few words with no verb of work and no question of another kind ("bloom filters").
    if words <= 5 && !starts_work && !about_mine && !lower.ends_with('?') && lower.chars().any(char::is_alphabetic) {
        return Intent::Learn { topic: clean(text).to_owned() };
    }
    Intent::Do
}

/// The topic after an opening that asks to be taught anywhere in `text` ("hello, teach me something"), if one does.
fn teach(text: &str, lower: &str) -> Option<String> {
    let (at, len) = TEACH.iter().filter_map(|k| lower.find(k).map(|i| (i, k.len()))).min_by_key(|&(i, _)| i)?;
    // "explain" only opens a request at the start ("can you explain" too): mid-sentence it is a word ("this doesn't
    // explain the crash").
    let opening = lower[..at].trim();
    if !opening.is_empty() && !["can you", "could you", "please", "hey", "hi", "hello", "pls"].iter().any(|p| opening.trim_end_matches([',', '!']).ends_with(p)) {
        return None;
    }
    // Lowercasing changes lengths only outside ASCII: cut where the original has a boundary.
    let rest = text.get(at + len..).unwrap_or("");
    let rest = clean(rest);
    let rest = ["about ", "how to ", "how ", "to ", "me ", "what "].iter().find_map(|p| rest.strip_prefix(p)).unwrap_or(rest).trim();
    let vague = rest.chars().filter(|c| c.is_alphanumeric()).count() < 3
        || ["something", "somthing", "anything", "a thing", "stuff"].iter().any(|v| rest.to_lowercase().starts_with(v));
    Some(if vague { SURPRISE.to_owned() } else { rest.to_owned() })
}

/// A topic without the punctuation and filler around it ("bloom filters work?" is "bloom filters").
fn clean(s: &str) -> &str {
    let s = s.trim().trim_start_matches([',', ':', '-']).trim().trim_end_matches(['?', '.', '!']).trim();
    [" work", " works", " actually work", " really work"].iter().rev().find_map(|w| s.strip_suffix(w)).unwrap_or(s).trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn learns(m: &str) -> String {
        match of(m) {
            Intent::Learn { topic } => topic,
            Intent::Do => panic!("{m:?} was taken as work"),
        }
    }

    #[test]
    fn asking_to_understand_is_a_lesson() {
        assert_eq!(learns("explain bloom filters"), "bloom filters");
        assert_eq!(learns("Explain how bloom filters work?"), "bloom filters");
        assert_eq!(learns("how does TCP congestion control work"), "TCP congestion control");
        assert_eq!(learns("what is a monad?"), "a monad");
        assert_eq!(learns("bloom filters"), "bloom filters");
        assert_eq!(learns("can you explain raft"), "raft");
        assert_eq!(learns("teach me about lifetimes"), "lifetimes");
        assert_eq!(learns("hello teach me somthing?"), SURPRISE);
    }

    #[test]
    fn asking_for_work_is_an_agents() {
        for m in [
            "fix the failing test in crates/harness",
            "write a script that renames my photos",
            "why does my build fail",
            "what is wrong with this function",
            "deploy the site",
            "this doesn't explain the crash, look again at the logs please",
        ] {
            assert_eq!(of(m), Intent::Do, "{m:?}");
        }
    }
}
