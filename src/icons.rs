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
//! wide as they are tall, filling nearly the whole height. They used to be discs in three separate
//! corners; one column reads as a single place to look instead of three.
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
// Sized to use nearly all of the available height: three slots at 26px with 7px gaps occupy 92 of 96
// pixels. An earlier four-slot layout reserved a spare slot and used only 81, which left the bars too
// small to notice at the 16px a taskbar actually renders — the reservation was costing the feature that
// exists today to benefit one that does not.
//
// Width is capped by the update arrow, not by the canvas: the arrow sits in the top-left and its colour
// reaches x 38 of 98, so a bar growing leftward would eventually collide with it. At 52px wide the two
// stay 4px apart. `the_arrow_never_reaches_the_indicator_column` is what holds that.

/// How many slots the column has: exactly one per signal.
///
/// A fourth was reserved for a while, for a signal that never arrived. Holding it open cost 22px of
/// height — a quarter of the icon — which came straight out of the bars that do exist. If a fourth
/// signal is ever added, every ratio here has to be re-tuned and re-checked at 16px; that is the price
/// of the bars being legible now, and it is the right way round.
const INDICATOR_SLOTS: usize = 3;
/// Bar height as a fraction of icon height. 26px of 96, so three bars and two gaps fill 92.
const BAR_HEIGHT_RATIO: f32 = 0.271;
/// Bar width as a multiple of its height. Wider than tall so a bar reads as a bar and not a dot.
const BAR_ASPECT: f32 = 2.0;
/// Vertical gap between bars, as a fraction of icon height. See the constraint above before lowering.
const BAR_GAP_RATIO: f32 = 0.073;
/// Gap between the column's right edge and the icon's, as a fraction of icon width.
const BAR_RIGHT_MARGIN_RATIO: f32 = 0.041;
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

// ── Update-available arrow geometry ─────────────────────────────────────────
// An up-arrow in the **top-left** corner, meaning "a newer release exists".
//
// Top-left because it is the one part of the icon nothing else claims: the indicator bars own the
// right-hand column, and the authorization mark owns the right edge. So this needs no negotiation with
// either and can never overlap them.
//
// It is a fifth *independent* signal, unlike the authorization mark which replaces everything. An
// update being available says nothing about your PRs, and vice versa, so both can be true at once and
// the icon has to be able to show both. That is what takes the packed variant index from four bits to
// five — see `IconSet`.

/// The arrow's colour: a bright green, deliberately **not** `MERGE_DOT_COLOR`.
///
/// Green because that is the conventional "there is something good waiting for you" colour, and a
/// *brighter* green than the merge bar because the icon's one rule is that a colour means exactly one
/// thing. Reusing the merge green would leave the icon impossible to describe without also saying where
/// the mark is — "green means an update, unless it is on the right, in which case a PR is mergeable".
/// Position already distinguishes them; the hue not having to do that work as well is what keeps this
/// legible.
const UPDATE_ARROW_COLOR: [u8; 4] = [0x2B, 0xE8, 0x6B, 0xFF];
/// Distance from the icon's top and left edges to the arrow's bounding box, as a fraction of width.
const ARROW_MARGIN_RATIO: f32 = 0.045;
/// Arrow width as a fraction of icon width, and total height as a fraction of icon height.
///
/// The arrow and the bars compete for the middle of the icon, and this is the constant that settles it.
///
/// It was briefly 0.48, which made the arrow unmissable but had two costs that only showed on a real
/// tray: it capped how wide the bars could grow, and it *hollowed out* the octocat's upper left —
/// thinning the ring's top rows from 52 opaque pixels to 28 while the bottom stayed full, so the glyph
/// read as sitting low even though its centroid and bounding box were unchanged. Neither measurement
/// catches that; the eye does.
///
/// At 0.34 the arrow's colour reaches x 38 of 98 and the bars begin at x 42, so the two are 4px apart
/// and the glyph keeps its shape. `the_arrow_never_reaches_the_indicator_column` fails if this grows far
/// enough to collide again — if it does, this is wrong, not the test.
const ARROW_WIDTH_RATIO: f32 = 0.34;
const ARROW_HEIGHT_RATIO: f32 = 0.37;
/// Where the triangular head ends and the shaft begins, as a fraction of the arrow's own height.
/// Above this is head, below is shaft.
const ARROW_HEAD_SPLIT: f32 = 0.55;
/// Shaft width as a fraction of the arrow's own width. Wide enough to survive being scaled to 16px —
/// at 0.38 the head still read but the stem thinned to nothing — and still narrower than the head, which
/// is what `the_arrow_points_up` pins down.
const ARROW_SHAFT_RATIO: f32 = 0.50;
/// Transparent border carved around the arrow, in source pixels. Matches `BAR_BORDER_PX` so the two
/// overlays look like they belong to the same icon.
const ARROW_BORDER_PX: f32 = BAR_BORDER_PX;

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

/// Distance from a point to a line segment. The building block for the arrowhead, whose edges are
/// three segments rather than the axis-aligned box `rounded_rect_sd` handles.
fn segment_distance(px: f32, py: f32, ax: f32, ay: f32, bx: f32, by: f32) -> f32 {
    let (vx, vy) = (bx - ax, by - ay);
    let (wx, wy) = (px - ax, py - ay);
    let len_sq = vx * vx + vy * vy;
    // A degenerate segment collapses to its start point; guarding avoids a divide by zero if a future
    // geometry change ever makes two vertices coincide.
    let t = if len_sq <= f32::EPSILON { 0.0 } else { ((wx * vx + wy * vy) / len_sq).clamp(0.0, 1.0) };
    (wx - t * vx).hypot(wy - t * vy)
}

/// Signed distance to a triangle: negative inside, positive outside, magnitude is the distance to the
/// outline.
///
/// The sign comes from the three edge cross-products agreeing, which works for either winding order, so
/// the caller does not have to remember whether the vertices go clockwise. That property is worth
/// having because "the arrow silently vanished" is what a wrong winding would look like.
fn triangle_sd(px: f32, py: f32, tri: [(f32, f32); 3]) -> f32 {
    let [(ax, ay), (bx, by), (cx, cy)] = tri;
    let distance = segment_distance(px, py, ax, ay, bx, by)
        .min(segment_distance(px, py, bx, by, cx, cy))
        .min(segment_distance(px, py, cx, cy, ax, ay));

    let cross = |x1: f32, y1: f32, x2: f32, y2: f32| x1 * y2 - y1 * x2;
    let s1 = cross(bx - ax, by - ay, px - ax, py - ay);
    let s2 = cross(cx - bx, cy - by, px - bx, py - by);
    let s3 = cross(ax - cx, ay - cy, px - cx, py - cy);
    let inside = (s1 >= 0.0 && s2 >= 0.0 && s3 >= 0.0) || (s1 <= 0.0 && s2 <= 0.0 && s3 <= 0.0);

    if inside { -distance } else { distance }
}

/// Signed distance to the whole up-arrow: the union of its head and its shaft.
///
/// A union of signed distances is their minimum, which is what makes carving the border trivial here.
/// Unlike the bars, where the border had to be built by inflating the rectangle's own half-extents, a
/// true signed distance means "one border-width outside the shape" is just `d <= BORDER`. Same visual
/// result, considerably less arithmetic to get wrong.
fn arrow_sd(px: f32, py: f32, width: f32, height: f32) -> f32 {
    let margin_x = width * ARROW_MARGIN_RATIO;
    let margin_y = width * ARROW_MARGIN_RATIO;
    let arrow_w = width * ARROW_WIDTH_RATIO;
    let arrow_h = height * ARROW_HEIGHT_RATIO;

    let left = margin_x;
    let top = margin_y;
    let centre_x = left + arrow_w / 2.0;
    let split_y = top + arrow_h * ARROW_HEAD_SPLIT;
    let bottom = top + arrow_h;

    // Head: apex up, base at the split.
    let head = triangle_sd(px, py, [(centre_x, top), (left, split_y), (left + arrow_w, split_y)]);

    // Shaft: from the split down to the bottom. Square corners, because a rounded shaft under a sharp
    // head reads as two unrelated shapes.
    let shaft_half_w = arrow_w * ARROW_SHAFT_RATIO / 2.0;
    let shaft_half_h = (bottom - split_y) / 2.0;
    let shaft = rounded_rect_sd(
        px,
        py,
        centre_x,
        split_y + shaft_half_h,
        shaft_half_w,
        shaft_half_h,
        0.0,
    );

    head.min(shaft)
}

/// Returns a copy of `src` with the update-available arrow drawn in its top-left corner.
///
/// Same carve-then-paint treatment as the indicator bars, so the two overlays match: a transparent
/// border is punched out around the shape first, then the colour is laid into it. Here it fits in one
/// pass because there is a single shape — the carve region and the paint region are disjoint, so
/// neither can eat the other. The bars need two passes only because several of them are adjacent.
fn with_update_arrow(src: &RgbaImage) -> RgbaImage {
    let (width, height) = src.dimensions();
    let mut out = src.clone();
    let w = width as f32;
    let h = height as f32;

    for y in 0..height {
        for x in 0..width {
            // Sample at the pixel centre so the anti-aliasing is symmetric, as everywhere else here.
            let distance = arrow_sd(x as f32 + 0.5, y as f32 + 0.5, w, h);

            if distance <= 0.5 {
                let coverage = (0.5 - distance).clamp(0.0, 1.0);
                let pixel = out.get_pixel_mut(x, y);
                *pixel = over(UPDATE_ARROW_COLOR, *pixel, coverage);
            } else if distance <= ARROW_BORDER_PX {
                let pixel = out.get_pixel_mut(x, y);
                pixel[3] = 0;
            }
        }
    }

    out
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
    variants: [T; 32],
    /// Indexed by whether an update is available, for the same reason the array grew: needing
    /// authorization and having an update pending are independent facts, so all four combinations of
    /// "waiting to authorize" and "update available" have to be drawable.
    needs_auth: [T; 2],
}

impl<T> IconSet<T> {
    fn index(unread: bool, review: bool, merge: bool, changes: bool, update: bool) -> usize {
        (usize::from(update) << 4)
            | (usize::from(unread) << 3)
            | (usize::from(review) << 2)
            | (usize::from(merge) << 1)
            | usize::from(changes)
    }

    pub fn get(&self, unread: bool, review: bool, merge: bool, changes: bool, update: bool) -> &T {
        &self.variants[Self::index(unread, review, merge, changes, update)]
    }

    /// The variant shown while a credential is waiting on the user: base glyph, big red exclamation, no
    /// bars. Takes only `update`, because the authorization mark overrides all four PR/notification
    /// signals but *not* the update arrow — an update is still worth showing while you are unauthorized,
    /// and the two live in opposite corners so they never collide.
    pub fn needs_auth(&self, update: bool) -> &T {
        &self.needs_auth[usize::from(update)]
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

    let variants: [RgbaImage; 32] = std::array::from_fn(|i| {
        let unread = i & 0b01000 != 0;
        // One call for all three, unlike the per-corner discs this replaced: the bars share a carve
        // pass, and carving them one at a time would let each bar's border bite into the last bar's
        // colour. See `with_indicator_bars`.
        let lit = [i & 0b00100 != 0, i & 0b00010 != 0, i & 0b00001 != 0];
        let base = if unread { &blue } else { &plain };
        let img = with_indicator_bars(base, lit);
        // Applied last and independently, in the opposite corner from everything else, so it composes
        // with any combination of the four signals below it.
        if i & 0b10000 != 0 { with_update_arrow(&img) } else { img }
    });

    // Built from the plain glyph, never the blue one: the blue tint means "unread notifications",
    // which is exactly the kind of claim this variant exists to withhold.
    let needs_auth_plain = with_exclamation(&plain);
    let needs_auth_update = with_update_arrow(&needs_auth_plain);

    Ok(IconSet { variants, needs_auth: [needs_auth_plain, needs_auth_update] })
}

/// Filename of the needs-authorization variant. A fixed name rather than something
/// `variant_filename` could produce, because it is not addressed by the 4-bit key.
#[cfg(target_os = "linux")]
const NEEDS_AUTH_FILENAME: &str = "github_needs_auth.png";
/// Same, with the update arrow. A fixed name for the same reason: neither is addressed by the packed
/// key.
#[cfg(target_os = "linux")]
const NEEDS_AUTH_UPDATE_FILENAME: &str = "github_needs_auth_update.png";

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
    if i & 0b00001 != 0 {
        name.push_str("_changes");
    }
    // Appended last even though it is the *high* bit, so that adding the update arrow did not rename
    // any of the sixteen files that already existed in users' asset directories. Cosmetic, but it keeps
    // `write_icon_if_changed`'s mtime-stability promise intact across the upgrade instead of rewriting
    // every icon once.
    if i & 0b10000 != 0 {
        name.push_str("_update");
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

/// Creates all thirty-four icon files in the asset directory and returns their paths.
/// Used on Linux by libappindicator, which requires file-system paths.
#[cfg(target_os = "linux")]
pub fn create_icons(app_asset_path: &Path) -> Result<IconSet<String>, String> {
    if let Err(e) = std::fs::create_dir_all(app_asset_path) {
        logln!("failed to create assets directory: {e}");
    }

    let images = build_variants()?;
    let mut paths = Vec::with_capacity(32);
    for (i, image) in images.variants.iter().enumerate() {
        let path = app_asset_path.join(variant_filename(i));
        write_icon_if_changed(&encode_png(image)?, &path);
        paths.push(path.to_string_lossy().into_owned());
    }

    let variants: [String; 32] = paths
        .try_into()
        .map_err(|_| "internal error: expected exactly 32 icon variants".to_string())?;

    // The two needs-authorization variants, which are not addressed by the packed key and so have fixed
    // names rather than generated ones. Indexed by whether the update arrow is drawn.
    let needs_auth_names = [NEEDS_AUTH_FILENAME, NEEDS_AUTH_UPDATE_FILENAME];
    let mut needs_auth_paths: Vec<String> = Vec::with_capacity(2);
    for (image, name) in images.needs_auth.iter().zip(needs_auth_names) {
        let path = app_asset_path.join(name);
        write_icon_if_changed(&encode_png(image)?, &path);
        needs_auth_paths.push(path.to_string_lossy().into_owned());
    }
    let needs_auth: [String; 2] = needs_auth_paths
        .try_into()
        .map_err(|_| "internal error: expected exactly 2 needs-auth variants".to_string())?;

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
    let mut icons = Vec::with_capacity(32);
    for image in &images.variants {
        icons.push(to_icon(image)?);
    }

    let variants: [tray_icon::Icon; 32] = icons
        .try_into()
        .map_err(|_| "internal error: expected exactly 32 icon variants".to_string())?;
    let needs_auth = [to_icon(&images.needs_auth[0])?, to_icon(&images.needs_auth[1])?];
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

    /// One slot per signal, and no more.
    ///
    /// A fourth was reserved for a while and this test asserted it stayed empty. Holding it open cost a
    /// quarter of the icon's height, taken straight out of the bars that do exist, so the reservation
    /// was dropped — and the assertion inverted: there must now be *exactly* as many slots as signals,
    /// because a spare one would be that cost creeping back.
    #[test]
    fn there_is_exactly_one_slot_per_signal() {
        assert_eq!(INDICATOR_SLOTS, INDICATOR_COLORS.len(), "one slot per signal, no spares");
        assert_eq!(INDICATOR_SLOTS, crate::state::PrAxis::ALL.len(), "and one signal per PR axis");
    }

    /// The column is centred as a block, so the empty space is split evenly rather than all landing at
    /// one end. With the stack filling 92 of 96 there is little to split, which is the point.
    #[test]
    fn the_column_is_centred_as_a_block() {
        let out = with_indicator_bars(&base(), [true, true, true]);
        let is_bar = |px: &Rgba<u8>| px[3] > 200 && INDICATOR_COLORS.contains(&px.0);

        let rows: Vec<u32> = (0..96).filter(|&y| (0..98).any(|x| is_bar(out.get_pixel(x, y)))).collect();
        let (top, bottom) = (rows[0], rows[rows.len() - 1]);
        let (above, below) = (top, 95 - bottom);
        assert!(
            (above as i32 - below as i32).abs() <= 1,
            "column sits {above}px from the top and {below}px from the bottom"
        );
        assert!(above <= 4, "the column should use nearly the whole height, {above}px is too much slack");
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
    fn all_thirty_two_variants_build_and_are_pairwise_distinct() {
        let set = build_variants().expect("variants must build");
        for i in 0..32 {
            for j in (i + 1)..32 {
                assert_ne!(
                    set.variants[i].as_raw(),
                    set.variants[j].as_raw(),
                    "variants {i:#07b} and {j:#07b} must be distinguishable"
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

    /// The needs-auth variants are reachable only through their own accessor, and are duplicates of
    /// nothing in the array — otherwise the one state that means "no answers available" would be
    /// indistinguishable from a state that claims answers.
    #[test]
    fn needs_auth_is_distinct_from_every_indicator_variant() {
        let set = build_variants().expect("variants must build");
        for update in [false, true] {
            let via_accessor = set.needs_auth(update).as_raw();
            assert_eq!(via_accessor, set.needs_auth[usize::from(update)].as_raw());
            for i in 0..32 {
                assert_ne!(
                    via_accessor,
                    set.variants[i].as_raw(),
                    "needs_auth(update={update}) must differ from variant {i:#07b}"
                );
            }
        }
        // And the two must differ from each other, or the arrow would be invisible in this state.
        assert_ne!(set.needs_auth(false).as_raw(), set.needs_auth(true).as_raw());
    }

    // ── The update-available arrow ──────────────────────────────────────────

    /// A point inside the arrow's shaft, and one inside its head, for a 98×96 source.
    fn arrow_probes() -> ((u32, u32), (u32, u32)) {
        let arrow_w = 98.0 * ARROW_WIDTH_RATIO;
        let arrow_h = 96.0 * ARROW_HEIGHT_RATIO;
        let left = 98.0 * ARROW_MARGIN_RATIO;
        let top = 98.0 * ARROW_MARGIN_RATIO;
        let centre_x = left + arrow_w / 2.0;
        let split = top + arrow_h * ARROW_HEAD_SPLIT;
        // Head: just below the apex, on the centre line, where the triangle is already wide enough to
        // contain a whole pixel.
        let head = (centre_x as u32, (top + arrow_h * 0.40) as u32);
        // Shaft: midway between the split and the bottom.
        let shaft = (centre_x as u32, ((split + top + arrow_h) / 2.0) as u32);
        (head, shaft)
    }

    #[test]
    fn the_arrow_preserves_dimensions() {
        let src = base();
        assert_eq!(with_update_arrow(&src).dimensions(), src.dimensions());
    }

    /// Both parts have to be drawn. A head with no shaft is a triangle, and a shaft with no head is a
    /// bar — neither reads as "up".
    #[test]
    fn both_parts_of_the_arrow_are_opaque_and_the_arrow_colour() {
        let out = with_update_arrow(&base());
        let (head, shaft) = arrow_probes();
        for (label, (x, y)) in [("head", head), ("shaft", shaft)] {
            let px = out.get_pixel(x, y);
            assert_eq!(px[3], 255, "the {label} must be fully opaque, got {px:?}");
            assert_eq!(px.0, UPDATE_ARROW_COLOR, "the {label} must be the arrow colour, got {px:?}");
        }
    }

    /// The arrow is in the top-left; the indicator bars are in the right-hand column. They must not
    /// touch, or two independent signals would be corrupting each other's pixels.
    #[test]
    fn the_arrow_never_reaches_the_indicator_column() {
        let with_bars = with_indicator_bars(&base(), [true, true, true]);
        let with_both = with_update_arrow(&with_bars);

        // Every bar pixel must survive the arrow being drawn over the same image.
        for i in 0..3 {
            let (cx, cy) = slot_centre(i);
            assert_eq!(
                with_both.get_pixel(cx, cy).0,
                with_bars.get_pixel(cx, cy).0,
                "the arrow must not disturb indicator slot {i}"
            );
        }

        // And the reverse: the arrow's own pixels must survive too, which is what proves the two
        // overlays are genuinely composable rather than merely usually-not-overlapping.
        let arrow_only = with_update_arrow(&base());
        let ((hx, hy), (sx, sy)) = arrow_probes();
        for (x, y) in [(hx, hy), (sx, sy)] {
            assert_eq!(
                with_both.get_pixel(x, y).0,
                arrow_only.get_pixel(x, y).0,
                "the bars must not disturb the arrow"
            );
        }
    }

    /// Same carved-border treatment as the bars, so the arrow stays legible against the glyph and any
    /// taskbar colour rather than blending into whichever is behind it.
    #[test]
    fn the_arrow_has_a_fully_transparent_border() {
        let out = with_update_arrow(&base());
        let ((hx, _), (sx, sy)) = arrow_probes();
        let _ = hx;
        let arrow_w = 98.0 * ARROW_WIDTH_RATIO;
        let shaft_half = arrow_w * ARROW_SHAFT_RATIO / 2.0;

        // Just outside the shaft's left edge: past its anti-aliased boundary, inside the carve.
        let probe_x = (sx as f32 - shaft_half - 1.5) as u32;
        let px = out.get_pixel(probe_x, sy);
        assert_eq!(px[3], 0, "the border must be erased to full transparency, got {px:?}");
    }

    /// The signed distance must be negative inside and positive outside, for either winding order. A
    /// sign error would make the arrow silently vanish, which no other test would obviously catch.
    #[test]
    fn triangle_distance_is_signed_consistently_for_either_winding() {
        let clockwise = [(10.0, 0.0), (0.0, 20.0), (20.0, 20.0)];
        let anticlockwise = [(10.0, 0.0), (20.0, 20.0), (0.0, 20.0)];
        for tri in [clockwise, anticlockwise] {
            assert!(triangle_sd(10.0, 15.0, tri) < 0.0, "the centroid must read as inside");
            assert!(triangle_sd(10.0, 40.0, tri) > 0.0, "far below must read as outside");
            assert!(triangle_sd(-10.0, 10.0, tri) > 0.0, "far left must read as outside");
        }
    }

    /// The arrow points **up**: its widest row is in the head, above the shaft, and it narrows towards
    /// the bottom. Measured on rendered pixels rather than trusting the constants, so an inverted
    /// geometry change fails here rather than shipping a down-arrow.
    #[test]
    fn the_arrow_points_up() {
        let out = with_update_arrow(&base());
        let is_arrow = |px: &Rgba<u8>| px.0 == UPDATE_ARROW_COLOR;
        let width_at = |y: u32| (0..98).filter(|&x| is_arrow(out.get_pixel(x, y))).count();

        let ((_, head_y), (_, shaft_y)) = arrow_probes();
        let head_width = width_at(head_y);
        let shaft_width = width_at(shaft_y);

        assert!(head_width > 0 && shaft_width > 0, "both bands must be drawn");
        assert!(
            head_width > shaft_width,
            "the head ({head_width}px) must be wider than the shaft ({shaft_width}px), or the arrow \
             is pointing the wrong way"
        );
    }

    /// The arrow composes with the sixteen indicator combinations rather than replacing them, which is
    /// the whole reason the index grew a bit instead of getting a seventeenth slot.
    #[test]
    fn every_variant_has_an_arrow_bearing_twin() {
        let set = build_variants().expect("variants must build");
        for base_index in 0..16 {
            let with_arrow = base_index | 0b10000;
            assert_ne!(
                set.variants[base_index].as_raw(),
                set.variants[with_arrow].as_raw(),
                "variant {base_index:#07b} and its arrow twin must differ"
            );
        }
    }

    #[test]
    fn get_indexes_by_the_matching_bit_pattern() {
        let set = build_variants().expect("variants must build");
        assert_eq!(
            set.get(false, false, false, false, false).as_raw(),
            set.variants[0b0000].as_raw()
        );
        assert_eq!(set.get(true, false, false, false, false).as_raw(), set.variants[0b01000].as_raw());
        assert_eq!(set.get(false, true, false, false, false).as_raw(), set.variants[0b00100].as_raw());
        assert_eq!(set.get(false, false, true, false, false).as_raw(), set.variants[0b00010].as_raw());
        assert_eq!(set.get(false, false, false, true, false).as_raw(), set.variants[0b00001].as_raw());
        assert_eq!(set.get(false, false, false, false, true).as_raw(), set.variants[0b10000].as_raw());
        assert_eq!(set.get(true, true, true, true, true).as_raw(), set.variants[0b11111].as_raw());
    }

    /// Geometric proof, independent of the rendered-pixel tests above, that the column fits.
    ///
    /// Two things have to hold at once and they pull against each other, which is exactly why this is
    /// asserted rather than eyeballed: the three bars must fit inside the icon's height, and the gap
    /// between two of them must survive being scaled to a 16px tray slot. Shrink the gap to buy room
    /// for a fourth slot and the second assertion fails.
    #[test]
    fn the_column_fits_and_its_gaps_survive_scaling() {
        let bar_h = 96.0 * BAR_HEIGHT_RATIO;
        let gap = 96.0 * BAR_GAP_RATIO;
        let slots = INDICATOR_SLOTS as f32;

        let stack = slots * bar_h + (slots - 1.0) * gap;
        assert!(stack <= 96.0, "the bars themselves are {stack:.1}px, taller than the 96px icon");
        // Deliberately *not* `stack + 2 * BAR_BORDER_PX <= 96.0`. With the column filling nearly the
        // whole height the top and bottom bars' transparent borders run off the edge and are clipped,
        // exactly as the authorization mark's ring already is. Clipping a transparent border costs
        // nothing; leaving 6px of height unused to avoid it would cost the bars their legibility.
        assert!(stack > 96.0 - 3.0 * bar_h, "the column should use most of the height, not a third");

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
        set.needs_auth[0].save(dir.join(NEEDS_AUTH_FILENAME)).expect("save");
        set.needs_auth[1].save(dir.join(NEEDS_AUTH_UPDATE_FILENAME)).expect("save");
        println!("dumped 34 variants to {}", dir.display());
    }
}
