//! The kitty graphics command: the control keys and payload of `ESC _ G <keys> ; <payload> ESC \`,
//! parsed as ghostty's graphics_command.zig does, and the `ESC _ G i=..;MSG ESC \` reply.
//!
//! Keys are one byte; a value is a one-byte non-digit (kept as its byte, so `t=f` is `'f'`) or a
//! decimal (`z`, `H` and `V` as `i32`, the rest as `u32`). A key longer than one byte, or a key or
//! value longer than 11 bytes, is skipped up to the next `,`. The last of two equal keys wins. The
//! payload is base64, decoded on [`Parser::complete`] as simdutf's loose decoder does.

use blaze_own::{AVec, Alloc};

use super::Wire;
use super::num::{IntError, parse_i32, parse_u32};

/// The most bytes a key or a value may have before it is skipped (`kv_temp: [11]u8`).
const KV_TEMP: usize = 11;

/// Why a command was refused while parsing. ghostty drops the command (no reply) for each.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CommandError {
    /// An action, quiet, format, medium, compression, cursor-movement, virtual-placement,
    /// animation or delete value outside its set, a key left without a value, or a range delete
    /// without `x <= y`.
    InvalidFormat,
    /// The payload is not base64.
    InvalidData,
    /// A value is not a number (`InvalidCharacter` from `parseInt`).
    InvalidCharacter,
    /// A value is out of its type's range.
    Overflow,
    /// The payload passed the parser's byte cap, or the allocator refused it.
    OutOfMemory,
}

impl From<IntError> for CommandError {
    fn from(e: IntError) -> CommandError {
        match e {
            IntError::InvalidCharacter => CommandError::InvalidCharacter,
            IntError::Overflow => CommandError::Overflow,
        }
    }
}

/// The most payload bytes one command may buffer (ghostty's per-protocol `max_bytes`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MaxBytes(pub usize);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    ControlKey,
    ControlKeyIgnore,
    ControlValue,
    ControlValueIgnore,
    Data,
}

/// The key/value pairs seen so far: one slot per key byte.
struct Kv {
    values: [u32; 256],
    present: [u64; 4],
}

impl Kv {
    const fn new() -> Kv {
        Kv { values: [0; 256], present: [0; 4] }
    }

    fn put(&mut self, key: u8, value: u32) {
        self.values[usize::from(key)] = value;
        self.present[usize::from(key >> 6)] |= 1 << (key & 63);
    }

    fn get(&self, key: u8) -> Option<u32> {
        let set = self.present[usize::from(key >> 6)] & (1 << (key & 63)) != 0;
        set.then(|| self.values[usize::from(key)])
    }
}

/// The command parser, fed the bytes after `G` up to the string terminator.
pub struct Parser<'a> {
    kv: Kv,
    temp: [u8; KV_TEMP],
    temp_len: usize,
    current: u8,
    data: AVec<'a, u8>,
    max_bytes: MaxBytes,
    state: State,
}

impl<'a> Parser<'a> {
    pub fn new_in(max_bytes: MaxBytes, alloc: Alloc<'a>) -> Parser<'a> {
        Parser {
            kv: Kv::new(),
            temp: [0; KV_TEMP],
            temp_len: 0,
            current: 0,
            data: AVec::new_in(alloc),
            max_bytes,
            state: State::ControlKey,
        }
    }

    /// One byte (`Parser.feed`).
    pub fn feed(&mut self, c: u8) -> Result<(), CommandError> {
        match self.state {
            State::ControlKey => match c {
                b'=' => {
                    if self.temp_len != 1 {
                        self.state = State::ControlValueIgnore;
                        self.temp_len = 0;
                    } else {
                        self.current = self.temp[0];
                        self.temp_len = 0;
                        self.state = State::ControlValue;
                    }
                }
                b';' => self.state = State::Data,
                _ => self.accumulate(c, State::ControlKeyIgnore),
            },
            State::ControlKeyIgnore => {
                if c == b'=' {
                    self.state = State::ControlValueIgnore;
                }
            }
            State::ControlValue => match c {
                b',' => self.finish_value(State::ControlKey)?,
                b';' => self.finish_value(State::Data)?,
                _ => self.accumulate(c, State::ControlValueIgnore),
            },
            State::ControlValueIgnore => match c {
                b',' => self.state = State::ControlKeyIgnore,
                b';' => self.state = State::Data,
                _ => {}
            },
            State::Data => {
                if self.data.len() >= self.max_bytes.0 {
                    return Err(CommandError::OutOfMemory);
                }
                self.data.try_push(c).map_err(|_| CommandError::OutOfMemory)?;
            }
        }
        Ok(())
    }

    /// Bytes in bulk (`Parser.feedSlice`): the same as feeding each, the payload copied at once.
    pub fn feed_slice(&mut self, bytes: &[u8]) -> Result<(), CommandError> {
        let mut rem = bytes;
        while let Some((&first, rest)) = rem.split_first() {
            if self.state == State::Data {
                if self.data.len() + rem.len() > self.max_bytes.0 {
                    return Err(CommandError::OutOfMemory);
                }
                return self.data.try_extend_from_slice(rem).map_err(|_| CommandError::OutOfMemory);
            }
            self.feed(first)?;
            rem = rest;
        }
        Ok(())
    }

    fn accumulate(&mut self, c: u8, overflow: State) {
        if self.temp_len >= KV_TEMP {
            self.state = overflow;
            self.temp_len = 0;
            return;
        }
        self.temp[self.temp_len] = c;
        self.temp_len += 1;
    }

    fn finish_value(&mut self, next: State) -> Result<(), CommandError> {
        self.state = next;
        let text = &self.temp[..self.temp_len];
        self.temp_len = 0;
        if let [c] = *text
            && !c.is_ascii_digit()
        {
            self.kv.put(self.current, u32::from(c));
            return Ok(());
        }
        let value = match self.current {
            b'z' | b'H' | b'V' => parse_i32(text)? as u32,
            _ => parse_u32(text)?,
        };
        self.kv.put(self.current, value);
        Ok(())
    }

    /// The command (`Parser.complete`): the keys checked, the payload base64-decoded.
    pub fn complete(mut self) -> Result<Command<'a>, CommandError> {
        match self.state {
            State::ControlKey | State::ControlKeyIgnore => return Err(CommandError::InvalidFormat),
            State::ControlValue => self.finish_value(State::Data)?,
            State::ControlValueIgnore | State::Data => {}
        }
        let kv = &self.kv;
        let action = match kv.get(b'a') {
            None => b't',
            Some(v) => u8::try_from(v).map_err(|_| CommandError::InvalidFormat)?,
        };
        let control = match action {
            b'q' => Control::Query(Transmission::parse(kv)?),
            b't' => Control::Transmit(Transmission::parse(kv)?),
            b'T' => Control::TransmitAndDisplay(Transmission::parse(kv)?, Display::parse(kv)?),
            b'p' => Control::Display(Display::parse(kv)?),
            b'd' => Control::Delete(Delete::parse(kv)?),
            b'f' => Control::TransmitAnimationFrame(AnimationFrameLoading::parse(kv)?),
            b'a' => Control::ControlAnimation(AnimationControl::parse(kv)?),
            b'c' => Control::ComposeAnimation(AnimationFrameComposition::parse(kv)?),
            _ => return Err(CommandError::InvalidFormat),
        };
        let quiet = match kv.get(b'q') {
            None | Some(0) => Quiet::No,
            Some(1) => Quiet::Ok,
            Some(2) => Quiet::Failures,
            Some(_) => return Err(CommandError::InvalidFormat),
        };
        let len = decode_base64_loose(&mut self.data).map_err(|_| CommandError::InvalidData)?;
        self.data.truncate(len);
        Ok(Command { control, quiet, data: self.data })
    }
}

/// simdutf's `base64_to_binary` (loose last chunk, ghostty's simd path) in place: ASCII
/// whitespace skipped, at most two `=` at the end, a last chunk of two or three symbols allowed,
/// one refused, and with padding the symbols and `=` a multiple of four. Returns the length.
pub(crate) fn decode_base64_loose(buf: &mut [u8]) -> Result<usize, InvalidBase64> {
    let is_space = |b: u8| matches!(b, b' ' | b'\t' | b'\n' | b'\x0c' | b'\r');
    let mut end = buf.len();
    let mut pad = 0usize;
    while end > 0 {
        let b = buf[end - 1];
        if is_space(b) {
            end -= 1;
        } else if b == b'=' {
            pad += 1;
            end -= 1;
        } else {
            break;
        }
    }
    if pad > 2 {
        return Err(InvalidBase64);
    }
    let (mut acc, mut k, mut out, mut symbols) = (0u32, 0usize, 0usize, 0usize);
    for i in 0..end {
        let b = buf[i];
        if is_space(b) {
            continue;
        }
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return Err(InvalidBase64),
        };
        acc = acc << 6 | u32::from(v);
        k += 1;
        symbols += 1;
        if k == 4 {
            buf[out] = (acc >> 16) as u8;
            buf[out + 1] = (acc >> 8) as u8;
            buf[out + 2] = acc as u8;
            out += 3;
            (acc, k) = (0, 0);
        }
    }
    match k {
        0 => {}
        2 => {
            buf[out] = (acc >> 4) as u8;
            out += 1;
        }
        3 => {
            buf[out] = (acc >> 10) as u8;
            buf[out + 1] = (acc >> 2) as u8;
            out += 2;
        }
        _ => return Err(InvalidBase64),
    }
    if pad > 0 && (symbols + pad) % 4 != 0 {
        return Err(InvalidBase64);
    }
    Ok(out)
}

/// The payload is not base64 as simdutf reads it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct InvalidBase64;

/// A parsed command and its decoded payload (a path for the file and shared-memory media).
#[derive(Debug)]
pub struct Command<'a> {
    pub control: Control,
    pub quiet: Quiet,
    pub data: AVec<'a, u8>,
}

impl Command<'_> {
    /// The transmission of a query, transmit, or transmit-and-display.
    pub fn transmission(&self) -> Option<Transmission> {
        match self.control {
            Control::Query(t) | Control::Transmit(t) | Control::TransmitAndDisplay(t, _) => Some(t),
            _ => None,
        }
    }

    /// The display of a put or transmit-and-display.
    pub fn display(&self) -> Option<Display> {
        match self.control {
            Control::Display(d) | Control::TransmitAndDisplay(_, d) => Some(d),
            _ => None,
        }
    }
}

/// `q=`: which replies are sent.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Quiet {
    /// `q=0`: every reply.
    No,
    /// `q=1`: no `OK`.
    Ok,
    /// `q=2`: none.
    Failures,
}

/// `a=`, with what each action carries.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Control {
    /// `a=q`
    Query(Transmission),
    /// `a=t`, the default.
    Transmit(Transmission),
    /// `a=T`
    TransmitAndDisplay(Transmission, Display),
    /// `a=p`
    Display(Display),
    /// `a=d`
    Delete(Delete),
    /// `a=f`
    TransmitAnimationFrame(AnimationFrameLoading),
    /// `a=a`
    ControlAnimation(AnimationControl),
    /// `a=c`
    ComposeAnimation(AnimationFrameComposition),
}

/// `f=`: the pixel format. Gray formats exist for completeness; no key selects them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Format {
    /// `f=24`
    Rgb,
    /// `f=32`, the default.
    Rgba,
    /// `f=100`
    Png,
    GrayAlpha,
    Gray,
}

impl Format {
    /// Bytes per pixel of a raw format; `None` for PNG, which is decoded first.
    pub const fn bpp(self) -> Option<u32> {
        match self {
            Format::Gray => Some(1),
            Format::GrayAlpha => Some(2),
            Format::Rgb => Some(3),
            Format::Rgba => Some(4),
            Format::Png => None,
        }
    }
}

/// `t=`: where the payload is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Medium {
    /// `t=d`, the default: the payload is the data.
    Direct,
    /// `t=f`: the payload is a file path.
    File,
    /// `t=t`: a temporary file path, deleted after reading.
    TemporaryFile,
    /// `t=s`: a POSIX shared memory name.
    SharedMemory,
}

/// `o=`
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Compression {
    None,
    /// `o=z`
    ZlibDeflate,
}

/// The transmission keys.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Transmission {
    /// `f`
    pub format: Format,
    /// `t`
    pub medium: Medium,
    /// `s`
    pub width: u32,
    /// `v`
    pub height: u32,
    /// `S`
    pub size: u32,
    /// `O`
    pub offset: u32,
    /// `i`
    pub image_id: u32,
    /// `I`
    pub image_number: u32,
    /// `p`
    pub placement_id: u32,
    /// `o`
    pub compression: Compression,
    /// `m`, read only for the direct medium.
    pub more_chunks: bool,
}

impl Default for Transmission {
    fn default() -> Transmission {
        Transmission {
            format: Format::Rgba,
            medium: Medium::Direct,
            width: 0,
            height: 0,
            size: 0,
            offset: 0,
            image_id: 0,
            image_number: 0,
            placement_id: 0,
            compression: Compression::None,
            more_chunks: false,
        }
    }
}

fn byte_of(v: u32) -> Result<u8, CommandError> {
    u8::try_from(v).map_err(|_| CommandError::InvalidFormat)
}

impl Transmission {
    fn parse(kv: &Kv) -> Result<Transmission, CommandError> {
        let mut r = Transmission::default();
        if let Some(v) = kv.get(b'f') {
            r.format = match v {
                24 => Format::Rgb,
                32 => Format::Rgba,
                100 => Format::Png,
                _ => return Err(CommandError::InvalidFormat),
            };
        }
        if let Some(v) = kv.get(b't') {
            r.medium = match byte_of(v)? {
                b'd' => Medium::Direct,
                b'f' => Medium::File,
                b't' => Medium::TemporaryFile,
                b's' => Medium::SharedMemory,
                _ => return Err(CommandError::InvalidFormat),
            };
        }
        let set = |key: u8, field: &mut u32| {
            if let Some(v) = kv.get(key) {
                *field = v;
            }
        };
        set(b's', &mut r.width);
        set(b'v', &mut r.height);
        set(b'S', &mut r.size);
        set(b'O', &mut r.offset);
        set(b'i', &mut r.image_id);
        set(b'I', &mut r.image_number);
        set(b'p', &mut r.placement_id);
        if let Some(v) = kv.get(b'o') {
            r.compression = match byte_of(v)? {
                b'z' => Compression::ZlibDeflate,
                _ => return Err(CommandError::InvalidFormat),
            };
        }
        // `m` is only for the direct medium (kitty ignores it otherwise).
        if r.medium == Medium::Direct
            && let Some(v) = kv.get(b'm')
        {
            r.more_chunks = v > 0;
        }
        Ok(r)
    }
}

/// `C=`
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CursorMovement {
    /// `C=0`, the default: the cursor moves past the image.
    After,
    /// `C=1`
    None,
}

/// The display (put) keys.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Display {
    /// `i`
    pub image_id: u32,
    /// `I`
    pub image_number: u32,
    /// `p`
    pub placement_id: u32,
    /// `x`: source rectangle left, pixels.
    pub x: u32,
    /// `y`
    pub y: u32,
    /// `w`
    pub width: u32,
    /// `h`
    pub height: u32,
    /// `X`: offset in the first cell, pixels.
    pub x_offset: u32,
    /// `Y`
    pub y_offset: u32,
    /// `c`
    pub columns: u32,
    /// `r`
    pub rows: u32,
    /// `C`
    pub cursor_movement: CursorMovement,
    /// `U`
    pub virtual_placement: bool,
    /// `P`
    pub parent_id: u32,
    /// `Q`
    pub parent_placement_id: u32,
    /// `H`
    pub horizontal_offset: i32,
    /// `V`
    pub vertical_offset: i32,
    /// `z`
    pub z: i32,
}

impl Default for Display {
    fn default() -> Display {
        Display {
            image_id: 0,
            image_number: 0,
            placement_id: 0,
            x: 0,
            y: 0,
            width: 0,
            height: 0,
            x_offset: 0,
            y_offset: 0,
            columns: 0,
            rows: 0,
            cursor_movement: CursorMovement::After,
            virtual_placement: false,
            parent_id: 0,
            parent_placement_id: 0,
            horizontal_offset: 0,
            vertical_offset: 0,
            z: 0,
        }
    }
}

impl Display {
    fn parse(kv: &Kv) -> Result<Display, CommandError> {
        let mut r = Display::default();
        let set = |key: u8, field: &mut u32| {
            if let Some(v) = kv.get(key) {
                *field = v;
            }
        };
        set(b'i', &mut r.image_id);
        set(b'I', &mut r.image_number);
        set(b'p', &mut r.placement_id);
        set(b'x', &mut r.x);
        set(b'y', &mut r.y);
        set(b'w', &mut r.width);
        set(b'h', &mut r.height);
        set(b'X', &mut r.x_offset);
        set(b'Y', &mut r.y_offset);
        set(b'c', &mut r.columns);
        set(b'r', &mut r.rows);
        if let Some(v) = kv.get(b'C') {
            r.cursor_movement = match v {
                0 => CursorMovement::After,
                1 => CursorMovement::None,
                _ => return Err(CommandError::InvalidFormat),
            };
        }
        if let Some(v) = kv.get(b'U') {
            r.virtual_placement = match v {
                0 => false,
                1 => true,
                _ => return Err(CommandError::InvalidFormat),
            };
        }
        if let Some(v) = kv.get(b'z') {
            r.z = v as i32;
        }
        set(b'P', &mut r.parent_id);
        set(b'Q', &mut r.parent_placement_id);
        if let Some(v) = kv.get(b'H') {
            r.horizontal_offset = v as i32;
        }
        if let Some(v) = kv.get(b'V') {
            r.vertical_offset = v as i32;
        }
        Ok(r)
    }
}

/// `X=` of a frame, `C=` of a composition.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CompositionMode {
    AlphaBlend,
    Overwrite,
}

fn composition_mode(v: u32) -> Result<CompositionMode, CommandError> {
    match v {
        0 => Ok(CompositionMode::AlphaBlend),
        1 => Ok(CompositionMode::Overwrite),
        _ => Err(CommandError::InvalidFormat),
    }
}

/// `a=f` keys. Parsed and validated; the action is answered "unimplemented", as in ghostty.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AnimationFrameLoading {
    pub x: u32,
    pub y: u32,
    /// `c`
    pub create_frame: u32,
    /// `r`
    pub edit_frame: u32,
    /// `z`
    pub gap_ms: u32,
    /// `X`
    pub composition_mode: CompositionMode,
    /// `Y`: `r | g << 8 | b << 16 | a << 24`.
    pub background: u32,
}

impl AnimationFrameLoading {
    fn parse(kv: &Kv) -> Result<AnimationFrameLoading, CommandError> {
        Ok(AnimationFrameLoading {
            x: kv.get(b'x').unwrap_or(0),
            y: kv.get(b'y').unwrap_or(0),
            create_frame: kv.get(b'c').unwrap_or(0),
            edit_frame: kv.get(b'r').unwrap_or(0),
            gap_ms: kv.get(b'z').unwrap_or(0),
            composition_mode: match kv.get(b'X') {
                None => CompositionMode::AlphaBlend,
                Some(v) => composition_mode(v)?,
            },
            background: kv.get(b'Y').unwrap_or(0),
        })
    }
}

/// `a=c` keys.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AnimationFrameComposition {
    /// `c`
    pub frame: u32,
    /// `r`
    pub edit_frame: u32,
    pub x: u32,
    pub y: u32,
    /// `w`
    pub width: u32,
    /// `h`
    pub height: u32,
    /// `X`
    pub left_edge: u32,
    /// `Y`
    pub top_edge: u32,
    /// `C`
    pub composition_mode: CompositionMode,
}

impl AnimationFrameComposition {
    fn parse(kv: &Kv) -> Result<AnimationFrameComposition, CommandError> {
        Ok(AnimationFrameComposition {
            frame: kv.get(b'c').unwrap_or(0),
            edit_frame: kv.get(b'r').unwrap_or(0),
            x: kv.get(b'x').unwrap_or(0),
            y: kv.get(b'y').unwrap_or(0),
            width: kv.get(b'w').unwrap_or(0),
            height: kv.get(b'h').unwrap_or(0),
            left_edge: kv.get(b'X').unwrap_or(0),
            top_edge: kv.get(b'Y').unwrap_or(0),
            composition_mode: match kv.get(b'C') {
                None => CompositionMode::AlphaBlend,
                Some(v) => composition_mode(v)?,
            },
        })
    }
}

/// `s=` of `a=a`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AnimationAction {
    Invalid,
    Stop,
    RunWait,
    Run,
}

/// `a=a` keys.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AnimationControl {
    /// `s`
    pub action: AnimationAction,
    /// `r`
    pub frame: u32,
    /// `z`
    pub gap_ms: u32,
    /// `c`
    pub current_frame: u32,
    /// `v`
    pub loops: u32,
}

impl AnimationControl {
    fn parse(kv: &Kv) -> Result<AnimationControl, CommandError> {
        Ok(AnimationControl {
            action: match kv.get(b's') {
                None | Some(0) => AnimationAction::Invalid,
                Some(1) => AnimationAction::Stop,
                Some(2) => AnimationAction::RunWait,
                Some(3) => AnimationAction::Run,
                Some(_) => return Err(CommandError::InvalidFormat),
            },
            frame: kv.get(b'r').unwrap_or(0),
            gap_ms: kv.get(b'z').unwrap_or(0),
            current_frame: kv.get(b'c').unwrap_or(0),
            loops: kv.get(b'v').unwrap_or(0),
        })
    }
}

/// Whether a delete also frees the images it leaves unplaced (the upper-case letter).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FreeImages {
    /// Lower case: placements only.
    No,
    /// Upper case: and the image data no placement uses.
    Yes,
}

impl FreeImages {
    fn of(letter: u8) -> FreeImages {
        match letter.is_ascii_uppercase() {
            true => FreeImages::Yes,
            false => FreeImages::No,
        }
    }
}

/// `d=`: what a delete removes. Coordinates are 1-based as sent.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Delete {
    /// `a`/`A`, the default: every placement on the screen (virtual ones excepted).
    All(FreeImages),
    /// `i`/`I`: by image id, and placement id if non-zero.
    Id { free: FreeImages, image_id: u32, placement_id: u32 },
    /// `n`/`N`: the newest image with a number.
    Newest { free: FreeImages, image_number: u32, placement_id: u32 },
    /// `c`/`C`: placements under the cursor.
    IntersectCursor(FreeImages),
    /// `f`/`F`: animation frames (no effect, as in ghostty).
    AnimationFrames(FreeImages),
    /// `p`/`P`: placements over a cell.
    IntersectCell { free: FreeImages, x: u32, y: u32 },
    /// `q`/`Q`: placements over a cell at a z-index.
    IntersectCellZ { free: FreeImages, x: u32, y: u32, z: i32 },
    /// `r`/`R`: image ids in `first..=last`.
    Range { free: FreeImages, first: u32, last: u32 },
    /// `x`/`X`: placements over a column.
    Column { free: FreeImages, x: u32 },
    /// `y`/`Y`: placements over a row.
    Row { free: FreeImages, y: u32 },
    /// `z`/`Z`: placements at a z-index.
    Z { free: FreeImages, z: i32 },
}

impl Delete {
    fn parse(kv: &Kv) -> Result<Delete, CommandError> {
        let what = match kv.get(b'd') {
            None => b'a',
            Some(v) => byte_of(v)?,
        };
        let free = FreeImages::of(what);
        let get = |key: u8| kv.get(key).unwrap_or(0);
        Ok(match what {
            b'a' | b'A' => Delete::All(free),
            b'i' | b'I' => Delete::Id { free, image_id: get(b'i'), placement_id: get(b'p') },
            b'n' | b'N' => {
                Delete::Newest { free, image_number: get(b'I'), placement_id: get(b'p') }
            }
            b'c' | b'C' => Delete::IntersectCursor(free),
            b'f' | b'F' => Delete::AnimationFrames(free),
            b'p' | b'P' => Delete::IntersectCell { free, x: get(b'x'), y: get(b'y') },
            b'q' | b'Q' => {
                Delete::IntersectCellZ { free, x: get(b'x'), y: get(b'y'), z: get(b'z') as i32 }
            }
            b'r' | b'R' => {
                let x = kv.get(b'x').ok_or(CommandError::InvalidFormat)?;
                let y = kv.get(b'y').ok_or(CommandError::InvalidFormat)?;
                if x > y {
                    return Err(CommandError::InvalidFormat);
                }
                Delete::Range { free, first: x, last: y }
            }
            b'x' | b'X' => Delete::Column { free, x: get(b'x') },
            b'y' | b'Y' => Delete::Row { free, y: get(b'y') },
            b'z' | b'Z' => Delete::Z { free, z: get(b'z') as i32 },
            _ => return Err(CommandError::InvalidFormat),
        })
    }
}

/// The message of a reply, each one ghostty sends.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Message {
    Ok,
    OutOfMemory,
    InvalidData,
    DecompressionFailed,
    FilePathTooLong,
    TemporaryFileNotInTempDir,
    TemporaryFileNotNamedCorrectly,
    UnsupportedFormat,
    UnsupportedMedium,
    UnsupportedDepth,
    DimensionsRequired,
    DimensionsTooLarge,
    ImageIdRequired,
    IdAndNumberExclusive,
    IdOrNumberRequired,
    ImageNotFound,
    VirtualWithParent,
    TerminalState,
    Unimplemented,
}

impl Message {
    pub const fn as_str(self) -> &'static str {
        match self {
            Message::Ok => "OK",
            Message::OutOfMemory => "ENOMEM: out of memory",
            Message::InvalidData => "EINVAL: invalid data",
            Message::DecompressionFailed => "EINVAL: decompression failed",
            Message::FilePathTooLong => "EINVAL: file path too long",
            Message::TemporaryFileNotInTempDir => "EINVAL: temporary file not in temp dir",
            Message::TemporaryFileNotNamedCorrectly => "EINVAL: temporary file not named correctly",
            Message::UnsupportedFormat => "EINVAL: unsupported format",
            Message::UnsupportedMedium => "EINVAL: unsupported medium",
            Message::UnsupportedDepth => "EINVAL: unsupported pixel depth",
            Message::DimensionsRequired => "EINVAL: dimensions required",
            Message::DimensionsTooLarge => "EINVAL: dimensions too large",
            Message::ImageIdRequired => "EINVAL: image ID required",
            Message::IdAndNumberExclusive => "EINVAL: image ID and number are mutually exclusive",
            Message::IdOrNumberRequired => "EINVAL: image ID or number required",
            Message::ImageNotFound => "ENOENT: image not found",
            Message::VirtualWithParent => "EINVAL: virtual placement cannot refer to a parent",
            Message::TerminalState => "EINVAL: failed to prepare terminal state",
            Message::Unimplemented => "ERROR: unimplemented action",
        }
    }
}

/// A reply to a command (`Response`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Response {
    pub id: u32,
    pub image_number: u32,
    pub placement_id: u32,
    pub message: Message,
}

impl Default for Response {
    fn default() -> Response {
        Response { id: 0, image_number: 0, placement_id: 0, message: Message::Ok }
    }
}

impl Response {
    /// A response with only a message (no ids: never sent unless ids are added).
    pub const fn message(message: Message) -> Response {
        Response { id: 0, image_number: 0, placement_id: 0, message }
    }

    pub fn ok(&self) -> bool {
        self.message == Message::Ok
    }

    /// No image id and no number: nothing is sent.
    pub fn empty(&self) -> bool {
        self.id == 0 && self.image_number == 0
    }

    /// `ESC _ G i=..[,I=..][,p=..];MSG ESC \`, or nothing for an [`Response::empty`] one.
    pub fn encode(&self) -> Wire {
        let mut w = Wire::new();
        if self.empty() {
            return w;
        }
        w.put(b"\x1b_G");
        let mut prior = false;
        for (key, value) in
            [(b"i=", self.id), (b"I=", self.image_number), (b"p=", self.placement_id)]
        {
            if value == 0 {
                continue;
            }
            if prior {
                w.put(b",");
            }
            prior = true;
            w.put(key);
            w.put_decimal(u64::from(value));
        }
        w.put(b";");
        w.put(self.message.as_str().as_bytes());
        w.put(b"\x1b\\");
        w
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alloc() -> Alloc<'static> {
        blaze_own::testing::heap().alloc()
    }

    fn parse(input: &[u8]) -> Result<Command<'static>, CommandError> {
        let mut p = Parser::new_in(MaxBytes(1024 * 1024), alloc());
        for &c in input {
            p.feed(c)?;
        }
        p.complete()
    }

    fn transmit(input: &[u8]) -> Transmission {
        match parse(input).unwrap().control {
            Control::Transmit(t) => t,
            other => panic!("not a transmit: {other:?}"),
        }
    }

    fn display(input: &[u8]) -> Display {
        match parse(input).unwrap().control {
            Control::Display(d) => d,
            other => panic!("not a display: {other:?}"),
        }
    }

    fn delete(input: &[u8]) -> Result<Delete, CommandError> {
        match parse(input)?.control {
            Control::Delete(d) => Ok(d),
            other => panic!("not a delete: {other:?}"),
        }
    }

    // ghostty graphics_command.zig:992 "transmission command"
    #[test]
    fn transmission_command() {
        let v = transmit(b"f=24,s=10,v=20");
        assert_eq!((v.format, v.width, v.height), (Format::Rgb, 10, 20));
    }

    // ghostty graphics_command.zig:1010 "feedSlice matches per-byte feed"
    #[test]
    fn feed_slice_matches_per_byte_feed() {
        let input = b"f=24,s=10,v=20;aGVsbG8gd29ybGQ=";
        let c1 = parse(input).unwrap();
        let mut p2 = Parser::new_in(MaxBytes(1024 * 1024), alloc());
        p2.feed_slice(input).unwrap();
        let c2 = p2.complete().unwrap();
        assert!(matches!(c1.control, Control::Transmit(_)));
        assert!(matches!(c2.control, Control::Transmit(_)));
        assert_eq!(c1.data.as_slice(), c2.data.as_slice());
    }

    // ghostty graphics_command.zig:1033 "feedSlice across slice boundaries"
    #[test]
    fn feed_slice_across_slice_boundaries() {
        let mut p = Parser::new_in(MaxBytes(1024 * 1024), alloc());
        p.feed_slice(b"f=24,s=10").unwrap();
        p.feed_slice(b",v=20;aGVsbG8g").unwrap();
        p.feed_slice(b"d29ybGQ=").unwrap();
        let c = p.complete().unwrap();
        assert!(matches!(c.control, Control::Transmit(_)));
        assert_eq!(c.data.as_slice(), b"hello world");
    }

    // ghostty graphics_command.zig:1051 "feedSlice respects max_bytes"
    #[test]
    fn feed_slice_respects_max_bytes() {
        let mut p = Parser::new_in(MaxBytes(4), alloc());
        p.feed_slice(b"f=24;ab").unwrap();
        assert_eq!(p.feed_slice(b"cde"), Err(CommandError::OutOfMemory));
    }

    #[test]
    fn feed_respects_max_bytes() {
        let mut p = Parser::new_in(MaxBytes(2), alloc());
        for &c in b";ab" {
            p.feed(c).unwrap();
        }
        assert_eq!(p.feed(b'c'), Err(CommandError::OutOfMemory));
    }

    // ghostty graphics_command.zig:1061 "transmission ignores 'm' if medium is not direct"
    #[test]
    fn transmission_ignores_m_if_medium_is_not_direct() {
        let v = transmit(b"a=t,t=t,m=1");
        assert_eq!(v.medium, Medium::TemporaryFile);
        assert!(!v.more_chunks);
    }

    // ghostty graphics_command.zig:1078 "transmission respects 'm' if medium is direct"
    #[test]
    fn transmission_respects_m_if_medium_is_direct() {
        let v = transmit(b"a=t,t=d,m=1");
        assert_eq!(v.medium, Medium::Direct);
        assert!(v.more_chunks);
    }

    // ghostty graphics_command.zig:1095 "query command"
    #[test]
    fn query_command() {
        let c = parse(b"i=31,s=1,v=1,a=q,t=d,f=24;QUFBQQ").unwrap();
        let Control::Query(v) = c.control else { panic!("{:?}", c.control) };
        assert_eq!((v.medium, v.width, v.height, v.image_id), (Medium::Direct, 1, 1, 31));
        assert_eq!(c.data.as_slice(), b"AAAA");
    }

    // ghostty graphics_command.zig:1115 "display command"
    #[test]
    fn display_command() {
        let v = display(b"a=p,U=1,i=31,c=80,r=120");
        assert_eq!((v.columns, v.rows, v.image_id), (80, 120, 31));
        assert!(v.virtual_placement);
    }

    // ghostty graphics_command.zig:1133 "delete command"
    #[test]
    fn delete_command() {
        assert_eq!(
            delete(b"a=d,d=p,x=3,y=4"),
            Ok(Delete::IntersectCell { free: FreeImages::No, x: 3, y: 4 })
        );
    }

    // ghostty graphics_command.zig:1153 "no control data"
    #[test]
    fn no_control_data() {
        let c = parse(b";QUFBQQ").unwrap();
        assert!(matches!(c.control, Control::Transmit(_)));
        assert_eq!(c.data.as_slice(), b"AAAA");
    }

    // ghostty graphics_command.zig:1168 "ignore unknown keys (long)"
    #[test]
    fn ignore_unknown_keys_long() {
        let v = transmit(b"f=24,s=10,v=20,hello=world");
        assert_eq!((v.format, v.width, v.height), (Format::Rgb, 10, 20));
    }

    // ghostty graphics_command.zig:1186 "ignore very long values"
    #[test]
    fn ignore_very_long_values() {
        let v = transmit(b"f=24,s=10,v=2000000000000000000000000000000000000000");
        assert_eq!((v.format, v.width, v.height), (Format::Rgb, 10, 0));
    }

    // ghostty graphics_command.zig:1204 "ensure very large negative values don't get skipped"
    #[test]
    fn ensure_very_large_negative_values_dont_get_skipped() {
        let v = display(b"a=p,i=1,z=-2000000000");
        assert_eq!((v.image_id, v.z), (1, -2_000_000_000));
    }

    // ghostty graphics_command.zig:1221 "ensure proper overflow error for u32"
    #[test]
    fn ensure_proper_overflow_error_for_u32() {
        assert_eq!(parse(b"a=p,i=10000000000").unwrap_err(), CommandError::Overflow);
    }

    // ghostty graphics_command.zig:1232 "ensure proper overflow error for i32"
    #[test]
    fn ensure_proper_overflow_error_for_i32() {
        assert_eq!(parse(b"a=p,i=1,z=-9999999999").unwrap_err(), CommandError::Overflow);
    }

    // ghostty graphics_command.zig:1243 "all i32 values"
    #[test]
    fn all_i32_values() {
        let v = display(b"a=p,i=1,z=-1");
        assert_eq!((v.image_id, v.z), (1, -1));
        let v = display(b"a=p,i=1,H=-1");
        assert_eq!((v.image_id, v.horizontal_offset), (1, -1));
        let v = display(b"a=p,i=1,V=-1");
        assert_eq!((v.image_id, v.vertical_offset), (1, -1));
    }

    // ghostty graphics_command.zig:1293 "response: encode nothing without ID or image number"
    #[test]
    fn response_encode_nothing_without_id_or_image_number() {
        assert_eq!(Response::default().encode().as_bytes(), b"");
    }

    // ghostty graphics_command.zig:1303 "response: encode with only image id"
    #[test]
    fn response_encode_with_only_image_id() {
        let r = Response { id: 4, ..Response::default() };
        assert_eq!(r.encode().as_bytes(), b"\x1b_Gi=4;OK\x1b\\");
    }

    // ghostty graphics_command.zig:1313 "response: encode with only image number"
    #[test]
    fn response_encode_with_only_image_number() {
        let r = Response { image_number: 4, ..Response::default() };
        assert_eq!(r.encode().as_bytes(), b"\x1b_GI=4;OK\x1b\\");
    }

    // ghostty graphics_command.zig:1323 "response: encode with image ID and number"
    #[test]
    fn response_encode_with_image_id_and_number() {
        let r = Response { id: 12, image_number: 4, ..Response::default() };
        assert_eq!(r.encode().as_bytes(), b"\x1b_Gi=12,I=4;OK\x1b\\");
    }

    #[test]
    fn response_encode_longest_fits() {
        let r = Response {
            id: u32::MAX,
            image_number: u32::MAX,
            placement_id: u32::MAX,
            message: Message::TemporaryFileNotNamedCorrectly,
        };
        let w = r.encode();
        assert_eq!(
            w.as_bytes(),
            b"\x1b_Gi=4294967295,I=4294967295,p=4294967295;EINVAL: temporary file not named correctly\x1b\\"
        );
    }

    // ghostty graphics_command.zig:1333 "delete range command 1"
    #[test]
    fn delete_range_command_1() {
        assert_eq!(
            delete(b"a=d,d=r,x=3,y=4"),
            Ok(Delete::Range { free: FreeImages::No, first: 3, last: 4 })
        );
    }

    // ghostty graphics_command.zig:1353 "delete range command 2"
    #[test]
    fn delete_range_command_2() {
        assert_eq!(
            delete(b"a=d,d=R,x=5,y=11"),
            Ok(Delete::Range { free: FreeImages::Yes, first: 5, last: 11 })
        );
    }

    // ghostty graphics_command.zig:1373 "delete range command 3"
    #[test]
    fn delete_range_command_3() {
        assert_eq!(delete(b"a=d,d=R,x=5,y=4"), Err(CommandError::InvalidFormat));
    }

    // ghostty graphics_command.zig:1384 "delete range command 4"
    #[test]
    fn delete_range_command_4() {
        assert_eq!(delete(b"a=d,d=R,x=5"), Err(CommandError::InvalidFormat));
    }

    // ghostty graphics_command.zig:1395 "delete range command 5"
    #[test]
    fn delete_range_command_5() {
        assert_eq!(delete(b"a=d,d=R,y=5"), Err(CommandError::InvalidFormat));
    }

    /// Inventory §9 rows a=t/T/q/p/d/f/a/c: each action letter maps to its control.
    #[test]
    fn inventory_actions() {
        let control = |s: &[u8]| parse(s).unwrap().control;
        assert!(matches!(control(b"a=t"), Control::Transmit(_)));
        assert!(matches!(control(b"a=T"), Control::TransmitAndDisplay(..)));
        assert!(matches!(control(b"a=q"), Control::Query(_)));
        assert!(matches!(control(b"a=p"), Control::Display(_)));
        assert!(matches!(control(b"a=d"), Control::Delete(Delete::All(FreeImages::No))));
        assert!(matches!(control(b"a=f"), Control::TransmitAnimationFrame(_)));
        assert!(matches!(
            control(b"a=a,s=3"),
            Control::ControlAnimation(AnimationControl { action: AnimationAction::Run, .. })
        ));
        assert!(matches!(
            control(b"a=c,C=1"),
            Control::ComposeAnimation(AnimationFrameComposition {
                composition_mode: CompositionMode::Overwrite,
                ..
            })
        ));
        assert_eq!(parse(b"a=x").unwrap_err(), CommandError::InvalidFormat);
        assert_eq!(parse(b"a=a,s=4").unwrap_err(), CommandError::InvalidFormat);
        // `a=116` is `t` by its code: a value is kept as a number.
        assert!(matches!(control(b"a=116"), Control::Transmit(_)));
        assert_eq!(parse(b"a=300").unwrap_err(), CommandError::InvalidFormat);
    }

    /// Inventory §9 rows d=a/A .. d=z/Z: each delete letter, lower and upper case.
    #[test]
    fn inventory_delete_letters() {
        use FreeImages::{No, Yes};
        let d = |s: &[u8]| delete(s).unwrap();
        assert_eq!(d(b"a=d,d=a"), Delete::All(No));
        assert_eq!(d(b"a=d,d=A"), Delete::All(Yes));
        assert_eq!(d(b"a=d,d=I,i=3,p=4"), Delete::Id { free: Yes, image_id: 3, placement_id: 4 });
        assert_eq!(
            d(b"a=d,d=n,I=3"),
            Delete::Newest { free: No, image_number: 3, placement_id: 0 }
        );
        assert_eq!(d(b"a=d,d=C"), Delete::IntersectCursor(Yes));
        assert_eq!(d(b"a=d,d=f"), Delete::AnimationFrames(No));
        assert_eq!(
            d(b"a=d,d=Q,x=1,y=2,z=-3"),
            Delete::IntersectCellZ { free: Yes, x: 1, y: 2, z: -3 }
        );
        assert_eq!(d(b"a=d,d=x,x=7"), Delete::Column { free: No, x: 7 });
        assert_eq!(d(b"a=d,d=Y,y=7"), Delete::Row { free: Yes, y: 7 });
        assert_eq!(d(b"a=d,d=z,z=-1"), Delete::Z { free: No, z: -1 });
        assert_eq!(delete(b"a=d,d=k"), Err(CommandError::InvalidFormat));
    }

    /// Inventory §9 rows f=, t=, o=z and the other keys.
    #[test]
    fn inventory_keys() {
        assert_eq!(transmit(b"f=32").format, Format::Rgba);
        assert_eq!(transmit(b"f=100").format, Format::Png);
        assert_eq!(transmit(b"i=1").format, Format::Rgba);
        // Nothing at all ends in the key state: refused.
        assert_eq!(parse(b"").unwrap_err(), CommandError::InvalidFormat);
        assert_eq!(parse(b"f=8").unwrap_err(), CommandError::InvalidFormat);
        assert_eq!(transmit(b"t=f").medium, Medium::File);
        assert_eq!(transmit(b"t=s").medium, Medium::SharedMemory);
        assert_eq!(parse(b"t=x").unwrap_err(), CommandError::InvalidFormat);
        assert_eq!(transmit(b"o=z").compression, Compression::ZlibDeflate);
        assert_eq!(parse(b"o=y").unwrap_err(), CommandError::InvalidFormat);
        let t = transmit(b"S=5,O=6,i=7,I=8,p=9");
        assert_eq!((t.size, t.offset, t.image_id, t.image_number, t.placement_id), (5, 6, 7, 8, 9));
        let d = display(b"a=p,x=1,y=2,w=3,h=4,X=5,Y=6,c=7,r=8,C=1,P=9,Q=10");
        assert_eq!((d.x, d.y, d.width, d.height, d.x_offset, d.y_offset), (1, 2, 3, 4, 5, 6));
        assert_eq!((d.columns, d.rows, d.parent_id, d.parent_placement_id), (7, 8, 9, 10));
        assert_eq!(d.cursor_movement, CursorMovement::None);
        assert_eq!(parse(b"a=p,C=2").unwrap_err(), CommandError::InvalidFormat);
        assert_eq!(parse(b"a=p,U=2").unwrap_err(), CommandError::InvalidFormat);
        assert_eq!(parse(b"q=1").unwrap().quiet, Quiet::Ok);
        assert_eq!(parse(b"q=2").unwrap().quiet, Quiet::Failures);
        assert_eq!(parse(b"q=3").unwrap_err(), CommandError::InvalidFormat);
        // The last of two equal keys wins.
        assert_eq!(transmit(b"s=1,s=2").width, 2);
        // A key left without a value is refused; a value left unfinished is read.
        assert_eq!(parse(b"s").unwrap_err(), CommandError::InvalidFormat);
        assert_eq!(transmit(b"s=3").width, 3);
        // An empty value is no number.
        assert_eq!(parse(b"s=,v=1").unwrap_err(), CommandError::InvalidCharacter);
        assert_eq!(parse(b";%%%").unwrap_err(), CommandError::InvalidData);
    }

    #[test]
    fn base64_loose_forms() {
        let dec = |s: &[u8]| {
            let mut v = s.to_vec();
            decode_base64_loose(&mut v).map(|n| v[..n].to_vec())
        };
        assert_eq!(dec(b"QUFBQQ"), Ok(b"AAAA".to_vec()));
        assert_eq!(dec(b"QUFBQQ=="), Ok(b"AAAA".to_vec()));
        assert_eq!(dec(b"QUF BQQ\n=\n="), Ok(b"AAAA".to_vec()));
        assert_eq!(dec(b"QUFBQUE"), Ok(b"AAAAA".to_vec()));
        assert_eq!(dec(b"QUFBQUE="), Ok(b"AAAAA".to_vec()));
        assert_eq!(dec(b""), Ok(b"".to_vec()));
        assert_eq!(dec(b"QUFBQUE=="), Err(InvalidBase64));
        assert_eq!(dec(b"QUFBQQ==="), Err(InvalidBase64));
        assert_eq!(dec(b"QUFBQ"), Err(InvalidBase64));
        assert_eq!(dec(b"Q"), Err(InvalidBase64));
        assert_eq!(dec(b"QU=FB"), Err(InvalidBase64));
        assert_eq!(dec(b"QU-B"), Err(InvalidBase64));
        // Loose: the bits after the last byte are not checked.
        assert_eq!(dec(b"QR"), Ok(b"A".to_vec()));
    }
}
