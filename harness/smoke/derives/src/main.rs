use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use std::num::ParseIntError;

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error("not a number: {0}")]
    Parse(#[from] ParseIntError),
    #[error("{count} is more than {max}")]
    TooMany { count: u32, max: u32 },
}

#[derive(Parser, Debug)]
#[command(name = "derives", version = "1.2.3", about = "proc-macro derives, built by blaze")]
struct Args {
    #[arg(short, long, default_value_t = 1)]
    count: u32,
    #[arg(long, value_enum, default_value_t = Shout::No)]
    shout: Shout,
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, Debug, PartialEq, ValueEnum)]
enum Shout {
    No,
    Yes,
}

#[derive(Subcommand, Debug)]
enum Command {
    Greet { name: String },
}

fn parse(text: &str, max: u32) -> Result<u32, Error> {
    let count: u32 = text.parse()?;
    if count > max {
        return Err(Error::TooMany { count, max });
    }
    Ok(count)
}

fn shown(result: Result<u32, Error>) -> String {
    match result {
        Ok(n) => format!("Ok({n})"),
        Err(error) => error.to_string(),
    }
}

fn main() {
    let args = Args::parse();
    let Command::Greet { name } = &args.command;
    for _ in 0..args.count {
        let line = format!("hello, {name}");
        println!("{}", if args.shout == Shout::Yes { line.to_uppercase() } else { line });
    }
    for text in ["7", "70", "x"] {
        println!("parse {text}: {}", shown(parse(text, 10)));
    }
    println!("version {}", Args::command().get_version().unwrap());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_is_valid() {
        Args::command().debug_assert();
    }

    #[test]
    fn defaults() {
        let args = Args::try_parse_from(["derives", "greet", "x"]).unwrap();
        assert_eq!((args.count, args.shout), (1, Shout::No));
        assert!(Args::try_parse_from(["derives", "--count", "x", "greet", "y"]).is_err());
    }

    #[test]
    fn errors() {
        assert!(matches!(parse("x", 1), Err(Error::Parse(_))));
        assert_eq!(parse("3", 2).unwrap_err().to_string(), "3 is more than 2");
    }
}
