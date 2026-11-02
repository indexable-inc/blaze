//! The program's asks to the daemon over blaze-inproc's channel ([`crate::patch::ask`]): a
//! message or the tiers' text read a piece at a time, and an answer told.

use blaze_inproc::ask::MOST_BODY;
use blaze_inproc::frame::{Kind, kinds};
use blaze_inproc::runtime::Attached;

use crate::patch::{Answer, ask};

/// The longest whole taken: a longer length the daemon says is refused, not reserved.
const MOST_WHOLE: u64 = 1 << 30;

/// The kinds of one whole's asks: the ask, the answer with a piece, and the answer when the
/// daemon has none.
pub(crate) struct Kinds {
    pub(crate) ask: Kind,
    pub(crate) piece: Kind,
    pub(crate) none: Kind,
}

/// Reads a whole into `out` (cleared first, its buffer kept), a piece per ask of `body(offset)`,
/// until the pieces, all naming one whole, add up to it: the whole's name, or `None` when the
/// daemon has none. Pieces naming another whole than the first start it again. `what` names it
/// in errors.
pub(crate) fn fetch<const N: usize>(
    attached: &Attached,
    kinds: &Kinds,
    what: &str,
    body: impl Fn(u64) -> [u8; N],
    out: &mut Vec<u8>,
) -> Result<Option<u64>, String> {
    let mut answer = [0u8; MOST_BODY];
    let mut whole: Option<(u64, u64)> = None;
    out.clear();
    loop {
        let offset = out.len() as u64;
        let (kind, len) = attached
            .request_into(kinds.ask, &body(offset), &mut answer)
            // alloc: error path
            .map_err(|it| format!("asking the daemon for {what}: {it}"))?;
        let got = &answer[..len.min(answer.len())];
        if kind == kinds.none {
            // Between two pieces: the daemon dropped the whole, and has nothing now.
            out.clear();
            return Ok(None);
        }
        if kind == kinds::REFUSED {
            // alloc: error path
            return Err(format!(
                "the daemon refused the ask for {what}: {}",
                String::from_utf8_lossy(got)
            ));
        }
        if kind != kinds.piece {
            // alloc: error path
            return Err(format!(
                "the daemon answered the ask for {what} with a frame of kind {:#06x}",
                kind.0
            ));
        }
        // alloc: error path
        let (name, total, bytes) =
            ask::read_piece(got).map_err(|why| format!("the daemon's answer for {what}: {why}"))?;
        if whole.is_some_and(|it| it != (name, total)) {
            out.clear();
            whole = None;
            continue;
        }
        if whole.is_none() {
            if total > MOST_WHOLE {
                // alloc: error path
                return Err(format!(
                    "the daemon's {what} is {total} bytes, past the {MOST_WHOLE} taken"
                ));
            }
            out.reserve(total as usize);
            whole = Some((name, total));
        }
        let end = offset + bytes.len() as u64;
        if end > total || (bytes.is_empty() && offset < total) {
            // alloc: error path
            return Err(format!(
                "the daemon's piece of {what} at byte {offset} has {} bytes of {total}",
                bytes.len()
            ));
        }
        out.extend_from_slice(bytes);
        if end == total {
            return Ok(Some(name));
        }
    }
}

/// Tells the daemon `said`, encoded into `buffer` (kept from answer to answer). A reason too
/// long for one ask is cut at a character boundary, and says so.
pub(crate) fn tell(attached: &Attached, said: &Answer, buffer: &mut Vec<u8>) -> Result<(), String> {
    buffer.clear();
    if said.encoded_len() <= MOST_BODY {
        said.encode_into(buffer);
    } else {
        cut(said, MOST_BODY).encode_into(buffer);
    }
    let mut refused = [0u8; 512];
    let (kind, len) = attached
        .request_into(ask::ANSWER, buffer, &mut refused)
        // alloc: error path
        .map_err(|it| format!("telling the daemon: {it}"))?;
    match kind {
        ask::ANSWERED => Ok(()),
        kinds::REFUSED => {
            let why = String::from_utf8_lossy(&refused[..len.min(refused.len())]);
            // alloc: error path
            Err(format!("the daemon refused the answer: {why}"))
        }
        // alloc: error path
        other => {
            Err(format!("the daemon answered an answer with a frame of kind {:#06x}", other.0))
        }
    }
}

/// What a reason cut to fit says at its end.
const CUT: &str = " (cut to fit one answer)";

/// `said` with its reason cut so it encodes in at most `most` bytes.
pub(crate) fn cut(said: &Answer, most: usize) -> Answer {
    let over = said.encoded_len().saturating_sub(most);
    // alloc: a copy of what fits, the caller's to keep; error path
    let why = |why: &str| if over == 0 { why.to_owned() } else { shorten(why, over) };
    match said {
        Answer::Applied { generation } => Answer::Applied { generation: *generation },
        Answer::RolledBack { generation, to, why: reason } => {
            Answer::RolledBack { generation: *generation, to: *to, why: why(reason) }
        }
        Answer::Declined { generation, why: reason } => {
            Answer::Declined { generation: *generation, why: why(reason) }
        }
    }
}

/// `why` less at least `over` bytes, ending in [`CUT`].
fn shorten(why: &str, over: usize) -> String {
    let mut end = why.len().saturating_sub(over + CUT.len());
    while !why.is_char_boundary(end) {
        end -= 1;
    }
    // alloc: a reason too long for an answer, once per such answer
    let mut out = String::with_capacity(end + CUT.len());
    out.push_str(&why[..end]);
    out.push_str(CUT);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reason_too_long_for_an_answer_is_cut_to_fit_at_a_character() {
        let why = "é".repeat(MOST_BODY);
        let said = Answer::Declined { generation: 4, why };
        let cut = cut(&said, MOST_BODY);
        assert!(cut.encoded_len() <= MOST_BODY, "{}", cut.encoded_len());
        let Answer::Declined { generation: 4, why } = &cut else { panic!("{cut:?}") };
        assert!(why.ends_with(CUT) && why.starts_with("éé"), "{why}");
        assert_eq!(Answer::decode(&cut.encode()), Ok(cut));
        let short = Answer::RolledBack { generation: 2, to: 1, why: "panicked".to_owned() };
        assert_eq!(super::cut(&short, MOST_BODY), short, "what fits is kept");
    }
}
