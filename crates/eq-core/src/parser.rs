//! Parse a raw EQ log line into its message body.
//!
//! EQ log format: `[Wed Jul 05 16:17:20 2026] <message>`
//!
//! This runs on EVERY appended line, so it allocates nothing: `LogLine` borrows
//! from the raw input. The embedded timestamp is deliberately not parsed —
//! triggers match on the message body, and timers start from the wall clock
//! when a line is read, so a per-line chrono parse (two format attempts) was
//! pure overhead that no consumer ever looked at.

/// One log line, borrowing from the raw input.
#[derive(Debug, Clone, Copy)]
pub struct LogLine<'a> {
    /// Everything after `] ` — this is what triggers match against.
    pub message: &'a str,
}

/// Parse one line. Always succeeds; a line with no recognizable `[...]` stamp
/// becomes an all-message `LogLine`.
pub fn parse_line(raw: &str) -> LogLine<'_> {
    let raw = raw.trim_end();

    if let Some(rest) = raw.strip_prefix('[') {
        if let Some(close) = rest.find(']') {
            return LogLine { message: rest[close + 1..].trim_start() };
        }
    }

    LogLine { message: raw }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_standard_eq_line() {
        let l = parse_line("[Wed Jul 05 16:17:20 2026] You begin casting Complete Heal.");
        assert_eq!(l.message, "You begin casting Complete Heal.");
    }

    #[test]
    fn handles_crlf_and_trailing_space() {
        let l = parse_line("[Wed Jul 05 16:17:20 2026] Your spell is interrupted.   \r");
        assert_eq!(l.message, "Your spell is interrupted.");
    }

    #[test]
    fn falls_back_when_no_timestamp() {
        let l = parse_line("a line with no stamp");
        assert_eq!(l.message, "a line with no stamp");
    }

    #[test]
    fn tolerates_garbage_timestamp() {
        let l = parse_line("[not a date] still has a message");
        assert_eq!(l.message, "still has a message");
    }
}
