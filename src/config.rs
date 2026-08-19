//! User-editable settings: `~/.github-trayicon/config.txt`.
//!
//! One `key=value` per line, `#` comments and blank lines ignored — the same tolerant shape
//! `access_token.rs` already uses for `client_id.txt`, so there is nothing new to explain to
//! someone who has already edited that file. Missing file, missing key, or an unrecognised value
//! are all just "use the default": a config file is a convenience, not something startup should
//! ever fail over.
//!
//! The file is **written on first run** with every setting at its default, so the settings are
//! discoverable by opening it rather than only by reading the README. An existing file is never
//! touched — not even to add a key it is missing.

use crate::log::Level;
use crate::{errorln, infoln};
use crate::state::PrAxis;
use std::path::Path;

const CONFIG_FILE: &str = "config.txt";

// ── Keys ──────────────────────────────────────────────────────────────────────
// Named constants rather than inline strings, because each appears three times: in the lookup, in
// the generated template, and in the round-trip test that proves those two agree.

const KEY_UPDATE_CHECK: &str = "updateCheck";
const KEY_NOTIFICATION_INDICATION: &str = "notificationIndication";
const KEY_REVIEW_REQUESTED: &str = "reviewRequested";
const KEY_READY_TO_MERGE: &str = "readyToMerge";
const KEY_CHANGES_REQUESTED: &str = "changesRequested";
const KEY_LOG_LEVEL: &str = "logLevel";

/// Keys that used to work, paired with what replaced them.
///
/// These are **not** aliases. The old spelling is not read, deliberately — that is the clean break.
/// They are listed only so `Config::load` can say so out loud, because the alternative is a feature
/// quietly changing behaviour on upgrade: an old `notifications=on` would stop being read and
/// notifications would go silent, and an old `update_check=off` would stop being read and the
/// updater someone deliberately disabled would switch back on.
const RENAMED_KEYS: [(&str, &str); 2] =
    [("notifications", KEY_NOTIFICATION_INDICATION), ("update_check", KEY_UPDATE_CHECK)];

/// Settings read from `config.txt`.
pub struct Config {
    /// Whether the blue "unread notifications" tint is drawn at all. Off by default: notifications
    /// need their own credential (see `access_token`), and the core of the app is PR status.
    pub notification_indication: bool,
    /// Whether to check GitHub for newer releases once a day. On unless explicitly turned off.
    pub update_check: bool,
    /// Whether each PR signal is wanted, indexed by `PrAxis::index`.
    ///
    /// Private, and read only through [`Config::pr_enabled`]. A public `[bool; 3]` invites a caller
    /// to build or destructure it in declaration order rather than axis order, and transposing two
    /// entries silently swaps which bar a setting controls — it compiles, type-checks, and is
    /// visible only by looking at the tray.
    pr_enabled: [bool; 3],
    /// How much detail the log file carries. `Error` by default — only failures — so the file stays
    /// quiet and readable; set `logLevel=info` to add the lifecycle narration when diagnosing.
    pub log_level: Level,
}

/// Where the settings file lives.
///
/// Public because the tray's Settings entry opens this exact path and then watches it for edits
/// (`settings_watch`). One definition, so the menu can never open a file the app does not read.
pub fn config_path(app_asset_path: &Path) -> std::path::PathBuf {
    app_asset_path.join(CONFIG_FILE)
}

impl Config {
    /// Reads `config.txt`, writing a default one first if there is none.
    ///
    /// Never fails. A file that cannot be written is logged and ignored; a file that cannot be read
    /// leaves every setting at its default.
    pub fn load(app_asset_path: &Path) -> Self {
        let path = config_path(app_asset_path);
        write_default_if_absent(&path);

        let content = std::fs::read_to_string(&path).unwrap_or_default();
        let values = parse(&content);
        warn_about_renamed_keys(&values);
        Self::from_values(&values)
    }

    /// The key-to-setting mapping, with no I/O.
    ///
    /// Split out purely so it can be tested: the whole risk in this file is a key being wired to the
    /// wrong setting, and `load` cannot be exercised without a filesystem. `parse` plus this is the
    /// entire behaviour of `load` bar reading the file.
    fn from_values(values: &std::collections::HashMap<&str, &str>) -> Self {
        Config {
            notification_indication: values
                .get(KEY_NOTIFICATION_INDICATION)
                .is_some_and(|v| is_on(v)),
            // Default **on**, unlike the notification tint, which deliberately departs from the
            // "preserve today's behaviour for someone who never creates the file" habit: an
            // auto-update mechanism that is off until you find out it exists does not do the job it
            // was asked to do. `is_on` cannot express a default-on flag, hence `is_off`.
            update_check: !values.get(KEY_UPDATE_CHECK).is_some_and(|v| is_off(v)),
            // Built by mapping over the axes rather than as a literal, so the axis name appears on
            // both sides of each pairing and the three cannot be written in the wrong order.
            pr_enabled: PrAxis::ALL.map(|axis| !values.get(pr_key(axis)).is_some_and(|v| is_off(v))),
            // Unrecognised (or absent) falls back to the quiet default, the same way a typo'd bool
            // does — see `Level::parse`.
            log_level: values.get(KEY_LOG_LEVEL).and_then(|v| Level::parse(v)).unwrap_or(Level::Error),
        }
    }

    /// Whether `axis`'s bar, menu entry and search are wanted at all.
    pub fn pr_enabled(&self, axis: PrAxis) -> bool {
        self.pr_enabled[axis.index()]
    }

    /// Whether any PR signal is wanted.
    ///
    /// With none of them enabled there is no reason to obtain a PR credential at all, which is what
    /// keeps a switched-off feature from making a network call or raising a sign-in dialog.
    pub fn any_pr_enabled(&self) -> bool {
        PrAxis::ALL.iter().any(|&axis| self.pr_enabled(axis))
    }
}

/// The config key for `axis`. The one place the mapping lives.
fn pr_key(axis: PrAxis) -> &'static str {
    match axis {
        PrAxis::ReviewRequested => KEY_REVIEW_REQUESTED,
        PrAxis::ReadyToMerge => KEY_READY_TO_MERGE,
        PrAxis::ChangesRequested => KEY_CHANGES_REQUESTED,
    }
}

/// The file written on first run: every setting, at its default, with a line explaining each.
///
/// Pure, and separated from the write so the round-trip test can prove the template and
/// [`Config::load`] agree without touching a disk. Values are *active* rather than commented out, so
/// the file states what is actually in force and editing one means changing a value.
fn default_config() -> String {
    format!(
        "# git-system-tray settings\n\
         #\n\
         # One key=value per line. Lines starting with # are ignored. Anything not recognised as an\n\
         # off value (off, false, 0, no) leaves the setting at its default, so a typo cannot silently\n\
         # switch something off.\n\
         #\n\
         # This file was created automatically with every setting at its default. It is never\n\
         # rewritten, so your edits are safe.\n\
         \n\
         # Check GitHub for a newer release once a day, and at startup.\n\
         {KEY_UPDATE_CHECK}=on\n\
         \n\
         # Tint the icon blue when you have unread GitHub notifications.\n\
         # Needs its own credential, which you will be asked for on the next start.\n\
         {KEY_NOTIFICATION_INDICATION}=off\n\
         \n\
         # The three pull-request signals, shown as coloured bars down the right of the icon.\n\
         # Turning one off removes its bar and its menu entry, and stops it being searched for.\n\
         {KEY_REVIEW_REQUESTED}=on\n\
         {KEY_READY_TO_MERGE}=on\n\
         {KEY_CHANGES_REQUESTED}=on\n\
         \n\
         # How much the log file records. \"error\" (the default) logs only failures; \"info\" adds\n\
         # the normal lifecycle detail (startup, sign-in, updates, each poll) for diagnosing.\n\
         {KEY_LOG_LEVEL}=error\n"
    )
}

/// Writes the default file if there is none.
///
/// Guarded on existence, **not** on the parsed settings being empty: a file holding nothing but
/// comments is a deliberate act, and overwriting it would throw away someone's notes.
fn write_default_if_absent(path: &Path) {
    if path.exists() {
        return;
    }
    match std::fs::write(path, default_config()) {
        Ok(()) => infoln!("wrote a default {} — every setting at its default", CONFIG_FILE),
        // Not fatal, and not worth a dialog: the defaults still apply, so the app behaves exactly as
        // it would have. Same treatment `access_token` gives a failed `client_id.txt` write.
        Err(e) => errorln!("could not write a default {CONFIG_FILE} ({e}) — using defaults"),
    }
}

/// Says so when the file still uses a key that has been renamed.
///
/// The old key genuinely has no effect. Without this the only symptom would be a feature quietly
/// behaving differently after an upgrade, which is the kind of thing people spend an hour on.
fn warn_about_renamed_keys(values: &std::collections::HashMap<&str, &str>) {
    for (old, new) in RENAMED_KEYS {
        if values.contains_key(old) {
            errorln!(
                "{CONFIG_FILE} uses \"{old}\", which is no longer read. Rename it to \"{new}\"."
            );
        }
    }
}

/// Whether a value reads as "off".
///
/// The mirror of `is_on`, needed because a default-on setting cannot be expressed with `is_on`: absent
/// has to mean on, so only an explicit off may turn it off. Deliberately not `!is_on(v)` — that would
/// make a typo like `updateCheck=yse` read as off, silently disabling a feature the user was trying to
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
    fn recognises_off_values_case_insensitively() {
        for v in ["off", "Off", "FALSE", "0", "no", " off "] {
            assert!(is_off(v), "{v:?} should read as off");
        }
    }

    /// The reason `is_off` is not `!is_on`. A default-on setting is turned off only by an explicit
    /// off value, so a typo has to leave it **on** — otherwise someone who mistyped while trying to
    /// confirm a setting would silently disable it.
    #[test]
    fn a_typo_does_not_read_as_off() {
        for v in ["yse", "onn", "enabled", "", "maybe", "1 "] {
            assert!(!is_off(v), "{v:?} must not read as off, or a typo disables the feature");
        }
    }

    #[test]
    fn comments_and_blank_lines_are_skipped() {
        let content = "# comment\n\nupdateCheck=on\n  # indented comment\n";
        let values = parse(content);
        assert_eq!(values.get("updateCheck"), Some(&"on"));
    }

    #[test]
    fn whitespace_around_key_and_value_is_trimmed() {
        let values = parse("  updateCheck = on  \n");
        assert_eq!(values.get("updateCheck"), Some(&"on"));
    }

    #[test]
    fn a_missing_key_is_absent_not_a_default_guess() {
        assert_eq!(parse("").get("updateCheck"), None);
        assert_eq!(parse("other=on").get("updateCheck"), None);
    }

    #[test]
    fn a_line_with_no_equals_sign_is_ignored_rather_than_panicking() {
        let values = parse("updateCheck\nupdateCheck=on\n");
        assert_eq!(values.get("updateCheck"), Some(&"on"));
    }

    // ── The generated file ──────────────────────────────────────────────────

    /// The template and the defaults must agree, and this is the only thing making that true: the
    /// file says `updateCheck=on` and the code defaults it to on, in two separate places that could
    /// drift. Parsing the template back and checking every key is what catches a drift.
    #[test]
    fn the_generated_file_states_exactly_the_documented_defaults() {
        // Bound first: `parse` borrows from its input, so the template has to outlive the map.
        let template = default_config();
        let values = parse(&template);

        assert_eq!(values.get(KEY_UPDATE_CHECK), Some(&"on"));
        assert_eq!(values.get(KEY_NOTIFICATION_INDICATION), Some(&"off"));
        assert_eq!(values.get(KEY_LOG_LEVEL), Some(&"error"));
        for axis in PrAxis::ALL {
            assert_eq!(values.get(pr_key(axis)), Some(&"on"), "{axis:?}");
        }
        // And nothing else, so a key added to the template without being read is caught.
        assert_eq!(values.len(), 6, "unexpected keys in the template: {values:?}");
    }

    /// Every key the template writes must be one `load` actually reads. A key present in the file
    /// but ignored by the code is worse than a missing one: it looks like it works.
    #[test]
    fn every_generated_key_is_one_that_is_read() {
        let known = [
            KEY_UPDATE_CHECK,
            KEY_NOTIFICATION_INDICATION,
            KEY_REVIEW_REQUESTED,
            KEY_READY_TO_MERGE,
            KEY_CHANGES_REQUESTED,
            KEY_LOG_LEVEL,
        ];
        let text = default_config();
        for key in parse(&text).keys() {
            assert!(known.contains(key), "{key:?} is written but never read");
        }
    }

    /// The template must survive its own comment stripping — a `#` in the wrong column, or a missing
    /// newline escape, would produce a file that parses to nothing while looking fine in the source.
    #[test]
    fn the_generated_file_is_mostly_comments_and_still_parses() {
        let text = default_config();
        assert!(text.starts_with('#'), "should open with an explanatory header");
        assert!(text.ends_with('\n'), "should end with a newline");
        assert!(text.lines().filter(|l| l.starts_with('#')).count() >= 8, "should explain itself");
    }

    // ── Renamed keys ────────────────────────────────────────────────────────

    /// The old names must not be read. This is the clean break, asserted rather than assumed: an
    /// accidentally reinstated alias would make the rename a no-op and the warning a lie.
    #[test]
    fn the_old_key_names_are_not_read() {
        let values = parse("notifications=on\nupdate_check=off\n");
        assert_eq!(values.get(KEY_NOTIFICATION_INDICATION), None);
        assert_eq!(values.get(KEY_UPDATE_CHECK), None);
        // …but they are recognised well enough to be warned about.
        for (old, _) in RENAMED_KEYS {
            assert!(values.contains_key(old), "{old:?} should be seen, just not obeyed");
        }
    }

    /// Each renamed key must point at a key that exists, or the warning would tell someone to use a
    /// name nothing reads.
    #[test]
    fn every_rename_points_at_a_real_key() {
        let text = default_config();
        let template = parse(&text);
        for (old, new) in RENAMED_KEYS {
            assert!(template.contains_key(new), "{old:?} points at {new:?}, which is not a real key");
        }
    }

    // ── Keys wired to the right settings ────────────────────────────────────
    //
    // The one real risk in this file: a key reaching the wrong setting. It compiles, type-checks, and
    // is visible only by watching the tray, so it is tested here rather than trusted.

    fn from(text: &str) -> Config {
        Config::from_values(&parse(text))
    }

    #[test]
    fn an_empty_file_gives_every_documented_default() {
        let cfg = from("");
        assert!(cfg.update_check, "updateCheck defaults on");
        assert!(!cfg.notification_indication, "notificationIndication defaults off");
        assert_eq!(cfg.log_level, Level::Error, "logLevel defaults to error");
        for axis in PrAxis::ALL {
            assert!(cfg.pr_enabled(axis), "{axis:?} defaults on");
        }
        assert!(cfg.any_pr_enabled());
    }

    /// The generated file must produce exactly the same `Config` as no file at all. This is what
    /// stops the template and the defaults drifting apart — and it compares the parsed *settings*,
    /// not the text, so it would catch a key written with the wrong value.
    #[test]
    fn the_generated_file_produces_the_same_settings_as_no_file() {
        let template = default_config();
        let generated = Config::from_values(&parse(&template));
        let defaults = from("");

        assert_eq!(generated.update_check, defaults.update_check);
        assert_eq!(generated.notification_indication, defaults.notification_indication);
        assert_eq!(generated.log_level, defaults.log_level);
        for axis in PrAxis::ALL {
            assert_eq!(generated.pr_enabled(axis), defaults.pr_enabled(axis), "{axis:?}");
        }
    }

    /// Turning off one PR key must disable exactly that axis. Checked for all three, because a
    /// transposed mapping would still pass if only one were tested.
    #[test]
    fn each_pr_key_disables_exactly_its_own_axis() {
        for target in PrAxis::ALL {
            let cfg = from(&format!("{}=off\n", pr_key(target)));
            for axis in PrAxis::ALL {
                let expected = axis != target;
                assert_eq!(
                    cfg.pr_enabled(axis),
                    expected,
                    "{}=off should disable {target:?} and nothing else, but {axis:?} is wrong",
                    pr_key(target)
                );
            }
        }
    }

    #[test]
    fn all_three_pr_keys_off_means_no_pr_signals_at_all() {
        let cfg = from("reviewRequested=off\nreadyToMerge=off\nchangesRequested=off\n");
        assert!(!cfg.any_pr_enabled(), "this is what skips the PR sign-in entirely");
        // …and the unrelated settings are untouched.
        assert!(cfg.update_check);
    }

    #[test]
    fn the_two_default_on_settings_are_turned_off_only_by_an_explicit_off() {
        assert!(!from("updateCheck=off\n").update_check);
        assert!(!from("readyToMerge=no\n").pr_enabled(PrAxis::ReadyToMerge));
        // A typo leaves them on, which is the whole reason `is_off` is not `!is_on`.
        assert!(from("updateCheck=yse\n").update_check);
        assert!(from("readyToMerge=onn\n").pr_enabled(PrAxis::ReadyToMerge));
    }

    #[test]
    fn the_default_off_setting_is_turned_on_only_by_a_recognised_yes() {
        assert!(from("notificationIndication=on\n").notification_indication);
        assert!(from("notificationIndication=TRUE\n").notification_indication);
        // A typo leaves it off, which is the safe direction for a setting that costs a sign-in.
        assert!(!from("notificationIndication=yse\n").notification_indication);
    }

    /// The clean break, asserted end to end rather than only at the parse layer: the old spellings
    /// must not reach the settings they used to control.
    #[test]
    fn the_old_key_names_no_longer_change_anything() {
        let cfg = from("notifications=on\nupdate_check=off\n");
        assert!(!cfg.notification_indication, "the old name must not switch notifications on");
        assert!(cfg.update_check, "the old name must not switch update checks off");
    }

    // ── Per-axis keys ───────────────────────────────────────────────────────

    /// The three axes must map to three *distinct* keys. Duplicating one would silently tie two bars
    /// to the same setting.
    #[test]
    fn each_axis_has_its_own_distinct_key() {
        let keys: Vec<&str> = PrAxis::ALL.iter().map(|&a| pr_key(a)).collect();
        let mut unique = keys.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), keys.len(), "duplicate axis keys: {keys:?}");
    }
}
