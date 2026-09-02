//! Lexer for the basil DSL: reduces raw source to logical lines.
//!
//! A logical line is one instruction: leading whitespace stripped, trailing
//! `\` line continuations joined, and full-line `#` comments skipped. Inline
//! comments are *not* stripped here — the parser and the identity code decide
//! what counts as significant content (the design excludes comments and
//! whitespace from the program hash, so the identity layer re-serializes
//! canonical instruction text itself).

/// One logical instruction line, with its 1-based starting source line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogicalLine {
    pub number: usize,
    pub text: String,
}

/// Split `source` into logical lines.
pub fn lex(source: &str) -> Vec<LogicalLine> {
    let mut out = Vec::new();
    let mut pending: Option<(usize, String)> = None;

    for (idx, raw) in source.lines().enumerate() {
        let line_number = idx + 1;
        let mut line = raw.trim_start().to_string();

        if line.is_empty() {
            continue;
        }

        // Skip full-line comments.
        if line.starts_with('#') {
            continue;
        }

        // Trailing backslash: line continuation. Join with the next line.
        let continues = line.ends_with('\\');
        if continues {
            line.pop(); // drop the backslash
        }

        match pending.take() {
            Some((start, mut acc)) => {
                acc.push_str(&line);
                if continues {
                    pending = Some((start, acc));
                } else {
                    out.push(LogicalLine {
                        number: start,
                        text: acc,
                    });
                }
            }
            None => {
                if continues {
                    pending = Some((line_number, line));
                } else {
                    out.push(LogicalLine {
                        number: line_number,
                        text: line,
                    });
                }
            }
        }
    }

    // Un-terminated continuation at EOF is a dangling backslash; surface it as
    // a line so the parser can report it.
    if let Some((start, acc)) = pending.take() {
        out.push(LogicalLine {
            number: start,
            text: acc,
        });
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: &[LogicalLine]) -> Vec<String> {
        lines.iter().map(|l| l.text.clone()).collect()
    }

    #[test]
    fn strips_comments_and_blanks() {
        let out = lex("# comment\n\nFROM alpine\n");
        assert_eq!(text(&out), vec!["FROM alpine".to_string()]);
    }

    #[test]
    fn joins_line_continuations() {
        let out = lex("RUN echo one && \\\n    echo two\n");
        // The trailing backslash is dropped and the continuation line's leading
        // whitespace is trimmed, so the join is deterministic.
        assert_eq!(text(&out), vec!["RUN echo one && echo two".to_string()]);
        assert_eq!(out[0].number, 1);
    }

    #[test]
    fn multiple_continuations() {
        let out = lex("RUN a && \\\n    b && \\\n    c\n");
        assert_eq!(text(&out), vec!["RUN a && b && c".to_string()]);
        assert_eq!(out[0].number, 1);
    }

    #[test]
    fn keeps_inline_comment_text() {
        let out = lex("RUN echo hi # trailing\n");
        assert_eq!(text(&out), vec!["RUN echo hi # trailing".to_string()]);
    }

    #[test]
    fn reports_source_line_number() {
        let out = lex("# c1\nFROM a\n# c2\nFROM b\n");
        assert_eq!(out[0].number, 2);
        assert_eq!(out[1].number, 4);
    }
}