//! Typing a message into a session and submitting it, per agent kind.
//!
//! Default: bracketed paste (multi-line text arrives as one input), a short pause, then `\r`.
//!
//! cursor-agent needs more. Right after launch it draws its input box and accepts typing, but drops
//! Enter until its session is set up (a few seconds), so the message sits there unsent. Typing sent
//! before its TUI is up at all can be lost. So for cursor: wait until the input box is drawn, paste,
//! press Enter, then keep pressing Enter while the message is still in the box. While it is busy,
//! Enter queues the message (the box clears), so this also covers sends during a turn.
use crate::daemon::Session;
use anyhow::{Result, bail};
use std::time::{Duration, Instant};

pub fn send(s: &Session, text: &str, submit: bool) -> Result<()> {
    if s.kind == "cursor" {
        wait_for(s, Duration::from_secs(30), |screen| cursor_input(screen).is_some());
    }
    s.write(format!("\x1b[200~{text}\x1b[201~").as_bytes())?;
    if !submit {
        return Ok(());
    }
    // Let the TUI finish handling the paste before Enter, or it may land inside it.
    std::thread::sleep(Duration::from_millis(150));
    s.write(b"\r")?;
    if s.kind == "cursor" {
        cursor_confirm(s, text)?;
    }
    Ok(())
}

fn cursor_confirm(s: &Session, text: &str) -> Result<()> {
    let needle: String = squash(text.lines().find(|l| !l.trim().is_empty()).unwrap_or("")).chars().take(24).collect();
    if needle.is_empty() {
        return Ok(());
    }
    let pending = |screen: &str| cursor_input(screen).is_some_and(|i| squash(&i).contains(&needle));
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        std::thread::sleep(Duration::from_millis(400));
        if !pending(&s.screen_text()) {
            return Ok(());
        }
        if Instant::now() > deadline {
            bail!("cursor kept the message in its input box for 60s without submitting it");
        }
        s.write(b"\r")?;
    }
}

/// The text of cursor's input box: the rows between its `▄▄▄` top and `▀▀▀` bottom borders.
fn cursor_input(screen: &str) -> Option<String> {
    let lines: Vec<&str> = screen.lines().collect();
    let top = lines.iter().rposition(|l| l.trim_start().starts_with("▄▄▄"))?;
    let bottom = top + 1 + lines[top + 1..].iter().position(|l| l.trim_start().starts_with("▀▀▀"))?;
    Some(lines[top + 1..bottom].join(" ").trim().trim_start_matches('→').to_string())
}

fn squash(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

fn wait_for(s: &Session, timeout: Duration, ok: impl Fn(&str) -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if ok(&s.screen_text()) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    false
}
