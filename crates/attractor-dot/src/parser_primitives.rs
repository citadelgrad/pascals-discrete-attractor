use std::time::Duration;

use winnow::ascii::{digit1, multispace0};
use winnow::combinator::{alt, opt, preceded, repeat};
use winnow::error::{ContextError, ErrMode};
use winnow::token::take_while;
use winnow::{ModalResult, Parser};

/// Whitespace consumer (including newlines).
pub(super) fn ws<'i>(input: &mut &'i str) -> ModalResult<&'i str> {
    multispace0.parse_next(input)
}

/// Parse an identifier: [A-Za-z_][A-Za-z0-9_]*
pub(super) fn identifier<'i>(input: &mut &'i str) -> ModalResult<&'i str> {
    (
        take_while(1, |c: char| c.is_ascii_alphabetic() || c == '_'),
        take_while(0.., |c: char| c.is_ascii_alphanumeric() || c == '_'),
    )
        .take()
        .parse_next(input)
}

/// Parse a qualified id: identifier ( '.' identifier )+  or plain identifier.
/// Returns the full dotted string.
pub(super) fn qualified_or_plain_id(input: &mut &str) -> ModalResult<String> {
    let first = identifier.parse_next(input)?;
    let rest: Vec<&str> = repeat(0.., preceded('.', identifier)).parse_next(input)?;
    if rest.is_empty() {
        Ok(first.to_string())
    } else {
        let mut s = first.to_string();
        for part in rest {
            s.push('.');
            s.push_str(part);
        }
        Ok(s)
    }
}

/// Parse a double-quoted string with escape support.
pub(super) fn quoted_string(input: &mut &str) -> ModalResult<String> {
    let _ = '"'.parse_next(input)?;
    let mut s = String::new();
    loop {
        let c = winnow::token::any.parse_next(input)?;
        match c {
            '"' => break,
            '\\' => {
                let esc = winnow::token::any.parse_next(input)?;
                match esc {
                    'n' => s.push('\n'),
                    't' => s.push('\t'),
                    '\\' => s.push('\\'),
                    '"' => s.push('"'),
                    other => {
                        s.push('\\');
                        s.push(other);
                    }
                }
            }
            other => s.push(other),
        }
    }
    Ok(s)
}

/// Parse a duration value: integer + suffix (ms, s, m, h, d).
pub(super) fn duration_value(input: &mut &str) -> ModalResult<Duration> {
    let digits: &str = digit1.parse_next(input)?;
    let val: u64 = digits
        .parse()
        .map_err(|_| ErrMode::Backtrack(ContextError::new()))?;
    let suffix = alt(("ms", "s", "m", "h", "d")).parse_next(input)?;
    let dur = match suffix {
        "ms" => Duration::from_millis(val),
        "s" => Duration::from_secs(val),
        "m" => Duration::from_secs(val * 60),
        "h" => Duration::from_secs(val * 3600),
        "d" => Duration::from_secs(val * 86400),
        _ => unreachable!(),
    };
    Ok(dur)
}

/// Parse a boolean value.
pub(super) fn boolean_value(input: &mut &str) -> ModalResult<bool> {
    use winnow::token::literal;
    alt((literal("true").value(true), literal("false").value(false))).parse_next(input)
}

/// Parse a float: optional sign, digits, '.', digits.
pub(super) fn float_value(input: &mut &str) -> ModalResult<f64> {
    let s: &str = (
        opt(alt(('-', '+'))),
        digit1,
        '.',
        digit1,
    )
        .take()
        .parse_next(input)?;
    s.parse()
        .map_err(|_| ErrMode::Backtrack(ContextError::new()))
}

/// Parse an integer: optional sign + digits.
pub(super) fn integer_value(input: &mut &str) -> ModalResult<i64> {
    let s: &str = (opt(alt(('-', '+'))), digit1)
        .take()
        .parse_next(input)?;
    s.parse()
        .map_err(|_| ErrMode::Backtrack(ContextError::new()))
}
