//@ edition: 2021
pub fn longest<'a>(a: &'a str, b: &'a str) -> &'a str { if a.len() >= b.len() { a } else { b } }
pub struct Parser<'s> { rest: &'s str }
impl<'s> Parser<'s> {
    pub fn new(s: &'s str) -> Self { Parser { rest: s } }
    pub fn next_word(&mut self) -> Option<&'s str> {
        let t = self.rest.trim_start();
        if t.is_empty() { return None; }
        let end = t.find(' ').unwrap_or(t.len());
        let (w, r) = t.split_at(end);
        self.rest = r;
        Some(w)
    }
}
