//! Renders LaTeX math (`$…$` inline, `$$…$$` display) into resolution-independent
//! GPU vector geometry instead of a rasterized bitmap.
//!
//! RaTeX typesets the formula and emits self-contained `<path>` SVG (glyph
//! outlines, no font dependency at render time). We transcribe those path
//! segments into gpui `PathBuilder` fills, so the result is true vector geometry
//! that stays crisp at any zoom / pixel density rather than an upscaled image.
//! KaTeX's TTF glyph outlines are quadratic, which maps 1:1 onto gpui's
//! `PathBuilder::curve_to(to, ctrl)`.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use gpui::{
    AnyElement, Hsla, PathBuilder, Pixels, Point, canvas, div, point, prelude::*, px,
};

/// One drawing op of a glyph/rule outline, in equation-local pixels (origin at
/// the equation's top-left, y pointing down — matching SVG and gpui).
#[derive(Clone, Copy)]
enum Seg {
    Move(Point<Pixels>),
    Line(Point<Pixels>),
    Quad {
        ctrl: Point<Pixels>,
        to: Point<Pixels>,
    },
    Close,
}

/// Cached vector geometry for one typeset formula. Independent of color (which
/// is applied at paint time from the surrounding text style), so it can be
/// shared across themes and re-renders.
struct CachedMath {
    /// Each inner `Vec<Seg>` is one fillable sub-path (glyph or rule). lyon
    /// tessellates each with nonzero fill, so holes/concavity render correctly.
    contours: Vec<Vec<Seg>>,
    width: Pixels,
    height: Pixels,
}

/// `(latex, display_style, em_px * 4 rounded)` — typesetting is expensive, and
/// the markdown element tree is rebuilt on every frame, so memoize the geometry.
type CacheKey = (String, bool, u32);

static CACHE: LazyLock<Mutex<HashMap<CacheKey, Arc<CachedMath>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// SVG user units per em that we ask RaTeX for. Arbitrary — we rescale to the
/// caller's `em_px` afterwards. Larger keeps integer rounding in the SVG small.
const UNITS_PER_EM: f64 = 40.0;


/// Inline math sits in a `flex_wrap` row that gpui aligns by the box's bottom
/// edge (gpui plumbs no text baseline to layout). The RaTeX box includes the
/// font descender, which would drop the equation below the surrounding text's
/// baseline. We pad this fraction of an em *below* the glyphs so bottom-edge
/// alignment lifts the equation's baseline up onto the text baseline. Tunable.
const INLINE_BASELINE_LIFT_EM: f32 = 0.3;

/// Render display math (`$$…$$`) as a centered block element, or `None` if the
/// LaTeX fails to parse. `scale` is how much larger than body text it renders.
pub fn display_math(latex: &str, em_px: Pixels, color: Hsla, scale: f32) -> Option<AnyElement> {
    let em_px = px(f32::from(em_px) * scale);
    let cached = render_geometry(latex, true, em_px)?;
    Some(
        div()
            .my_4()
            .w_full()
            .flex()
            .flex_row()
            .justify_center()
            .child(math_canvas(cached, color, px(0.)))
            .into_any_element(),
    )
}

/// Render inline math (`$…$`) as a bare sized element the caller places into the
/// current text flow, or `None` if the LaTeX fails to parse.
pub fn inline_math(latex: &str, em_px: Pixels, color: Hsla) -> Option<AnyElement> {
    let cached = render_geometry(latex, false, em_px)?;
    let lift = px(INLINE_BASELINE_LIFT_EM * f32::from(em_px));
    Some(math_canvas(cached, color, lift).into_any_element())
}

/// `bottom_pad` adds empty space below the glyphs; with the row's bottom-edge
/// alignment this lifts the equation up (used for inline baseline correction).
fn math_canvas(cached: Arc<CachedMath>, color: Hsla, bottom_pad: Pixels) -> impl IntoElement {
    let width = cached.width;
    let height = cached.height + bottom_pad;
    canvas(
        move |_, _, _| {},
        move |bounds, _, window, _| {
            let origin = bounds.origin;
            for contour in &cached.contours {
                let mut builder = PathBuilder::fill();
                for seg in contour {
                    match *seg {
                        Seg::Move(p) => builder.move_to(p + origin),
                        Seg::Line(p) => builder.line_to(p + origin),
                        Seg::Quad { ctrl, to } => builder.curve_to(to + origin, ctrl + origin),
                        Seg::Close => builder.close(),
                    }
                }
                if let Ok(path) = builder.build() {
                    window.paint_path(path, color);
                }
            }
        },
    )
    .w(width)
    .h(height)
}

fn render_geometry(latex: &str, display: bool, em_px: Pixels) -> Option<Arc<CachedMath>> {
    let key: CacheKey = (latex.to_string(), display, (f32::from(em_px) * 4.0).round() as u32);
    if let Some(cached) = CACHE.lock().ok()?.get(&key) {
        return Some(cached.clone());
    }

    let nodes = ratex_parser::parse(latex).ok()?;
    let mut options = ratex_layout::LayoutOptions::default();
    if !display {
        options = options.with_style(ratex_types::math_style::MathStyle::Text);
    }
    let layout_box = ratex_layout::layout(&nodes, &options);
    let display_list = ratex_layout::to_display_list(&layout_box);

    let svg = ratex_svg::render_to_svg(
        &display_list,
        &ratex_svg::SvgOptions {
            font_size: UNITS_PER_EM,
            padding: 0.0,
            embed_glyphs: true,
            ..Default::default()
        },
    );

    let tree = usvg::Tree::from_str(&svg, &usvg::Options::default()).ok()?;
    let size = tree.size();
    let (tree_w, tree_h) = (size.width(), size.height());
    if tree_w <= 0.0 || tree_h <= 0.0 {
        return None;
    }

    // usvg resolves SVG "pt" lengths at 96/72 px-per-pt, and with `padding: 0`
    // the tree is exactly the math box, so one em is `UNITS_PER_EM * 96/72`
    // tree-pixels. Rescale so one em equals `em_px` on screen. Because the
    // output is tessellated vector geometry, this scale is lossless.
    let px_per_em = (UNITS_PER_EM * 96.0 / 72.0) as f32;
    let scale = f32::from(em_px) / px_per_em;

    let mut contours = Vec::new();
    collect_contours(tree.root(), scale, &mut contours);
    if contours.is_empty() {
        return None;
    }

    let cached = Arc::new(CachedMath {
        contours,
        width: px(tree_w * scale),
        height: px(tree_h * scale),
    });
    CACHE.lock().ok()?.insert(key, cached.clone());
    Some(cached)
}

fn collect_contours(group: &usvg::Group, scale: f32, out: &mut Vec<Vec<Seg>>) {
    use usvg::tiny_skia_path::PathSegment;

    for node in group.children() {
        match node {
            usvg::Node::Path(path) => {
                let t = path.abs_transform();
                let map = |x: f32, y: f32| -> Point<Pixels> {
                    let nx = t.sx * x + t.kx * y + t.tx;
                    let ny = t.ky * x + t.sy * y + t.ty;
                    point(px(nx * scale), px(ny * scale))
                };
                let mut segs = Vec::new();
                for seg in path.data().segments() {
                    match seg {
                        PathSegment::MoveTo(p) => segs.push(Seg::Move(map(p.x, p.y))),
                        PathSegment::LineTo(p) => segs.push(Seg::Line(map(p.x, p.y))),
                        PathSegment::QuadTo(c, p) => segs.push(Seg::Quad {
                            ctrl: map(c.x, c.y),
                            to: map(p.x, p.y),
                        }),
                        // KaTeX outlines are quadratic; cubics shouldn't appear,
                        // but degrade to a line to the endpoint rather than panic.
                        PathSegment::CubicTo(_, _, p) => segs.push(Seg::Line(map(p.x, p.y))),
                        PathSegment::Close => segs.push(Seg::Close),
                    }
                }
                if !segs.is_empty() {
                    out.push(segs);
                }
            }
            usvg::Node::Group(inner) => collect_contours(inner, scale, out),
            _ => {}
        }
    }
}
