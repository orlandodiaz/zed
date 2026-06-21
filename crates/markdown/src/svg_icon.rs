//! Renders icon SVGs as resolution-independent GPU vector geometry, like
//! [`crate::math`], but preserving each path's fill color so two-tone logos
//! (e.g. a black disc + white glyph) stay crisp at any pixel density instead of
//! aliasing like a downscaled bitmap. Used for page icons next to wikilinks and
//! in the preview title.
//!
//! `usvg` parses the file; each `<path>` becomes a `PathBuilder` fill painted
//! with its own color and fill rule. Geometry is cached per file path (in the
//! SVG's own coordinate space) and rescaled to the requested size at paint time.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, LazyLock, Mutex};

use gpui::{
    AnyElement, FillOptions, FillRule, Hsla, PathBuilder, PathStyle, Pixels, Rgba, canvas, point,
    prelude::*, px,
};

/// One drawing op of a path outline, in the SVG's coordinate space (the tree
/// size after the viewBox transform). Scaled to the target size at paint time.
#[derive(Clone, Copy)]
enum Seg {
    Move(f32, f32),
    Line(f32, f32),
    Quad { cx: f32, cy: f32, x: f32, y: f32 },
    Cubic { c1x: f32, c1y: f32, c2x: f32, c2y: f32, x: f32, y: f32 },
    Close,
}

/// One filled sub-path with its own color and winding rule.
struct Contour {
    color: Hsla,
    even_odd: bool,
    segs: Vec<Seg>,
}

struct CachedIcon {
    /// Painted in document order so later paths overlay earlier ones.
    contours: Vec<Contour>,
    view_w: f32,
    view_h: f32,
}

/// Parsing an SVG is comparatively expensive and the markdown element tree is
/// rebuilt every frame, so cache the geometry per file path. (Icons rarely
/// change; a file edit isn't picked up until restart.)
static CACHE: LazyLock<Mutex<HashMap<String, Option<Arc<CachedIcon>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Renders the SVG at `path` as a vector element sized to fit `size` (preserving
/// aspect ratio), or `None` if it can't be read/parsed.
pub fn render_svg_icon(path: &Path, size: Pixels) -> Option<AnyElement> {
    let cached = cached_icon(path)?;
    let max_dim = cached.view_w.max(cached.view_h);
    if max_dim <= 0.0 {
        return None;
    }
    let scale = f32::from(size) / max_dim;
    let width = px(cached.view_w * scale);
    let height = px(cached.view_h * scale);

    Some(
        canvas(
            move |_, _, _| {},
            move |bounds, _, window, _| {
                let origin = bounds.origin;
                let map = |x: f32, y: f32| point(px(x * scale), px(y * scale)) + origin;
                for contour in &cached.contours {
                    let mut builder = if contour.even_odd {
                        PathBuilder::default().with_style(PathStyle::Fill(
                            FillOptions::default().with_fill_rule(FillRule::EvenOdd),
                        ))
                    } else {
                        PathBuilder::fill()
                    };
                    for seg in &contour.segs {
                        match *seg {
                            Seg::Move(x, y) => builder.move_to(map(x, y)),
                            Seg::Line(x, y) => builder.line_to(map(x, y)),
                            Seg::Quad { cx, cy, x, y } => builder.curve_to(map(x, y), map(cx, cy)),
                            Seg::Cubic {
                                c1x,
                                c1y,
                                c2x,
                                c2y,
                                x,
                                y,
                            } => builder.cubic_bezier_to(map(x, y), map(c1x, c1y), map(c2x, c2y)),
                            Seg::Close => builder.close(),
                        }
                    }
                    if let Ok(path) = builder.build() {
                        window.paint_path(path, contour.color);
                    }
                }
            },
        )
        .w(width)
        .h(height)
        .flex_none()
        .into_any_element(),
    )
}

fn cached_icon(path: &Path) -> Option<Arc<CachedIcon>> {
    let key = path.to_string_lossy().into_owned();
    let mut cache = CACHE.lock().ok()?;
    if let Some(entry) = cache.get(&key) {
        return entry.clone();
    }
    let parsed = std::fs::read(path)
        .ok()
        .and_then(|bytes| parse_icon(&bytes))
        .map(Arc::new);
    cache.insert(key, parsed.clone());
    parsed
}

fn parse_icon(bytes: &[u8]) -> Option<CachedIcon> {
    let tree = usvg::Tree::from_data(bytes, &usvg::Options::default()).ok()?;
    let size = tree.size();
    let (view_w, view_h) = (size.width(), size.height());
    if view_w <= 0.0 || view_h <= 0.0 {
        return None;
    }
    let mut contours = Vec::new();
    collect_contours(tree.root(), &mut contours);
    if contours.is_empty() {
        return None;
    }
    Some(CachedIcon {
        contours,
        view_w,
        view_h,
    })
}

fn collect_contours(group: &usvg::Group, out: &mut Vec<Contour>) {
    use usvg::tiny_skia_path::PathSegment;

    for node in group.children() {
        match node {
            usvg::Node::Path(path) => {
                let Some(fill) = path.fill() else {
                    continue;
                };
                let usvg::Paint::Color(c) = fill.paint() else {
                    // Gradients/patterns aren't supported; skip rather than mis-fill.
                    continue;
                };
                let color: Hsla = Rgba {
                    r: c.red as f32 / 255.0,
                    g: c.green as f32 / 255.0,
                    b: c.blue as f32 / 255.0,
                    a: fill.opacity().get(),
                }
                .into();
                let even_odd = fill.rule() == usvg::FillRule::EvenOdd;

                let t = path.abs_transform();
                let map = |x: f32, y: f32| -> (f32, f32) {
                    (t.sx * x + t.kx * y + t.tx, t.ky * x + t.sy * y + t.ty)
                };
                let mut segs = Vec::new();
                for seg in path.data().segments() {
                    match seg {
                        PathSegment::MoveTo(p) => {
                            let (x, y) = map(p.x, p.y);
                            segs.push(Seg::Move(x, y));
                        }
                        PathSegment::LineTo(p) => {
                            let (x, y) = map(p.x, p.y);
                            segs.push(Seg::Line(x, y));
                        }
                        PathSegment::QuadTo(c0, p) => {
                            let (cx, cy) = map(c0.x, c0.y);
                            let (x, y) = map(p.x, p.y);
                            segs.push(Seg::Quad { cx, cy, x, y });
                        }
                        PathSegment::CubicTo(c0, c1, p) => {
                            let (c1x, c1y) = map(c0.x, c0.y);
                            let (c2x, c2y) = map(c1.x, c1.y);
                            let (x, y) = map(p.x, p.y);
                            segs.push(Seg::Cubic {
                                c1x,
                                c1y,
                                c2x,
                                c2y,
                                x,
                                y,
                            });
                        }
                        PathSegment::Close => segs.push(Seg::Close),
                    }
                }
                if !segs.is_empty() {
                    out.push(Contour {
                        color,
                        even_odd,
                        segs,
                    });
                }
            }
            usvg::Node::Group(inner) => collect_contours(inner, out),
            _ => {}
        }
    }
}
