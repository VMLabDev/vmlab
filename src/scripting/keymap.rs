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

/// X11 keysym for a canonical qcode (as produced by [`parse_chord`] /
/// [`char_keys`]). Used by the VNC input transport, which speaks keysyms
/// rather than QMP qcodes. Printable ASCII keysyms equal their codepoint.
pub fn keysym(qcode: &str) -> Result<u32, String> {
    let sym = match qcode {
        // Letters and digits: keysym == ASCII codepoint.
        s if s.len() == 1 && s.chars().next().unwrap().is_ascii_alphanumeric() => {
            s.chars().next().unwrap() as u32
        }
        // Punctuation qcodes → their ASCII character keysym.
        "spc" => 0x20,
        "minus" => 0x2d,
        "equal" => 0x3d,
        "bracket_left" => 0x5b,
        "bracket_right" => 0x5d,
        "semicolon" => 0x3b,
        "apostrophe" => 0x27,
        "grave_accent" => 0x60,
        "backslash" => 0x5c,
        "comma" => 0x2c,
        "dot" => 0x2e,
        "slash" => 0x2f,
        // Control / navigation (X11 0xff__ keysyms).
        "ret" => 0xff0d,
        "tab" => 0xff09,
        "esc" => 0xff1b,
        "backspace" => 0xff08,
        "delete" => 0xffff,
        "insert" => 0xff63,
        "up" => 0xff52,
        "down" => 0xff54,
        "left" => 0xff51,
        "right" => 0xff53,
        "home" => 0xff50,
        "end" => 0xff57,
        "pgup" => 0xff55,
        "pgdn" => 0xff56,
        "menu" => 0xff67,
        "print" => 0xff61,
        "pause" => 0xff13,
        "caps_lock" => 0xffe5,
        "num_lock" => 0xff7f,
        "scroll_lock" => 0xff14,
        // Modifiers.
        "ctrl" => 0xffe3,
        "alt" => 0xffe9,
        "shift" => 0xffe1,
        "meta_l" => 0xffe7,
        // Function keys F1..F12 → 0xffbe..0xffc9.
        s if s.starts_with('f')
            && s[1..]
                .parse::<u8>()
                .map(|n| (1..=12).contains(&n))
                .unwrap_or(false) =>
        {
            0xffbe + (s[1..].parse::<u32>().unwrap() - 1)
        }
        _ => return Err(format!("no keysym for qcode `{qcode}`")),
    };
    Ok(sym)
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

    #[test]
    fn keysyms() {
        assert_eq!(keysym("a").unwrap(), 0x61);
        assert_eq!(keysym("1").unwrap(), 0x31);
        assert_eq!(keysym("ret").unwrap(), 0xff0d);
        assert_eq!(keysym("right").unwrap(), 0xff53);
        assert_eq!(keysym("ctrl").unwrap(), 0xffe3);
        assert_eq!(keysym("spc").unwrap(), 0x20);
        assert_eq!(keysym("f1").unwrap(), 0xffbe);
        assert_eq!(keysym("f12").unwrap(), 0xffc9);
        assert_eq!(keysym("slash").unwrap(), 0x2f);
        assert!(keysym("bogus").is_err());
        // Every qcode parse_chord/char_keys can emit must have a keysym.
        for c in "abz09".chars() {
            assert!(keysym(&char_keys(c).unwrap()[0]).is_ok());
        }
    }
}
