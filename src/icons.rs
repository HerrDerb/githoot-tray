//! Icon management for GitHub Tray Icon.

#[cfg(target_os = "linux")]
use crate::logln;

const GITHUB_ICON: &[u8] = include_bytes!("../assets/github.png");
const GITHUB_BLUE_ICON: &[u8] = include_bytes!("../assets/github_blue.png");

#[cfg(target_os = "linux")]
use std::path::Path;

/// Writes icon bytes only when the file is missing or its contents differ.
///
/// The bytes are compiled in and never change between runs, so rewriting on every launch just
/// churned the mtime — which some StatusNotifierItem hosts use to decide whether to reload a
/// cached pixmap.
#[cfg(target_os = "linux")]
fn write_icon_if_changed(bytes: &[u8], path: &Path) {
    if std::fs::read(path).map(|existing| existing == bytes).unwrap_or(false) {
        return;
    }
    if let Err(e) = std::fs::write(path, bytes) {
        logln!("failed to write icon file '{}': {e}", path.display());
    }
}

/// Creates icon files in the asset directory and returns their paths as strings.
/// Used on Linux by libappindicator, which requires file-system paths.
#[cfg(target_os = "linux")]
pub fn create_icons(app_asset_path: &Path) -> (String, String) {
    if let Err(e) = std::fs::create_dir_all(app_asset_path) {
        logln!("failed to create assets directory: {e}");
    }

    let github_icon_path = app_asset_path.join("github.png");
    let github_blue_icon_path = app_asset_path.join("github_blue.png");
    write_icon_if_changed(GITHUB_ICON, &github_icon_path);
    write_icon_if_changed(GITHUB_BLUE_ICON, &github_blue_icon_path);

    (
        github_icon_path.to_string_lossy().into_owned(),
        github_blue_icon_path.to_string_lossy().into_owned(),
    )
}

/// Decodes the embedded PNG assets into `tray_icon::Icon` objects for Windows.
#[cfg(target_os = "windows")]
pub fn load_tray_icons() -> Result<(tray_icon::Icon, tray_icon::Icon), String> {
    fn decode(bytes: &[u8]) -> Result<tray_icon::Icon, String> {
        let img = image::load_from_memory(bytes)
            .map_err(|e| format!("failed to decode icon PNG: {e}"))?
            .into_rgba8();
        let (w, h) = img.dimensions();
        tray_icon::Icon::from_rgba(img.into_raw(), w, h)
            .map_err(|e| format!("failed to create tray icon: {e}"))
    }

    Ok((decode(GITHUB_ICON)?, decode(GITHUB_BLUE_ICON)?))
}
