//! The terminfo entry ghostty answers XTGETTCAP from (terminfo/ghostty.zig, ghostty 73534c468),
//! with `TN`, `Co` and `RGB` added as Source.zig:71 `xtgettcapMap` adds them. Values are kept
//! in terminfo source form, as ghostty writes them; [`reply_value`] turns one into the bytes
//! an XTGETTCAP reply carries, by ghostty's rule (Source.zig:88): a string with `%` is sent
//! raw; any other has `\E` made ESC and a leading `^X` made its control byte (`^?` is DEL).
//! Nothing else is decoded, so `cr` answers the two bytes `\r`, as ghostty's does.
//!
//! The table is sorted by name (bytes), so a lookup halves; upper-case hex keeps that order, so
//! a request is decoded and searched, never re-encoded per row.

/// A capability's value.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cap {
    /// Present: the reply has no `=`.
    Bool,
    /// A number, answered in decimal.
    Num(u32),
    /// A string in terminfo source form.
    Str(&'static str),
}

/// The names of the entry; `TN` answers the first.
pub const NAMES: [&str; 3] = ["xterm-ghostty", "ghostty", "Ghostty"];

/// The longest name in [`GHOSTTY`], in bytes.
pub const MAX_NAME: usize = 7;

/// Every capability ghostty answers, sorted by name.
pub static GHOSTTY: [(&str, Cap); 271] = [
    ("AX", Cap::Bool),
    ("BD", Cap::Str(r"\E[?2004l")),
    ("BE", Cap::Str(r"\E[?2004h")),
    ("Clmg", Cap::Str(r"\E[s")),
    ("Cmg", Cap::Str(r"\E[%i%p1%d;%p2%ds")),
    ("Co", Cap::Num(256)),
    ("Dsmg", Cap::Str(r"\E[?69l")),
    ("E3", Cap::Str(r"\E[3J")),
    ("Enmg", Cap::Str(r"\E[?69h")),
    ("Ms", Cap::Str(r"\E]52;%p1%s;%p2%s\007")),
    ("PE", Cap::Str(r"\E[201~")),
    ("PS", Cap::Str(r"\E[200~")),
    ("RGB", Cap::Num(8)),
    ("RV", Cap::Str(r"\E[>c")),
    ("Se", Cap::Str(r"\E[0 q")),
    ("Setulc", Cap::Str(r"\E[58:2::%p1%{65536}%/%d:%p1%{256}%/%{255}%&%d:%p1%{255}%&%d%;m")),
    ("Smulx", Cap::Str(r"\E[4:%p1%dm")),
    ("Ss", Cap::Str(r"\E[%p1%d q")),
    ("Su", Cap::Bool),
    ("Sync", Cap::Str(r"\E[?2026%?%p1%{1}%-%tl%eh%;")),
    ("TN", Cap::Str(r"xterm-ghostty")),
    ("Tc", Cap::Bool),
    ("XM", Cap::Str(r"\E[?1006;1000%?%p1%{1}%=%th%el%;")),
    ("XR", Cap::Str(r"\E[>0q")),
    ("XT", Cap::Bool),
    ("acsc", Cap::Str(r"++\,\,--..00``aaffgghhiijjkkllmmnnooppqqrrssttuuvvwwxxyyzz{{||}}~~")),
    ("am", Cap::Bool),
    ("bce", Cap::Bool),
    ("bel", Cap::Str(r"^G")),
    ("blink", Cap::Str(r"\E[5m")),
    ("bold", Cap::Str(r"\E[1m")),
    ("cbt", Cap::Str(r"\E[Z")),
    ("ccc", Cap::Bool),
    ("civis", Cap::Str(r"\E[?25l")),
    ("clear", Cap::Str(r"\E[H\E[2J")),
    ("cnorm", Cap::Str(r"\E[?12l\E[?25h")),
    ("colors", Cap::Num(256)),
    ("cols", Cap::Num(80)),
    ("cr", Cap::Str(r"\r")),
    ("csr", Cap::Str(r"\E[%i%p1%d;%p2%dr")),
    ("cub", Cap::Str(r"\E[%p1%dD")),
    ("cub1", Cap::Str(r"^H")),
    ("cud", Cap::Str(r"\E[%p1%dB")),
    ("cud1", Cap::Str(r"^J")),
    ("cuf", Cap::Str(r"\E[%p1%dC")),
    ("cuf1", Cap::Str(r"\E[C")),
    ("cup", Cap::Str(r"\E[%i%p1%d;%p2%dH")),
    ("cuu", Cap::Str(r"\E[%p1%dA")),
    ("cuu1", Cap::Str(r"\E[A")),
    ("cvvis", Cap::Str(r"\E[?12;25h")),
    ("dch", Cap::Str(r"\E[%p1%dP")),
    ("dch1", Cap::Str(r"\E[P")),
    ("dim", Cap::Str(r"\E[2m")),
    ("dl", Cap::Str(r"\E[%p1%dM")),
    ("dl1", Cap::Str(r"\E[M")),
    ("dsl", Cap::Str(r"\E]2;\007")),
    ("ech", Cap::Str(r"\E[%p1%dX")),
    ("ed", Cap::Str(r"\E[J")),
    ("el", Cap::Str(r"\E[K")),
    ("el1", Cap::Str(r"\E[1K")),
    ("fd", Cap::Str(r"\E[?1004l")),
    ("fe", Cap::Str(r"\E[?1004h")),
    ("flash", Cap::Str(r"\E[?5h$<100/>\E[?5l")),
    ("fsl", Cap::Str(r"^G")),
    ("fullkbd", Cap::Bool),
    ("home", Cap::Str(r"\E[H")),
    ("hpa", Cap::Str(r"\E[%i%p1%dG")),
    ("hs", Cap::Bool),
    ("ht", Cap::Str(r"^I")),
    ("hts", Cap::Str(r"\EH")),
    ("ich", Cap::Str(r"\E[%p1%d@")),
    ("ich1", Cap::Str(r"\E[@")),
    ("il", Cap::Str(r"\E[%p1%dL")),
    ("il1", Cap::Str(r"\E[L")),
    ("ind", Cap::Str(r"\n")),
    ("indn", Cap::Str(r"\E[%p1%dS")),
    (
        "initc",
        Cap::Str(
            r"\E]4;%p1%d;rgb\:%p2%{255}%*%{1000}%/%2.2X/%p3%{255}%*%{1000}%/%2.2X/%p4%{255}%*%{1000}%/%2.2X\E\\",
        ),
    ),
    ("invis", Cap::Str(r"\E[8m")),
    ("it", Cap::Num(8)),
    ("kDC", Cap::Str(r"\E[3;2~")),
    ("kDC3", Cap::Str(r"\E[3;3~")),
    ("kDC4", Cap::Str(r"\E[3;4~")),
    ("kDC5", Cap::Str(r"\E[3;5~")),
    ("kDC6", Cap::Str(r"\E[3;6~")),
    ("kDC7", Cap::Str(r"\E[3;7~")),
    ("kDN", Cap::Str(r"\E[1;2B")),
    ("kDN3", Cap::Str(r"\E[1;3B")),
    ("kDN4", Cap::Str(r"\E[1;4B")),
    ("kDN5", Cap::Str(r"\E[1;5B")),
    ("kDN6", Cap::Str(r"\E[1;6B")),
    ("kDN7", Cap::Str(r"\E[1;7B")),
    ("kEND", Cap::Str(r"\E[1;2F")),
    ("kEND3", Cap::Str(r"\E[1;3F")),
    ("kEND4", Cap::Str(r"\E[1;4F")),
    ("kEND5", Cap::Str(r"\E[1;5F")),
    ("kEND6", Cap::Str(r"\E[1;6F")),
    ("kEND7", Cap::Str(r"\E[1;7F")),
    ("kHOM", Cap::Str(r"\E[1;2H")),
    ("kHOM3", Cap::Str(r"\E[1;3H")),
    ("kHOM4", Cap::Str(r"\E[1;4H")),
    ("kHOM5", Cap::Str(r"\E[1;5H")),
    ("kHOM6", Cap::Str(r"\E[1;6H")),
    ("kHOM7", Cap::Str(r"\E[1;7H")),
    ("kIC", Cap::Str(r"\E[2;2~")),
    ("kIC3", Cap::Str(r"\E[2;3~")),
    ("kIC4", Cap::Str(r"\E[2;4~")),
    ("kIC5", Cap::Str(r"\E[2;5~")),
    ("kIC6", Cap::Str(r"\E[2;6~")),
    ("kIC7", Cap::Str(r"\E[2;7~")),
    ("kLFT", Cap::Str(r"\E[1;2D")),
    ("kLFT3", Cap::Str(r"\E[1;3D")),
    ("kLFT4", Cap::Str(r"\E[1;4D")),
    ("kLFT5", Cap::Str(r"\E[1;5D")),
    ("kLFT6", Cap::Str(r"\E[1;6D")),
    ("kLFT7", Cap::Str(r"\E[1;7D")),
    ("kNXT", Cap::Str(r"\E[6;2~")),
    ("kNXT3", Cap::Str(r"\E[6;3~")),
    ("kNXT4", Cap::Str(r"\E[6;4~")),
    ("kNXT5", Cap::Str(r"\E[6;5~")),
    ("kNXT6", Cap::Str(r"\E[6;6~")),
    ("kNXT7", Cap::Str(r"\E[6;7~")),
    ("kPRV", Cap::Str(r"\E[5;2~")),
    ("kPRV3", Cap::Str(r"\E[5;3~")),
    ("kPRV4", Cap::Str(r"\E[5;4~")),
    ("kPRV5", Cap::Str(r"\E[5;5~")),
    ("kPRV6", Cap::Str(r"\E[5;6~")),
    ("kPRV7", Cap::Str(r"\E[5;7~")),
    ("kRIT", Cap::Str(r"\E[1;2C")),
    ("kRIT3", Cap::Str(r"\E[1;3C")),
    ("kRIT4", Cap::Str(r"\E[1;4C")),
    ("kRIT5", Cap::Str(r"\E[1;5C")),
    ("kRIT6", Cap::Str(r"\E[1;6C")),
    ("kRIT7", Cap::Str(r"\E[1;7C")),
    ("kUP", Cap::Str(r"\E[1;2A")),
    ("kUP3", Cap::Str(r"\E[1;3A")),
    ("kUP4", Cap::Str(r"\E[1;4A")),
    ("kUP5", Cap::Str(r"\E[1;5A")),
    ("kUP6", Cap::Str(r"\E[1;6A")),
    ("kUP7", Cap::Str(r"\E[1;7A")),
    ("kbs", Cap::Str(r"^?")),
    ("kcbt", Cap::Str(r"\E[Z")),
    ("kcub1", Cap::Str(r"\EOD")),
    ("kcud1", Cap::Str(r"\EOB")),
    ("kcuf1", Cap::Str(r"\EOC")),
    ("kcuu1", Cap::Str(r"\EOA")),
    ("kdch1", Cap::Str(r"\E[3~")),
    ("kend", Cap::Str(r"\EOF")),
    ("kent", Cap::Str(r"\EOM")),
    ("kf1", Cap::Str(r"\EOP")),
    ("kf10", Cap::Str(r"\E[21~")),
    ("kf11", Cap::Str(r"\E[23~")),
    ("kf12", Cap::Str(r"\E[24~")),
    ("kf13", Cap::Str(r"\E[1;2P")),
    ("kf14", Cap::Str(r"\E[1;2Q")),
    ("kf15", Cap::Str(r"\E[1;2R")),
    ("kf16", Cap::Str(r"\E[1;2S")),
    ("kf17", Cap::Str(r"\E[15;2~")),
    ("kf18", Cap::Str(r"\E[17;2~")),
    ("kf19", Cap::Str(r"\E[18;2~")),
    ("kf2", Cap::Str(r"\EOQ")),
    ("kf20", Cap::Str(r"\E[19;2~")),
    ("kf21", Cap::Str(r"\E[20;2~")),
    ("kf22", Cap::Str(r"\E[21;2~")),
    ("kf23", Cap::Str(r"\E[23;2~")),
    ("kf24", Cap::Str(r"\E[24;2~")),
    ("kf25", Cap::Str(r"\E[1;5P")),
    ("kf26", Cap::Str(r"\E[1;5Q")),
    ("kf27", Cap::Str(r"\E[1;5R")),
    ("kf28", Cap::Str(r"\E[1;5S")),
    ("kf29", Cap::Str(r"\E[15;5~")),
    ("kf3", Cap::Str(r"\EOR")),
    ("kf30", Cap::Str(r"\E[17;5~")),
    ("kf31", Cap::Str(r"\E[18;5~")),
    ("kf32", Cap::Str(r"\E[19;5~")),
    ("kf33", Cap::Str(r"\E[20;5~")),
    ("kf34", Cap::Str(r"\E[21;5~")),
    ("kf35", Cap::Str(r"\E[23;5~")),
    ("kf36", Cap::Str(r"\E[24;5~")),
    ("kf37", Cap::Str(r"\E[1;6P")),
    ("kf38", Cap::Str(r"\E[1;6Q")),
    ("kf39", Cap::Str(r"\E[1;6R")),
    ("kf4", Cap::Str(r"\EOS")),
    ("kf40", Cap::Str(r"\E[1;6S")),
    ("kf41", Cap::Str(r"\E[15;6~")),
    ("kf42", Cap::Str(r"\E[17;6~")),
    ("kf43", Cap::Str(r"\E[18;6~")),
    ("kf44", Cap::Str(r"\E[19;6~")),
    ("kf45", Cap::Str(r"\E[20;6~")),
    ("kf46", Cap::Str(r"\E[21;6~")),
    ("kf47", Cap::Str(r"\E[23;6~")),
    ("kf48", Cap::Str(r"\E[24;6~")),
    ("kf49", Cap::Str(r"\E[1;3P")),
    ("kf5", Cap::Str(r"\E[15~")),
    ("kf50", Cap::Str(r"\E[1;3Q")),
    ("kf51", Cap::Str(r"\E[1;3R")),
    ("kf52", Cap::Str(r"\E[1;3S")),
    ("kf53", Cap::Str(r"\E[15;3~")),
    ("kf54", Cap::Str(r"\E[17;3~")),
    ("kf55", Cap::Str(r"\E[18;3~")),
    ("kf56", Cap::Str(r"\E[19;3~")),
    ("kf57", Cap::Str(r"\E[20;3~")),
    ("kf58", Cap::Str(r"\E[21;3~")),
    ("kf59", Cap::Str(r"\E[23;3~")),
    ("kf6", Cap::Str(r"\E[17~")),
    ("kf60", Cap::Str(r"\E[24;3~")),
    ("kf61", Cap::Str(r"\E[1;4P")),
    ("kf62", Cap::Str(r"\E[1;4Q")),
    ("kf63", Cap::Str(r"\E[1;4R")),
    ("kf7", Cap::Str(r"\E[18~")),
    ("kf8", Cap::Str(r"\E[19~")),
    ("kf9", Cap::Str(r"\E[20~")),
    ("khome", Cap::Str(r"\EOH")),
    ("kich1", Cap::Str(r"\E[2~")),
    ("kind", Cap::Str(r"\E[1;2B")),
    ("km", Cap::Bool),
    ("kmous", Cap::Str(r"\E[<")),
    ("knp", Cap::Str(r"\E[6~")),
    ("kpp", Cap::Str(r"\E[5~")),
    ("kri", Cap::Str(r"\E[1;2A")),
    ("kxIN", Cap::Str(r"\E[I")),
    ("kxOUT", Cap::Str(r"\E[O")),
    ("lines", Cap::Num(24)),
    ("mc5i", Cap::Bool),
    ("mir", Cap::Bool),
    ("msgr", Cap::Bool),
    ("npc", Cap::Bool),
    ("oc", Cap::Str(r"\E]104\007")),
    ("op", Cap::Str(r"\E[39;49m")),
    ("pairs", Cap::Num(32767)),
    ("rc", Cap::Str(r"\E8")),
    ("rep", Cap::Str(r"%p1%c\E[%p2%{1}%-%db")),
    ("rev", Cap::Str(r"\E[7m")),
    ("ri", Cap::Str(r"\EM")),
    ("rin", Cap::Str(r"\E[%p1%dT")),
    ("ritm", Cap::Str(r"\E[23m")),
    ("rmacs", Cap::Str(r"\E(B")),
    ("rmam", Cap::Str(r"\E[?7l")),
    ("rmcup", Cap::Str(r"\E[?1049l")),
    ("rmir", Cap::Str(r"\E[4l")),
    ("rmkx", Cap::Str(r"\E[?1l\E>")),
    ("rmso", Cap::Str(r"\E[27m")),
    ("rmul", Cap::Str(r"\E[24m")),
    ("rmxx", Cap::Str(r"\E[29m")),
    ("rs1", Cap::Str(r"\E]\E\\\Ec")),
    ("rv", Cap::Str(r"\E\\[[0-9]+;[0-9]+;[0-9]+c")),
    ("sc", Cap::Str(r"\E7")),
    ("setab", Cap::Str(r"\E[%?%p1%{8}%<%t4%p1%d%e%p1%{16}%<%t10%p1%{8}%-%d%e48;5;%p1%d%;m")),
    ("setaf", Cap::Str(r"\E[%?%p1%{8}%<%t3%p1%d%e%p1%{16}%<%t9%p1%{8}%-%d%e38;5;%p1%d%;m")),
    ("setrgbb", Cap::Str(r"\E[48:2:%p1%d:%p2%d:%p3%dm")),
    ("setrgbf", Cap::Str(r"\E[38:2:%p1%d:%p2%d:%p3%dm")),
    (
        "sgr",
        Cap::Str(
            r"%?%p9%t\E(0%e\E(B%;\E[0%?%p6%t;1%;%?%p5%t;2%;%?%p2%t;4%;%?%p1%p3%|%t;7%;%?%p4%t;5%;%?%p7%t;8%;m",
        ),
    ),
    ("sgr0", Cap::Str(r"\E(B\E[m")),
    ("sitm", Cap::Str(r"\E[3m")),
    ("smacs", Cap::Str(r"\E(0")),
    ("smam", Cap::Str(r"\E[?7h")),
    ("smcup", Cap::Str(r"\E[?1049h")),
    ("smir", Cap::Str(r"\E[4h")),
    ("smkx", Cap::Str(r"\E[?1h\E=")),
    ("smso", Cap::Str(r"\E[7m")),
    ("smul", Cap::Str(r"\E[4m")),
    ("smxx", Cap::Str(r"\E[9m")),
    ("tbc", Cap::Str(r"\E[3g")),
    ("tsl", Cap::Str(r"\E]2;")),
    ("u6", Cap::Str(r"\E[%i%d;%dR")),
    ("u7", Cap::Str(r"\E[6n")),
    ("u8", Cap::Str(r"\E[?%[;0123456789]c")),
    ("u9", Cap::Str(r"\E[c")),
    ("vpa", Cap::Str(r"\E[%i%p1%dd")),
    ("xenl", Cap::Bool),
    ("xm", Cap::Str(r"\E[<%i%p3%d;%p1%d;%p2%d;%?%p4%tM%em%;")),
    ("xr", Cap::Str(r"\EP>\\|[ -~]+a\E\\")),
];

/// The capability named `name`.
pub fn lookup(name: &[u8]) -> Option<&'static (&'static str, Cap)> {
    GHOSTTY.binary_search_by(|(n, _)| n.as_bytes().cmp(name)).ok().map(|i| &GHOSTTY[i])
}

/// The capability whose name's upper-case hex is `hex` (XTGETTCAP's key, already upper-cased by
/// the DCS handler as ghostty's is, dcs.zig:183). Anything that is not upper-case hex of a known
/// name is `None`: ghostty's map is keyed by that exact text.
pub fn lookup_hex(hex: &[u8]) -> Option<&'static (&'static str, Cap)> {
    if hex.len() % 2 != 0 || hex.len() > 2 * MAX_NAME {
        return None;
    }
    let mut name = [0u8; MAX_NAME];
    for (i, pair) in hex.as_chunks::<2>().0.iter().enumerate() {
        name[i] = (upper_hex_digit(pair[0])? << 4) | upper_hex_digit(pair[1])?;
    }
    lookup(&name[..hex.len() / 2])
}

fn upper_hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// The bytes a reply carries for a string value, each passed to `put` (Source.zig:88-115).
pub fn reply_value(s: &str, mut put: impl FnMut(u8)) {
    let s = s.as_bytes();
    if s.contains(&b'%') {
        s.iter().for_each(|&b| put(b));
        return;
    }
    let mut i = 0;
    // A leading `^X` (ghostty refuses at compile time a `^` anywhere else; the table test holds
    // that none is there).
    if s.len() >= 2 && s[0] == b'^' {
        put(if s[1] == b'?' { 0x7F } else { s[1].wrapping_sub(64) });
        i = 2;
    }
    while i < s.len() {
        if s[i] == b'\\' && s.get(i + 1) == Some(&b'E') {
            put(0x1B);
            i += 2;
        } else {
            put(s[i]);
            i += 1;
        }
    }
}
