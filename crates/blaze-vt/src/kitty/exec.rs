//! Running a kitty graphics command against a screen's storage (ghostty's graphics_exec.zig).
//!
//! [`KittyGraphics`] is the state a terminal holds for this protocol: the two screens' storages,
//! the generation counter, and a load waiting on the host. The core lane owns it and passes the
//! screen in as a [`GraphicsScreen`]; [`KittyGraphics::handle`] returns an [`Outcome`]: the
//! reply to write to the PTY, the cursor movement after a placement, and whether the host must
//! do IO ([`KittyGraphics::pending`], answered by [`KittyGraphics::answer`]).

use blaze_own::{AVec, Alloc};

use super::command::{Command, Control, CursorMovement, Display, Message, Quiet, Response};
use super::image::{
    DecodePng, FileRead, ImageError, Init, IoFailed, LoadingImage, Media, NeedsIo, Resolved,
};
use super::storage::{
    Generations, GraphicsScreen, ImageStorage, Location, Placement, StorageLimit,
};

/// Which of the terminal's screens a command ran on.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ScreenKey {
    Primary,
    Alternate,
}

/// How kitty graphics start (ghostty's `Screen.Options` kitty fields).
#[derive(Clone, Copy, Debug)]
pub struct GraphicsOpts {
    /// Each screen's byte cap; 0 disables kitty graphics.
    pub storage_limit: StorageLimit,
    /// The media beyond direct accepted.
    pub media: Media,
    /// The PNG decoder, `None` for no PNG (`f=100` refused).
    pub png: Option<DecodePng>,
}

impl GraphicsOpts {
    /// ghostty's defaults: 320 MB, direct only, no PNG decoder.
    pub const DEFAULT: GraphicsOpts =
        GraphicsOpts { storage_limit: StorageLimit::DEFAULT, media: Media::DIRECT, png: None };
}

/// The cursor movement after a placement with `C=0` (ghostty's `display`): index `index` times
/// (scrolling as a linefeed does), then `setCursorPos(cursor.y, col_1based)` with the 0-based row
/// the cursor is then on given as the 1-based row, so the cursor ends on the image's last row
/// (one up from where the indexes left it, unless that is row 0), just right of the image.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct CursorAfter {
    pub index: u32,
    pub col_1based: u32,
}

/// What a command did that the core acts on.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Outcome {
    /// The reply to write to the PTY.
    pub reply: Option<Response>,
    /// The cursor movement to do, after the reply is queued.
    pub cursor: Option<CursorAfter>,
    /// The command waits on host IO ([`KittyGraphics::pending`]).
    pub waits_on_host: bool,
}

/// Commands dropped by design, counted (rule 3).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct GraphicsStats {
    /// Commands on a screen whose storage is disabled (limit 0).
    pub disabled: u64,
    /// Replies not sent because of `q=1` or `q=2`.
    pub quieted: u64,
    /// Error replies not sent because they carry no image id or number (ghostty's
    /// `if (resp.empty()) null`): e.g. `a=p` with neither `i` nor `I`.
    pub idless_errors: u64,
    /// Loads waiting on host IO dropped by a later command or a reset.
    pub superseded: u64,
}

/// The IO a waiting load asks of the host.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Request<'p> {
    /// `realpath(path)`, and the temporary directory (`TMPDIR` or the default, and resolved):
    /// answer [`Answer::Resolved`].
    Resolve { path: &'p [u8] },
    /// Open `path`, seek to `read.offset`, read at most `read.most` bytes, close it; delete it
    /// when `read.unlink` says so (even if the read failed): answer [`Answer::Read`].
    Read { path: &'p [u8], read: FileRead },
    /// `shm_open(name, O_RDONLY)`, `fstat`, map all of it, close, `shm_unlink`: answer
    /// [`Answer::SharedMemory`] with the whole mapping.
    SharedMemory { name: &'p [u8] },
}

/// The host's answer to a [`Request`].
#[derive(Clone, Copy, Debug)]
pub enum Answer<'h> {
    Resolved(Resolved<'h>),
    Read(&'h [u8]),
    SharedMemory(&'h [u8]),
    /// The IO failed (any step): the command fails with `EINVAL: invalid data`.
    Failed(IoFailed),
}

/// The host answered what was not asked, or nothing was pending: a host bug.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct UnaskedAnswer;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Purpose {
    /// `a=q`: only `LoadingImage.init` runs.
    Query,
    /// `a=t` or `a=T`; `more` is the command's `m=1`.
    Transmit { more: bool },
}

#[derive(Debug)]
enum Stage<'a> {
    Resolve,
    Read { real: AVec<'a, u8>, read: FileRead },
    SharedMemory,
}

#[derive(Debug)]
struct Pending<'a> {
    screen: ScreenKey,
    purpose: Purpose,
    loading: LoadingImage<'a>,
    /// The command's payload: the path or shared-memory name.
    path: AVec<'a, u8>,
    stage: Stage<'a>,
    result: Response,
    quiet: Quiet,
}

/// What `loadAndAddImage` gave.
enum Loaded {
    /// More chunks to come (`m=1`).
    More,
    /// Stored, with the display of a transmit-and-display.
    Stored { id: u32, implicit_id: bool, display: Option<Display> },
}

enum Load<'a> {
    Done(Result<Loaded, ImageError>),
    Waits(LoadingImage<'a>, NeedsIo),
}

/// A terminal's kitty graphics state.
pub struct KittyGraphics<'a> {
    alloc: Alloc<'a>,
    png: Option<DecodePng>,
    primary: ImageStorage<'a>,
    alternate: ImageStorage<'a>,
    generations: Generations,
    pending: Option<Pending<'a>>,
    stats: GraphicsStats,
}

impl<'a> KittyGraphics<'a> {
    pub fn new(opts: GraphicsOpts, alloc: Alloc<'a>) -> KittyGraphics<'a> {
        KittyGraphics {
            alloc,
            png: opts.png,
            primary: ImageStorage::new_in(opts.storage_limit, opts.media, alloc),
            alternate: ImageStorage::new_in(opts.storage_limit, opts.media, alloc),
            generations: Generations::default(),
            pending: None,
            stats: GraphicsStats::default(),
        }
    }

    pub fn storage(&self, screen: ScreenKey) -> &ImageStorage<'a> {
        match screen {
            ScreenKey::Primary => &self.primary,
            ScreenKey::Alternate => &self.alternate,
        }
    }

    /// The storage, for the renderer to clear `dirty`.
    pub fn storage_mut(&mut self, screen: ScreenKey) -> &mut ImageStorage<'a> {
        match screen {
            ScreenKey::Primary => &mut self.primary,
            ScreenKey::Alternate => &mut self.alternate,
        }
    }

    pub fn stats(&self) -> GraphicsStats {
        self.stats
    }

    /// The generation counter, for a core that runs storage operations itself.
    pub fn generations(&mut self) -> &mut Generations {
        &mut self.generations
    }

    /// Set every screen's byte cap (`setKittyGraphicsSizeLimit`). Each screen is passed so the
    /// pins of what is evicted are untracked.
    pub fn set_limit(
        &mut self,
        limit: StorageLimit,
        primary: &mut impl GraphicsScreen,
        alternate: &mut impl GraphicsScreen,
    ) -> Result<(), super::storage::OutOfMemory> {
        self.primary.set_limit(limit, primary, &mut self.generations)?;
        self.alternate.set_limit(limit, alternate, &mut self.generations)
    }

    /// Set every screen's media (`setKittyGraphicsLoadingLimits`).
    pub fn set_media(&mut self, media: Media) {
        self.primary.media = media;
        self.alternate.media = media;
    }

    /// A screen's reset (`Screen.reset`): its storage goes back to the defaults, limit and media
    /// included (ghostty assigns `.{ .dirty = true }`), and a load waiting for it is dropped.
    pub fn reset_screen(&mut self, which: ScreenKey, screen: &mut impl GraphicsScreen) {
        self.drop_pending_for(which);
        let s = self.storage_mut(which);
        s.reset(screen);
        s.media = Media::DIRECT;
        s.dirty = true;
    }

    /// Erase in display, all or scrollback (`eraseDisplay` `.complete`, `.scrollback`): every
    /// placement but the virtual ones goes, images stay (`d=a`).
    pub fn erase_display(&mut self, which: ScreenKey, screen: &mut impl GraphicsScreen) {
        let gens = &mut self.generations;
        let s = match which {
            ScreenKey::Primary => &mut self.primary,
            ScreenKey::Alternate => &mut self.alternate,
        };
        s.delete(super::command::Delete::All(super::command::FreeImages::No), screen, gens);
    }

    /// The IO a waiting load asks of the host, if any.
    pub fn pending(&self) -> Option<Request<'_>> {
        let p = self.pending.as_ref()?;
        Some(match &p.stage {
            Stage::Resolve => Request::Resolve { path: &p.path },
            Stage::Read { real, read } => Request::Read { path: real, read: *read },
            Stage::SharedMemory => Request::SharedMemory { name: &p.path },
        })
    }

    /// The screen a waiting load is for: the answer is given that screen.
    pub fn pending_screen(&self) -> Option<ScreenKey> {
        self.pending.as_ref().map(|p| p.screen)
    }

    fn drop_pending_for(&mut self, which: ScreenKey) {
        if self.pending.as_ref().is_some_and(|p| p.screen == which) {
            self.pending = None;
            self.stats.superseded += 1;
        }
    }

    /// Run a kitty graphics command (`execute`) on the active screen `which`.
    pub fn handle(
        &mut self,
        cmd: Command<'a>,
        which: ScreenKey,
        screen: &mut impl GraphicsScreen,
    ) -> Outcome {
        if self.pending.take().is_some() {
            self.stats.superseded += 1;
        }
        if !self.storage(which).enabled() {
            self.stats.disabled += 1;
            return Outcome::default();
        }
        let mut quiet = cmd.quiet;
        let mut out = Outcome::default();
        let control = cmd.control;
        let resp = match control {
            Control::Query(_) => self.query(cmd, which, &mut out),
            Control::Display(d) => self.display(d, which, screen, &mut out),
            Control::Delete(d) => {
                let gens = &mut self.generations;
                let s = match which {
                    ScreenKey::Primary => &mut self.primary,
                    ScreenKey::Alternate => &mut self.alternate,
                };
                s.delete(d, screen, gens);
                Some(Response::default())
            }
            Control::Transmit(_) | Control::TransmitAndDisplay(..) => {
                if let Some(loading) = self.storage_mut(which).loading.as_mut() {
                    match cmd.quiet {
                        Quiet::No => quiet = loading.quiet,
                        q => loading.quiet = q,
                    }
                }
                self.transmit(cmd, quiet, which, screen, &mut out)
            }
            Control::TransmitAnimationFrame(_)
            | Control::ControlAnimation(_)
            | Control::ComposeAnimation(_) => Some(Response::message(Message::Unimplemented)),
        };
        match resp {
            Some(r) => self.finish(r, quiet, out),
            None => out,
        }
    }

    /// The host's answer to [`KittyGraphics::pending`]; `screen` is the one
    /// [`KittyGraphics::pending_screen`] names.
    pub fn answer(
        &mut self,
        answer: Answer<'_>,
        screen: &mut impl GraphicsScreen,
    ) -> Result<Outcome, UnaskedAnswer> {
        let mut p = self.pending.take().ok_or(UnaskedAnswer)?;
        let stage = core::mem::replace(&mut p.stage, Stage::Resolve);
        let step = match (stage, answer) {
            (_, Answer::Failed(IoFailed)) => Err(ImageError::InvalidData),
            (Stage::Resolve, Answer::Resolved(at)) => match p.loading.resolved(at) {
                Err(e) => Err(e),
                Ok(read) => {
                    let mut real = AVec::new_in(self.alloc);
                    if real.try_extend_from_slice(at.real).is_ok() {
                        p.stage = Stage::Read { real, read };
                        self.pending = Some(p);
                        return Ok(Outcome { waits_on_host: true, ..Outcome::default() });
                    }
                    Err(ImageError::OutOfMemory)
                }
            },
            (Stage::Read { read, .. }, Answer::Read(bytes)) => p.loading.file_read(read, bytes),
            (Stage::SharedMemory, Answer::SharedMemory(segment)) => {
                p.loading.shared_memory(segment)
            }
            (stage, _) => {
                p.stage = stage;
                self.pending = Some(p);
                return Err(UnaskedAnswer);
            }
        };
        let which = p.screen;
        let mut out = Outcome::default();
        let mut result = p.result;
        let resp = match (step, p.purpose) {
            (Err(e), _) => {
                result.message = e.message();
                Some(result)
            }
            (Ok(()), Purpose::Query) => Some(result),
            (Ok(()), Purpose::Transmit { more }) => {
                let loaded = self.after_init(p.loading, more, which, screen);
                self.transmit_tail(loaded, result, which, screen, &mut out)
            }
        };
        Ok(match resp {
            Some(r) => self.finish(r, p.quiet, out),
            None => out,
        })
    }

    /// The quiet rules (`execute`'s tail).
    fn finish(&mut self, resp: Response, quiet: Quiet, mut out: Outcome) -> Outcome {
        let send = match quiet {
            Quiet::No => !resp.empty(),
            Quiet::Ok => !resp.ok(),
            Quiet::Failures => false,
        };
        if send {
            out.reply = Some(resp);
        } else if !resp.empty() {
            self.stats.quieted += 1;
        } else if !resp.ok() {
            self.stats.idless_errors += 1;
        }
        out
    }

    fn query(
        &mut self,
        mut cmd: Command<'a>,
        which: ScreenKey,
        out: &mut Outcome,
    ) -> Option<Response> {
        let Some(t) = cmd.transmission() else { unreachable!("a query has a transmission") };
        if t.image_id == 0 {
            return Some(Response::message(Message::ImageIdRequired));
        }
        let mut result = Response {
            id: t.image_id,
            image_number: t.image_number,
            placement_id: t.placement_id,
            message: Message::Ok,
        };
        let media = self.storage(which).media;
        match LoadingImage::init(&cmd, media, self.png, self.alloc) {
            Err(e) => result.message = e.message(),
            Ok(Init::Ready(_)) => {}
            Ok(Init::Waits(loading, io)) => {
                let path = core::mem::replace(&mut cmd.data, AVec::new_in(self.alloc));
                self.wait(which, Purpose::Query, loading, io, path, result, cmd.quiet, out);
                return None;
            }
        }
        Some(result)
    }

    #[allow(clippy::too_many_arguments)]
    fn wait(
        &mut self,
        screen: ScreenKey,
        purpose: Purpose,
        loading: LoadingImage<'a>,
        io: NeedsIo,
        path: AVec<'a, u8>,
        result: Response,
        quiet: Quiet,
        out: &mut Outcome,
    ) {
        let stage = match io {
            NeedsIo::Resolve => Stage::Resolve,
            NeedsIo::SharedMemory => Stage::SharedMemory,
        };
        self.pending = Some(Pending { screen, purpose, loading, path, stage, result, quiet });
        out.waits_on_host = true;
    }

    fn transmit(
        &mut self,
        mut cmd: Command<'a>,
        quiet: Quiet,
        which: ScreenKey,
        screen: &mut impl GraphicsScreen,
        out: &mut Outcome,
    ) -> Option<Response> {
        let Some(t) = cmd.transmission() else { unreachable!("a transmit has a transmission") };
        let result = Response {
            id: t.image_id,
            image_number: t.image_number,
            placement_id: t.placement_id,
            message: Message::Ok,
        };
        if t.image_id > 0 && t.image_number > 0 {
            return Some(Response::message(Message::IdAndNumberExclusive));
        }
        match self.load_and_add(&cmd, which, screen) {
            Load::Done(loaded) => self.transmit_tail(loaded, result, which, screen, out),
            Load::Waits(loading, io) => {
                let path = core::mem::replace(&mut cmd.data, AVec::new_in(self.alloc));
                let purpose = Purpose::Transmit { more: t.more_chunks };
                self.wait(which, purpose, loading, io, path, result, quiet, out);
                None
            }
        }
    }

    /// `transmit` after the image is loaded: display it if asked, and the reply.
    fn transmit_tail(
        &mut self,
        loaded: Result<Loaded, ImageError>,
        mut result: Response,
        which: ScreenKey,
        screen: &mut impl GraphicsScreen,
        out: &mut Outcome,
    ) -> Option<Response> {
        let (id, implicit_id, display) = match loaded {
            Err(e) => {
                result.message = e.message();
                return Some(result);
            }
            Ok(Loaded::More) => return Some(Response::default()),
            Ok(Loaded::Stored { id, implicit_id, display }) => (id, implicit_id, display),
        };
        if let Some(mut d) = display {
            d.image_id = id;
            result = self.display(d, which, screen, out).unwrap_or_default();
        }
        if implicit_id {
            return Some(Response::default());
        }
        result.id = id;
        Some(result)
    }

    /// `loadAndAddImage`: a chunk added to the load in progress, or a new load.
    fn load_and_add(
        &mut self,
        cmd: &Command<'a>,
        which: ScreenKey,
        screen: &mut impl GraphicsScreen,
    ) -> Load<'a> {
        let Some(t) = cmd.transmission() else { unreachable!("a transmit has a transmission") };
        let storage = self.storage_mut(which);
        if let Some(loading) = storage.loading.as_mut() {
            // A failed chunk leaves the load in place, as ghostty does.
            if let Err(e) = loading.add_data(&cmd.data) {
                return Load::Done(Err(e));
            }
            if t.more_chunks {
                return Load::Done(Ok(Loaded::More));
            }
            let loading = storage.loading.take().expect("checked above");
            return Load::Done(self.after_init(loading, false, which, screen));
        }
        let media = storage.media;
        match LoadingImage::init(cmd, media, self.png, self.alloc) {
            Err(e) => Load::Done(Err(e)),
            Ok(Init::Ready(loading)) => {
                Load::Done(self.after_init(loading, t.more_chunks, which, screen))
            }
            Ok(Init::Waits(loading, io)) => Load::Waits(loading, io),
        }
    }

    /// `loadAndAddImage` from the id: an implicit id, a stored load, or the image completed
    /// and stored.
    fn after_init(
        &mut self,
        mut loading: LoadingImage<'a>,
        more: bool,
        which: ScreenKey,
        screen: &mut impl GraphicsScreen,
    ) -> Result<Loaded, ImageError> {
        let png = self.png;
        let gens = &mut self.generations;
        let storage = match which {
            ScreenKey::Primary => &mut self.primary,
            ScreenKey::Alternate => &mut self.alternate,
        };
        if loading.image.id == 0 {
            loading.image.id = storage.next_image_id;
            storage.next_image_id = storage.next_image_id.wrapping_add(1);
            if loading.image.number == 0 {
                loading.image.implicit_id = true;
            }
        }
        if more {
            storage.loading = Some(loading);
            return Ok(Loaded::More);
        }
        let display = loading.display;
        let img = loading.complete(png)?;
        let (id, implicit_id) = (img.id, img.implicit_id);
        storage.add_image(img, screen, gens).map_err(|_| ImageError::OutOfMemory)?;
        Ok(Loaded::Stored { id, implicit_id, display })
    }

    /// `display`: place a stored image at the cursor or virtually.
    fn display(
        &mut self,
        d: Display,
        which: ScreenKey,
        screen: &mut impl GraphicsScreen,
        out: &mut Outcome,
    ) -> Option<Response> {
        if d.image_id == 0 && d.image_number == 0 {
            return Some(Response::message(Message::IdOrNumberRequired));
        }
        let mut result = Response {
            id: d.image_id,
            image_number: d.image_number,
            placement_id: d.placement_id,
            message: Message::Ok,
        };
        let gens = &mut self.generations;
        let storage = match which {
            ScreenKey::Primary => &mut self.primary,
            ScreenKey::Alternate => &mut self.alternate,
        };
        let img = match d.image_id {
            0 => storage.image_by_number(d.image_number),
            id => storage.image_by_id(id),
        };
        let Some(img) = img else {
            result.message = Message::ImageNotFound;
            return Some(result);
        };
        let image_id = img.id;
        result.id = image_id;
        let location = if d.virtual_placement {
            if d.parent_id > 0 {
                result.message = Message::VirtualWithParent;
                return Some(result);
            }
            Location::Virtual
        } else {
            match screen.track_cursor() {
                Ok(pin) => Location::Pin(pin),
                Err(_) => {
                    result.message = Message::TerminalState;
                    return Some(result);
                }
            }
        };
        let p = Placement {
            location,
            x_offset: d.x_offset,
            y_offset: d.y_offset,
            source_x: d.x,
            source_y: d.y,
            source_width: d.width,
            source_height: d.height,
            columns: d.columns,
            rows: d.rows,
            z: d.z,
        };
        if storage.add_placement(image_id, result.placement_id, p, gens).is_err() {
            if let Location::Pin(pin) = location {
                screen.untrack(pin);
            }
            result.message = Message::OutOfMemory;
            return Some(result);
        }
        if let (Location::Pin(pin), CursorMovement::After) = (location, d.cursor_movement) {
            let img = storage.image_by_id(image_id).expect("placed above");
            let size = p.grid_size(img, screen.grid());
            let col = u32::from(screen.locate(pin).col);
            out.cursor = Some(CursorAfter {
                index: size.rows,
                col_1based: col.saturating_add(size.cols).saturating_add(1),
            });
        }
        Some(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kitty::command::{MaxBytes, Parser};
    use crate::kitty::image::{Generation, PngError, Unlink};
    use crate::kitty::storage::tests::{FakeScreen, alloc};
    use crate::kitty::storage::{ActivePoint, PlacementKey, ScreenPoint};

    fn cmd(input: &str) -> Command<'static> {
        let mut p = Parser::new_in(MaxBytes(1024 * 1024), alloc());
        p.feed_slice(input.as_bytes()).unwrap();
        p.complete().unwrap()
    }

    struct T {
        k: KittyGraphics<'static>,
        s: FakeScreen,
    }

    impl T {
        fn new() -> T {
            T::with(GraphicsOpts::DEFAULT)
        }
        fn with(opts: GraphicsOpts) -> T {
            T { k: KittyGraphics::new(opts, alloc()), s: FakeScreen::new(5, 5) }
        }
        fn run(&mut self, input: &str) -> Outcome {
            self.k.handle(cmd(input), ScreenKey::Primary, &mut self.s)
        }
        fn reply(&mut self, input: &str) -> Option<Response> {
            self.run(input).reply
        }
        fn storage(&self) -> &ImageStorage<'static> {
            self.k.storage(ScreenKey::Primary)
        }
    }

    // ghostty graphics_exec.zig:396 "kittygfx more chunks with q=1"
    #[test]
    fn kittygfx_more_chunks_with_q_1() {
        let mut t = T::new();
        assert_eq!(t.reply("a=T,f=24,t=d,i=1,s=1,v=2,c=10,r=1,m=1,q=1;////"), None);
        assert_eq!(t.reply("m=0;////"), None);
    }

    // ghostty graphics_exec.zig:426 "kittygfx more chunks with q=0"
    #[test]
    fn kittygfx_more_chunks_with_q_0() {
        let mut t = T::new();
        assert_eq!(t.reply("a=t,f=24,t=d,s=1,v=2,c=10,r=1,m=1,i=1,q=0;////"), None);
        assert!(t.reply("m=0;////").unwrap().ok());
    }

    // ghostty graphics_exec.zig:456 "kittygfx more chunks with chunk increasing q"
    #[test]
    fn kittygfx_more_chunks_with_chunk_increasing_q() {
        let mut t = T::new();
        assert_eq!(t.reply("a=t,f=24,t=d,s=1,v=2,c=10,r=1,m=1,i=1,q=0;////"), None);
        assert_eq!(t.reply("m=0,q=1;////"), None);
    }

    // ghostty graphics_exec.zig:486 "kittygfx default format is rgba"
    #[test]
    fn kittygfx_default_format_is_rgba() {
        let mut t = T::new();
        assert!(t.reply("a=t,t=d,i=1,s=1,v=2,c=10,r=1;///////////").unwrap().ok());
        assert_eq!(t.storage().image_by_id(1).unwrap().format, crate::kitty::command::Format::Rgba);
    }

    // ghostty graphics_exec.zig:506 "kittygfx test valid u32 (expect invalid image ID)"
    #[test]
    fn kittygfx_test_valid_u32_expect_invalid_image_id() {
        let mut t = T::new();
        let r = t.reply("a=p,i=4294967295").unwrap();
        assert!(!r.ok());
        assert_eq!(r.message.as_str(), "ENOENT: image not found");
    }

    // ghostty graphics_exec.zig:523 "kittygfx test valid i32 (expect invalid image ID)"
    #[test]
    fn kittygfx_test_valid_i32_expect_invalid_image_id() {
        let mut t = T::new();
        let r = t.reply("a=p,i=1,z=-2147483648").unwrap();
        assert!(!r.ok());
        assert_eq!(r.message.as_str(), "ENOENT: image not found");
    }

    // ghostty graphics_exec.zig:540 "kittygfx no response with no image ID or number"
    #[test]
    fn kittygfx_no_response_with_no_image_id_or_number() {
        let mut t = T::new();
        assert_eq!(t.reply("a=t,f=24,t=d,s=1,v=2,c=10,r=1,i=0,I=0;////////"), None);
        // It is stored under an implicit id all the same.
        assert!(t.storage().image_by_id(2_147_483_647).is_some_and(|i| i.implicit_id));
    }

    // ghostty graphics_exec.zig:558 "kittygfx no response with no image ID or number load and display"
    #[test]
    fn kittygfx_no_response_with_no_image_id_or_number_load_and_display() {
        let mut t = T::new();
        assert_eq!(t.reply("a=T,f=24,t=d,s=1,v=2,c=10,r=1,i=0,I=0;////////"), None);
        assert_eq!(t.storage().placement_count(), 1);
    }

    // ghostty graphics_exec.zig:576 "kittygfx retransmit same id gets fresh image generation"
    #[test]
    fn kittygfx_retransmit_same_id_gets_fresh_image_generation() {
        let mut t = T::new();
        assert!(t.reply("a=t,t=d,f=24,i=1,s=1,v=2;////////").unwrap().ok());
        let gen1 = t.storage().image_by_id(1).unwrap().generation;
        assert!(gen1 > Generation(0));
        assert_eq!(gen1, t.storage().generation);
        assert!(t.reply("a=t,t=d,f=24,i=1,s=1,v=2;AAAAAAAA").unwrap().ok());
        let gen2 = t.storage().image_by_id(1).unwrap().generation;
        assert!(gen2 > gen1);
        assert_eq!(gen2, t.storage().generation);
    }

    // ghostty graphics_exec.zig:615 "kittygfx delete then retransmit same id gets fresh generation"
    #[test]
    fn kittygfx_delete_then_retransmit_same_id_gets_fresh_generation() {
        let mut t = T::new();
        assert!(t.reply("a=T,t=d,f=24,i=1,s=1,v=2,c=1,r=1;////////").unwrap().ok());
        let gen1 = t.storage().image_by_id(1).unwrap().generation;
        assert_eq!(t.reply("a=d,d=A"), None);
        assert!(t.storage().image_by_id(1).is_none());
        let gen_delete = t.storage().generation;
        assert!(gen_delete > gen1);
        assert!(t.reply("a=t,t=d,f=24,i=1,s=1,v=2;////////").unwrap().ok());
        let gen2 = t.storage().image_by_id(1).unwrap().generation;
        assert!(gen2 > gen1 && gen2 > gen_delete);
    }

    /// Inventory §9: `ESC _ G i=..;OK ESC \` and the error replies, with `I` and `p` echoed.
    #[test]
    fn replies_encode_ids_and_messages() {
        let mut t = T::new();
        let r = t.reply("a=t,f=24,i=7,p=3,s=1,v=1;AAAA").unwrap();
        assert_eq!(r.encode().as_bytes(), b"\x1b_Gi=7,p=3;OK\x1b\\");
        let r = t.reply("a=t,f=24,I=9,s=1,v=1;AAAA").unwrap();
        assert_eq!(r.encode().as_bytes(), b"\x1b_Gi=2147483647,I=9;OK\x1b\\");
        let r = t.reply("a=t,f=24,i=8,s=2,v=1;AAAA").unwrap();
        assert_eq!(r.encode().as_bytes(), b"\x1b_Gi=8;EINVAL: invalid data\x1b\\");
        // Both an id and a number: refused, and with no id the refusal is not sent.
        assert_eq!(t.reply("a=t,f=24,i=8,I=9,s=1,v=1;AAAA"), None);
        assert_eq!(t.k.stats().idless_errors, 1);
        // q=2 sends nothing; q=1 sends errors only.
        assert_eq!(t.reply("a=t,f=24,i=8,s=2,v=1,q=2;AAAA"), None);
        assert!(t.reply("a=t,f=24,i=8,s=2,v=1,q=1;AAAA").is_some());
        assert_eq!(t.reply("a=t,f=24,i=8,s=1,v=1,q=1;AAAA"), None);
        assert_eq!(t.k.stats().quieted, 2);
    }

    /// Inventory §9: `a=q` checks the image and stores nothing.
    #[test]
    fn query_checks_without_storing() {
        let mut t = T::new();
        let r = t.reply("a=q,f=24,i=31,s=1,v=1;AAAA").unwrap();
        assert!(r.ok());
        assert_eq!(r.id, 31);
        assert_eq!(t.storage().image_count(), 0);
        // Query runs `init` only: a short payload is not refused (complete never runs).
        assert!(t.reply("a=q,f=24,i=31,s=2,v=2;AAAA").unwrap().ok());
        assert_eq!(
            t.reply("a=q,f=24,t=f,i=31;L3RtcC94").unwrap().message,
            Message::UnsupportedMedium
        );
        assert_eq!(t.reply("a=q,f=24,s=1,v=1;AAAA"), None);
    }

    /// Inventory §9: display, its errors, and the cursor after it.
    #[test]
    fn display_places_and_moves_cursor() {
        let mut t = T::with(GraphicsOpts::DEFAULT);
        t.s = FakeScreen::new(10, 10).px(100, 100);
        t.s.cursor = ActivePoint { row: 1, col: 2 };
        // 20x20 pixels in 10x10 cells: 2x2 cells. The cursor is indexed twice, then put right
        // of the image.
        let mut data = std::string::String::from("a=t,f=24,i=1,s=20,v=20;");
        data.push_str(&"AAAA".repeat(400));
        assert!(t.reply(&data).unwrap().ok());
        let out = t.run("a=p,i=1,p=5");
        assert!(out.reply.unwrap().ok());
        assert_eq!(out.cursor, Some(CursorAfter { index: 2, col_1based: 5 }));
        let at = t.storage().placement(PlacementKey::external(1, 5)).unwrap().location;
        let Location::Pin(pin) = at else { panic!("not at a pin") };
        assert_eq!(t.s.locate(pin), ScreenPoint { row: 1, col: 2 });
        // C=1 leaves the cursor.
        assert_eq!(t.run("a=p,i=1,C=1").cursor, None);
        // By number.
        assert!(t.reply("a=t,f=24,I=4,s=1,v=1;AAAA").is_some());
        assert_eq!(t.reply("a=p,I=4").unwrap().id, 2_147_483_647);
        // Neither id nor number: refused, and not sent.
        assert_eq!(t.reply("a=p"), None);
        // Virtual with a parent.
        assert_eq!(t.reply("a=p,i=1,U=1,P=2").unwrap().message, Message::VirtualWithParent);
        let out = t.run("a=p,i=1,U=1,p=9");
        assert!(out.reply.unwrap().ok());
        assert_eq!(out.cursor, None);
        assert_eq!(
            t.storage().placement(PlacementKey::external(1, 9)).unwrap().location,
            Location::Virtual
        );
        // The screen could not track a pin.
        t.s.refuse_track = true;
        assert_eq!(t.reply("a=p,i=1").unwrap().message, Message::TerminalState);
    }

    /// Inventory §9: transmit-and-display reports the display's error under the image id.
    #[test]
    fn transmit_and_display_reports_display_errors() {
        let mut t = T::new();
        let r = t.reply("a=T,f=24,i=3,U=1,P=1,s=1,v=1;AAAA").unwrap();
        assert_eq!((r.id, r.message), (3, Message::VirtualWithParent));
        assert!(t.storage().image_by_id(3).is_some());
        // An implicit id swallows it.
        assert_eq!(t.reply("a=T,f=24,U=1,P=1,s=1,v=1;AAAA"), None);
    }

    /// Inventory §9: animation actions are unimplemented, as in ghostty.
    #[test]
    fn animation_is_unimplemented() {
        let mut t = T::new();
        for a in ["a=f,i=1", "a=a,i=1", "a=c,i=1"] {
            assert_eq!(t.reply(a), None);
        }
        assert_eq!(t.k.stats().idless_errors, 3);
    }

    /// Inventory §9: a limit of 0 disables kitty graphics on a screen.
    #[test]
    fn disabled_storage_drops_commands() {
        let mut t =
            T::with(GraphicsOpts { storage_limit: StorageLimit(0), ..GraphicsOpts::DEFAULT });
        assert_eq!(t.reply("a=t,f=24,i=1,s=1,v=1;AAAA"), None);
        assert_eq!(t.k.stats().disabled, 1);
    }

    /// Inventory §9: chunked transmission keeps the first chunk's display and id; a failed
    /// chunk leaves the load in place.
    #[test]
    fn chunked_transmit_and_display() {
        let mut t = T::new();
        assert_eq!(t.reply("a=T,f=24,i=4,s=1,v=2,p=2,m=1;////"), None);
        assert!(t.storage().is_loading());
        let r = t.reply("m=0;////").unwrap();
        // The reply is the display's: its placement id.
        assert_eq!((r.id, r.placement_id, r.ok()), (4, 2, true));
        assert!(!t.storage().is_loading());
        assert!(t.storage().placement(PlacementKey::external(4, 2)).is_some());
        // A transmit while loading continues it, even one naming a file.
        assert_eq!(t.reply("a=t,f=24,i=5,s=1,v=1,m=1;AA"), None);
        assert!(t.reply("a=t,t=f,m=0;AAA").unwrap().ok());
        assert!(t.storage().image_by_id(5).is_some());
    }

    fn png_never<'a>(
        _: &[u8],
        _: Alloc<'a>,
    ) -> Result<crate::kitty::image::DecodedPng<'a>, PngError> {
        Err(PngError::InvalidData)
    }

    /// Inventory §9: t=f, t=t and t=s are host requests; the image completes on the answer.
    #[test]
    fn file_media_are_host_requests() {
        let mut t = T::with(GraphicsOpts { media: Media::ALL, ..GraphicsOpts::DEFAULT });
        // "/tmp/tty-graphics-protocol-x" in base64.
        let out = t.run("a=T,f=24,t=t,i=6,s=1,v=1;L3RtcC90dHktZ3JhcGhpY3MtcHJvdG9jb2wteA==");
        assert_eq!(out, Outcome { waits_on_host: true, ..Outcome::default() });
        assert_eq!(t.k.pending(), Some(Request::Resolve { path: b"/tmp/tty-graphics-protocol-x" }));
        assert_eq!(t.k.pending_screen(), Some(ScreenKey::Primary));
        let at = Resolved {
            real: b"/private/tmp/tty-graphics-protocol-x",
            temp_dir: None,
            temp_dir_real: None,
        };
        // /private/tmp is not /tmp: the temporary directory must say so.
        let r = t.k.answer(Answer::Resolved(at), &mut t.s).unwrap().reply.unwrap();
        assert_eq!((r.id, r.message), (6, Message::TemporaryFileNotInTempDir));
        assert!(t.k.pending().is_none());
        t.run("a=T,f=24,t=t,i=6,s=1,v=1;L3RtcC90dHktZ3JhcGhpY3MtcHJvdG9jb2wteA==");
        let at = Resolved { temp_dir_real: Some(b"/private/tmp"), ..at };
        let out = t.k.answer(Answer::Resolved(at), &mut t.s).unwrap();
        assert!(out.waits_on_host);
        let read = FileRead { offset: 0, most: crate::kitty::image::MAX_SIZE, unlink: Unlink::Yes };
        assert_eq!(
            t.k.pending(),
            Some(Request::Read { path: b"/private/tmp/tty-graphics-protocol-x", read })
        );
        // The wrong answer is a host bug, and the request stays.
        assert_eq!(t.k.answer(Answer::SharedMemory(b"x"), &mut t.s), Err(UnaskedAnswer));
        let out = t.k.answer(Answer::Read(&[1, 2, 3]), &mut t.s).unwrap();
        assert!(out.reply.unwrap().ok());
        assert!(out.cursor.is_some());
        assert_eq!(t.storage().placement_count(), 1);
        assert_eq!(t.k.answer(Answer::Read(&[]), &mut t.s), Err(UnaskedAnswer));
        // Shared memory, and a failed IO.
        t.run("a=t,f=24,t=s,i=7,s=1,v=1;L3NobQ==");
        assert_eq!(t.k.pending(), Some(Request::SharedMemory { name: b"/shm" }));
        assert!(
            t.k.answer(Answer::SharedMemory(&[1, 2, 3, 0, 0]), &mut t.s)
                .unwrap()
                .reply
                .unwrap()
                .ok()
        );
        t.run("a=t,f=24,t=f,i=8,s=1,v=1;L3NobQ==");
        let r = t.k.answer(Answer::Failed(IoFailed), &mut t.s).unwrap().reply.unwrap();
        assert_eq!((r.id, r.message), (8, Message::InvalidData));
        // A command while one waits drops it.
        t.run("a=t,f=24,t=f,i=8,s=1,v=1;L3NobQ==");
        t.run("a=d");
        assert!(t.k.pending().is_none());
        assert_eq!(t.k.stats().superseded, 1);
        // A PNG file with a decoder that refuses it.
        let mut t = T::with(GraphicsOpts {
            media: Media::ALL,
            png: Some(png_never),
            ..GraphicsOpts::DEFAULT
        });
        t.run("a=t,f=100,t=f,i=9;L3NobQ==");
        let at = Resolved { real: b"/shm", temp_dir: None, temp_dir_real: None };
        t.k.answer(Answer::Resolved(at), &mut t.s).unwrap();
        let r = t.k.answer(Answer::Read(b"\x89PNG"), &mut t.s).unwrap().reply.unwrap();
        assert_eq!(r.message, Message::InvalidData);
    }

    /// Inventory §9: a screen reset restores the defaults; erase display removes placements.
    #[test]
    fn reset_and_erase() {
        let mut t = T::with(GraphicsOpts { media: Media::ALL, ..GraphicsOpts::DEFAULT });
        t.run("a=T,f=24,i=1,s=1,v=1;AAAA");
        t.run("a=p,i=1,U=1");
        t.k.erase_display(ScreenKey::Primary, &mut t.s);
        assert_eq!((t.storage().placement_count(), t.storage().image_count()), (1, 1));
        t.k.reset_screen(ScreenKey::Primary, &mut t.s);
        assert_eq!((t.storage().placement_count(), t.storage().image_count()), (0, 0));
        assert_eq!(t.storage().media, Media::DIRECT);
        assert_eq!(t.s.tracked(), 0);
        // The other screen is its own.
        assert_eq!(t.k.storage(ScreenKey::Alternate).media, Media::ALL);
    }
}
