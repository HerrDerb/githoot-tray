/// Icon management for GitHub Tray Icon.
use std::path::Path;

const GITHUB_ICON: &[u8] = include_bytes!("../assets/github.png");
const GITHUB_BLUE_ICON: &[u8] = include_bytes!("../assets/github_blue.png");

/// Writes icon bytes to a file, logging errors if they occur.
fn write_to_icon_file(bytes: &[u8], path: &str) {
    if let Err(e) = std::fs::write(path, bytes) {
        eprintln!("Failed to write icon file '{}': {e}", path);
    }
}

/// Creates icon files in the asset directory and returns their paths as strings.
pub fn create_icons(app_asset_path: &Path) -> (String, String) {
    if let Err(e) = std::fs::create_dir_all(&app_asset_path) {
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
