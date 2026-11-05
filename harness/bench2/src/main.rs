use regex_automata::meta::Regex;

fn main() {
    for pattern in [r"a+b*", r"(?i)foo|bar\d{2,5}", r"[a-z&&[^aeiou]]+\b", r"\p{Greek}+", r"(unclosed"] {
        match regex_syntax::Parser::new().parse(pattern) {
            Ok(hir) => println!("{pattern} => {hir} / {:?}", hir.properties().minimum_len()),
            Err(err) => println!("{pattern} => error: {err}"),
        }
    }
    let re = Regex::new(r"(?P<y>\d{4})-(?P<m>\d{2})|qu[a-z]+k").unwrap();
    let hay = "on 2026-10 the quick brown fox, 1999-12";
    for m in re.find_iter(hay) {
        show(m.range(), &hay[m.range()]);
    }
    tally(hay, &re);
}

const LIMIT: usize = 3;
static GREETING: &str = "matches";

macro_rules! label {
    ($value:expr) => {
        format!("<{}>", $value)
    };
}

trait Describe {
    fn describe(&self) -> String {
        String::from("thing")
    }
}

struct Hit(usize);

impl Describe for Hit {}

fn tally(hay: &str, re: &Regex) {
    let count = re.find_iter(hay).take(LIMIT).count();
    let twice = (0..count).map(|n| n * 2).sum::<usize>();
    println!("{GREETING}: {} {twice} {}", label!(count), Hit(count).describe());
    println!("{} {:?}", include_str!("motto.txt").trim(), include_bytes!("blob.bin"));
}

fn show<T: std::fmt::Debug>(value: T, text: &str) {
    let line = |value: &T| format!("{value:?} {text}");
    println!("{}", line(&value));
}
