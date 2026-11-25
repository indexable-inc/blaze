//! APC dispatch (ghostty's terminal/apc.zig): the bytes between `ESC _` and the string
//! terminator are identified as kitty graphics (`G...`) or the glyph protocol (`25a1;...`),
//! buffered by that protocol's parser, and turned into a command at the end. [`Apc`] joins the
//! dispatcher to the kitty graphics state and the glyph glossary, so the core lane feeds it
//! APC bytes and gets replies back.

use blaze_own::Alloc;

use super::Wire;
use super::command::{self, CommandError, MaxBytes};
use super::exec::{CursorAfter, KittyGraphics, ScreenKey};
use super::glyph::{self, Glossary, RequestError};
use super::storage::GraphicsScreen;

/// An APC protocol.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Protocol {
    Kitty,
    Glyph,
}

impl Protocol {
    /// `defaultMaxBytes`: kitty payloads carry whole images (65 MiB), glyph ones one outline
    /// (1 MiB).
    pub const fn default_max_bytes(self) -> MaxBytes {
        match self {
            Protocol::Kitty => MaxBytes(65 * 1024 * 1024),
            Protocol::Glyph => MaxBytes(1024 * 1024),
        }
    }
}

/// Which protocols are recognized, and each one's byte cap.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ApcOpts {
    pub kitty: Option<MaxBytes>,
    pub glyph: Option<MaxBytes>,
}

impl ApcOpts {
    /// ghostty's defaults: both enabled, at their default caps.
    pub const DEFAULT: ApcOpts = ApcOpts {
        kitty: Some(Protocol::Kitty.default_max_bytes()),
        glyph: Some(Protocol::Glyph.default_max_bytes()),
    };
}

/// Why an APC string yields no command. Each is a by-design drop, counted in [`ApcStats`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dropped {
    /// Bytes or an end came with no APC started.
    NotStarted,
    /// No known (or no enabled) protocol: an unknown identifier, one longer than four bytes,
    /// or the string ended while it was still being identified.
    Unrecognized,
    /// The kitty parser refused a byte, or the command at the end.
    Kitty(CommandError),
    /// The glyph parser refused a byte, or the request at the end.
    Glyph(RequestError),
}

/// A complete APC command.
pub enum ApcCommand<'a> {
    Kitty(command::Command<'a>),
    Glyph(glyph::Request<'a>),
}

enum State<'a> {
    Inactive,
    Ignore(Dropped),
    Identify { len: usize, buf: [u8; 4] },
    Kitty(command::Parser<'a>),
    Glyph(glyph::Parser<'a>),
}

/// The APC dispatcher (`apc.Handler`).
pub struct Handler<'a> {
    state: State<'a>,
    opts: ApcOpts,
    alloc: Alloc<'a>,
}

impl<'a> Handler<'a> {
    pub fn new(opts: ApcOpts, alloc: Alloc<'a>) -> Handler<'a> {
        Handler { state: State::Inactive, opts, alloc }
    }

    /// Turn a protocol on or off (`enable`); a disabled protocol's strings are ignored.
    pub fn set_enabled(&mut self, protocol: Protocol, max_bytes: Option<MaxBytes>) {
        match protocol {
            Protocol::Kitty => self.opts.kitty = max_bytes,
            Protocol::Glyph => self.opts.glyph = max_bytes,
        }
    }

    /// `ESC _` was seen: a new string begins, dropping any unfinished one.
    pub fn start(&mut self) {
        self.state = State::Identify { len: 0, buf: [0; 4] };
    }

    /// One byte of the string.
    pub fn feed(&mut self, byte: u8) {
        match &mut self.state {
            State::Inactive => self.state = State::Ignore(Dropped::NotStarted),
            State::Ignore(_) => {}
            State::Identify { len, buf } => {
                if *len == 0 && byte == b'G' {
                    if let Some(max) = self.opts.kitty {
                        self.state = State::Kitty(command::Parser::new_in(max, self.alloc));
                        return;
                    }
                }
                if byte == b';' {
                    self.state = match (&buf[..*len] == b"25a1", self.opts.glyph) {
                        (true, Some(max)) => State::Glyph(glyph::Parser::new(max.0, self.alloc)),
                        _ => State::Ignore(Dropped::Unrecognized),
                    };
                    return;
                }
                if *len >= buf.len() {
                    self.state = State::Ignore(Dropped::Unrecognized);
                    return;
                }
                buf[*len] = byte;
                *len += 1;
            }
            State::Kitty(p) => {
                if let Err(e) = p.feed(byte) {
                    self.state = State::Ignore(Dropped::Kitty(e));
                }
            }
            State::Glyph(p) => {
                if let Err(e) = p.feed(byte) {
                    self.state = State::Ignore(Dropped::Glyph(e));
                }
            }
        }
    }

    /// Many bytes (`feedSlice`): the same as feeding each, the parsers' caps checked in bulk.
    pub fn feed_slice(&mut self, bytes: &[u8]) {
        let mut rem = bytes;
        while let Some((&first, rest)) = rem.split_first() {
            match &mut self.state {
                State::Inactive | State::Identify { .. } => {
                    self.feed(first);
                    rem = rest;
                }
                State::Ignore(_) => return,
                State::Kitty(p) => {
                    if let Err(e) = p.feed_slice(rem) {
                        self.state = State::Ignore(Dropped::Kitty(e));
                    }
                    return;
                }
                State::Glyph(p) => {
                    if let Err(e) = p.feed_slice(rem) {
                        self.state = State::Ignore(Dropped::Glyph(e));
                    }
                    return;
                }
            }
        }
    }

    /// The string terminator was seen (`end`): the command, or why there is none.
    pub fn end(&mut self) -> Result<ApcCommand<'a>, Dropped> {
        match core::mem::replace(&mut self.state, State::Inactive) {
            State::Inactive => Err(Dropped::NotStarted),
            State::Ignore(why) => Err(why),
            State::Identify { .. } => Err(Dropped::Unrecognized),
            State::Kitty(p) => p.complete().map(ApcCommand::Kitty).map_err(Dropped::Kitty),
            State::Glyph(p) => p.complete().map(ApcCommand::Glyph).map_err(Dropped::Glyph),
        }
    }

    /// Whether a string is being read (started and not ended).
    pub fn is_active(&self) -> bool {
        !matches!(self.state, State::Inactive)
    }
}

/// A reply to write to the PTY.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Reply {
    Kitty(command::Response),
    Glyph(glyph::Response),
}

impl Reply {
    pub fn encode(&self) -> Wire {
        match self {
            Reply::Kitty(r) => r.encode(),
            Reply::Glyph(r) => r.encode(),
        }
    }
}

/// What an APC string did.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ApcOutcome {
    pub reply: Option<Reply>,
    /// The cursor movement a kitty display asks for, after the reply.
    pub cursor: Option<CursorAfter>,
    /// A kitty load waits on host IO ([`KittyGraphics::pending`]).
    pub waits_on_host: bool,
}

/// APC strings dropped by design, counted (rule 3).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ApcStats {
    pub not_started: u64,
    pub unrecognized: u64,
    pub kitty_errors: u64,
    pub glyph_errors: u64,
    /// Glyph requests answered with nothing (`reply=0`, `reply=2` on success, a query
    /// without `cp`).
    pub glyph_unanswered: u64,
}

/// The APC state of a terminal: the dispatcher, kitty graphics, and the glyph glossary.
pub struct Apc<'a> {
    handler: Handler<'a>,
    graphics: KittyGraphics<'a>,
    glossary: Glossary<'a>,
    /// Set on every glyph register or clear, for the renderer to pick up and reset.
    pub glossary_dirty: bool,
    stats: ApcStats,
    alloc: Alloc<'a>,
}

impl<'a> Apc<'a> {
    pub fn new(opts: ApcOpts, graphics: KittyGraphics<'a>, alloc: Alloc<'a>) -> Apc<'a> {
        Apc {
            handler: Handler::new(opts, alloc),
            graphics,
            glossary: Glossary::new(alloc),
            glossary_dirty: false,
            stats: ApcStats::default(),
            alloc,
        }
    }

    pub fn handler_mut(&mut self) -> &mut Handler<'a> {
        &mut self.handler
    }

    pub fn graphics(&self) -> &KittyGraphics<'a> {
        &self.graphics
    }

    pub fn graphics_mut(&mut self) -> &mut KittyGraphics<'a> {
        &mut self.graphics
    }

    pub fn glossary(&self) -> &Glossary<'a> {
        &self.glossary
    }

    pub fn stats(&self) -> ApcStats {
        self.stats
    }

    /// `ESC _`.
    pub fn start(&mut self) {
        self.handler.start();
    }

    /// Bytes of the string.
    pub fn put(&mut self, bytes: &[u8]) {
        self.handler.feed_slice(bytes);
    }

    /// The string terminator: run the command on screen `which`.
    pub fn end(&mut self, which: ScreenKey, screen: &mut impl GraphicsScreen) -> ApcOutcome {
        match self.handler.end() {
            Err(why) => {
                let n = match why {
                    Dropped::NotStarted => &mut self.stats.not_started,
                    Dropped::Unrecognized => &mut self.stats.unrecognized,
                    Dropped::Kitty(_) => &mut self.stats.kitty_errors,
                    Dropped::Glyph(_) => &mut self.stats.glyph_errors,
                };
                *n += 1;
                ApcOutcome::default()
            }
            Ok(ApcCommand::Kitty(cmd)) => {
                let out = self.graphics.handle(cmd, which, screen);
                ApcOutcome {
                    reply: out.reply.map(Reply::Kitty),
                    cursor: out.cursor,
                    waits_on_host: out.waits_on_host,
                }
            }
            Ok(ApcCommand::Glyph(req)) => {
                if matches!(req, glyph::Request::Register(_) | glyph::Request::Clear(_)) {
                    self.glossary_dirty = true;
                }
                let reply = glyph::execute(&mut self.glossary, &req, self.alloc);
                if reply.is_none() {
                    self.stats.glyph_unanswered += 1;
                }
                ApcOutcome { reply: reply.map(Reply::Glyph), cursor: None, waits_on_host: false }
            }
        }
    }

    /// A full reset (RIS): the glossary is emptied and any unfinished string dropped. Each
    /// screen's kitty images are reset by the core lane through [`KittyGraphics::reset_screen`].
    pub fn full_reset(&mut self) {
        self.handler.state = State::Inactive;
        self.glossary.clear();
        self.glossary_dirty = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kitty::exec::GraphicsOpts;
    use crate::kitty::storage::tests::{FakeScreen, alloc};

    fn handler() -> Handler<'static> {
        Handler::new(ApcOpts::DEFAULT, alloc())
    }

    fn feed_each(h: &mut Handler<'_>, s: &[u8]) {
        for &b in s {
            h.feed(b);
        }
    }

    fn run(s: &[u8]) -> Result<ApcCommand<'static>, Dropped> {
        let mut h = handler();
        h.start();
        feed_each(&mut h, s);
        h.end()
    }

    fn ignored(h: &Handler<'_>) -> bool {
        matches!(h.state, State::Ignore(_))
    }

    // ghostty apc.zig:274 "unknown APC command"
    #[test]
    fn unknown_apc_command() {
        assert_eq!(run(b"Xabcdef1234").err(), Some(Dropped::Unrecognized));
    }

    // ghostty apc.zig:284 "garbage Kitty command"
    #[test]
    fn garbage_kitty_command() {
        assert!(matches!(run(b"Gabcdef1234"), Err(Dropped::Kitty(_))));
    }

    // ghostty apc.zig:295 "Kitty command with overflow u32"
    #[test]
    fn kitty_command_with_overflow_u32() {
        assert_eq!(run(b"Ga=p,i=10000000000").err(), Some(Dropped::Kitty(CommandError::Overflow)));
    }

    // ghostty apc.zig:306 "Kitty command with overflow i32"
    #[test]
    fn kitty_command_with_overflow_i32() {
        assert_eq!(
            run(b"Ga=p,i=1,z=-9999999999").err(),
            Some(Dropped::Kitty(CommandError::Overflow))
        );
    }

    // ghostty apc.zig:317 "kitty feed error deinits parser"
    #[test]
    fn kitty_feed_error_deinits_parser() {
        let mut h = handler();
        h.start();
        feed_each(&mut h, b"Ga=p,i=10000000000;");
        assert!(ignored(&h));
    }

    // ghostty apc.zig:333 "kitty max bytes exceeded"
    #[test]
    fn kitty_max_bytes_exceeded() {
        let mut h = Handler::new(ApcOpts { kitty: Some(MaxBytes(4)), ..ApcOpts::DEFAULT }, alloc());
        h.start();
        feed_each(&mut h, b"Ga=t;");
        assert!(!ignored(&h));
        feed_each(&mut h, b"abcd");
        assert!(!ignored(&h));
        h.feed(b'e');
        assert!(ignored(&h));
        assert_eq!(h.end().err(), Some(Dropped::Kitty(CommandError::OutOfMemory)));
    }

    // ghostty apc.zig:351 "valid Kitty command"
    #[test]
    fn valid_kitty_command() {
        assert!(matches!(run(b"Gf=24,s=10,v=20,hello=world"), Ok(ApcCommand::Kitty(_))));
    }

    // ghostty apc.zig:364 "identify with unrecognized command"
    #[test]
    fn identify_with_unrecognized_command() {
        assert_eq!(run(b"abcd;payload").err(), Some(Dropped::Unrecognized));
    }

    // ghostty apc.zig:373 "identify buffer overflow"
    #[test]
    fn identify_buffer_overflow() {
        assert_eq!(run(b"abcde;payload").err(), Some(Dropped::Unrecognized));
    }

    // ghostty apc.zig:382 "identify with no input"
    #[test]
    fn identify_with_no_input() {
        assert_eq!(run(b"").err(), Some(Dropped::Unrecognized));
    }

    // ghostty apc.zig:389 "identify with unknown partial input"
    #[test]
    fn identify_with_unknown_partial_input() {
        assert_eq!(run(b"25a").err(), Some(Dropped::Unrecognized));
    }

    // ghostty apc.zig:398 "garbage glyph command"
    #[test]
    fn garbage_glyph_command() {
        assert_eq!(run(b"25a1;X").err(), Some(Dropped::Glyph(RequestError::InvalidFormat)));
    }

    // ghostty apc.zig:407 "valid glyph command"
    #[test]
    fn valid_glyph_command() {
        assert!(matches!(run(b"25a1;q;cp=E0A0"), Ok(ApcCommand::Glyph(glyph::Request::Query(_)))));
    }

    // ghostty apc.zig:419 "feedSlice valid Kitty command"
    #[test]
    fn feed_slice_valid_kitty_command() {
        let mut h = handler();
        h.start();
        h.feed_slice(b"Gf=24,s=10,v=20;aGVsbG8=");
        let Ok(ApcCommand::Kitty(cmd)) = h.end() else { panic!() };
        assert_eq!(cmd.data.as_slice(), b"hello");
    }

    // ghostty apc.zig:433 "feedSlice identify split across slices"
    #[test]
    fn feed_slice_identify_split_across_slices() {
        let mut h = handler();
        h.start();
        h.feed_slice(b"G");
        h.feed_slice(b"f=24,s=10,");
        h.feed_slice(b"v=20;aGVsbG8=");
        let Ok(ApcCommand::Kitty(cmd)) = h.end() else { panic!() };
        assert_eq!(cmd.data.as_slice(), b"hello");
    }

    // ghostty apc.zig:449 "feedSlice unknown APC command is ignored"
    #[test]
    fn feed_slice_unknown_apc_command_is_ignored() {
        let mut h = handler();
        h.start();
        h.feed_slice(b"Xabcdef1234");
        assert!(ignored(&h));
        h.feed_slice(b"more data that is dropped");
        assert_eq!(h.end().err(), Some(Dropped::Unrecognized));
    }

    // ghostty apc.zig:461 "feedSlice valid glyph command"
    #[test]
    fn feed_slice_valid_glyph_command() {
        let mut h = handler();
        h.start();
        h.feed_slice(b"25a1;q;cp=E0A0");
        assert!(matches!(h.end(), Ok(ApcCommand::Glyph(glyph::Request::Query(_)))));
    }

    // ghostty apc.zig:473 "feedSlice kitty max bytes exceeded"
    #[test]
    fn feed_slice_kitty_max_bytes_exceeded() {
        let mut h = Handler::new(ApcOpts { kitty: Some(MaxBytes(4)), ..ApcOpts::DEFAULT }, alloc());
        h.start();
        h.feed_slice(b"Ga=t;abcd");
        assert!(!ignored(&h));
        h.feed_slice(b"e");
        assert!(ignored(&h));
    }

    // ghostty apc.zig:486 "disabled glyph command is ignored"
    #[test]
    fn disabled_glyph_command_is_ignored() {
        let mut h = handler();
        h.set_enabled(Protocol::Glyph, None);
        h.start();
        feed_each(&mut h, b"25a1;q;cp=e0a0");
        assert_eq!(h.end().err(), Some(Dropped::Unrecognized));
    }

    #[test]
    fn disabled_kitty_command_is_ignored() {
        let mut h = handler();
        h.set_enabled(Protocol::Kitty, None);
        h.start();
        feed_each(&mut h, b"Ga=q,i=1;AAAA");
        assert_eq!(h.end().err(), Some(Dropped::Unrecognized));
        // Enabled, `G` alone is a kitty command with no keys: refused at the end.
        assert_eq!(run(b"G").err(), Some(Dropped::Kitty(CommandError::InvalidFormat)));
    }

    #[test]
    fn not_started_and_restart() {
        let mut h = handler();
        assert_eq!(h.end().err(), Some(Dropped::NotStarted));
        h.feed(b'G');
        assert_eq!(h.end().err(), Some(Dropped::NotStarted));
        assert!(!h.is_active());
        // A new start drops an unfinished string.
        h.start();
        h.feed_slice(b"Xabc");
        h.start();
        h.feed_slice(b"25a1;s");
        assert!(matches!(h.end(), Ok(ApcCommand::Glyph(glyph::Request::Support))));
    }

    fn apc() -> Apc<'static> {
        Apc::new(ApcOpts::DEFAULT, KittyGraphics::new(GraphicsOpts::DEFAULT, alloc()), alloc())
    }

    fn string(a: &mut Apc<'static>, s: &[u8], screen: &mut FakeScreen) -> ApcOutcome {
        a.start();
        a.put(s);
        a.end(ScreenKey::Primary, screen)
    }

    #[test]
    fn apc_kitty_query_reply() {
        let mut a = apc();
        let mut screen = FakeScreen::new(80, 24);
        let out = string(&mut a, b"Ga=q,i=31,s=1,v=1,f=24;AAAA", &mut screen);
        let wire = out.reply.unwrap().encode();
        assert_eq!(wire.as_bytes(), b"\x1b_Gi=31;OK\x1b\\");
    }

    #[test]
    fn apc_glyph_register_query_clear() {
        let mut a = apc();
        let mut screen = FakeScreen::new(80, 24);
        let out = string(&mut a, b"25a1;r;cp=e0a0;AAAAAAAAAAAAAA==", &mut screen);
        assert_eq!(out.reply.unwrap().encode().as_bytes(), b"\x1b_25a1;r;cp=e0a0;status=0\x1b\\");
        assert!(a.glossary_dirty && a.glossary().contains(0xE0A0));
        a.glossary_dirty = false;
        let out = string(&mut a, b"25a1;q;cp=e0a0", &mut screen);
        assert_eq!(
            out.reply.unwrap().encode().as_bytes(),
            b"\x1b_25a1;q;cp=e0a0;status=glossary\x1b\\"
        );
        assert!(!a.glossary_dirty);
        let out = string(&mut a, b"25a1;r;cp=e0a1;reply=2;AAAAAAAAAAAAAA==", &mut screen);
        assert_eq!(out.reply, None);
        assert_eq!(a.stats().glyph_unanswered, 1);
        a.full_reset();
        assert!(a.glossary().is_empty() && a.glossary_dirty);
    }

    #[test]
    fn apc_drops_are_counted() {
        let mut a = apc();
        let mut screen = FakeScreen::new(80, 24);
        string(&mut a, b"Xyz", &mut screen);
        string(&mut a, b"Ga=p,i=10000000000", &mut screen);
        string(&mut a, b"25a1;x", &mut screen);
        assert_eq!(a.end(ScreenKey::Primary, &mut screen), ApcOutcome::default());
        let s = a.stats();
        assert_eq!((s.unrecognized, s.kitty_errors, s.glyph_errors, s.not_started), (1, 1, 1, 1));
    }

    /// Any split of a string into slices, or byte-by-byte feeding, ends in the same result.
    #[test]
    fn split_feed_equivalence() {
        const STRINGS: [&[u8]; 8] = [
            b"Gf=24,s=10,v=20;aGVsbG8=",
            b"25a1;q;cp=E0A0",
            b"25a1;r;cp=e0a0;AAAAAAAAAAAAAA==",
            b"Xabcdef",
            b"25a",
            b"abcde;x",
            b"Ga=p,i=10000000000",
            b"25a1;s",
        ];
        fn summary(r: Result<ApcCommand<'_>, Dropped>) -> std::string::String {
            match r {
                Ok(ApcCommand::Kitty(c)) => {
                    std::format!("kitty {:?} {:?}", c.control, c.data.as_slice())
                }
                Ok(ApcCommand::Glyph(g)) => std::format!("glyph {g:?}"),
                Err(e) => std::format!("dropped {e:?}"),
            }
        }
        blaze_sim::check(blaze_sim::Config::cases(400), |tape| {
            let s = *tape.pick(&STRINGS, "string").unwrap();
            let max = tape.int_in(0usize..=40, "max bytes");
            let opts = ApcOpts { kitty: Some(MaxBytes(max)), glyph: Some(MaxBytes(max)) };
            let mut whole = Handler::new(opts, alloc());
            whole.start();
            feed_each(&mut whole, s);
            let mut split = Handler::new(opts, alloc());
            split.start();
            let mut rest = s;
            while !rest.is_empty() {
                let n = 1 + tape.below(rest.len() as u64, "split") as usize;
                let (head, tail) = rest.split_at(n);
                if tape.coin("slice or bytes") {
                    split.feed_slice(head);
                } else {
                    feed_each(&mut split, head);
                }
                rest = tail;
            }
            assert_eq!(summary(whole.end()), summary(split.end()), "{}", s.escape_ascii());
        });
    }
}
