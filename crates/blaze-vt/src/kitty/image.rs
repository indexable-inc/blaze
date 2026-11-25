//! An image as it loads and once loaded (ghostty's graphics_image.zig).
//!
//! A direct transmission is its payload, in one command or chunks (`m=1`). The file, temporary
//! file and shared-memory media are IO, so they are host requests here (rule 14): the payload is
//! a path, [`super::Request`] asks the host to resolve or read it, and the answer comes back to
//! the loading image, which checks it as ghostty checks the file it opened. Completion inflates
//! zlib (blaze-deflate) and decodes PNG through the host's [`DecodePng`] function, then checks the
//! dimensions and the byte count.

use blaze_deflate::{Limits as InflateLimits, MAX_RATIO, unzlib_prefix};
use blaze_own::{AVec, Alloc};

use super::command::{Command, Compression, Display, Format, Medium, Message, Quiet};

/// The widest or tallest image, from kitty.
pub const MAX_DIMENSION: u32 = 10_000;

/// The most bytes an image may have, compressed, read or decoded, from kitty (400 MiB).
pub const MAX_SIZE: usize = 400 * 1024 * 1024;

/// Which media beyond direct a terminal accepts (ghostty's `LoadingImage.Limits`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Media {
    pub file: bool,
    pub temporary_file: bool,
    pub shared_memory: bool,
}

impl Media {
    /// Every medium.
    pub const ALL: Media = Media { file: true, temporary_file: true, shared_memory: true };
    /// Direct only, ghostty's default.
    pub const DIRECT: Media = Media { file: false, temporary_file: false, shared_memory: false };

    fn allows(self, medium: Medium) -> bool {
        match medium {
            Medium::Direct => true,
            Medium::File => self.file,
            Medium::TemporaryFile => self.temporary_file,
            Medium::SharedMemory => self.shared_memory,
        }
    }
}

/// Why an image did not load. Each is a reply message ([`ImageError::message`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ImageError {
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
}

impl ImageError {
    /// ghostty's `encodeError`.
    pub const fn message(self) -> Message {
        match self {
            ImageError::OutOfMemory => Message::OutOfMemory,
            ImageError::InvalidData => Message::InvalidData,
            ImageError::DecompressionFailed => Message::DecompressionFailed,
            ImageError::FilePathTooLong => Message::FilePathTooLong,
            ImageError::TemporaryFileNotInTempDir => Message::TemporaryFileNotInTempDir,
            ImageError::TemporaryFileNotNamedCorrectly => Message::TemporaryFileNotNamedCorrectly,
            ImageError::UnsupportedFormat => Message::UnsupportedFormat,
            ImageError::UnsupportedMedium => Message::UnsupportedMedium,
            ImageError::UnsupportedDepth => Message::UnsupportedDepth,
            ImageError::DimensionsRequired => Message::DimensionsRequired,
            ImageError::DimensionsTooLarge => Message::DimensionsTooLarge,
        }
    }
}

/// A stamp given each time an image is stored or a storage changes: unique and increasing.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct Generation(pub u64);

/// What a PNG decoder returns: RGBA pixels, four bytes each.
#[derive(Debug)]
pub struct DecodedPng<'a> {
    pub width: u32,
    pub height: u32,
    pub rgba: AVec<'a, u8>,
}

/// Why a PNG decoder refused (ghostty's `decode_png` errors).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PngError {
    InvalidData,
    OutOfMemory,
}

/// The host's PNG decoder (ghostty's `sys.decode_png`): pure, no IO. `None` in
/// [`super::Opts::png`] means PNG is unsupported, as a build without one.
pub type DecodePng = for<'a> fn(&[u8], Alloc<'a>) -> Result<DecodedPng<'a>, PngError>;

/// A loaded image: raw pixels, `width * height * bpp` bytes, never compressed, never PNG.
#[derive(Debug)]
pub struct Image<'a> {
    pub id: u32,
    pub number: u32,
    pub width: u32,
    pub height: u32,
    pub format: Format,
    pub compression: Compression,
    pub data: AVec<'a, u8>,
    /// Given when stored; [`Generation::default`] until then.
    pub generation: Generation,
    /// Loaded by a command with neither id nor number: it gets an id, but no reply.
    pub implicit_id: bool,
}

impl<'a> Image<'a> {
    /// An image with no pixels yet (ghostty's `Image{}` with these fields).
    pub fn empty_in(id: u32, width: u32, height: u32, alloc: Alloc<'a>) -> Image<'a> {
        Image {
            id,
            number: 0,
            width,
            height,
            format: Format::Rgb,
            compression: Compression::None,
            data: AVec::new_in(alloc),
            generation: Generation::default(),
            implicit_id: false,
        }
    }
}

/// What the host found resolving a file path (`realpath`), and the temporary directory it uses
/// (`TMPDIR` or its default, and that resolved), for the temporary-file checks.
#[derive(Clone, Copy, Debug)]
pub struct Resolved<'h> {
    pub real: &'h [u8],
    pub temp_dir: Option<&'h [u8]>,
    pub temp_dir_real: Option<&'h [u8]>,
}

/// The host could not do the IO asked (resolve, open, stat, seek, read, map): ghostty replies
/// `EINVAL: invalid data` for each.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct IoFailed;

/// Whether the host deletes the file after reading it (a temporary file that passed its checks;
/// it is deleted even if the read then fails).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Unlink {
    No,
    Yes,
}

/// The read the host does for a file medium: from `offset`, at most `most` bytes. ghostty's
/// reader refuses a read that reaches `most` (Zig 0.15 `appendRemaining` returns StreamTooLong
/// when its limit is used up), so a file of exactly `most` bytes after the offset is refused too.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FileRead {
    pub offset: u64,
    pub most: usize,
    pub unlink: Unlink,
}

/// The IO a non-direct medium needs before the image can complete.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NeedsIo {
    /// Resolve the path (file and temporary file), then [`LoadingImage::resolved`].
    Resolve,
    /// Open, stat, map and unlink the shared memory, then [`LoadingImage::shared_memory`].
    SharedMemory,
}

/// What [`LoadingImage::init`] gives: an image whose data is in hand, or the IO it waits on.
#[derive(Debug)]
pub enum Init<'a> {
    Ready(LoadingImage<'a>),
    Waits(LoadingImage<'a>, NeedsIo),
}

/// An image being loaded: from its first command, gathering chunks until complete.
#[derive(Debug)]
pub struct LoadingImage<'a> {
    pub image: Image<'a>,
    /// The display of a transmit-and-display, run once the image is stored.
    pub display: Option<Display>,
    /// The quiet of the first chunk, used by later chunks that set none.
    pub quiet: Quiet,
    medium: Medium,
    offset: u32,
    size: u32,
}

impl<'a> LoadingImage<'a> {
    /// The image of a transmission's first command (`LoadingImage.init`). A direct payload is
    /// added; another medium is checked against `media` and the decoder, and waits on IO.
    pub fn init(
        cmd: &Command<'_>,
        media: Media,
        png: Option<DecodePng>,
        alloc: Alloc<'a>,
    ) -> Result<Init<'a>, ImageError> {
        let Some(t) = cmd.transmission() else {
            return Err(ImageError::InvalidData);
        };
        let mut loading = LoadingImage {
            image: Image {
                id: t.image_id,
                number: t.image_number,
                width: t.width,
                height: t.height,
                format: t.format,
                compression: t.compression,
                data: AVec::new_in(alloc),
                generation: Generation::default(),
                implicit_id: false,
            },
            display: cmd.display(),
            quiet: cmd.quiet,
            medium: t.medium,
            offset: t.offset,
            size: t.size,
        };
        if t.medium == Medium::Direct {
            loading.add_data(&cmd.data)?;
            return Ok(Init::Ready(loading));
        }
        // A PNG with no decoder would be read only to be refused.
        if t.format == Format::Png && png.is_none() {
            return Err(ImageError::UnsupportedMedium);
        }
        if !media.allows(t.medium) {
            return Err(ImageError::UnsupportedMedium);
        }
        // realpath asserts on an interior NUL, so ghostty refuses it first.
        if cmd.data.scan_for(&0).is_some() {
            return Err(ImageError::InvalidData);
        }
        let io = match t.medium {
            Medium::SharedMemory => NeedsIo::SharedMemory,
            _ => NeedsIo::Resolve,
        };
        Ok(Init::Waits(loading, io))
    }

    /// The medium of the first command.
    pub fn medium(&self) -> Medium {
        self.medium
    }

    /// The checks `readFile` makes on the resolved path before opening it, and the read to do.
    pub fn resolved(&self, at: Resolved<'_>) -> Result<FileRead, ImageError> {
        let path = at.real;
        if path.starts_with(b"/proc/")
            || path.starts_with(b"/sys/")
            || (path.starts_with(b"/dev/") && !path.starts_with(b"/dev/shm/"))
        {
            return Err(ImageError::InvalidData);
        }
        let unlink = match self.medium {
            Medium::TemporaryFile => {
                if !in_temp_dir(at) {
                    return Err(ImageError::TemporaryFileNotInTempDir);
                }
                if !contains(path, b"tty-graphics-protocol") {
                    return Err(ImageError::TemporaryFileNotNamedCorrectly);
                }
                Unlink::Yes
            }
            _ => Unlink::No,
        };
        let most = match self.size {
            0 => MAX_SIZE,
            s => (s as usize).min(MAX_SIZE),
        };
        Ok(FileRead { offset: u64::from(self.offset), most, unlink })
    }

    /// The bytes the host read for [`FileRead`]; a read that reached `most` is refused.
    pub fn file_read(&mut self, read: FileRead, bytes: &[u8]) -> Result<(), ImageError> {
        if bytes.len() >= read.most {
            return Err(ImageError::InvalidData);
        }
        self.image.data.clear();
        self.image.data.try_extend_from_slice(bytes).map_err(|_| ImageError::OutOfMemory)
    }

    /// The whole shared-memory segment the host mapped (its `fstat` size, page padding included).
    pub fn shared_memory(&mut self, segment: &[u8]) -> Result<(), ImageError> {
        let stat_size = segment.len();
        if stat_size == 0 {
            return Err(ImageError::InvalidData);
        }
        let expected = match self.image.format.bpp() {
            None => stat_size,
            Some(bpp) => {
                (u64::from(self.image.width) * u64::from(self.image.height) * u64::from(bpp))
                    .try_into()
                    .unwrap_or(usize::MAX)
            }
        };
        if stat_size < expected {
            return Err(ImageError::InvalidData);
        }
        let start = self.offset as usize;
        let end = match self.size {
            0 => expected,
            s => start.saturating_add(s as usize).min(expected),
        };
        // ghostty slices `map[start..end]` unchecked; an offset past the end is refused here.
        let Some(bytes) = segment.get(start..end) else {
            return Err(ImageError::InvalidData);
        };
        self.image.data.clear();
        self.image.data.try_extend_from_slice(bytes).map_err(|_| ImageError::OutOfMemory)
    }

    /// A chunk (`addData`): refused past [`MAX_SIZE`].
    pub fn add_data(&mut self, data: &[u8]) -> Result<(), ImageError> {
        if data.is_empty() {
            return Ok(());
        }
        if self.image.data.len() + data.len() > MAX_SIZE {
            return Err(ImageError::InvalidData);
        }
        self.image.data.try_extend_from_slice(data).map_err(|_| ImageError::OutOfMemory)
    }

    /// The finished image (`complete`): inflated, decoded, and its size checked.
    pub fn complete(self, png: Option<DecodePng>) -> Result<Image<'a>, ImageError> {
        let mut img = self.image;
        if img.compression == Compression::ZlibDeflate {
            let alloc = *img.data.allocator();
            let mut out = AVec::new_in(alloc);
            // Zig's reader refuses output that reaches its limit, so the most is one less.
            let limits = InflateLimits {
                most_out: MAX_SIZE as u64 - 1,
                most_ratio: MAX_RATIO,
                most_header: 0,
            };
            unzlib_prefix(&img.data, limits, alloc, &mut out)
                .map_err(|_| ImageError::DecompressionFailed)?;
            img.data = out;
            img.compression = Compression::None;
        }
        if img.format == Format::Png {
            let decode = png.ok_or(ImageError::UnsupportedFormat)?;
            let alloc = *img.data.allocator();
            let decoded = decode(&img.data, alloc).map_err(|e| match e {
                PngError::InvalidData => ImageError::InvalidData,
                PngError::OutOfMemory => ImageError::OutOfMemory,
            })?;
            if decoded.rgba.len() > MAX_SIZE {
                return Err(ImageError::InvalidData);
            }
            img.data = decoded.rgba;
            img.width = decoded.width;
            img.height = decoded.height;
            img.format = Format::Rgba;
        }
        if img.width == 0 || img.height == 0 {
            return Err(ImageError::DimensionsRequired);
        }
        if img.width > MAX_DIMENSION || img.height > MAX_DIMENSION {
            return Err(ImageError::DimensionsTooLarge);
        }
        let bpp = img.format.bpp().ok_or(ImageError::UnsupportedFormat)?;
        let expected = u64::from(img.width) * u64::from(img.height) * u64::from(bpp);
        if img.data.len() as u64 != expected {
            return Err(ImageError::InvalidData);
        }
        Ok(img)
    }
}

/// `isPathInTempDir`: under `/tmp`, `/dev/shm`, the temporary directory or its resolved form.
fn in_temp_dir(at: Resolved<'_>) -> bool {
    let path = at.real;
    path.starts_with(b"/tmp")
        || path.starts_with(b"/dev/shm")
        || at.temp_dir.is_some_and(|d| path.starts_with(d))
        || at.temp_dir_real.is_some_and(|d| path.starts_with(d))
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kitty::command::{MaxBytes, Parser};

    fn alloc() -> Alloc<'static> {
        blaze_own::testing::heap().alloc()
    }

    fn command(input: &[u8]) -> Command<'static> {
        let mut p = Parser::new_in(MaxBytes(1024 * 1024 * 64), alloc());
        p.feed_slice(input).unwrap();
        p.complete().unwrap()
    }

    fn ready(i: Result<Init<'static>, ImageError>) -> LoadingImage<'static> {
        match i.unwrap() {
            Init::Ready(l) => l,
            Init::Waits(_, io) => panic!("waits on {io:?}"),
        }
    }

    fn waits(i: Result<Init<'static>, ImageError>) -> (LoadingImage<'static>, NeedsIo) {
        match i.unwrap() {
            Init::Waits(l, io) => (l, io),
            Init::Ready(_) => panic!("ready"),
        }
    }

    fn base64(data: &[u8]) -> std::vec::Vec<u8> {
        let mut out = std::vec![0u8; blaze_base64::STANDARD.encoded_len(data.len()).unwrap()];
        let n = blaze_base64::STANDARD.encode_into(data, &mut out).unwrap();
        out.truncate(n);
        out
    }

    fn with_payload(keys: &[u8], data: &[u8]) -> Command<'static> {
        let mut input = keys.to_vec();
        input.push(b';');
        input.extend_from_slice(&base64(data));
        command(&input)
    }

    const RGB_RAW: &[u8] = include_bytes!("testdata/image-rgb-none-20x15-2147483647-raw.data");
    const RGB_ZLIB: &[u8] =
        include_bytes!("testdata/image-rgb-zlib_deflate-128x96-2147483647-raw.data");
    const PNG_RAW: &[u8] = include_bytes!("testdata/image-png-none-50x76-2147483647-raw.data");

    // ghostty graphics_image.zig:565 "image load with invalid RGB data"
    #[test]
    fn image_load_with_invalid_rgb_data() {
        // Four raw bytes for one RGB pixel.
        let cmd = with_payload(b"a=t,f=24,t=d,s=1,v=1,i=31", b"AAAA");
        let loading = ready(LoadingImage::init(&cmd, Media::DIRECT, None, alloc()));
        assert_eq!(loading.complete(None).unwrap_err(), ImageError::InvalidData);
    }

    // ghostty graphics_image.zig:584 "image load with image too wide"
    #[test]
    fn image_load_with_image_too_wide() {
        let cmd = with_payload(b"a=t,f=24,t=d,s=10001,v=1,i=31", b"AAAA");
        let loading = ready(LoadingImage::init(&cmd, Media::DIRECT, None, alloc()));
        assert_eq!(loading.complete(None).unwrap_err(), ImageError::DimensionsTooLarge);
    }

    // ghostty graphics_image.zig:603 "image load with image too tall"
    #[test]
    fn image_load_with_image_too_tall() {
        let cmd = with_payload(b"a=t,f=24,t=d,s=1,v=10001,i=31", b"AAAA");
        let loading = ready(LoadingImage::init(&cmd, Media::DIRECT, None, alloc()));
        assert_eq!(loading.complete(None).unwrap_err(), ImageError::DimensionsTooLarge);
    }

    // ghostty graphics_image.zig:622 "image load: rgb, zlib compressed, direct"
    #[test]
    fn image_load_rgb_zlib_compressed_direct() {
        let cmd = with_payload(b"a=t,f=24,t=d,o=z,s=128,v=96,i=31", RGB_ZLIB);
        let loading = ready(LoadingImage::init(&cmd, Media::DIRECT, None, alloc()));
        let img = loading.complete(None).unwrap();
        assert_eq!(img.compression, Compression::None);
        assert_eq!(img.data.len(), 128 * 96 * 3);
    }

    // ghostty graphics_image.zig:650 "image load: rgb, not compressed, direct"
    #[test]
    fn image_load_rgb_not_compressed_direct() {
        let cmd = with_payload(b"a=t,f=24,t=d,s=20,v=15,i=31", RGB_RAW);
        let loading = ready(LoadingImage::init(&cmd, Media::DIRECT, None, alloc()));
        let img = loading.complete(None).unwrap();
        assert_eq!(img.compression, Compression::None);
        assert_eq!(img.data.as_slice(), RGB_RAW);
    }

    // ghostty graphics_image.zig:678 "image load: rgb, zlib compressed, direct, chunked"
    #[test]
    fn image_load_rgb_zlib_compressed_direct_chunked() {
        let cmd = with_payload(b"a=t,f=24,t=d,o=z,s=128,v=96,i=31,m=1", &RGB_ZLIB[..1024]);
        let mut loading = ready(LoadingImage::init(&cmd, Media::DIRECT, None, alloc()));
        for chunk in RGB_ZLIB[1024..].chunks(1024) {
            loading.add_data(chunk).unwrap();
        }
        let img = loading.complete(None).unwrap();
        assert_eq!(img.compression, Compression::None);
    }

    // ghostty graphics_image.zig:715 "image load: rgb, zlib compressed, direct, chunked with zero initial chunk"
    #[test]
    fn image_load_rgb_zlib_compressed_direct_chunked_with_zero_initial_chunk() {
        let cmd = command(b"a=t,f=24,t=d,o=z,s=128,v=96,i=31,m=1");
        let mut loading = ready(LoadingImage::init(&cmd, Media::DIRECT, None, alloc()));
        for chunk in RGB_ZLIB.chunks(1024) {
            loading.add_data(chunk).unwrap();
        }
        let img = loading.complete(None).unwrap();
        assert_eq!(img.compression, Compression::None);
    }

    fn temp_path_cmd(medium: u8, path: &[u8]) -> Command<'static> {
        let mut keys = b"a=t,f=24,s=20,v=15,i=31,t=".to_vec();
        keys.push(medium);
        with_payload(&keys, path)
    }

    // ghostty graphics_image.zig:751 "image load: temporary file without correct path"
    #[test]
    fn image_load_temporary_file_without_correct_path() {
        let cmd = temp_path_cmd(b't', b"/tmp/blaze-1/image.data");
        let (loading, io) = waits(LoadingImage::init(&cmd, Media::ALL, None, alloc()));
        assert_eq!(io, NeedsIo::Resolve);
        let at = Resolved { real: b"/tmp/blaze-1/image.data", temp_dir: None, temp_dir_real: None };
        // Refused before any read, so nothing deletes the file (ghostty's test checks it remains).
        assert_eq!(loading.resolved(at), Err(ImageError::TemporaryFileNotNamedCorrectly));
    }

    // ghostty graphics_image.zig:784 "image load: rgb, not compressed, temporary file"
    #[test]
    fn image_load_rgb_not_compressed_temporary_file() {
        let path = b"/var/folders/x/T/blaze/tty-graphics-protocol-image.data";
        let cmd = temp_path_cmd(b't', path);
        let (mut loading, _) = waits(LoadingImage::init(&cmd, Media::ALL, None, alloc()));
        let at = Resolved { real: path, temp_dir: Some(b"/var/folders/x/T/"), temp_dir_real: None };
        let read = loading.resolved(at).unwrap();
        assert_eq!(read, FileRead { offset: 0, most: MAX_SIZE, unlink: Unlink::Yes });
        loading.file_read(read, RGB_RAW).unwrap();
        let img = loading.complete(None).unwrap();
        assert_eq!(img.data.as_slice(), RGB_RAW);
    }

    // ghostty graphics_image.zig:821 "image load: rgb, not compressed, regular file"
    #[test]
    fn image_load_rgb_not_compressed_regular_file() {
        let path = b"/home/u/image.data";
        let cmd = temp_path_cmd(b'f', path);
        let (mut loading, _) = waits(LoadingImage::init(&cmd, Media::ALL, None, alloc()));
        let read =
            loading.resolved(Resolved { real: path, temp_dir: None, temp_dir_real: None }).unwrap();
        assert_eq!(read.unlink, Unlink::No);
        loading.file_read(read, RGB_RAW).unwrap();
        assert_eq!(loading.complete(None).unwrap().data.as_slice(), RGB_RAW);
    }

    fn fake_png<'a>(data: &[u8], alloc: Alloc<'a>) -> Result<DecodedPng<'a>, PngError> {
        // A stand-in decoder: the fixture is 50x76; any bytes become that many RGBA pixels.
        if !data.starts_with(b"\x89PNG") {
            return Err(PngError::InvalidData);
        }
        let mut rgba = AVec::new_in(alloc);
        rgba.try_extend_from_slice(&std::vec![7u8; 50 * 76 * 4])
            .map_err(|_| PngError::OutOfMemory)?;
        Ok(DecodedPng { width: 50, height: 76, rgba })
    }

    // ghostty graphics_image.zig:856 "image load: png, not compressed, regular file"
    #[test]
    fn image_load_png_not_compressed_regular_file() {
        let path = b"/home/u/image.png";
        let cmd = with_payload(b"a=t,f=100,t=f,i=31", path);
        // Without a decoder a PNG file is refused before it is read.
        assert_eq!(
            LoadingImage::init(&cmd, Media::ALL, None, alloc()).unwrap_err(),
            ImageError::UnsupportedMedium
        );
        let (mut loading, _) = waits(LoadingImage::init(&cmd, Media::ALL, Some(fake_png), alloc()));
        let read =
            loading.resolved(Resolved { real: path, temp_dir: None, temp_dir_real: None }).unwrap();
        loading.file_read(read, PNG_RAW).unwrap();
        let img = loading.complete(Some(fake_png)).unwrap();
        assert_eq!((img.width, img.height, img.format), (50, 76, Format::Rgba));
    }

    #[test]
    fn png_direct_without_decoder_is_unsupported_format() {
        let cmd = with_payload(b"a=t,f=100,i=31", PNG_RAW);
        let loading = ready(LoadingImage::init(&cmd, Media::DIRECT, None, alloc()));
        assert_eq!(loading.complete(None).unwrap_err(), ImageError::UnsupportedFormat);
    }

    // ghostty graphics_image.zig:894 "limits: direct medium always allowed"
    #[test]
    fn limits_direct_medium_always_allowed() {
        let cmd = with_payload(b"a=t,f=24,t=d,s=20,v=15,i=31", RGB_RAW);
        let loading = ready(LoadingImage::init(&cmd, Media::DIRECT, None, alloc()));
        loading.complete(None).unwrap();
    }

    // ghostty graphics_image.zig:915 "limits: file medium blocked by limits"
    #[test]
    fn limits_file_medium_blocked_by_limits() {
        let cmd = temp_path_cmd(b'f', b"/home/u/image.data");
        assert_eq!(
            LoadingImage::init(&cmd, Media::DIRECT, None, alloc()).unwrap_err(),
            ImageError::UnsupportedMedium
        );
    }

    // ghostty graphics_image.zig:945 "limits: file medium allowed by limits"
    #[test]
    fn limits_file_medium_allowed_by_limits() {
        let cmd = temp_path_cmd(b'f', b"/home/u/image.data");
        let media = Media { file: true, temporary_file: false, shared_memory: false };
        let (_, io) = waits(LoadingImage::init(&cmd, media, None, alloc()));
        assert_eq!(io, NeedsIo::Resolve);
    }

    // ghostty graphics_image.zig:980 "limits: temporary file medium blocked by limits"
    #[test]
    fn limits_temporary_file_medium_blocked_by_limits() {
        let cmd = temp_path_cmd(b't', b"/tmp/tty-graphics-protocol-x");
        let media = Media { file: true, temporary_file: false, shared_memory: true };
        assert_eq!(
            LoadingImage::init(&cmd, media, None, alloc()).unwrap_err(),
            ImageError::UnsupportedMedium
        );
    }

    // ghostty graphics_image.zig:1017 "limits: temporary file medium allowed by limits"
    #[test]
    fn limits_temporary_file_medium_allowed_by_limits() {
        let cmd = temp_path_cmd(b't', b"/tmp/tty-graphics-protocol-x");
        let media = Media { file: false, temporary_file: true, shared_memory: false };
        let (_, io) = waits(LoadingImage::init(&cmd, media, None, alloc()));
        assert_eq!(io, NeedsIo::Resolve);
    }

    #[test]
    fn file_checks_refuse_special_paths_and_full_reads() {
        let cmd = temp_path_cmd(b'f', b"x");
        let (mut loading, _) = waits(LoadingImage::init(&cmd, Media::ALL, None, alloc()));
        for p in [&b"/proc/self/mem"[..], b"/sys/x", b"/dev/zero"] {
            let at = Resolved { real: p, temp_dir: None, temp_dir_real: None };
            assert_eq!(loading.resolved(at), Err(ImageError::InvalidData));
        }
        let at = Resolved { real: b"/dev/shm/x", temp_dir: None, temp_dir_real: None };
        let read = loading.resolved(at).unwrap();
        // A read that fills the limit is refused (the Zig 0.15 reader's StreamTooLong).
        let short = FileRead { most: 4, ..read };
        assert_eq!(loading.file_read(short, b"abcd"), Err(ImageError::InvalidData));
        loading.file_read(short, b"abc").unwrap();
        // A temporary file outside the temporary directories.
        let cmd = temp_path_cmd(b't', b"x");
        let (loading, _) = waits(LoadingImage::init(&cmd, Media::ALL, None, alloc()));
        let at = Resolved {
            real: b"/home/tty-graphics-protocol",
            temp_dir: Some(b"/var/t"),
            temp_dir_real: Some(b"/private/var/t"),
        };
        assert_eq!(loading.resolved(at), Err(ImageError::TemporaryFileNotInTempDir));
        let at = Resolved { real: b"/private/var/t/tty-graphics-protocol", ..at };
        assert_eq!(loading.resolved(at).unwrap().unlink, Unlink::Yes);
        // A NUL in the path is refused before any IO.
        let cmd = temp_path_cmd(b'f', b"/a\0b");
        assert_eq!(
            LoadingImage::init(&cmd, Media::ALL, None, alloc()).unwrap_err(),
            ImageError::InvalidData
        );
    }

    #[test]
    fn shared_memory_checks() {
        let cmd = with_payload(b"a=t,f=24,t=s,s=20,v=15,i=31", b"/shm-name");
        let (mut loading, io) = waits(LoadingImage::init(&cmd, Media::ALL, None, alloc()));
        assert_eq!(io, NeedsIo::SharedMemory);
        assert_eq!(loading.shared_memory(b""), Err(ImageError::InvalidData));
        assert_eq!(loading.shared_memory(&RGB_RAW[..899]), Err(ImageError::InvalidData));
        // A segment padded to its page size is cut to the expected size.
        let mut page = RGB_RAW.to_vec();
        page.resize(4096, 0xFF);
        loading.shared_memory(&page).unwrap();
        assert_eq!(loading.complete(None).unwrap().data.as_slice(), RGB_RAW);
        // An offset past the end is refused.
        let cmd = with_payload(b"a=t,f=24,t=s,s=20,v=15,i=31,O=1000,S=5", b"/shm-name");
        let (mut loading, _) = waits(LoadingImage::init(&cmd, Media::ALL, None, alloc()));
        assert_eq!(loading.shared_memory(&page), Err(ImageError::InvalidData));
    }

    #[test]
    fn decompression_failure_and_dimensions_required() {
        let cmd = with_payload(b"a=t,f=24,o=z,s=1,v=1,i=31", b"not zlib");
        let loading = ready(LoadingImage::init(&cmd, Media::DIRECT, None, alloc()));
        assert_eq!(loading.complete(None).unwrap_err(), ImageError::DecompressionFailed);
        let cmd = with_payload(b"a=t,f=24,i=31", b"abc");
        let loading = ready(LoadingImage::init(&cmd, Media::DIRECT, None, alloc()));
        assert_eq!(loading.complete(None).unwrap_err(), ImageError::DimensionsRequired);
    }
}
