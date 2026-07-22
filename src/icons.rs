//! Toolbar icons: Lucide (https://lucide.dev/icons) SVGs, rasterized once at
//! startup into egui textures. Source SVGs live in `assets/icons/` — fetched
//! from Lucide's repo and saved with `stroke="currentColor"` replaced by an
//! explicit white, since `currentColor` only resolves inside a browser's CSS
//! cascade, not a standalone parsed SVG.

use std::collections::HashMap;

use eframe::egui;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Icon {
    Pencil,
    Eraser,
    Highlighter,
    Square,
    Circle,
    MoveUpRight,
    Type,
    ListOrdered,
    Blend,
    Contrast,
    WandSparkles,
    Copy,
    Save,
    Pin,
    Undo2,
    Redo2,
    Zap,
    MousePointer2,
    Video,
    CircleStop,
    CirclePlay,
    CirclePause,
    X,
}

impl Icon {
    const ALL: [Icon; 23] = [
        Icon::Pencil,
        Icon::Eraser,
        Icon::Highlighter,
        Icon::Square,
        Icon::Circle,
        Icon::MoveUpRight,
        Icon::Type,
        Icon::ListOrdered,
        Icon::Blend,
        Icon::Contrast,
        Icon::WandSparkles,
        Icon::Copy,
        Icon::Save,
        Icon::Pin,
        Icon::Undo2,
        Icon::Redo2,
        Icon::Zap,
        Icon::MousePointer2,
        Icon::Video,
        Icon::CircleStop,
        Icon::CirclePlay,
        Icon::CirclePause,
        Icon::X,
    ];

    fn svg_source(self) -> &'static str {
        match self {
            Icon::Pencil => include_str!("../assets/icons/pencil.svg"),
            Icon::Eraser => include_str!("../assets/icons/eraser.svg"),
            Icon::Highlighter => include_str!("../assets/icons/highlighter.svg"),
            Icon::Square => include_str!("../assets/icons/square.svg"),
            Icon::Circle => include_str!("../assets/icons/circle.svg"),
            Icon::MoveUpRight => include_str!("../assets/icons/move-up-right.svg"),
            Icon::Type => include_str!("../assets/icons/type.svg"),
            Icon::ListOrdered => include_str!("../assets/icons/list-ordered.svg"),
            Icon::Blend => include_str!("../assets/icons/blend.svg"),
            Icon::Contrast => include_str!("../assets/icons/contrast.svg"),
            Icon::WandSparkles => include_str!("../assets/icons/wand-sparkles.svg"),
            Icon::Copy => include_str!("../assets/icons/copy.svg"),
            Icon::Save => include_str!("../assets/icons/save.svg"),
            Icon::Pin => include_str!("../assets/icons/pin.svg"),
            Icon::Undo2 => include_str!("../assets/icons/undo-2.svg"),
            Icon::Redo2 => include_str!("../assets/icons/redo-2.svg"),
            Icon::Zap => include_str!("../assets/icons/zap.svg"),
            Icon::MousePointer2 => include_str!("../assets/icons/mouse-pointer-2.svg"),
            Icon::Video => include_str!("../assets/icons/video.svg"),
            Icon::CircleStop => include_str!("../assets/icons/circle-stop.svg"),
            Icon::CirclePlay => include_str!("../assets/icons/circle-play.svg"),
            Icon::CirclePause => include_str!("../assets/icons/circle-pause.svg"),
            Icon::X => include_str!("../assets/icons/x.svg"),
        }
    }
}

/// Rasterizes every icon once at the given pixel size and uploads each as its
/// own egui texture. Called once, at app construction (see
/// `OverlayApp::new`), since the icon set is fixed for the process lifetime.
pub fn load_all(ctx: &egui::Context, size: u32) -> HashMap<Icon, egui::TextureHandle> {
    Icon::ALL
        .into_iter()
        .filter_map(|icon| rasterize(icon.svg_source(), size).map(|img| (icon, img)))
        .map(|(icon, color_image)| {
            let tex = ctx.load_texture(format!("icon-{icon:?}"), color_image, egui::TextureOptions::LINEAR);
            (icon, tex)
        })
        .collect()
}

fn rasterize(svg: &str, size: u32) -> Option<egui::ColorImage> {
    let tree = usvg::Tree::from_str(svg, &usvg::Options::default()).ok()?;
    let tree_size = tree.size();
    let scale = size as f32 / tree_size.width().max(tree_size.height());

    let mut pixmap = tiny_skia::Pixmap::new(size, size)?;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );

    Some(egui::ColorImage::from_rgba_premultiplied(
        [size as usize, size as usize],
        pixmap.data(),
    ))
}
