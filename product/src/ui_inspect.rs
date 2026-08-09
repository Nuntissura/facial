//! Built-in, self-contained GUI inspector (WP-008).
//!
//! Renders each tab of the live `FacialApp` UI **headlessly** — egui computes
//! every widget rectangle on the CPU via `Context::run`, with no window and no
//! renderer (so nothing pops in front of the operator, [GLOBAL-BUILD-046]).
//! For each tab it walks egui's own shape list and emits:
//!   * `<tab>.svg`         — a faithful vector wireframe
//!   * `<tab>.png`         — the same snapshot rasterized in-process for direct review
//!   * `<tab>.layout.json` — structured rects + text (a model reads this to find
//!                           overlaps / off-canvas widgets / cramped spacing)
//! plus an `index.html` + `index.json`. Rasterization uses pure-Rust `resvg`;
//! the inspector never launches a browser or desktop automation.

use std::path::{Path, PathBuf};

use chrono::Utc;
use egui::Shape;

use crate::config::AppConfig;
use crate::service::FacialService;
use crate::ui::{FacialApp, Tab};

const SCREEN_W: f32 = 1280.0;
const SCREEN_H: f32 = 800.0;

/// Capture the requested tabs (default: all) into a timestamped snapshot dir.
/// Returns the snapshot directory path.
pub fn run(config: AppConfig, out_dir: Option<PathBuf>, tabs: &[Tab]) -> Result<PathBuf, String> {
    let workspace = config.workspace_root.clone();
    let stamp = Utc::now().format("%Y%m%d_%H%M%S").to_string();
    let root =
        out_dir.unwrap_or_else(|| workspace.join(".facial").join("ui-snapshots").join(&stamp));
    std::fs::create_dir_all(&root).map_err(|e| format!("create snapshot dir: {e}"))?;
    let service = FacialService::new(config);
    let ctx = egui::Context::default();
    ctx.set_pixels_per_point(1.0);
    let mut app =
        FacialApp::new_with_ctx_for_inspector(&ctx, service, &root.join("_inspector-workspace"));

    let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(SCREEN_W, SCREEN_H));
    let mut index_rows: Vec<(String, String, usize, usize)> = Vec::new();

    for &tab in tabs {
        app.set_active_tab(tab);
        // Three passes: egui settles layout that depends on the prior frame's
        // sizes (row heights feed ScrollArea content memory, which feeds the
        // next frame's inner sizes — two passes were not enough to converge).
        let mut shapes = Vec::new();
        for _ in 0..3 {
            let input = egui::RawInput {
                screen_rect: Some(screen),
                ..Default::default()
            };
            let full = ctx.run(input.clone(), |ctx| app.render_ui(ctx));
            shapes = full.shapes;
        }

        let mut rects = Vec::new();
        let mut texts = Vec::new();
        let mut svg_body = String::new();
        for (index, clipped) in shapes.iter().enumerate() {
            emit_shape_clipped(
                &clipped.shape,
                clipped.clip_rect,
                index,
                &mut svg_body,
                &mut rects,
                &mut texts,
            );
        }

        let svg = wrap_svg(&svg_body, SCREEN_W, SCREEN_H);
        let layout = build_layout_json(tab, &rects, &texts);

        let base = tab.vocab();
        write_visual_artifacts(&root, base, &svg)?;
        std::fs::write(
            root.join(format!("{base}.layout.json")),
            serde_json::to_string_pretty(&layout).unwrap_or_default(),
        )
        .map_err(|e| format!("write {base}.layout.json: {e}"))?;
        index_rows.push((
            base.to_string(),
            tab.label().to_string(),
            rects.len(),
            texts.len(),
        ));
    }

    // Floating dialogs only render while open, so tab snapshots alone miss
    // them. Force the Compare folder browser open and capture it with extra
    // passes: any auto-size feedback loop (content sized from available_*)
    // shows up as a window that is wider every pass, so 10 passes make it
    // unmissable in the captured geometry.
    if tabs.contains(&Tab::Compare) {
        app.set_active_tab(Tab::Compare);
        app.debug_open_folder_picker(0);
        let mut shapes = Vec::new();
        for _ in 0..10 {
            let input = egui::RawInput {
                screen_rect: Some(screen),
                ..Default::default()
            };
            let full = ctx.run(input.clone(), |ctx| app.render_ui(ctx));
            shapes = full.shapes;
        }
        let mut rects = Vec::new();
        let mut texts = Vec::new();
        let mut svg_body = String::new();
        for (index, clipped) in shapes.iter().enumerate() {
            emit_shape_clipped(
                &clipped.shape,
                clipped.clip_rect,
                index,
                &mut svg_body,
                &mut rects,
                &mut texts,
            );
        }
        let svg = wrap_svg(&svg_body, SCREEN_W, SCREEN_H);
        let layout = build_layout_json(Tab::Compare, &rects, &texts);
        write_visual_artifacts(&root, "compare_dialog", &svg)?;
        std::fs::write(
            root.join("compare_dialog.layout.json"),
            serde_json::to_string_pretty(&layout).unwrap_or_default(),
        )
        .map_err(|e| format!("write compare_dialog.layout.json: {e}"))?;
        index_rows.push((
            "compare_dialog".to_string(),
            "Compare + folder dialog".to_string(),
            rects.len(),
            texts.len(),
        ));
    }

    // Media explorer forced states (WP-044): a deterministic fixture folder
    // (12 tiny PNGs + 2 video names) drives the thumbnail grid so the book
    // layout, full-grid wall, and chrome-hidden mode can all be reviewed.
    // The debug hook disables the async thumb engine, so every tile paints
    // its placeholder and snapshots stay byte-identical.
    if tabs.contains(&Tab::Media) {
        // The compare_dialog capture leaves the folder browser open; close it
        // or it floats over every media preset.
        app.debug_close_folder_picker();
        let fixture_dir = root.join("_fixture");
        std::fs::create_dir_all(&fixture_dir).map_err(|e| format!("create fixture dir: {e}"))?;
        std::fs::create_dir_all(fixture_dir.join("subfolder-a")).ok();
        std::fs::create_dir_all(fixture_dir.join("subfolder-b")).ok();
        let couch_dir = fixture_dir.join("subfolder-a");
        for name in ["portraits", "training-sets", "video-clips"] {
            std::fs::create_dir_all(couch_dir.join(name)).ok();
        }
        let deep_dir = couch_dir.join("training-sets");
        std::fs::create_dir_all(deep_dir.join("approved-yaw-renders")).ok();
        std::fs::create_dir_all(deep_dir.join("needs-review")).ok();
        let empty_dir = fixture_dir.join("empty-collection");
        std::fs::create_dir_all(&empty_dir).ok();
        for i in 0..28 {
            std::fs::create_dir_all(
                fixture_dir.join(format!("collection-{i:02}-descriptive-folder-name")),
            )
            .ok();
        }
        let mut fixture_files: Vec<String> = Vec::new();
        for i in 0..12 {
            let path = fixture_dir.join(format!("sample_{i:02}.png"));
            if !path.exists() {
                let img = image::RgbaImage::from_pixel(4, 4, image::Rgba([200, 200, 200, 255]));
                let _ = img.save(&path);
            }
            fixture_files.push(path.to_string_lossy().to_string());
        }
        for name in ["clip_a.mp4", "clip_b.mov"] {
            let path = fixture_dir.join(name);
            if !path.exists() {
                let _ = std::fs::write(&path, b"");
            }
            fixture_files.push(path.to_string_lossy().to_string());
        }
        let folder = fixture_dir.to_string_lossy().to_string();

        let presets: [(&str, &str, bool, bool, bool, bool, bool, u8, bool); 9] = [
            (
                "media_grid",
                "Media grid (two-panel book)",
                false,
                false,
                false,
                false,
                false,
                0,
                false,
            ),
            (
                "media_full",
                "Media full-window wall",
                true,
                false,
                false,
                false,
                false,
                0,
                false,
            ),
            (
                "media_hidden",
                "Media fullscreen book",
                false,
                true,
                false,
                false,
                false,
                0,
                false,
            ),
            (
                "media_names",
                "Media grid with filenames",
                false,
                false,
                true,
                false,
                false,
                0,
                false,
            ),
            (
                "media_settings",
                "Media settings popup",
                false,
                false,
                false,
                true,
                false,
                0,
                false,
            ),
            (
                "media_settings_playback",
                "Media settings playback category",
                false,
                false,
                false,
                true,
                false,
                1,
                false,
            ),
            (
                "media_settings_controls",
                "Media settings controls category",
                false,
                false,
                false,
                true,
                false,
                2,
                false,
            ),
            (
                "media_scrollbar",
                "Media large scrollbar hover",
                false,
                false,
                false,
                false,
                true,
                0,
                false,
            ),
            (
                "media_video",
                "Media selected-video controls",
                false,
                false,
                false,
                false,
                false,
                0,
                true,
            ),
        ];
        for (
            base,
            label,
            full_grid,
            chrome_hidden,
            show_names,
            show_settings,
            hover_scroll,
            settings_category,
            show_video,
        ) in presets
        {
            let mut files = fixture_files.clone();
            if show_video {
                files.swap(3, 12);
            }
            app.debug_media_load_fixture(&folder, files);
            if show_video {
                app.debug_media_select_index(3);
            }
            app.debug_media_set_view(full_grid, chrome_hidden);
            app.debug_media_set_names(show_names);
            app.debug_media_show_settings(show_settings);
            app.debug_media_set_settings_category(settings_category);
            let mut shapes = Vec::new();
            for _ in 0..3 {
                let mut input = egui::RawInput {
                    screen_rect: Some(screen),
                    ..Default::default()
                };
                if hover_scroll {
                    input
                        .events
                        .push(egui::Event::PointerMoved(egui::pos2(736.0, 520.0)));
                }
                let full = ctx.run(input, |ctx| app.render_ui(ctx));
                shapes = full.shapes;
            }
            let mut rects = Vec::new();
            let mut texts = Vec::new();
            let mut svg_body = String::new();
            for (index, clipped) in shapes.iter().enumerate() {
                emit_shape_clipped(
                    &clipped.shape,
                    clipped.clip_rect,
                    index,
                    &mut svg_body,
                    &mut rects,
                    &mut texts,
                );
            }
            let svg = wrap_svg(&svg_body, SCREEN_W, SCREEN_H);
            let layout = build_layout_json(Tab::Media, &rects, &texts);
            write_visual_artifacts(&root, base, &svg)?;
            std::fs::write(
                root.join(format!("{base}.layout.json")),
                serde_json::to_string_pretty(&layout).unwrap_or_default(),
            )
            .map_err(|e| format!("write {base}.layout.json: {e}"))?;
            index_rows.push((
                base.to_string(),
                label.to_string(),
                rects.len(),
                texts.len(),
            ));
        }
        let couch_presets = [
            (
                "media_folders_couch",
                "Couch folder navigator",
                couch_dir.clone(),
                false,
                1usize,
            ),
            (
                "media_folders_long",
                "Couch folder navigator long list",
                fixture_dir.clone(),
                false,
                18usize,
            ),
            (
                "media_folders_deep",
                "Couch folder navigator deep path",
                deep_dir.clone(),
                false,
                1usize,
            ),
            (
                "media_folders_empty",
                "Couch folder navigator empty folder",
                empty_dir.clone(),
                false,
                0usize,
            ),
            (
                "media_folders_fullscreen",
                "Couch folder navigator fullscreen",
                couch_dir.clone(),
                true,
                2usize,
            ),
        ];
        for (base, label, preset_folder, chrome_hidden, cursor) in couch_presets {
            app.debug_media_load_fixture(
                &preset_folder.to_string_lossy(),
                if preset_folder == fixture_dir {
                    fixture_files.clone()
                } else {
                    Vec::new()
                },
            );
            app.debug_media_set_view(false, chrome_hidden);
            app.debug_media_show_folder_navigator(true, cursor);
            let mut shapes = Vec::new();
            for _ in 0..4 {
                let input = egui::RawInput {
                    screen_rect: Some(screen),
                    ..Default::default()
                };
                let full = ctx.run(input, |ctx| app.render_ui(ctx));
                shapes = full.shapes;
            }
            let mut rects = Vec::new();
            let mut texts = Vec::new();
            let mut svg_body = String::new();
            for (index, clipped) in shapes.iter().enumerate() {
                emit_shape_clipped(
                    &clipped.shape,
                    clipped.clip_rect,
                    index,
                    &mut svg_body,
                    &mut rects,
                    &mut texts,
                );
            }
            let svg = wrap_svg(&svg_body, SCREEN_W, SCREEN_H);
            let layout = build_layout_json(Tab::Media, &rects, &texts);
            write_visual_artifacts(&root, base, &svg)?;
            std::fs::write(
                root.join(format!("{base}.layout.json")),
                serde_json::to_string_pretty(&layout).unwrap_or_default(),
            )
            .map_err(|e| format!("write {base}.layout.json: {e}"))?;
            index_rows.push((
                base.to_string(),
                label.to_string(),
                rects.len(),
                texts.len(),
            ));
        }
        // Reset the forced state so later captures are unaffected.
        app.debug_media_set_view(false, false);
        app.debug_media_set_names(false);
        app.debug_media_show_settings(false);
        app.debug_media_show_folder_navigator(false, 0);
    }

    write_index(&root, &index_rows)?;
    Ok(root)
}

/// Recursively convert one egui shape into SVG and collect structured geometry.
/// `clip` is the shape's clip rect: geometry extending past it is flagged
/// `clipped` in the layout JSON (visually cropped by a ScrollArea/TextEdit),
/// so layout review can tell designed cropping from real overflow.
fn emit_shape_clipped(
    shape: &Shape,
    clip: egui::Rect,
    index: usize,
    svg: &mut String,
    rects: &mut Vec<RectInfo>,
    texts: &mut Vec<TextInfo>,
) {
    let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(SCREEN_W, SCREEN_H));
    let clip = clip.intersect(screen);
    if !clip.is_positive() {
        return;
    }
    svg.push_str(&format!(
        "<defs><clipPath id=\"clip-{index}\"><rect x=\"{:.1}\" y=\"{:.1}\" width=\"{:.1}\" height=\"{:.1}\"/></clipPath></defs><g clip-path=\"url(#clip-{index})\">\n",
        clip.min.x,
        clip.min.y,
        clip.width(),
        clip.height()
    ));
    emit_shape(shape, clip, svg, rects, texts);
    svg.push_str("</g>\n");
}

fn emit_shape(
    shape: &Shape,
    clip: egui::Rect,
    svg: &mut String,
    rects: &mut Vec<RectInfo>,
    texts: &mut Vec<TextInfo>,
) {
    match shape {
        Shape::Vec(children) => {
            for c in children {
                emit_shape(c, clip, svg, rects, texts);
            }
        }
        Shape::Rect(r) => {
            let fill = color_css(r.fill);
            let (sc, sw) = (color_css(r.stroke.color), r.stroke.width);
            // skip fully invisible rects (transparent fill + no stroke)
            if r.fill.a() == 0 && (sw <= 0.0 || r.stroke.color.a() == 0) {
                return;
            }
            let rx = r.rounding.nw.max(0.0);
            svg.push_str(&format!(
                "<rect x=\"{:.1}\" y=\"{:.1}\" width=\"{:.1}\" height=\"{:.1}\" rx=\"{:.1}\" fill=\"{}\" stroke=\"{}\" stroke-width=\"{:.1}\"/>\n",
                r.rect.min.x, r.rect.min.y, r.rect.width().max(0.0), r.rect.height().max(0.0), rx, fill, sc, sw
            ));
            rects.push(RectInfo {
                x: r.rect.min.x,
                y: r.rect.min.y,
                w: r.rect.width(),
                h: r.rect.height(),
                fill,
                clipped: !contains_with_tolerance(clip, r.rect),
            });
        }
        Shape::Text(t) => {
            let size = t.galley.size();
            let text = t.galley.text().to_string();
            let color = color_css(t.override_text_color.unwrap_or(t.fallback_color));
            // `pos` is the galley anchor; right/center-aligned galleys (e.g.
            // labels inside right_to_left layouts) extend left of the anchor.
            // galley.rect is glyph bounds relative to the anchor, so offsetting
            // by its min yields true top-left geometry for every alignment.
            let origin_x = t.pos.x + t.galley.rect.min.x;
            let origin_y = t.pos.y + t.galley.rect.min.y;
            // Place each newline-separated line; robust against version churn.
            let lines: Vec<&str> = text.split('\n').collect();
            let n = lines.len().max(1) as f32;
            let line_h = size.y / n;
            let font_px = (line_h * 0.78).max(8.0);
            for (i, line) in lines.iter().enumerate() {
                if line.trim().is_empty() {
                    continue;
                }
                let y = origin_y + (i as f32 + 0.8) * line_h;
                svg.push_str(&format!(
                    "<text x=\"{:.1}\" y=\"{:.1}\" font-family=\"monospace\" font-size=\"{:.1}\" fill=\"{}\">{}</text>\n",
                    origin_x, y, font_px, color, xml_escape(line)
                ));
            }
            let text_rect = egui::Rect::from_min_size(
                egui::pos2(origin_x, origin_y),
                egui::vec2(size.x, size.y),
            );
            texts.push(TextInfo {
                text,
                x: origin_x,
                y: origin_y,
                w: size.x,
                h: size.y,
                clipped: !contains_with_tolerance(clip, text_rect),
            });
        }
        Shape::LineSegment { points, stroke } => {
            svg.push_str(&format!(
                "<line x1=\"{:.1}\" y1=\"{:.1}\" x2=\"{:.1}\" y2=\"{:.1}\" stroke=\"{}\" stroke-width=\"{:.1}\"/>\n",
                points[0].x, points[0].y, points[1].x, points[1].y, color_css(stroke.color), stroke.width
            ));
        }
        Shape::Circle(c) => {
            svg.push_str(&format!(
                "<circle cx=\"{:.1}\" cy=\"{:.1}\" r=\"{:.1}\" fill=\"{}\" stroke=\"{}\" stroke-width=\"{:.1}\"/>\n",
                c.center.x, c.center.y, c.radius, color_css(c.fill), color_css(c.stroke.color), c.stroke.width
            ));
        }
        Shape::Path(p) => {
            if p.points.len() >= 2 {
                let pts: Vec<String> = p
                    .points
                    .iter()
                    .map(|pt| format!("{:.1},{:.1}", pt.x, pt.y))
                    .collect();
                let tag = if p.closed { "polygon" } else { "polyline" };
                svg.push_str(&format!(
                    "<{tag} points=\"{}\" fill=\"{}\" stroke=\"{}\" stroke-width=\"{:.1}\"/>\n",
                    pts.join(" "),
                    color_css(p.fill),
                    color_css(p.stroke.color),
                    p.stroke.width
                ));
            }
        }
        _ => {} // Mesh / bezier / callback / noop: not needed for layout inspection
    }
}

/// True when `outer` contains `inner` with a 1px tolerance (float jitter).
fn contains_with_tolerance(outer: egui::Rect, inner: egui::Rect) -> bool {
    inner.min.x >= outer.min.x - 1.0
        && inner.min.y >= outer.min.y - 1.0
        && inner.max.x <= outer.max.x + 1.0
        && inner.max.y <= outer.max.y + 1.0
}

struct RectInfo {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    fill: String,
    clipped: bool,
}

struct TextInfo {
    text: String,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    clipped: bool,
}

fn build_layout_json(tab: Tab, rects: &[RectInfo], texts: &[TextInfo]) -> serde_json::Value {
    let texts_json: Vec<serde_json::Value> = texts
        .iter()
        .map(|t| {
            serde_json::json!({ "text": t.text, "x": t.x, "y": t.y, "w": t.w, "h": t.h, "clipped": t.clipped })
        })
        .collect();
    let rects_json: Vec<serde_json::Value> = rects
        .iter()
        .map(|r| serde_json::json!({ "x": r.x, "y": r.y, "w": r.w, "h": r.h, "fill": r.fill, "clipped": r.clipped }))
        .collect();
    serde_json::json!({
        "tab": tab.vocab(),
        "label": tab.label(),
        "screen": { "w": SCREEN_W, "h": SCREEN_H },
        "text_count": texts.len(),
        "rect_count": rects.len(),
        "texts": texts_json,
        "rects": rects_json,
    })
}

fn wrap_svg(body: &str, w: f32, h: f32) -> String {
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{w}\" height=\"{h}\" viewBox=\"0 0 {w} {h}\">\n\
<rect x=\"0\" y=\"0\" width=\"{w}\" height=\"{h}\" fill=\"#ffffff\"/>\n{body}</svg>\n"
    )
}

fn write_visual_artifacts(root: &Path, base: &str, svg: &str) -> Result<(), String> {
    let svg_path = root.join(format!("{base}.svg"));
    std::fs::write(&svg_path, svg).map_err(|e| format!("write {base}.svg: {e}"))?;

    let mut options = resvg::usvg::Options::default();
    let fontdb = options.fontdb_mut();
    fontdb.load_font_data(
        include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/assets/fonts/Inter-Regular.ttf"
        ))
        .to_vec(),
    );
    fontdb.load_font_data(
        include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/assets/fonts/Inter-SemiBold.ttf"
        ))
        .to_vec(),
    );
    fontdb.load_font_data(
        egui_phosphor::Variant::Regular
            .font_data()
            .font
            .into_owned(),
    );
    fontdb.set_sans_serif_family("Inter");
    fontdb.set_monospace_family("Inter");
    let tree = resvg::usvg::Tree::from_str(svg, &options)
        .map_err(|e| format!("parse {base}.svg for PNG: {e}"))?;
    let size = tree.size().to_int_size();
    let mut pixmap = resvg::tiny_skia::Pixmap::new(size.width(), size.height())
        .ok_or_else(|| format!("allocate {base}.png {}x{}", size.width(), size.height()))?;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::default(),
        &mut pixmap.as_mut(),
    );
    pixmap
        .save_png(root.join(format!("{base}.png")))
        .map_err(|e| format!("write {base}.png: {e}"))
}

fn write_index(root: &Path, rows: &[(String, String, usize, usize)]) -> Result<(), String> {
    let mut html = String::from(
        "<!doctype html><meta charset=\"utf-8\"><title>facial GUI snapshot</title>\
<style>body{font-family:sans-serif;margin:1.5rem}a{display:block;margin:.3rem 0}</style>\
<h1>facial GUI snapshot</h1>\n",
    );
    let mut json_rows = Vec::new();
    for (base, label, rc, tc) in rows {
        html.push_str(&format!(
            "<a href=\"{base}.png\">{label}</a> <small>({rc} rects, {tc} texts) &mdash; <a href=\"{base}.svg\">SVG</a> &middot; <a href=\"{base}.layout.json\">layout.json</a></small>\n"
        ));
        json_rows.push(serde_json::json!({
            "tab": base, "label": label, "svg": format!("{base}.svg"),
            "png": format!("{base}.png"), "layout": format!("{base}.layout.json"),
            "rects": rc, "texts": tc
        }));
    }
    std::fs::write(root.join("index.html"), html).map_err(|e| format!("write index.html: {e}"))?;
    std::fs::write(
        root.join("index.json"),
        serde_json::to_string_pretty(&serde_json::json!({ "tabs": json_rows })).unwrap_or_default(),
    )
    .map_err(|e| format!("write index.json: {e}"))?;
    Ok(())
}

fn color_css(c: egui::Color32) -> String {
    let [r, g, b, a] = c.to_array();
    if a == 255 {
        format!("#{r:02x}{g:02x}{b:02x}")
    } else {
        format!("rgba({r},{g},{b},{:.3})", a as f32 / 255.0)
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
