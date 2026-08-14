//! Icon management for GitHub Tray Icon.
//!
//! Four independent signals are drawn onto the tray icon, giving sixteen variants:
//!   * base glyph — dark (no unread notifications) or blue (unread notifications)
//!   * review dot — red, top-right, when a PR is awaiting your review
//!   * ready-to-merge dot — green, bottom-right, when one of your PRs is approved and passing
//!   * changes-requested dot — orange, bottom-left, when a reviewer asked for changes on one of
//!     your PRs
//!
//! All three dots are composited at runtime rather than shipped as extra PNGs, so there is one
//! code path, they can never drift between the two base icons, and `assets/` stays at two files.
//! The three corners were sized and spaced so the dots never touch even when all three are drawn
//! at once — see `dots_never_overlap_even_with_the_largest_dot` below.

use image::{Rgba, RgbaImage};

#[cfg(target_os = "linux")]
use crate::logln;
#[cfg(target_os = "linux")]
use std::path::Path;

const GITHUB_ICON: &[u8] = include_bytes!("../assets/github.png");
const GITHUB_BLUE_ICON: &[u8] = include_bytes!("../assets/github_blue.png");

// ── Dot geometry ────────────────────────────────────────────────────────────
// Proportional to the icon width so the dots survive the source assets being resized.
// The taskbar scales 98×96 down to roughly 16×16, so a disc has to be a sizeable fraction of the
// icon to be legible at all. 0.165 was picked by rendering 0.140/0.165/0.195 and inspecting each
// at 16px and 20px on light and dark backgrounds: 0.140 gets faint once scaled, 0.195 is clearest
// but swallows the glyph's ear, and this sits between the two. All three dots share this sizing —
// there is only one glyph-legibility budget, not three.

/// Disc radius as a fraction of icon width.
const DOT_RADIUS_RATIO: f32 = 0.165;
/// Gap between a disc and the icon's edges, as a fraction of icon width.
const DOT_MARGIN_RATIO: f32 = 0.02;
/// Total width of the erased ring around a disc, in source pixels. Punching the base icon through
/// to full transparency here keeps a dot readable against the dark glyph *and* against whatever
/// colour the user's taskbar happens to be.
const DOT_RING_PX: f32 = 2.5;
/// How much of that ring is a soft fade at its *outer* edge. The remainder, next to the disc, is
/// erased completely — a ring that only ever faded would leave no fully-transparent separator at
/// all, since the innermost ring pixel starts partway down the gradient.
const DOT_RING_FADE_PX: f32 = 1.0;

/// Bright red, chosen to hold contrast on both light and dark taskbars.
const REVIEW_DOT_COLOR: [u8; 4] = [0xF0, 0x3E, 0x3E, 0xFF];
/// A clear, saturated green — picked to read distinctly from the review red at a glance, not just
/// on close inspection. Like the red, not yet visually proven at 16px on a real taskbar; sanity
/// check on a real render before shipping.
const MERGE_DOT_COLOR: [u8; 4] = [0x1A, 0xC9, 0x4A, 0xFF];
/// An amber/orange, chosen to sit clearly between the review red and the merge green in hue so
/// three dots on screen at once stay distinguishable. Same caveat as the green: unproven on a
/// real render.
const CHANGES_DOT_COLOR: [u8; 4] = [0xE0, 0x8A, 0x00, 0xFF];

// ── Authorization mark geometry ─────────────────────────────────────────────
// A big exclamation mark down the **right-hand side** of the icon, for the one state where the app
// has no credential and therefore no answer to give. Deliberately unlike the corner dots: a dot says
// "here is one more fact about your PRs", this says "none of those facts are available".
//
// Three layouts were built and compared at 16px and 22px before settling here, and the reasoning is
// worth keeping because the constraint is not obvious:
//
//   1. Mark through the *centre* of the glyph. Legible, but it cut the octocat in half.
//   2. Mark on its own strip beside the glyph, on a widened canvas. The octocat stayed pristine, but
//      a tray slot is a fixed square. Panels letterbox a non-square pixmap back into it, so the
//      octocat came out visibly shrunk — verified on a real GNOME panel, not assumed.
//   3. This: the canvas stays exactly square, so nothing is ever letterboxed or scaled down, and the
//      mark moves to the right edge where it overlaps the glyph rather than bisecting it.
//
// So the octocat keeps its **full size** and sits *behind* the mark. It gives up its right-hand
// sliver, which is the cheapest part of it to lose — far cheaper than either shrinking the whole
// glyph or splitting it down the middle.
//
// The erase ring is therefore back: the mark is over the glyph again, and the ring is what keeps it
// readable against the octocat's bright white face and against any taskbar colour.
//
// All ratios are of icon *width*, except the vertical extents which are of height, so the mark
// survives the source assets being resized the same way the dots do.

/// The mark's colour. Deliberately the same red as the review dot, which was already tuned to hold
/// contrast on light and dark taskbars. The two can never be on screen together — a missing
/// credential means there is no review answer to draw — so there is nothing to confuse.
const AUTH_MARK_COLOR: [u8; 4] = REVIEW_DOT_COLOR;
/// Width of the mark, as a fraction of icon width. One value doing two jobs: the stem's width and
/// the dot's diameter, as a real exclamation mark has.
const AUTH_MARK_BAND_RATIO: f32 = 0.26;
/// Gap between the mark's right edge and the icon's right edge, as a fraction of icon width. Small,
/// but non-zero: flush against the edge looks like a rendering accident rather than a decision.
const AUTH_MARK_RIGHT_MARGIN_RATIO: f32 = 0.03;
/// Lower edge of the stem, as a fraction of icon height. The stem's *upper* edge is always y = 0:
/// the mark spans the full height of the icon, which is what makes it dominate rather than decorate.
const AUTH_MARK_STEM_BOTTOM_RATIO: f32 = 0.66;
/// Centre of the dot beneath the stem, as a fraction of icon height. Placed so the dot's lower edge
/// lands just inside the bottom of the icon.
const AUTH_MARK_DOT_CENTRE_RATIO: f32 = 0.867;
/// The mark's erase ring, wider and with a longer fade than the corner dots'.
///
/// Full erasure only reaches `RING - FADE` from a shape, so this pair has to cover half the gap
/// between stem and dot or that gap stops being fully transparent, the octocat's white face shows
/// through it, and at 16px the two halves blur into one solid red bar. That regression happened twice
/// during development; `the_gap_keeps_stem_and_dot_apart` is what now catches it.
const AUTH_MARK_RING_PX: f32 = 5.5;
const AUTH_MARK_RING_FADE_PX: f32 = 1.5;

/// Which corner a dot is drawn in.
#[derive(Clone, Copy, Debug)]
enum Corner {
    TopRight,
    BottomRight,
    BottomLeft,
}

impl Corner {
    /// Disc centre for this corner, given the image size and disc geometry.
    fn centre(self, width: f32, height: f32, radius: f32, margin: f32) -> (f32, f32) {
        match self {
            Corner::TopRight => (width - radius - margin, radius + margin),
            Corner::BottomRight => (width - radius - margin, height - radius - margin),
            Corner::BottomLeft => (radius + margin, height - radius - margin),
        }
    }
}

/// Returns a copy of `src` with a dot of `color` drawn in `corner`.
///
/// Generalized from a single hardcoded review dot: the pixel math (disc + soft erase ring) never
/// depended on which corner or which color it was, only the geometry constants above did.
fn with_dot(src: &RgbaImage, color: [u8; 4], corner: Corner) -> RgbaImage {
    let (width, height) = src.dimensions();
    let mut out = src.clone();

    let radius = width as f32 * DOT_RADIUS_RATIO;
    let margin = width as f32 * DOT_MARGIN_RATIO;
    let (centre_x, centre_y) = corner.centre(width as f32, height as f32, radius, margin);
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
                *pixel = over(color, *pixel, coverage);
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

/// Returns a copy of `src` with a big red exclamation mark down its right-hand side.
///
/// Dimensions are preserved, deliberately: the canvas stays square so no tray panel ever letterboxes
/// it and scales the octocat down. The glyph keeps its full size and sits behind the mark, giving up
/// only its right-hand sliver. See the geometry block above for the two layouts rejected first.
///
/// Same visual language as `with_dot` — solid shape, then a fully-erased ring with a soft outer fade
/// — so the mark stays legible against the glyph beneath it and against any taskbar colour. The
/// difference is the shape, so the arithmetic is expressed as a *signed* distance (negative inside,
/// positive outside) rather than `with_dot`'s distance-to-a-centre: that is what lets one pass cover
/// a stem and a dot at once by taking the nearer of the two.
///
/// The stem is a capsule rather than a rectangle. Round ends match the dot below it, which is what
/// makes the two read as one mark rather than a bar sitting above an unrelated circle.
fn with_exclamation(src: &RgbaImage) -> RgbaImage {
    let (width, height) = src.dimensions();
    let mut out = src.clone();

    let w = width as f32;
    let h = height as f32;
    // `.max(1.0)` so a pathologically small source cannot produce a zero radius and draw nothing.
    let radius = (w * AUTH_MARK_BAND_RATIO).round().max(1.0) / 2.0;
    // Measured in from the right edge, so the mark hugs that side rather than the centre.
    let centre_x = w - (w * AUTH_MARK_RIGHT_MARGIN_RATIO) - radius;
    // The stem's outer edge sits at y = 0, so its capsule centre starts one radius down.
    let stem_top = radius;
    let stem_bottom = h * AUTH_MARK_STEM_BOTTOM_RATIO - radius;
    let dot_centre_y = h * AUTH_MARK_DOT_CENTRE_RATIO;

    for y in 0..height {
        for x in 0..width {
            // Sample at the pixel centre so the anti-aliasing is symmetric, as in `with_dot`.
            let px = x as f32 + 0.5;
            let py = y as f32 + 0.5;

            // Distance to a vertical segment: clamping y to the segment's extent is what turns the
            // point-to-point distance into point-to-capsule.
            let stem = (px - centre_x).hypot(py - py.clamp(stem_top, stem_bottom)) - radius;
            let dot = (px - centre_x).hypot(py - dot_centre_y) - radius;
            let distance = stem.min(dot);

            // The two branches are disjoint, so the erase can never eat mark pixels this same pass
            // has just drawn.
            if distance <= 0.5 {
                let coverage = (0.5 - distance).clamp(0.0, 1.0);
                let pixel = out.get_pixel_mut(x, y);
                *pixel = over(AUTH_MARK_COLOR, *pixel, coverage);
            } else if distance <= AUTH_MARK_RING_PX {
                let erase =
                    ((AUTH_MARK_RING_PX - distance) / AUTH_MARK_RING_FADE_PX).clamp(0.0, 1.0);
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

/// The sixteen icon variants, indexed by a packed 4-bit key: bit 3 is the unread-notifications
/// tint, bit 2 the review dot, bit 1 the ready-to-merge dot, bit 0 the changes-requested dot —
/// plus one extra, `needs_auth`, which is not part of that space at all.
///
/// `needs_auth` is a separate field rather than a seventeenth array slot on purpose: the four
/// signals are combinable, and this one is not. It replaces the whole picture rather than adding to
/// it, because it means "there is no credential, so none of those four questions could even be
/// asked" — drawing dots alongside it would assert answers the app does not have. Keeping it out of
/// the array means the 4-bit index can never reach it by accident.
pub struct IconSet<T> {
    variants: [T; 16],
    needs_auth: T,
}

impl<T> IconSet<T> {
    fn index(unread: bool, review: bool, merge: bool, changes: bool) -> usize {
        (usize::from(unread) << 3)
            | (usize::from(review) << 2)
            | (usize::from(merge) << 1)
            | usize::from(changes)
    }

    pub fn get(&self, unread: bool, review: bool, merge: bool, changes: bool) -> &T {
        &self.variants[Self::index(unread, review, merge, changes)]
    }

    /// The variant shown while a credential is waiting on the user: base glyph, big red
    /// exclamation, no dots. Takes no arguments precisely because it overrides all four signals.
    pub fn needs_auth(&self) -> &T {
        &self.needs_auth
    }
}

/// Builds all sixteen variants from the two embedded base icons.
///
/// Infallible past decoding the two base assets: compositing dots never fails, so the loop needs
/// no error path of its own — that is what makes a plain `std::array::from_fn` sufficient here
/// (contrast `create_icons`/`load_tray_icons` below, where PNG encoding or icon creation can fail
/// per variant).
fn build_variants() -> Result<IconSet<RgbaImage>, String> {
    let plain = decode(GITHUB_ICON)?;
    let blue = decode(GITHUB_BLUE_ICON)?;

    let variants: [RgbaImage; 16] = std::array::from_fn(|i| {
        let unread = i & 0b1000 != 0;
        let review = i & 0b0100 != 0;
        let merge = i & 0b0010 != 0;
        let changes = i & 0b0001 != 0;

        let mut img = if unread { blue.clone() } else { plain.clone() };
        if review {
            img = with_dot(&img, REVIEW_DOT_COLOR, Corner::TopRight);
        }
        if merge {
            img = with_dot(&img, MERGE_DOT_COLOR, Corner::BottomRight);
        }
        if changes {
            img = with_dot(&img, CHANGES_DOT_COLOR, Corner::BottomLeft);
        }
        img
    });

    // Built from the plain glyph, never the blue one: the blue tint means "unread notifications",
    // which is exactly the kind of claim this variant exists to withhold.
    Ok(IconSet { variants, needs_auth: with_exclamation(&plain) })
}

/// Filename of the needs-authorization variant. A fixed name rather than something
/// `variant_filename` could produce, because it is not addressed by the 4-bit key.
#[cfg(target_os = "linux")]
const NEEDS_AUTH_FILENAME: &str = "github_needs_auth.png";

/// Filename for variant `i`, built from which bits are set rather than a hand-written 16-entry
/// table — the table would just be this function's output written out by hand, with all the same
/// opportunities to get one entry wrong.
#[cfg(target_os = "linux")]
fn variant_filename(i: usize) -> String {
    let mut name = String::from("github");
    if i & 0b1000 != 0 {
        name.push_str("_blue");
    }
    if i & 0b0100 != 0 {
        name.push_str("_review");
    }
    if i & 0b0010 != 0 {
        name.push_str("_merge");
    }
    if i & 0b0001 != 0 {
        name.push_str("_changes");
    }
    name.push_str(".png");
    name
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

/// Creates all seventeen icon files in the asset directory and returns their paths.
/// Used on Linux by libappindicator, which requires file-system paths.
#[cfg(target_os = "linux")]
pub fn create_icons(app_asset_path: &Path) -> Result<IconSet<String>, String> {
    if let Err(e) = std::fs::create_dir_all(app_asset_path) {
        logln!("failed to create assets directory: {e}");
    }

    let images = build_variants()?;
    let mut paths = Vec::with_capacity(16);
    for (i, image) in images.variants.iter().enumerate() {
        let path = app_asset_path.join(variant_filename(i));
        write_icon_if_changed(&encode_png(image)?, &path);
        paths.push(path.to_string_lossy().into_owned());
    }

    let variants: [String; 16] = paths
        .try_into()
        .map_err(|_| "internal error: expected exactly 16 icon variants".to_string())?;

    let needs_auth_path = app_asset_path.join(NEEDS_AUTH_FILENAME);
    write_icon_if_changed(&encode_png(&images.needs_auth)?, &needs_auth_path);
    let needs_auth = needs_auth_path.to_string_lossy().into_owned();

    Ok(IconSet { variants, needs_auth })
}

// ─── Windows and macOS ────────────────────────────────────────────────────────

/// Decodes and composites the embedded assets into `tray_icon::Icon` objects.
///
/// Shared by both `tray-icon` platforms. Deliberately *not* paired with
/// `TrayIconBuilder::with_icon_as_template(true)` on macOS: template mode is the idiomatic way to
/// get automatic menu-bar tinting, but it forces the image monochrome, which would erase the blue
/// unread glyph and all three colored dots — the only things this icon exists to say.
#[cfg(any(target_os = "windows", target_os = "macos"))]
pub fn load_tray_icons() -> Result<IconSet<tray_icon::Icon>, String> {
    fn to_icon(img: &RgbaImage) -> Result<tray_icon::Icon, String> {
        let (w, h) = img.dimensions();
        tray_icon::Icon::from_rgba(img.as_raw().clone(), w, h)
            .map_err(|e| format!("failed to create tray icon: {e}"))
    }

    let images = build_variants()?;
    let mut icons = Vec::with_capacity(16);
    for image in &images.variants {
        icons.push(to_icon(image)?);
    }

    let variants: [tray_icon::Icon; 16] = icons
        .try_into()
        .map_err(|_| "internal error: expected exactly 16 icon variants".to_string())?;
    let needs_auth = to_icon(&images.needs_auth)?;
    Ok(IconSet { variants, needs_auth })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> RgbaImage {
        decode(GITHUB_ICON).expect("embedded asset must decode")
    }

    /// Where `with_dot` puts a disc's centre, mirroring `Corner::centre` for test assertions.
    fn centre_of(corner: Corner) -> (u32, u32) {
        let radius = 98.0 * DOT_RADIUS_RATIO;
        let margin = 98.0 * DOT_MARGIN_RATIO;
        let (cx, cy) = corner.centre(98.0, 96.0, radius, margin);
        (cx as u32, cy as u32)
    }

    #[test]
    fn embedded_assets_are_the_expected_shape() {
        let img = base();
        assert_eq!(img.dimensions(), (98, 96));
    }

    #[test]
    fn dot_preserves_dimensions() {
        let src = base();
        assert_eq!(with_dot(&src, REVIEW_DOT_COLOR, Corner::TopRight).dimensions(), src.dimensions());
    }

    /// Checks a dot's centre pixel is fully opaque and dominated by its own color, for all three
    /// dots — proving the corner math, not just the top-right case the original test covered.
    #[test]
    fn each_dot_centre_is_opaque_and_its_own_color() {
        for (color, corner, dominant) in [
            (REVIEW_DOT_COLOR, Corner::TopRight, 0usize),
            (MERGE_DOT_COLOR, Corner::BottomRight, 1usize),
            (CHANGES_DOT_COLOR, Corner::BottomLeft, 0usize),
        ] {
            let out = with_dot(&base(), color, corner);
            let (cx, cy) = centre_of(corner);
            let px = out.get_pixel(cx, cy);
            assert_eq!(px[3], 255, "dot centre must be fully opaque, got {px:?}");
            assert!(
                px[dominant] > 200,
                "dot centre must be dominated by its own color channel {dominant}, got {px:?}"
            );
        }
    }

    /// The erased ring is what keeps a dot legible against the dark glyph and against an
    /// arbitrary taskbar colour. Only the top-right/red case needs this in detail — the geometry
    /// is shared, so proving it once is proving it for all three corners.
    #[test]
    fn ring_around_dot_is_fully_transparent() {
        let out = with_dot(&base(), REVIEW_DOT_COLOR, Corner::TopRight);
        let radius = 98.0 * DOT_RADIUS_RATIO;
        let (cx, cy) = centre_of(Corner::TopRight);
        let cx = cx as f32;
        let cy = cy as f32;

        // Straight down from the centre, in the solid part of the ring: past the disc's own
        // anti-aliased edge, but inside the band that is erased completely.
        let probe_y = (cy + radius + 1.0) as u32;
        let px = out.get_pixel(cx as u32, probe_y);
        assert_eq!(px[3], 0, "ring must be erased to full transparency, got {px:?}");

        // …and beyond the ring the base icon must be intact again.
        let beyond_y = (cy + radius + DOT_RING_PX + 1.0) as u32;
        let beyond = out.get_pixel(cx as u32, beyond_y);
        let original = base().get_pixel(cx as u32, beyond_y).0;
        assert_eq!(beyond.0, original, "the erase must not bleed past the ring");
    }

    /// Each dot must leave the *other two* corners untouched — the three-dot design only works if
    /// they never interfere with each other.
    #[test]
    fn a_dot_never_touches_the_other_two_corners() {
        let probes: [(&str, (u32, u32)); 3] = [
            ("top-right", centre_of(Corner::TopRight)),
            ("bottom-right", centre_of(Corner::BottomRight)),
            ("bottom-left", centre_of(Corner::BottomLeft)),
        ];

        for (color, corner, own) in [
            (REVIEW_DOT_COLOR, Corner::TopRight, "top-right"),
            (MERGE_DOT_COLOR, Corner::BottomRight, "bottom-right"),
            (CHANGES_DOT_COLOR, Corner::BottomLeft, "bottom-left"),
        ] {
            let src = base();
            let out = with_dot(&src, color, corner);
            for (name, (x, y)) in probes {
                if name == own {
                    continue;
                }
                assert_eq!(
                    src.get_pixel(x, y),
                    out.get_pixel(x, y),
                    "{own} dot must not touch the {name} corner"
                );
            }
        }
    }

    #[test]
    fn all_sixteen_variants_build_and_are_pairwise_distinct() {
        let set = build_variants().expect("variants must build");
        for i in 0..16 {
            for j in (i + 1)..16 {
                assert_ne!(
                    set.variants[i].as_raw(),
                    set.variants[j].as_raw(),
                    "variants {i:#06b} and {j:#06b} must be distinguishable"
                );
            }
        }
    }

    /// The mark's centre column, and the Y coordinates of its interesting bands, for a 98×96 source:
    /// the middle of the stem, the middle of the dot, and the middle of the gap between them.
    fn auth_mark_probes() -> (u32, u32, u32, u32) {
        let radius = (98.0 * AUTH_MARK_BAND_RATIO).round() / 2.0;
        let centre_x = 98.0 - (98.0 * AUTH_MARK_RIGHT_MARGIN_RATIO) - radius;
        let stem_bottom = 96.0 * AUTH_MARK_STEM_BOTTOM_RATIO - radius;
        let dot_centre = 96.0 * AUTH_MARK_DOT_CENTRE_RATIO;
        (
            centre_x as u32,
            ((radius + stem_bottom) / 2.0) as u32,
            dot_centre as u32,
            // Halfway between the stem's lower edge and the dot's upper edge.
            (((stem_bottom + radius) + (dot_centre - radius)) / 2.0) as u32,
        )
    }

    /// The canvas must stay exactly square. This is the whole reason the mark moved to the right edge
    /// instead of getting its own strip: a non-square pixmap gets letterboxed into the tray's square
    /// slot, which scales the octocat down. Widen this and that regression comes back.
    #[test]
    fn exclamation_preserves_dimensions() {
        let src = base();
        assert_eq!(with_exclamation(&src).dimensions(), src.dimensions());
    }

    /// The octocat keeps its full size, so the majority of it must come through untouched — the mark
    /// takes only the right-hand sliver. Checks every pixel left of the mark and its erase ring.
    #[test]
    fn the_mark_only_touches_the_right_hand_sliver_of_the_glyph() {
        let src = base();
        let out = with_exclamation(&src);
        let (centre_x, ..) = auth_mark_probes();
        let radius = (98.0 * AUTH_MARK_BAND_RATIO).round() / 2.0;
        let untouched_until = (centre_x as f32 - radius - AUTH_MARK_RING_PX) as u32;

        assert!(
            untouched_until > src.width() / 2,
            "more than half the glyph must survive, only {untouched_until} of {} columns do",
            src.width()
        );
        for y in 0..src.height() {
            for x in 0..untouched_until {
                assert_eq!(
                    out.get_pixel(x, y).0,
                    src.get_pixel(x, y).0,
                    "the glyph must be byte-identical at ({x}, {y})"
                );
            }
        }
    }

    /// Both halves of the mark must actually be drawn, opaque and red. A stem alone, or a dot alone,
    /// would still look like *something* on screen, so this checks each separately rather than
    /// trusting one probe.
    #[test]
    fn both_halves_of_the_exclamation_are_opaque_red() {
        let out = with_exclamation(&base());
        let (centre_x, stem_y, dot_y, _) = auth_mark_probes();
        for (label, y) in [("stem", stem_y), ("dot", dot_y)] {
            let px = out.get_pixel(centre_x, y);
            assert_eq!(px[3], 255, "{label} must be fully opaque, got {px:?}");
            assert!(px[0] > 200, "{label} must be dominated by red, got {px:?}");
            assert!(px[1] < 100 && px[2] < 100, "{label} must not be washed out, got {px:?}");
        }
    }

    /// The gap is what makes this read as an exclamation mark rather than a solid bar once the icon
    /// is scaled down, and it only does that if it is *fully* transparent.
    ///
    /// This is the assertion that pins `AUTH_MARK_RING_PX`/`AUTH_MARK_RING_FADE_PX` to the gap width.
    /// Twice during development the ring was too narrow, the middle of the gap came out only partly
    /// erased, the octocat's white face showed through, and the mark read as one solid bar at 16px.
    #[test]
    fn the_gap_keeps_stem_and_dot_apart() {
        let out = with_exclamation(&base());
        let (centre_x, .., gap_y) = auth_mark_probes();
        let px = out.get_pixel(centre_x, gap_y);
        assert_eq!(px[3], 0, "the middle of the gap must be fully transparent, got {px:?}");
    }

    /// The mark hugs the right edge but must not be flush against it, which would read as a
    /// rendering accident. Checks there is still clear canvas to its right.
    #[test]
    fn the_mark_leaves_a_margin_at_the_right_edge() {
        let out = with_exclamation(&base());
        let (.., stem_y, _, _) = auth_mark_probes();
        let px = out.get_pixel(out.width() - 1, stem_y);
        assert_eq!(px[3], 0, "the right edge must stay clear of the mark, got {px:?}");
    }

    /// The needs-auth variant is reachable only through its own accessor, and is not a duplicate
    /// of any of the sixteen — otherwise the one state that means "no answers available" would be
    /// indistinguishable from a state that claims answers.
    #[test]
    fn needs_auth_is_distinct_from_every_dotted_variant() {
        let set = build_variants().expect("variants must build");
        assert_eq!(set.needs_auth().as_raw(), set.needs_auth.as_raw());
        for i in 0..16 {
            assert_ne!(
                set.needs_auth.as_raw(),
                set.variants[i].as_raw(),
                "needs_auth must be distinguishable from variant {i:#06b}"
            );
        }
    }

    /// Renders the needs-auth variant to a PNG for eyeballing. No assertion can judge whether a
    /// mark still reads as an exclamation once a taskbar has scaled it to 16px, and several colour
    /// and geometry choices in this file carry a "sanity check on a real render" caveat for exactly
    /// that reason. `#[ignore]`d because its only output is a picture for a human. Run with
    /// `cargo test -- --ignored render_needs_auth --nocapture`.
    #[test]
    #[ignore = "writes a PNG for a human to look at"]
    fn render_needs_auth_for_inspection() {
        let set = build_variants().expect("variants must build");
        let path = std::env::temp_dir().join("github_needs_auth_preview.png");
        set.needs_auth.save(&path).expect("must be able to save the preview");
        println!("wrote {}", path.display());
    }

    #[test]
    fn get_indexes_by_the_matching_bit_pattern() {
        let set = build_variants().expect("variants must build");
        assert_eq!(
            set.get(false, false, false, false).as_raw(),
            set.variants[0b0000].as_raw()
        );
        assert_eq!(set.get(true, false, false, false).as_raw(), set.variants[0b1000].as_raw());
        assert_eq!(set.get(false, true, false, false).as_raw(), set.variants[0b0100].as_raw());
        assert_eq!(set.get(false, false, true, false).as_raw(), set.variants[0b0010].as_raw());
        assert_eq!(set.get(false, false, false, true).as_raw(), set.variants[0b0001].as_raw());
        assert_eq!(set.get(true, true, true, true).as_raw(), set.variants[0b1111].as_raw());
    }

    /// Geometric proof, independent of the pairwise-distinctness test above, that the three dots
    /// cannot possibly overlap: each disc (plus its erase ring) fits entirely within its own
    /// quadrant-ish corner region, with room to spare.
    #[test]
    fn dots_never_overlap_even_with_the_largest_dot() {
        let radius = 98.0 * DOT_RADIUS_RATIO;
        let margin = 98.0 * DOT_MARGIN_RATIO;
        let reach = radius + DOT_RING_PX; // furthest the ring extends from the centre

        let top_right = Corner::TopRight.centre(98.0, 96.0, radius, margin);
        let bottom_right = Corner::BottomRight.centre(98.0, 96.0, radius, margin);
        let bottom_left = Corner::BottomLeft.centre(98.0, 96.0, radius, margin);

        // Top-right vs bottom-right: separated vertically.
        assert!(bottom_right.1 - top_right.1 > 2.0 * reach, "top/bottom-right dots would overlap");
        // Bottom-right vs bottom-left: separated horizontally.
        assert!(bottom_right.0 - bottom_left.0 > 2.0 * reach, "the two bottom dots would overlap");
    }

    #[test]
    fn over_is_a_no_op_at_zero_coverage() {
        let dst = Rgba([1, 2, 3, 200]);
        assert_eq!(over(REVIEW_DOT_COLOR, dst, 0.0), dst);
    }

    #[test]
    fn over_fully_replaces_at_full_coverage() {
        let out = over(REVIEW_DOT_COLOR, Rgba([1, 2, 3, 255]), 1.0);
        assert_eq!(out, Rgba(REVIEW_DOT_COLOR));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn variant_filenames_are_unique_and_encode_the_bits() {
        let names: Vec<String> = (0..16).map(variant_filename).collect();
        let mut sorted = names.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), 16, "every variant must get a distinct filename");

        assert_eq!(variant_filename(0b0000), "github.png");
        assert_eq!(variant_filename(0b1111), "github_blue_review_merge_changes.png");
    }
}
