//! s9-app-01: a small program of the kind tempo's crates are made of: a lexer over `chars`, a
//! recursive-descent parser borrowing a token slice and a variable map, an error enum with a
//! `Display` impl, `?` throughout, slice patterns (`[Tok::Ident(name), Tok::Eq, rest @ ..]`),
//! and a `HashMap` of variables printed sorted. Every line's answer follows from the grammar
//! (`expr := term (('+'|'-') term)*`, `term := factor (('*'|'/') factor)*`,
//! `factor := num | ident | '-' factor | '(' expr ')'`) and f64 arithmetic.

use std::collections::HashMap;
use std::fmt;

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Num(f64),
    Ident(String),
    Op(char),
    LParen,
    RParen,
    Eq,
}

#[derive(Debug)]
enum CalcError {
    BadChar(char, usize),
    Unexpected(String),
    Unknown(String),
    DivZero,
}

impl fmt::Display for CalcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CalcError::BadChar(c, at) => write!(f, "bad character {c:?} at {at}"),
            CalcError::Unexpected(what) => write!(f, "unexpected {what}"),
            CalcError::Unknown(name) => write!(f, "unknown name {name}"),
            CalcError::DivZero => write!(f, "division by zero"),
        }
    }
}

fn lex(s: &str) -> Result<Vec<Tok>, CalcError> {
    let cs: Vec<char> = s.chars().collect();
    let mut i = 0;
    let mut out = Vec::new();
    while i < cs.len() {
        let c = cs[i];
        if c.is_whitespace() {
            i += 1;
        } else if c.is_ascii_digit() || c == '.' {
            let start = i;
            while i < cs.len() && (cs[i].is_ascii_digit() || cs[i] == '.') {
                i += 1;
            }
            let text: String = cs[start..i].iter().collect();
            let n = text.parse().map_err(|_| CalcError::Unexpected(text.clone()))?;
            out.push(Tok::Num(n));
        } else if c.is_alphabetic() {
            let start = i;
            while i < cs.len() && cs[i].is_alphanumeric() {
                i += 1;
            }
            out.push(Tok::Ident(cs[start..i].iter().collect()));
        } else {
            out.push(match c {
                '+' | '-' | '*' | '/' => Tok::Op(c),
                '(' => Tok::LParen,
                ')' => Tok::RParen,
                '=' => Tok::Eq,
                _ => return Err(CalcError::BadChar(c, i)),
            });
            i += 1;
        }
    }
    Ok(out)
}

struct Parser<'a> {
    toks: &'a [Tok],
    pos: usize,
    vars: &'a HashMap<String, f64>,
}

impl Parser<'_> {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn next(&mut self) -> Option<Tok> {
        let t = self.toks.get(self.pos).cloned();
        self.pos += 1;
        t
    }

    fn expr(&mut self) -> Result<f64, CalcError> {
        let mut v = self.term()?;
        while let Some(Tok::Op(c @ ('+' | '-'))) = self.peek().cloned() {
            self.pos += 1;
            let r = self.term()?;
            if c == '+' { v += r } else { v -= r }
        }
        Ok(v)
    }

    fn term(&mut self) -> Result<f64, CalcError> {
        let mut v = self.factor()?;
        while let Some(Tok::Op(c @ ('*' | '/'))) = self.peek().cloned() {
            self.pos += 1;
            let r = self.factor()?;
            if c == '*' {
                v *= r;
            } else {
                if r == 0.0 {
                    return Err(CalcError::DivZero);
                }
                v /= r;
            }
        }
        Ok(v)
    }

    fn factor(&mut self) -> Result<f64, CalcError> {
        match self.next() {
            Some(Tok::Num(n)) => Ok(n),
            Some(Tok::Ident(name)) => self.vars.get(&name).copied().ok_or(CalcError::Unknown(name)),
            Some(Tok::Op('-')) => Ok(-self.factor()?),
            Some(Tok::LParen) => {
                let v = self.expr()?;
                match self.next() {
                    Some(Tok::RParen) => Ok(v),
                    other => Err(CalcError::Unexpected(format!("{other:?}"))),
                }
            }
            other => Err(CalcError::Unexpected(format!("{other:?}"))),
        }
    }
}

fn eval(toks: &[Tok], vars: &HashMap<String, f64>) -> Result<f64, CalcError> {
    let mut p = Parser { toks, pos: 0, vars };
    let v = p.expr()?;
    match p.peek() {
        None => Ok(v),
        Some(t) => Err(CalcError::Unexpected(format!("{t:?}"))),
    }
}

fn run(line: &str, vars: &mut HashMap<String, f64>) -> Result<String, CalcError> {
    let toks = lex(line)?;
    if let [Tok::Ident(name), Tok::Eq, rest @ ..] = toks.as_slice() {
        let v = eval(rest, vars)?;
        vars.insert(name.clone(), v);
        return Ok(format!("{name} = {v}"));
    }
    Ok(eval(&toks, vars)?.to_string())
}

fn main() {
    let mut vars = HashMap::new();
    let lines = [
        "1 + 2 * 3",
        "(1 + 2) * 3",
        "x = 10 / 4",
        "x * 2 - -1",
        "y",
        "2 / (x - 2.5)",
        "3 $ 4",
        "(1 + 2",
        "1 2",
        "0.1 + 0.2",
        "y = x * x",
    ];
    for line in lines {
        match run(line, &mut vars) {
            Ok(s) => println!("{line} => {s}"),
            Err(e) => println!("{line} => error: {e}"),
        }
    }
    let mut names: Vec<(&String, &f64)> = vars.iter().collect();
    names.sort_by(|a, b| a.0.cmp(b.0));
    println!("vars: {}", names.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(" "));
}
