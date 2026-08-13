---
file_id: facial-manual
file_kind: built_in_manual
updated_at: 2026-08-09
---

# FACIAL — Built-in Manual

<topic id="contents" summary="What Facial is, plus a quick-link contents list to every section">

## Contents

**What Facial is.** Facial is a desktop app for sorting and grading batches of
photos. You bring in a folder of images, pick the automatic checks you want (photo
quality, duplicate-finding, and — when switched on — face identity), press Run, and
the app scores every picture and can split the batch into keep / review / cull piles
for you. Its **Compare** tab can show folders side by side for human compare work.
Headless lane commands provide the model-safe way to split large folders across parallel
agents. It never changes your original files in the normal mode (it works on
copies), and it never opens external pop-up windows or OS file browsers. Most paths
are typed or pasted as text; the Compare tab also has an in-app folder picker that
stays inside Facial.

This manual has two halves. The first half is **for operators** — plain steps for
getting work done, one section per tab. The second half is a clearly separated
**reference part** for technical users and automation (the CLI, the command API, file
paths, recovery, and the like). If you just want to use the app, read the operator
half and ignore the reference half.

### Operator guide

1. [Start here (for operators)](#start-here-for-operators)
2. [The tabs at a glance](#the-tabs-at-a-glance)
3. [Media tab](#media-tab)
4. [Project tab](#project-tab)
5. [Quality & IQ tab](#quality--iq-tab)
6. [Identity tab](#identity-tab)
7. [Duplicates tab](#duplicates-tab)
8. [Run tab](#run-tab)
9. [Compare tab](#compare-tab)
10. [App settings](#app-settings-settings--app)

### Reference part (technical / automation)

10. [Reference: Headless CLI](#reference-headless-cli)
11. [Reference: File-based command + receipt API](#reference-file-based-command--receipt-api)
12. [Reference: AppStateSnapshot schema](#reference-appstatesnapshot-schema)
13. [Reference: Output & artifact paths](#reference-output--artifact-paths)
14. [Reference: Where errors & events appear](#reference-where-errors--events-appear)
15. [Reference: Failure recovery & rerun](#reference-failure-recovery--rerun)
16. [Reference: No-window safety rule](#reference-no-window-safety-rule)
17. [Reference: Identity model provisioning](#reference-identity-model-provisioning)
18. [Reference: Media browser automation](#reference-media-browser-automation)
19. [Reference: GUI inspector](#reference-gui-inspector)

In the in-app **Manual** tab, use the **Quick links** row at the top to jump to any
section.

</topic>

<topic id="start-here" summary="The simplest end-to-end path for a new operator, in plain steps">

## Start here (for operators)

This is the shortest path from a folder of photos to sorted results. Follow it once
and you will understand how the whole app fits together.

1. **Set an output folder.** Open **Settings → App**. In the **Copy output folder**
   box, type or paste a folder path and click **Set**. This is where the app will
   save the pictures it works on and the results it produces. Nothing else in the app
   will run until this is set, so do it first.

2. **Import your images.** Go to the **Project** tab. Open the **Import images**
   section, click into the big box, and type or paste the location of your photos —
   one per line. You can point to a single picture or to a whole folder. Click
   **Import images**. (There is no "browse" pop-up anywhere in this app — you always
   type or paste the path.) Your originals are left untouched; the app works on
   copies.

3. **Pick what to check.** Decide what you want the app to do and tick the matching
   checks on the right tab:
   - **Quality & IQ** tab — grade photos for sharpness, exposure, composition, and an
     overall quality score.
   - **Duplicates** tab — find exact copies, look-alikes, and burst/blink runs.
   - **Identity** tab — confirm a face is really the person you care about (this one
     needs you to switch on the face-recognition engine first; see the Identity tab
     section).
   - Ticking a box just *selects* the check — nothing runs yet.

4. **Run.** Go to the **Run** tab. Check the **Selected features** line shows
   the checks you ticked, then click **Run selected features**. Watch the **Run
   summary** fill in, line by line, as each check finishes.

5. **Review and sort.** Still on **Run**, scroll to **Sort run into folders**
   and click **Sort now**. The app copies your photos into three piles — **keep**,
   **review**, and **cull** — inside your output folder. Nothing is moved or deleted;
   sorting only copies.

6. **Review visually.** Use the new **Media** tab for fast browsing and bulk file actions:
   filter images/videos, select groups, and run Open / Copy / Paste / Delete from
   right-click menus or shortcuts.

7. **Eyeball them.** Use the **Compare** tab any time you want to *look* at pictures
   instead of scoring them — open folders side by side and flip through them to make
   your own call. Headless lane commands support parallel model work over massive folders.

That is the whole loop: output folder → import → tick checks → Run → sort → Media/Compare.

</topic>

<topic id="tour" summary="A one-line plain-language description of each of the nine tabs">

## The tabs at a glance

Eight tabs run across the top of the app, in this left-to-right order: Media, Project,
Quality & IQ, Identity, Duplicates, Run, Compare, Manual. Here is what each
one is for, in one line, so you know where to go.

- **Media** — the front page: a book-style media browser with a Library panel,
  Viewer panel, folder navigation, labels/tags/notes, favorites, full controller
  support, and name/fuzzy/semantic search.
- **Project** — name the job, import your photos, and list the people (models) you
  are sorting for.
- **Quality & IQ** — tick the automatic photo-grading checks (sharpness, exposure,
  composition, overall quality).
- **Identity** — switch on the face-recognition engine and tick identity checks that
  confirm whether a face is the right person.
- **Duplicates** — tick the checks that find exact copies, look-alikes, and burst /
  blink runs.
- **Run** — the "go" button: run your ticked checks, watch results, and sort
  a finished batch into keep / review / cull.
- **Compare** — view folders side by side for human compare work.
- **Manual** — this guide, with quick links to jump to any section at the right end
  of the tab row.

The header **Settings** button beside **Refresh** opens the unified settings window.
Its **App** category sets paths, theme, and text size and exposes Advanced / Debug.

</topic>

<topic id="media-tab" summary="Media tab — the book-style media browser front page">

## Media tab

### What it is

The Media tab is the front page of the app: a persistent folder-tab strip above a
media browser that reads like an open book. The left side is the **Library panel**: folder navigation plus the
virtualized thumbnail overview. The right side is the **Viewer panel**: the selected
image or video, its playback controls, and its metadata outside fullscreen. Those are
the canonical names used in the app, code, diagnostics, and this manual. The Library
panel starts with large **500-point thumbnails** and filenames hidden. Thumbnail images
and the full Viewer are borderless. The tags and notes
editors are also borderless, using a slightly darker recessed fill to remain
obviously editable. Drag the **Library / Viewer split** handle to resize them; press **Tab** to
collapse the Viewer into a **full-window thumbnail wall** and Tab again to
bring the book back. Everything works with mouse, keyboard, or a game
controller.

Each folder tab is a separate viewport over the same media database. It preserves its
folder, selected item(s), cursor, search and filter, search scope, sorting, thumbnail
layout, Library scroll position, and staged folder-navigator location. Use **+** or
**Ctrl+T** to choose
a folder for a new tab, **Ctrl+Tab/Ctrl+Shift+Tab** to move between tabs, and
**Ctrl+W** or the tab close control to close one. Note that **+** and **Ctrl+T** open the
folder browser first — the tab appears when you commit a folder with **Open in new tab**.
Commands sent while that browser is still capturing its blurred backdrop are accepted
rather than rejected, and a failed commit closes the browser and states why instead of
leaving you behind the blur.
Not every tab is a folder: the **★ Favorites** tab is a collection built from the
metadata database (see [Favorites and panels](#favorites-and-panels)).
Switching tabs immediately restores
its last-good inventory while an asynchronous reconciliation scan checks for changes.
That restore now also republishes the tab's last grid order, so the thumbnails are on
screen in the same frame instead of blanking while the order is recomputed, and it works
even for a folder whose previous scan was interrupted or hit an unreadable subfolder.
Tab changes are committed to the shared Media database before the visible viewport
changes. If a stored tab document is corrupt, Facial starts with one safe tab and keeps
the rejected raw value under the `media_tabs_v1_rejected` recovery key; a later clean
save does not erase that recovery copy. If that separate recovery write fails, Facial
leaves the corrupt primary value untouched, disables automatic tab persistence for that
session, and reports `persistence_blocked: true` in Media diagnostics. Repair the
workspace database or switch to a writable workspace before changing tabs you need to
retain, then restart Facial; it will retry recovery before permitting persistence.

### Browsing

- **Choose a folder** with the Browse button, by pasting a path, by clicking a
  breadcrumb segment or folder row, or from the Favorites panel.
- **Folder strip**: the current path renders as clickable breadcrumbs; below it,
  a compact **Drives** row switches directly between assigned disks, and `..` plus
  the child folders are plain rows — click to enter. Drag the short
  handle under the folder list to change how tall it is before it scrolls. Both
  minimalist handles keep a wider invisible grab target, so they are easy to catch.
- **Folders window**: choose **Folders** in the toolbar, press **Ctrl+G**, or press
  controller **Select/Back**. Facial opens a large couch-distance folder navigator
  over a lightly softened media book while leaving the compact desktop strip unchanged.
  Its stable 1800×1360 preferred size is clamped to the current screen, so it cannot
  slowly grow between frames. Folder names and icons are 52 points high in 112-point
  rows with a clear vermilion focus marker. D-pad/left stick moves through the folder
  list; on the drive rail, Left/Right changes disks and Down returns to the folders.
  **A** browses into the focused item; Right enters a focused folder.
  **B** or Left goes to the parent from folder rows; at a drive root it keeps the navigator open
  and focuses the current drive so another disk is one D-pad move away. Select/Back or
  Esc closes it. Assigned disks appear in a large horizontal drive rail above the
  virtualized folder list, so folders remain immediately visible. Returning
  without changing folders preserves the thumbnail cursor and scroll position.
  Mapped NAS drives appear in the rail. For a share with no drive letter, paste
  its UNC location (for example `\\server\share`) into the navigator's **Go** field;
  Facial validates and stages it in-app, or reports that exact location as unavailable.
  Browsing, Parent, and Go never change the active Media tab or trigger its scan. Choose
  **Open folder** to commit the staged path to the current tab, or **Open in new tab**
  to create and select a separate viewport while leaving the prior tab untouched.
  Models use `facial-cli media_folder_navigate --action ACTION` with `open`, `close`,
  `toggle`, `up`, `down`, `page_up`, `page_down`, `home`, `end`, `enter`, `parent`,
  `refresh`, `commit`, or `open_new_tab`; these receipt-backed intents drive the same navigator state as the
  controller and work without injecting keyboard focus.
- **Tree** in the toolbar toggles showing media from all subfolders (recursive).
- The **media type** dropdown selects **Images**, **Videos**, or **All** for the grid.
  It filters only the selected folder and, when **Tree** is on, its subfolders. Visible video tiles get
  real cached frame thumbnails from one dedicated FFmpeg worker; an unavailable or
  failed decoder falls back to the film-strip tile. Video work never occupies the
  image decoder pool. Visible extraction gets a 15-second off-thread attempt cap so
  temporary CPU contention does not become a permanent failed tile; speculative
  prefetch keeps a five-second cap. A fresh Media view starts on **Tree + All**, so the chosen
  folder and every supported image/video below it appear in one progressive view.
- **Sort** by Name, Modified, Size, or Created — click the active one again to
  flip between ascending and descending. The same sort menu is in the right-click
  menu. **Sort is per tab**, so two open tabs can be ordered differently at once,
  and each tab keeps its choice across restarts.
  Name sorts from the path alone; Modified, Size, and Created need a background
  metadata pass, which is collected in one filesystem call per file and is
  cancellable. Files whose creation time the volume does not record sort **last**
  in both directions rather than pretending to be zero. Note that Windows sets a
  *new* creation time when a file is copied, so a copied file can look "newer"
  than the original — that is Windows behavior, not Facial reordering things.
  Choosing a sort no longer blanks the grid: the current order stays on screen
  and is replaced when the new one is ready.
- **Load order**: thumbnails come first. A folder publishes its rows in batches
  while it is still being enumerated, and every batch is immediately renderable,
  so you can look and scroll before the scan finishes — regardless of the sort
  key or an active query. Playback controls, color-label dots, and favorite stars
  fill in afterwards. That order is the point of the app: you should never wait
  on metadata to start looking at a large folder.
- **Filenames in other scripts**: Japanese, Korean, Thai, Chinese, Cyrillic and
  emoji filenames render using fonts Windows already ships. Facial tries several
  candidates per script (for example Meiryo, then Yu Gothic, then MS Gothic for
  Japanese) and falls back silently if none is present, so nothing is bundled and
  the download stays small. On a current Windows 11 machine the resolved faces
  total roughly **57 MB** of font data held in memory; if you only ever see Latin
  filenames you can set `FACIAL_SYSTEM_FONTS=0` to skip them and reclaim it.
  Emoji render in **black and white** in the app — the UI renderer does not draw
  layered color fonts.
- **Thumbnail size**: the toolbar slider, Ctrl+mouse-wheel over the grid, or
  the controller triggers. Thumbnails decode in the background, are cached on
  disk, and appear without blocking scrolling. **Names** toggles filenames; it
  starts off and the choice is saved.
- **Scrollbars** are intentionally large, with a 24-point grab region and a long
  handle. They stay invisible at rest, appear quickly when the scroll area or bar
  is hovered (and while scrolling/dragging), then disappear quickly when idle.
- **Very large folders** publish a first batch of at most 64 items, then bounded
  follow-up batches while the recursive scan continues. A saved last-good
  inventory appears immediately on repeat visits. If a NAS share disconnects or
  part of its tree becomes unreadable, Facial keeps that inventory and labels it
  stale/offline instead of treating an incomplete scan as authoritative deletion.
  The current viewport is decoded first; rapid scrolling drops obsolete queued
  work instead of making the new viewport wait.

### Playing videos

Select a video and press the large **Play** control in the Viewer panel, press **Enter**, or
press controller **A**. Facial loads LibVLC only at that moment and embeds its native
video surface in the Viewer panel; merely scanning, selecting, or scrolling videos
does not load VLC. The large control strip provides play/pause, a scrubbable timeline,
volume, audio-track selection, and subtitle-track selection. Videos **loop by default**;
turn this off in **Settings → Playback**. **Open in VLC** and double-click/Open file
hand the exact path to Windows' registered media-file application (normally VLC when it
is the association). **Choose app…** opens the Windows app selector.

Every video thumbnail also has a small **Play** button. It moves the same single
LibVLC player into that Library tile; Facial never creates one decoder per thumbnail.
The player has an explicit `library` or `viewer` owner, so starting one surface
atomically replaces the other instead of allowing two decoders to contend. The
active tile keeps only play/pause visible at rest and reveals its scrubber and volume
slider on hover. Scrolling or filtering that tile out stops its playback, so an
invisible video cannot keep decoding or playing audio. In fullscreen, Viewer
transport becomes a translucent bottom strip that appears only while the video is
hovered.

Facial discovers VLC from a portable `vlc` folder beside the executable, the normal
Windows Program Files locations, PATH, or `FACIAL_VLC_DIR`. Video thumbnails discover
FFmpeg through PATH or `FACIAL_FFMPEG`. Leaving Media, selecting a different item, or
opening an in-app overlay hides/stops the native video surface so it cannot cover UI.
Embedded playback defaults to VLC's composition-safe `wingdi` output because affected
Direct3D overlay/DPI combinations can produce sound while leaving the host visually blank.
This renderer is loaded only after Play. `FACIAL_VLC_VOUT=direct3d11|direct3d9|directdraw|wingdi|glwin32`
is an expert override for a machine where accelerated 4K playback has been visually verified.
During playback, Facial reserves interactive media capacity and reduces scan, stat,
and thumbnail-prefetch pressure without stopping reconciliation. If an action selects a
video while a very large asynchronous display index is still building, Facial keeps the
requested file pending for ten seconds normally, or for at most 120 seconds while the
same large-folder scan is still reconciling. Terminal publication relocates the exact
canonical file and restores the ordinary ten-second invisible-tile cutoff. A stalled
scan therefore cannot keep an invisible decoder alive indefinitely. Facial scrolls and
attaches playback only when that exact row becomes available rather than silently
targeting another row. The native video window is **clipped to the panel that owns it**, so a tile
scrolled half out of view can no longer paint video over the toolbar or the
Viewer, and a video whose owning tile is not drawn this frame is hidden rather
than left floating. Hiding it repaints the area underneath, so the last frame
cannot linger on screen. Changing folders makes an explicit decision about
playback: a video that is not inside the folder you moved to is stopped and
released, instead of continuing to play audio with no picture.

Set
`FACIAL_PLAYBACK_TRACE` to a writable TSV path for owner, command, timing, native HWND,
and surface-bound diagnostics. The trace now also records the placement
lifecycle — `vlc.show_at` (with the exact pixel bounds and whether clipping
applied), `vlc.clip`, `vlc.hide`, `vlc.stop`, and
`ui.folder_change.stop_playback` — which is the fastest way to tell "not
decoding" from "decoding into a surface nobody placed". An experimental
remote-file VLC cache can be enabled with `FACIAL_VLC_REMOTE_CACHE_MS=50..10000` only
after comparing recorded start, seek, and stall timings; there is deliberately no
guessed default.

### Selecting and acting on files

Click selects; Ctrl+click toggles; Shift+click (or Shift+arrows) selects a
range; Ctrl+A / Ctrl+Shift+A / Ctrl+I select all / none / invert. The
right-click menu is the same Explorer-style list everywhere (tiles, preview,
folder rows, empty space):

- **Open file** (Enter) and **Open file location** (Ctrl+L).
- **Copy** (Ctrl+C) then **Paste** (Ctrl+V) copies files into the current folder.
- **Cut** (Ctrl+X) then Paste **moves** them — cut tiles dim until pasted, and a
  file is never deleted unless its copy arrived intact.
- **Delete** (Delete key) removes the selected files.
- **Rename** (F2) opens an in-app rename box (the extension is kept unless you
  type a new one; name collisions are refused, never overwritten).
- **New folder** creates a folder here; **Refresh** (F5) rescans.
- **Copy absolute path** copies selected file paths (or the folder) as usable full paths.
  **Copy portable path** copies workspace-relative paths where possible, otherwise paths
  relative to the selected media folder so a drive-letter move does not rewrite them.
- **Labels** adds or removes reusable labels for the selected files;
  **Toggle favorite** stars the file
  or, with nothing selected, the current folder.

### Labels, tags, and notes

The Viewer panel carries the file metadata editors. They are related, but each has a
different job:

- **Labels** are reusable named color markers. A file can have zero, one, or several.
  With no labels assigned, use **Create label** to enter a unique name and choose a
  unique color. Use the **Labels** dropdown to add existing labels; the same checked
  list removes an assigned label. Renaming or recoloring a label changes its presentation
  everywhere without disconnecting files because assignments use an immutable internal ID.
- **Tags** are lightweight comma-separated words or phrases. Facial trims them,
  lowercases them, removes duplicates, and stores them in deterministic sorted order.
  Use tags when you want an open-ended vocabulary without creating a color definition.
- **Notes** are free text and keep the text you enter. Use them for descriptions,
  reminders, provenance, or anything that does not fit a reusable tag/label.

Assigned label colors appear in a bounded badge lane at the top-right of each Library
thumbnail; when a tile cannot fit every badge, `+N` reports the hidden remainder. The
favorite and video controls remain in separate positions.

Open **Settings → Media → Label manager** to view every definition, create a label,
rename or recolor it, see its usage count, or remove it. Removing an in-use label always
shows the affected count and requires confirmation; the definition and its assignments
are removed atomically. Names are unique without regard to case, and colors are stored
as unique canonical `#RRGGBB` values even though the normal GUI uses a color picker.

Everything saves to `<workspace_root>/.facial/media/media.redb` and survives restarts
and workspace relocation. File edits debounce for about 800 ms after the last change and
force-save on shutdown or workspace switch. A failed save is shown in the Settings/footer
status and remains queued for retry; it is never silently discarded.

### Search

The toolbar search box searches only the **currently selected Media folder** and the
items included by its Tree setting; it never searches all disks or the whole PC. It combines free text with **filter chips**:

- `tag:hero` — only files carrying that tag; `label:selects` — only files carrying
  that current label name (a stable label ID also works);
  `kind:img` / `kind:vid` — only that media type; `note:word` — notes containing
  the word; `fav:` — only favorites (`fav:0` for only non-favorites). Chips show
  under the toolbar with an × to remove them, and they all combine (AND).
- **Subtract a filter** by putting `!` or `-` in front of it: `!tag:reject`,
  `-label:red`, `-kind:vid`, `!note:draft`, `!fav:`, or a bare word like
  `-blooper` to exclude file names containing it. Both markers work, so whichever
  habit you have is fine. A **quoted** term is always literal, so a file that
  really starts with a hyphen is found with `"-take01"` rather than excluded.
  Additive and subtractive terms combine: `tag:hero -label:red` means *carries
  hero, does not carry red*.
- **This folder** (next to Tree) limits results to files sitting directly in the
  current folder while the subfolder scan stays loaded. It filters what you
  already have rather than rescanning, so toggling it is instant and does not
  throw away the recursive inventory. It is per tab.
- **Ctrl+K** jumps straight to the search box from anywhere in Media.
- The **mode** menu picks how free text ranks: **Name** (substring),
  **Fuzzy** (typo-tolerant subsequence — `rdress` finds `red_dress`), or
  **Semantic** (meaning-based, see below).
- **Autocomplete** pops under the box while you type: tags, labels, folder
  names, and file names, ranked. Tag/label/folder rows complete the token you
  are typing. **A file row is a result you can open**: click it to select and
  reveal that file in the current tab, or **Ctrl+click** to open it in a new tab
  rooted at its folder. If the file has since moved or been deleted, the app
  says so instead of opening whatever is now in its place.

**Semantic search** understands what is IN the picture (“red dress”, “beach at
sunset”). It needs two CLIP model files dropped into `product/models/`
(see [Reference: Media browser automation](#reference-media-browser-automation)).
The first time you search a folder semantically the app indexes it in the
background (progress shows under the toolbar); afterwards queries are instant.
Without the models, Semantic mode still works using your names, tags, and notes
— the status line says it is in local fallback.

### Favorites and panels

- **Favorites tab** (Ctrl+B or a custom remap): favorites are a Media tab named
  **★ Favorites**, not a side panel, so it sits beside your folder tabs and you
  can keep it open. It has three sub-views:
  **Fav videos**, **Fav images**, and **Color labels** (pick a label to list the
  files carrying it; the count beside each name is how many files use it).
  Opening it again focuses the existing tab rather than making another one.
  It builds its rows from the metadata database, so it never scans the disk and
  appears immediately. Thumbnails, selection, playback, context menus, tags and
  labels all behave exactly as in a folder tab.
  Create, rename, recolor, and delete labels in **Settings → Media → Label
  manager** — that stays the single place labels are edited.
- **Settings window** (header button beside global Refresh, or Ctrl+P):
  one viewport-clamped popup with **Media**, **Playback**, **Controls**, and **App**
  categories. Its outer size stays fixed while categories change; content scrolls inside,
  so the title and Close footer remain reachable. It captures the unobscured app once,
  applies a neutral Gaussian blur, and uses that untinted image as its backdrop.
  Clicking outside or pressing Escape closes it through the existing
  live auto-save path, without an Apply/Save prompt. It replaces the separate Options tab.
  The Controls category uses a centered, width-capped **Action / Keyboard / Controller**
  table rather than the Media panel split. Every empty cell says **Unassigned**; narrow
  windows switch to labeled stacked rows instead of clipping the Controller column.
  Click any binding, press the new key or controller input, done. Conflicting bindings
  move to the new action; one click resets all defaults.
  Choose **Couch fullscreen** in the Settings header for a screen-filling surface with
  larger local type and controls. This temporary mode does not change the normal app font
  preference or normal Settings geometry. The first Escape returns to normal Settings;
  Escape again closes it. If the app was already fullscreen, that prior state is restored.

### Controller

Plug in a gamepad and it just works (a small pad icon shows in the toolbar):
D-pad moves the thumbnail selection, the **left stick scrolls** smoothly, **A** opens
images and plays/pauses the selected video,
**B** goes to the parent folder, **X** toggles selection, **Y** switches
Library + Viewer / full Library, **LB/RB** jump to sibling folders, **LT/RT** zoom
thumbnails, **R3** toggles controller cursor mode, **L3** toggles fullscreen, and
**Select/Back** opens the large Folders window. In cursor mode the right stick moves
the Windows pointer, A left-clicks, and B right-clicks. **Start/Menu** performs Facial's
built-in Alt+Tab, immediately releases simulated pointer buttons, disables cursor mode,
and hands control to the newly focused app. Guide + Start/Menu remains reserved because
Facial suppresses all controller input while Guide is held.
Facial also suppresses controller input and releases simulated buttons whenever its
window loses focus.
During active embedded-video playback, the **right stick** controls transport instead:
left/right seeks 10 seconds and up/down changes volume. Push the **left stick right** to
focus the selected-folder search field. Every control remains remappable.
Settings stays available from the header, Ctrl+P, or an explicit remap. While the Folders window is
open, D-pad/left stick moves its focus, A/Right enters, and B/Left goes to the parent;
thumbnail actions cannot leak through the window. **Open file location** remains
Ctrl+L and can be assigned any controller input in Settings. Every action is remappable.

If the toolbar or Controls category says no controller is detected, run
`facial-cli controller-probe` from a terminal. It uses the same acquisition stack as
the GUI without opening or focusing a window and prints structured JSON. Facial accepts
a Windows joystick (WinMM/DirectInput-compatible) as soon as that direct route exposes
one and initializes gilrs/WGI only for controllers absent from the direct route, so a
broken or broker-dependent WGI startup cannot block an already usable pad. Read
`gamepads` for WGI-route devices and `legacy_fallback` for direct joystick devices;
`gamepad_count: 0` alone does not mean the controller is absent. The snapshot reports
device identity, buttons, and centered axes when acquired, so a no-context model can
separate acquisition, mapping, and focus-gate problems without guessing or injecting input.

### Hiding the interface

**Ctrl+F** (the **Fullscreen** shortcut, or L3) makes Facial a borderless fullscreen app and hides every
surface except the Library panel and full Viewer panel.
The Library / Viewer split remains resizable. **Esc** or Ctrl+F restores the normal
window (a hint shows briefly so you are never stuck). The Viewer's filename,
favorite/rating-like star, color labels, tags, and notes are not rendered in this mode,
leaving the full Viewer-panel surface for the image or video.

### Large-folder diagnostics and recovery

While a scan runs, the Media status reports the growing item count. If a fast
scroll briefly shows placeholders, stop over the desired row: current-viewport
requests take priority and cached thumbnails fill in without blocking input.
The shared media-I/O diagnostics report queue depth/wait, active work class, cache
hits, filesystem latency, scan/query timing, player command/poll timing, and maximum
UI-frame time. Model-facing Media intent receipts include this structured snapshot.
Use Media **Refresh** or **F5** to rescan the selected folder after external file changes.
The header's **Global Refresh** separately reloads models, worktrees, features, the
manual, and retryable thumbnails; it does not replace a folder scan. Models can reproduce the UI
without opening a foreground window with `facial-cli ui-inspect --out DIR`; couch-folder
presets cover normal, long-list, deep-path, empty-folder, and fullscreen states. The
index includes `media_grid`, `media_full`, `media_hidden`, `media_names`,
`media_settings`, and `media_scrollbar` states. For a real-tree scan timing and
exact-path-set check, set `FACIAL_LARGE_MEDIA_TEST_DIR` (and optionally the independent
`FACIAL_EXPECT_MEDIA_COUNT`), then run the ignored Cargo test `large_media_scan_probe`
with `--nocapture`.
For video proof, use a folder containing an MP4/MKV/WebM/MOV/AVI/M4V/WMV/MPEG;
the dedicated thumbnail test generates a real MP4 through FFmpeg, and the LibVLC
loader test checks every required runtime symbol without launching a window.

</topic>

<topic id="project" summary="Project tab — name the job, import images, and list models">

## Project tab

**What it is**
The Project tab is your starting point. It is where you name the batch of photos you
are working on, bring your images into the app, and keep a short list of the people
(models) you are sorting photos for. Think of it as setting up the job before you do
any reviewing.

**When to use it**
Use it at the very beginning, whenever you start working on a new set of pictures. If
you have only ever used the Compare tab to look at photos side by side, the Project
tab is the step that happens *before* that: it is how the pictures get into the app
and how you label the job so everything stays organized.

**How to use it**
The tab is split into three drop-down sections. You can open or close each one by
clicking its title.

1. **Project & worktrees** (the top section):
   - Type a name for this batch into the **Project name** box (for example, the
     model's name or the shoot date). This is just a label so you can tell your jobs
     apart.
   - Leave the **Work in place / source parent** checkbox unticked for now. Unticked
     means the app makes its own copies of your photos and never changes your
     originals (the safe default). The line of text right under the checkbox tells
     you which mode you are in.
   - You usually do **not** need the **New worktree** button. The app makes its own
     work folder automatically when you run things. Only click it if you specifically
     want a fresh, separate internal folder.
   - The **Worktrees** list shows past work folders grouped by project name. You can
     click one to switch back to it, but for everyday use you can ignore this list.

2. **Import images** (the middle section):
   - Click into the big box and type or paste the location of your photos — one per
     line. You can point to a single picture or to a whole folder (the app will look
     through the folder and everything inside it).
   - Click the **Import images** button.
   - A short summary line appears telling you what was brought in. Note there is no
     pop-up "browse" window anywhere in this app — you always type or paste the
     location as text.

3. **Models** (the bottom section):
   - To add a person you are curating photos for, type their name in the **New
     model** box, optionally add a short description underneath, and click **Add
     model**.
   - The list above shows the models you have already added.

**What you get**
After these steps you have a named job with your photos loaded into the app and ready
to be scored, compared, or sorted on the other tabs. Imported photos are copied into
the app's own images folder (unless you turned on "Work in place"), so your originals
are left untouched. The Compare tab and the analysis tabs will now have something to
work with.

**Good to know**
- The app never touches your original files in the normal (copy) mode — it works on
  copies. That is deliberate and safe.
- Two important settings used to live here but now live under **Settings → App**: the
  **Workspace root** (where the app keeps its working files) and the **Copy / output
  folder** (where copies and results are written). The note at the top of the Project
  tab points you there. You must set the copy/output folder before you can run any
  analysis or sorting.
- Unsupported file types (anything that is not a normal image like jpg or png) are
  simply skipped during import — they will not cause an error. Supported types are
  jpg, jpeg, png, webp, bmp, tif, tiff, and gif.

**For automation (LLMs):** This tab maps to these surfaces. UI-intents (need a
running GUI): `set_project --project NAME`, `set_worktree --worktree PATH`,
`set_in_place [--in-place]`, `import_paths --project NAME [--image PATH ...]
[--in-place]`, and `select_tab --tab project`. Backend (headless) equivalents:
`list_worktrees` (project → run dirs) and `start_run --project NAME --image PATH ...
--feature KEY ...`, which ingests and runs in one step without the GUI. There is no
headless command to add a model. Workspace root and copy/output location are set via
**Settings → App** or `set_workspace_root --path DIR` / `set_copy_location --path DIR`.

</topic>

<topic id="quality-iq" summary="Quality & IQ tab — pick automatic photo-grading checks">

## Quality & IQ tab

**What it is**
This is where you pick the automatic "photo grading" checks you want the app to run
on a batch of images. The app can score things like sharpness, exposure, focus on the
eyes, overall composition, and a general quality rating for each picture, so you do
not have to judge every shot by eye.

**When to use it**
Use it when you have a folder of images and you want the app to grade them for you
instead of comparing them one by one. It is the step that lets you later separate the
good keepers from the weak or rejected shots automatically.

**How to use it**
1. Open the **Quality & IQ** tab. You will see a short description line and a
   scrollable list of grading checks, each with a tick box.
2. Tick the checks you want to run. The general-purpose quality score is a good
   default; you can add composition and face/sharpness checks for more detail.
3. You can tick as many as you like. (Tip: if you only see "No features mapped to this
   tab", the plugins have not loaded yet — see the Good to know note below.)
4. That is all you do here. Ticking a box just adds that check to your selection —
   nothing runs yet.
5. Go to the **Run** tab and press **Run selected features** to actually
   grade the images. (Before that, make sure you have imported your images on the
   Project tab and set a copy/output folder under **Settings → App**, or the Run button stays
   disabled.)

**What you get**
Each check you ticked produces a per-image score sheet for the whole batch — for
example a 0–100 quality number and a quality band like "excellent / good / usable /
weak / reject" for every image. After the run, the results appear in the **Run**
tab under "Run summary", and you can then use the **Sort** action there to
automatically split the batch into keep / review / cull folders based on these scores.
So the payoff is: tick a few boxes here, and the app does the tedious grading and
sorting for you.

**Good to know**
- Ticking boxes does nothing on its own — the grading always runs from the **Run**
  tab. Think of this tab as your checklist, and Run as the "go"
  button.
- Your ticks stay selected when you switch tabs, so you can set them up here, then
  walk over to Run.
- If the list says "No features mapped to this tab", click **Refresh plugins** on the
  Run tab to reload the available checks, then come back.
- These quality scores are honest, useful heuristics for triage, not lab-grade
  measurements — treat them as a fast first pass to separate obvious keepers from
  obvious rejects.

**For automation (LLMs):**
- Open this tab as a ui-intent: `facial-cli select_tab --tab quality_iq`.
- Tick/untick checks via `facial-cli set_features --feature facet:quality_pass --feature
  python-ofiq:scalar_quality ...` (Quality & IQ holds all `facet:*` except
  `duplicate_pass`/`burst_blink_pass`/`diagnostics_pass`, plus all `python-ofiq:*` and
  all `ediffiqa:*`); unknown keys are dropped.
- List the exact feature keys with `facial-cli list_features`. Selection is reflected in
  `selected_features` from `facial-cli get_state`.
- These are ui-intents (need a running GUI to apply). To run fully headless instead,
  skip the tab and use the backend command directly: `facial-cli start_run --project NAME
  --image PATH --feature facet:quality_pass ...`.

</topic>

<topic id="identity" summary="Identity tab — switch on face recognition and tick identity checks">

## Identity tab

**What it is**
The Identity tab is where you switch on the app's face-recognition "engine" so it can
tell whether the person in your photos is the same person you care about. Until you
turn it on, the app cannot judge identity at all.

**When to use it**
Use it when you are working with one specific model/person and you want the app to
help you confirm a face is really her (and flag the ones that are not, or that show no
clear face). You set this up once at the start of a working session, then you can lean
on identity checks for the rest of your sorting and comparing.

**How to use it**
1. Open the **Identity** tab.
2. In the **Model path (ArcFace ONNX)** box, point the app to the face-recognition
   model file. This is the brain that recognizes faces. If the box is empty, the
   engine stays off.
3. The **Detector path (YuNet ONNX, optional)** box is optional. The app already has
   a built-in face finder, so you can leave this blank. Only fill it in if someone
   gave you a specific detector file to use instead.
4. Click **Set identity engine**.
5. Read the status line that appears right under the button. If it says **loaded**,
   the engine is on and ready. If it starts with **error:**, the engine did not turn
   on. Read the message, fix the file you pointed to, and click the button again.
6. Below the line "All deepface identity features," tick the identity checks you want
   to use. Each one you tick gets added to the list the app will run later (the actual
   running happens over on the Run tab).

**What you get**
Once the status line says "loaded," the identity engine is active for the whole
session. From then on the app can compare faces and give an honest verdict instead of
guessing. Any identity checks you ticked here are now selected and ready to run. If
the engine is off, the app simply reports that identity is unavailable rather than
faking an answer, so you never get a fake "match."

**Good to know**
- The detector box is genuinely optional. Leave it empty and the built-in face finder
  is used. Filling it in only matters if you were handed a specific file.
- A "loaded" status is what you want before relying on any identity result. If you
  skip this tab, identity checks have nothing to run on.
- The model file is large and only needs to be set once per session. The status line
  is your proof it worked.
- For the full list of files the engine can use (and how to provision them outside the
  GUI), see the reference section "Identity model provisioning."

**For automation (LLMs):**
- `facial-cli identity_status` reports availability and provenance; `facial-cli identity_gate
  --image PATH` and `facial-cli identity_gate_dir --dir DIR` run identity verdicts;
  `facial-cli identity_dedup --dir DIR` groups near-duplicate faces.
- UI-intents: `select_tab` with `tab="identity"` opens this tab in a live GUI;
  `set_features` with `feature_keys` ticks the `deepface:*` identity checks shown
  here.

</topic>

<topic id="duplicates" summary="Duplicates tab — tick checks that find copies, look-alikes, and bursts">

## Duplicates tab

**What it is**
The Duplicates tab is where you tick on the checks that hunt for repeated and
near-repeated photos in your batch: exact copies, look-alikes that are almost
identical, and burst-shot runs where you fired off many frames of the same moment
(including ones where someone blinked). It is the "find the same picture over and
over" toolbox.

**When to use it**
Use it when a folder feels bloated with shots that are basically the same: the same
export saved twice, ten near-identical frames from a burst, or two pictures so close
you would only ever keep one. Turning these checks on tells the app to flag those
clusters so you do not have to eyeball every pair yourself. It pairs naturally with
the Compare tab: Compare lets you eyeball folders, Duplicates lets the app point out
the repeats for you.

**How to use it**
1. Open the **Duplicates** tab. You will see a short line at the top ("All imagededup
   features + facet duplicate_pass / burst_blink_pass.") and a scrolling list of
   checkboxes underneath. Each checkbox is one duplicate-finding check.
2. Tick the checkbox for each check you want to run. You can tick one, several, or all
   of them. Plain-language guide to what each one looks for:
   - **Exact duplicate / hash duplicate checks** — finds files that are the same
     picture (true copies), and groups them so you can see each set of repeats
     together.
   - **Near-duplicate (look-alike) check** — finds pictures that are not identical but
     are so close they are effectively the same shot.
   - **Remove-candidates check** — goes one step further and builds a conservative
     suggested "you could drop this one" list, keeping the best of each look-alike
     pair.
   - **Burst / blink check** — spots runs of rapid-fire frames of the same moment,
     points out which frames have closed eyes, and recommends one frame to keep per
     burst.
3. Ticking a box only **selects** the check; it does not run anything yet. Selecting
   here adds it to the app's shared selection list.
4. Go to the **Run** tab and press **Run selected features** to actually run
   everything you ticked (across Duplicates and any other tab). A copy/output folder
   must be set first, or the Run button stays greyed out.

**What you get**
Nothing on your original photos changes — these checks only look and report. After the
run finishes, the Run tab shows a short summary line for each check (done or
failed), and the findings are written out as results you can open: groups of exact
copies, lists of look-alike pairs, a suggested remove list, and burst groups with a
recommended keeper and blink flags. From there you can use **Sort run into folders**
(on the Run tab) to copy your images into keep / review / cull buckets, which
uses these duplicate and blink findings to decide what lands in "cull."

**Good to know**
- This tab is a checklist, not a Run button. Ticking boxes here does nothing until you
  press Run on the Run tab.
- The remove-candidates and burst checks only ever *suggest* what to drop — they never
  delete anything. You stay in control; deletion is your decision.
- Treat the suggestions as a smart shortlist, not a final verdict. For anything
  borderline, open the Compare tab and look at the pair yourself before culling.

**For automation (LLMs):**
Open this tab with the `select_tab` ui-intent, vocab `duplicates` (CLI: `facial
select_tab --tab duplicates`). The checkboxes correspond to feature keys
`imagededup:hash_duplicates`, `imagededup:cnn_duplicates`,
`imagededup:remove_candidates`, `facet:duplicate_pass`, and `facet:burst_blink_pass`;
select them with `set_features --feature <key> ...` and trigger the run with
`start_run_ui` (or run fully headless with the backend `start_run --feature <key>
...`). Headless layout snapshot: `facial-cli ui-inspect --tab duplicates`.

</topic>

<topic id="run-debug" summary="Run tab — run your checks, watch results, and sort into folders">

## Run tab

**What it is**
This is the tab where you actually press "go." You pick which checks you want done on
your pictures, run them, and then watch the results come in. The behind-the-scenes
debug panels that used to live here have moved to **Settings → App → Advanced / Debug**; this
tab now focuses on running checks, reading the summary, and sorting the batch.

**When to use it**
Use it after you have chosen your photos and decided which checks to run (for example,
quality checks or duplicate-finding from the other tabs). Come here when you are ready
to start the actual run, when you want to see whether a run finished, or when you want
the app to automatically split a finished batch into "keep," "review," and "cull"
piles.

**How to use it**
1. Look at the top line, "Selected features." This tells you how many checks are
   turned on right now and lists them. If it says zero, go turn some on in the other
   tabs first.
2. If you have added new tools and they are not showing up, click **Refresh plugins**
   to reload the list (note: this also clears your current selection, so re-pick your
   checks afterward).
3. Make sure you have set an output folder. If you have not, you will see a message
   telling you to set a "copy output folder" first, and the run button will not work.
   You set this in **Settings → App → Workspace settings**.
4. Click **Run selected features** (the play button) to start. The app works on the
   photos you have imported.
5. Watch **Run summary** fill in. Each line shows a check and whether it passed or
   failed, plus a short message.
6. When a run is finished, you can sort it automatically. Scroll to **Sort run into
   folders**. Leave the "Run id" box blank to sort the most recent run. Click **Sort
   now**.
7. By default, sorting makes copies into "keep," "review," and "cull" folders inside
   your output folder. Your originals are never moved or deleted. If you would rather
   choose your own three folders, tick **Work in parent folder** and type a folder
   path for each pile.

**What you get**
- A live list of results in **Run summary**, showing every check and whether it passed
  or failed.
- The **Run output** box shows where the app saved the results file for this run.
- After sorting, your photos are copied (not moved) into three piles: **keep** (the
  good ones), **review** (borderline, worth a second look), and **cull** (rejects,
  blanks, blinks, or duplicates). A status line tells you how many landed in each
  pile.
- The behind-the-scenes panels — the live **Events** list, **Last applied model
  action**, **Last receipt**, **AppStateSnapshot**, and **Artifact links** — are no
  longer on this tab. They now live under **Settings → App → Advanced / Debug**. You do not
  need them for normal use, but they are there if something goes wrong and you want to
  look closer (see the App settings section).

**Good to know**
- Sorting only copies files. Nothing is ever overwritten or deleted, so it is safe to
  run.
- If "Sort now" complains, it usually means there is no finished run yet, or the run
  id box is empty and there is no recent run to fall back to. Run your checks first.
- This tab never opens any pop-up windows or extra programs. Everything stays inside
  the app.
- How sorting decides: a picture lands in **cull** if it is on the
  remove-candidates list, marked keep:false, a blink frame, or scored "reject"; in
  **review** if it scored "weak"; and in **keep** otherwise.

**For automation (LLMs):**
This tab is exposed for headless and GUI driving. Read live state with `facial
get_state` (returns the full `AppStateSnapshot`, `active_tab` = `run_debug`). Open the
tab via the `select_tab --tab run_debug` ui-intent. Run features headlessly with
`facial-cli start_run` or, against a live GUI, the `start_run_ui` ui-intent (which presses
"Run selected features"). Check progress with `facial-cli get_run_status --run-id ID`,
results with `facial-cli get_run_summary --run-id ID`, and files with `facial-cli
list_artifacts --run-id ID`. Sort a finished run with `facial-cli sort_run --run-id ID
[--in-parent --keep-dir DIR --review-dir DIR --cull-dir DIR]`.

</topic>

<topic id="compare" summary="Compare tab — view folders side by side without changing files">

## Compare tab

**What it is**
The Compare tab is the visual side-by-side light table: open folders next to each
other and flip through their pictures, so you can judge which images or which version
of a batch looks best with your own eyes. It is the operator compare tool and should
not be renamed to Lanes.

**When to use it**
Use it whenever you need to *look* at pictures rather than score them with a tool.
Common situations: picking the best shot out of a folder, comparing two exports of the
same shoot to see which came out better, or eyeballing a batch before you decide what
to keep. The visual compare mode does not change, sort, or delete anything — it is
purely for viewing and judging.

**How to use it**
1. You start with visible compare panes. Use the **2**, **4**, or **8** preset buttons
   in the top bar to expand the workspace quickly, or use **+ Pane** / **- Pane** for
   one-at-a-time changes. You can have up to 16 compare panes.
2. In a pane, click **Browse...** to open the in-app folder picker. Click a folder to
   step into it, use **Up** to go back out, and confirm with **Use this folder**.
   (Nothing pops open outside the app — it all happens inside Facial.) If you already
   know the folder path, you can paste it into the box instead and press Enter.
3. If you want the pictures inside sub-folders included too, tick **Include
   subfolders**. The pane reloads automatically.
4. Flip through the images with the **Prev** / **Next** buttons under the picture, by
   rolling the **mouse wheel** while hovering over the picture, or with the **left/
   right arrow keys** (the arrows move whichever pane your mouse is hovering over).
5. Open files from the list or image area with **Open file** from right-click context
   menus, or press **Ctrl/Cmd+O** for the active pane.
6. To jump straight to a specific image, type its number into the small **go to** box
   at the bottom of the pane and press Enter.
7. (Optional) Give each pane a name in the small name box at the top — handy when
   several panes are open so you remember which batch is which.
8. (Optional) Use **Clone** to add another pane with the last pane's folder and
   recursive setting copied over. It does not copy decoded images or runtime state.
9. (Optional) Turn on **Sync panes** in the top bar to move every pane together at the
   same time — perfect for comparing two versions of the same shoot picture-for-
   picture. Leave it off and each pane moves on its own.

**What you get**
A clean, full-size view of the current picture in each pane, sitting on a white mat
like a print on a contact sheet. Under each picture you see the file name (hover it to
see the full location) and a counter showing where you are, like "12 / 340". Nothing
is saved, moved, or altered — the result is simply that you can see and compare the
images and make your own decision about them.

**Good to know**
- The **Prev/Next** buttons stay greyed out until a folder has finished loading. Very
  large folders load their list first, then show pictures one at a time, so the screen
  may say "loading" for a moment.
- Compare is designed for folders with thousands of images. The UI shows counts,
  current image, and status; it does not try to draw every file path on screen.
- **Anchors** (top bar) pins your identity reference pictures as a small strip above
  the compare panes, so you can keep "what the right person looks like" in view while you
  judge. It is only available when a reference folder has been set up; if the button
  is greyed out, hover it for the hint.
- The previous picture stays on screen while the next one loads, so flipping never
  leaves you staring at a blank panel.
- Right-click on an item, the image, or the list body for Explorer-style list actions:
  **Open file**, **Copy**, **Paste**, **Delete**, and bulk selection actions (**Select all / none / invert**).

**For automation (LLMs):**
Open this tab with the ui-intent `select_tab --tab compare`. A headless layout snapshot
is available with `facial-cli ui-inspect --tab compare`; it produces the canonical
`compare.svg` / `compare.layout.json` snapshot. `--tab lanes` remains accepted as a
temporary compatibility alias, but it still opens/captures Compare.

For model/automation work, use lane commands when one project has several large folders
and you want each folder to become its own explicit unit of work instead of forcing one
serial run over everything. Headless lane state is available through `list_lanes`,
`set_lane`, `scan_lane`, `scan_all_lanes`,
`claim_lane`, `release_lane`, and `lane_status`; these commands write normal JSON
receipts and persist state under `<workspace_root>/.facial/lanes/`. Bounded per-lane
batch processing is available through `start_lane_batch` and
`start_all_lane_batches`.

</topic>

<topic id="options" summary="Settings App category — set output/workspace folders, choose theme and text size, and reach Advanced / Debug">

## App settings (Settings → App)

**What it is**
The **App** category in the unified Settings window is where you tell the app two important
"where" things (where it should keep its working files, and where it should save the
pictures and results it produces), and where you set how the app looks (light or dark,
and how big the text is).

The App category has two sub-tabs across the top: **Preferences** (the everyday
settings described below) and **Advanced / Debug** (the behind-the-scenes panels moved
here from the Run tab). The view opens on **Preferences** by default; non-technical
operators can ignore the Advanced / Debug sub-tab entirely.

**When to use it**
Use it right at the start, before you run anything, to point the app at the folders
you want it to use. Also come back here any time the text feels too small or too big,
or you want to switch between a light and a dark look. Setting the output folder here
is the thing that unlocks the rest of the app, so it is a natural first stop for a new
project.

**How to use it**
1. Click **Settings** beside **Refresh**, then choose **App**. It opens on the
   **Preferences** sub-tab, where all the everyday settings below live.
2. Under **Workspace**, look at the **Copy output folder** box. This is where the app
   saves the pictures it works on and the results it produces. Type or paste a folder
   path into it and click **Set**. If this box is empty, the app shows a reminder that
   it is required, and runs and sorting will not start until you fill it in.
3. (Optional) Under **Workspace root**, you can point the app at a different project's
   working area by typing a folder path and clicking **Set workspace**. Most of the
   time you can leave this as it is. After you set either folder, a short status line
   confirms it worked or explains the problem.
4. Under **Interface**, click **Paper (light)** for a bright look or **Ink (dark)**
   for a dark look. The whole app changes the instant you click.
5. Still under **Interface**, drag the **Font size** slider left for smaller text or
   right for bigger text. The text resizes as you drag so you can see the effect live.
   The available range is small to large (about 12 up to 40).
6. If you ever want the original text size back, click **Reset to default (19 pt)**.
7. At the bottom, read the **Current configuration** lines. These just show you, in
   plain text, the current settings: which settings file is in use, the current font
   size, the in-place default, and which folders are active. Nothing here is a button;
   it is for reference.
8. (Optional, for troubleshooting) Switch to the **Advanced / Debug** sub-tab at the
   top to see the behind-the-scenes panels that used to sit on the Run tab: the live
   **Events** stream, **Last applied model action**, **Last receipt**,
   **AppStateSnapshot**, and **Artifact links**. These are status and diagnostic
   details, useful when something goes wrong or when an LLM/agent is driving the app.
   Non-technical operators can ignore this sub-tab; nothing here changes your pictures
   or settings.

**What you get**
Once the **Copy output folder** is set, the rest of the app comes to life: you can run
features and sort pictures, and everything the app produces lands in that folder. Your
theme and font-size choices take effect immediately and are remembered, so the next
time you open the app it looks exactly the way you left it. The window's own size and
position are also remembered between sessions.

**Good to know**
- The output folder is a gate. If runs or sorting refuse to start, the most common
  reason is that this folder is still empty here. Set it and try again.
- There are no pop-up "browse for folder" windows anywhere in this app. You type or
  paste the folder path into the box yourself, on purpose.
- Theme and font size are looks only. Changing them never changes your pictures, your
  results, or any run; they are purely how the app appears to you.

**For automation (LLMs):**
This tab maps to the `select_tab --tab options` ui-intent (vocab value `options`). The
two workspace fields are backed by headless backend commands: `facial-cli
set_workspace_root --path DIR` and `facial-cli set_copy_location --path DIR`; current
values appear in `facial-cli get_state` (`workspace_root`, plus copy-location state in the
status bar). Theme and font size are display-only with no headless command
(`FACIAL_THEME` and `FACIAL_FONT_SIZE` set them at startup). The whole tab can be
rendered headlessly for inspection with `facial-cli ui-inspect --tab options`.

</topic>

<topic id="ref-headless-cli" summary="Reference — the headless CLI: subcommands, flags, and examples" ingestable="true">

## Reference: Headless CLI

> **This is the reference / automation half of the manual.** The sections from here
> down are the technical surfaces — the CLI, the command/receipt API, schemas, paths,
> recovery, and provisioning. Operators using the GUI do not need them.

`facial.exe` is GUI-only. Terminal and model commands run through `facial-cli.exe`; the
egui GUI is never launched by them. Exit codes: `0` = ok/accepted/applied; `1` =
error/rejected/parse failure.

In an installed build, shortcuts target `facial.exe` directly. The GUI resolves its
writable config and default workspace internally below `%LOCALAPPDATA%\Facial`; it does
not need a `.cmd` launcher or launcher-populated environment. Development and controlled
automation may still override roots with `FACIAL_REPO_ROOT`, `FACIAL_CONFIG_PATH`, and
`FACIAL_WORKSPACE_ROOT`.

Release verification uses `product/scripts/check-exe-layout.ps1`. Besides checking the
two canonical delivery files, it quietly asks the compiled setup to export its embedded
GUI and CLI payloads to a temporary directory, confirms their PE subsystems are GUI=2
and console=3, and applies an exact source allowlist: the two Facial shortcuts and sole
completion action must target `{app}\{#AppExe}` where `AppExe` is exactly `facial.exe`,
and the only other shortcut target may be `{uninstallexe}`. Unquoted, unparseable,
shell-host, script, and extra targets fail validation. This verifier does not install or
launch Facial and removes its bounded temporary extraction directory afterward.

```
facial-cli controller-probe             inspect controller acquisition/state as JSON
facial-cli run-queue [--once | --watch [--poll-ms N]]
                                        drain commands/ (default --once;
                                        --watch loops until <api_root>/stop)
facial-cli command <path>               parse + dispatch a command file, print receipt JSON
facial-cli command --json '<json>'      parse + dispatch an inline JSON command
facial-cli <kind> [--flags...]          convenience builder for a single command
```

Convenience kinds and flags:

```
list_features | list_models | list_worktrees | get_state
start_run --project NAME [--feature plugin:feat ...] [--image PATH ...] [--worktree PATH] [--in-place]
get_run_status --run-id ID | get_run_summary --run-id ID | list_artifacts --run-id ID
read_artifact --path PATH
set_workspace_root --path DIR | set_copy_location --path DIR
sort_run --run-id ID [--in-parent --keep-dir DIR --review-dir DIR --cull-dir DIR]
identity_status | identity_gate --image PATH | identity_gate_dir --dir DIR
identity_dedup --dir DIR [--threshold 0.90]
render_eval --dir DIR
calibrate_threshold
anchor_montage --image PATH
review_init --dir DIR [--shards N] [--gate-manifest PATH] [--clusters PATH]
review_claim --session S [--shard K] [--actor A] [--steal]
review_decide --session S --id ID --decision accept|reject|hold [--reason TEXT] [--actor A]
review_status --session S
review_montage --session S [--shard K] [--page N] [--face-crop] [--filter k=v ...]
review_export --session S --out DIR --name NAME [--repeats N] [--allow-partial]
set_project --project NAME | set_worktree --worktree PATH | select_tab --tab VOCAB
set_features [--feature plugin:feat ...] | set_in_place [--in-place]
import_paths --project NAME [--image PATH ...] [--in-place] | start_run_ui
```

Review queue (WP-016): `review_init` walks a folder (recursive), assigns every image a
stable content ID (sha256; the 16-char short form works everywhere), splits the set
into N shards, and writes a session under `<copy_location>/review/<session_id>/` (or
`<workspace_root>/.facial/review/` when no copy location is set). Optional joins at
init: `--gate-manifest` attaches each image's identity-gate row (verdict, framing,
face box, sharpness, yaw, hair flag) as filterable metadata; `--clusters` attaches
near-dup cluster ids from `identity_dedup`. Parallel agents each `review_claim` a
shard (atomic; `--steal` takes over a dead agent's claim and is ledgered), work
through the returned worklist with `review_decide`, and anyone can read
`review_status` for funnel counts (candidates/accepted/rejected/hold/undecided),
per-shard/per-actor/per-cluster progress, live claims, and surfaced decision
conflicts. All state is an append-only `ledger.jsonl` + derived views — nothing is
tracked in chat.

`review_montage` renders paged contact sheets (6x5 tiles, 256px) with a `.map.json`
keyed by image ID — never positional inference; near-dup clusters tile together.
`--face-crop` crops tiles to the joined gate face box (+30% margin, flagged per tile,
full image fallback). `--filter` terms compose: `decision=undecided`,
`framing=close-up`, `hair_color=pink_purple`, `yaw_estimate=profile`,
`face_crop_sharpness_min=50`, `cluster=c0000`, `shard=1`.

`review_export --format kohya` (the default and only format) verifies every accepted
image's sha256, copies into `<out>/<repeats>_<name>/`, and writes
`dataset_manifest.json` with the full lineage funnel (source -> candidates -> decided
-> exported), per-file hashes, and explicit problems (changed/missing files are
reported, never copied). Undecided images block the export unless `--allow-partial`.

Identity tooling (WP-017/WP-018): `identity_dedup` groups a folder by ArcFace cosine
(greedy clustering, deterministic; emits groups + a recommended keeper, deletes
nothing; 20k-image cap). `render_eval` scores a folder of renders against the anchor
set grouped by config key (immediate subfolder name, else filename stem with the
trailing index stripped) and emits per-group mean/min/max — `no_face`/`error` rows are
counted separately and NEVER enter the statistics. `calibrate_threshold` reports
anchor pairwise self-consistency + negative-set distribution and recommends a gate
threshold (refuses below 4 anchors; report-only, never applied). `anchor_montage`
renders candidate-vs-anchors as one grid with per-anchor cosine similarity in the tile
map.

Common flags: `--action-id ID` (join key; uuid auto-generated when omitted), `--actor
ID` (attribution, e.g. swarm model id). `--feature`/`--features` and
`--image`/`--images` are repeatable.

### Quick start (fully headless, no GUI)

Replace paths with real ones on your machine.

1. Set or verify the runtime workspace root for the project you are operating:
   `facial-cli set_workspace_root --path C:/my-project` then `facial-cli get_state`.
2. List what features exist: `facial-cli list_features`.
3. Ensure a copy/output folder is configured, then start a run end to end (normalizes
   inputs, copies by default, runs the passes, writes `results.json`, prints the
   receipt):
   `facial-cli set_copy_location --path C:/facial-output`
   `facial-cli start_run --project demo --image C:/photos --feature facet:quality_pass --feature deepface:detect`
4. Read the run id from the printed receipt (`result.run_id`), then read the summary:
   `facial-cli get_run_summary --run-id <run_id>`.
5. List and read artifacts:
   `facial-cli list_artifacts --run-id <run_id>` then
   `facial-cli read_artifact --path <one path from the list>`.

### Examples

- `facial-cli list_features` — print every plugin + feature as a receipt.
- `facial-cli get_state` — print and persist the AppStateSnapshot, including active
  `workspace_root`, `api_root`, and `worktrees_root`.
- `facial-cli set_workspace_root --path C:/my-project` — select another project's runtime
  root before queue or pipeline work.
- `facial-cli start_run --project demo --image C:/photos --feature facet:quality_pass --feature deepface:detect`
- `facial-cli get_run_summary --run-id 20260608_120000_ab12cd34`
- `facial-cli list_artifacts --run-id 20260608_120000_ab12cd34`
- `facial-cli read_artifact --path <worktree>/runs/<run_id>/results.json`
- `facial-cli command --json '{"action_id":"","kind":"list_models"}'` (blank action_id
  auto-fills a uuid).
- Drain a producer-fed queue once: `facial run-queue --once` (prints one receipt JSON
  line per processed command).
- Long-running drainer: `facial run-queue --watch --poll-ms 250` (loops until you
  create the file `<api_root>/stop`).

</topic>

<topic id="ref-command-api" summary="Reference — the file-based command + receipt protocol" ingestable="true">

## Reference: File-based command + receipt API

There is no socket and no window interaction. Backend models drive the app by dropping
command files into `<api_root>/commands/` and reading the resulting receipts, or by
calling the headless CLI. The GUI applies ui-intents from `<api_root>/intents/` on its
own frames.

### On-disk directory layout

`<api_root>` defaults to `<workspace_root>/.facial/data/api` (override with
`FACIAL_DATA_ROOT`). `ApiPaths::ensure_dirs()` creates:

```
<api_root>/
  commands/            # producers drop <action_id>.json here (input queue)
  processing/          # a command is atomically renamed here while running
  receipts/            # <action_id>.json terminal receipt written here (output)
  intents/             # ui-intents persisted here, awaiting live GUI apply
  intents/applied/     # applied ui-intent receipts archived here (audit)
  dead/                # unparseable/quarantined commands moved here
  state/state.json     # latest AppStateSnapshot (written by get_state/capture)
  stop                 # sentinel file: create it to stop `run-queue --watch`
```

Queue rules: files mid-write must use a `.tmp` suffix (the queue skips `*.tmp` and any
non-`.json`). A command is claimed by atomic rename into `processing/`, dispatched, its
receipt written to `receipts/<action_id>.json`, then the processing file is removed.
Idempotent: if `receipts/<id>.json` already exists the command is dropped without
reprocessing. On startup, `recover_processing` moves any orphaned
`processing/<id>.json` (no matching receipt) back to `commands/`.

### Command envelope

`protocol_version` is currently `1`. The variant is selected by a flat `kind`
discriminator flattened into the object alongside its fields.

```json
{
  "action_id": "11111111-1111-1111-1111-111111111111",
  "protocol_version": 1,
  "actor": "swarm-model-7",
  "issued_at": "2026-06-08T12:00:00Z",
  "kind": "start_run",
  "project_name": "demo",
  "image_paths": ["C:/photos/a.jpg", "C:/photos"],
  "feature_keys": ["facet:quality_pass", "deepface:detect"],
  "worktree_path": null,
  "in_place": false
}
```

Fields: `action_id` (required join key across command/receipt/intent/events; if blank,
a uuid is generated), `protocol_version` (defaults to 1), `actor` (optional
attribution), `issued_at` (optional rfc3339), and the flattened `kind` + variant
fields.

### Receipt schema (always written, never panics)

```json
{
  "action_id": "11111111-1111-1111-1111-111111111111",
  "kind": "start_run",
  "status": "ok",
  "actor": "swarm-model-7",
  "protocol_version": 1,
  "started_at": "2026-06-08T12:00:00.100Z",
  "finished_at": "2026-06-08T12:00:01.250Z",
  "result": { "run_id": "...", "status": "completed", "...": "..." },
  "error": null,
  "note": null
}
```

`status` is one of: `ok` (backend command completed), `error` (backend command
failed), `accepted` (ui-intent validated + persisted, awaiting GUI apply), `applied`
(ui-intent applied by a live GUI frame), `rejected` (refused: bad vocab, path escape,
run already active, unparseable). `result` is omitted when null; `error` and `note`
are omitted when absent. Every receipt is also mirrored to `events.jsonl` (source
attribution `api`); `ok`/`accepted`/`applied` map to `applied=true`.

### Backend-executable commands (terminal receipt, run fully headless)

- `list_features` — `result` = array of plugin manifests (each with nested
  `features`). Status `ok`.
- `list_models` — `result` = array of model records. Status `ok`.
- `list_worktrees` — `result` = object `{ "<project>": ["<run dir>", ...] }`. Status
  `ok`.
- `get_state` — `result` = full `AppStateSnapshot` (also persisted to
  `state/state.json`). Status `ok`.
- `set_workspace_root` — field `path`. Creates/persists the runtime root used for
  `.facial/data`, `.facial/worktrees`, API queues, receipts, and debug events.
- `start_run` — fields `project_name` (req), `image_paths`, `feature_keys` (req,
  non-empty or `error`), `worktree_path` (optional; created if null/blank/"no worktree
  yet"), `in_place`. `result` = `RunSummary`. Status `ok` on success, `error` on
  failure (e.g. "no features selected", "No images available").
- `get_run_status` — field `run_id`. `result` = `{ "status":
  "completed"|"unknown", "found": bool }`. Status `ok`.
- `get_run_summary` — field `run_id`. `result` = the parsed `results.json`. Status
  `ok`, or `error` if the run is not found / unreadable.
- `list_artifacts` — field `run_id`. `result` = sorted array of every file path under
  the run dir. Status `ok`, `error` if not found, `rejected` if the run dir escapes
  the allowed artifact roots (`worktrees_root`, `api_root`, copy output root, or
  current in-place run roots).
- `read_artifact` — field `path`. Path is canonicalized and must live under an allowed
  artifact root or it is `rejected`. `result` = parsed JSON if parseable, else the raw
  string. Status `ok`/`error`/`rejected`.
- `list_lanes` — `result` = array of lane records. Creates the default lane registry
  when `<workspace_root>/.facial/lanes/lanes.json` does not exist.
- `set_lane` — fields `lane_id`, `name`, `mode` (`compare|review|batch`), `folder`,
  `recursive` (default `true`), `steal`, and `feature_keys`. Sets lane metadata
  without scanning. If the lane is claimed, the command's top-level `actor` must own
  the claim unless `steal=true`.
- `scan_lane` — fields `lane_id` and `steal`. Inventories supported image paths for
  that lane, records sorted paths/count, and returns `{ lane_id, item_count, files,
  dir_errors, last_error }`. If the lane is claimed, the command's top-level `actor`
  must own the claim unless `steal=true`.
- `scan_all_lanes` — optional field `steal`; scans every configured lane and returns
  one result per lane, including failed lanes with `last_error`. Owned claimed lanes
  are scanned when the command's top-level `actor` matches the lane claim; other
  owners remain lane-local errors unless `steal=true`.
- `claim_lane` / `release_lane` — fields `lane_id` and `steal`; ownership uses the
  command's top-level `actor` field. The CLI convenience form also accepts `--actor`.
  Claims are actor-attributed and persisted under
  `<workspace_root>/.facial/lanes/claims/`.
- `lane_status` — optional field `lane_id`; returns one lane or all lanes with claim,
  count, file list, last-error state, and latest batch metadata:
  `batch_status`, `batch_action_id`, `batch_updated_at`, `last_run_id`, and
  `last_batch_error`.
- `start_lane_batch` — fields `lane_id`, optional `project_name`, optional
  `feature_keys`, `in_place`, and `steal`. Runs the lane's scanned inventory through
  the existing pipeline; if `feature_keys` is empty, the lane's stored feature keys
  are used. The lane's `batch_action_id` is the command `action_id`, so
  `lane_status` joins back to the normal command receipt.
- `start_all_lane_batches` — fields `project_name`, optional `feature_keys`,
  `concurrency_limit` (default `2`), `in_place`, and `steal`. Runs every `batch` mode
  lane and returns an aggregate result with per-lane status, run id, count, output
  path, and error. Lane failures are isolated in the aggregate receipt, and each
  aggregate child `action_id` is also written as a normal `start_lane_batch` receipt
  under `receipts/<action_id>.json`.

### UI-intent commands (persisted to intents/; applied by a live GUI)

These return `accepted` from the backend (persisted to `intents/<id>.json`), then
`applied` or `rejected` when a live GUI frame consumes them. They require a running GUI
to take effect.

- `set_project` — field `project_name`. Sets the GUI project name.
- `set_worktree` — field `worktree_path`. Selects an existing worktree.
- `select_tab` — field `tab`, vocab one of `media | project | quality_iq |
  identity | duplicates | run_debug | manual | lanes | options`; `compare` is
  accepted as a backward-compatible alias. Unknown vocab is `rejected` at
  validation time.
- `set_features` — field `feature_keys`. Unknown keys are dropped (noted).
- `set_in_place` — field `in_place` (bool).
- `import_paths` — fields `project_name`, `paths`, `in_place`. Ingests into the live
  GUI worktree (copy or in-place).
- `start_run_ui` — asks the live GUI to press "Run selected features". `rejected` if a
  run is already active or no features are selected.
- `ui_snapshot` — captures the exact live GUI to a PNG without activating, focusing,
  raising, or clicking the window. Use `--out FILE.png` or accept the unique default
  below `.facial/ui-snapshots/live-ui/`.

### Driving the frontend through intents

A model controls the live GUI without touching the screen, mouse, or keyboard:

1. Issue a ui-intent — either `facial <intent> ...`, `facial command --json '...'`, or
   drop `<api_root>/commands/<id>.json` and run `facial run-queue --once`. The backend
   validates it and writes it to `intents/<id>.json`, returning an `accepted` receipt.
2. The running GUI polls `intents/` every ~250 ms and applies at most ONE intent per
   frame (FIFO by modified time). It then writes a follow-up receipt (`applied` or
   `rejected`) to `receipts/<id>.json`, archives the intent to
   `intents/applied/<id>.json`, and records a model-action event.
3. Observe the result: read `receipts/<id>.json`, re-read `get_state` (or the
   AppStateSnapshot panel under Settings → App → Advanced / Debug), and read the event stream.
   The Settings → App → Advanced / Debug sub-tab also shows "Last applied model action" and
   "Last receipt".
4. When visual proof is needed, issue `ui_snapshot --out FILE.png` and inspect the
   applied receipt's `capture_path`. If a video is active, the app captures its decoded
   frame and composites it into the live GUI framebuffer at the diagnosed native-surface
   bounds. The `-video.png` sidecar is either the LibVLC snapshot or, for vouts that
   reject that call, the exact visible framebuffer crop at those bounds.

Typical drive sequence to run features through the GUI:

1. `select_tab --tab quality_iq` (or whichever tab holds the features).
2. `select_tab --tab compare` for manual review and loading independent folders in
   compare panes (`lanes` remains accepted as a temporary alias only).
3. `set_project --project demo`.
4. `import_paths --project demo --image C:/photos` (copy) or add `--in-place`.
5. `set_features --feature facet:quality_pass --feature python-ofiq:scalar_quality`.
6. `start_run_ui` — the GUI presses "Run selected features".
7. Poll `get_state` until `running_pipeline` is false, then read `get_run_summary` /
   `list_artifacts`.

All ui-intents require a running GUI to reach `applied`; until then they sit in
`intents/` as `accepted`. For fully headless runs, prefer the backend `start_run`
command instead of the `start_run_ui` intent.

</topic>

<topic id="ref-appstatesnapshot" summary="Reference — the AppStateSnapshot schema returned by get_state" ingestable="true">

## Reference: AppStateSnapshot schema

The `get_state` command (CLI or file-based API) returns the full live state object and
also persists it to `<api_root>/state/state.json`.

```json
{
  "protocol_version": 1,
  "captured_at": "2026-06-08T12:00:00Z",
  "repo_root": "D:/Projects/LLM projects/facial",
  "workspace_root": "D:/Projects/other-project",
  "worktrees_root": "D:/Projects/other-project/.facial/worktrees",
  "api_root": "D:/Projects/other-project/.facial/data/api",
  "ingest_in_place_default": false,
  "models": [ /* ModelRecord */ ],
  "plugins": [ /* PluginManifest with nested features */ ],
  "worktrees": { "<project>": ["<run dir>", "..."] },
  "lanes": [ /* LaneRecord */ ],
  "active_tab": "manual",
  "project_name": "default-project",
  "worktree_path": "no worktree yet",
  "in_place": false,
  "selected_features": ["facet:quality_pass"],
  "running_pipeline": false,
  "run_output": "no run yet"
}
```

In a headless `get_state` the live-GUI fields (`active_tab`, `project_name`,
`worktree_path`, `selected_features`, `running_pipeline`, `run_output`) are defaults;
only a running GUI populates them with real session state.

### Feature keys and GUI tab grouping

Feature keys use the format `plugin_id:feature_id`. The five bundled source families
are `facet`, `python-ofiq`, `deepface`, `imagededup`, and `ediffiqa` (spec name
`eDifFIQA`). Run `facial-cli list_features` for the authoritative list and each feature's
output contract.

The GUI groups feature checkboxes by tab: `deepface:*` → Identity; `imagededup:*` and
`facet:duplicate_pass`/`facet:burst_blink_pass` → Duplicates; `facet:diagnostics_pass`
→ Run; all other `facet:*`, all `python-ofiq:*`, all `ediffiqa:*` → Quality &
IQ. Any unknown prefix falls back to Run so nothing is hidden.

</topic>

<topic id="ref-outputs" summary="Reference — where artifacts, summaries, state, and events live on disk">

## Reference: Output & artifact paths

- Worktree: `worktrees/<slug>/<timestamp_id>/` (internal workspace surface).
- Imported images in copy mode: `<copy/output folder>/images/`.
- In-place images: original source paths, unchanged.
- Copy-mode run root: `<copy/output folder>/runs/<run_id>/`.
- In-place run root: `<source parent>/.facial/runs/<run_id>/`.
- Per-feature artifact: `<run root>/<plugin_id>/<feature_id>/<feature_id>.json`.
- Per-run summary: `<run root>/results.json` (this path is the
  `output_path`/`run_output` shown in the GUI and returned by `start_run`). A per-run
  `results.json` aggregates every plugin result; its `totals` count ok/skipped/failed
  and run `status` is `completed` when nothing failed, else `partial`.
- Latest captured state: `<api_root>/state/state.json`.
- Lane registry: `<workspace_root>/.facial/lanes/lanes.json`.
- Lane claims: `<workspace_root>/.facial/lanes/claims/<lane_id>.json`.
- Receipts: `<api_root>/receipts/<action_id>.json`.
- Event log (append-only): `<workspace_root>/.facial/data/events.jsonl`.

`repo_root` is the app install root. `workspace_root` is the selectable runtime root
for the external project being processed. `worktrees_root` defaults to
`<workspace_root>/.facial/worktrees`; `<api_root>` and the data root default under
`<workspace_root>/.facial/data`. The copy/output folder is set by the GUI, `facial
set_copy_location --path DIR`, config `copy_location`, or `FACIAL_COPY_LOCATION`.
Internal roots are relocatable via `FACIAL_WORKSPACE_ROOT`, `FACIAL_DATA_ROOT`,
`FACIAL_WORKTREES_ROOT`, and `FACIAL_REPO_ROOT`.

### Copy vs in-place

- **Copy (default, non-destructive):** each source file is copied into `<copy/output
  folder>/images/`. Source files are never mutated. Name collisions get a uuid suffix.
  Runs write to `<copy/output folder>/runs/<run_id>/`.
- **In-place:** opt-in. The app keeps the original source paths and does not create
  symlink/hardlink surrogates. Runs write to `<source parent>/.facial/runs/<run_id>/`.

Toggle copy vs in-place: GUI "Work in place" checkbox (Project tab); headless
`--in-place` on `import_paths` / `start_run`, or the `facial-cli set_in_place [--in-place]`
ui-intent. Default is controlled by `ingest_in_place_default` (config `default.json` /
env `FACIAL_INGEST_IN_PLACE`); ships `false` (copy).

### Copy-location gate and sort-into-folders

No run, sort, or task may start until a **copy/output location** is set. Set it in the
GUI **Settings → App**, via `facial-cli set_copy_location --path DIR`, or via config
`copy_location` / `FACIAL_COPY_LOCATION`. Until set, the Run button is disabled and
both `run_pipeline` and `sort_run` refuse with "Set a copy/output location before
starting any task".

`sort_run` deterministically sorts a completed run's images into **keep / review /
cull** from the run's on-disk verdicts (copy-only, non-destructive):

- **cull**: in `imagededup:remove_candidates` remove list, `keep: false`, a blink
  frame, or `quality_band: reject`.
- **review**: `quality_band: weak` (and not already cull).
- **keep**: everything else in the run's per-image universe.

Default mode copies into `<copy location>/keep | review | cull`. With work-in-parent
on, you supply an explicit folder per bucket. Headless: `facial-cli sort_run --run-id ID
[--in-parent --keep-dir DIR --review-dir DIR --cull-dir DIR]`. Result JSON: `run_id,
mode, total, keep, review, cull, keep_dir, review_dir, cull_dir, errors`.

</topic>

<topic id="ref-errors-events" summary="Reference — where errors, events, and quarantined commands surface">

## Reference: Where errors & events appear

- **Event stream**: the Settings → App → Advanced / Debug "Events" panel, mirrored to
  `<workspace_root>/.facial/data/events.jsonl` (`[ts] LEVEL source - message`). Levels
  include INFO, WARN, ERROR. Sources include Service, Pipeline, Ingest, ModelRegistry,
  plugin_host, and `api` (command receipts).
- **Run summary**: failed features appear immediately as `plugin::feature -> failed
  (message)` lines in the Run "Run summary" panel and inside `results.json`
  (`totals` counts ok/skipped/failed; run `status` is `completed` when nothing failed,
  else `partial`).
- **Receipts**: backend command failures are `error`; refusals are `rejected`, each
  carrying an `error` and/or `note` string explaining why.
- **Quarantine**: unparseable commands are moved to `<api_root>/dead/<id>.json` with a
  paired `rejected` receipt (kind `unparseable`).

</topic>

<topic id="ref-recovery-rerun" summary="Reference — failure recovery and rerun behavior">

## Reference: Failure recovery & rerun

- **Orphaned in-flight command**: a file left in `processing/` with no matching receipt
  is automatically moved back to `commands/` at startup (`recover_processing`), so it
  is retried on the next queue drain.
- **Quarantined command**: inspect `<api_root>/dead/<id>.json`, fix the JSON, and
  re-drop it into `commands/` with a fresh `action_id` (the same id is treated as
  already-processed and dropped).
- **Invalid feature key** (`plugin:feature` malformed or unknown): the pipeline emits a
  failure event, records the reason in the run summary, and continues the remaining
  features where possible.
- **No valid images**: the run is refused with `No images available`; correct the
  paths/extensions and rerun.
- **Pipeline failure**: the current run stops, partial outputs are preserved under the
  run dir, and an explicit rerun is required after correction.
- **Run already active**: a new `start_run_ui` intent is `rejected`; wait for
  `running_pipeline` to clear (poll `get_state`) then retry.
- **Missing plugin manifest**: the result surfaces a missing-plugin failure that
  includes the `plugins_root` and the list of loaded plugin ids for diagnosis.
- **Reruns are idempotent at the queue level**: reusing an `action_id` that already has
  a receipt is dropped without reprocessing; use a new id to force a rerun.

</topic>

<topic id="ref-no-window-safety" summary="Reference — the no-window safety rule the whole app enforces">

## Reference: No-window safety rule

No plugin, pipeline, GUI control, or debug action may launch an external OS window or
grab focus. There are NO file pickers (paths are typed/pasted as text), NO
Explorer/Finder/Browser launches from app controls, and NO UI-spawning subprocesses.
All model and backend navigation is file-based (commands/receipts/intents) or via the
headless CLI. Every execution and model action emits an event to
`<workspace_root>/.facial/data/events.jsonl` so activity is observable without any
foreground interruption.

Models must also never activate, raise, focus, foreground, or temporarily make Facial
always-on-top for navigation or inspection. Start automated live instances with
`facial.exe --background`, navigate them only with receipt-backed intents, use
`ui-inspect` for deterministic fixtures, and use `ui_snapshot` for the exact live UI.
If either navigation or visual proof is unavailable through those routes, treat that as
missing product tooling and add the required intent/capture/diagnostic/Manual coverage;
do not compensate by taking over the operator's desktop.

The app is one self-contained Rust binary with no external API serving layer (no HTTP,
no sockets). The feature key format is always `plugin_id:feature_id`, and the default
run path contract is `<copy/output folder>/runs/<run_id>/<plugin>/<feature>/` in copy
mode or `<source parent>/.facial/runs/<run_id>/<plugin>/<feature>/` in in-place mode.

</topic>

<topic id="ref-identity-provisioning" summary="Reference — provisioning the YuNet + ArcFace identity engine and its outputs" ingestable="true">

## Reference: Identity model provisioning

Optional, pure-Rust ONNX face identity (Phase 2). DISABLED unless an embedder model is
provisioned; when disabled the app reports `identity: unavailable` and never fakes a
verdict. Provision via:

- `product/config/default.json` keys:
  - `identity_model_path`
  - `identity_detector_path`
  - optional `identity_reference_dir` / `identity_negative_dir`
  - optional `identity_threshold` / `identity_margin`
- environment variables at launch:
  - `FACIAL_IDENTITY_MODEL`
  - `FACIAL_IDENTITY_DETECTOR`
  - `FACIAL_IDENTITY_REF_DIR`
  - `FACIAL_IDENTITY_NEG_DIR`
  - `FACIAL_IDENTITY_THRESHOLD`
  - `FACIAL_IDENTITY_MARGIN`

Detector (WP-020): **YuNet ships built into the binary** (OpenCV Zoo 2023mar float
model, MIT — license at `product/assets/models/YuNet-LICENSE.txt`), so face detection
needs zero provisioning. Resolution order: `identity_detector_path` /
`FACIAL_IDENTITY_DETECTOR` override → bundled YuNet → none (resize alignment). A
configured path that fails to load falls back to the bundled model with origin
`bundled_fallback`. Every load runs a startup self-check (blank-frame inference must
expose the 12-output 2023mar layout) so a wrong export can never silently produce wrong
geometry. `identity_status` reports `detector_origin`
(`override|bundled|bundled_fallback|none`) and `detector_sha256`, and the model
registry carries a `yunet-detector` record with the same provenance. Only the ArcFace
**embedder** (`identity_model_path` / `FACIAL_IDENTITY_MODEL`, ~166 MB) still needs
provisioning — without it the whole identity engine stays disabled.

Example (PowerShell):

```powershell
# Configure both identity dependencies for a no-context run.
$env:FACIAL_IDENTITY_MODEL = "D:/Projects/LLM projects/facial/product/models/w600k_r50.onnx"
$env:FACIAL_IDENTITY_DETECTOR = "D:/Projects/LLM projects/facial/product/models/yunet_2023mar.onnx"
facial-cli identity_status
```

Equivalent config-file mode in `product/config/default.json`:

```json
{
  "identity_model_path": "D:/Projects/LLM projects/facial/product/models/w600k_r50.onnx",
  "identity_detector_path": "D:/Projects/LLM projects/facial/product/models/yunet_2023mar.onnx"
}
```

Alignment: provision a **YuNet** detector ONNX via `identity_detector_path` or
`FACIAL_IDENTITY_DETECTOR` (`face_detection_yunet_2023mar.onnx`, pure-Rust
tract-compatible). When present, faces are detected and aligned via a 5-point
similarity transform to the canonical ArcFace template (`align="yunet_112"`); with no
detector (or if the detector misses a face), it falls back to a whole-image resize
(`align="resize_112"`). The per-image `align` field reports which path was used.

Method (deterministic): detect+align (or resize) → 112x112 → embed via `tract` →
L2-normalize → cosine vs the reference and negative sets. Verdict = `match` / `no_match`
/ `unsure` / `no_reference`, plus `no_face` (no face detected, align fell back to
resize) and `error` (image failed to decode/infer). Similarities, margin, and the model
sha256 are stamped into the result for audit. YuNet alignment materially sharpens
separation (validated: same-person cosine ~0.70+, different ~0.0, vs a fuzzy 0.24-0.56
spread under resize). The proxy `deepface:*` features are unchanged and remain labelled
as proxies.

Face geometry (same YuNet pass, no external `cv2`): every gate row also carries
`face_count` (faces at/above `identity_count_threshold`, default 0.9, after IoU NMS 0.3
— use it to reject collages / group shots), `face_box` (`{x,y,w,h}` original px of the
strongest face), `face_frac` (face area / image area — scale-bucket hint), `face_score`,
and `framing` (`close-up` | `three-quarter` | `full-body` | `none`, derived from
`face_frac` via `framing_closeup_min`/`framing_threequarter_min`). This lets a model do
scale-bucketing, framing-bucketing, and collage-rejection in one tool.

Trust: every face/quality output carries `source` = `real` (the YuNet+ArcFace engine:
the identity gate and deepface represent/verify/find when a model is provisioned) or
`proxy` (heuristic plugins: deepface detect/analyze, facet, ofiq, ediffiqa, imagededup).
Trust-weight `proxy` outputs accordingly — they look authoritative but are heuristics.

Curation metadata (WP-019, every gate row + CSV): `face_crop_sharpness` (laplacian
variance over the face box — face-region focus, not whole-frame), `yaw_estimate`
(`frontal|quarter|profile` bucket from the 5-point landmark geometry, with `yaw_ratio`;
buckets only — 5 points cannot give degrees), and `hair_color` (+`hair_confidence`,
`hair_source: "proxy"` — an HSV heuristic over the strip above the face box; a triage
hint for wig/dye outliers, never a gate). These columns join into review sessions via
`review_init --gate-manifest` and drive `--filter` terms.

Eyes-open (WP-021, wave 2 — `source: real`): provision the PIPNet 98-pt landmark model
(`pipnet_r18_wflw_98.onnx`, ~47 MB, MIT — license beside it in `product/models/`) via
`landmark_model_path` in the config, `FACIAL_LANDMARK_MODEL`, or simply by placing it
at `product/models/pipnet_r18_wflw_98.onnx` (auto-detected). It lazy-loads on the first
gate call. Every gate row then adds `eyes_open` (`open|closed`), raw
`ear_left`/`ear_right` (simplified WFLW EAR: mid-lid vertical / eye width; open eyes
measure ~0.32-0.42, the bucket threshold `ear_open_min` 0.15 and `ear_method` are
stamped per row so you can re-bucket downstream), and `landmark_conf_min`. Filter
reviews with e.g. `--filter eyes_open=closed`. Without the model the fields are null and
everything else works unchanged.

Occlusion: deliberately NOT emitted. The landmark-confidence occlusion proxy failed its
validation gate (painted eye/mouth occlusions produced no confidence separation, while
EAR separated cleanly), so per the WP-021 contract the flag is withheld rather than
shipped as a misleading signal. Honest occlusion detection needs a face-parsing
segmentation model — a future packet if field feedback demands it.

Commands:
- `facial-cli identity_status` — availability + provenance (incl. `detector_origin`).
- `facial-cli identity_gate --image PATH` — one image, returns the row JSON.
- `facial-cli identity_gate_dir --dir DIR` — gate every top-level image in `DIR` in one
  call. Writes `runs/<run_id>/identity_gate.csv` + `manifest.json` (schema_version 2)
  under the copy-root (else `<DIR>/.facial/runs/<run_id>/`); the receipt returns
  `run_id`, the artifact paths, and a per-verdict `summary`. Per-image errors are
  isolated to their row (the batch never aborts); output is deterministic (sorted
  inputs, stable NMS tiebreak). Tune `identity_count_threshold` /
  `FACIAL_IDENTITY_COUNT_THRESHOLD` to change the face-count cutoff.
- `facial-cli identity_dedup --dir DIR [--threshold 0.90]` — ArcFace-cosine near-dup groups
  + recommended keeper per group (see the command API reference).
- `facial render_eval --dir DIR` / `facial calibrate_threshold` / `facial
  anchor_montage --image PATH` — train→eval loop tools (command API reference).

</topic>

<topic id="ref-media-automation" summary="Reference — media browser storage, CLI commands, intents, and CLIP provisioning" ingestable="true">

## Reference: Media browser automation

Everything the Media tab does is drivable by a no-context model.

### Storage (workspace-relative, survives relocation)

```text
<workspace_root>/.facial/media/media.redb        # irreplaceable notes/tags/labels/favorites/settings (redb, single writer)
<workspace_root>/.facial/media/thumbs/<xx>/<sha256>.jpg   # thumbnail disk cache (256/512-edge JPEGs)
<workspace_root>/.facial/media/clip_index.redb   # CLIP embedding cache (regenerable, safe to delete)
```

Keys are casefolded, slash-normalized, and workspace-relative when the file
lives under the workspace root. `media.redb` takes an EXCLUSIVE lock: while the
GUI runs, headless `media_meta_*` commands return errored receipts (never
ok-empty) — drive a live GUI through ui-intents instead, or run headless with
the GUI closed. `clip_index.redb` is separate on purpose so indexing never
contends with metadata.

### Headless commands (terminal receipts)

```text
facial-cli media_meta_get  --path PATH
facial-cli media_meta_set  --path PATH [--notes TEXT] [--tags "a,b"] [--label ID_OR_NAME]  # legacy exclusive-label setter
facial-cli media_meta_list [--tag TAG] [--label LABEL]        # all rows + tag vocabulary
facial-cli media_labels_list                                  # stable IDs + names + backend hex
facial-cli media_label_create --name NAME --hex "#12ABEF" [--path PATH]
facial-cli media_label_update --label ID [--name NAME] [--hex "#12ABEF"]
facial-cli media_label_delete --label ID --confirm
facial-cli media_label_assign --path PATH --action add|remove|clear [--label ID_OR_NAME]
facial-cli media_fav_add   --path PATH | media_fav_remove --path PATH | media_fav_list
facial-cli thumbs_gc [--cap-mb N]                             # sweep the thumbnail cache
facial-cli media_index_build --dir DIR [--recursive]          # embed images into the CLIP index
facial-cli media_semantic_search --query "red dress" --dir DIR [--concurrency-limit N]
```

`media_semantic_search` ranks by cosine over CACHED embeddings and reports how
many files were skipped as unindexed — run `media_index_build` first.

**Headless without CLIP models:** `media_index_build` and
`media_semantic_search` return `status: "error"` (exit 1) with the
"semantic search: local fallback (missing …)" message and produce NO results —
the local name/tags/notes fallback ranking is a GUI behavior only. Provision
the models for headless semantic work.

Receipt details: `media_meta_set` echoes tags normalized (trimmed, lowercased,
deduped, sorted); `media_meta_get` / `media_meta_list` / `media_fav_list`
include a `db_status` field (null when healthy, otherwise the degradation
reason). Metadata receipts expose `labels` as an array and retain singular `label`
as a first-item compatibility alias. Catalog delete refuses an in-use label without
`--confirm` and reports both usage and removed-assignment counts.

### UI-intents (applied by a live GUI)

```text
facial-cli media_set_folder --dir DIR         # point the active tab at a folder and scan
facial-cli media_tabs --action list|labels|select|open|close|open_collection|set_scope|set_sort [--tab-id ID] [--path VALUE]
#   list            no flags. Reports every tab and the active tab's grid state.
#   labels          no flags. Colour-label catalog.
#   select --tab-id ID          make that tab active.
#   close  --tab-id ID          close that tab (the last tab resets instead).
#   open   --path DIR           open DIR in a NEW tab and select it. This is the
#                               model equivalent of the folder browser's
#                               "Open in new tab"; media_folder_navigate
#                               --action open_new_tab uses the browser's staged
#                               folder and takes no path of its own.
#   open_collection --path VIEW
#   set_scope --path folder|tab
#   set_sort  --path KEY[:asc|:desc]
#   Passing a flag an action does not use is refused, not ignored.
#
#   labels lists the colour-label catalog. The receipt carries it BOTH as a
#     readable note and as a structured `label_catalog` array (id, name, hex,
#     usage) — use the array; never parse the note. Use this rather than the
#     backend media_labels_list while the GUI is running, since the GUI holds
#     the media database open.
#   open_collection --path fav_videos|fav_images|labels[:LABEL_ID] opens (or
#     focuses) the ★ Favorites tab without any filesystem scan. To show one
#     label's files, pass its stable ID (not its name) after a colon — get the
#     ID from `media_tabs --action labels`.
#   set_scope --path folder|tab sets this tab's search scope. Its receipt reports
#     last_scope_change.scan_unchanged and .inventory_unchanged as structured
#     fields, so you can prove scope never rescans without parsing text.
#   set_sort --path name|modified|size|created[:asc|:desc] sets this tab's order.
#     The ★ Favorites tab accepts only `name`; the stat-based keys are refused
#     there because a collection carries no file metadata.
#   media_set_folder is refused on the ★ Favorites tab, which has no folder.
#   list receipts report, PER TAB: kind, collection view, collection label id,
#   search scope, sort key and direction. They also report, ONCE at the top level
#   for the ACTIVE tab: display_count and display_provenance
#   (empty | provisional | settled). Provenance tells you whether the grid is
#   showing a renderable provisional order or the final one.
facial-cli media_search --query Q [--mode name|fuzzy|semantic|tags|notes]
#   The receipt reports matched_count/excluded_count ONLY when counts_settled is
#   true. Ranking is asynchronous, so immediately after a query change the counts
#   are null and counts_settled is false — poll `media_tabs --action list` and
#   read display_count once display_provenance is "settled". query_diagnostics
#   and search_status are withheld on the same condition, so no field in the
#   receipt ever describes a different query than the one you sent.
facial-cli media_select --file PATH [--file PATH ...]
facial-cli media_open_selected
facial-cli media_folder_navigate --action open|close|toggle|up|down|page_up|page_down|home|end|enter|parent|refresh|commit|open_new_tab
# Actions are accepted while the navigator's blurred backdrop is still being
# captured; they settle that capture instead of being rejected (WP-064).
facial-cli media_video_control --action status|play_pause|play|play_library|pause|stop|seek_ms|volume|audio_track|subtitle_track|loop|capture_frame [--value N] [--out FILE.png]
facial-cli media_label_mutation --action create|update|delete|add|remove|clear [--path PATH] [--label ID_OR_NAME] [--name NAME] [--hex "#12ABEF"] [--confirm]
#   --label takes a name OR an id for add|remove, but update|delete require the
#   stable ID. Get IDs from `media_tabs --action labels`.
facial-cli select_tab --tab media
facial-cli ui_snapshot [--out FILE.png]
```

`--mode tags` / `--mode notes` rewrite the query into `tag:<q>` / `note:<q>`
filter chips. Accepted intents echo their payload in the receipt.
`media_video_control` targets the selected video. `status` returns structured live
time/length/volume and audio/subtitle track IDs in the applied receipt. `seek_ms` uses milliseconds,
`volume` uses 0–125 percent, and track actions use the IDs exposed by LibVLC.
`play_library` moves the one shared player into the selected Library thumbnail; `play`
targets the Viewer panel. Both paths remain receipt-backed and never create another decoder.
Facial warms LibVLC's plugin/instance cache on a bounded background startup worker while
the normal service and model initialization runs; the first explicit Play action never
performs that one-time plugin warm-up on the UI frame.
`loop --value 1` enables the default repeat behavior; `loop --value 0` disables it.
`media_label_mutation` is the live-GUI label path; it uses the already-open database and
returns the applied catalog/assignment state instead of failing on the GUI's exclusive
redb lock.

`capture_frame` asks the embedded player itself to export the currently decoded frame.
Its applied receipt includes `capture_path`, `capture_exists`, and the same live player
state. Relative `--out` paths resolve from the configured workspace root; omitting
`--out` writes a unique PNG below `.facial/ui-snapshots/live-video/`. Use this together
with `ui-inspect --tab media`: the SVG/layout artifacts prove deterministic egui chrome
and the LibVLC PNG proves decoded pixels. For exact live composition, launch with
`facial.exe --background`, navigate using `media_*` intents, then run `ui_snapshot`.
Its receipt reports `foreground_activation: false`, the output dimensions, native
surface diagnostics, `video_capture_source` (`libvlc` or `live_framebuffer_crop`), and
whether an active decoded frame needed compositing at the live Library or Viewer bounds.
No model workflow should bring the window forward.
Automated playback diagnostics must set `FACIAL_TEST_SILENT=1`; this passes
`--no-audio` to LibVLC so a test can never play through the operator's headset.

### CLIP model provisioning (semantic search)

Drop TWO ONNX exports into `product/models/` (or point
`FACIAL_CLIP_IMAGE_MODEL` / `FACIAL_CLIP_TEXT_MODEL` at them):

```text
product/models/clip-vit-b32-visual.onnx    # image encoder WITH projection head -> [1, 512]
product/models/clip-vit-b32-textual.onnx   # text encoder WITH projection head  -> [1, 512]
```

Use CLIPVisionModelWithProjection / CLIPTextModelWithProjection exports of
`openai/clip-vit-base-patch32` (the projection-head variants; plain
`last_hidden_state` exports are rejected by the load-time self-check). The text
encoder takes `input_ids` (int64, [1,77]) and optionally `attention_mask`; the
image encoder takes `pixel_values` (float32, [1,3,224,224]). The BPE
vocabulary is vendored at `product/assets/clip/bpe_simple_vocab_16e6.txt`
(MIT, from openai/CLIP). Runtime is the app's own tract engine — no Python, no
onnxruntime, CPU only. Load failures and absent models degrade to the local
metadata scorer with the reason in the toolbar status line.

### Failure modes

- "media db is locked by another instance" — the GUI holds `media.redb`; use
  ui-intents or close the GUI. Backend metadata commands (`media_fav_list`,
  `media_labels_list`, `media_meta_*`) fail this way by design rather than
  returning an empty result. Their live-GUI equivalents are
  `media_tabs --action labels` and `media_label_mutation`.
- `inventory_error: "inventory manifest table ... does not exist"` in scan
  diagnostics on the **first** scan in a brand-new workspace is expected: the
  last-good inventory table is created by that first write. It disappears on the
  next scan and never affects the row count. Persisting across later scans means
  the workspace database is not writable.
- "semantic search: local fallback (missing …)" — provision the CLIP models.
- "skipped N unindexed" — run `media_index_build` for that folder.
- Deleting `.facial/media/thumbs/` or `clip_index.redb` is always safe; they
  rebuild on demand. `media.redb` is irreplaceable operator metadata: deleting it loses
  tags, notes, labels, favorites, and label definitions. Back it up with the workspace.

</topic>

<topic id="ref-gui-inspector" summary="Reference — the headless GUI inspector for layout review and visual regression">

## Reference: GUI inspector

The GUI inspector renders every tab **headlessly** — egui computes each widget's
rectangle on the CPU, so **no window appears** — and writes, per tab, a directly
viewable PNG, a vector wireframe, and a structured layout a model can read. Use it to keep the GUI
clean, focused, and organised: review it whenever you add or move panels, buttons, or
fields, and as part of testing a GUI change.

Run:

```
facial-cli ui-inspect [--out DIR] [--tab VOCAB ...]
```

- No flags → captures all nine tabs plus the forced-state presets
  (`compare_dialog`, `media_grid`, `media_full`, `media_hidden`). `--tab`
  (repeatable) limits to specific tabs (`media | project | quality_iq |
  identity | duplicates | run_debug | manual | lanes | options`; `compare`
  remains accepted as an alias).
- Output (default `<workspace_root>/.facial/ui-snapshots/<timestamp>/`):
  - `<tab>.svg` — a labelled wireframe of panels/buttons/fields. **Open it in
    a vector viewer** when vector inspection is useful.
  - `<tab>.png` — the same wireframe rasterized in-process with pure-Rust `resvg`;
    models can open it directly without browser automation or an external converter.
  - `<tab>.layout.json` — `screen` size, and `texts` / `rects` each with `x,y,w,h`. A
    model reads this to detect problems precisely.
  - `index.html` (links every tab PNG, SVG, and layout) + `index.json`.
- Output is deterministic: an unchanged GUI produces a byte-identical `layout.json`, so
  two snapshots diff cleanly for visual-regression review.
- The inspector keeps its media metadata database inside the snapshot directory. You can
  run it while Facial is open: it does not lock or modify the live workspace metadata and
  should not display a database-lock warning merely because the GUI is running.
- The inspector disables both controller acquisition routes before rendering. This keeps
  headless inspection device-neutral and prevents a connected pad from navigating or
  synthesizing Start/Menu Alt+Tab. Use `facial-cli controller-probe` for controller
  acquisition and state instead.

How to read it (find issues without opening the app):
- **Off-canvas**: any text/rect where `x + w > screen.w` (1280) or `y + h > screen.h`
  (800) is clipped — e.g. a long path that overflows the panel.
- **Overlap**: two texts at nearly the same `y` whose `x` ranges intersect.
- **Cramped / wasted space**: many single-line rows with tiny or uneven `y` gaps;
  zero-width text rows are empty placeholders eating vertical space.
- **Duplicates**: the same label text appearing twice usually means a redundant
  control.

When you add a GUI widget, keep it inspectable: render through `FacialApp::render_ui`
(both the live app and the inspector call it), so new widgets appear in the next
snapshot automatically.

</topic>
