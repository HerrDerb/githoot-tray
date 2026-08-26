//! The release workflow must publish, and sign, the *legacy* asset names as well as the current ones.
//!
//! ## Why this test exists
//!
//! `git-system-tray` was renamed to `githoot-tray` in b84ccd8, and the release assets were renamed
//! with it. The self-updater's asset name is baked in at compile time (`ASSET` in `src/update.rs`),
//! so every copy installed before that commit asks for `git-system-tray.exe` and then looks that
//! name up in the signed `sha256sums.txt`. v1.9.0's manifest lists only the new names, so those
//! copies fail with "the signed sums file has no entry for git-system-tray.exe" — and, because a
//! published release is immutable, that release can never be repaired. Self-update was a one-way
//! door: the fleet is stranded until a *later* release carries both names.
//!
//! This is checked in the test suite rather than left to review because the failure is invisible
//! from inside the repository. Everything about a rename-only release looks correct; only an old
//! binary out in the world can tell that it broke, and by the time it says so the release is frozen.
//!
//! ## Sunset
//!
//! The shim exists for binaries built before the rename. Once no installed copy predates
//! `githoot-tray` naming (i.e. everyone has taken one release carrying both names), delete the
//! legacy uploads, the legacy hashes and this test together. Removing them while old copies remain
//! strands exactly the machines that were slowest to update.

/// Names an old, pre-rename binary asks for, per platform.
const LEGACY_ASSETS: [&str; 3] = [
    "git-system-tray",
    "git-system-tray.exe",
    "git-system-tray-macos-aarch64.zip",
];

fn workflow() -> String {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/.github/workflows/release.yml");
    std::fs::read_to_string(path).expect("release.yml must be readable")
}

/// Uploading a legacy-named copy is not enough on its own: the updater verifies the download against
/// the signed manifest, so an asset with no line in `sha256sums.txt` is refused just as hard as a
/// missing one. This asserts the name reaches the `sha256sum` invocation, which is what produces
/// that line.
#[test]
fn the_signed_manifest_covers_the_legacy_asset_names() {
    let yaml = workflow();
    let hash_step = yaml
        .split_once("- name: Hash them")
        .expect("release.yml must still have a `Hash them` step")
        .1
        .split_once("- uses:")
        .expect("the `Hash them` step must be followed by another step")
        .0;

    for asset in LEGACY_ASSETS {
        assert!(
            hash_step.contains(asset),
            "sha256sums.txt would not cover `{asset}`, so pre-rename copies cannot self-update"
        );
    }
}

/// And the bytes have to actually be on the release under that name, or the download 404s before
/// verification is ever reached.
#[test]
fn the_release_carries_the_legacy_asset_names() {
    let yaml = workflow();
    for asset in LEGACY_ASSETS {
        assert!(
            yaml.contains(asset),
            "`{asset}` is never uploaded, so pre-rename copies have nothing to download"
        );
    }
}
