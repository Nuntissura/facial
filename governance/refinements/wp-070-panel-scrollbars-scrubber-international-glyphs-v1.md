---
file_id: REF-WP-070-PANEL-SCROLLBARS-SCRUBBER-INTERNATIONAL-GLYPHS-V1
file_kind: refinement
updated_at: "2026-08-12"
---

<topic id="operator-request" status="active" version="1" wp="WP-070" summary="Fix the overlapping left-panel scrollbars, lengthen the playback scrubbers, and render Japanese, Korean, Thai, Cyrillic and emoji filenames." updated_at="2026-08-12">

## Operator request

Verbatim operator items folded into this packet:

- "scrollbar in the left panel for the file list overlaps with the left panel scrollbar"
- "playback scrubbers are to short"
- "support of japanese, korean, thai, cyrilic/rusian, emojis to display in filenames"

## Interpretation

Three independent presentation defects on the Media surface, grouped because they are all
single-surface rendering corrections with no shared runtime state and no behavioral coupling.

</topic>

<topic id="evidence-and-research" status="active" version="1" wp="WP-070" summary="Floating scrollbars reserve no layout width so a nested strip scrollbar lands on the outer one; the windowed Viewer scrubber uses egui's default 100pt width; and no loaded font has any CJK or Thai glyph, though Cyrillic and monochrome emoji are covered." updated_at="2026-08-12">

## Baseline project evidence (inspected, current HEAD)

### Overlapping left-panel scrollbars

- The Library page is a `child_ui` over the left rect (`product/src/ui.rs:7054`) inside
  `CentralPanel`; Media is not wrapped in the generic `tab_body_scroll`
  (`product/src/ui.rs:15450-15453`).
- Outer grid viewport (`product/src/ui.rs:7078-7081`):
  ```rust
  let scroll_out = ScrollArea::vertical()
      .id_source(("media_grid_scroll", &active_media_tab_id))
      .auto_shrink([false, false])
      .show_viewport(&mut child, |ui, viewport| {
  ```
- Inner folder strip **nested inside the grid's own content**
  (`product/src/ui.rs:7123-7127`), height-limited by `strip_max`
  (`product/src/ui.rs:7073-7076`, `product/src/ui.rs:7122`):
  ```rust
  ScrollArea::vertical()
      .id_source(("media_folder_strip", &active_media_tab_id))
      .max_height(list_h)
      .auto_shrink([false, true])
      .show_rows(ui, row_h, child_count + 1, |ui, visible_rows| {
  ```
  Rows are allocated at `ui.available_width()` (`product/src/ui.rs:7132`, `:7153`), i.e. the
  full inner width.
- Neither call sets `scroll_bar_visibility`; both inherit the global floating style
  (`product/src/theme.rs:392-405`):
  ```rust
  let mut scroll = egui::style::ScrollStyle::floating();
  scroll.bar_width = 24.0;
  scroll.floating_width = 20.0;
  scroll.handle_min_length = 64.0;
  scroll.floating_allocated_width = 0.0;   // reserves no layout width
  ```
- **Root cause:** because `floating_allocated_width = 0.0`, the inner strip's scrollbar is drawn
  at the same right-edge x as the outer grid scrollbar, over the strip's vertical band. The
  overlap region is exactly `product/src/ui.rs:7123-7186` inside `product/src/ui.rs:7078`.
- Both id_sources are tab-scoped (`product/src/ui.rs:7077`), so this is a geometry collision,
  not an ID collision. Note for hygiene: the navigator lists at
  `product/src/ui.rs:9180-9189` do share one id_source across two branches.
- Other Media scroll areas for reference: Viewer labels
  (`product/src/ui.rs:8184-8188`), favorites overlay (`product/src/ui.rs:9436-9438`), tab strip
  (`product/src/ui.rs:5968-5969`), navigator lists (`product/src/ui.rs:9104-9107`,
  `:9150-9153`).

### Scrubbers

`product/src/theme.rs` never sets `style.spacing.slider_width`, so egui's default of 100 pt
applies wherever a `Slider` is added without `add_sized`. Three transports exist with three
different sizing rules:

1. **Library inline tile** — `draw_media_inline_video_tile`
   (`product/src/ui.rs:7601-7742`). Scrubber only while hovered
   (`product/src/ui.rs:7682`), sized from available width
   (`product/src/ui.rs:7686-7693`): `let width = (ui.available_width() - 8.0).max(48.0);`
   with `add_sized([width, 24.0], …)`. Volume is on a separate row
   (`product/src/ui.rs:7711-7732`).
2. **Viewer, windowed** — `draw_media_video_preview` (`product/src/ui.rs:8345`), non-fullscreen
   branch. **Uses plain `ui.add`, i.e. the 100 pt default**
   (`product/src/ui.rs:8609-8614`):
   ```rust
   if ui.add(egui::Slider::new(&mut time, 0.0..=length).show_value(false).clamp_to_range(true))
       .on_hover_text("Scrub timeline").changed()
   ```
   It shares the row with a 64×48 play button (`product/src/ui.rs:8585-8588`) and a 20 pt time
   label (`product/src/ui.rs:8623-8631`); volume is on the next row
   (`product/src/ui.rs:8644-8663`). **This is the shortest scrubber and the primary target.**
3. **Viewer, fullscreen / chrome-hidden** — `product/src/ui.rs:8518-8525`:
   `let scrub_width = (controls.available_width() - 250.0).max(80.0);` with
   `add_sized([scrub_width, 36.0], …)`. The hard-coded 250 pt reserve covers a trailing time
   label, a speaker icon, and a 90×36 volume slider sharing the row
   (`product/src/ui.rs:8534-8556`).

This asymmetry explains the operator's experience: the fullscreen scrubber scales with width,
the windowed one does not.

### International filenames and emoji

- Font setup is `install_fonts` at `product/src/theme.rs:231-305`, called once from
  `product/src/ui.rs:976`. Defaults are retained —
  `let mut fonts = FontDefinitions::default();` (`product/src/theme.rs:234`) — and only
  `insert(0, …)` is used, so egui's own fallbacks stay in the chains.
- Vendored fonts, all `include_bytes!` from `product/assets/fonts/`: `Inter-Regular`
  (`product/src/theme.rs:236-242`), `Inter-SemiBold` (`:243-249`), `SpaceGrotesk-Medium`
  (`:251-257`), `IBMPlexMono-Regular` (`:259-265`), plus `egui_phosphor` icons
  (`product/src/theme.rs:290`). The assets directory holds exactly these four TTFs and two OFL
  licenses — **no CJK, Thai, or emoji font is vendored.**
- Chains: Proportional `["Inter", "Ubuntu-Light", "NotoEmoji-Regular", "emoji-icon-font",
  "phosphor"]` (`product/src/theme.rs:268-270`); Monospace
  (`product/src/theme.rs:272-274`); `FontFamily::Name("facial-heading")`
  (`product/src/theme.rs:226`, built `:277-302`). Grid filename captions use
  `TextStyle::Small` → Proportional (`product/src/ui.rs:7898-7911`).
- **Measured cmap coverage of the shipped TTFs** (parsed directly from the files):

  | Font | Latin | Cyrillic | Greek | Hiragana | CJK | Hangul | Thai | Emoji U+1F600 |
  |---|---|---|---|---|---|---|---|---|
  | Inter-Regular | Y | **Y** | Y | n | n | n | n | n |
  | Inter-SemiBold | Y | **Y** | Y | n | n | n | n | n |
  | SpaceGrotesk-Medium | Y | n | n | n | n | n | n | n |
  | IBMPlexMono-Regular | Y | Y | n | n | n | n | n | n |
  | Ubuntu-Light (egui default) | Y | **Y** | Y | n | n | n | n | n |
  | Hack-Regular (egui default) | Y | Y | Y | n | n | n | n | n |
  | NotoEmoji-Regular (egui default) | n | n | n | n | n | n | n | **Y** |
  | emoji-icon-font (egui default) | n | n | n | n | n | n | n | n |

- **Confirmed gap: Japanese, Korean, Thai and CJK have zero coverage in every loaded font.**
  Those filenames must render as tofu.
- **Cyrillic is already covered** by Inter and the retained Ubuntu-Light fallback, and
  monochrome emoji U+1F600 by the retained NotoEmoji-Regular. The operator's Russian and emoji
  reports are therefore **not explained by glyph coverage** and are recorded as
  NOT_DETERMINED pending live reproduction. One partial explanation exists for headings and
  tab labels specifically: `SpaceGrotesk` has no Cyrillic and would fall through to
  `Inter-SemiBold`, which does — so it should still render.
- No system-font loading exists anywhere: a repo-wide search for
  `Meiryo|Malgun|Leelawadee|Segoe|Windows/Fonts|font-kit|fontdb|cosmic-text|FontData::from_owned`
  in `product/src/**.rs` finds only `product/src/theme.rs:231` and
  `product/src/ui_inspect.rs:2000-2022`.
- No font-fallback crate is a direct dependency: the only font deps in `product/Cargo.toml`
  are `egui = "0.27"` (line 20) and `egui-phosphor = "0.5"` (line 22). `fontdb`/`ttf-parser`
  appear in `Cargo.lock` only transitively under `resvg` and are used exclusively for offline
  SVG→PNG in the inspector (`product/src/ui_inspect.rs:1999-2022`,
  `fontdb.set_sans_serif_family("Inter")`), never for the live egui context.

## Current external sources checked

- egui issue #3060 "Supporting Chinese, Japanese and Korean" was closed as *not planned*;
  loading a system font into `FontDefinitions` is the sanctioned workaround, and
  `families.get_mut(&family).insert(0, name)` confirms ordered per-glyph fallback chains:
  <https://github.com/emilk/egui/issues/3060>
- egui discussion #2169 on loading fonts at runtime, including reading a Windows system font
  from disk: <https://github.com/emilk/egui/discussions/2169>
- `egui-system-fonts` — an existing crate that detects the system locale and builds a fallback
  chain for Korean/Japanese/Chinese/Cyrillic/Latin:
  <https://lib.rs/crates/egui-system-fonts>
- `egui-cjk-font` — an existing crate that loads system CJK fonts (Meiryo for Japanese, Malgun
  Gothic for Korean) with a `merge_cjk_font` API that preserves custom fonts:
  <https://crates.io/crates/egui-cjk-font>
- Microsoft `SHGetKnownFolderPath` / `FOLDERID_Fonts` is the supported way to resolve the
  system font directory without hardcoding a drive letter or profile path, which keeps the
  disk-agnostic rule (`FACIAL-BUILD-003`) intact:
  <https://learn.microsoft.com/en-us/windows/win32/api/shlobj_core/nf-shlobj_core-shgetknownfolderpath>
- Windows font inventory: Meiryo (Japanese), Malgun Gothic (Korean), Leelawadee UI (Thai), and
  Segoe UI Emoji ship with Windows 10/11 client SKUs:
  <https://learn.microsoft.com/en-us/typography/fonts/windows_11_font_list>
- Segoe UI Emoji is a COLR/CPAL layered color font; epaint 0.27 rasterizes monochrome outlines
  only, so color emoji will render as the base glyph rather than in color:
  <https://learn.microsoft.com/en-us/typography/opentype/spec/colr>
- Noto CJK is the bundling alternative; the full-coverage variants are tens of megabytes,
  which is the size trade this packet weighs:
  <https://github.com/notofonts/noto-cjk>
- egui `ScrollArea` documentation and `ScrollStyle::floating`, confirming floating scrollbars
  overlay content and that `floating_allocated_width` controls reserved layout width:
  <https://docs.rs/egui/latest/egui/containers/scroll_area/struct.ScrollArea.html>
- egui issue #3316 on floating scrollbar support and its interaction with nested scroll areas:
  <https://github.com/emilk/egui/issues/3316>
- egui PR #4791 "Improved Behavior of Nested ScrollArea", confirming nested-scroll overlap is a
  known upstream area rather than a local misuse:
  <https://github.com/emilk/egui/pull/4791>
- egui `Style::spacing::slider_width` documentation confirming the 100 pt default that the
  windowed Viewer scrubber inherits: <https://docs.rs/crate/egui/0.27.0>
- Hugging Face, Civitai, Reddit, and X/Twitter searches produced no further directly
  transferable implementation evidence for these three items.

## Selected approach

### Scrollbars

- Give the inner folder strip its own horizontal inset so its floating scrollbar cannot land on
  the outer grid scrollbar's x band — reserve the strip's scrollbar lane inside the strip
  rather than changing the global `floating_allocated_width`, which would alter every scroll
  surface in the application.
- Where the strip is short enough not to scroll, no lane is reserved, so the common case loses
  no width.
- Keep the WP-050 scrollbar identity intact: 24 pt grab region, 64 pt minimum handle, hidden at
  rest, revealed on hover.

### Scrubbers

- Give the windowed Viewer scrubber the same width-derived sizing the fullscreen path already
  uses: compute the remaining row width after the transport button and time label, with a
  sensible minimum, via `add_sized`.
- Replace the fullscreen path's hard-coded 250 pt reserve with a measured reserve derived from
  the widgets actually on the row, so the scrubber grows correctly at other window sizes and
  font scales.
- Set an explicit `style.spacing.slider_width` in `theme.rs` so no future slider silently
  inherits the 100 pt default.
- Increase the scrubber hit height for pointer and controller seeking, consistent with the
  couch-distance intent already established in WP-051/WP-062.

### Fonts

- Load Windows system fonts at startup and append them to the existing fallback chains, keeping
  Inter as the primary face so the brutalist visual identity is unchanged: Meiryo (Japanese),
  Malgun Gothic (Korean), Leelawadee UI (Thai), a CJK-capable face for Chinese, and Segoe UI
  Emoji for emoji coverage.
- Resolve the font directory through `SHGetKnownFolderPath(FOLDERID_Fonts)` — a platform
  directory resolution, not a hardcoded path — satisfying `FACIAL-BUILD-003` and
  `[GLOBAL-PORTABILITY]`.
- Every system font is optional: a missing file degrades to the current behavior with a stated
  diagnostic, never a startup failure. The app must remain usable on a Windows SKU that lacks a
  given face.
- Do **not** vendor Noto CJK. Bundling full CJK coverage adds tens of megabytes to a delivery
  artifact governed by the single-canonical-pair invariant, for glyphs the host OS already has.
  This is recorded as an explicit operator-visible trade: system fonts keep the executable small
  but depend on the host having the faces installed.
- Because Cyrillic and monochrome emoji are already covered by the measured cmaps, the operator's
  reports for those two scripts are treated as **unreproduced** and must be reproduced live with
  actual filenames before any change is made for them. If they reproduce, the cause is elsewhere
  — text layout, elision, or the inspector's separate `fontdb` path — and will be diagnosed from
  evidence rather than patched speculatively.
- The inspector's offline SVG rasterization path (`product/src/ui_inspect.rs:1999-2022`) uses a
  separate `fontdb` with `sans_serif_family("Inter")` and will show tofu for CJK even when the
  live app renders correctly. Align it so snapshots are trustworthy evidence.

## Rejected options

- Setting `floating_allocated_width` globally to reserve width: it would shift layout on every
  scroll surface in the application and change the WP-050 minimal-scrollbar identity.
- Removing the nested folder-strip scroll area: the strip needs independent scrolling; that is
  its purpose.
- Making both scrollbars always-visible non-floating: reverses a deliberate WP-050 design
  decision and consumes width permanently.
- Vendoring Noto CJK + Noto Thai + an emoji font: tens of megabytes added to both delivery
  artifacts for glyphs Windows already ships.
- Adding `font-kit` or `cosmic-text`: heavy dependencies for what is a bounded, well-documented
  known-folder file read on the one platform this app targets.
- Rendering color emoji: epaint 0.27 does not rasterize COLR/CPAL layers; monochrome coverage
  is the honest, achievable outcome and will be stated as such.
- "Fixing" Cyrillic and emoji speculatively: the measured cmaps show they are already covered,
  so a change made now would be unverified guesswork.

</topic>

<topic id="red-team-and-verification" status="active" version="1" wp="WP-070" summary="Font memory and startup cost, missing system faces, layout shifts from wider scrubbers, and unreproduced script reports have explicit controls and direct visual proof gates." updated_at="2026-08-12">

## Risks, failure scenarios, and controls

- **Loading several large system fonts inflates startup time and memory.** Meiryo, Malgun
  Gothic and a CJK face are multi-megabyte. Control: measure startup delta and resident memory;
  load off the first-paint path if the measurement warrants it; record the numbers rather than
  assuming.
- **A required system font is absent on the operator's or a user's Windows SKU.** Control: every
  load is optional and independently fallible, with a diagnostic naming which faces resolved;
  absence degrades to current behavior and never blocks startup.
- **Font fallback order regresses Latin rendering.** Inter must stay first. Control: append
  system faces after the existing entries and assert the chain head is unchanged; visually
  compare a Latin-only snapshot before and after for byte-identical layout where possible.
- **Deterministic `ui-inspect` snapshots stop being byte-identical** because font availability
  varies by machine. Control: the inspector must produce deterministic output — pin the
  inspector's font set explicitly rather than inheriting whatever the host has, and state that
  contract.
- **Emoji render as monochrome and the operator expects color.** Control: state the limitation
  plainly in the Manual and this packet; do not claim color emoji support.
- **A wider windowed scrubber pushes the time label or volume control out of the panel at small
  window sizes.** Control: minimum widths for each widget on the row, and inspector coverage at
  a constrained window size and a high font scale, reusing the existing
  `media_settings_constrained_high_font` preset pattern.
- **Reserving a scrollbar lane in the folder strip narrows rows and truncates folder names.**
  Control: reserve only when the strip actually scrolls; inspect strip rendering at short,
  long, and deep folder lists using the existing `media_folders_*` presets.
- **The scrollbar fix is asserted from code reading rather than seen.** Control: the acceptance
  gate is direct inspection of the rendered PNG at a strip length that forces both scrollbars,
  not a code diff.
- **Cyrillic and emoji do not reproduce, and the packet is reported as fixing them.** Control:
  the packet reports exactly what was reproduced and what was not; an unreproduced report is
  recorded as such, not silently claimed.
- **Filenames in scripts with complex shaping (Thai) render with wrong mark placement.** epaint
  does not do full complex-script shaping. Control: verify Thai filenames visually and state
  any shaping limitation honestly rather than claiming full support.

## Verification

- Direct visual inspection — not code reading — of rendered PNGs for each item:
  - a folder strip long enough to scroll inside a grid long enough to scroll, confirming the two
    scrollbars no longer share an x band;
  - the windowed Viewer scrubber at default, narrow, and wide panel widths, plus the fullscreen
    scrubber, confirming both scale;
  - filenames in Japanese, Korean, Thai, Russian, and emoji rendered in the grid caption, the
    Viewer metadata, and the tab title.
- A dedicated test fixture folder containing files named in each target script, so the check is
  reproducible by another model.
- Focused unit tests: font chain construction with all system faces present, with each face
  individually absent, and with none present; slider width derivation at several panel widths;
  scrollbar lane reservation only when scrolling.
- Full `cargo test --manifest-path product/Cargo.toml`.
- Deterministic `facial-cli ui-inspect` presets extended for the international filename fixture,
  the dual-scrollbar state, and the scrubber at constrained width and high font scale; affected
  PNG and `layout.json` artifacts opened and directly inspected.
- Startup time and memory measured before and after font loading, with the numbers recorded.
- Live reproduction attempt for the Cyrillic and emoji reports on real filenames, with the
  outcome recorded either way.
- Independent high-risk adversarial review of font resolution failure paths, inspector
  determinism, and layout behavior at extreme window sizes and font scales.

</topic>
