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
    let configured_font_size = config.font_size_pt;
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

        let presets: [(&str, &str, bool, bool, bool, bool, bool, u8, bool); 10] = [
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
                "media_settings_app",
                "Media settings app category",
                false,
                false,
                false,
                true,
                false,
                3,
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
        let mut settings_geometry: Vec<(u8, usize, egui::Rect)> = Vec::new();
        let mut settings_final_rects: Vec<(u8, egui::Rect)> = Vec::new();
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
            let settle_passes = if show_settings { 30 } else { 3 };
            for pass in 0..settle_passes {
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
                if show_settings {
                    let modal_rect = ctx
                        .memory(|memory| memory.area_rect(egui::Id::new("media_settings_window")))
                        .ok_or_else(|| {
                            format!("{base}: Settings area missing after pass {pass}")
                        })?;
                    // egui's anchored Area reports its pre-anchor seed rect on
                    // the first settling frames. Enforce the live geometry
                    // once its documented prior-frame memory has converged.
                    if pass >= 2 && !contains_with_tolerance(screen, modal_rect) {
                        return Err(format!(
                            "{base}: Settings escaped viewport on pass {pass}: {modal_rect:?}"
                        ));
                    }
                    settings_geometry.push((settings_category, pass, modal_rect));
                    if pass + 1 == settle_passes {
                        settings_final_rects.push((settings_category, modal_rect));
                    }
                }
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
            if show_settings {
                for required in ["Media settings", "Close"] {
                    if !texts
                        .iter()
                        .any(|text| text.text == required && !text.clipped)
                    {
                        return Err(format!(
                            "{base}: required visible Settings text missing: {required}"
                        ));
                    }
                }
            }
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

        let baseline = settings_final_rects
            .first()
            .map(|(_, rect)| *rect)
            .ok_or_else(|| "Settings stability presets produced no geometry".to_string())?;
        for (category, rect) in &settings_final_rects {
            let delta = (rect.min - baseline.min)
                .abs()
                .max((rect.max - baseline.max).abs());
            if delta.x > 1.0 || delta.y > 1.0 {
                return Err(format!(
                    "Settings category {category} changed outer bounds: baseline={baseline:?} observed={rect:?}"
                ));
            }
        }
        let geometry_json: Vec<serde_json::Value> = settings_geometry
            .iter()
            .map(|(category, pass, rect)| {
                serde_json::json!({
                    "category": category,
                    "pass": pass,
                    "rect": { "x": rect.min.x, "y": rect.min.y, "w": rect.width(), "h": rect.height() }
                })
            })
            .collect();
        std::fs::write(
            root.join("media_settings_stability.json"),
            serde_json::to_string_pretty(&serde_json::json!({
                "settle_passes_per_category": 30,
                "category_switch_sequence": ["Media", "Playback", "Controls", "App"],
                "stable_tolerance_points": 1.0,
                "passes": geometry_json,
            }))
            .unwrap_or_default(),
        )
        .map_err(|error| format!("write media_settings_stability.json: {error}"))?;

        // Constrained viewport + high-font proof (WP-055). The Controls category
        // is the tallest surface and therefore the strongest footer/title test.
        let constrained_screen =
            egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(760.0, 520.0));
        app.debug_media_load_fixture(&folder, fixture_files.clone());
        app.debug_media_set_font_size(&ctx, 32.0);
        app.debug_media_show_settings(true);
        app.debug_media_set_settings_category(2);
        let mut constrained_shapes = Vec::new();
        let mut constrained_modal_rect = egui::Rect::NOTHING;
        for pass in 0..30 {
            let input = egui::RawInput {
                screen_rect: Some(constrained_screen),
                ..Default::default()
            };
            let full = ctx.run(input, |ctx| app.render_ui(ctx));
            constrained_shapes = full.shapes;
            let modal_rect = ctx
                .memory(|memory| memory.area_rect(egui::Id::new("media_settings_window")))
                .ok_or_else(|| format!("constrained Settings area missing after pass {pass}"))?;
            constrained_modal_rect = modal_rect;
            if pass >= 2 && !contains_with_tolerance(constrained_screen, modal_rect) {
                return Err(format!(
                    "constrained Settings escaped viewport on pass {pass}: {modal_rect:?}"
                ));
            }
        }
        let (mut constrained_rects, mut constrained_texts, mut constrained_svg_body) =
            (Vec::new(), Vec::new(), String::new());
        for (index, clipped) in constrained_shapes.iter().enumerate() {
            emit_shape_clipped(
                &clipped.shape,
                clipped.clip_rect,
                index,
                &mut constrained_svg_body,
                &mut constrained_rects,
                &mut constrained_texts,
            );
        }
        // Regression proof for the inspector itself. This sentence contains no
        // newline, but egui word-wraps it into multiple Galley rows at 32 pt.
        // The old newline-only SVG emitter painted it as one overflowing row.
        const WRAPPED_CONTROLS_HELP: &str = "Click a keyboard or controller cell to remap it. Controller video defaults use the right stick.";
        let wrapped_help = constrained_texts
            .iter()
            .find(|text| text.text == WRAPPED_CONTROLS_HELP)
            .ok_or_else(|| "constrained Settings wrapped Controls help missing".to_string())?;
        if WRAPPED_CONTROLS_HELP.contains('\n') || wrapped_help.rows.len() < 2 {
            return Err(format!(
                "constrained Settings Controls help did not automatically wrap: {} rows",
                wrapped_help.rows.len()
            ));
        }
        for (row_index, row) in wrapped_help.rows.iter().enumerate() {
            let row_rect =
                egui::Rect::from_min_size(egui::pos2(row.x, row.y), egui::vec2(row.w, row.h));
            if row.clipped || !contains_with_tolerance(constrained_modal_rect, row_rect) {
                return Err(format!(
                    "constrained Settings wrapped Controls row {row_index} escaped content/modal bounds: row={row_rect:?} modal={constrained_modal_rect:?} clipped={}",
                    row.clipped
                ));
            }
        }
        let wrapped_svg_marker = format!(
            "<g class=\"egui-galley\" aria-label=\"{}\" data-egui-row-count=\"{}\"",
            xml_escape(WRAPPED_CONTROLS_HELP),
            wrapped_help.rows.len()
        );
        if !constrained_svg_body.contains(&wrapped_svg_marker) {
            return Err(
                "constrained Settings wrapped Controls rows were not emitted to SVG".to_string(),
            );
        }
        for required in ["Media settings", "Close"] {
            if !constrained_texts
                .iter()
                .any(|text| text.text == required && !text.clipped)
            {
                return Err(format!(
                    "constrained high-font Settings text missing: {required}"
                ));
            }
        }
        let constrained_svg = wrap_svg(
            &constrained_svg_body,
            constrained_screen.width(),
            constrained_screen.height(),
        );
        let constrained_layout = build_layout_json_at_size(
            Tab::Media,
            &constrained_rects,
            &constrained_texts,
            constrained_screen.size(),
        );
        write_visual_artifacts(
            &root,
            "media_settings_constrained_high_font",
            &constrained_svg,
        )?;
        std::fs::write(
            root.join("media_settings_constrained_high_font.layout.json"),
            serde_json::to_string_pretty(&constrained_layout).unwrap_or_default(),
        )
        .map_err(|error| {
            format!("write media_settings_constrained_high_font.layout.json: {error}")
        })?;
        index_rows.push((
            "media_settings_constrained_high_font".to_string(),
            "Settings constrained viewport + 32 pt".to_string(),
            constrained_rects.len(),
            constrained_texts.len(),
        ));

        // Backdrop interaction proof: click over the underlying Manual tab.
        // Settings must close, while navigation remains Media (no click-through).
        app.debug_media_set_font_size(&ctx, configured_font_size);
        app.debug_media_load_fixture(&folder, fixture_files.clone());
        app.debug_media_show_settings(true);
        for _ in 0..4 {
            let input = egui::RawInput {
                screen_rect: Some(screen),
                ..Default::default()
            };
            let _ = ctx.run(input, |ctx| app.render_ui(ctx));
        }
        let click_pos = egui::pos2(798.0, 24.0);
        for pressed in [true, false] {
            let mut input = egui::RawInput {
                screen_rect: Some(screen),
                ..Default::default()
            };
            input.events.push(egui::Event::PointerMoved(click_pos));
            input.events.push(egui::Event::PointerButton {
                pos: click_pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            });
            let _ = ctx.run(input, |ctx| app.render_ui(ctx));
        }
        if app.debug_media_settings_visible() || app.debug_active_tab() != Tab::Media {
            return Err("Settings backdrop did not close cleanly or leaked its click".to_string());
        }

        // Escape uses the same forced live-save close path.
        app.debug_media_show_settings(true);
        let mut escape_input = egui::RawInput {
            screen_rect: Some(screen),
            ..Default::default()
        };
        escape_input.events.push(egui::Event::Key {
            key: egui::Key::Escape,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        });
        let _ = ctx.run(escape_input, |ctx| app.render_ui(ctx));
        if app.debug_media_settings_visible() {
            return Err("Escape did not close Settings".to_string());
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
            // `pos` is the galley anchor; right/center-aligned galleys (e.g.
            // labels inside right_to_left layouts) extend left of the anchor.
            // galley.rect is glyph bounds relative to the anchor, so offsetting
            // by its min yields true top-left geometry for every alignment.
            let origin_x = t.pos.x + t.galley.rect.min.x;
            let origin_y = t.pos.y + t.galley.rect.min.y;
            // A Galley row is the authoritative layout unit. A paragraph with
            // no literal newline may still have many rows after word wrapping,
            // so reconstructing lines from `galley.text()` loses the layout and
            // makes the PNG disagree with egui. Emit each glyph at egui's own
            // baseline coordinate; this also preserves mixed-format positions.
            emit_text_galley(t, svg);
            let text_rect = egui::Rect::from_min_size(
                egui::pos2(origin_x, origin_y),
                egui::vec2(size.x, size.y),
            );
            let rows = t
                .galley
                .rows
                .iter()
                .map(|row| {
                    let rect = row.rect.translate(t.pos.to_vec2());
                    TextRowInfo {
                        text: row.text(),
                        x: rect.min.x,
                        y: rect.min.y,
                        w: rect.width(),
                        h: rect.height(),
                        clipped: !contains_with_tolerance(clip, rect),
                    }
                })
                .collect();
            texts.push(TextInfo {
                text,
                x: origin_x,
                y: origin_y,
                w: size.x,
                h: size.y,
                clipped: !contains_with_tolerance(clip, text_rect),
                rows,
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

/// Emit the exact rows and glyph baselines produced by egui's text layout.
///
/// SVG's own line wrapping is deliberately not used: browser/resvg font
/// metrics are not guaranteed to make the same line-break decisions as egui.
/// One positioned SVG `<text>` node per glyph is verbose, but inspection output
/// is bounded and the explicit coordinates keep both SVG and PNG deterministic.
fn emit_text_galley(t: &egui::epaint::TextShape, svg: &mut String) {
    let rotation = if t.angle == 0.0 {
        String::new()
    } else {
        format!(
            " transform=\"rotate({:.3} {:.1} {:.1})\"",
            t.angle.to_degrees(),
            t.pos.x,
            t.pos.y
        )
    };
    svg.push_str(&format!(
        "<g class=\"egui-galley\" aria-label=\"{}\" data-egui-row-count=\"{}\"{}>\n",
        xml_escape(t.galley.text()),
        t.galley.rows.len(),
        rotation
    ));
    for (row_index, row) in t.galley.rows.iter().enumerate() {
        let row_rect = row.rect.translate(t.pos.to_vec2());
        svg.push_str(&format!(
            "<g class=\"egui-row\" data-egui-row=\"{row_index}\" data-egui-x=\"{:.1}\" data-egui-y=\"{:.1}\" data-egui-width=\"{:.1}\" data-egui-height=\"{:.1}\">\n",
            row_rect.min.x,
            row_rect.min.y,
            row_rect.width(),
            row_rect.height()
        ));
        for glyph in &row.glyphs {
            let Some(section) = t.galley.job.sections.get(glyph.section_index as usize) else {
                continue;
            };
            let format = &section.format;
            let base_color = t.override_text_color.unwrap_or_else(|| {
                if format.color == egui::Color32::PLACEHOLDER {
                    t.fallback_color
                } else {
                    format.color
                }
            });
            let color = color_css(base_color.gamma_multiply(t.opacity_factor));
            let font_family = match &format.font_id.family {
                egui::FontFamily::Monospace => "monospace".to_string(),
                egui::FontFamily::Proportional => "sans-serif".to_string(),
                egui::FontFamily::Name(name) => format!("'{}', sans-serif", xml_escape(name)),
            };
            let font_style = if format.italics { "italic" } else { "normal" };
            svg.push_str(&format!(
                "<text x=\"{:.1}\" y=\"{:.1}\" font-family=\"{}\" font-size=\"{:.1}\" font-style=\"{}\" fill=\"{}\" xml:space=\"preserve\">{}</text>\n",
                t.pos.x + glyph.pos.x,
                t.pos.y + glyph.pos.y,
                font_family,
                format.font_id.size,
                font_style,
                color,
                xml_escape(&glyph.chr.to_string())
            ));
        }
        svg.push_str("</g>\n");
    }
    svg.push_str("</g>\n");
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
    rows: Vec<TextRowInfo>,
}

struct TextRowInfo {
    text: String,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    clipped: bool,
}

fn build_layout_json(tab: Tab, rects: &[RectInfo], texts: &[TextInfo]) -> serde_json::Value {
    build_layout_json_at_size(tab, rects, texts, egui::vec2(SCREEN_W, SCREEN_H))
}

fn build_layout_json_at_size(
    tab: Tab,
    rects: &[RectInfo],
    texts: &[TextInfo],
    screen: egui::Vec2,
) -> serde_json::Value {
    let texts_json: Vec<serde_json::Value> = texts
        .iter()
        .map(|t| {
            let rows: Vec<serde_json::Value> = t
                .rows
                .iter()
                .map(|row| {
                    serde_json::json!({
                        "text": row.text,
                        "x": row.x,
                        "y": row.y,
                        "w": row.w,
                        "h": row.h,
                        "clipped": row.clipped,
                    })
                })
                .collect();
            serde_json::json!({
                "text": t.text,
                "x": t.x,
                "y": t.y,
                "w": t.w,
                "h": t.h,
                "clipped": t.clipped,
                "row_count": t.rows.len(),
                "automatically_wrapped": t.rows.len() > t.text.matches('\n').count() + 1,
                "rows": rows,
            })
        })
        .collect();
    let rects_json: Vec<serde_json::Value> = rects
        .iter()
        .map(|r| serde_json::json!({ "x": r.x, "y": r.y, "w": r.w, "h": r.h, "fill": r.fill, "clipped": r.clipped }))
        .collect();
    serde_json::json!({
        "tab": tab.vocab(),
        "label": tab.label(),
        "screen": { "w": screen.x, "h": screen.y },
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
