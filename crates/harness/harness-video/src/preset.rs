//! Export presets: what the encoder makes, by name. `../presets.json` is their one home (compiled in); this is its typed
//! form. A field the file has and this does not know, or a value outside the ones named here, refuses the whole file.
use serde::Deserialize;

/// The presets file.
const FILE: &str = include_str!("../presets.json");
/// The preset a render with none named takes.
pub const DEFAULT: &str = "youtube-hdr10-master";

#[derive(Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Codec {
    H264,
    Hevc,
}

#[derive(Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Profile {
    /// H.264 High.
    High,
    /// HEVC Main (8-bit).
    Main,
    /// HEVC Main 10.
    Main10,
}

/// 4:2:0, biplanar, video range.
#[derive(Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pixel {
    #[serde(rename = "yuv420p8")]
    Yuv420p8,
    #[serde(rename = "yuv420p10")]
    Yuv420p10,
}

#[derive(Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Colour {
    /// BT.709 primaries, transfer and matrix, video (limited) range: SDR.
    #[serde(rename = "bt709-video-range")]
    Bt709,
    /// BT.2020 primaries and matrix, the PQ transfer, video range, the P3-D65 1000-nit mastering display as SEI.
    #[serde(rename = "hdr10")]
    Hdr10,
}

#[derive(Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Audio {
    /// AAC-LC stereo at `audio_kbps`: the scene's soundtrack, or digital silence when it has none (a track is always
    /// written).
    AacLc,
    /// Apple Lossless, 24-bit stereo: the scene's soundtrack; no track when it has none.
    Alac,
    /// No audio track.
    None,
}

#[derive(Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Container {
    /// MP4 with the `moov` before the `mdat` (playable as it downloads).
    #[serde(rename = "mp4-moov-first")]
    Mp4MoovFirst,
}

/// One preset.
#[derive(Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Export {
    /// What it is for, in words.
    #[serde(rename = "for")]
    pub what: String,
    /// Pixels; 0 is the scene's own (its points times its scale).
    pub width: u32,
    pub height: u32,
    /// Frames a second; 0 is the scene's own.
    pub fps: u32,
    pub codec: Codec,
    pub profile: Profile,
    pub pixel: Pixel,
    /// An average bitrate, or (with none) a constant `quality`.
    pub bitrate_kbps: Option<u32>,
    /// Constant quality, 0 to 1 (VideoToolbox's scale).
    pub quality: Option<f64>,
    /// The longest stretch between key frames.
    pub keyframe_seconds: f64,
    /// Every group of pictures decodes alone (X refuses an open one).
    pub closed_gop: bool,
    pub colour: Colour,
    pub audio: Audio,
    pub audio_kbps: Option<u32>,
    pub container: Container,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(rename = "about")]
    _about: String,
    #[serde(rename = "x")]
    _x: serde_json::Value,
    presets: std::collections::BTreeMap<String, Export>,
}

fn file() -> Result<File, String> {
    let f: File = serde_json::from_str(FILE).map_err(|e| format!("presets.json: {e}"))?;
    for (name, p) in &f.presets {
        p.check().map_err(|e| format!("presets.json: preset {name}: {e}"))?;
    }
    Ok(f)
}

/// The preset `name`. Err names it and lists the ones there are.
pub fn named(name: &str) -> Result<Export, String> {
    let mut f = file()?;
    f.presets.remove(name).ok_or_else(|| {
        format!("no preset {name:?} in presets.json (it has: {})", f.presets.keys().cloned().collect::<Vec<_>>().join(", "))
    })
}

/// Every preset's name.
pub fn names() -> Result<Vec<String>, String> {
    Ok(file()?.presets.into_keys().collect())
}

impl Export {
    /// The combinations the encoder has: each refusal names the field and the rule.
    fn check(&self) -> Result<(), String> {
        match (self.codec, self.profile, self.pixel) {
            (Codec::H264, Profile::High, Pixel::Yuv420p8)
            | (Codec::Hevc, Profile::Main, Pixel::Yuv420p8)
            | (Codec::Hevc, Profile::Main10, Pixel::Yuv420p10) => {}
            other => {
                return Err(format!(
                    "{other:?}: the encoder has h264 high yuv420p8, hevc main yuv420p8 and hevc main10 yuv420p10"
                ));
            }
        }
        match (self.colour, self.pixel) {
            (Colour::Bt709, Pixel::Yuv420p8) | (Colour::Hdr10, Pixel::Yuv420p10) => {}
            other => return Err(format!("{other:?}: bt709-video-range is 8-bit and hdr10 is 10-bit")),
        }
        match (self.bitrate_kbps, self.quality) {
            (Some(k), None) if k > 0 => {}
            (None, Some(q)) if (0.0..=1.0).contains(&q) => {}
            other => return Err(format!("{other:?}: give bitrate_kbps above 0, or quality in 0..=1, not both")),
        }
        if !(self.keyframe_seconds > 0.0) {
            return Err(format!("keyframe_seconds {} is not above 0", self.keyframe_seconds));
        }
        match (self.audio, self.audio_kbps) {
            (Audio::AacLc, Some(k)) if (64..=320).contains(&k) => {}
            (Audio::Alac | Audio::None, None) => {}
            other => return Err(format!("{other:?}: aac-lc takes audio_kbps in 64..=320; alac and none take none")),
        }
        if (self.width == 0) != (self.height == 0) || self.width % 2 != 0 || self.height % 2 != 0 {
            return Err(format!("{}x{}: both 0 (the scene's own) or both even", self.width, self.height));
        }
        Ok(())
    }

    /// A line for a person: `h264 high yuv420p8 1920x1080 60 fps, 40000 kb/s, ...`.
    pub fn words(&self) -> String {
        let rate = match (self.bitrate_kbps, self.quality) {
            (Some(k), _) => format!("{k} kb/s"),
            (_, Some(q)) => format!("quality {q}"),
            _ => String::new(),
        };
        let audio = match (self.audio, self.audio_kbps) {
            (Audio::AacLc, Some(k)) => format!("aac-lc {k} kb/s stereo 48 kHz"),
            (Audio::Alac, _) => "alac 24-bit stereo 48 kHz".to_owned(),
            _ => "no audio".to_owned(),
        };
        format!(
            "{:?} {:?} {:?} {}x{} {} fps, {rate}, a key frame every {} s, {} gop, {:?}, {audio}, {:?}",
            self.codec,
            self.profile,
            self.pixel,
            self.width,
            self.height,
            self.fps,
            self.keyframe_seconds,
            if self.closed_gop { "closed" } else { "open allowed" },
            self.colour,
            self.container
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shipped file parses, every preset passes its checks, and the ones the film is rendered with are there and
    /// are what X's page asks for.
    #[test]
    fn presets_load() {
        let names = names().unwrap_or_else(|e| panic!("{e}"));
        for want in ["x-landscape-1080p60", "x-square-1080", "x-portrait-1080x1920", DEFAULT] {
            assert!(names.iter().any(|n| n == want), "no preset {want}");
        }
        let x = named("x-landscape-1080p60").unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((x.width, x.height, x.fps), (1920, 1080, 60));
        assert_eq!((x.codec, x.profile, x.pixel, x.colour), (Codec::H264, Profile::High, Pixel::Yuv420p8, Colour::Bt709));
        assert!(x.closed_gop && x.audio == Audio::AacLc && x.bitrate_kbps.is_some_and(|k| k >= 5000));
        assert!(named("nope").is_err_and(|e| e.contains("x-landscape-1080p60")));
    }
}
