//! Operator-facing rendering for a Rust error and its source chain.
#![expect(clippy::print_stderr, reason = "`main` prints the error chain on stderr for CLIs")]

/// Render `error: source: deeper source` without internal type or backtrace
/// noise. This is suitable for terminal errors and language-binding exceptions.
#[must_use]
pub fn format(error: &(dyn std::error::Error + 'static)) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    message
}

/// Run a CLI body, render any chained error with its command prefix, and
/// return the conventional process exit code.
pub fn main<E>(prefix: &str, run: impl FnOnce() -> Result<(), E>) -> std::process::ExitCode
where
    E: std::error::Error + 'static,
{
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{prefix}: {}", format(&error));
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::format;

    #[derive(Debug)]
    struct Layer {
        message: &'static str,
        source: Option<Box<Self>>,
    }

    impl std::fmt::Display for Layer {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str(self.message)
        }
    }

    impl std::error::Error for Layer {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            self.source.as_deref().map(|source| source as _)
        }
    }

    #[test]
    fn renders_every_source_in_order() {
        let error = Layer {
            message: "outer",
            source: Some(Box::new(Layer {
                message: "middle",
                source: Some(Box::new(Layer { message: "leaf", source: None })),
            })),
        };

        assert_eq!(format(&error), "outer: middle: leaf");
    }
}
