//! Icon management for GitHub Tray Icon.
//!
//! Two independent signals are drawn onto the tray icon, giving four variants:
//!   * base glyph — dark (no unread notifications) or blue (unread notifications)
//!   * review dot — a red disc in the upper-right when a PR is awaiting your review
//!
//! The dot is composited at runtime rather than shipped as two extra PNGs, so there is one code
//! path, the dot can never drift between the two base icons, and `assets/` stays at two files.

use image::{Rgba, RgbaImage};

#[cfg(target_os = "linux")]
use crate::logln;
#[cfg(target_os = "linux")]
use std::path::Path;

const GITHUB_ICON: &[u8] = include_bytes!("../assets/github.png");
const GITHUB_BLUE_ICON: &[u8] = include_bytes!("../assets/github_blue.png");

// ── Review-dot geometry ───────────────────────────────────────────────────────
// Proportional to the icon width so the dot survives the source assets being resized.
// The taskbar scales 98×96 down to roughly 16×16, so the disc has to be a sizeable fraction of
// the icon to be legible at all. 0.165 was picked by rendering 0.140/0.165/0.195 and inspecting
// each at 16px and 20px on light and dark backgrounds: 0.140 gets faint once scaled, 0.195 is
// clearest but swallows the glyph's ear, and this sits between the two.

/// Disc radius as a fraction of icon width.
const DOT_RADIUS_RATIO: f32 = 0.165;
/// Gap between the disc and the icon's top/right edges, as a fraction of icon width.
const DOT_MARGIN_RATIO: f32 = 0.02;
/// Total width of the erased ring around the disc, in source pixels. Punching the base icon
/// through to full transparency here keeps the red readable against the dark glyph *and* against
/// whatever colour the user's taskbar happens to be.
const DOT_RING_PX: f32 = 2.5;
/// How much of that ring is a soft fade at its *outer* edge. The remainder, next to the disc, is
/// erased completely — a ring that only ever faded would leave no fully-transparent separator at
/// all, since the innermost ring pixel starts partway down the gradient.
const DOT_RING_FADE_PX: f32 = 1.0;
/// Bright red, chosen to hold contrast on both light and dark taskbars.
const DOT_COLOR: [u8; 4] = [0xF0, 0x3E, 0x3E, 0xFF];

/// Returns a copy of `src` with the review dot drawn in the upper-right corner.
pub fn with_review_dot(src: &RgbaImage) -> RgbaImage {
    let (width, height) = src.dimensions();
    let mut out = src.clone();

    let radius = width as f32 * DOT_RADIUS_RATIO;
    let margin = width as f32 * DOT_MARGIN_RATIO;
    let centre_x = width as f32 - radius - margin;
    let centre_y = radius + margin;
    let ring_outer = radius + DOT_RING_PX;

    for y in 0..height {
        for x in 0..width {
            // Sample at the pixel centre so the anti-aliasing is symmetric.
            let dx = x as f32 + 0.5 - centre_x;
            let dy = y as f32 + 0.5 - centre_y;
            let distance = (dx * dx + dy * dy).sqrt();

            if distance <= radius + 0.5 {
                let coverage = (radius + 0.5 - distance).clamp(0.0, 1.0);
                let pixel = out.get_pixel_mut(x, y);
                *pixel = over(DOT_COLOR, *pixel, coverage);
            } else if distance <= ring_outer {
                // Clamping at 1.0 is what creates the solid band: only the outermost
                // DOT_RING_FADE_PX of the ring produces a value below 1.
                let erase = ((ring_outer - distance) / DOT_RING_FADE_PX).clamp(0.0, 1.0);
                let pixel = out.get_pixel_mut(x, y);
                pixel[3] = (f32::from(pixel[3]) * (1.0 - erase)).round() as u8;
            }
        }
    }

    out
}

/// Source-over composite of `src` (at `coverage` alpha) onto `dst`, un-premultiplied throughout.
fn over(src: [u8; 4], dst: Rgba<u8>, coverage: f32) -> Rgba<u8> {
    let src_a = f32::from(src[3]) / 255.0 * coverage;
    let dst_a = f32::from(dst[3]) / 255.0;
    let out_a = src_a + dst_a * (1.0 - src_a);

    if out_a <= f32::EPSILON {
        return Rgba([0, 0, 0, 0]);
    }

    let mut out = [0u8; 4];
    for channel in 0..3 {
        let s = f32::from(src[channel]) / 255.0;
        let d = f32::from(dst[channel]) / 255.0;
        out[channel] = (((s * src_a + d * dst_a * (1.0 - src_a)) / out_a) * 255.0).round() as u8;
    }
    out[3] = (out_a * 255.0).round() as u8;
    Rgba(out)
}

fn decode(bytes: &[u8]) -> Result<RgbaImage, String> {
    image::load_from_memory(bytes)
        .map_err(|e| format!("failed to decode icon PNG: {e}"))
        .map(|img| img.into_rgba8())
}

/// The four icon variants, indexed by `[unread_notifications][review_pending]`.
pub struct IconSet<T> {
    variants: [[T; 2]; 2],
}

impl<T> IconSet<T> {
    pub fn get(&self, unread: bool, review: bool) -> &T {
        &self.variants[usize::from(unread)][usize::from(review)]
    }
}

/// Builds all four variants from the two embedded base icons.
fn build_variants() -> Result<IconSet<RgbaImage>, String> {
    let plain = decode(GITHUB_ICON)?;
    let blue = decode(GITHUB_BLUE_ICON)?;
    let plain_dot = with_review_dot(&plain);
    let blue_dot = with_review_dot(&blue);

    Ok(IconSet {
        variants: [[plain, plain_dot], [blue, blue_dot]],
    })
}

// ─── Linux ────────────────────────────────────────────────────────────────────

/// Encodes an RGBA image as PNG bytes.
#[cfg(target_os = "linux")]
fn encode_png(img: &RgbaImage) -> Result<Vec<u8>, String> {
    use image::codecs::png::PngEncoder;
    use image::{ColorType, ImageEncoder};

    let mut bytes = Vec::new();
    PngEncoder::new(&mut bytes)
        .write_image(img.as_raw(), img.width(), img.height(), ColorType::Rgba8)
        .map_err(|e| format!("failed to encode icon PNG: {e}"))?;
    Ok(bytes)
}

/// Writes icon bytes only when the file is missing or its contents differ.
///
/// The bytes are derived from compiled-in assets and never change between runs, so rewriting on
/// every launch just churned the mtime — which some StatusNotifierItem hosts use to decide
/// whether to reload a cached pixmap.
#[cfg(target_os = "linux")]
fn write_icon_if_changed(bytes: &[u8], path: &Path) {
    if std::fs::read(path).map(|existing| existing == bytes).unwrap_or(false) {
        return;
    }
    if let Err(e) = std::fs::write(path, bytes) {
        logln!("failed to write icon file '{}': {e}", path.display());
    }
}

/// Creates all four icon files in the asset directory and returns their paths.
/// Used on Linux by libappindicator, which requires file-system paths.
#[cfg(target_os = "linux")]
pub fn create_icons(app_asset_path: &Path) -> Result<IconSet<String>, String> {
    if let Err(e) = std::fs::create_dir_all(app_asset_path) {
        logln!("failed to create assets directory: {e}");
    }

    let images = build_variants()?;
    let names = [["github.png", "github_dot.png"], ["github_blue.png", "github_blue_dot.png"]];

    let mut paths: [[String; 2]; 2] = Default::default();
    for unread in 0..2 {
        for review in 0..2 {
            let path = app_asset_path.join(names[unread][review]);
            write_icon_if_changed(&encode_png(&images.variants[unread][review])?, &path);
            paths[unread][review] = path.to_string_lossy().into_owned();
        }
    }

    Ok(IconSet { variants: paths })
}

// ─── Windows ──────────────────────────────────────────────────────────────────

/// Decodes and composites the embedded assets into `tray_icon::Icon` objects.
#[cfg(target_os = "windows")]
pub fn load_tray_icons() -> Result<IconSet<tray_icon::Icon>, String> {
    fn to_icon(img: &RgbaImage) -> Result<tray_icon::Icon, String> {
        let (w, h) = img.dimensions();
        tray_icon::Icon::from_rgba(img.as_raw().clone(), w, h)
            .map_err(|e| format!("failed to create tray icon: {e}"))
    }

    let images = build_variants()?;
    Ok(IconSet {
        variants: [
            [to_icon(&images.variants[0][0])?, to_icon(&images.variants[0][1])?],
            [to_icon(&images.variants[1][0])?, to_icon(&images.variants[1][1])?],
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> RgbaImage {
        decode(GITHUB_ICON).expect("embedded asset must decode")
    }

    #[test]
    fn embedded_assets_are_the_expected_shape() {
        let img = base();
        assert_eq!(img.dimensions(), (98, 96));
    }

    #[test]
    fn dot_preserves_dimensions() {
        let src = base();
        assert_eq!(with_review_dot(&src).dimensions(), src.dimensions());
    }

    #[test]
    fn dot_centre_is_opaque_red() {
        let src = base();
        let out = with_review_dot(&src);
        let radius = 98.0 * DOT_RADIUS_RATIO;
        let margin = 98.0 * DOT_MARGIN_RATIO;
        let (cx, cy) = ((98.0 - radius - margin) as u32, (radius + margin) as u32);

        let px = out.get_pixel(cx, cy);
        assert_eq!(px[3], 255, "dot centre must be fully opaque");
        assert!(px[0] > 200, "dot centre must be red-dominant, got {px:?}");
        assert!(px[1] < 100 && px[2] < 100, "dot centre must not be washed out, got {px:?}");
    }

    /// The erased ring is what keeps the red legible against the dark glyph and against an
    /// arbitrary taskbar colour.
    #[test]
    fn ring_around_dot_is_fully_transparent() {
        let out = with_review_dot(&base());
        let radius = 98.0 * DOT_RADIUS_RATIO;
        let margin = 98.0 * DOT_MARGIN_RATIO;
        let cx = 98.0 - radius - margin;
        let cy = radius + margin;

        // Straight down from the centre, in the solid part of the ring: past the disc's own
        // anti-aliased edge, but inside the band that is erased completely.
        let probe_y = (cy + radius + 1.0) as u32;
        let px = out.get_pixel(cx as u32, probe_y);
        assert_eq!(px[3], 0, "ring must be erased to full transparency, got {px:?}");

        // …and beyond the ring the base icon must be intact again.
        let beyond = out.get_pixel(cx as u32, (cy + radius + DOT_RING_PX + 1.0) as u32);
        let original = base().get_pixel(cx as u32, (cy + radius + DOT_RING_PX + 1.0) as u32).0;
        assert_eq!(beyond.0, original, "the erase must not bleed past the ring");
    }

    #[test]
    fn pixels_far_from_the_dot_are_untouched() {
        let src = base();
        let out = with_review_dot(&src);
        // Bottom-left corner is nowhere near the upper-right dot.
        for (x, y) in [(0u32, 95u32), (4, 90), (20, 80)] {
            assert_eq!(src.get_pixel(x, y), out.get_pixel(x, y), "({x},{y}) must be unchanged");
        }
    }

    #[test]
    fn all_four_variants_build_and_differ() {
        let set = build_variants().expect("variants must build");
        let plain = set.get(false, false).as_raw();
        let plain_dot = set.get(false, true).as_raw();
        let blue = set.get(true, false).as_raw();
        let blue_dot = set.get(true, true).as_raw();

        assert_ne!(plain, plain_dot, "dot variant must differ from its base");
        assert_ne!(blue, blue_dot, "dot variant must differ from its base");
        assert_ne!(plain, blue, "base icons must differ from each other");
    }

    #[test]
    fn over_is_a_no_op_at_zero_coverage() {
        let dst = Rgba([1, 2, 3, 200]);
        assert_eq!(over(DOT_COLOR, dst, 0.0), dst);
    }

    #[test]
    fn over_fully_replaces_at_full_coverage() {
        let out = over(DOT_COLOR, Rgba([1, 2, 3, 255]), 1.0);
        assert_eq!(out, Rgba(DOT_COLOR));
    }
}
