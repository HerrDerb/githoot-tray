//! Attaches the Windows resource section: `VERSIONINFO`, an application manifest, and an icon.
//!
//! ## Why this file exists
//!
//! The shipped `githoot-tray.exe` was flagged by Windows Defender as
//! `Trojan:Win32/Bearfoos.B!ml`. That verdict was a confirmed false positive — the flagged bytes
//! hashed to exactly the digest in the release's minisign-signed `sha256sums.txt` — but `!ml` means
//! a machine-learning model, and the model scores features. The binary was carrying an unusually bad
//! set of them: **no** version resource, **no** manifest, **no** icon, no Authenticode signature, a
//! `windows_subsystem = "windows"` console-less process, and a self-updater that writes a new `.exe`
//! next to itself and renames it over the running one.
//!
//! This file addresses the first three, which are free. It deliberately does **not** pretend to fix
//! the verdict: the behavioural signals are untouched, and code signing remains the strong lever.
//! What it does buy is that the binary now identifies itself — to the model, to Explorer's Properties
//! dialog, to AppLocker rules, and to anyone auditing what is running in their tray.
//!
//! ## Host-gated, and why that is the right gate here
//!
//! `[target.'cfg(...)'.build-dependencies]` in `Cargo.toml` is evaluated against the **host**, not
//! the target — that is Cargo's documented behaviour for build dependencies, since the build script
//! runs on the host. So `winresource` and `image` only exist here when compiling *on* Windows, and
//! the `#[cfg(windows)]` below has to match that or the file would not compile. The extra
//! `CARGO_CFG_TARGET_OS` check then makes sure a Windows host cross-compiling *to* Linux does not
//! try to attach a PE resource to an ELF.
//!
//! The practical consequence: cross-compiling the Windows target from a Linux host silently produces
//! a resource-less binary. `.github/workflows/release.yml` builds the Windows target on a
//! `windows-latest` runner, so releases are covered, and `tests/version_resource.rs` fails loudly if
//! that ever stops being true.
//!
//! ## The version, and why `rerun-if-env-changed` is load-bearing
//!
//! `src/version.rs` prefers `GHT_VERSION` (the git tag, injected by CI) over `Cargo.toml`, and notes
//! that `option_env!` is folded into the crate fingerprint automatically so no build script is needed
//! for *that*. True for the Rust constant, not for this file: a build script is only re-run when
//! Cargo is told what it depends on. Without the line below, changing the tag would rebuild
//! `version::VERSION` while serving a cached resource from the previous tag, and the binary would
//! report two different versions depending on who asked.

fn main() {
    // Emitted unconditionally, on every host. If these lines were inside the Windows branch, a Linux
    // build would register no dependencies at all, and a later Windows build in the same tree could
    // reuse a stale script output.
    println!("cargo:rerun-if-env-changed=GHT_VERSION");
    println!("cargo:rerun-if-changed=assets/tray.png");
    println!("cargo:rerun-if-changed=windows/githoot-tray.manifest");

    #[cfg(windows)]
    windows_resource();
}

#[cfg(windows)]
fn windows_resource() {
    // Building *for* Windows, not merely *on* it.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let version = resolve_version();
    let (major, minor, patch) = parse_triple(&version);
    // VERSIONINFO's numeric fields are four 16-bit values packed into two DWORDs. The fourth is the
    // build number, and it is always zero: this project's tags are three-part, and inventing a
    // fourth component would make the resource claim something no tag can express.
    let packed = ((major as u64) << 48) | ((minor as u64) << 32) | ((patch as u64) << 16);

    let mut res = winresource::WindowsResource::new();
    res.set_version_info(winresource::VersionInfo::FILEVERSION, packed);
    res.set_version_info(winresource::VersionInfo::PRODUCTVERSION, packed);

    // The string block. Explorer shows these; so does every "what is this process" tool.
    res.set("CompanyName", "HerrDerb");
    res.set("ProductName", "githoot-tray");
    res.set(
        "FileDescription",
        "githoot-tray - GitHub pull request tray icon",
    );
    // Matches LICENSE, which is the Unlicense. Stating a real licence rather than a plausible
    // copyright line matters: this is the field a compliance scan reads.
    res.set("LegalCopyright", "Public domain (Unlicense)");
    res.set("OriginalFilename", "githoot-tray.exe");
    // Text form of the same numbers. Windows does not enforce that the two agree, and this is the one
    // a human sees, so it is derived from the same parse rather than typed separately.
    res.set("FileVersion", &format!("{major}.{minor}.{patch}.0"));
    res.set("ProductVersion", &format!("{major}.{minor}.{patch}.0"));

    res.set_manifest_file("windows/githoot-tray.manifest");

    let icon = generate_icon();
    // A failure here must not be fatal: the icon is cosmetic, while the manifest and VERSIONINFO are
    // the point of the exercise. Losing all three because a PNG could not be re-encoded would be the
    // wrong trade, and the warning is visible in the build log.
    match icon {
        Ok(path) => res.set_icon(&path),
        Err(e) => {
            println!("cargo:warning=could not generate the application icon: {e}");
            &mut res
        }
    };

    if let Err(e) = res.compile() {
        // Hard failure, unlike the icon. A release that silently loses its version resource is the
        // exact regression this whole file exists to prevent, and `rc.exe` not being found is the
        // realistic cause — which is a broken toolchain, not something to shrug at.
        panic!("could not compile the Windows resource section: {e}");
    }
}

/// The version this build should claim, resolved exactly as `src/version.rs` resolves it.
///
/// Kept in step by the assertion in `tests/version_resource.rs`, which compares the embedded number
/// against what the built binary prints for `--print-version`. Two independent copies of this rule
/// is not ideal, but the alternative is a shared module a build script and the crate can both see,
/// and `src/version.rs` is `const`-evaluated at compile time, so there is nothing to share.
#[cfg(windows)]
fn resolve_version() -> String {
    match std::env::var("GHT_VERSION") {
        Ok(v) if !v.trim().is_empty() => v.trim().trim_start_matches('v').to_string(),
        // `CARGO_PKG_VERSION` is always set for a build script, so this is the `Cargo.toml` fallback
        // a local build takes, matching `env!("CARGO_PKG_VERSION")` in `src/version.rs`.
        _ => std::env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION is always set by Cargo"),
    }
}

/// Splits `major.minor.patch` into numbers.
///
/// Panics rather than defaulting to `0.0.0`: a resource claiming version zero would pass every
/// "is the field populated" check while being a lie, and the tag is under this project's own control.
#[cfg(windows)]
fn parse_triple(v: &str) -> (u16, u16, u16) {
    let mut parts = v.split('.');
    let mut next = |what: &str| -> u16 {
        parts
            .next()
            .and_then(|p| p.parse().ok())
            .unwrap_or_else(|| panic!("version {v:?} has no numeric {what} component"))
    };
    (next("major"), next("minor"), next("patch"))
}

/// Re-encodes `assets/tray.png` as an `.ico` in `OUT_DIR` and returns its path.
///
/// Generated rather than committed because the README makes a point of `assets/` holding exactly two
/// files so the icon variants cannot drift apart. A third, hand-maintained `.ico` is precisely the
/// drift that argument is about.
///
/// 64x64 and square: the source is 98x96, and an `.ico` frame that is not square is legal but renders
/// badly at every size Explorer asks for.
#[cfg(windows)]
fn generate_icon() -> Result<String, Box<dyn std::error::Error>> {
    use image::ColorType;
    use image::ImageEncoder as _;
    use image::codecs::ico::IcoEncoder;

    let out_dir = std::env::var("OUT_DIR")?;
    let ico_path = std::path::Path::new(&out_dir).join("githoot-tray.ico");

    let src = image::load_from_memory(&std::fs::read("assets/tray.png")?)?;
    let square = src.resize_exact(64, 64, image::imageops::FilterType::Lanczos3);
    let rgba = square.to_rgba8();
    let (w, h) = rgba.dimensions();

    let file = std::fs::File::create(&ico_path)?;
    IcoEncoder::new(std::io::BufWriter::new(file)).write_image(
        rgba.as_raw(),
        w,
        h,
        ColorType::Rgba8,
    )?;

    Ok(ico_path.to_string_lossy().into_owned())
}
