//! The Windows `VERSIONINFO` resource has to exist, and has to agree with `--print-version`.
//!
//! ## Why this test exists at all
//!
//! A PE with no version resource, no company, no product and no icon is one of the features
//! Defender's machine-learning models weigh, and this binary was flagged as
//! `Trojan:Win32/Bearfoos.B!ml` while carrying none of them. Embedding the resource is not a
//! guarantee — the behavioural signals (unsigned, self-replacing, spawns its successor) are
//! untouched by it — but it removes a whole cluster of static ones for free.
//!
//! ## Why it is an integration test
//!
//! There is nothing to unit-test: the resource is not Rust, it is bytes the linker attaches. Only
//! the finished, linked artefact can answer whether it landed. `CARGO_BIN_EXE_githoot-tray` is
//! the path to exactly that artefact, and Cargo builds the binary before running this file.
//!
//! ## Why the version is checked, and not just "is the field non-empty"
//!
//! `src/version.rs` prefers `GHT_VERSION` over `Cargo.toml`, and `build.rs` has to resolve the
//! version the same way. If the two ever disagree the binary ships claiming one version in its
//! resource and another to the updater — silent, and exactly the class of failure `version.rs`
//! warns about. So the expected value is taken from the binary itself, by running the
//! `--print-version` the updater already relies on, rather than restated here as a literal.
#![cfg(windows)]

use std::os::windows::ffi::OsStrExt;
use winapi::ctypes::c_void;

const EXE: &str = env!("CARGO_BIN_EXE_githoot-tray");

/// The fixed, binary half of a version resource.
///
/// Declared here because `winapi` 0.3.9 does not ship it — it has `winver`'s three functions but not
/// `verrsrc.h`'s struct. The layout is thirteen `DWORD`s and has not changed since Win32, so this is
/// a transcription rather than a guess; only the four fields actually read are named usefully.
#[repr(C)]
struct VsFixedFileInfo {
    signature: u32,
    struct_version: u32,
    file_version_ms: u32,
    file_version_ls: u32,
    product_version_ms: u32,
    product_version_ls: u32,
    file_flags_mask: u32,
    file_flags: u32,
    file_os: u32,
    file_type: u32,
    file_subtype: u32,
    file_date_ms: u32,
    file_date_ls: u32,
}

/// `VS_FFI_SIGNATURE`. Checked, so a wrong pointer or a layout surprise fails loudly instead of
/// producing a plausible-looking version number out of unrelated bytes.
const VS_FFI_SIGNATURE: u32 = 0xFEEF_04BD;

/// A NUL-terminated UTF-16 buffer, which is what every `*W` entry point below wants.
fn wide(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// The whole version-info block for the built binary.
///
/// Asserts rather than returning an `Option`: a size of zero *is* the missing-resource case, and it
/// is the thing this file was written to catch.
fn version_block() -> Vec<u8> {
    use winapi::um::winver::{GetFileVersionInfoSizeW, GetFileVersionInfoW};

    let path = wide(EXE);
    // The second argument is a legacy out-param that has been ignored since Win32s; it still has to
    // be a valid pointer.
    let mut handle = 0u32;
    let size = unsafe { GetFileVersionInfoSizeW(path.as_ptr(), &mut handle) };
    assert!(
        size > 0,
        "{EXE} carries no VERSIONINFO resource (GetFileVersionInfoSizeW returned 0, last error {}). \
         That is the red state this test was written for: build.rs has not embedded the resource.",
        std::io::Error::last_os_error()
    );

    let mut buf = vec![0u8; size as usize];
    let ok = unsafe {
        GetFileVersionInfoW(path.as_ptr(), handle, size, buf.as_mut_ptr() as *mut c_void)
    };
    assert!(
        ok != 0,
        "GetFileVersionInfoW failed: {}",
        std::io::Error::last_os_error()
    );
    buf
}

/// One `VerQueryValueW` lookup, returning the sub-block pointer and its length.
///
/// Shared by both readers below because the call is identical apart from the key, and its out-param
/// is a `*mut c_void` that has to be passed by `&mut` — easy to get subtly wrong twice.
fn query(block: &[u8], key: &[u16]) -> Option<(*mut c_void, u32)> {
    use winapi::um::winver::VerQueryValueW;

    let mut ptr: *mut c_void = std::ptr::null_mut();
    let mut len = 0u32;
    let ok = unsafe {
        VerQueryValueW(
            block.as_ptr() as *const c_void,
            key.as_ptr(),
            &mut ptr,
            &mut len,
        )
    };
    if ok == 0 || ptr.is_null() {
        None
    } else {
        Some((ptr, len))
    }
}

/// The `langID` / `charsetID` pair naming the `StringFileInfo` sub-block, read from the resource
/// rather than assumed.
///
/// `VerQueryValue` matches the sub-block by name, and the tool that wrote the resource chose both the
/// language and the hex casing. Hardcoding `040904b0` would make this test assert something about
/// `winresource`'s formatting habits instead of about the resource's contents.
fn translation(block: &[u8]) -> (u16, u16) {
    let key = wide(r"\VarFileInfo\Translation");
    let (ptr, len) =
        query(block, &key).expect(r"no VarFileInfo\Translation in the version resource");
    assert!(
        len >= 4,
        "the translation table is {len} bytes, too short for one entry"
    );
    // An array of {langID, charsetID} u16 pairs. Only the first is read: the resource is written with
    // a single language, and a second entry would mean the fields below might not all live in it.
    let pair = unsafe { std::slice::from_raw_parts(ptr as *const u16, 2) };
    (pair[0], pair[1])
}

/// Reads one `StringFileInfo` value, from whichever language block the resource actually declares.
fn string_value(block: &[u8], name: &str) -> String {
    let (lang, charset) = translation(block);
    // Lowercase first, then uppercase. `VerQueryValue`'s comparison is documented only as matching the
    // name, and both casings occur in the wild depending on the resource compiler, so trying both is
    // cheaper than relying on undocumented case-insensitivity.
    let lower = wide(&format!(r"\StringFileInfo\{lang:04x}{charset:04x}\{name}"));
    let upper = wide(&format!(r"\StringFileInfo\{lang:04X}{charset:04X}\{name}"));
    let (ptr, len) = query(block, &lower)
        .or_else(|| query(block, &upper))
        .unwrap_or_else(|| {
            panic!("no {name} in the version resource (language block {lang:04x}{charset:04x})")
        });
    // `len` counts UTF-16 units and includes the terminating NUL.
    let units = unsafe { std::slice::from_raw_parts(ptr as *const u16, len as usize) };
    String::from_utf16_lossy(units)
        .trim_end_matches('\0')
        .to_string()
}

/// `major.minor.patch` out of the fixed part of the resource.
fn fixed_version(block: &[u8]) -> (u16, u16, u16) {
    let root = wide(r"\");
    let (ptr, len) = query(block, &root).expect("no VS_FIXEDFILEINFO in the version resource");
    assert!(
        len as usize >= std::mem::size_of::<VsFixedFileInfo>(),
        "the fixed block is {len} bytes, too short to be a VS_FIXEDFILEINFO"
    );
    let info = unsafe { &*(ptr as *const VsFixedFileInfo) };
    assert_eq!(
        info.signature, VS_FFI_SIGNATURE,
        "the fixed block does not start with VS_FFI_SIGNATURE, so this is not a version resource"
    );
    (
        (info.file_version_ms >> 16) as u16,
        (info.file_version_ms & 0xFFFF) as u16,
        (info.file_version_ls >> 16) as u16,
    )
}

/// What the binary itself says its version is — the same call the updater's smoke test makes.
fn reported_version() -> String {
    let out = std::process::Command::new(EXE)
        .arg("--print-version")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("could not run the built binary with --print-version");
    assert!(
        out.status.success(),
        "--print-version exited with {}",
        out.status
    );
    String::from_utf8(out.stdout)
        .expect("--print-version did not print UTF-8")
        .trim()
        .to_string()
}

#[test]
fn version_resource_matches_the_reported_version() {
    let block = version_block();
    let (major, minor, patch) = fixed_version(&block);

    assert_eq!(
        format!("{major}.{minor}.{patch}"),
        reported_version(),
        "the embedded FileVersion and --print-version disagree, so build.rs and src/version.rs are \
         resolving the version differently"
    );

    // The string block carries the same number in text form. Windows does not enforce that the two
    // agree, and Explorer shows this one, so it gets its own assertion.
    assert_eq!(
        string_value(&block, "FileVersion"),
        format!("{major}.{minor}.{patch}.0")
    );
    assert_eq!(
        string_value(&block, "ProductVersion"),
        format!("{major}.{minor}.{patch}.0")
    );
}

#[test]
fn version_resource_identifies_the_publisher() {
    let block = version_block();

    // Exact values, not merely non-empty: "a field is present" was never the point — an ML model and
    // a human reading the Properties dialog both want to see who ships this and what it is.
    assert_eq!(string_value(&block, "CompanyName"), "HerrDerb");
    assert_eq!(string_value(&block, "ProductName"), "githoot-tray");
    assert_eq!(
        string_value(&block, "OriginalFilename"),
        "githoot-tray.exe"
    );
    assert!(
        string_value(&block, "FileDescription").contains("githoot-tray"),
        "FileDescription should name the product; got {:?}",
        string_value(&block, "FileDescription")
    );
    assert!(
        string_value(&block, "LegalCopyright").contains("Unlicense"),
        "LegalCopyright should match LICENSE, which is the Unlicense; got {:?}",
        string_value(&block, "LegalCopyright")
    );
}
