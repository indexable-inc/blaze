//! How much of the person's plan is used, at the sidebar's foot (scene/widgets/frame.flecs `hn.SideStatus`; the meter is
//! scene/widgets/chat.flecs `hn.ChatPlan`): the plan's window nearest its limit ([`shown`]) as a small bar filling and quiet words ("79% of
//! week", [`words`]); muted, the warning tone once the agent warns of it, the failed tone once it refuses for it (the
//! context meter's tones, hn.ChatContextTone). Its tooltip ([`tip`]) says every window, how much of each is used and when
//! each resets in plain words, and that the plan is the account's, not this chat's.
//!
//! The windows are the agent's last `plan` event (harness-proto `Body::Plan`: claude's `rate_limit_event`, codex's
//! `account/rateLimits/updated`). The plan is the account's, so a chat shows the latest any chat of its agent kind was
//! told ([`App::chat_plan`]), and nothing until one was. An offscreen shot can stand one in
//! (`HARNESS_NATIVE_SHOT_PLAN`, [`App::set_shot_plan`]).
use super::pick::{TRACK_ALPHA, fill_of, set_fill};
use super::*;
use harness_proto::event::PlanWindow;

/// The window the bar shows: the one nearest its limit, a refused one first, then one the agent warns of, then the most
/// used; on a tie the first (the shorter: the adapters list them shortest first). None for no windows.
pub(super) fn shown(windows: &[PlanWindow]) -> Option<&PlanWindow> {
    windows.iter().rev().max_by(|a, b| (a.limited, a.warn).cmp(&(b.limited, b.warn)).then(a.used.total_cmp(&b.used)))
}

/// A share used as a whole percentage, 0 to 100.
fn pct(used: f64) -> u32 {
    (used * 100.0).round().clamp(0.0, 100.0) as u32
}

/// The bar's words for window `w`: "79% of week", "8% of 5h".
pub(super) fn words(w: &PlanWindow) -> String {
    format!("{}% of {}", pct(w.used), w.name)
}

/// Window `name` (an adapter's short one) as the tooltip says it: `5h` is the 5-hour window, `week` Weekly, `day` Daily,
/// `Opus week` Opus weekly, `3d` the 3-day window; any other as it is.
fn long_name(name: &str) -> String {
    let count = |unit: char, word: &str| {
        name.strip_suffix(unit).filter(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit())).map(|n| format!("{n}-{word} window"))
    };
    match name {
        "week" => "Weekly".to_owned(),
        "day" => "Daily".to_owned(),
        n => n
            .strip_suffix(" week")
            .map(|m| format!("{m} weekly"))
            .or_else(|| count('h', "hour"))
            .or_else(|| count('d', "day"))
            .or_else(|| count('m', "minute"))
            .unwrap_or_else(|| n.to_owned()),
    }
}

/// A moment as this machine's clock reads it: its day (local days since the epoch, so two moments on one date share
/// it), weekday (0 Sunday), month (0 January), day of the month, hour and minute.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Clock {
    day: i64,
    wday: usize,
    mon: usize,
    mday: i32,
    hour: i32,
    min: i32,
}

impl Clock {
    /// Unix second `secs` in the local time zone.
    fn local(secs: u64) -> Self {
        let t = secs as libc::time_t;
        // SAFETY: localtime_r writes the broken-down time into `tm`, which lives for the call (as archive.rs's).
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        unsafe { libc::localtime_r(&t, &mut tm) };
        Self {
            day: (secs as i64 + i64::from(tm.tm_gmtoff)).div_euclid(86_400),
            wday: tm.tm_wday.clamp(0, 6) as usize,
            mon: tm.tm_mon.clamp(0, 11) as usize,
            mday: tm.tm_mday,
            hour: tm.tm_hour,
            min: tm.tm_min,
        }
    }
}

/// Whether unix second `secs` is at night here (from seven in the evening to six in the morning): the music's scale.
pub(in crate::app) fn night(secs: u64) -> bool {
    let h = Clock::local(secs).hour;
    !(6..19).contains(&h)
}

const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// An hour and minute as a person says them: "3:10 pm", "4 pm", "12 am".
fn clock_words(hour: i32, min: i32) -> String {
    let (h, half) = match hour {
        0 => (12, "am"),
        1..=11 => (hour, "am"),
        12 => (12, "pm"),
        _ => (hour - 12, "pm"),
    };
    if min == 0 { format!("{h} {half}") } else { format!("{h}:{min:02} {half}") }
}

/// When a window resets (`at`), in plain words from `now`: "at 3:10 pm" today, "tomorrow at 9 am", "Fri 4 pm" within
/// the week, "Nov 16" past it.
fn reset_words(at: Clock, now: Clock) -> String {
    let time = clock_words(at.hour, at.min);
    match at.day - now.day {
        ..=0 => format!("at {time}"),
        1 => format!("tomorrow at {time}"),
        2..=6 => format!("{} {time}", DAYS[at.wday]),
        _ => format!("{} {}", MONTHS[at.mon], at.mday),
    }
}

/// The plan meter's tooltip for `windows` as agent kind `agent`'s maker counts them, at unix second `now` (`local`
/// reads a unix second on this machine's clock): each window, how much of it is used and when it resets ("5-hour window
/// 8%, resets at 3:10 pm · Weekly 79%, resets Fri 4 pm"), then whose plan it is.
fn tip(windows: &[PlanWindow], agent: &str, now: u64, local: impl Fn(u64) -> Clock) -> String {
    let today = local(now);
    let each: Vec<String> = windows // alloc: a few windows' words, while the plan shows
        .iter()
        .map(|w| {
            let used = if w.limited { "used up".to_owned() } else { format!("{}%", pct(w.used)) };
            match w.resets_at {
                Some(at) => format!("{} {used}, resets {}", long_name(&w.name), reset_words(local(at), today)),
                None => format!("{} {used}", long_name(&w.name)),
            }
        })
        .collect();
    let who = match agent {
        "claude" => "Claude",
        "codex" => "Codex",
        "cursor" => "Cursor",
        other => other,
    };
    format!("{}\nYour account's {who} plan, shared by all its chats: not this chat's alone", each.join(" \u{b7} "))
}

/// hn.ChatContextTone's slot for window `w`: full once the agent refuses for it, warn once it warns of it, else ok.
fn tone_of(w: &PlanWindow) -> &'static str {
    if w.limited {
        "hn.ChatContextTone.full"
    } else if w.warn {
        "hn.ChatContextTone.warn"
    } else {
        "hn.ChatContextTone.ok"
    }
}

/// The plan meter in `slot` (the status bar's `plan`) for `windows` (none: no meter, no room), as agent kind `agent`'s
/// maker counts them, at unix second `now`: the shown window's bar filling in its tone over a track of `ink` faintly,
/// its words in its tone (`worded`; else the bar alone, the words leading its tooltip), the tooltip saying every window.
pub(in crate::app) fn fill_plan(slot: View<'_>, windows: &[PlanWindow], agent: &str, ink: Fill, now: u64, worded: bool) {
    let w = slot.entity().world();
    view::for_each_of(slot, shown(windows).map(|s| ("plan", "hn.ChatPlan", s)), |v, s, _| {
        let tone = fill_of(w, tone_of(s));
        let text = tip(windows, agent, now, Clock::local);
        let text = if worded { text } else { format!("{}\n{text}", words(s)) };
        if v.entity().try_cloned::<&Tooltip>().is_none_or(|t| t.text.as_str() != text) {
            v.set(Tooltip { text: text.as_str().into() });
        }
        if let Some(p) = v.part("words") {
            // Where room is short (the sidebar's foot) the bar alone shows; its words lead the tooltip.
            p.text(&if worded { words(s) } else { String::new() });
            set_fill(p, tone);
        }
        let Some(track) = v.part("track") else { return };
        set_fill(track, Fill { dark: Rgba { a: TRACK_ALPHA, ..ink.dark }, light: Rgba { a: TRACK_ALPHA, ..ink.light } });
        let Some(size) = track.entity().try_cloned::<&Size>() else { return };
        let Some(bar) = track.part("fill") else { return };
        // As long as the share used, a dot (the bar's own height) at the least once any is used.
        let (full, tall) = (size.w.value, size.h.value);
        let long = if s.used > 0.0 { (full * s.used as f32).clamp(tall, full) } else { 0.0 };
        let fill = Size { w: Sizing::fixed(long), h: Sizing::fixed(tall) };
        if bar.entity().try_cloned::<&Size>() != Some(fill) {
            bar.set(fill);
        }
        set_fill(bar, tone);
    });
}

impl App {
    /// The plan as the latest any chat of agent kind `kind` was told (it is the account's, not one chat's): its windows,
    /// none before any said. An offscreen shot's stand-in ([`App::set_shot_plan`]) wins.
    pub(in crate::app) fn chat_plan(&self, kind: &str) -> Vec<PlanWindow> {
        if !self.shot_plan.is_empty() {
            return self.shot_plan.clone();
        }
        let chats = self.chats.borrow();
        // alloc: a few windows, once a frame for the chat in view
        chats.values().filter(|t| t.agent == kind && !t.plan.is_empty()).max_by_key(|t| t.plan_at).map(|t| t.plan.clone()).unwrap_or_default()
    }

    /// For an offscreen shot (shot.rs `HARNESS_NATIVE_SHOT_PLAN`): every chat's status bar shows the plan `spec` says, as
    /// if its agent had reported it. `spec` is `name=share` windows, comma-separated (`5h=0.08,week=0.79`), a `!` after a
    /// share for a window the agent refuses for; each warns from [`PlanWindow::WARN`] of it used, the five-hour one
    /// resetting two hours from now and any other three days on.
    pub fn set_shot_plan(&mut self, spec: &str) -> Result<(), String> {
        self.shot_plan = shot_plan(spec, model::now_ms() / 1000)?;
        Ok(())
    }
}

/// [`App::set_shot_plan`]'s windows for `spec` at unix second `now`, or why `spec` is not one.
fn shot_plan(spec: &str, now: u64) -> Result<Vec<PlanWindow>, String> {
    spec.split(',')
        .map(|w| {
            let bad = || format!("HARNESS_NATIVE_SHOT_PLAN={spec}: {w:?} is not name=share (5h=0.08, week=1!)");
            let (name, share) = w.trim().split_once('=').ok_or_else(bad)?;
            let (share, limited) = share.strip_suffix('!').map_or((share, false), |s| (s, true));
            let used: f64 = share.parse().map_err(|_| bad())?;
            let later = if name == "5h" { 2 * 3_600 } else { 3 * 86_400 };
            Ok(PlanWindow { name: name.to_owned(), used, resets_at: Some(now + later), warn: limited || used >= PlanWindow::WARN, limited })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(name: &str, used: f64, warn: bool, limited: bool) -> PlanWindow {
        PlanWindow { name: name.into(), used, resets_at: None, warn, limited }
    }

    /// The bar shows the window nearest its limit: the most used, but one the agent warns of over a fuller unwarned one,
    /// a refused one over all; on a tie the first.
    #[test]
    fn the_bar_shows_the_window_nearest_its_limit() {
        let name = |ws: &[PlanWindow]| shown(ws).map(|w| w.name.clone());
        assert_eq!(name(&[]), None);
        assert_eq!(name(&[window("5h", 0.08, false, false), window("week", 0.79, true, false)]).as_deref(), Some("week"));
        assert_eq!(name(&[window("5h", 0.62, false, false), window("week", 0.41, false, false)]).as_deref(), Some("5h"));
        assert_eq!(name(&[window("5h", 0.9, false, false), window("week", 0.76, true, false)]).as_deref(), Some("week"));
        assert_eq!(name(&[window("5h", 1.0, true, true), window("week", 0.95, true, false)]).as_deref(), Some("5h"));
        assert_eq!(name(&[window("5h", 0.0, false, false), window("week", 0.0, false, false)]).as_deref(), Some("5h"));
    }

    /// Its words: the share as a whole percentage, then the window's name.
    #[test]
    fn the_words_say_the_share_of_the_window() {
        assert_eq!(words(&window("week", 0.786, true, false)), "79% of week");
        assert_eq!(words(&window("5h", 0.08, false, false)), "8% of 5h");
        assert_eq!(words(&window("week", 1.2, true, true)), "100% of week");
        assert_eq!((long_name("5h"), long_name("week"), long_name("Opus week"), long_name("3d")), ("5-hour window".into(), "Weekly".into(), "Opus weekly".into(), "3-day window".into()));
    }

    /// Resets in plain words: the time alone today, tomorrow by name, a weekday within the week, a date past it; the
    /// tooltip says every window and whose plan it is.
    #[test]
    fn resets_in_plain_words() {
        let at = |day: i64, wday: usize, hour: i32, min: i32| Clock { day, wday, mon: 10, mday: 9 + (day - 100) as i32, hour, min };
        let now = at(100, 1, 11, 0);
        assert_eq!(reset_words(at(100, 1, 15, 10), now), "at 3:10 pm");
        assert_eq!(reset_words(at(101, 2, 9, 0), now), "tomorrow at 9 am");
        assert_eq!(reset_words(at(104, 5, 16, 0), now), "Fri 4 pm");
        assert_eq!(reset_words(at(107, 1, 0, 30), now), "Nov 16");
        assert_eq!(clock_words(0, 5), "12:05 am");
        let ws = [
            PlanWindow { resets_at: Some(1), ..window("5h", 0.08, false, false) },
            PlanWindow { resets_at: Some(2), ..window("week", 0.79, true, false) },
        ];
        let local = |s: u64| match s {
            1 => at(100, 1, 15, 10),
            2 => at(104, 5, 16, 0),
            _ => now,
        };
        assert_eq!(
            tip(&ws, "claude", 0, local),
            "5-hour window 8%, resets at 3:10 pm \u{b7} Weekly 79%, resets Fri 4 pm\nYour account's Claude plan, shared by all its chats: not this chat's alone"
        );
    }

    /// A shot's stand-in reads its windows, warns past the threshold, and names a bad spec.
    #[test]
    fn a_shot_stands_in_a_plan() {
        let ws = shot_plan("5h=0.08,week=0.79", 1_000).unwrap();
        assert_eq!(ws.iter().map(|w| (w.name.as_str(), w.warn, w.limited, w.resets_at)).collect::<Vec<_>>(), [("5h", false, false, Some(8_200)), ("week", true, false, Some(260_200))]);
        assert!(shot_plan("week=1!", 0).unwrap()[0].limited);
        assert!(shot_plan("week", 0).unwrap_err().contains("week"));
    }
}
