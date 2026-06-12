//! Keyboard chord and text → QMP `send-key` qcode translation (PRD §10.3).
//! US layout; chords use QMP sendkey naming with common aliases.

/// Parse a chord like `ctrl-alt-del` into qcodes for one `send-key` call.
pub fn parse_chord(chord: &str) -> Result<Vec<String>, String> {
    if chord.is_empty() {
        return Err("empty key chord".into());
    }
    chord.split('-').map(key_name).collect()
}

/// Canonical qcode for one key name (case-insensitive, aliases accepted).
pub fn key_name(name: &str) -> Result<String, String> {
    let lower = name.to_ascii_lowercase();
    let canonical = match lower.as_str() {
        // aliases → QMP qcodes
        "del" | "delete" => "delete",
        "esc" | "escape" => "esc",
        "enter" | "return" => "ret",
        "space" => "spc",
        "win" | "super" | "meta" => "meta_l",
        "ctrl" | "control" => "ctrl",
        "alt" => "alt",
        "shift" => "shift",
        "tab" => "tab",
        "backspace" => "backspace",
        "up" | "down" | "left" | "right" | "home" | "end" | "pgup" | "pgdn" | "insert" | "menu"
        | "print" | "pause" | "caps_lock" | "num_lock" | "scroll_lock" => {
            return Ok(lower);
        }
        "pageup" => "pgup",
        "pagedown" => "pgdn",
        s if s.len() == 1 && s.chars().next().unwrap().is_ascii_alphanumeric() => {
            return Ok(lower);
        }
        s if s.starts_with('f')
            && s[1..]
                .parse::<u8>()
                .map(|n| (1..=12).contains(&n))
                .unwrap_or(false) =>
        {
            return Ok(lower);
        }
        _ => return Err(format!("unknown key `{name}` in chord")),
    };
    Ok(canonical.to_string())
}

/// One typed character: qcodes pressed together (shift pairs handled).
pub fn char_keys(c: char) -> Result<Vec<String>, String> {
    let plain = |k: &str| Ok(vec![k.to_string()]);
    let shifted = |k: &str| Ok(vec!["shift".to_string(), k.to_string()]);
    match c {
        'a'..='z' | '0'..='9' => plain(&c.to_string()),
        'A'..='Z' => shifted(&c.to_ascii_lowercase().to_string()),
        ' ' => plain("spc"),
        '\n' => plain("ret"),
        '\t' => plain("tab"),
        '-' => plain("minus"),
        '=' => plain("equal"),
        '[' => plain("bracket_left"),
        ']' => plain("bracket_right"),
        ';' => plain("semicolon"),
        '\'' => plain("apostrophe"),
        '`' => plain("grave_accent"),
        '\\' => plain("backslash"),
        ',' => plain("comma"),
        '.' => plain("dot"),
        '/' => plain("slash"),
        '!' => shifted("1"),
        '@' => shifted("2"),
        '#' => shifted("3"),
        '$' => shifted("4"),
        '%' => shifted("5"),
        '^' => shifted("6"),
        '&' => shifted("7"),
        '*' => shifted("8"),
        '(' => shifted("9"),
        ')' => shifted("0"),
        '_' => shifted("minus"),
        '+' => shifted("equal"),
        '{' => shifted("bracket_left"),
        '}' => shifted("bracket_right"),
        ':' => shifted("semicolon"),
        '"' => shifted("apostrophe"),
        '~' => shifted("grave_accent"),
        '|' => shifted("backslash"),
        '<' => shifted("comma"),
        '>' => shifted("dot"),
        '?' => shifted("slash"),
        other => Err(format!("cannot type character {other:?} (US layout only)")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chords() {
        assert_eq!(
            parse_chord("ctrl-alt-del").unwrap(),
            vec!["ctrl", "alt", "delete"]
        );
        assert_eq!(parse_chord("Enter").unwrap(), vec!["ret"]);
        assert_eq!(parse_chord("f2").unwrap(), vec!["f2"]);
        assert!(parse_chord("ctrl-flurb").is_err());
    }

    #[test]
    fn typing() {
        assert_eq!(char_keys('a').unwrap(), vec!["a"]);
        assert_eq!(char_keys('A').unwrap(), vec!["shift", "a"]);
        assert_eq!(char_keys('!').unwrap(), vec!["shift", "1"]);
        assert_eq!(char_keys('\n').unwrap(), vec!["ret"]);
        assert!(char_keys('é').is_err());
    }
}
