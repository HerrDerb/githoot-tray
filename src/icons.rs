/// Icon management for GitHub Tray Icon.

const GITHUB_ICON: &[u8] = include_bytes!("../assets/github.png");
const GITHUB_BLUE_ICON: &[u8] = include_bytes!("../assets/github_blue.png");

#[cfg(target_os = "linux")]
use std::path::Path;

/// Writes icon bytes to a file, logging errors if they occur.
#[cfg(target_os = "linux")]
fn write_to_icon_file(bytes: &[u8], path: &str) {
    if let Err(e) = std::fs::write(path, bytes) {
        eprintln!("Failed to write icon file '{}': {e}", path);
    }
}

/// Creates icon files in the asset directory and returns their paths as strings.
/// Used on Linux by libappindicator, which requires file-system paths.
#[cfg(target_os = "linux")]
pub fn create_icons(app_asset_path: &Path) -> (String, String) {
    if let Err(e) = std::fs::create_dir_all(app_asset_path) {
        eprintln!("Failed to create assets directory: {e}");
    }
    let github_icon_path = app_asset_path.join("github.png");
    let github_blue_icon_path = app_asset_path.join("github_blue.png");
    write_to_icon_file(GITHUB_ICON, github_icon_path.to_str().unwrap_or("github.png"));
    write_to_icon_file(GITHUB_BLUE_ICON, github_blue_icon_path.to_str().unwrap_or("github_blue.png"));
    (
        github_icon_path.to_str().unwrap_or("").to_string(),
        github_blue_icon_path.to_str().unwrap_or("").to_string(),
    )
}

/// Decodes the embedded PNG assets into `tray_icon::Icon` objects for Windows.
#[cfg(target_os = "windows")]
pub fn load_tray_icons() -> (tray_icon::Icon, tray_icon::Icon) {
    fn decode(bytes: &[u8]) -> tray_icon::Icon {
        let img = image::load_from_memory(bytes)
            .expect("Failed to decode icon PNG")
            .into_rgba8();
        let (w, h) = img.dimensions();
        tray_icon::Icon::from_rgba(img.into_raw(), w, h).expect("Failed to create tray icon")
    }
    (decode(GITHUB_ICON), decode(GITHUB_BLUE_ICON))
}
