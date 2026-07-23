//! Markup annotation model: the active tool, the annotation list (undo/redo
//! via an index pointer into an append-only `Vec`, redrawn from scratch each
//! frame — the egui-idiomatic way to do this rather than mutating a pixel
//! buffer), and the pixel-processing helpers for Blur/Invert Colors.
//!
//! All annotation geometry is stored in the overlay window's local
//! coordinate space (the same space `to_local()` in `overlay.rs` produces),
//! since that's a fixed offset from virtual-desktop coordinates for the
//! lifetime of a single show.

use eframe::egui;

use crate::icons::Icon;

/// The active markup tool. `MagicErase` samples the color under the start of
/// the stroke and paints with it — not true content-aware fill (that's
/// deferred to a stretch milestone).
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub enum Tool {
    Draw,
    Erase,
    Highlight,
    Box,
    Circle,
    Arrow,
    Text,
    Number,
    Blur,
    InvertColors,
    MagicErase,
}

impl Tool {
    /// Every tool, in toolbar display order.
    pub const ALL: [Tool; 11] = [
        Tool::Draw,
        Tool::Erase,
        Tool::Highlight,
        Tool::Box,
        Tool::Circle,
        Tool::Arrow,
        Tool::Text,
        Tool::Number,
        Tool::Blur,
        Tool::InvertColors,
        Tool::MagicErase,
    ];

    /// Lucide icon (https://lucide.dev/icons) for this tool's toolbar button.
    pub fn icon(self) -> Icon {
        match self {
            Tool::Draw => Icon::Pencil,
            Tool::Erase => Icon::Eraser,
            Tool::Highlight => Icon::Highlighter,
            Tool::Box => Icon::Square,
            Tool::Circle => Icon::Circle,
            Tool::Arrow => Icon::MoveUpRight,
            Tool::Text => Icon::Type,
            Tool::Number => Icon::ListOrdered,
            Tool::Blur => Icon::Blend,
            Tool::InvertColors => Icon::Contrast,
            Tool::MagicErase => Icon::WandSparkles,
        }
    }

    /// Toolbar hover tooltip text for this tool.
    pub fn tooltip(self) -> &'static str {
        match self {
            Tool::Draw => "Freeform draw",
            Tool::Erase => "Erase (click/drag over an annotation to remove it)",
            Tool::Highlight => "Highlight",
            Tool::Box => "Box",
            Tool::Circle => "Circle",
            Tool::Arrow => "Arrow",
            Tool::Text => "Text",
            Tool::Number => "Sequential numbered badge",
            Tool::Blur => "Blur region",
            Tool::InvertColors => "Invert colors in region",
            Tool::MagicErase => "Magic erase (paints with the color under where your stroke starts)",
        }
    }
}

/// One committed piece of markup. Immutable once created; undo/redo just
/// moves an index pointer over this list rather than mutating anything.
#[derive(Clone)]
pub enum Annotation {
    /// A freehand Draw, Highlight, or Magic-Erase path. `highlight` selects
    /// the semi-transparent, thicker rendering used by the Highlight tool
    /// (see `paint_annotations`/`bake_annotations`) — Draw and Magic-Erase
    /// both use `highlight: false` and are otherwise indistinguishable at
    /// this layer (Magic-Erase's "paint with the sampled color" behavior is
    /// just a `color` chosen at stroke-start time by the caller).
    Stroke {
        points: Vec<egui::Pos2>,
        color: egui::Color32,
        width: f32,
        highlight: bool,
    },
    /// An axis-aligned rectangle outline (Box tool).
    Box {
        rect: egui::Rect,
        color: egui::Color32,
        width: f32,
    },
    /// An ellipse outline inscribed in `rect` (Circle tool).
    Circle {
        rect: egui::Rect,
        color: egui::Color32,
        width: f32,
    },
    /// A straight line with an arrowhead at `to` (Arrow tool).
    Arrow {
        from: egui::Pos2,
        to: egui::Pos2,
        color: egui::Color32,
        width: f32,
    },
    /// A free-text label anchored at its top-left corner (Text tool).
    Text {
        pos: egui::Pos2,
        text: String,
        color: egui::Color32,
        size: f32,
    },
    /// A filled, numbered circular badge, incrementing per placement
    /// (Number tool).
    Number {
        pos: egui::Pos2,
        n: u32,
        color: egui::Color32,
    },
    /// A pre-processed pixel patch (Blur or Invert Colors), baked once at
    /// creation time and just re-drawn as a texture afterward.
    Processed { rect: egui::Rect, image: image::RgbaImage },
}

impl Annotation {
    /// Rough hit-test used by the Erase tool: is `pos` close enough to this
    /// annotation's geometry to count as "erasing" it?
    pub fn hit_test(&self, pos: egui::Pos2, tolerance: f32) -> bool {
        match self {
            Annotation::Stroke { points, width, .. } => points
                .windows(2)
                .any(|w| distance_to_segment(pos, w[0], w[1]) <= tolerance + width),
            Annotation::Box { rect, width, .. } | Annotation::Circle { rect, width, .. } => {
                distance_to_rect_edge(pos, *rect) <= tolerance + width
            }
            Annotation::Arrow { from, to, width, .. } => distance_to_segment(pos, *from, *to) <= tolerance + width,
            Annotation::Text { pos: p, size, .. } => p.distance(pos) <= tolerance + size,
            Annotation::Number { pos: p, .. } => p.distance(pos) <= tolerance + 14.0,
            Annotation::Processed { rect, .. } => rect.contains(pos),
        }
    }
}

fn distance_to_segment(p: egui::Pos2, a: egui::Pos2, b: egui::Pos2) -> f32 {
    let ab = b - a;
    let len_sq = ab.length_sq();
    if len_sq <= f32::EPSILON {
        return p.distance(a);
    }
    let t = ((p - a).dot(ab) / len_sq).clamp(0.0, 1.0);
    let closest = a + ab * t;
    p.distance(closest)
}

fn distance_to_rect_edge(p: egui::Pos2, rect: egui::Rect) -> f32 {
    let edges = [
        (rect.left_top(), rect.right_top()),
        (rect.right_top(), rect.right_bottom()),
        (rect.right_bottom(), rect.left_bottom()),
        (rect.left_bottom(), rect.left_top()),
    ];
    edges
        .iter()
        .map(|(a, b)| distance_to_segment(p, *a, *b))
        .fold(f32::INFINITY, f32::min)
}

/// Lightly smooths a freehand point path with a 3-point moving average
/// (each interior point becomes a weighted blend of itself and its two
/// neighbors; the endpoints are left untouched so the stroke's start/end
/// don't drift). Cheap enough to run every frame on the already-decimated
/// point list a live stroke accumulates. A no-op (returns the input as-is)
/// for anything too short to smooth.
pub fn smooth_points(points: &[egui::Pos2]) -> Vec<egui::Pos2> {
    if points.len() < 3 {
        return points.to_vec();
    }
    let mut out = Vec::with_capacity(points.len());
    out.push(points[0]);
    for i in 1..points.len() - 1 {
        let blended = (points[i - 1].to_vec2() + points[i].to_vec2() * 2.0 + points[i + 1].to_vec2()) / 4.0;
        out.push(blended.to_pos2());
    }
    out.push(points[points.len() - 1]);
    out
}

/// Paint every active annotation (`annotations[..active_count]`) onto
/// `painter`. `textures` is a parallel cache (same length as `annotations`)
/// for the `Processed` variant's lazily-created egui texture. `smooth`
/// mirrors `Config::smooth_strokes` — see `smooth_points`.
pub fn paint_annotations(
    ctx: &egui::Context,
    painter: &egui::Painter,
    annotations: &[Annotation],
    active_count: usize,
    textures: &mut [Option<egui::TextureHandle>],
    smooth: bool,
) {
    for (i, ann) in annotations.iter().enumerate().take(active_count) {
        match ann {
            Annotation::Stroke {
                points,
                color,
                width,
                highlight,
            } => {
                if points.len() < 2 {
                    continue;
                }
                let (draw_color, draw_width) = if *highlight {
                    (
                        egui::Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 90),
                        width * 4.0,
                    )
                } else {
                    (*color, *width)
                };
                let drawn = if smooth { smooth_points(points) } else { points.clone() };
                painter.add(egui::Shape::line(drawn, egui::Stroke::new(draw_width, draw_color)));
            }
            Annotation::Box { rect, color, width } => {
                painter.rect_stroke(*rect, 0.0, egui::Stroke::new(*width, *color), egui::StrokeKind::Inside);
            }
            Annotation::Circle { rect, color, width } => {
                painter.add(egui::Shape::ellipse_stroke(
                    rect.center(),
                    rect.size() / 2.0,
                    egui::Stroke::new(*width, *color),
                ));
            }
            Annotation::Arrow { from, to, color, width } => {
                draw_arrow(painter, *from, *to, *color, *width);
            }
            Annotation::Text { pos, text, color, size } => {
                painter.text(
                    *pos,
                    egui::Align2::LEFT_TOP,
                    text,
                    egui::FontId::proportional(*size),
                    *color,
                );
            }
            Annotation::Number { pos, n, color } => {
                let radius = 14.0;
                painter.circle_filled(*pos, radius, *color);
                painter.circle_stroke(*pos, radius, egui::Stroke::new(1.5, egui::Color32::WHITE));
                painter.text(
                    *pos,
                    egui::Align2::CENTER_CENTER,
                    n.to_string(),
                    egui::FontId::proportional(14.0),
                    egui::Color32::WHITE,
                );
            }
            Annotation::Processed { rect, image } => {
                let tex = textures[i].get_or_insert_with(|| {
                    let color_image = egui::ColorImage::from_rgba_unmultiplied(
                        [image.width() as usize, image.height() as usize],
                        image.as_raw(),
                    );
                    ctx.load_texture(format!("annotation-{i}"), color_image, egui::TextureOptions::LINEAR)
                });
                painter.image(
                    tex.id(),
                    *rect,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    egui::Color32::WHITE,
                );
            }
        }
    }
}

/// Public so `overlay.rs` can reuse the exact same arrowhead geometry for
/// the live drag preview, rather than duplicating it.
pub fn draw_arrow(painter: &egui::Painter, from: egui::Pos2, to: egui::Pos2, color: egui::Color32, width: f32) {
    let stroke = egui::Stroke::new(width, color);
    painter.line_segment([from, to], stroke);

    let direction = to - from;
    if direction.length_sq() < 1.0 {
        return;
    }
    let dir = direction.normalized();
    let head_len = (width * 5.0).max(10.0);
    let head_angle = 0.5_f32; // radians
    let left = egui::vec2(
        dir.x * head_angle.cos() - dir.y * head_angle.sin(),
        dir.x * head_angle.sin() + dir.y * head_angle.cos(),
    );
    let right = egui::vec2(
        dir.x * (-head_angle).cos() - dir.y * (-head_angle).sin(),
        dir.x * (-head_angle).sin() + dir.y * (-head_angle).cos(),
    );
    painter.line_segment([to, to - left * head_len], stroke);
    painter.line_segment([to, to - right * head_len], stroke);
}

/// Apply a gaussian blur to `image` (used by the Blur tool). `strength` is
/// the tool's existing stroke-width slider repurposed as a blur-intensity
/// control (rather than adding a second, tool-specific slider to the
/// toolbar) — scaled and clamped so the slider's usual 1-20 range maps to a
/// sigma that actually obscures text/detail rather than reading as
/// barely-there.
pub fn blur_image(image: &image::RgbaImage, strength: f32) -> image::RgbaImage {
    let sigma = (strength * 2.5).clamp(4.0, 60.0);
    imageproc::filter::gaussian_blur_f32(image, sigma)
}

/// Invert every pixel's RGB (alpha untouched) — used by the Invert Colors
/// tool.
pub fn invert_image(image: &image::RgbaImage) -> image::RgbaImage {
    let mut out = image.clone();
    for px in out.pixels_mut() {
        px.0[0] = 255 - px.0[0];
        px.0[1] = 255 - px.0[1];
        px.0[2] = 255 - px.0[2];
    }
    out
}

/// Bake every active annotation onto `image` as real pixels — used when
/// producing the actual Copy/Save/Autosave/Pin output, since those need the
/// markup burned in rather than just painted on top for on-screen display.
/// `origin` is the annotation-coordinate-space point that maps to `image`'s
/// (0,0) — i.e. the selection's local top-left corner, since annotations are
/// stored in the overlay window's full local coordinate space, not relative
/// to the crop.
///
/// Loads Segoe UI directly from the Windows fonts folder for text rendering
/// rather than bundling a font asset — reasonable since this is a
/// Windows-only app to begin with (see [[project-status]]). Loaded once and
/// cached: this used to re-read the font file from disk on every single
/// call, and since `bake_annotations` runs synchronously after every
/// committed annotation (see `autosave_current_selection` in `overlay.rs`),
/// that repeated disk I/O was part of what made the UI thread stall for a
/// moment after finishing each stroke.
pub fn bake_annotations(
    image: &mut image::RgbaImage,
    annotations: &[Annotation],
    active_count: usize,
    origin: egui::Vec2,
    smooth: bool,
) {
    let font = cached_font();

    for ann in annotations.iter().take(active_count) {
        match ann {
            Annotation::Stroke {
                points,
                color,
                width,
                highlight,
            } => {
                let drawn = if smooth { smooth_points(points) } else { points.clone() };
                if *highlight {
                    draw_highlight_stroke(image, &drawn, origin, *color, width * 4.0);
                } else {
                    let px = to_rgba(*color, 255);
                    for pair in drawn.windows(2) {
                        let a = pair[0] - origin;
                        let b = pair[1] - origin;
                        draw_thick_line(image, (a.x, a.y), (b.x, b.y), px, *width);
                    }
                }
            }
            Annotation::Box { rect, color, width } => {
                let r = translate_rect(*rect, origin);
                draw_thick_rect(image, r, to_rgba(*color, 255), *width);
            }
            Annotation::Circle { rect, color, width } => {
                let r = translate_rect(*rect, origin);
                let center = (r.center().x as i32, r.center().y as i32);
                let rx = (r.width() / 2.0) as i32;
                let ry = (r.height() / 2.0) as i32;
                for _ in 0..(*width as i32).max(1) {
                    imageproc::drawing::draw_hollow_ellipse_mut(image, center, rx, ry, to_rgba(*color, 255));
                }
            }
            Annotation::Arrow { from, to, color, width } => {
                let a = *from - origin;
                let b = *to - origin;
                let px = to_rgba(*color, 255);
                draw_thick_line(image, (a.x, a.y), (b.x, b.y), px, *width);
                for (hx, hy) in arrow_head_points(a, b, *width) {
                    draw_thick_line(image, (b.x, b.y), (hx, hy), px, *width);
                }
            }
            Annotation::Text { pos, text, color, size } => {
                if let Some(font) = font {
                    let p = *pos - origin;
                    imageproc::drawing::draw_text_mut(
                        image,
                        to_rgba(*color, 255),
                        p.x as i32,
                        p.y as i32,
                        ab_glyph::PxScale::from(*size),
                        font,
                        text,
                    );
                }
            }
            Annotation::Number { pos, n, color } => {
                let p = *pos - origin;
                imageproc::drawing::draw_filled_circle_mut(image, (p.x as i32, p.y as i32), 14, to_rgba(*color, 255));
                if let Some(font) = font {
                    let label = n.to_string();
                    let x_offset = if label.len() > 1 { 9 } else { 5 };
                    imageproc::drawing::draw_text_mut(
                        image,
                        to_rgba(egui::Color32::WHITE, 255),
                        p.x as i32 - x_offset,
                        p.y as i32 - 8,
                        ab_glyph::PxScale::from(16.0),
                        font,
                        &label,
                    );
                }
            }
            Annotation::Processed { rect, image: patch } => {
                let r = translate_rect(*rect, origin);
                image::imageops::overlay(image, patch, r.left() as i64, r.top() as i64);
            }
        }
    }
}

fn to_rgba(color: egui::Color32, alpha: u8) -> image::Rgba<u8> {
    image::Rgba([color.r(), color.g(), color.b(), alpha])
}

/// Bakes a Highlight stroke as one flat translucent tint over whatever's
/// underneath, rather than stamping overlapping filled circles directly onto
/// `image` the way `draw_thick_line` does for opaque strokes.
///
/// `imageproc::drawing::draw_filled_circle_mut` draws onto a plain
/// `image::RgbaImage` by overwriting pixels (`Canvas::draw_pixel` ==
/// `put_pixel`, not alpha blending — blending needs the `imageproc::Blend`
/// wrapper). For an opaque stroke that's fine, but a highlight's
/// `to_rgba(color, 90)` pixel was being written raw: the real screenshot
/// pixel underneath was destroyed rather than blended with, so the "see
/// through" effect only ever existed in the alpha byte, not in the RGB —
/// once that alpha channel is dropped or ignored (clipboard paste, JPEG
/// export, most viewers), what's left is solid highlight color. Rendering
/// the whole stroke as an opaque coverage mask first and compositing it onto
/// the real image in a single blend pass (instead of blending per
/// overlapping circle stamp, which would compound alpha well past 90/255
/// wherever stamps overlap) fixes both problems at once.
fn draw_highlight_stroke(
    image: &mut image::RgbaImage,
    points: &[egui::Pos2],
    origin: egui::Vec2,
    color: egui::Color32,
    width: f32,
) {
    if points.len() < 2 {
        return;
    }
    let half = (width / 2.0).max(0.5);
    let radius = half.round().max(1.0) as i32;

    let translated: Vec<(f32, f32)> = points.iter().map(|p| ((*p - origin).x, (*p - origin).y)).collect();
    let mut min_x = f32::MAX;
    let mut min_y = f32::MAX;
    let mut max_x = f32::MIN;
    let mut max_y = f32::MIN;
    for (x, y) in &translated {
        min_x = min_x.min(x - radius as f32);
        max_x = max_x.max(x + radius as f32);
        min_y = min_y.min(y - radius as f32);
        max_y = max_y.max(y + radius as f32);
    }
    let min_x = (min_x.floor().max(0.0) as i64).min(image.width() as i64);
    let min_y = (min_y.floor().max(0.0) as i64).min(image.height() as i64);
    let max_x = (max_x.ceil() as i64).clamp(0, image.width() as i64);
    let max_y = (max_y.ceil() as i64).clamp(0, image.height() as i64);
    if max_x <= min_x || max_y <= min_y {
        return;
    }
    let w = (max_x - min_x) as u32;
    let h = (max_y - min_y) as u32;

    let mut mask = image::RgbaImage::new(w, h);
    let opaque = image::Rgba([255, 255, 255, 255]);
    for pair in translated.windows(2) {
        let a = (pair[0].0 - min_x as f32, pair[0].1 - min_y as f32);
        let b = (pair[1].0 - min_x as f32, pair[1].1 - min_y as f32);
        draw_thick_line(&mut mask, a, b, opaque, width);
    }

    let tint = to_rgba(color, 90);
    let alpha = tint.0[3] as f32 / 255.0;
    for y in 0..h {
        for x in 0..w {
            if mask.get_pixel(x, y).0[3] == 0 {
                continue;
            }
            let ix = min_x as u32 + x;
            let iy = min_y as u32 + y;
            let bg = *image.get_pixel(ix, iy);
            let blend = |s: u8, d: u8| (s as f32 * alpha + d as f32 * (1.0 - alpha)).round() as u8;
            image.put_pixel(
                ix,
                iy,
                image::Rgba([
                    blend(tint.0[0], bg.0[0]),
                    blend(tint.0[1], bg.0[1]),
                    blend(tint.0[2], bg.0[2]),
                    bg.0[3],
                ]),
            );
        }
    }
}

/// Loaded from disk once (first call) and cached for the rest of the
/// process's lifetime. `pub(crate)` so `recording_border` can reuse it for
/// the "REC mm:ss" label rather than loading its own copy.
pub(crate) fn cached_font() -> Option<&'static ab_glyph::FontArc> {
    static FONT: std::sync::OnceLock<Option<ab_glyph::FontArc>> = std::sync::OnceLock::new();
    FONT.get_or_init(|| {
        std::fs::read("C:\\Windows\\Fonts\\segoeui.ttf")
            .ok()
            .and_then(|bytes| ab_glyph::FontArc::try_from_vec(bytes).ok())
    })
    .as_ref()
}

fn translate_rect(rect: egui::Rect, offset: egui::Vec2) -> egui::Rect {
    egui::Rect::from_min_max(rect.min - offset, rect.max - offset)
}

fn arrow_head_points(from: egui::Pos2, to: egui::Pos2, width: f32) -> [(f32, f32); 2] {
    let direction = to - from;
    if direction.length_sq() < 1.0 {
        return [(to.x, to.y), (to.x, to.y)];
    }
    let dir = direction.normalized();
    let head_len = (width * 5.0).max(10.0);
    let angle = 0.5_f32;
    let rotate = |v: egui::Vec2, a: f32| egui::vec2(v.x * a.cos() - v.y * a.sin(), v.x * a.sin() + v.y * a.cos());
    let left = to - rotate(dir, angle) * head_len;
    let right = to - rotate(dir, -angle) * head_len;
    [(left.x, left.y), (right.x, right.y)]
}

/// Draws a stroke-width line by densely stamping overlapping filled circles
/// along the segment — the same technique a raster paint program's round
/// brush uses. `imageproc` has no native width parameter for line drawing;
/// two other approaches were tried and discarded here: parallel 1px offset
/// lines per segment left visible gaps at every joint on a curve (each short
/// segment computed its own independent offset), and a filled
/// rectangle-plus-end-cap-circles "capsule" per segment still showed a
/// subtly scalloped/beaded outline on tight curves (each segment's circle
/// peeking out next to its neighbor's, rather than blending into one smooth
/// tube). Stamping circles closely enough together that each overlaps the
/// last by at least half its radius has no seams to begin with — there's no
/// per-segment shape boundary for consecutive stamps to disagree about.
fn draw_thick_line(image: &mut image::RgbaImage, a: (f32, f32), b: (f32, f32), color: image::Rgba<u8>, width: f32) {
    let half = (width / 2.0).max(0.5);
    let radius = half.round().max(1.0) as i32;
    let dx = b.0 - a.0;
    let dy = b.1 - a.1;
    let len = (dx * dx + dy * dy).sqrt();

    if len < 0.001 {
        imageproc::drawing::draw_filled_circle_mut(image, (a.0.round() as i32, a.1.round() as i32), radius, color);
        return;
    }

    let step = (half * 0.5).max(1.0);
    let steps = (len / step).ceil().max(1.0) as i32;
    for i in 0..=steps {
        let t = i as f32 / steps as f32;
        let x = (a.0 + dx * t).round() as i32;
        let y = (a.1 + dy * t).round() as i32;
        imageproc::drawing::draw_filled_circle_mut(image, (x, y), radius, color);
    }
}

/// Approximate a stroke-width rect outline by drawing several nested hollow
/// rects inset from the edge.
fn draw_thick_rect(image: &mut image::RgbaImage, rect: egui::Rect, color: image::Rgba<u8>, width: f32) {
    let steps = (width.max(1.0)) as i32;
    for i in 0..steps {
        let inset = i as f32;
        let r = rect.shrink(inset);
        if r.width() <= 0.0 || r.height() <= 0.0 {
            continue;
        }
        let irect =
            imageproc::rect::Rect::at(r.left() as i32, r.top() as i32).of_size(r.width() as u32, r.height() as u32);
        imageproc::drawing::draw_hollow_rect_mut(image, irect, color);
    }
}
