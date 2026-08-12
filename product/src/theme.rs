//! Flat-paper visual identity (WP-014 layout, WP-015 identity).
//!
//! Two palettes share one structure: **Paper** (warm desk, lighter sheets,
//! denim ink) and **Ink** (dark slate desk, ink sheets, paper text). Hierarchy
//! comes from two surface tones, quiet hairlines, and ONE vermilion accent
//! reserved for the active tab, one primary action per surface, and busy
//! state — never inside data surfaces (images, logs).
//!
//! Typography: Inter (vendored, SIL OFL) for UI, Inter SemiBold for headings,
//! monospace only for code/log/event text. egui's default fonts stay in the
//! fallback chain so arrows/symbols Inter lacks keep rendering; Phosphor is
//! merged for icons.

use std::sync::atomic::{AtomicU8, Ordering};

use eframe::egui::{self, Color32, Rounding, Stroke};

// ---------------------------------------------------------------------------
// Mode
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Paper,
    Ink,
}

static MODE: AtomicU8 = AtomicU8::new(0);

pub fn set_mode(mode: Mode) {
    MODE.store(if mode == Mode::Ink { 1 } else { 0 }, Ordering::Relaxed);
}

pub fn mode() -> Mode {
    if MODE.load(Ordering::Relaxed) == 1 {
        Mode::Ink
    } else {
        Mode::Paper
    }
}

pub fn mode_from_str(s: &str) -> Mode {
    if s.trim().eq_ignore_ascii_case("ink") {
        Mode::Ink
    } else {
        Mode::Paper
    }
}

pub fn mode_to_str(mode: Mode) -> &'static str {
    match mode {
        Mode::Paper => "paper",
        Mode::Ink => "ink",
    }
}

// ---------------------------------------------------------------------------
// Palette (mode-aware getters; do NOT cache across frames)
// ---------------------------------------------------------------------------

/// Desk: the window background. True white paper / near-black slab.
pub fn desk() -> Color32 {
    match mode() {
        Mode::Paper => Color32::from_rgb(255, 255, 255), // #FFFFFF
        Mode::Ink => Color32::from_rgb(12, 12, 12),      // #0C0C0C
    }
}

/// Panel surface. No cards: same plane as the desk; structure comes from
/// black rules, not tonal steps.
pub fn sheet() -> Color32 {
    desk()
}

/// Recessed well (image viewports, code/event areas): one whisper off the
/// desk so large media areas read as a zone without becoming a card.
pub fn well() -> Color32 {
    match mode() {
        Mode::Paper => Color32::from_rgb(247, 247, 245), // #F7F7F5
        Mode::Ink => Color32::from_rgb(20, 20, 20),      // #141414
    }
}

/// Slightly recessed fill for borderless Media metadata editors. Darker than
/// the page/well so the input affordance remains obvious without a box rule.
pub fn media_field() -> Color32 {
    match mode() {
        Mode::Paper => Color32::from_rgb(234, 234, 231), // #EAEAE7
        Mode::Ink => Color32::from_rgb(34, 34, 34),      // #222222
    }
}

/// Primary ink: near-black on white / near-white on black (high contrast).
pub fn ink() -> Color32 {
    match mode() {
        Mode::Paper => Color32::from_rgb(10, 10, 10), // #0A0A0A
        Mode::Ink => Color32::from_rgb(242, 242, 242), // #F2F2F2
    }
}

/// Secondary ink (~60% strength): inactive tabs, secondary labels.
pub fn ink_soft() -> Color32 {
    match mode() {
        Mode::Paper => Color32::from_rgb(100, 100, 100), // #646464
        Mode::Ink => Color32::from_rgb(168, 168, 168),   // #A8A8A8
    }
}

/// Faint ink: hints, disabled, metadata.
pub fn ink_faint() -> Color32 {
    match mode() {
        Mode::Paper => Color32::from_rgb(154, 154, 154), // #9A9A9A
        Mode::Ink => Color32::from_rgb(118, 118, 118),   // #767676
    }
}

/// Primary rules + widget borders: thin SOLID BLACK lines (white in Ink).
pub fn rule() -> Color32 {
    match mode() {
        Mode::Paper => Color32::from_rgb(0, 0, 0),
        Mode::Ink => Color32::from_rgb(235, 235, 235),
    }
}

/// Secondary rules at ~40% strength (dividers inside quiet regions).
pub fn rule_soft() -> Color32 {
    match mode() {
        Mode::Paper => Color32::from_rgb(150, 150, 150),
        Mode::Ink => Color32::from_rgb(110, 110, 110),
    }
}

/// Selection / active tint: flat monochrome gray.
pub fn selection_bg() -> Color32 {
    match mode() {
        Mode::Paper => Color32::from_rgb(232, 232, 232), // #E8E8E8
        Mode::Ink => Color32::from_rgb(48, 48, 48),      // #303030
    }
}

/// Vermilion, demoted to FUNCTIONAL signals only (WP-048 brutalist spec):
/// busy state, grid cursor/focus marker, the logomark dot. The active tab,
/// primary buttons, and all decoration are monochrome now.
pub fn accent() -> Color32 {
    Color32::from_rgb(214, 69, 42) // #D6452A
}

/// Hover/pressed shade of the primary (black) button.
pub fn accent_dim() -> Color32 {
    match mode() {
        Mode::Paper => Color32::from_rgb(48, 48, 48),
        Mode::Ink => Color32::from_rgb(210, 210, 210),
    }
}

/// Text/foreground placed ON the primary button (inverse of ink).
pub fn on_accent() -> Color32 {
    match mode() {
        Mode::Paper => Color32::from_rgb(255, 255, 255),
        Mode::Ink => Color32::from_rgb(10, 10, 10),
    }
}

/// Primary button fill: solid black on paper, solid white on ink.
pub fn primary_fill() -> Color32 {
    ink()
}

/// Errors: brick that stays legible on either desk.
pub fn error_ink() -> Color32 {
    match mode() {
        Mode::Paper => Color32::from_rgb(140, 58, 58), // #8C3A3A
        Mode::Ink => Color32::from_rgb(212, 116, 106), // #D4746A
    }
}

/// Success state ink.
pub fn ok_ink() -> Color32 {
    match mode() {
        Mode::Paper => Color32::from_rgb(63, 122, 78), // #3F7A4E
        Mode::Ink => Color32::from_rgb(126, 184, 142), // #7EB88E
    }
}

/// Warning state ink.
pub fn warn_ink() -> Color32 {
    match mode() {
        Mode::Paper => Color32::from_rgb(168, 115, 30), // #A8731E
        Mode::Ink => Color32::from_rgb(216, 169, 90),   // #D8A95A
    }
}

/// The white photo mat behind compare prints (light-table look).
pub fn mat() -> Color32 {
    match mode() {
        Mode::Paper => Color32::from_rgb(253, 252, 248), // print white
        Mode::Ink => Color32::from_rgb(229, 224, 211),   // paper print on dark table
    }
}

/// Thin solid black 1px line: panel divisions, widget borders, tab rules.
pub fn rule_stroke() -> Stroke {
    Stroke::new(1.0, rule())
}

/// Secondary 1px line at ~40% strength for dividers inside quiet regions.
pub fn rule_soft_stroke() -> Stroke {
    Stroke::new(1.0, rule_soft())
}

/// 1px ink stroke for emphasized edges (active widgets, focus).
pub fn hairline_stroke() -> Stroke {
    Stroke::new(1.0, ink())
}

/// Sharp corners everywhere (brutalist spec: no rounding).
pub fn rounding() -> Rounding {
    Rounding::ZERO
}

// ---------------------------------------------------------------------------
// Fonts
// ---------------------------------------------------------------------------

/// Named font family used by headings (SemiBold face).
pub const HEADING_FAMILY: &str = "facial-heading";

/// Embed Inter (UI) + Inter SemiBold (headings) + Phosphor icons, keeping
/// egui's default fonts as fallbacks for glyph coverage (arrows, symbols).
/// Call once at startup, before the first frame.
/// Windows faces that cover the scripts the vendored fonts do not, ordered so
/// the broadest coverage is tried first. `Segoe UI Emoji` is a COLR/CPAL colour
/// font; epaint rasterizes monochrome outlines only, so emoji resolve in black
/// and white (WP-070).
/// Several candidates per script: Windows SKUs and language packs vary in which
/// faces are installed (Meiryo in particular is absent on many Windows 11
/// installs), so the first file that loads wins and a script is only lost when
/// every candidate for it is missing.
#[cfg(windows)]
const SYSTEM_FALLBACK_FILES: &[(&str, &[&str])] = &[
    // Japanese: Meiryo, then Yu Gothic, then MS Gothic.
    (
        "facial-sys-jp",
        &["meiryo.ttc", "YuGothR.ttc", "yugothic.ttf", "msgothic.ttc"],
    ),
    // Korean: Malgun Gothic, then Gulim.
    ("facial-sys-kr", &["malgun.ttf", "gulim.ttc"]),
    // Thai: Leelawadee UI, then Leelawadee, then Tahoma.
    (
        "facial-sys-th",
        &["LeelawUI.ttf", "leelawad.ttf", "tahoma.ttf"],
    ),
    // Chinese: Microsoft YaHei, then SimSun. YaHei also carries kana, so it
    // doubles as a Japanese backstop when no Japanese face is installed.
    ("facial-sys-cjk", &["msyh.ttc", "simsun.ttc"]),
    // Emoji: Segoe UI Emoji. It is a COLR/CPAL colour font and epaint only
    // rasterizes monochrome outlines, so this is a coverage backstop behind
    // egui's own NotoEmoji rather than colour emoji support.
    ("facial-sys-emoji", &["seguiemj.ttf"]),
];

/// Resolve the system font directory without hardcoding a drive letter or user
/// profile path, keeping the disk-agnostic rule intact (FACIAL-BUILD-003).
#[cfg(windows)]
fn system_font_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("SystemRoot")
        .or_else(|| std::env::var_os("windir"))
        .map(|root| std::path::Path::new(&root).join("Fonts"))
}

/// Load optional system script faces and return the family names that resolved.
#[cfg(windows)]
fn load_system_fallback_fonts(fonts: &mut egui::FontDefinitions) -> Vec<String> {
    let Some(dir) = system_font_dir() else {
        return Vec::new();
    };
    let mut loaded = Vec::new();
    for (family, candidates) in SYSTEM_FALLBACK_FILES {
        for file in *candidates {
            // A Windows SKU without a given face is expected, not an error:
            // try the next candidate, and if none load, that script simply
            // keeps the previous (tofu) behavior rather than failing startup.
            let Ok(bytes) = std::fs::read(dir.join(file)) else {
                continue;
            };
            fonts
                .font_data
                .insert((*family).to_string(), egui::FontData::from_owned(bytes));
            loaded.push((*family).to_string());
            break;
        }
    }
    loaded
}

#[cfg(not(windows))]
fn load_system_fallback_fonts(_fonts: &mut egui::FontDefinitions) -> Vec<String> {
    Vec::new()
}

/// Names of the system fallback faces that actually loaded, for diagnostics and
/// tests (WP-070). Empty on non-Windows or when no candidate resolved.
/// The bytes of each resolved system fallback face, for consumers that keep
/// their own font database — notably the inspector's resvg rasterizer, which
/// does not share egui's font chain (WP-070).
pub fn system_fallback_font_data() -> Vec<(String, Vec<u8>)> {
    #[cfg(windows)]
    {
        let Some(dir) = system_font_dir() else {
            return Vec::new();
        };
        let mut loaded = Vec::new();
        for (family, candidates) in SYSTEM_FALLBACK_FILES {
            for file in *candidates {
                if let Ok(bytes) = std::fs::read(dir.join(file)) {
                    loaded.push(((*family).to_string(), bytes));
                    break;
                }
            }
        }
        loaded
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}

pub fn system_fallback_font_report() -> Vec<(String, String)> {
    #[cfg(windows)]
    {
        let Some(dir) = system_font_dir() else {
            return Vec::new();
        };
        let mut report = Vec::new();
        for (family, candidates) in SYSTEM_FALLBACK_FILES {
            for file in *candidates {
                let path = dir.join(file);
                if path.exists() {
                    report.push(((*family).to_string(), (*file).to_string()));
                    break;
                }
            }
        }
        report
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}

pub fn install_fonts(ctx: &egui::Context) {
    use egui::{FontData, FontDefinitions, FontFamily};

    let mut fonts = FontDefinitions::default();

    fonts.font_data.insert(
        "Inter".into(),
        FontData::from_static(include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/assets/fonts/Inter-Regular.ttf"
        ))),
    );
    fonts.font_data.insert(
        "Inter-SemiBold".into(),
        FontData::from_static(include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/assets/fonts/Inter-SemiBold.ttf"
        ))),
    );
    // Brutalist display face for headings/tabs (WP-048, vendored OFL).
    fonts.font_data.insert(
        "SpaceGrotesk".into(),
        FontData::from_static(include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/assets/fonts/SpaceGrotesk-Medium.ttf"
        ))),
    );
    // Data face: paths, receipts, code, logs (WP-048, vendored OFL).
    fonts.font_data.insert(
        "IBMPlexMono".into(),
        FontData::from_static(include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/assets/fonts/IBMPlexMono-Regular.ttf"
        ))),
    );

    // WP-070: none of the vendored faces, and none of egui's defaults, contain
    // a single Japanese, Korean, Thai or CJK glyph — those filenames render as
    // tofu. Windows already ships faces that cover them, so load those from the
    // platform font directory instead of bundling tens of megabytes of Noto
    // into both delivery artifacts. Every load is optional: a missing face
    // degrades to current behavior and never blocks startup.
    let system_faces = load_system_fallback_fonts(&mut fonts);

    // UI face first, egui defaults retained behind it for coverage, then the
    // system script faces so Latin rendering is completely unchanged.
    if let Some(prop) = fonts.families.get_mut(&FontFamily::Proportional) {
        prop.insert(0, "Inter".into());
        prop.extend(system_faces.iter().cloned());
    }
    // Monospace: Plex Mono first, egui's default mono behind it.
    if let Some(mono) = fonts.families.get_mut(&FontFamily::Monospace) {
        mono.insert(0, "IBMPlexMono".into());
        mono.extend(system_faces.iter().cloned());
    }

    // Heading family: Space Grotesk display, Inter weights behind it.
    let mut heading_chain: Vec<String> = vec![
        "SpaceGrotesk".into(),
        "Inter-SemiBold".into(),
        "Inter".into(),
    ];
    if let Some(prop) = fonts.families.get(&FontFamily::Proportional) {
        // Skips "Inter" (already at the head) but keeps the egui defaults and
        // the WP-070 system script faces, so headings and tab titles render
        // non-Latin folder names too.
        heading_chain.extend(prop.iter().skip(1).cloned());
    }
    fonts
        .families
        .insert(FontFamily::Name(HEADING_FAMILY.into()), heading_chain);

    // Phosphor icons merge into both proportional chains as a fallback.
    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
    if let Some(chain) = fonts.families.get(&FontFamily::Proportional).cloned() {
        if let Some(heading) = fonts
            .families
            .get_mut(&FontFamily::Name(HEADING_FAMILY.into()))
        {
            for name in chain {
                if !heading.contains(&name) {
                    heading.push(name);
                }
            }
        }
    }

    ctx.set_fonts(fonts);
}

// ---------------------------------------------------------------------------
// Style installation
// ---------------------------------------------------------------------------

/// Build the flat Visuals for the active mode (exposed for reuse/tests).
pub fn flat_visuals() -> egui::Visuals {
    let mut visuals = match mode() {
        Mode::Paper => egui::Visuals::light(),
        Mode::Ink => egui::Visuals::dark(),
    };

    visuals.override_text_color = Some(ink());
    visuals.window_fill = sheet();
    visuals.panel_fill = desk();
    visuals.extreme_bg_color = well();
    visuals.faint_bg_color = well();
    visuals.code_bg_color = well();
    visuals.hyperlink_color = ink_soft();
    visuals.warn_fg_color = warn_ink();
    visuals.error_fg_color = error_ink();

    visuals.window_rounding = Rounding::ZERO;
    visuals.menu_rounding = Rounding::ZERO;
    visuals.window_shadow = egui::epaint::Shadow::NONE;
    visuals.popup_shadow = egui::epaint::Shadow::NONE;
    visuals.window_stroke = rule_stroke();

    visuals.selection = egui::style::Selection {
        bg_fill: selection_bg(),
        stroke: Stroke::new(1.0, ink()),
    };

    let widgets = &mut visuals.widgets;

    // Static text/frames: invisible chrome, ink text.
    widgets.noninteractive.bg_fill = sheet();
    // NEVER make this transparent: egui fades disabled widgets toward
    // `noninteractive.weak_bg_fill` (Visuals::fade_out_to_color), and a
    // TRANSPARENT target makes every `add_enabled(false, ..)` widget fully
    // invisible instead of grayed out (Painter::is_visible -> false).
    widgets.noninteractive.weak_bg_fill = well();
    widgets.noninteractive.bg_stroke = rule_stroke();
    widgets.noninteractive.fg_stroke = Stroke::new(1.0, ink());
    widgets.noninteractive.rounding = rounding();

    // Resting interactive widgets: sheet fill, quiet border, soft ink label.
    widgets.inactive.bg_fill = sheet();
    widgets.inactive.weak_bg_fill = sheet();
    widgets.inactive.bg_stroke = rule_stroke();
    widgets.inactive.fg_stroke = Stroke::new(1.0, ink_soft());
    widgets.inactive.rounding = rounding();

    // Hover: ink border + full ink label; fill barely shifts (flat, no glow).
    widgets.hovered.bg_fill = selection_bg();
    widgets.hovered.weak_bg_fill = selection_bg();
    widgets.hovered.bg_stroke = Stroke::new(1.0, ink_soft());
    widgets.hovered.fg_stroke = Stroke::new(1.0, ink());
    widgets.hovered.rounding = rounding();

    widgets.active.bg_fill = selection_bg();
    widgets.active.weak_bg_fill = selection_bg();
    widgets.active.bg_stroke = hairline_stroke();
    widgets.active.fg_stroke = Stroke::new(1.0, ink());
    widgets.active.rounding = rounding();

    widgets.open.bg_fill = selection_bg();
    widgets.open.weak_bg_fill = selection_bg();
    widgets.open.bg_stroke = rule_stroke();
    widgets.open.fg_stroke = Stroke::new(1.0, ink());
    widgets.open.rounding = rounding();

    visuals
}

/// Install the flat theme for the active mode: visuals + roomier flat spacing.
/// Call at startup and again whenever the mode changes.
pub fn install_style(ctx: &egui::Context) {
    ctx.set_visuals(flat_visuals());
    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.spacing.button_padding = egui::vec2(10.0, 4.0);
    style.spacing.menu_margin = egui::Margin::same(8.0);
    style.spacing.window_margin = egui::Margin::same(10.0);
    style.spacing.indent = 18.0;
    style.spacing.interact_size.y = 26.0;
    // WP-070: egui defaults sliders to 100 points. The windowed Viewer scrubber
    // inherited that and read as far too short next to the width-derived
    // fullscreen one. Transport sliders size themselves explicitly; this raises
    // the floor for any slider that does not.
    style.spacing.slider_width = 220.0;
    // WP-050 accessible floating scrollbars: a large 24-point grab target
    // with a long handle, completely absent at rest and quick to appear on
    // scroll-area/bar hover or drag. Floating bars preserve layout width.
    let mut scroll = egui::style::ScrollStyle::floating();
    scroll.bar_width = 24.0;
    scroll.floating_width = 20.0;
    scroll.handle_min_length = 64.0;
    // Floating bars reserve no layout width by design (WP-050). Nested scroll
    // areas therefore draw their bars at the same right-edge x as their parent;
    // the Library folder strip insets itself instead of changing this globally,
    // which would shift every scroll surface in the app (WP-070).
    scroll.floating_allocated_width = 0.0;
    scroll.dormant_background_opacity = 0.0;
    scroll.dormant_handle_opacity = 0.0;
    scroll.active_background_opacity = 0.08;
    scroll.active_handle_opacity = 0.82;
    scroll.interact_background_opacity = 0.18;
    scroll.interact_handle_opacity = 1.0;
    style.spacing.scroll = scroll;
    style.animation_time = 0.045;
    ctx.set_style(style);
}

/// Apply text styles at a chosen base size (points). Headings use the
/// SemiBold family; monospace is reserved for code/event/log text.
/// Call from startup and from the Options tab.
pub fn apply_text_styles(ctx: &egui::Context, base_pt: f32) {
    use egui::{FontFamily, FontId, TextStyle};
    let base = base_pt.clamp(10.0, 48.0);
    let mut style = (*ctx.style()).clone();
    style.text_styles = [
        (
            TextStyle::Heading,
            FontId::new(
                (base * 1.35).round(),
                FontFamily::Name(HEADING_FAMILY.into()),
            ),
        ),
        (
            TextStyle::Body,
            FontId::new(base.round(), FontFamily::Proportional),
        ),
        (
            TextStyle::Monospace,
            FontId::new((base * 0.9).round(), FontFamily::Monospace),
        ),
        (
            TextStyle::Button,
            FontId::new(base.round(), FontFamily::Proportional),
        ),
        (
            TextStyle::Small,
            FontId::new((base * 0.82).round(), FontFamily::Proportional),
        ),
    ]
    .into();
    ctx.set_style(style);
}

// ---------------------------------------------------------------------------
// Building blocks
// ---------------------------------------------------------------------------

/// Section header: ink title over a quiet rule. Replaces bare `ui.heading(..)`.
pub fn section(ui: &mut egui::Ui, title: &str) {
    ui.add_space(2.0);
    ui.label(egui::RichText::new(title).heading().color(ink()));
    hairline(ui);
    ui.add_space(2.0);
}

/// Small faint kicker label for sub-grouping inside a section.
pub fn kicker(ui: &mut egui::Ui, text: &str) {
    ui.add_space(4.0);
    ui.label(
        egui::RichText::new(text.to_uppercase())
            .small()
            .color(ink_faint()),
    );
}

/// 1px quiet rule divider; flat replacement for ui.separator().
pub fn hairline(ui: &mut egui::Ui) {
    let width = ui.available_width();
    let (rect, _response) = ui.allocate_exact_size(egui::vec2(width, 1.0), egui::Sense::hover());
    ui.painter()
        .hline(rect.x_range(), rect.center().y, rule_stroke());
}

/// Opaque overlay panel: white surface with a thin black border, sharp
/// corners — a bordered plane, not a card (no tonal step, no rounding).
pub fn sheet_frame() -> egui::Frame {
    egui::Frame::none()
        .fill(sheet())
        .stroke(rule_stroke())
        .rounding(Rounding::ZERO)
        .inner_margin(egui::Margin::same(10.0))
}

/// Recessed zone for image viewports / log areas: whisper-off-white field
/// bounded by a thin black line, sharp corners.
pub fn well_frame() -> egui::Frame {
    egui::Frame::none()
        .fill(well())
        .stroke(rule_stroke())
        .rounding(Rounding::ZERO)
        .inner_margin(egui::Margin::same(4.0))
}

/// The primary action button — solid black (white in Ink), at most one per
/// surface. Monochrome per the brutalist spec; vermilion is signals-only.
pub fn primary_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    let label = egui::RichText::new(text).color(on_accent());
    let button = egui::Button::new(label)
        .fill(primary_fill())
        .stroke(Stroke::NONE)
        .rounding(Rounding::ZERO);
    let response = ui.add(button);
    if response.hovered() {
        // Flat hover: one shade lighter, same geometry.
        ui.painter().rect(
            response.rect,
            Rounding::ZERO,
            accent_dim().gamma_multiply(0.35),
            Stroke::NONE,
        );
    }
    response
}

/// Disabled-aware variant of [`primary_button`].
pub fn primary_button_enabled(ui: &mut egui::Ui, enabled: bool, text: &str) -> egui::Response {
    if enabled {
        primary_button(ui, text)
    } else {
        ui.add_enabled(false, egui::Button::new(text))
    }
}

/// Flat tab strip item: ink label with a 2px SOLID BLACK underline when
/// active (monochrome brutalist spec). Returns true when clicked.
pub fn tab_item(ui: &mut egui::Ui, active: bool, label: &str) -> bool {
    let text = egui::RichText::new(label).color(if active { ink() } else { ink_soft() });
    let response = ui
        .add(egui::Label::new(text).sense(egui::Sense::click()))
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    let underline_y = response.rect.bottom() + 3.0;
    if active {
        ui.painter().hline(
            response.rect.x_range(),
            underline_y,
            Stroke::new(2.0, ink()),
        );
    } else if response.hovered() {
        ui.painter().hline(
            response.rect.x_range(),
            underline_y,
            Stroke::new(2.0, rule_soft()),
        );
    }
    response.clicked()
}

// ---------------------------------------------------------------------------
// Paper grain (WP-048: "flat paper look with rough grain")
// ---------------------------------------------------------------------------

/// Edge of the tiled grain texture in pixels.
pub const GRAIN_TILE: usize = 192;

/// Build the deterministic rough-grain tile: seeded xorshift noise, black at
/// 0-5% alpha (white in Ink mode is handled at paint time via tint). Subtle
/// enough to never affect text contrast; deterministic so inspector runs and
/// app sessions always produce the same paper.
pub fn grain_texture(ctx: &egui::Context) -> egui::TextureHandle {
    let mut state: u32 = 0x9E37_79B9; // fixed seed — same paper every run
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        state
    };
    let mut pixels = Vec::with_capacity(GRAIN_TILE * GRAIN_TILE);
    for _ in 0..GRAIN_TILE * GRAIN_TILE {
        let r = next();
        // Sparse speckle: ~1 in 6 pixels carries any ink at all.
        let alpha = if r % 6 == 0 { (r >> 8) % 13 } else { 0 } as u8;
        pixels.push(egui::Color32::from_black_alpha(alpha));
    }
    let image = egui::ColorImage {
        size: [GRAIN_TILE, GRAIN_TILE],
        pixels,
    };
    ctx.load_texture("paper-grain", image, egui::TextureOptions::NEAREST)
}

/// Tile the grain across `rect` (call FIRST inside a panel so all widgets
/// paint above it). In Ink mode the black speckle is repainted white-ish via
/// additive-looking tint inversion (kept subtle either way).
pub fn paint_grain(painter: &egui::Painter, rect: egui::Rect, texture: &egui::TextureHandle) {
    let tile = GRAIN_TILE as f32;
    let tint = match mode() {
        Mode::Paper => Color32::WHITE,
        // Dark mode: dial the speckle far down (black-on-black is invisible;
        // heavy inversion would sparkle). A faint gray tint keeps texture.
        Mode::Ink => Color32::from_gray(60),
    };
    let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
    let mut y = rect.min.y;
    while y < rect.max.y {
        let mut x = rect.min.x;
        while x < rect.max.x {
            let cell = egui::Rect::from_min_size(
                egui::pos2(x, y),
                egui::vec2(tile.min(rect.max.x - x), tile.min(rect.max.y - y)),
            );
            // Clip the last partial tile's UV so the pattern never stretches.
            let cell_uv = egui::Rect::from_min_max(
                uv.min,
                egui::pos2(cell.width() / tile, cell.height() / tile),
            );
            painter.image(texture.id(), cell, cell_uv, tint);
            x += tile;
        }
        y += tile;
    }
}

/// Paint the facial logomark: face-detection corner brackets around an accent
/// dot. `size` is the square edge length; returns the rect it occupied.
pub fn logomark(ui: &mut egui::Ui, size: f32) -> egui::Rect {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
    paint_logomark(ui.painter(), rect);
    rect
}

/// Logomark painter shared by the header widget and the window-icon generator.
pub fn paint_logomark(painter: &egui::Painter, rect: egui::Rect) {
    let stroke = Stroke::new((rect.width() * 0.085).max(1.5), ink());
    let arm = rect.width() * 0.28;
    let r = rect.shrink(rect.width() * 0.08);
    let corners = [
        (r.left_top(), egui::vec2(arm, 0.0), egui::vec2(0.0, arm)),
        (r.right_top(), egui::vec2(-arm, 0.0), egui::vec2(0.0, arm)),
        (r.left_bottom(), egui::vec2(arm, 0.0), egui::vec2(0.0, -arm)),
        (
            r.right_bottom(),
            egui::vec2(-arm, 0.0),
            egui::vec2(0.0, -arm),
        ),
    ];
    for (corner, dx, dy) in corners {
        painter.line_segment([corner, corner + dx], stroke);
        painter.line_segment([corner, corner + dy], stroke);
    }
    painter.circle_filled(rect.center(), rect.width() * 0.14, accent());
}

/// Render the logomark motif into a raw RGBA buffer for the OS window icon.
/// Pure pixel code (no egui painter): paper field, denim brackets, accent dot.
pub fn window_icon_rgba(size: usize) -> Vec<u8> {
    let s = size as f32;
    let mut rgba = vec![0u8; size * size * 4];
    let paper = (240u8, 237u8, 228u8);
    let denim = (38u8, 56u8, 89u8);
    let verm = (214u8, 69u8, 42u8);

    let stroke_w = (s * 0.10).max(2.0);
    let inset = s * 0.14;
    let arm = s * 0.30;
    let dot_r = s * 0.16;
    let c = s / 2.0;

    let in_bracket = |x: f32, y: f32| -> bool {
        let near = |v: f32, target: f32| (v - target).abs() <= stroke_w / 2.0;
        let lo = inset;
        let hi = s - inset;
        // horizontal arms
        ((near(y, lo) || near(y, hi)) && ((x >= lo && x <= lo + arm) || (x >= hi - arm && x <= hi)))
            // vertical arms
            || ((near(x, lo) || near(x, hi))
                && ((y >= lo && y <= lo + arm) || (y >= hi - arm && y <= hi)))
    };

    for y in 0..size {
        for x in 0..size {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            let d = ((fx - c).powi(2) + (fy - c).powi(2)).sqrt();
            let px = if d <= dot_r {
                verm
            } else if in_bracket(fx, fy) {
                denim
            } else {
                paper
            };
            let i = (y * size + x) * 4;
            rgba[i] = px.0;
            rgba[i + 1] = px.1;
            rgba[i + 2] = px.2;
            rgba[i + 3] = 255;
        }
    }
    rgba
}

#[cfg(test)]
mod tests {
    use super::*;

    /// WP-070. The inspector rendered tofu for Japanese and Korean filenames
    /// while the fonts were supposedly installed, so assert the two halves
    /// separately: that a candidate file resolves on this machine, and that the
    /// bytes we hand egui actually contain the glyphs for that script.
    #[cfg(windows)]
    #[test]
    fn system_fallback_faces_resolve_and_cover_their_scripts() {
        use ab_glyph::{Font, FontVec};

        let report = system_fallback_font_report();
        assert!(
            !report.is_empty(),
            "no Windows fallback face resolved; international filenames would render as tofu"
        );

        let dir = system_font_dir().expect("system font dir");
        // One representative codepoint per script we claim to support.
        let probes: &[(&str, char)] = &[
            ("facial-sys-jp", '\u{65e5}'),    // CJK ideograph used in Japanese
            ("facial-sys-kr", '\u{d55c}'),    // Hangul syllable
            ("facial-sys-th", '\u{0e01}'),    // Thai character
            ("facial-sys-cjk", '\u{4e2d}'),   // Chinese ideograph
        ];
        for (family, probe) in probes {
            let Some((_, file)) = report.iter().find(|(name, _)| name == family) else {
                // Absent on this SKU is acceptable; absence is handled at load.
                continue;
            };
            let bytes = std::fs::read(dir.join(file))
                .unwrap_or_else(|error| panic!("read {file}: {error}"));
            let font = match FontVec::try_from_vec(bytes) {
                Ok(font) => font,
                Err(error) => panic!(
                    "{family}: egui cannot parse {file} ({error}); it would be inserted into the \
                     font chain but contribute no glyphs"
                ),
            };
            assert_ne!(
                font.glyph_id(*probe).0,
                0,
                "{family}: {file} parsed but has no glyph for U+{:04X}",
                *probe as u32
            );
        }
    }

    /// WP-070. The files parse and cover their scripts, so if filenames still
    /// render as tofu the wiring is wrong. Assert the loaded faces actually
    /// reach the families egui resolves text through, and that egui reports a
    /// real advance width for a Japanese glyph rather than the replacement box.
    #[cfg(windows)]
    #[test]
    fn system_fallback_faces_are_wired_into_the_font_families() {
        let report = system_fallback_font_report();
        if report.is_empty() {
            return; // no faces on this SKU; load path is a documented no-op
        }
        let ctx = egui::Context::default();
        install_fonts(&ctx);
        // Fonts are only realized during a frame.
        let _ = ctx.run(egui::RawInput::default(), |_| {});

        // A face that loaded must appear in the proportional chain, otherwise
        // it contributes no glyphs no matter how complete its cmap is.
        let font_id = egui::FontId::new(14.0, egui::FontFamily::Proportional);
        let missing = ctx.fonts(|fonts| fonts.glyph_width(&font_id, '\u{e000}'));
        for (family, file) in &report {
            let probe = match family.as_str() {
                "facial-sys-jp" | "facial-sys-cjk" => '\u{65e5}',
                "facial-sys-kr" => '\u{d55c}',
                "facial-sys-th" => '\u{0e01}',
                _ => continue,
            };
            let width = ctx.fonts(|fonts| fonts.glyph_width(&font_id, probe));
            assert!(
                width > 0.0 && (width - missing).abs() > f32::EPSILON,
                "{family} ({file}) loaded but egui resolves U+{:04X} to the replacement glyph \
                 (width {width} == missing-glyph width {missing}); the face is not reachable \
                 from the proportional family",
                probe as u32
            );
        }
    }
}
