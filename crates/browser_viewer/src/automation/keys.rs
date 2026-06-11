//! CP6: Playwright-style key spec → CDP `Input.dispatchKeyEvent` fields.
//!
//! Mirrors `@playwright/mcp`'s `browser_press_key` `key` argument: a single
//! key name (`Enter`, `ArrowDown`, `a`, `F5`) optionally prefixed with
//! `+`-joined modifiers (`Control+a`, `Shift+ArrowRight`). Modifier names
//! match Playwright (`Control`/`Ctrl`, `Shift`, `Alt`, `Meta`/`Cmd`).
//!
//! Windows virtual-key codes are inlined as integer literals (rather than
//! pulling in the `windows` crate) so this module stays portable and unit
//! testable. The values match `browser_view::keystroke_to_cdp`, which drives
//! the human-typing path through the same CDP method.

use anyhow::{Result, anyhow};

/// CDP modifier bitmask (matches `cdp_modifiers_mask` in `browser_view`):
/// Alt=1, Ctrl=2, Meta=4, Shift=8.
const MOD_ALT: i32 = 1;
const MOD_CTRL: i32 = 2;
const MOD_META: i32 = 4;
const MOD_SHIFT: i32 = 8;

/// A parsed key press, ready to feed to `WebView2Session::dispatch_key_event`.
#[derive(Debug, Clone, PartialEq)]
pub struct KeyPress {
    /// `KeyboardEvent.key` — the logical value (`"Enter"`, `"a"`, `"A"`).
    pub key: String,
    /// `KeyboardEvent.code` — the physical US-QWERTY code (`"Enter"`, `"KeyA"`).
    pub code: String,
    /// `windowsVirtualKeyCode` for the renderer.
    pub windows_virtual_key_code: i32,
    /// `text` — set only for keys that should produce an `input` event;
    /// `None` for non-printable keys and for any Ctrl/Alt combo.
    pub text: Option<String>,
    /// CDP modifier bitmask.
    pub modifiers: i32,
}

/// Parse a Playwright-style key spec into CDP dispatch fields.
///
/// Examples: `"Enter"`, `"Escape"`, `"ArrowLeft"`, `"a"`, `"A"`, `"F5"`,
/// `"Control+a"`, `"Shift+Tab"`.
pub fn parse_key(spec: &str) -> Result<KeyPress> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Err(anyhow!("empty key"));
    }

    // A bare "+" is the literal plus key, not a chord separator.
    if spec == "+" {
        return finish_parse("+", &[]);
    }

    let mut parts: Vec<&str> = spec.split('+').collect();
    // A trailing empty part means the key itself is "+": e.g. "Control++".
    let key_name = match parts.pop() {
        Some("") => "+",
        Some(name) => name,
        None => return Err(anyhow!("empty key")),
    };
    finish_parse(key_name, &parts)
}

fn finish_parse(key_name: &str, modifier_tokens: &[&str]) -> Result<KeyPress> {
    let mut modifiers = 0;
    let mut shift = false;
    for token in modifier_tokens {
        match token.to_ascii_lowercase().as_str() {
            "control" | "ctrl" | "ctl" => modifiers |= MOD_CTRL,
            "shift" => {
                modifiers |= MOD_SHIFT;
                shift = true;
            }
            "alt" | "option" => modifiers |= MOD_ALT,
            "meta" | "cmd" | "command" | "super" | "win" => modifiers |= MOD_META,
            "" => return Err(anyhow!("malformed key spec (empty modifier)")),
            other => return Err(anyhow!("unknown modifier {other:?}")),
        }
    }

    let (key, code, vk, mut text) =
        map_key(key_name, shift).ok_or_else(|| anyhow!("unsupported key {key_name:?}"))?;

    // Ctrl/Alt combos suppress the text payload so e.g. Ctrl+A doesn't also
    // dump "a" into a focused field (matches the human-typing path).
    if modifiers & (MOD_CTRL | MOD_ALT) != 0 {
        text = None;
    }

    Ok(KeyPress {
        key,
        code,
        windows_virtual_key_code: vk,
        text,
        modifiers,
    })
}

/// Map a single key name (no modifiers) to `(key, code, vk, text)`.
fn map_key(name: &str, shift: bool) -> Option<(String, String, i32, Option<String>)> {
    let lower = name.to_ascii_lowercase();

    let special: Option<(&str, &str, i32, Option<&str>)> = match lower.as_str() {
        // Enter must carry text "\r" so Chromium fires the `keypress`/`char`
        // event — that's what triggers implicit form submission and most JS
        // Enter handlers. (Matches Playwright's US keyboard layout.) Without
        // it, `keydown` fires but navigation/submit does not.
        "enter" | "return" => Some(("Enter", "Enter", 0x0D, Some("\r"))),
        "tab" => Some(("Tab", "Tab", 0x09, None)),
        "escape" | "esc" => Some(("Escape", "Escape", 0x1B, None)),
        "backspace" => Some(("Backspace", "Backspace", 0x08, None)),
        "delete" | "del" => Some(("Delete", "Delete", 0x2E, None)),
        "insert" => Some(("Insert", "Insert", 0x2D, None)),
        "space" => Some((" ", "Space", 0x20, Some(" "))),
        "arrowup" | "up" => Some(("ArrowUp", "ArrowUp", 0x26, None)),
        "arrowdown" | "down" => Some(("ArrowDown", "ArrowDown", 0x28, None)),
        "arrowleft" | "left" => Some(("ArrowLeft", "ArrowLeft", 0x25, None)),
        "arrowright" | "right" => Some(("ArrowRight", "ArrowRight", 0x27, None)),
        "home" => Some(("Home", "Home", 0x24, None)),
        "end" => Some(("End", "End", 0x23, None)),
        "pageup" => Some(("PageUp", "PageUp", 0x21, None)),
        "pagedown" => Some(("PageDown", "PageDown", 0x22, None)),
        _ => None,
    };
    if let Some((key, code, vk, text)) = special {
        return Some((
            key.to_string(),
            code.to_string(),
            vk,
            text.map(str::to_string),
        ));
    }

    // Function keys F1–F24.
    if let Some(digits) = lower.strip_prefix('f') {
        if let Ok(n) = digits.parse::<u8>() {
            if (1..=24).contains(&n) {
                let name = format!("F{n}");
                let vk = 0x70 + (n as i32 - 1); // VK_F1 = 0x70
                return Some((name.clone(), name, vk, None));
            }
        }
    }

    // Single printable character.
    let mut chars = name.chars();
    let first = chars.next()?;
    if chars.next().is_some() {
        return None; // multi-char name we don't recognize
    }
    map_char(first, shift)
}

fn map_char(ch: char, shift_held: bool) -> Option<(String, String, i32, Option<String>)> {
    if ch.is_ascii_alphabetic() {
        let upper = ch.to_ascii_uppercase();
        let shifted = shift_held || ch.is_ascii_uppercase();
        let logical = if shifted {
            upper.to_string()
        } else {
            ch.to_ascii_lowercase().to_string()
        };
        let code = format!("Key{upper}");
        let vk = upper as i32; // VK_A..VK_Z == uppercase ASCII
        return Some((logical.clone(), code, vk, Some(logical)));
    }
    if ch.is_ascii_digit() {
        let code = format!("Digit{ch}");
        let vk = ch as i32; // VK_0..VK_9 == ASCII '0'..'9'
        let s = ch.to_string();
        return Some((s.clone(), code, vk, Some(s)));
    }

    // Punctuation: (code, vk) from the US-QWERTY layout. `key`/`text` are the
    // literal character (shifted symbols are layout-dependent; we pass the
    // character through as-is).
    let (code, vk): (&str, i32) = match ch {
        '`' | '~' => ("Backquote", 0xC0),
        '-' | '_' => ("Minus", 0xBD),
        '=' | '+' => ("Equal", 0xBB),
        '[' | '{' => ("BracketLeft", 0xDB),
        ']' | '}' => ("BracketRight", 0xDD),
        '\\' | '|' => ("Backslash", 0xDC),
        ';' | ':' => ("Semicolon", 0xBA),
        '\'' | '"' => ("Quote", 0xDE),
        ',' | '<' => ("Comma", 0xBC),
        '.' | '>' => ("Period", 0xBE),
        '/' | '?' => ("Slash", 0xBF),
        ' ' => ("Space", 0x20),
        _ => ("", 0),
    };
    let s = ch.to_string();
    Some((s.clone(), code.to_string(), vk, Some(s)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enter_maps_to_return() {
        let k = parse_key("Enter").unwrap();
        assert_eq!(k.key, "Enter");
        assert_eq!(k.code, "Enter");
        assert_eq!(k.windows_virtual_key_code, 0x0D);
        assert_eq!(
            k.text.as_deref(),
            Some("\r"),
            "Enter needs text for keypress"
        );
        assert_eq!(k.modifiers, 0);
    }

    #[test]
    fn lowercase_letter_is_printable() {
        let k = parse_key("a").unwrap();
        assert_eq!(k.key, "a");
        assert_eq!(k.code, "KeyA");
        assert_eq!(k.windows_virtual_key_code, 'A' as i32);
        assert_eq!(k.text.as_deref(), Some("a"));
    }

    #[test]
    fn uppercase_letter_implies_shift_value() {
        let k = parse_key("A").unwrap();
        assert_eq!(k.key, "A");
        assert_eq!(k.text.as_deref(), Some("A"));
    }

    #[test]
    fn ctrl_combo_suppresses_text() {
        let k = parse_key("Control+a").unwrap();
        assert_eq!(k.modifiers, MOD_CTRL);
        assert_eq!(k.key, "a");
        assert_eq!(k.text, None, "ctrl combos must not emit text");
    }

    #[test]
    fn shift_tab_sets_modifier() {
        let k = parse_key("Shift+Tab").unwrap();
        assert_eq!(k.modifiers, MOD_SHIFT);
        assert_eq!(k.code, "Tab");
    }

    #[test]
    fn arrow_and_function_keys() {
        assert_eq!(parse_key("ArrowDown").unwrap().code, "ArrowDown");
        assert_eq!(parse_key("down").unwrap().key, "ArrowDown");
        let f5 = parse_key("F5").unwrap();
        assert_eq!(f5.code, "F5");
        assert_eq!(f5.windows_virtual_key_code, 0x74);
    }

    #[test]
    fn literal_plus_key() {
        let k = parse_key("+").unwrap();
        assert_eq!(k.key, "+");
        assert_eq!(k.code, "Equal");
    }

    #[test]
    fn unknown_modifier_errors() {
        assert!(parse_key("Hyper+a").is_err());
    }

    #[test]
    fn empty_errors() {
        assert!(parse_key("").is_err());
        assert!(parse_key("   ").is_err());
    }
}
