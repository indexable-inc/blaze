//! Compression. Cold values: zstd with a dictionary per kind. Values of one kind (the debug
//! info of compiled functions, the outputs of build-script runs) share most of their structure,
//! which a dictionary trained on them takes out of each. Long cold values instead get zstd's
//! long-distance matching over a wide window, on several threads. Chunks of hot outputs: LZ4,
//! which decompresses at memory speed.
//!
//! A value put or read is compressed or decompressed in its thread's zstd contexts, made once:
//! a new context allocates and clears its tables at its first frame, which costs more than a
//! small value's compression.

use std::cell::RefCell;

use zstd::{
    dict::{DecoderDictionary, EncoderDictionary},
    stream::raw::{CParameter, DParameter},
    zstd_safe::{self, CCtx, CDict, DCtx, ResetDirective},
};

/// One level for every cold value: compression is deterministic for a level and a dictionary.
const LEVEL: i32 = 6;

/// From this length on a cold value is compressed by [`compress_long`]: a dictionary gains a
/// long value little, a 128 MiB window and threads gain it much.
#[cfg(not(test))]
pub(crate) const LONG_FROM: usize = 8 << 20;
#[cfg(test)]
pub(crate) const LONG_FROM: usize = 64 << 10;

/// zstd's default limit for decompression, so any zstd reads these frames.
const WINDOW_LOG: u32 = 27;

/// zstd's threaded output depends on its parameters but not on the machine or the number of
/// cores, as long as the worker count is fixed and not zero: so it is fixed here.
const WORKERS: u32 = 4;

#[cfg(not(test))]
pub(crate) const DICTIONARY_SIZE: usize = 64 << 10;
#[cfg(test)]
pub(crate) const DICTIONARY_SIZE: usize = 4 << 10;

/// A kind's dictionary is trained once this many of its values, and [`TRAIN_BYTES`] of them,
/// are kept raw; its values from then on are compressed.
#[cfg(not(test))]
pub(crate) const TRAIN_VALUES: usize = 256;
#[cfg(test)]
pub(crate) const TRAIN_VALUES: usize = 64;

#[cfg(not(test))]
pub(crate) const TRAIN_BYTES: u64 = 2 << 20;
#[cfg(test)]
pub(crate) const TRAIN_BYTES: u64 = 48 << 10;

/// At most this many samples, and bytes of them, go into one training.
pub(crate) const SAMPLES_MOST: usize = 4096;
pub(crate) const SAMPLE_BYTES_MOST: u64 = 32 << 20;

pub(crate) fn train(samples: &[&[u8]]) -> Result<Vec<u8>, String> {
    zstd::dict::from_samples(samples, DICTIONARY_SIZE)
        // alloc: the error's message
        .map_err(|error| {
            format!("training a dictionary on {} values failed: {error}", samples.len())
        })
}

/// A kind's dictionary, digested once for both directions. Loading 64 KiB of dictionary into a
/// context costs more than compressing or decompressing a small value, so each put and read
/// references this instead of loading the bytes.
pub(crate) struct Prepared {
    bytes: Vec<u8>,
    encoder: EncoderDictionary<'static>,
    decoder: DecoderDictionary<'static>,
}

impl Prepared {
    /// `bytes` come from a pack, which nothing but their content hash has checked: zstd may
    /// refuse them (a dictionary's magic number before a corrupt header).
    pub(crate) fn new(bytes: Vec<u8>) -> Result<Prepared, String> {
        let encoder = EncoderDictionary::try_copy(&bytes, LEVEL)
            // alloc: the error's message
            .map_err(|error| format!("does not load for compressing: {error}"))?;
        let decoder = DecoderDictionary::try_copy(&bytes)
            // alloc: the error's message
            .map_err(|error| format!("does not load for decompressing: {error}"))?;
        Ok(Prepared { bytes, encoder, decoder })
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// A thread's zstd contexts.
struct Contexts {
    /// Between values it is as new: no dictionary, every parameter its default.
    compress: CCtx<'static>,
    /// Reads at most a [`WINDOW_LOG`] window; never given a dictionary to keep.
    decompress: DCtx<'static>,
}

thread_local! {
    static CONTEXTS: RefCell<Option<Contexts>> = const { RefCell::new(None) };
}

fn failed(what: &str, code: usize) -> String {
    // alloc: the error's message
    format!("{what}: {}", zstd_safe::get_error_name(code))
}

/// `f` with the thread's contexts, made at its first call.
fn with_contexts<R>(f: impl FnOnce(&mut Contexts) -> Result<R, String>) -> Result<R, String> {
    CONTEXTS.with(|cell| {
        let mut cell = cell.borrow_mut();
        if cell.is_none() {
            let compress = CCtx::try_create().ok_or("starting zstd failed: it made no context")?;
            let mut decompress =
                DCtx::try_create().ok_or("starting zstd failed: it made no context")?;
            decompress
                .set_parameter(DParameter::WindowLogMax(WINDOW_LOG))
                .map_err(|code| failed("starting zstd failed", code))?;
            *cell = Some(Contexts { compress, decompress });
        }
        f(cell.as_mut().expect("made above"))
    })
}

/// `raw` in zstd with `dictionary`: the bytes a fresh context makes ([`Batch::compress`]).
pub(crate) fn compress(dictionary: &Prepared, raw: &[u8]) -> Result<Vec<u8>, String> {
    with_contexts(|contexts| {
        let context = &mut contexts.compress;
        // alloc: the compressed value, which the record keeps
        let mut out = Vec::with_capacity(zstd_safe::compress_bound(raw.len()));
        let cdict: *const CDict<'static> = dictionary.encoder.as_cdict();
        // SAFETY: `ref_cdict` asks the dictionary to outlive the thread's `'static` context. The
        // context reads it only in this `compress2`, and the `reset` below, which runs whether or
        // not either failed, drops the context's pointer to it before `dictionary` is released.
        let cdict = unsafe { &*cdict };
        let done = context.ref_cdict(cdict).and_then(|_| context.compress2(&mut out, raw));
        // The context outlives `dictionary`: it must not keep pointing at it, and the next value
        // must start from a context as new.
        let reset = context.reset(ResetDirective::SessionAndParameters);
        // alloc: the error's message
        done.map_err(|code| failed(&format!("compressing {} bytes failed", raw.len()), code))?;
        reset.map_err(|code| failed("resetting zstd failed", code))?;
        Ok(out)
    })
}

/// One compressor for a batch of values of one kind. Without a dictionary its frames are read
/// by [`decompress_long`].
pub(crate) struct Batch<'d>(zstd::bulk::Compressor<'d>);

impl<'d> Batch<'d> {
    pub(crate) fn new(dictionary: Option<&'d Prepared>) -> Result<Batch<'d>, String> {
        match dictionary {
            Some(dictionary) => {
                zstd::bulk::Compressor::with_prepared_dictionary(&dictionary.encoder)
            }
            None => zstd::bulk::Compressor::new(LEVEL),
        }
        .map(Batch)
        // alloc: the error's message
        .map_err(|error| format!("starting zstd failed: {error}"))
    }

    pub(crate) fn compress(&mut self, raw: &[u8]) -> Result<Vec<u8>, String> {
        // alloc: the error's message
        self.0
            .compress(raw)
            .map_err(|error| format!("compressing {} bytes failed: {error}", raw.len()))
    }
}

pub(crate) fn compress_long(raw: &[u8]) -> Result<Vec<u8>, String> {
    let mut compressor = zstd::bulk::Compressor::new(LEVEL)
        // alloc: the error's message
        .map_err(|error| format!("starting zstd failed: {error}"))?;
    for parameter in [
        CParameter::EnableLongDistanceMatching(true),
        CParameter::WindowLog(WINDOW_LOG),
        CParameter::NbWorkers(WORKERS),
    ] {
        compressor
            .set_parameter(parameter)
            // alloc: the error's message
            .map_err(|error| format!("zstd refused {parameter:?}: {error}"))?;
    }
    // alloc: the error's message
    compressor
        .compress(raw)
        .map_err(|error| format!("compressing {} bytes failed: {error}", raw.len()))
}

/// The length a zstd frame of `stored` bytes may decode to, checked before anything of that
/// length is allocated: `raw` comes from a record header, which nothing has verified yet.
/// Every frame here is one `ZSTD_compress2` of a whole value, which writes its length.
///
/// Only a standard zstd frame (magic `28 B5 2F FD`, RFC 8878 3.1.1) is read: the store never
/// wrote another, and a legacy (v0.1 to v0.7) or skippable frame in a pack, which nothing has
/// verified yet, never reaches zstd's decoders.
fn zstd_length(stored: &[u8], raw: u64) -> Result<usize, String> {
    match stored.first_chunk::<4>() {
        Some(magic) if u32::from_le_bytes(*magic) == zstd_safe::zstd_sys::ZSTD_MAGICNUMBER => {}
        Some(magic) => {
            // alloc: the error's message
            return Err(format!(
                "it starts with {magic:02x?}, not zstd's frame magic [28, b5, 2f, fd]"
            ));
        }
        // alloc: the error's message
        None => return Err(format!("its {} bytes are too short for a zstd frame", stored.len())),
    }
    match zstd::zstd_safe::get_frame_content_size(stored) {
        Ok(Some(said)) if said == raw => {}
        // alloc: the error's message
        Ok(Some(said)) => {
            return Err(format!("its frame holds {said} bytes, not the {raw} recorded"));
        }
        Ok(None) => return Err("its frame does not say how long it is".into()),
        Err(_) => return Err("it is not a zstd frame".into()),
    }
    // Each block has a 3-byte header and decodes to at most 128 KiB (RFC 8878 3.1.1.2).
    let most = (stored.len() as u64 / 3).saturating_mul(128 << 10);
    if raw > most {
        // alloc: the error's message
        return Err(format!("{} bytes of zstd cannot decode to the {raw} recorded", stored.len()));
    }
    // alloc: the error's message
    usize::try_from(raw).map_err(|_| "its length does not fit in memory".to_owned())
}

/// Exactly `raw` bytes, or why not.
pub(crate) fn decompress(
    dictionary: &Prepared,
    stored: &[u8],
    raw: u64,
) -> Result<Vec<u8>, String> {
    let raw = zstd_length(stored, raw)?;
    with_contexts(|contexts| {
        // alloc: the value made whole, kept in `Owned` and read in place after
        let mut bytes = Vec::with_capacity(raw);
        contexts
            .decompress
            .decompress_using_ddict(&mut bytes, stored, dictionary.decoder.as_ddict())
            .map_err(|code| failed("it does not decompress", code))?;
        whole(bytes, raw)
    })
}

/// What [`compress_long`] made: exactly `raw` bytes, or why not.
pub(crate) fn decompress_long(stored: &[u8], raw: u64) -> Result<Vec<u8>, String> {
    let raw = zstd_length(stored, raw)?;
    with_contexts(|contexts| {
        // alloc: the value made whole, kept in `Owned` and read in place after
        let mut bytes = Vec::with_capacity(raw);
        contexts
            .decompress
            .decompress(&mut bytes, stored)
            .map_err(|code| failed("it does not decompress", code))?;
        whole(bytes, raw)
    })
}

fn whole(bytes: Vec<u8>, raw: usize) -> Result<Vec<u8>, String> {
    if bytes.len() != raw {
        // alloc: the error's message
        return Err(format!("it decompresses to {} bytes, not the {raw} recorded", bytes.len()));
    }
    Ok(bytes)
}

/// A chunk in LZ4, if that saves at least an eighth of it.
pub(crate) fn lz4(raw: &[u8]) -> Option<Vec<u8>> {
    let stored = lz4_flex::block::compress(raw);
    (stored.len() + raw.len() / 8 <= raw.len()).then_some(stored)
}

/// [`lz4`] into `out`, a buffer the caller reuses: the same bytes, in `out` when it says so.
pub(crate) fn lz4_into(raw: &[u8], out: &mut Vec<u8>) -> bool {
    out.clear();
    out.resize(lz4_flex::block::get_maximum_output_size(raw.len()), 0);
    let len =
        lz4_flex::block::compress_into(raw, out).expect("the most LZ4 can grow a block by fits");
    out.truncate(len);
    len + raw.len() / 8 <= raw.len()
}

pub(crate) fn unlz4(stored: &[u8], raw: u64) -> Result<Vec<u8>, String> {
    // A block's densest byte, a match length's 255, is what an LZ4 block can grow by per byte;
    // `raw` is unverified, and it is allocated before a byte is decoded.
    if raw > (stored.len() as u64).saturating_mul(255).saturating_add(16) {
        // alloc: the error's message
        return Err(format!("{} bytes of LZ4 cannot decode to the {raw} recorded", stored.len()));
    }
    // alloc: the error's message
    let raw = usize::try_from(raw).map_err(|_| "its length does not fit in memory".to_owned())?;
    let bytes = lz4_flex::block::decompress(stored, raw)
        // alloc: the error's message
        .map_err(|error| format!("it does not decompress: {error}"))?;
    whole(bytes, raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(n: usize) -> Vec<u8> {
        format!(
            "{{\"unit\": \"crate-{n}\", \"cfg\": [\"feature=\\\"std\\\"\", \"target_os=\\\"linux\\\"\"], \
             \"env\": {{\"OUT_DIR\": \"/build/out/{n}\", \"PROFILE\": \"debug\"}}, \"links\": \
             [\"z\", \"ssl\", \"crypto\"], \"rerun\": [\"build.rs\", \"src/{n}.c\"], \"n\": {}}}\n",
            n * 7919 % 1000
        )
        .repeat(4)
        .into_bytes()
    }

    #[test]
    fn a_dictionary_trained_on_a_kind_shrinks_its_values_and_round_trips() {
        let samples: Vec<Vec<u8>> = (0..200).map(sample).collect();
        let refs: Vec<&[u8]> = samples.iter().map(Vec::as_slice).collect();
        let dictionary = Prepared::new(train(&refs).unwrap()).unwrap();
        assert!(dictionary.bytes().len() <= DICTIONARY_SIZE);
        let loaded = zstd::bulk::Decompressor::with_dictionary(dictionary.bytes());
        let value = sample(12_345);
        let stored = compress(&dictionary, &value).unwrap();
        assert!(stored.len() * 4 < value.len(), "{} of {}", stored.len(), value.len());
        assert_eq!(decompress(&dictionary, &stored, value.len() as u64).unwrap(), value);
        assert_eq!(compress(&dictionary, &value).unwrap(), stored, "deterministic");
        let read = loaded.and_then(|mut it| it.decompress(&stored, value.len())).unwrap();
        assert_eq!(read, value, "a frame of the prepared dictionary reads with its bytes loaded");
    }

    #[test]
    fn a_wrong_length_or_garbage_is_refused() {
        let samples: Vec<Vec<u8>> = (0..200).map(sample).collect();
        let refs: Vec<&[u8]> = samples.iter().map(Vec::as_slice).collect();
        let dictionary = Prepared::new(train(&refs).unwrap()).unwrap();
        let value = sample(3);
        let stored = compress(&dictionary, &value).unwrap();
        assert!(decompress(&dictionary, &stored, value.len() as u64 + 1).is_err());
        assert!(decompress(&dictionary, &stored, value.len() as u64 - 1).is_err());
        assert!(decompress(&dictionary, b"not zstd at all", 10).is_err());
    }

    /// Only a standard zstd frame reaches zstd: a legacy frame (v0.7's magic), a skippable frame
    /// and a stub are refused naming what they start with, whatever decoders this target's zstd
    /// was built with.
    #[test]
    fn a_frame_of_another_magic_is_refused_naming_it() {
        let dictionary = Prepared::new(OLD_DICTIONARY.to_vec()).unwrap();
        for (stored, named) in [
            (&[0x27, 0xb5, 0x2f, 0xfd, 0, 0, 0, 0][..], "[27, b5, 2f, fd]"),
            (&[0x50, 0x2a, 0x4d, 0x18, 0, 0, 0, 0][..], "[50, 2a, 4d, 18]"),
            (&[0x28, 0xb5][..], "2 bytes are too short"),
        ] {
            for why in [
                decompress(&dictionary, stored, 10).unwrap_err(),
                decompress_long(stored, 10).unwrap_err(),
            ] {
                assert!(why.contains(named), "{why}");
            }
        }
    }

    /// Bytes zstd will not load as a dictionary, its magic number then a corrupt header, are
    /// refused, not a panic.
    #[test]
    fn a_dictionary_zstd_refuses_is_an_error() {
        let mut bytes = vec![0x37, 0xa4, 0x30, 0xec, 1, 0, 0, 0];
        bytes.extend([0xff; 120]);
        let why = Prepared::new(bytes).err().expect("a corrupt dictionary header is refused");
        assert!(why.contains("does not load"), "{why}");
    }

    /// A record header's `raw` is not trusted before its bytes are decoded: a length its bytes
    /// cannot reach is refused before anything that long is allocated.
    #[test]
    fn a_length_the_bytes_cannot_reach_is_refused_before_it_is_allocated() {
        let samples: Vec<Vec<u8>> = (0..200).map(sample).collect();
        let refs: Vec<&[u8]> = samples.iter().map(Vec::as_slice).collect();
        let dictionary = Prepared::new(train(&refs).unwrap()).unwrap();
        let huge = 1 << 40;
        let tiny = [0x28, 0xb5, 0x2f, 0xfd, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        for stored in [compress(&dictionary, &sample(4)).unwrap(), tiny.to_vec()] {
            assert!(decompress(&dictionary, &stored, huge).is_err());
            assert!(decompress_long(&stored, huge).is_err());
        }
        let stored = lz4(&sample(1).repeat(64)).unwrap();
        let why = unlz4(&stored, huge).unwrap_err();
        assert!(why.contains("cannot decode"), "{why}");
        assert!(unlz4(&[0; 16], u64::MAX).is_err());
    }

    /// A long value whose repeats are further apart than zstd's default window reaches.
    fn long_value() -> Vec<u8> {
        let mut rng = blaze_ids::rng::SplitMix64::new(5);
        let block: Vec<u8> = (0..LONG_FROM / 2).map(|_| rng.next_u64() as u8).collect();
        let mut value = block.clone();
        value.extend((0..LONG_FROM).map(|_| rng.next_u64() as u8));
        value.extend_from_slice(&block);
        value
    }

    #[test]
    fn a_long_value_round_trips_the_same_every_time() {
        let value = long_value();
        let stored = compress_long(&value).unwrap();
        assert!(stored.len() < value.len() * 7 / 8, "{} of {}", stored.len(), value.len());
        assert_eq!(decompress_long(&stored, value.len() as u64).unwrap(), value);
        assert_eq!(compress_long(&value).unwrap(), stored, "deterministic");
        assert!(decompress_long(&stored, value.len() as u64 - 1).is_err());
    }

    #[test]
    fn a_batch_compresses_as_one_call_does_and_without_a_dictionary_reads_as_long() {
        let samples: Vec<Vec<u8>> = (0..200).map(sample).collect();
        let refs: Vec<&[u8]> = samples.iter().map(Vec::as_slice).collect();
        let dictionary = Prepared::new(train(&refs).unwrap()).unwrap();
        let mut batch = Batch::new(Some(&dictionary)).unwrap();
        for n in [5, 6, 7] {
            let value = sample(n);
            let stored = batch.compress(&value).unwrap();
            assert_eq!(stored, compress(&dictionary, &value).unwrap());
            assert_eq!(decompress(&dictionary, &stored, value.len() as u64).unwrap(), value);
        }
        let mut plain = Batch::new(None).unwrap();
        let value = sample(8);
        let stored = plain.compress(&value).unwrap();
        assert_eq!(decompress_long(&stored, value.len() as u64).unwrap(), value);
    }

    /// The reference for the thread's contexts: a context made for the one value and dropped.
    fn compress_fresh(dictionary: &Prepared, raw: &[u8]) -> Vec<u8> {
        zstd::bulk::Compressor::with_prepared_dictionary(&dictionary.encoder)
            .unwrap()
            .compress(raw)
            .unwrap()
    }

    fn decompress_fresh(dictionary: &Prepared, stored: &[u8], raw: usize) -> Vec<u8> {
        zstd::bulk::Decompressor::with_prepared_dictionary(&dictionary.decoder)
            .unwrap()
            .decompress(stored, raw)
            .unwrap()
    }

    fn two_dictionaries() -> (Prepared, Prepared) {
        let first: Vec<Vec<u8>> = (0..200).map(sample).collect();
        let second: Vec<Vec<u8>> = (0..200).map(|n| sample(n * 31 + 7).repeat(2)).collect();
        let train_on = |samples: &[Vec<u8>]| {
            let refs: Vec<&[u8]> = samples.iter().map(Vec::as_slice).collect();
            Prepared::new(train(&refs).unwrap()).unwrap()
        };
        (train_on(&first), train_on(&second))
    }

    /// A thread's contexts carry nothing from one value to the next: values of two dictionaries
    /// and long frames, interleaved on one thread, give the bytes a fresh context gives each.
    #[test]
    fn reused_contexts_make_and_read_the_bytes_fresh_ones_do() {
        let (first, second) = two_dictionaries();
        let long = long_value();
        let mut rng = blaze_ids::rng::SplitMix64::new(9);
        for step in 0..60u64 {
            let value = sample(rng.next_u64() as usize % 5000).repeat(1 + step as usize % 3);
            let dictionary = if rng.next_u64() % 2 == 0 { &first } else { &second };
            let stored = compress(dictionary, &value).unwrap();
            assert_eq!(stored, compress_fresh(dictionary, &value), "step {step}");
            assert_eq!(decompress(dictionary, &stored, value.len() as u64).unwrap(), value);
            assert_eq!(decompress_fresh(dictionary, &stored, value.len()), value);
            if step % 20 == 0 {
                let stored = compress_long(&long).unwrap();
                assert_eq!(decompress_long(&stored, long.len() as u64).unwrap(), long);
            }
            let other = if std::ptr::eq(dictionary, &first) { &second } else { &first };
            assert!(
                decompress(other, &stored, value.len() as u64).is_err(),
                "step {step}: the wrong dictionary"
            );
        }
    }

    /// Each thread's contexts make the same bytes as every other's.
    #[test]
    #[cfg(not(blaze_check))]
    fn every_threads_contexts_make_the_same_bytes() {
        let (first, second) = two_dictionaries();
        let values: Vec<Vec<u8>> = (0..40).map(|n| sample(n * 101).repeat(1 + n % 4)).collect();
        let want: Vec<Vec<u8>> = values
            .iter()
            .enumerate()
            .map(|(n, value)| compress_fresh([&first, &second][n % 2], value))
            .collect();
        std::thread::scope(|scope| {
            for t in 0..4 {
                let (first, second, values, want) = (&first, &second, &values, &want);
                scope.spawn(move || {
                    for (n, value) in
                        values.iter().enumerate().cycle().skip(t * 7).take(values.len() * 2)
                    {
                        let dictionary = [first, second][n % 2];
                        let stored = compress(dictionary, value).unwrap();
                        assert_eq!(stored, want[n], "thread {t}, value {n}");
                        assert_eq!(
                            &decompress(dictionary, &stored, value.len() as u64).unwrap(),
                            value
                        );
                    }
                });
            }
        });
    }

    #[test]
    fn lz4_keeps_only_what_it_shrinks_and_round_trips() {
        let text = sample(1).repeat(64);
        let stored = lz4(&text).unwrap();
        assert_eq!(unlz4(&stored, text.len() as u64).unwrap(), text);
        assert!(unlz4(&stored, text.len() as u64 + 1).is_err());
        let mut rng = blaze_ids::rng::SplitMix64::new(6);
        let noise: Vec<u8> = (0..64 << 10).map(|_| rng.next_u64() as u8).collect();
        assert_eq!(lz4(&noise), None);
    }

    /// The on-disk bytes do not change with the buffer: into a reused one, each value is what
    /// `lz4` makes, kept exactly when `lz4` keeps it.
    #[test]
    fn lz4_into_a_reused_buffer_makes_the_same_bytes() {
        let mut rng = blaze_ids::rng::SplitMix64::new(7);
        let noise: Vec<u8> = (0..64 << 10).map(|_| rng.next_u64() as u8).collect();
        let mut out = vec![0xAA; 3];
        for value in
            [sample(1).repeat(64), noise, sample(2).repeat(3), Vec::new(), sample(3).repeat(500)]
        {
            let kept = lz4_into(&value, &mut out);
            assert_eq!(lz4(&value), kept.then(|| out.clone()), "{} bytes", value.len());
        }
    }

    /// What C zstd 1.5.7 (zstd-sys 2.1.2+zstd.1.5.7, the zstd every target used until wasm got
    /// libzstd-rs-sys: vendor/zstd-sys/SOURCE.md) wrote with this module's calls: a dictionary
    /// [`train`]ed on 200 [`fixture_sample`]s, one of them through [`compress`], and
    /// [`fixture_long`] through [`compress_long`] on its 4 workers. Never regenerated: they stand
    /// for what stores already hold.
    const OLD_DICTIONARY: &[u8] = include_bytes!("fixtures/zstd-1.5.7/dictionary");
    const OLD_FRAME: &[u8] = include_bytes!("fixtures/zstd-1.5.7/dictionary-level6.zst");
    const OLD_LONG_FRAME: &[u8] = include_bytes!("fixtures/zstd-1.5.7/long-level6.zst");

    /// The fixtures' values: kept apart from [`sample`], which may change.
    fn fixture_sample(n: usize) -> Vec<u8> {
        format!(
            "{{\"unit\": \"crate-{n}\", \"cfg\": [\"feature=\\\"std\\\"\", \"target_os=\\\"linux\\\"\"], \
             \"env\": {{\"OUT_DIR\": \"/build/out/{n}\", \"PROFILE\": \"debug\"}}, \"links\": \
             [\"z\", \"ssl\", \"crypto\"], \"rerun\": [\"build.rs\", \"src/{n}.c\"], \"n\": {}}}\n",
            n * 7919 % 1000
        )
        .repeat(4)
        .into_bytes()
    }

    /// 9 MiB of numbered copies of one 60 KiB block. zstd runs a frame of at most 512 KiB on one
    /// thread whatever its worker count (`ZSTDMT_JOBSIZE_MIN`); this one it cuts into 2 MiB jobs
    /// (level 6 with long-distance matching), five of them for the 4 workers, so the frame pins
    /// zstdmt's output and, on wasm, libzstd-rs-sys's inline pool (vendor/libzstd-rs-sys
    /// SOURCE.md). It compresses to 26 KB.
    fn fixture_long() -> Vec<u8> {
        const LEN: usize = 9 << 20;
        let block: String = (0..4096u64)
            .map(|i| format!("{i:06} {:07}\n", i * 2_654_435_761 % 1_000_003))
            .collect();
        let mut out = String::with_capacity(LEN + block.len() + 32);
        let mut copy = 0u64;
        while out.len() < LEN {
            out.push_str(&format!("copy {copy}\n"));
            out.push_str(&block);
            copy += 1;
        }
        out.truncate(LEN);
        out.into_bytes()
    }

    /// Durability: frames the old zstd wrote read back to their values, whichever zstd this
    /// target builds.
    #[test]
    fn frames_c_zstd_1_5_7_wrote_decode_to_the_same_values() {
        let dictionary = Prepared::new(OLD_DICTIONARY.to_vec()).unwrap();
        let value = fixture_sample(12_345);
        assert_eq!(decompress(&dictionary, OLD_FRAME, value.len() as u64).unwrap(), value);
        let long = fixture_long();
        assert_eq!(decompress_long(OLD_LONG_FRAME, long.len() as u64).unwrap(), long);
    }

    /// What every target writes is what C zstd 1.5.7 wrote, so a store made on one target is the
    /// store another makes. On wasm that is libzstd-rs-sys, a translation of zstd's dev branch
    /// after 1.5.7 (vendor/libzstd-rs-sys/SOURCE.md): the same bytes are measured, not promised,
    /// and this test is what holds them, on every bump of either zstd.
    #[test]
    fn every_target_writes_the_bytes_c_zstd_1_5_7_wrote() {
        let samples: Vec<Vec<u8>> = (0..200).map(fixture_sample).collect();
        let refs: Vec<&[u8]> = samples.iter().map(Vec::as_slice).collect();
        assert_eq!(train(&refs).unwrap(), OLD_DICTIONARY, "the trained dictionary");
        let dictionary = Prepared::new(OLD_DICTIONARY.to_vec()).unwrap();
        assert_eq!(
            compress(&dictionary, &fixture_sample(12_345)).unwrap(),
            OLD_FRAME,
            "a dictionary frame"
        );
        let long = fixture_long();
        assert_eq!(compress_long(&long).unwrap(), OLD_LONG_FRAME, "a long frame on 4 workers");
        // The fixture stands for the threaded path only while zstd cuts it into jobs: on one
        // thread the same parameters write other bytes.
        let mut alone = zstd::bulk::Compressor::new(LEVEL).unwrap();
        for parameter in [
            CParameter::EnableLongDistanceMatching(true),
            CParameter::WindowLog(WINDOW_LOG),
            CParameter::NbWorkers(0),
        ] {
            alone.set_parameter(parameter).unwrap();
        }
        assert_ne!(
            alone.compress(&long).unwrap(),
            OLD_LONG_FRAME,
            "the long fixture runs on one thread"
        );
    }
}
