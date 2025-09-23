use tray_icon::Icon;
use image::io::Reader as ImageReader;
use image::ImageFormat;

const GITHUB_ICON: &[u8] = include_bytes!("../assets/github.ico");
const GITHUB_BLUE_ICON: &[u8] = include_bytes!("../assets/github_blue.ico");

fn icon_from_ico_bytes(bytes: &[u8]) -> Icon {
    let img = ImageReader::with_format(std::io::Cursor::new(bytes), ImageFormat::Ico)
        .decode()
        .expect("Failed to decode icon bytes");
    let rgba = img.to_rgba8();
    let (width, height) = rgba.dimensions();
    Icon::from_rgba(rgba.into_raw(), width, height).expect("Failed to create Icon from RGBA")
}

pub fn load_icons() -> (Icon, Icon) {
    let icon = icon_from_ico_bytes(GITHUB_ICON);
    let icon_with_notification = icon_from_ico_bytes(GITHUB_BLUE_ICON);
    (icon, icon_with_notification)
}
