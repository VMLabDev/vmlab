//! The `fmt` placeholder format-spec grammar, shared by the compiler
//! (compile-time validation of literal templates) and the VM (runtime
//! application) so the two can never disagree:
//!
//! ```text
//! {[:[[fill]align][0][width][.prec][type]]}
//! align: < ^ >      type: x X b o (int only)
//! ```
//!
//! Width/precision count characters (consistent with `pad_left`). The `0`
//! flag zero-pads numbers sign-aware. Precision means digits for floats
//! and truncation for strings.

/// Horizontal alignment inside `width`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FmtAlign {
    Left,
    Center,
    Right,
}

/// Integer base formats (`x X b o`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FmtNum {
    HexLower,
    HexUpper,
    Binary,
    Octal,
}

/// A parsed format spec (the text between `{:` and `}`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FmtSpec {
    pub fill: Option<char>,
    pub align: Option<FmtAlign>,
    pub zero: bool,
    pub width: Option<usize>,
    pub prec: Option<usize>,
    pub num: Option<FmtNum>,
}

fn align_of(c: char) -> Option<FmtAlign> {
    match c {
        '<' => Some(FmtAlign::Left),
        '^' => Some(FmtAlign::Center),
        '>' => Some(FmtAlign::Right),
        _ => None,
    }
}

/// Parse a spec string (without the `{:` `}` delimiters).
pub fn parse_spec(spec: &str) -> Result<FmtSpec, String> {
    let mut s = FmtSpec::default();
    let chars: Vec<char> = spec.chars().collect();
    let mut i = 0;
    if chars.len() >= 2 && align_of(chars[1]).is_some() {
        s.fill = Some(chars[0]);
        s.align = align_of(chars[1]);
        i = 2;
    } else if !chars.is_empty() && align_of(chars[0]).is_some() {
        s.align = align_of(chars[0]);
        i = 1;
    }
    if i < chars.len() && chars[i] == '0' {
        s.zero = true;
        i += 1;
    }
    let mut width = 0usize;
    let mut got_width = false;
    while i < chars.len() && chars[i].is_ascii_digit() {
        width = width * 10 + (chars[i] as usize - '0' as usize);
        got_width = true;
        i += 1;
        if width > 100_000 {
            return Err("format width too large".into());
        }
    }
    if got_width {
        s.width = Some(width);
    }
    if i < chars.len() && chars[i] == '.' {
        i += 1;
        let mut prec = 0usize;
        let mut got_prec = false;
        while i < chars.len() && chars[i].is_ascii_digit() {
            prec = prec * 10 + (chars[i] as usize - '0' as usize);
            got_prec = true;
            i += 1;
            if prec > 10_000 {
                return Err("format precision too large".into());
            }
        }
        if !got_prec {
            return Err("`.` in a format spec must be followed by a precision".into());
        }
        s.prec = Some(prec);
    }
    if i < chars.len() {
        match chars[i] {
            'x' => s.num = Some(FmtNum::HexLower),
            'X' => s.num = Some(FmtNum::HexUpper),
            'b' => s.num = Some(FmtNum::Binary),
            'o' => s.num = Some(FmtNum::Octal),
            c => {
                return Err(format!(
                    "unknown format type `{c}` (expected `x`, `X`, `b` or `o`)"
                ));
            }
        }
        i += 1;
    }
    if i < chars.len() {
        return Err(format!(
            "trailing characters after the format spec: `{}`",
            chars[i..].iter().collect::<String>()
        ));
    }
    Ok(s)
}

/// Scan a template: count placeholders (`{}` and `{:spec}`), validating
/// every spec. Returns the placeholder count or the first spec error.
/// `{{`/`}}` are escapes; any other braces are literal text.
pub fn analyze_template(template: &str) -> Result<usize, String> {
    let bytes = template.as_bytes();
    let mut count = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{' {
            if bytes.get(i + 1) == Some(&b'{') {
                i += 2;
                continue;
            }
            if bytes.get(i + 1) == Some(&b'}') {
                count += 1;
                i += 2;
                continue;
            }
            if bytes.get(i + 1) == Some(&b':') {
                let Some(end) = template[i + 2..].find('}') else {
                    return Err("unterminated `{:` format placeholder".into());
                };
                parse_spec(&template[i + 2..i + 2 + end])?;
                count += 1;
                i += 2 + end + 1;
                continue;
            }
        }
        i += 1;
    }
    Ok(count)
}

/// Apply a parsed spec to an already-rendered value string. The caller
/// pre-renders `plain` (so custom Display impls have already run) and
/// passes what kind of value it was for numeric-aware padding rules.
/// `num`-format rendering happens in the caller (it needs the raw int) —
/// here `plain` is already base-converted when `spec.num` is set.
pub fn pad_spec(spec: &FmtSpec, plain: String, numeric: bool) -> String {
    let Some(w) = spec.width else {
        return plain;
    };
    let len = plain.chars().count();
    if len >= w {
        return plain;
    }
    let pad = w - len;
    let zero_pad = spec.zero && spec.align.is_none() && numeric;
    let fill = if zero_pad {
        '0'
    } else {
        spec.fill.unwrap_or(' ')
    };
    let align = spec.align.unwrap_or(if numeric {
        FmtAlign::Right
    } else {
        FmtAlign::Left
    });
    let filler = |n: usize| fill.to_string().repeat(n);
    match align {
        _ if zero_pad => {
            // Sign-aware: -5 → -005, not 00-5.
            if let Some(rest) = plain.strip_prefix('-') {
                format!("-{}{rest}", filler(pad))
            } else {
                format!("{}{plain}", filler(pad))
            }
        }
        FmtAlign::Left => format!("{plain}{}", filler(pad)),
        FmtAlign::Right => format!("{}{plain}", filler(pad)),
        FmtAlign::Center => {
            let left = pad / 2;
            format!("{}{plain}{}", filler(left), filler(pad - left))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_forms() {
        assert_eq!(parse_spec("").unwrap(), FmtSpec::default());
        let s = parse_spec("*^10.2x");
        // `x` after precision is fine grammatically; semantic check is the
        // caller's (int vs float) — the parse succeeds.
        assert!(s.is_ok());
        let s = parse_spec(">8").unwrap();
        assert_eq!(s.align, Some(FmtAlign::Right));
        assert_eq!(s.width, Some(8));
        let s = parse_spec("08").unwrap();
        assert!(s.zero);
        assert_eq!(s.width, Some(8));
        assert_eq!(parse_spec(".2").unwrap().prec, Some(2));
        assert_eq!(parse_spec("X").unwrap().num, Some(FmtNum::HexUpper));
        assert!(parse_spec("q").is_err());
        assert!(parse_spec(".").is_err());
        assert!(parse_spec("8z").is_err());
    }

    #[test]
    fn analyze_counts_and_validates() {
        assert_eq!(analyze_template("no holes").unwrap(), 0);
        assert_eq!(analyze_template("{} and {:>8} and {{literal}}").unwrap(), 2);
        assert!(analyze_template("{:q}").is_err());
        assert!(analyze_template("{:>8").is_err());
        // Non-placeholder braces stay literal.
        assert_eq!(analyze_template("{not a hole}").unwrap(), 0);
    }

    #[test]
    fn padding() {
        let spec = parse_spec(">5").unwrap();
        assert_eq!(pad_spec(&spec, "ab".into(), false), "   ab");
        let spec = parse_spec("<5").unwrap();
        assert_eq!(pad_spec(&spec, "ab".into(), false), "ab   ");
        let spec = parse_spec("*^6").unwrap();
        assert_eq!(pad_spec(&spec, "ab".into(), false), "**ab**");
        let spec = parse_spec("05").unwrap();
        assert_eq!(pad_spec(&spec, "-5".into(), true), "-0005");
        assert_eq!(pad_spec(&spec, "42".into(), true), "00042");
        // Numbers default to right alignment, text to left.
        let spec = parse_spec("4").unwrap();
        assert_eq!(pad_spec(&spec, "7".into(), true), "   7");
        assert_eq!(pad_spec(&spec, "a".into(), false), "a   ");
    }
}
