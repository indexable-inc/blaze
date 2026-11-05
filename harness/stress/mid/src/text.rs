pub fn shout(s: &str) -> String {
    let mut out = s.to_uppercase();
    out.push('!');
    out
}

pub fn words(s: &str) -> Vec<&str> {
    s.split_whitespace().collect()
}

pub fn initials(s: &str) -> String {
    words(s).iter().filter_map(|w| w.chars().next()).collect()
}
