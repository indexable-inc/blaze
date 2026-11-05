//! What a reply asks Kip to do: `<Kip mood="wave"/>` (reply.rs), or a `Kip` element of generated UI. Its `mood` is a word
//! or a few in turn (`"wave then idle"`, `"thinking, celebrate"`), read by [`moods`] into the character's own [`Mood`]s;
//! an [`Act`] plays them, each for its clip's length (a mood that holds, for [`HOLD`]), and the last one stays.
use super::anim::{Mascot, Mood};

/// The most moods one act plays in turn.
pub const MAX_STEPS: usize = 6;

/// How long a mood that holds (thinking, working, confused, asleep, idle) stays before the next one, seconds.
pub const HOLD: f32 = 1.8;

/// The moods a word stands for: Kip's own names, and the words an agent reaches for (`hello`, `celebrate`, `oops`).
/// `celebrate` is two hops for joy. `None` for a word it does not know.
#[must_use]
pub fn named(word: &str) -> Option<&'static [Mood]> {
    Some(match word {
        "wave" | "hello" | "hi" | "hey" | "greet" | "bye" | "goodbye" => &[Mood::Wave],
        "happy" | "glad" | "joy" | "yay" | "pleased" => &[Mood::Happy],
        "celebrate" | "party" | "hooray" | "cheer" | "woohoo" | "tada" => &[Mood::Happy, Mood::Happy],
        "thinking" | "think" | "hmm" | "ponder" | "curious" => &[Mood::Thinking],
        "working" | "work" | "busy" => &[Mood::Working],
        "oops" | "sorry" | "confused" | "huh" | "uhoh" | "uh-oh" => &[Mood::Confused],
        "sleepy" | "sleeping" | "sleep" | "tired" | "zzz" => &[Mood::Sleeping],
        "idle" | "rest" | "calm" | "still" => &[Mood::Idle],
        _ => return None,
    })
}

/// The moods `words` asks for, in turn: words apart by spaces, commas, `then` or arrows, any case. Words it does not know
/// are passed over; with none it knows (or none at all) Kip waves, which is what showing up is.
#[must_use]
pub fn moods(words: &str) -> Vec<Mood> {
    let mut out = Vec::new();
    for w in words.split(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | '>' | '\u{2192}' | '+')).filter(|w| !w.is_empty()) {
        let w = w.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '-').to_ascii_lowercase();
        for &m in named(&w).unwrap_or_default() {
            if out.len() < MAX_STEPS {
                out.push(m);
            }
        }
    }
    if out.is_empty() {
        out.push(Mood::Wave);
    }
    out
}

/// What VoiceOver says of a Kip asked for `words`: who it is and what it does first.
#[must_use]
pub fn spoken(words: &str) -> String {
    let doing = match moods(words)[0] {
        Mood::Wave => "waving",
        Mood::Happy => "hopping for joy",
        Mood::Thinking => "thinking",
        Mood::Working => "busy",
        Mood::Confused => "puzzled",
        Mood::Sleeping => "asleep",
        Mood::Idle => "standing",
    };
    format!("Kip, {doing}")
}

/// A run of moods being played on one character.
#[derive(Clone, Debug, PartialEq)]
pub struct Act {
    steps: Vec<Mood>,
    /// The step playing, and until when (the character's clock) before the next.
    at: usize,
    until: f64,
    started: bool,
}

impl Act {
    #[must_use]
    pub fn new(steps: Vec<Mood>) -> Self {
        Self { steps: if steps.is_empty() { vec![Mood::Idle] } else { steps }, at: 0, until: 0.0, started: false }
    }

    #[must_use]
    pub fn steps(&self) -> &[Mood] {
        &self.steps
    }

    /// It has begun (played, or put at its end).
    #[must_use]
    pub fn started(&self) -> bool {
        self.started
    }

    /// Its last step is under way or over: nothing more is to come.
    #[must_use]
    pub fn done(&self, kip: &Mascot) -> bool {
        self.started && self.at + 1 >= self.steps.len() && kip.time() >= self.until
    }

    /// Play it on `kip` from its first step.
    pub fn play(&mut self, kip: &mut Mascot) {
        self.started = true;
        self.at = 0;
        self.begin(kip);
    }

    /// Put `kip` where it ends without playing it (a reply read back from history): the last mood that holds, or idle
    /// after a one-shot.
    pub fn settle(&mut self, kip: &mut Mascot) {
        self.started = true;
        self.at = self.steps.len() - 1;
        self.until = kip.time();
        let last = self.steps[self.at];
        kip.set_mood(if last.one_shot() { Mood::Idle } else { last });
    }

    /// The next step once this one has had its time.
    pub fn tick(&mut self, kip: &mut Mascot) {
        if self.started && self.at + 1 < self.steps.len() && kip.time() >= self.until {
            self.at += 1;
            self.begin(kip);
        }
    }

    fn begin(&mut self, kip: &mut Mascot) {
        let m = self.steps[self.at];
        let length = kip.tuning.clip(m).length;
        if m.one_shot() {
            // A one-shot plays over idle: a wave with a hand still at its chin would be two moods at once.
            kip.set_mood(Mood::Idle);
            kip.play(m);
        } else {
            kip.set_mood(m);
            kip.poke();
        }
        let held = if m.one_shot() || length > 0.0 { length } else { HOLD };
        self.until = kip.time() + f64::from(held.max(0.1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_read_as_kips_moods_in_turn() {
        assert_eq!(moods("wave"), [Mood::Wave]);
        assert_eq!(moods("Hello"), [Mood::Wave], "any case, and the words an agent reaches for");
        assert_eq!(moods("wave then idle"), [Mood::Wave, Mood::Idle]);
        assert_eq!(moods("thinking, celebrate"), [Mood::Thinking, Mood::Happy, Mood::Happy], "celebrate is two hops");
        assert_eq!(moods("oops \u{2192} sleepy"), [Mood::Confused, Mood::Sleeping]);
        assert_eq!(moods("sorry."), [Mood::Confused], "punctuation round a word");
        assert_eq!(moods("dance wave"), [Mood::Wave], "a word it does not know is passed over");
        assert_eq!(moods("dance"), [Mood::Wave], "with none it knows, it waves");
        assert_eq!(moods(""), [Mood::Wave]);
        assert_eq!(moods(&"happy ".repeat(20)).len(), MAX_STEPS);
    }
}
