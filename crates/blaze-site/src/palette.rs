//! Exact dark & light color tokens from `site/styles/tokens.css`, typography roles from
//! `site/client/lib/page/contract.ts`, and the 9 embedded SVG icons/signature (`assets/*.svg`).

use blaze_ui::types::Rgba;

/// Semantic palette token resolved per appearance (dark / light).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Token {
    Bg,
    Bg2,
    Bg3,
    Ink1,
    Ink2,
    Ink3,
    Ink4,
    Line,
    Line2,
    STint,
    Fire,
    SynText,
    SynKw,
    SynFn,
    SynTy,
    SynParam,
    SynMark,
}

#[inline]
fn rgb8(r: u8, g: u8, b: u8) -> Rgba {
    Rgba::new(f32::from(r) / 255.0, f32::from(g) / 255.0, f32::from(b) / 255.0, 1.0)
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SiteTheme {
    pub dark: bool,
    pub bg: Rgba,
    pub bg_2: Rgba,
    pub bg_3: Rgba,
    pub ink_1: Rgba,
    pub ink_2: Rgba,
    pub ink_3: Rgba,
    pub ink_4: Rgba,
    pub line: Rgba,
    pub line_2: Rgba,
    pub s_tint: Rgba,
    pub fire: Rgba,
    pub syn_text: Rgba,
    pub syn_kw: Rgba,
    pub syn_fn: Rgba,
    pub syn_ty: Rgba,
    pub syn_param: Rgba,
    pub syn_mark: Rgba,
    pub selection: Rgba,
}

impl SiteTheme {
    /// Obsidian dark theme (`tokens.css` `:root`).
    pub fn dark() -> Self {
        Self {
            dark: true,
            bg: rgb8(0x10, 0x10, 0x11),
            bg_2: rgb8(0x14, 0x14, 0x15),
            bg_3: rgb8(0x19, 0x19, 0x1a),
            ink_1: rgb8(0xeb, 0xea, 0xe6),
            ink_2: rgb8(0xb3, 0xb2, 0xaf),
            ink_3: rgb8(0x87, 0x86, 0x84),
            ink_4: rgb8(0x57, 0x57, 0x5a),
            line: rgb8(0x28, 0x28, 0x2a),
            line_2: rgb8(0x33, 0x33, 0x35),
            s_tint: Rgba::new(0.922, 0.918, 0.902, 0.06),
            fire: rgb8(0xe0, 0x46, 0x1a),
            syn_text: rgb8(0xbc, 0xbe, 0xc4),
            syn_kw: rgb8(0xcf, 0x8e, 0x6d),
            syn_fn: rgb8(0x56, 0xa8, 0xf5),
            syn_ty: rgb8(0xc7, 0x7d, 0xbb),
            syn_param: rgb8(0xbc, 0xbe, 0xc4),
            syn_mark: rgb8(0x7a, 0x7e, 0x85),
            selection: Rgba::new(0.32, 0.56, 0.98, 0.28),
        }
    }

    /// Warm paper light theme (`tokens.css` `@media (prefers-color-scheme: light)`).
    pub fn light() -> Self {
        Self {
            dark: false,
            bg: rgb8(0xf2, 0xf1, 0xee),
            bg_2: rgb8(0xea, 0xe9, 0xe4),
            bg_3: rgb8(0xf6, 0xf5, 0xf2),
            ink_1: rgb8(0x16, 0x16, 0x17),
            ink_2: rgb8(0x46, 0x46, 0x4a),
            ink_3: rgb8(0x6b, 0x6a, 0x6e),
            ink_4: rgb8(0xa2, 0xa1, 0x9d),
            line: rgb8(0xdc, 0xdb, 0xd5),
            line_2: rgb8(0xcd, 0xcc, 0xc6),
            s_tint: Rgba::new(0.086, 0.086, 0.090, 0.06),
            fire: rgb8(0xd6, 0x3c, 0x12),
            syn_text: rgb8(0x08, 0x08, 0x08),
            syn_kw: rgb8(0x00, 0x33, 0xb3),
            syn_fn: rgb8(0x00, 0x62, 0x7a),
            syn_ty: rgb8(0x87, 0x10, 0x94),
            syn_param: rgb8(0x08, 0x08, 0x08),
            syn_mark: rgb8(0x8c, 0x8c, 0x8c),
            selection: Rgba::new(0.18, 0.48, 0.95, 0.20),
        }
    }

    #[inline]
    pub const fn color(&self, token: Token) -> Rgba {
        match token {
            Token::Bg => self.bg,
            Token::Bg2 => self.bg_2,
            Token::Bg3 => self.bg_3,
            Token::Ink1 => self.ink_1,
            Token::Ink2 => self.ink_2,
            Token::Ink3 => self.ink_3,
            Token::Ink4 => self.ink_4,
            Token::Line => self.line,
            Token::Line2 => self.line_2,
            Token::STint => self.s_tint,
            Token::Fire => self.fire,
            Token::SynText => self.syn_text,
            Token::SynKw => self.syn_kw,
            Token::SynFn => self.syn_fn,
            Token::SynTy => self.syn_ty,
            Token::SynParam => self.syn_param,
            Token::SynMark => self.syn_mark,
        }
    }
}

/// Inline or block SVG picture identifier (9 embedded SVG assets).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ImageName {
    Rust = 0,
    X = 1,
    RsFile = 2,
    KindStruct = 3,
    Helix = 4,
    Folder = 5,
    Yc = 6,
    Mail = 7,
    Signature = 8,
}

impl ImageName {
    pub const COUNT: usize = 9;

    pub const ALL: [Self; Self::COUNT] = [
        Self::Rust,
        Self::X,
        Self::RsFile,
        Self::KindStruct,
        Self::Helix,
        Self::Folder,
        Self::Yc,
        Self::Mail,
        Self::Signature,
    ];

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "rust" => Some(Self::Rust),
            "x" => Some(Self::X),
            "rsFile" => Some(Self::RsFile),
            "kindStruct" => Some(Self::KindStruct),
            "helix" => Some(Self::Helix),
            "folder" => Some(Self::Folder),
            "yc" => Some(Self::Yc),
            "mail" => Some(Self::Mail),
            "signature" => Some(Self::Signature),
            _ => None,
        }
    }

    pub const fn index(self) -> usize {
        match self {
            Self::Rust => 0,
            Self::X => 1,
            Self::RsFile => 2,
            Self::KindStruct => 3,
            Self::Helix => 4,
            Self::Folder => 5,
            Self::Yc => 6,
            Self::Mail => 7,
            Self::Signature => 8,
        }
    }

    pub const fn is_mask(self) -> bool {
        matches!(self, Self::Rust | Self::X | Self::Signature)
    }

    pub const fn aspect(self) -> f32 {
        match self {
            Self::Rust | Self::X | Self::RsFile | Self::KindStruct | Self::Yc => 1.0,
            Self::Helix => 0.62 / 0.98,
            Self::Folder | Self::Mail => 1.15,
            Self::Signature => 248.0 / 78.0,
        }
    }

    pub const fn svg_source(self) -> &'static str {
        match self {
            Self::Rust => include_str!("../assets/rust.svg"),
            Self::X => include_str!("../assets/x.svg"),
            Self::RsFile => include_str!("../assets/rs-file.svg"),
            Self::KindStruct => include_str!("../assets/kind-struct.svg"),
            Self::Helix => include_str!("../assets/helix.svg"),
            Self::Folder => include_str!("../assets/folder.svg"),
            Self::Yc => include_str!("../assets/yc.svg"),
            Self::Mail => include_str!("../assets/mail.svg"),
            Self::Signature => include_str!("../assets/signature.svg"),
        }
    }
}

/// Link hover & underline style for a typography role.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LinkStyle {
    pub ink: Token,
    pub hover: Token,
    pub underline: bool,
}

/// Typography role matching `site/client/lib/page/contract.ts::ROLES`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RoleName {
    Wordmark,
    Bar,
    Footer,
    Backed,
    Title,
    TitleMark,
    Label,
    Cmd,
    CmdDim,
    CmdFaint,
    CmdPrompt,
    Button,
    Item,
    Sub,
    SubHead,
    SubMark,
    Code,
    CodeArg,
    CodeProg,
    CodeMark,
    CodePrompt,
    Badge,
    RsText,
    RsKw,
    RsFn,
    RsTy,
    RsParam,
    RsMark,
    RaceWho,
    RaceWhoLead,
    RaceNum,
    RaceNumLead,
    RaceUnit,
    Sign,
    Note,
    TipTitle,
    Tip,
    TipCode,
    TipPrompt,
}

/// Resolved typography metrics & colors for a [`RoleName`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RoleSpec {
    /// `0` = Inter, `1` = Berkeley Mono.
    pub family: u32,
    pub wght: u32,
    pub opsz: f32,
    pub size: f32,
    pub narrow: Option<f32>,
    pub track: f32,
    pub lead: f32,
    pub ink: Token,
    pub link: Option<LinkStyle>,
}

impl RoleName {
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "wordmark" => Some(Self::Wordmark),
            "bar" => Some(Self::Bar),
            "footer" => Some(Self::Footer),
            "backed" => Some(Self::Backed),
            "title" => Some(Self::Title),
            "titleMark" => Some(Self::TitleMark),
            "label" => Some(Self::Label),
            "cmd" => Some(Self::Cmd),
            "cmdDim" => Some(Self::CmdDim),
            "cmdFaint" => Some(Self::CmdFaint),
            "cmdPrompt" => Some(Self::CmdPrompt),
            "button" => Some(Self::Button),
            "item" => Some(Self::Item),
            "sub" => Some(Self::Sub),
            "subHead" => Some(Self::SubHead),
            "subMark" => Some(Self::SubMark),
            "code" => Some(Self::Code),
            "codeArg" => Some(Self::CodeArg),
            "codeProg" => Some(Self::CodeProg),
            "codeMark" => Some(Self::CodeMark),
            "codePrompt" => Some(Self::CodePrompt),
            "badge" => Some(Self::Badge),
            "rsText" => Some(Self::RsText),
            "rsKw" => Some(Self::RsKw),
            "rsFn" => Some(Self::RsFn),
            "rsTy" => Some(Self::RsTy),
            "rsParam" => Some(Self::RsParam),
            "rsMark" => Some(Self::RsMark),
            "raceWho" => Some(Self::RaceWho),
            "raceWhoLead" => Some(Self::RaceWhoLead),
            "raceNum" => Some(Self::RaceNum),
            "raceNumLead" => Some(Self::RaceNumLead),
            "raceUnit" => Some(Self::RaceUnit),
            "sign" => Some(Self::Sign),
            "note" => Some(Self::Note),
            "tipTitle" => Some(Self::TipTitle),
            "tip" => Some(Self::Tip),
            "tipCode" => Some(Self::TipCode),
            "tipPrompt" => Some(Self::TipPrompt),
            _ => None,
        }
    }

    pub const fn spec(self) -> RoleSpec {
        match self {
            Self::Wordmark => RoleSpec { family: 0, wght: 600, opsz: 16.0, size: 16.0, narrow: None, track: -0.02, lead: 1.5, ink: Token::Ink1, link: None },
            Self::Bar => RoleSpec { family: 0, wght: 400, opsz: 14.0, size: 14.0, narrow: Some(13.0), track: 0.0, lead: 1.5, ink: Token::Ink3, link: Some(LinkStyle { ink: Token::Ink3, hover: Token::Ink1, underline: false }) },
            Self::Footer => RoleSpec { family: 0, wght: 400, opsz: 14.0, size: 14.0, narrow: None, track: 0.0, lead: 1.5, ink: Token::Ink3, link: Some(LinkStyle { ink: Token::Ink3, hover: Token::Ink1, underline: false }) },
            Self::Backed => RoleSpec { family: 0, wght: 400, opsz: 14.0, size: 13.0, narrow: None, track: 0.0, lead: 1.5, ink: Token::Ink3, link: Some(LinkStyle { ink: Token::Ink3, hover: Token::Ink1, underline: false }) },
            Self::Title => RoleSpec { family: 0, wght: 600, opsz: 32.0, size: 40.0, narrow: Some(32.0), track: -0.032, lead: 1.1, ink: Token::Ink1, link: None },
            Self::TitleMark => RoleSpec { family: 0, wght: 500, opsz: 24.0, size: 24.8, narrow: Some(19.84), track: 0.0, lead: 1.1, ink: Token::Ink3, link: Some(LinkStyle { ink: Token::Ink3, hover: Token::Ink1, underline: false }) },
            Self::Label => RoleSpec { family: 0, wght: 500, opsz: 14.0, size: 13.0, narrow: None, track: 0.0, lead: 1.5, ink: Token::Ink3, link: None },
            Self::Cmd => RoleSpec { family: 1, wght: 400, opsz: 0.0, size: 15.0, narrow: Some(11.0), track: -0.01, lead: 1.5, ink: Token::Ink1, link: None },
            Self::CmdDim => RoleSpec { family: 1, wght: 400, opsz: 0.0, size: 15.0, narrow: Some(11.0), track: -0.01, lead: 1.5, ink: Token::Ink3, link: None },
            Self::CmdFaint => RoleSpec { family: 1, wght: 400, opsz: 0.0, size: 15.0, narrow: Some(11.0), track: -0.01, lead: 1.5, ink: Token::Ink4, link: None },
            Self::CmdPrompt => RoleSpec { family: 1, wght: 400, opsz: 0.0, size: 15.0, narrow: Some(11.0), track: -0.01, lead: 1.5, ink: Token::Fire, link: None },
            Self::Button => RoleSpec { family: 0, wght: 500, opsz: 14.0, size: 13.0, narrow: None, track: -0.005, lead: 1.0, ink: Token::Ink2, link: None },
            Self::Item => RoleSpec { family: 0, wght: 600, opsz: 17.0, size: 17.0, narrow: None, track: -0.012, lead: 1.55, ink: Token::Ink1, link: Some(LinkStyle { ink: Token::Ink1, hover: Token::Ink1, underline: true }) },
            Self::Sub => RoleSpec { family: 0, wght: 400, opsz: 15.0, size: 15.0, narrow: None, track: 0.0, lead: 1.5, ink: Token::Ink3, link: Some(LinkStyle { ink: Token::Ink2, hover: Token::Ink2, underline: true }) },
            Self::SubHead => RoleSpec { family: 0, wght: 500, opsz: 15.0, size: 15.0, narrow: None, track: -0.006, lead: 1.5, ink: Token::Ink1, link: Some(LinkStyle { ink: Token::Ink1, hover: Token::Ink1, underline: true }) },
            Self::SubMark => RoleSpec { family: 0, wght: 400, opsz: 15.0, size: 15.0, narrow: None, track: 0.0, lead: 1.5, ink: Token::Ink3, link: Some(LinkStyle { ink: Token::Ink3, hover: Token::Ink1, underline: false }) },
            Self::Code => RoleSpec { family: 1, wght: 500, opsz: 0.0, size: 12.9, narrow: None, track: 0.0, lead: 1.3, ink: Token::Ink1, link: None },
            Self::CodeArg => RoleSpec { family: 1, wght: 400, opsz: 0.0, size: 12.9, narrow: None, track: 0.0, lead: 1.3, ink: Token::Ink2, link: None },
            Self::CodeProg => RoleSpec { family: 1, wght: 600, opsz: 0.0, size: 12.9, narrow: None, track: 0.0, lead: 1.3, ink: Token::Ink1, link: None },
            Self::CodeMark => RoleSpec { family: 1, wght: 400, opsz: 0.0, size: 12.9, narrow: None, track: 0.0, lead: 1.3, ink: Token::Ink4, link: None },
            Self::CodePrompt => RoleSpec { family: 1, wght: 400, opsz: 0.0, size: 12.9, narrow: None, track: 0.0, lead: 1.3, ink: Token::Ink4, link: None },
            Self::Badge => RoleSpec { family: 0, wght: 500, opsz: 14.0, size: 11.5, narrow: None, track: 0.01, lead: 1.3, ink: Token::Ink2, link: None },
            Self::RsText => RoleSpec { family: 1, wght: 400, opsz: 0.0, size: 12.9, narrow: None, track: 0.0, lead: 1.3, ink: Token::SynText, link: None },
            Self::RsKw => RoleSpec { family: 1, wght: 400, opsz: 0.0, size: 12.9, narrow: None, track: 0.0, lead: 1.3, ink: Token::SynKw, link: None },
            Self::RsFn => RoleSpec { family: 1, wght: 400, opsz: 0.0, size: 12.9, narrow: None, track: 0.0, lead: 1.3, ink: Token::SynFn, link: None },
            Self::RsTy => RoleSpec { family: 1, wght: 400, opsz: 0.0, size: 12.9, narrow: None, track: 0.0, lead: 1.3, ink: Token::SynTy, link: None },
            Self::RsParam => RoleSpec { family: 1, wght: 400, opsz: 0.0, size: 12.9, narrow: None, track: 0.0, lead: 1.3, ink: Token::SynParam, link: None },
            Self::RsMark => RoleSpec { family: 1, wght: 400, opsz: 0.0, size: 12.9, narrow: None, track: 0.0, lead: 1.3, ink: Token::SynMark, link: None },
            Self::RaceWho => RoleSpec { family: 1, wght: 400, opsz: 0.0, size: 13.0, narrow: None, track: 0.0, lead: 1.5, ink: Token::Ink3, link: None },
            Self::RaceWhoLead => RoleSpec { family: 1, wght: 400, opsz: 0.0, size: 13.0, narrow: None, track: 0.0, lead: 1.5, ink: Token::Ink1, link: None },
            Self::RaceNum => RoleSpec { family: 0, wght: 400, opsz: 15.0, size: 15.0, narrow: None, track: 0.0, lead: 1.5, ink: Token::Ink2, link: None },
            Self::RaceNumLead => RoleSpec { family: 0, wght: 400, opsz: 15.0, size: 15.0, narrow: None, track: 0.0, lead: 1.5, ink: Token::Fire, link: None },
            Self::RaceUnit => RoleSpec { family: 0, wght: 400, opsz: 14.0, size: 13.0, narrow: None, track: 0.0, lead: 1.5, ink: Token::Ink3, link: None },
            Self::Sign => RoleSpec { family: 0, wght: 400, opsz: 14.0, size: 13.0, narrow: None, track: 0.0, lead: 1.5, ink: Token::Ink3, link: Some(LinkStyle { ink: Token::Ink1, hover: Token::Ink1, underline: true }) },
            Self::Note => RoleSpec { family: 0, wght: 400, opsz: 14.0, size: 12.0, narrow: None, track: 0.0, lead: 1.6, ink: Token::Ink3, link: Some(LinkStyle { ink: Token::Ink2, hover: Token::Ink1, underline: true }) },
            Self::TipTitle => RoleSpec { family: 0, wght: 600, opsz: 14.0, size: 13.0, narrow: None, track: -0.003, lead: 1.45, ink: Token::Ink1, link: None },
            Self::Tip => RoleSpec { family: 0, wght: 400, opsz: 14.0, size: 13.0, narrow: None, track: -0.003, lead: 1.45, ink: Token::Ink2, link: Some(LinkStyle { ink: Token::Ink1, hover: Token::Ink1, underline: true }) },
            Self::TipCode => RoleSpec { family: 1, wght: 500, opsz: 0.0, size: 11.2, narrow: None, track: 0.0, lead: 1.3, ink: Token::Ink1, link: None },
            Self::TipPrompt => RoleSpec { family: 1, wght: 400, opsz: 0.0, size: 11.2, narrow: None, track: 0.0, lead: 1.3, ink: Token::Ink4, link: None },
        }
    }
}
