//! The version this binary believes it is, and how two versions compare.
//!
//! Split out from `update` because it answers a question the updater only *consumes*: the baseline to
//! compare a release against. Getting that baseline wrong makes every comparison wrong, in a way no
//! amount of care in the update logic can recover from, so it gets its own module and its own tests.
//!
//! ## Why not just `CARGO_PKG_VERSION`
//!
//! `.github/workflows/release.yml` says outright that the git tag is the source of truth for a release
//! and that `Cargo.toml`'s version is never consulted by the workflow. Nothing enforces that the two
//! agree — they have been kept in step by hand — so a binary built from a commit where the bump was
//! forgotten would ship as `v1.4.0` while believing it is `1.3.0`. It would then either offer an
//! update that is already installed, or, worse, never notice a real one.
//!
//! So CI passes the tag in through `GST_VERSION` and this module prefers it. A locally built binary
//! has no `GST_VERSION` and falls back to `Cargo.toml`. That asymmetry is deliberate: a local build is
//! not a release and has no tag to claim.

/// The version this binary reports, without a leading `v`.
///
/// `option_env!` is resolved at compile time, so there is no runtime cost and no way for the value to
/// be absent — the `match` picks the fallback while the constant is still being folded.
///
/// **This constant** needs no `rerun-if-env-changed` to be reliable: rustc records the environment
/// variables an `option_env!` reads, and Cargo folds them into the crate's fingerprint, so changing
/// `GST_VERSION` does force a rebuild. Verified by hand rather than assumed, because the failure it
/// would cause — a cached binary shipping a release under the previous version's name — is silent and
/// would only surface as an updater that never sees an update.
///
/// `build.rs` is a different matter and does need the directive. It exists now, and embeds this same
/// version into the Windows `VERSIONINFO` resource; a build script is only re-run when Cargo is told
/// what it depends on, so without `cargo:rerun-if-env-changed=GST_VERSION` a tag change would rebuild
/// this constant while serving a stale resource. `tests/version_resource.rs` asserts the two agree.
pub const VERSION: &str = match option_env!("GST_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

/// A parsed `major.minor.patch`.
///
/// Deliberately not a `semver` dependency. This project's tags are plain three-part numbers, and the
/// whole of what is needed is an ordering — `derive(PartialOrd, Ord)` on a tuple-shaped struct gives
/// exactly that, comparing major, then minor, then patch, for free.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl Version {
    /// Parses `1.2.3` or `v1.2.3`, returning `None` for anything else.
    ///
    /// Strict on purpose. This reads *release tags from the internet*, and a tag that does not look
    /// like a version must be skipped rather than guessed at — a lenient parser that treated
    /// `v2.0.0-rc1` as `2.0.0` would offer a release candidate as if it were final. Extra parts,
    /// missing parts, empty components and non-digits are all rejected.
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        let text = text.strip_prefix('v').unwrap_or(text);

        let mut parts = text.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        let patch = parts.next()?.parse().ok()?;
        // A fourth component means this is not the shape we know how to order.
        if parts.next().is_some() {
            return None;
        }

        Some(Version { major, minor, patch })
    }

    /// This binary's own version, or `None` if it somehow cannot be parsed.
    ///
    /// `None` is not reachable from a normal build — `CARGO_PKG_VERSION` is validated by Cargo, and CI
    /// sets `GST_VERSION` from a `v*` tag — but it is returned rather than panicked on because a tray
    /// app that refuses to start over a malformed version string would be trading a working icon for a
    /// feature the user can live without.
    pub fn current() -> Option<Self> {
        Self::parse(VERSION)
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_with_and_without_the_v_prefix() {
        let expected = Version { major: 1, minor: 2, patch: 3 };
        assert_eq!(Version::parse("1.2.3"), Some(expected));
        assert_eq!(Version::parse("v1.2.3"), Some(expected));
        assert_eq!(Version::parse("  v1.2.3  "), Some(expected));
    }

    /// The case a string comparison gets wrong, which is the whole reason this is parsed rather than
    /// compared as text: `"1.10.0" < "1.9.0"` lexicographically, because `'1' < '9'`.
    #[test]
    fn ten_is_newer_than_nine() {
        let ten = Version::parse("v1.10.0").expect("parses");
        let nine = Version::parse("v1.9.0").expect("parses");
        assert!(ten > nine, "1.10.0 must be newer than 1.9.0");
        assert!("1.10.0" < "1.9.0", "…which a string compare gets backwards");
    }

    #[test]
    fn orders_by_major_then_minor_then_patch() {
        let v = |s| Version::parse(s).expect("parses");
        assert!(v("2.0.0") > v("1.99.99"));
        assert!(v("1.3.0") > v("1.2.99"));
        assert!(v("1.2.4") > v("1.2.3"));
        assert_eq!(v("1.2.3"), v("v1.2.3"));
    }

    /// Anything that is not exactly three numbers is rejected rather than coerced. A pre-release tag
    /// is the case that matters: treating `2.0.0-rc1` as `2.0.0` would offer a release candidate as if
    /// it were final.
    #[test]
    fn rejects_anything_that_is_not_three_plain_numbers() {
        for bad in [
            "", "1", "1.2", "1.2.3.4", "1.2.x", "x.2.3", "1..3", "v", "1.2.-3", "1.2.3-rc1",
            "1.2.3+build", "latest", "release-1.2.3",
        ] {
            assert_eq!(Version::parse(bad), None, "{bad:?} must not parse");
        }
    }

    /// Whatever the build put in must itself be parseable, or the app could never compare against a
    /// release. This is the test that fails if `GST_VERSION` is ever set to something odd.
    #[test]
    fn the_baked_in_version_is_parseable() {
        assert!(Version::current().is_some(), "VERSION {VERSION:?} must parse");
    }

    #[test]
    fn display_round_trips_through_parse() {
        let v = Version::parse("v3.14.15").expect("parses");
        assert_eq!(v.to_string(), "3.14.15");
        assert_eq!(Version::parse(&v.to_string()), Some(v));
    }
}
