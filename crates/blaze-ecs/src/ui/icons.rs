//! Crisp inline SVG icon library matching `/site/renderer/core/src/icons.rs` (Phosphor Light
//! aesthetic, MIT) plus the Blaze pixel-art flame mark. Zero external requests, zero `std`, zero
//! `unsafe`.

/// Crisp 20x20 Blaze pixel-art flame logo (adapted from `/site/static/brand/blaze-icon-dark.svg`).
pub const BLAZE_LOGO_SVG: &str = concat!(
    r##"<svg class="brand-logo" width="18" height="18" viewBox="0 0 26 26" shape-rendering="crispEdges" aria-hidden="true">"##,
    r##"<rect width="26" height="26" rx="6" fill="#151517"/>"##,
    r##"<rect x="12" y="3" width="1" height="1" fill="#a8240f"/>"##,
    r##"<rect x="11" y="4" width="1" height="1" fill="#a8240f"/><rect x="12" y="4" width="1" height="1" fill="#e0461a"/>"##,
    r##"<rect x="11" y="5" width="2" height="1" fill="#e0461a"/><rect x="13" y="5" width="1" height="1" fill="#a8240f"/>"##,
    r##"<rect x="10" y="6" width="1" height="1" fill="#a8240f"/><rect x="11" y="6" width="1" height="1" fill="#ff8f1f"/><rect x="12" y="6" width="1" height="1" fill="#e0461a"/><rect x="13" y="6" width="1" height="1" fill="#a8240f"/>"##,
    r##"<rect x="10" y="7" width="1" height="1" fill="#e0461a"/><rect x="11" y="7" width="2" height="1" fill="#ff8f1f"/><rect x="13" y="7" width="1" height="1" fill="#e0461a"/>"##,
    r##"<rect x="10" y="8" width="1" height="1" fill="#e0461a"/><rect x="11" y="8" width="1" height="1" fill="#ffd43b"/><rect x="12" y="8" width="1" height="1" fill="#ff8f1f"/><rect x="13" y="8" width="1" height="1" fill="#e0461a"/><rect x="16" y="8" width="1" height="1" fill="#a8240f"/>"##,
    r##"<rect x="9" y="9" width="1" height="1" fill="#a8240f"/><rect x="10" y="9" width="1" height="1" fill="#ff8f1f"/><rect x="11" y="9" width="2" height="1" fill="#ffd43b"/><rect x="13" y="9" width="1" height="1" fill="#e0461a"/><rect x="15" y="9" width="1" height="1" fill="#a8240f"/><rect x="16" y="9" width="1" height="1" fill="#e0461a"/>"##,
    r##"<rect x="9" y="10" width="1" height="1" fill="#a8240f"/><rect x="10" y="10" width="1" height="1" fill="#ff8f1f"/><rect x="11" y="10" width="1" height="1" fill="#fff6c2"/><rect x="12" y="10" width="1" height="1" fill="#ffd43b"/><rect x="13" y="10" width="1" height="1" fill="#ff8f1f"/><rect x="15" y="10" width="1" height="1" fill="#e0461a"/><rect x="16" y="10" width="1" height="1" fill="#ff8f1f"/><rect x="17" y="10" width="1" height="1" fill="#a8240f"/>"##,
    r##"<rect x="10" y="11" width="1" height="1" fill="#e0461a"/><rect x="11" y="11" width="2" height="1" fill="#f0f6fc"/><rect x="13" y="11" width="1" height="1" fill="#ff8f1f"/><rect x="14" y="11" width="1" height="1" fill="#e0461a"/><rect x="15" y="11" width="2" height="1" fill="#ff8f1f"/><rect x="17" y="11" width="1" height="1" fill="#e0461a"/>"##,
    r##"<rect x="11" y="12" width="2" height="11" fill="#f0f6fc"/><rect x="13" y="12" width="1" height="1" fill="#ffd43b"/><rect x="14" y="12" width="1" height="1" fill="#ff8f1f"/><rect x="15" y="12" width="2" height="1" fill="#ffd43b"/><rect x="17" y="12" width="1" height="1" fill="#e0461a"/>"##,
    r##"<rect x="13" y="13" width="2" height="1" fill="#ffd43b"/><rect x="15" y="13" width="1" height="1" fill="#fff6c2"/><rect x="16" y="13" width="1" height="1" fill="#ffd43b"/><rect x="17" y="13" width="1" height="1" fill="#ff8f1f"/>"##,
    r##"<rect x="13" y="14" width="4" height="1" fill="#fff6c2"/><rect x="17" y="14" width="1" height="1" fill="#ffd43b"/>"##,
    r##"<rect x="13" y="15" width="4" height="1" fill="#f0f6fc"/><rect x="13" y="16" width="4" height="1" fill="#f0f6fc"/><rect x="16" y="17" width="2" height="5" fill="#f0f6fc"/><rect x="13" y="21" width="4" height="2" fill="#f0f6fc"/>"##,
    r##"<rect x="13" y="17" width="3" height="4" fill="#151517"/>"##,
    r##"</svg>"##,
);

/// Named crisp geometric icons in uniform 24x24 thin-stroke style (matching Blaze site's Phosphor Light).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icon {
    Cube,
    Diamond,
    Tree,
    Sliders,
    Graph,
    Table,
    Funnel,
    Code,
    Search,
    Slot,
    Gen,
    Parent,
    Tag,
    Link,
    Compass,
    Lightning,
    Gauge,
    Shield,
    Target,
    Scale,
    Plus,
    Trash,
    Copy,
    Refresh,
    Pulse,
    Caret,
    Close,
    Up,
    Down,
    Fold,
    Unfold,
    Braces,
    Rows,
    Contrast,
    Fit,
    Layers,
    Eye,
}

impl Icon {
    /// SVG inner path/shape markup on a 24x24 viewBox (`stroke-width="1.5"`).
    pub const fn body(self) -> &'static str {
        match self {
            Self::Cube => {
                r#"<path d="M12 3l8 4.5v9L12 21l-8-4.5v-9L12 3z"/><path d="M12 12l8-4.5M12 12v9M12 12L4 7.5"/>"#
            }
            Self::Diamond => r#"<path d="M12 3l8.5 9L12 21 3.5 12 12 3z"/><circle cx="12" cy="12" r="2"/>"#,
            Self::Tree => {
                r#"<circle cx="6" cy="6" r="2.2"/><circle cx="18" cy="10" r="2.2"/><circle cx="18" cy="18" r="2.2"/><path d="M6 8.2V18h9.8M6 10h9.8"/>"#
            }
            Self::Sliders => {
                r#"<path d="M4 7h10M18 7h2M4 12h4M12 12h8M4 17h12M20 17h0"/><circle cx="16" cy="7" r="2"/><circle cx="10" cy="12" r="2"/><circle cx="18" cy="17" r="2"/>"#
            }
            Self::Graph => {
                r#"<circle cx="6" cy="7" r="2.4"/><circle cx="18" cy="6" r="2.4"/><circle cx="16" cy="18" r="2.4"/><circle cx="6.5" cy="17.5" r="2"/><path d="M8.3 6.8l7.3-.6M7.6 8.8l6.7 7.4M17.4 8.3l-1 7.3M8.5 17.6l5.1.3"/>"#
            }
            Self::Table => {
                r#"<rect x="3.5" y="4.5" width="17" height="15" rx="2"/><path d="M3.5 9.5h17M3.5 14.5h17M9.5 9.5v10"/>"#
            }
            Self::Funnel => r#"<path d="M4 5.5h16l-6.2 7.2v5.3l-3.6 1.5v-6.8L4 5.5z"/>"#,
            Self::Code => r#"<path d="M8.5 7.5L4 12l4.5 4.5M15.5 7.5L20 12l-4.5 4.5M13.5 5.5l-3 13"/>"#,
            Self::Search => r#"<circle cx="10.5" cy="10.5" r="6.5"/><path d="M15.5 15.5L20.5 20.5"/>"#,
            // Slot ID: crisp hexagon/key-slot badge (replaces verbose "slot" word)
            Self::Slot => r#"<path d="M9 4.5l-1.8 15M16.8 4.5L15 19.5M4.5 9.5h15.5M3.8 14.5h15.5"/>"#,
            // Generation counter: crisp cycle ring (replaces verbose "gen" word)
            Self::Gen => {
                r#"<path d="M19.5 12a7.5 7.5 0 1 1-2.2-5.3"/><path d="M19.5 4.5v4h-4"/>"#
            }
            // Parent hierarchy link (replaces verbose "Parent" word)
            Self::Parent => r#"<path d="M6 4.5v9.5a2 2 0 0 0 2 2h10.5"/><path d="M15 12.5l3.5 3.5-3.5 3.5"/>"#,
            // Tag label icon (replaces verbose "Tags" word and ugly `#` prefixes)
            Self::Tag => {
                r#"<path d="M4.5 11.2V5.5a1 1 0 0 1 1-1h5.7a1 1 0 0 1 .7.3l7.3 7.3a1.4 1.4 0 0 1 0 2l-5.1 5.1a1.4 1.4 0 0 1-2 0l-7.3-7.3a1 1 0 0 1-.3-.7z"/><circle cx="8.5" cy="8.5" r="1.1" fill="currentColor" stroke="none"/>"#
            }
            // IsA prefab inheritance link
            Self::Link => {
                r#"<path d="M10.5 13.5l3-3M8.2 15.8l-1.7 1.7a3.2 3.2 0 0 1-4.5-4.5l2.5-2.5a3.2 3.2 0 0 1 4.5 0"/><path d="M15.8 8.2l1.7-1.7a3.2 3.2 0 0 1 4.5 4.5l-2.5 2.5a3.2 3.2 0 0 1-4.5 0"/>"#
            }
            // Transform / Spatial axes
            Self::Compass => {
                r#"<path d="M12 3.5v17M3.5 12h17M9.5 6L12 3.5 14.5 6M18 9.5L20.5 12 18 14.5M6 9.5L3.5 12 6 14.5M9.5 18L12 20.5 14.5 18"/>"#
            }
            // Velocity / motion
            Self::Lightning => r#"<path d="M13.5 3L5.5 13.5h6l-1 7.5 8-10.5h-6l1-7.5z"/>"#,
            // Health / vitals
            Self::Gauge => r#"<path d="M3.5 12h4l2.2-5.5 4.2 11 2.4-5.5h4.2"/>"#,
            // Shield / armor
            Self::Shield => {
                r#"<path d="M12 3.5l7.5 2.8v5.4c0 4.6-3.1 7.8-7.5 9.3-4.4-1.5-7.5-4.7-7.5-9.3V6.3L12 3.5z"/>"#
            }
            // Weapon / target
            Self::Target => {
                r#"<circle cx="12" cy="12" r="7.5"/><circle cx="12" cy="12" r="3"/><path d="M12 2.5v3M12 18.5v3M2.5 12h3M18.5 12h3"/>"#
            }
            // Mass / weight
            Self::Scale => {
                r#"<path d="M7.5 9h9l2 10.5h-13L7.5 9z"/><path d="M9.5 9a2.5 2.5 0 0 1 5 0"/>"#
            }
            Self::Plus => r#"<path d="M12 5v14M5 12h14"/>"#,
            Self::Trash => {
                r#"<path d="M4.5 7h15M9.5 7V4.8a.8.8 0 0 1 .8-.8h3.4a.8.8 0 0 1 .8.8V7M6.5 7l1 12.2a1.3 1.3 0 0 0 1.3 1.3h6.4a1.3 1.3 0 0 0 1.3-1.3L17.5 7M10 11v5.5M14 11v5.5"/>"#
            }
            Self::Copy => {
                r#"<rect x="8.5" y="8.5" width="11" height="11" rx="2"/><path d="M15.5 8.5V6a1.5 1.5 0 0 0-1.5-1.5H6A1.5 1.5 0 0 0 4.5 6v8A1.5 1.5 0 0 0 6 15.5h2.5"/>"#
            }
            Self::Refresh => {
                r#"<path d="M19.5 12a7.5 7.5 0 1 1-2.2-5.3L19.5 9"/><path d="M19.5 4.5V9H15"/>"#
            }
            Self::Pulse => {
                r#"<circle cx="12" cy="12" r="2.5" fill="currentColor" stroke="none"/><path d="M7.8 7.8a6 6 0 0 0 0 8.4M16.2 7.8a6 6 0 0 1 0 8.4M5 5a10 10 0 0 0 0 14M19 5a10 10 0 0 1 0 14"/>"#
            }
            Self::Caret => r#"<path d="M6.5 9.5L12 15l5.5-5.5"/>"#,
            Self::Close => r#"<path d="M6.5 6.5l11 11M17.5 6.5l-11 11"/>"#,
            Self::Up => r#"<path d="M6 15l6-6 6 6"/>"#,
            Self::Down => r#"<path d="M6 9l6 6 6-6"/>"#,
            Self::Fold => r#"<path d="M7.5 16.5L12 12l4.5 4.5M7.5 7.5L12 12l4.5-4.5"/>"#,
            Self::Unfold => r#"<path d="M7.5 9.5L12 5l4.5 4.5M7.5 14.5L12 19l4.5-4.5"/>"#,
            Self::Braces => {
                r#"<path d="M8.5 5c-1.7 0-2.5.8-2.5 2.5v2.3c0 1.2-.6 2.2-2 2.2 1.4 0 2 1 2 2.2v2.3c0 1.7.8 2.5 2.5 2.5M15.5 5c1.7 0 2.5.8 2.5 2.5v2.3c0 1.2.6 2.2 2 2.2-1.4 0-2 1-2 2.2v2.3c0 1.7-.8 2.5-2.5 2.5"/>"#
            }
            Self::Rows => {
                r#"<path d="M4 7h16M4 12h16M4 17h10"/>"#
            }
            Self::Contrast => {
                r#"<circle cx="12" cy="12" r="8"/><path d="M12 4a8 8 0 0 1 0 16V4z" fill="currentColor" stroke="none"/>"#
            }
            Self::Fit => {
                r#"<path d="M4 9V5.5A1.5 1.5 0 0 1 5.5 4H9M15 4h3.5A1.5 1.5 0 0 1 20 5.5V9M20 15v3.5a1.5 1.5 0 0 1-1.5 1.5H15M9 20H5.5A1.5 1.5 0 0 1 4 18.5V15"/>"#
            }
            Self::Layers => {
                r#"<path d="M12 3.5l8.5 4.7L12 13 3.5 8.2 12 3.5z"/><path d="M3.5 12.2L12 17l8.5-4.8M3.5 16.2L12 21l8.5-4.8"/>"#
            }
            Self::Eye => {
                r#"<path d="M2.5 12s3.5-6.5 9.5-6.5S21.5 12 21.5 12s-3.5 6.5-9.5 6.5S2.5 12 2.5 12z"/><circle cx="12" cy="12" r="2.5"/>"#
            }
        }
    }
}

/// Write an inline `<svg class="ico ...">` into `out`.
pub fn write_icon<W: core::fmt::Write>(out: &mut W, icon: Icon, size: u8, extra_cls: &str) {
    let _ = write!(
        out,
        r#"<svg class="ico{}" width="{}" height="{}" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.55" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">{}</svg>"#,
        extra_cls,
        size,
        size,
        icon.body()
    );
}

/// Pick a crisp geometric icon from a component's schema shape (purely structural, domain-agnostic).
pub fn schema_icon(field_count: usize, numeric_count: usize, bool_count: usize, opaque: bool) -> Icon {
    if opaque || field_count == 0 {
        Icon::Cube
    } else if field_count == 1 {
        Icon::Gauge
    } else if numeric_count == field_count && (2..=4).contains(&field_count) {
        Icon::Compass
    } else if bool_count > 0 {
        Icon::Sliders
    } else {
        Icon::Layers
    }
}
