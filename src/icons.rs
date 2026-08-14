//! Icon management for GitHub Tray Icon.
//!
//! Four independent signals are drawn onto the tray icon, giving sixteen variants:
//!   * base glyph — dark (no unread notifications) or blue (unread notifications)
//!   * review indicator — red, slot 1, when a PR is awaiting your review
//!   * ready-to-merge indicator — green, slot 2, when one of your PRs is approved and passing
//!   * changes-requested indicator — amber, slot 3, when a reviewer asked for changes on one of
//!     your PRs
//!
//! The three indicators are stacked in a column down the right-hand side, as rounded bars twice as
//! wide as they are tall. They used to be discs in three separate corners; one column reads as a
//! single place to look instead of three, and gives a fourth slot somewhere obvious to go.
//!
//! Everything is composited at runtime rather than shipped as extra PNGs, so there is one code
//! path, the variants can never drift between the two base icons, and `assets/` stays at two files.

use image::{Rgba, RgbaImage};

#[cfg(target_os = "linux")]
use crate::logln;
#[cfg(target_os = "linux")]
use std::path::Path;

const GITHUB_ICON: &[u8] = include_bytes!("../assets/github.png");
const GITHUB_BLUE_ICON: &[u8] = include_bytes!("../assets/github_blue.png");

// ── Indicator geometry ──────────────────────────────────────────────────────
// One column of rounded bars down the right-hand side. Ratios rather than pixels so the layout
// survives the source assets being resized, exactly as the old corner discs did.
//
// The sizing is set by one hard constraint, which is worth writing down because it is not obvious:
// what separates two bars is the *transparent gap* between them, and the taskbar scales 98×96 down
// to roughly 16×16. A gap below about 6 source pixels therefore lands under one rendered pixel and
// stops separating anything at all. So the column cannot be made to fit more slots by tightening the
// gaps — extra slots have to come out of the bar height instead.
//
// Four slots at 15px bars and 7px gaps was chosen over 16/6 (red and green merge at 16px) and 13/9
// (bars start to look thin), rendered and compared at 16px and 22px.

/// How many slots the column reserves.
///
/// Deliberately one more than the three signals in use. Reserving the space now is free; discovering
/// later that a fourth signal has nowhere to go means re-tuning every ratio here and re-checking them
/// all at 16px. Slot 4 is laid out but never drawn, so it costs only the height it holds open.
const INDICATOR_SLOTS: usize = 4;
/// Bar height as a fraction of icon height.
const BAR_HEIGHT_RATIO: f32 = 0.156;
/// Bar width as a multiple of its height. Wider than tall so a bar reads as a bar and not a dot.
const BAR_ASPECT: f32 = 2.0;
/// Vertical gap between bars, as a fraction of icon height. See the constraint above before lowering.
const BAR_GAP_RATIO: f32 = 0.073;
/// Gap between the column's right edge and the icon's, as a fraction of icon width.
const BAR_RIGHT_MARGIN_RATIO: f32 = 0.061;
/// Width of the transparent border carved around each bar, in source pixels.
///
/// Not a soft ring: the border is a bar-shaped hole punched to full transparency, one of these
/// larger than the bar itself, so it traces the bar's outline exactly. An earlier version used a
/// distance-field ring with a soft fade, which pinched wherever two bars' rings met and let wedges
/// of the glyph show through the gaps. See `with_indicator_bars` for why the two passes matter.
const BAR_BORDER_PX: f32 = 3.0;

/// Bright red, chosen to hold contrast on both light and dark taskbars.
const REVIEW_DOT_COLOR: [u8; 4] = [0xF0, 0x3E, 0x3E, 0xFF];
/// A clear, saturated green — picked to read distinctly from the review red at a glance, not just
/// on close inspection.
const MERGE_DOT_COLOR: [u8; 4] = [0x1A, 0xC9, 0x4A, 0xFF];
/// An amber/orange, chosen to sit clearly between the review red and the merge green in hue so
/// three bars on screen at once stay distinguishable.
const CHANGES_DOT_COLOR: [u8; 4] = [0xE0, 0x8A, 0x00, 0xFF];

/// The indicator colours, in slot order. Indexed by `state::PrAxis::index`, so slot 1 is
/// review-requested, slot 2 ready-to-merge, slot 3 changes-requested. Slot 4 has no colour because
/// it has no signal yet; `bar_slots` simply never lights it.
const INDICATOR_COLORS: [[u8; 4]; 3] = [REVIEW_DOT_COLOR, MERGE_DOT_COLOR, CHANGES_DOT_COLOR];

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

/// One indicator bar: where it sits and what colour it is.
#[derive(Clone, Copy, Debug)]
struct Bar {
    centre_x: f32,
    centre_y: f32,
    half_w: f32,
    half_h: f32,
    color: [u8; 4],
}

/// Signed distance from a point to a rounded rectangle: negative inside, positive outside, and the
/// magnitude is the distance to the outline. That last property is what lets one expression describe
/// both the bar and the border carved around it — the border is the same shape at a larger offset.
fn rounded_rect_sd(px: f32, py: f32, cx: f32, cy: f32, half_w: f32, half_h: f32, r: f32) -> f32 {
    let dx = (px - cx).abs() - (half_w - r);
    let dy = (py - cy).abs() - (half_h - r);
    dx.max(0.0).hypot(dy.max(0.0)) + dx.max(dy).min(0.0) - r
}

/// The bars to draw for a given set of lit signals, in slot order.
///
/// `lit` is indexed by `state::PrAxis::index`. Slots are *fixed*: an unlit signal leaves its slot
/// empty rather than letting the ones below move up, so a bar's vertical position always means the
/// same thing. The column is laid out for `INDICATOR_SLOTS`, which is one more than `lit` covers, so
/// the reserved slot holds its space open at the bottom.
fn bar_slots(width: f32, height: f32, lit: [bool; 3]) -> Vec<Bar> {
    let bar_h = height * BAR_HEIGHT_RATIO;
    let bar_w = bar_h * BAR_ASPECT;
    let gap = height * BAR_GAP_RATIO;

    let slots = INDICATOR_SLOTS as f32;
    let stack_h = slots * bar_h + (slots - 1.0) * gap;
    // Centred as a block, so reserving a slot shifts the whole column rather than hanging it off
    // one edge.
    let top = (height - stack_h) / 2.0;
    let centre_x = width - width * BAR_RIGHT_MARGIN_RATIO - bar_w / 2.0;

    lit.iter()
        .enumerate()
        .filter(|&(_, &on)| on)
        .map(|(i, _)| Bar {
            centre_x,
            centre_y: top + bar_h / 2.0 + i as f32 * (bar_h + gap),
            half_w: bar_w / 2.0,
            half_h: bar_h / 2.0,
            color: INDICATOR_COLORS[i],
        })
        .collect()
}

/// Returns a copy of `src` with the lit indicator bars drawn down its right-hand side.
///
/// **Two passes, and the order is the whole point.** Every bar's transparent border is carved first,
/// then the colours are painted into the holes. A single pass would let one bar's carve erase the
/// colour a previous bar had already painted, eating a bite out of its neighbour.
///
/// The border is a bar-shaped hole punched to full transparency, `BAR_BORDER_PX` larger than the bar
/// itself, so it traces the outline exactly and two adjacent borders simply merge. The version this
/// replaced used a soft distance-field ring measured to the nearest bar, which pinched wherever two
/// rings met, let wedges of the glyph show through the gaps, and got clipped flat at the icon's edge.
fn with_indicator_bars(src: &RgbaImage, lit: [bool; 3]) -> RgbaImage {
    let (width, height) = src.dimensions();
    let mut out = src.clone();
    let bars = bar_slots(width as f32, height as f32, lit);
    if bars.is_empty() {
        return out;
    }

    // Pass 1 — carve the borders.
    for y in 0..height {
        for x in 0..width {
            // Sample at the pixel centre so the anti-aliasing is symmetric.
            let px = x as f32 + 0.5;
            let py = y as f32 + 0.5;
            for bar in &bars {
                let distance = rounded_rect_sd(
                    px,
                    py,
                    bar.centre_x,
                    bar.centre_y,
                    bar.half_w + BAR_BORDER_PX,
                    bar.half_h + BAR_BORDER_PX,
                    bar.half_h + BAR_BORDER_PX,
                );
                if distance <= 0.5 {
                    let coverage = (0.5 - distance).clamp(0.0, 1.0);
                    let pixel = out.get_pixel_mut(x, y);
                    pixel[3] = (f32::from(pixel[3]) * (1.0 - coverage)).round() as u8;
                }
            }
        }
    }

    // Pass 2 — paint the bars into the holes just carved. Nearest bar wins, which matters only on
    // the anti-aliased edge where two bars' outlines are equidistant.
    for y in 0..height {
        for x in 0..width {
            let px = x as f32 + 0.5;
            let py = y as f32 + 0.5;
            let nearest = bars
                .iter()
                .map(|bar| {
                    let d = rounded_rect_sd(
                        px, py, bar.centre_x, bar.centre_y, bar.half_w, bar.half_h, bar.half_h,
                    );
                    (d, bar.color)
                })
                .min_by(|a, b| a.0.total_cmp(&b.0));

            if let Some((distance, color)) = nearest
                && distance <= 0.5
            {
                let coverage = (0.5 - distance).clamp(0.0, 1.0);
                let pixel = out.get_pixel_mut(x, y);
                *pixel = over(color, *pixel, coverage);
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
        // One call for all three, unlike the per-corner discs this replaced: the bars share a carve
        // pass, and carving them one at a time would let each bar's border bite into the last bar's
        // colour. See `with_indicator_bars`.
        let lit = [i & 0b0100 != 0, i & 0b0010 != 0, i & 0b0001 != 0];
        let base = if unread { &blue } else { &plain };
        with_indicator_bars(base, lit)
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

    /// Centre of slot `i`'s bar for a 98×96 source, mirroring `bar_slots` for test assertions.
    fn slot_centre(i: usize) -> (u32, u32) {
        let bar_h = 96.0 * BAR_HEIGHT_RATIO;
        let gap = 96.0 * BAR_GAP_RATIO;
        let slots = INDICATOR_SLOTS as f32;
        let top = (96.0 - (slots * bar_h + (slots - 1.0) * gap)) / 2.0;
        let centre_x = 98.0 - 98.0 * BAR_RIGHT_MARGIN_RATIO - (bar_h * BAR_ASPECT) / 2.0;
        (centre_x as u32, (top + bar_h / 2.0 + i as f32 * (bar_h + gap)) as u32)
    }

    #[test]
    fn embedded_assets_are_the_expected_shape() {
        let img = base();
        assert_eq!(img.dimensions(), (98, 96));
    }

    #[test]
    fn indicator_bars_preserve_dimensions() {
        let src = base();
        assert_eq!(with_indicator_bars(&src, [true, true, true]).dimensions(), src.dimensions());
    }

    /// Each bar's centre must be fully opaque and dominated by its own colour, proving the slot math
    /// rather than just the first slot.
    #[test]
    fn each_bar_centre_is_opaque_and_its_own_color() {
        let out = with_indicator_bars(&base(), [true, true, true]);
        for (i, dominant) in [(0usize, 0usize), (1, 1), (2, 0)] {
            let (cx, cy) = slot_centre(i);
            let px = out.get_pixel(cx, cy);
            assert_eq!(px[3], 255, "slot {i} centre must be fully opaque, got {px:?}");
            assert!(
                px[dominant] > 200,
                "slot {i} centre must be dominated by channel {dominant}, got {px:?}"
            );
        }
    }

    /// A bar is wider than it is tall, which is the shape change that distinguishes these from the
    /// discs they replaced. Measured on the rendered pixels rather than trusting the constant.
    #[test]
    fn a_bar_is_twice_as_wide_as_it_is_tall() {
        let out = with_indicator_bars(&base(), [true, false, false]);
        let (_, cy) = slot_centre(0);
        let lit = |px: &Rgba<u8>| px[3] > 200 && px[0] > 200 && px[1] < 100;

        let width = (0..98).filter(|&x| lit(out.get_pixel(x, cy))).count();
        let (cx, _) = slot_centre(0);
        let height = (0..96).filter(|&y| lit(out.get_pixel(cx, y))).count();

        let ratio = width as f32 / height as f32;
        assert!(
            (ratio - BAR_ASPECT).abs() < 0.2,
            "bar measured {width}×{height} = {ratio:.2}, expected ≈{BAR_ASPECT}"
        );
    }

    /// The carved border is what keeps a bar legible against the glyph and against an arbitrary
    /// taskbar colour, and it must reach *full* transparency rather than fading.
    #[test]
    fn the_border_around_a_bar_is_fully_transparent() {
        let out = with_indicator_bars(&base(), [true, false, false]);
        let bar_h = 96.0 * BAR_HEIGHT_RATIO;
        let (cx, cy) = slot_centre(0);

        // Straight down from the centre: past the bar's own anti-aliased edge, inside the border.
        let probe_y = (cy as f32 + bar_h / 2.0 + 1.0) as u32;
        let px = out.get_pixel(cx, probe_y);
        assert_eq!(px[3], 0, "border must be erased to full transparency, got {px:?}");

        // …and beyond the border the base icon must be intact again.
        let beyond_y = (cy as f32 + bar_h / 2.0 + BAR_BORDER_PX + 1.5) as u32;
        assert_eq!(
            out.get_pixel(cx, beyond_y).0,
            base().get_pixel(cx, beyond_y).0,
            "the carve must not bleed past the border"
        );
    }

    /// Slots are fixed: lighting one signal must not move any other signal's bar. This is what makes
    /// a bar's vertical position mean something, and it is the property a collapse-upward layout
    /// would trade away.
    #[test]
    fn lighting_one_signal_does_not_move_the_others() {
        let all = with_indicator_bars(&base(), [true, true, true]);
        for (i, lit) in [(0usize, [true, false, false]), (1, [false, true, false]), (2, [false, false, true])] {
            let one = with_indicator_bars(&base(), lit);
            let (cx, cy) = slot_centre(i);
            assert_eq!(
                one.get_pixel(cx, cy).0,
                all.get_pixel(cx, cy).0,
                "slot {i} must render identically whether or not the other slots are lit"
            );
        }
    }

    /// The reserved fourth slot must actually be held open: the three lit bars have to sit in the top
    /// three of four positions, leaving the bottom one empty. If the column silently re-centred on
    /// three, adding a fourth signal later would move every existing bar.
    #[test]
    fn the_fourth_slot_is_reserved_and_left_empty() {
        assert_eq!(INDICATOR_SLOTS, 4, "this test describes a four-slot column");

        let out = with_indicator_bars(&base(), [true, true, true]);
        let src = base();
        let (cx, cy) = slot_centre(3);

        assert_eq!(
            out.get_pixel(cx, cy).0,
            src.get_pixel(cx, cy).0,
            "slot 4 must be untouched — reserved, not drawn"
        );
        // And it must be inside the canvas, or "reserved" would mean nothing.
        assert!(cy < 96, "slot 4 must fall within the icon, got y={cy}");
    }

    /// Nothing lit must leave the base icon completely untouched, so a quiet state is exactly the
    /// plain glyph rather than a glyph with invisible carve damage.
    #[test]
    fn no_signals_lit_leaves_the_glyph_alone() {
        let src = base();
        let out = with_indicator_bars(&src, [false, false, false]);
        assert_eq!(out.as_raw(), src.as_raw());
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

    /// Geometric proof, independent of the rendered-pixel tests above, that the whole column fits.
    ///
    /// Two things have to hold at once and they pull against each other, which is exactly why this is
    /// asserted rather than eyeballed: all four slots plus their borders must fit inside the icon's
    /// height, and the gap between two bars must survive being scaled to a 16px tray slot. Shrink the
    /// gap to buy room for a fifth slot and the second assertion fails.
    #[test]
    fn the_four_slot_column_fits_and_its_gaps_survive_scaling() {
        let bar_h = 96.0 * BAR_HEIGHT_RATIO;
        let gap = 96.0 * BAR_GAP_RATIO;
        let slots = INDICATOR_SLOTS as f32;

        let stack = slots * bar_h + (slots - 1.0) * gap;
        assert!(
            stack + 2.0 * BAR_BORDER_PX <= 96.0,
            "column plus borders is {:.1}px, taller than the 96px icon",
            stack + 2.0 * BAR_BORDER_PX
        );

        // A gap below one rendered pixel separates nothing. 96px of source becomes 16px of tray.
        let rendered_gap = gap * 16.0 / 96.0;
        assert!(
            rendered_gap >= 1.0,
            "gap renders at {rendered_gap:.2}px in a 16px slot, too small to read as a gap"
        );

        // The bars must also clear the icon's right edge rather than bleeding off it.
        let bar_w = bar_h * BAR_ASPECT;
        let right_edge = 98.0 - 98.0 * BAR_RIGHT_MARGIN_RATIO;
        assert!(right_edge <= 98.0 && right_edge - bar_w > 0.0, "the column must fit horizontally");
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

/// Linux-gated because it names its files with `variant_filename`/`NEEDS_AUTH_FILENAME`, which only
/// exist there — libappindicator is the one backend that needs icons on disk. Nothing stops the other
/// platforms from rendering; they just have no filename convention to borrow.
#[cfg(all(test, target_os = "linux"))]
mod render_dump {
    use super::*;

    /// Dumps every built variant to `$GST_ICON_DUMP` for visual inspection.
    ///
    /// No assertion can judge whether a bar still reads as a bar once a taskbar has scaled it to
    /// 16px, and several colour and geometry choices in this file carry a "check on a real render"
    /// caveat for exactly that reason. `#[ignore]`d: it writes files and its only output is pictures
    /// for a human. Run with
    /// `GST_ICON_DUMP=/tmp/icons cargo test -- --ignored dump_all_variants --nocapture`.
    #[test]
    #[ignore = "writes PNGs for a human to look at"]
    fn dump_all_variants() {
        let Ok(dir) = std::env::var("GST_ICON_DUMP") else { return };
        let dir = std::path::PathBuf::from(dir);
        let set = build_variants().expect("variants must build");
        for (i, img) in set.variants.iter().enumerate() {
            img.save(dir.join(variant_filename(i))).expect("save");
        }
        set.needs_auth.save(dir.join(NEEDS_AUTH_FILENAME)).expect("save");
        println!("dumped 17 variants to {}", dir.display());
    }
}
