//! User-editable settings: `~/.github-trayicon/config.txt`.
//!
//! One `key=value` per line, `#` comments and blank lines ignored — the same tolerant shape
//! `access_token.rs` already uses for `client_id.txt`, so there is nothing new to explain to
//! someone who has already edited that file. Missing file, missing key, or an unrecognised value
//! are all just "use the default": a config file is a convenience, not something startup should
//! ever fail over.

use std::path::Path;

const CONFIG_FILE: &str = "config.txt";

/// Settings read from `config.txt`. New fields should default to whatever preserves today's
/// behavior for someone who never creates the file.
pub struct Config {
    /// Off by default: notifications need their own credential (see `access_token`), and the
    /// core of the app is PR status now, not notifications. Someone who wants the notification
    /// half back opts in explicitly.
    pub notifications: bool,
    /// Whether to check GitHub for newer releases once a day. On unless explicitly turned off.
    pub update_check: bool,
}

impl Config {
    /// Reads `config.txt` from the app's asset directory. Never fails — an absent or malformed
    /// file just means every setting takes its default.
    pub fn load(app_asset_path: &Path) -> Self {
        let path = app_asset_path.join(CONFIG_FILE);
        let content = std::fs::read_to_string(&path).unwrap_or_default();
        let values = parse(&content);

        Config {
            notifications: values.get("notifications").is_some_and(|v| is_on(v)),
            // Default **on**, unlike every other setting here, which deliberately departs from the
            // "preserve today's behaviour for someone who never creates the file" rule stated above.
            // An auto-update mechanism that is off until you find out it exists does not do the job it
            // was asked to do. `is_on` cannot express a default-on flag, hence `is_off` for the opt-out.
            update_check: !values.get("update_check").is_some_and(|v| is_off(v)),
        }
    }
}

/// Whether a value reads as "off".
///
/// The mirror of `is_on`, needed because a default-on setting cannot be expressed with `is_on`: absent
/// has to mean on, so only an explicit off may turn it off. Deliberately not `!is_on(v)` — that would
/// make a typo like `update_check=yse` read as off, silently disabling a feature the user was trying to
/// confirm. An unrecognised value leaves the default alone.
fn is_off(value: &str) -> bool {
    matches!(value.trim().to_ascii_lowercase().as_str(), "off" | "false" | "0" | "no")
}

/// Parses `key=value` lines into a lookup, tolerating comments, blank lines, and stray
/// whitespace. Later duplicate keys win, matching how most `key=value` config formats behave.
fn parse(content: &str) -> std::collections::HashMap<&str, &str> {
    content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| line.split_once('='))
        .map(|(k, v)| (k.trim(), v.trim()))
        .collect()
}

/// Whether a value means "on". Only a recognised affirmative counts — an unrecognised value
/// (typo, wrong casing convention) must read as "off", the safe default, not silently as "on".
fn is_on(value: &str) -> bool {
    matches!(value.to_ascii_lowercase().as_str(), "on" | "true" | "1" | "yes")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_on_values_case_insensitively() {
        for v in ["on", "On", "TRUE", "1", "yes"] {
            assert!(is_on(v), "{v:?} should read as on");
        }
    }

    #[test]
    fn unrecognised_values_read_as_off() {
        for v in ["off", "false", "0", "no", "", "enabled", "onn"] {
            assert!(!is_on(v), "{v:?} should read as off");
        }
    }

    #[test]
    fn comments_and_blank_lines_are_skipped() {
        let content = "# comment\n\nnotifications=on\n  # indented comment\n";
        let values = parse(content);
        assert_eq!(values.get("notifications"), Some(&"on"));
    }

    #[test]
    fn whitespace_around_key_and_value_is_trimmed() {
        let values = parse("  notifications = on  \n");
        assert_eq!(values.get("notifications"), Some(&"on"));
    }

    #[test]
    fn a_missing_key_is_absent_not_a_default_guess() {
        assert_eq!(parse("").get("notifications"), None);
        assert_eq!(parse("other=on").get("notifications"), None);
    }

    #[test]
    fn a_line_with_no_equals_sign_is_ignored_rather_than_panicking() {
        let values = parse("notifications\nnotifications=on\n");
        assert_eq!(values.get("notifications"), Some(&"on"));
    }
}
