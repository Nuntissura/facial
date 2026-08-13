mod api;
mod config;
mod debug;
mod folder_picker;
mod identity;
mod landmarks;
mod lanes;
mod media_clip;
mod media_db;
mod media_explorer;
mod media_fs;
mod media_input;
mod media_io;
mod media_search;
mod media_tabs;
mod media_thumbs;
mod models;
mod platform_input;
mod plugin_host;
mod plugins;
mod review;
mod service;
mod theme;
mod ui;
mod ui_inspect;
mod video_player;

use config::load_config;
use service::FacialService;
use ui::FacialApp;

use api::{ApiPaths, Command, CommandKind};

/// Apply after persisted eframe window settings are merged. On Windows, winit
/// force-activates a window created fullscreen even when `active` was false,
/// so background automation must explicitly clear both properties at the
/// final window-builder hook as well as on the initial viewport.
fn background_safe_viewport(
    builder: eframe::egui::ViewportBuilder,
) -> eframe::egui::ViewportBuilder {
    builder.with_fullscreen(false).with_active(false)
}

/// Launch the desktop application. The `facial` binary is GUI-only; terminal
/// commands live in the sibling `facial-cli` binary so Windows never creates a
/// console before the desktop process starts.
pub fn run_gui(args: &[String]) -> i32 {
    let config = load_config();
    // LibVLC's first instance creation can spend seconds loading its plugin
    // registry. Warm that process-global OS/plugin cache while the existing
    // service/model startup runs, never on the first Play frame.
    video_player::prewarm_async();
    let gui_args = if args.first().map(String::as_str) == Some("gui") {
        &args[1..]
    } else {
        args
    };
    let background = gui_args.iter().any(|arg| arg == "--background");
    let paths = ApiPaths::from_config(&config);
    if let Err(error) = paths
        .ensure_dirs()
        .and_then(|()| api::recover_ui_intents(&paths))
    {
        eprintln!("failed to recover interrupted UI intents: {error}");
    }
    let service = FacialService::new(config);
    // Window identity (WP-015): logomark icon, sane minimum size, and
    // remembered window geometry (eframe persistence, user app-data).
    let icon_size = 64usize;
    let icon = eframe::egui::IconData {
        rgba: theme::window_icon_rgba(icon_size),
        width: icon_size as u32,
        height: icon_size as u32,
    };
    let viewport = eframe::egui::ViewportBuilder::default()
        .with_icon(std::sync::Arc::new(icon))
        .with_min_inner_size([980.0, 640.0])
        .with_inner_size([1280.0, 800.0])
        .with_active(!background);
    let native_options = eframe::NativeOptions {
        viewport: if background {
            background_safe_viewport(viewport)
        } else {
            viewport
        },
        // eframe restores persisted fullscreen after `viewport` is
        // built. This hook runs after that merge and prevents winit's
        // fullscreen creation path from force-activating Facial.
        window_builder: background
            .then(|| Box::new(background_safe_viewport) as eframe::WindowBuilderHook),
        persist_window: true,
        ..Default::default()
    };

    let result = eframe::run_native(
        "facial",
        native_options,
        Box::new(move |cc| Box::new(FacialApp::new(cc, service))),
    );
    if let Err(err) = result {
        eprintln!("failed to start facial: {err}");
        return 1;
    }
    0
}

/// Run one terminal/model command through the console-subsystem sibling
/// executable. `ui-inspect` remains here because it is a headless model tool.
pub fn run_cli_entry(args: &[String]) -> i32 {
    if args.first().map(String::as_str) == Some("controller-probe") {
        return match media_input::controller_probe() {
            Ok(snapshot) => match serde_json::to_string_pretty(&snapshot) {
                Ok(json) => {
                    println!("{json}");
                    0
                }
                Err(error) => {
                    eprintln!("facial-cli controller-probe: serialize error: {error}");
                    1
                }
            },
            Err(error) => {
                eprintln!("facial-cli controller-probe: {error}");
                1
            }
        };
    }
    let config = load_config();
    match args.first().map(String::as_str) {
        Some("ui-inspect") => {
            let code = run_ui_inspect(config, &args[1..]);
            code
        }
        Some(sub) if !matches!(sub, "run-queue" | "command" | "help" | "--help" | "-h") => {
            match build_command_from_flags(sub, &args[1..]) {
                Ok(cmd) if cmd.command.is_ui_intent() => {
                    let paths = ApiPaths::from_config(&config);
                    let _ = paths.ensure_dirs();
                    let receipt = api::dispatch_ui_intent(&paths, &cmd);
                    if let Err(error) = api::write_receipt_file(&paths, &receipt) {
                        eprintln!("facial-cli: failed to write UI-intent receipt: {error}");
                        return 1;
                    }
                    match serde_json::to_string_pretty(&receipt) {
                        Ok(json) => println!("{json}"),
                        Err(error) => {
                            eprintln!("facial-cli: receipt serialize error: {error}");
                            return 1;
                        }
                    }
                    exit_code_for_status(receipt.status)
                }
                Ok(_) => {
                    let mut service = FacialService::new(config.clone());
                    let paths = ApiPaths::from_config(&config);
                    let _ = paths.ensure_dirs();
                    let _ = api::recover_processing(&paths);
                    run_cli(&mut service, &paths, args)
                }
                Err(error) => {
                    eprintln!("facial-cli: {error}");
                    print_cli_usage();
                    1
                }
            }
        }
        Some(_) => {
            let mut service = FacialService::new(config.clone());
            let paths = ApiPaths::from_config(&config);
            let _ = paths.ensure_dirs();
            let _ = api::recover_processing(&paths);
            run_cli(&mut service, &paths, args)
        }
        None => {
            print_cli_usage();
            1
        }
    }
}

/// Headless GUI inspector (WP-008): render each tab offscreen and write
/// `<tab>.png` + `<tab>.svg` + `<tab>.layout.json` + an index. No GUI window appears.
/// `facial-cli ui-inspect [--out DIR] [--tab VOCAB ...]`
fn run_ui_inspect(config: config::AppConfig, args: &[String]) -> i32 {
    let mut out_dir: Option<std::path::PathBuf> = None;
    let mut tabs: Vec<ui::Tab> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--out" => {
                i += 1;
                match args.get(i) {
                    Some(v) => out_dir = Some(std::path::PathBuf::from(v)),
                    None => {
                        eprintln!("facial-cli ui-inspect: --out requires a value");
                        return 1;
                    }
                }
            }
            "--tab" => {
                i += 1;
                match args.get(i).and_then(|v| ui::Tab::from_vocab(v)) {
                    Some(t) => tabs.push(t),
                    None => {
                        eprintln!("facial-cli ui-inspect: --tab requires a valid vocab (project|quality_iq|identity|duplicates|run_debug|manual|media|compare|lanes|options)");
                        return 1;
                    }
                }
            }
            other => {
                eprintln!("facial-cli ui-inspect: unknown flag {other}");
                return 1;
            }
        }
        i += 1;
    }
    let tabs = if tabs.is_empty() {
        ui::Tab::ALL.to_vec()
    } else {
        tabs
    };
    match ui_inspect::run(config, out_dir, &tabs) {
        Ok(dir) => {
            println!("ui snapshot written: {}", dir.display());
            println!("visual index: {}", dir.join("index.html").display());
            0
        }
        Err(e) => {
            eprintln!("facial-cli ui-inspect failed: {e}");
            1
        }
    }
}

/// Exit code for a terminal/accepted receipt status string.
/// 0 for ok/accepted/applied; 1 for error/rejected.
fn exit_code_for_status(status: api::ActionStatus) -> i32 {
    use api::ActionStatus;
    match status {
        ActionStatus::Ok | ActionStatus::Accepted | ActionStatus::Applied => 0,
        ActionStatus::Error | ActionStatus::Rejected => 1,
    }
}

/// Headless CLI entry. Dispatches a single command (or drains the queue)
/// against the live service, writes receipts, prints results, and returns
/// an exit code. Never launches the egui GUI.
fn run_cli(service: &mut FacialService, paths: &ApiPaths, args: &[String]) -> i32 {
    let Some(sub) = args.first().map(|s| s.as_str()) else {
        eprintln!("facial-cli: missing subcommand");
        print_cli_usage();
        return 1;
    };
    let rest = &args[1..];

    match sub {
        "help" | "--help" | "-h" => {
            print_cli_usage();
            0
        }
        "run-queue" => run_queue_cli(service, paths, rest),
        "command" => command_cli(service, paths, rest),
        // convenience builders: `facial <kind> [--flags...]`
        _ => match build_command_from_flags(sub, rest) {
            Ok(cmd) => dispatch_and_report(service, paths, &cmd),
            Err(err) => {
                eprintln!("facial-cli: {err}");
                print_cli_usage();
                1
            }
        },
    }
}

/// `facial-cli run-queue [--once | --watch [--poll-ms N]]`
fn run_queue_cli(service: &mut FacialService, paths: &ApiPaths, args: &[String]) -> i32 {
    let mut watch = false;
    let mut poll_ms: u64 = 250;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--once" => watch = false,
            "--watch" => watch = true,
            "--poll-ms" => {
                i += 1;
                match args.get(i).and_then(|v| v.parse::<u64>().ok()) {
                    Some(v) => poll_ms = v,
                    None => {
                        eprintln!("facial run-queue: --poll-ms requires a number");
                        return 1;
                    }
                }
            }
            other => {
                eprintln!("facial run-queue: unknown flag {other}");
                return 1;
            }
        }
        i += 1;
    }

    if watch {
        match api::watch_queue(service, paths, poll_ms) {
            Ok(()) => 0,
            Err(err) => {
                eprintln!("facial run-queue --watch: {err}");
                1
            }
        }
    } else {
        let receipts = api::run_queue_once(service, paths);
        // Print each receipt as a JSON line so callers can tail stdout.
        let mut worst = 0;
        for receipt in &receipts {
            match serde_json::to_string(receipt) {
                Ok(line) => println!("{line}"),
                Err(err) => eprintln!("facial run-queue: receipt serialize error: {err}"),
            }
            worst = worst.max(exit_code_for_status(receipt.status));
        }
        worst
    }
}

/// `facial-cli command <path>` | `facial-cli command --json '<json>'`
fn command_cli(service: &mut FacialService, paths: &ApiPaths, args: &[String]) -> i32 {
    let parsed = match args.first().map(|s| s.as_str()) {
        Some("--json") => match args.get(1) {
            Some(json) => api::parse_command_str(json),
            None => {
                eprintln!("facial command --json: missing JSON argument");
                return 1;
            }
        },
        Some(path) => api::parse_command_file(std::path::Path::new(path)),
        None => {
            eprintln!("facial command: missing <path> or --json <json>");
            return 1;
        }
    };

    match parsed {
        Ok(cmd) => dispatch_and_report(service, paths, &cmd),
        Err(err) => {
            eprintln!("facial command: parse error: {err}");
            1
        }
    }
}

/// Dispatch one Command, write its receipt, print the receipt JSON to stdout,
/// and return the matching exit code.
fn dispatch_and_report(service: &mut FacialService, paths: &ApiPaths, cmd: &Command) -> i32 {
    let receipt = api::dispatch(service, paths, cmd);
    if let Err(err) = api::write_receipt(service, paths, &receipt) {
        eprintln!(
            "facial: failed to write receipt for {}: {err}",
            receipt.action_id
        );
    }
    match serde_json::to_string_pretty(&receipt) {
        Ok(json) => println!("{json}"),
        Err(err) => eprintln!("facial: receipt serialize error: {err}"),
    }
    exit_code_for_status(receipt.status)
}

/// Build a Command from a `<kind>` convenience subcommand plus `--flags`.
/// The kind string is the snake_case CommandKind discriminator.
fn build_command_from_flags(kind: &str, args: &[String]) -> Result<Command, String> {
    // Collected flag values.
    let mut action_id: Option<String> = None;
    let mut actor: Option<String> = None;
    let mut project: Option<String> = None;
    let mut worktree: Option<String> = None;
    let mut tab: Option<String> = None;
    let mut run_id: Option<String> = None;
    let mut artifact_path: Option<String> = None;
    let mut features: Vec<String> = Vec::new();
    let mut images: Vec<String> = Vec::new();
    let mut dir: Option<String> = None;
    let mut in_place = false;
    let mut keep_dir: Option<String> = None;
    let mut cull_dir: Option<String> = None;
    let mut review_dir: Option<String> = None;
    let mut in_parent = false;
    let mut session: Option<String> = None;
    let mut shards: Option<usize> = None;
    let mut shard: Option<usize> = None;
    let mut image_id: Option<String> = None;
    let mut decision: Option<String> = None;
    let mut reason: Option<String> = None;
    let mut steal = false;
    let mut gate_manifest: Option<String> = None;
    let mut clusters: Option<String> = None;
    let mut page: Option<usize> = None;
    let mut face_crop = false;
    let mut filters: Vec<String> = Vec::new();
    let mut out_dir: Option<String> = None;
    let mut repeats: Option<usize> = None;
    let mut name: Option<String> = None;
    let mut allow_partial = false;
    let mut threshold: Option<f32> = None;
    let mut lane_id: Option<String> = None;
    let mut lane_name: Option<String> = None;
    let mut lane_mode: Option<String> = None;
    let mut lane_recursive: Option<bool> = None;
    let mut concurrency_limit: Option<usize> = None;
    // media metadata + browser flags (WP-042)
    let mut media_notes: Option<String> = None;
    let mut media_tags: Option<String> = None;
    let mut media_label: Option<String> = None;
    let mut media_hex: Option<String> = None;
    let mut media_confirm = false;
    let mut media_tag_filter: Option<String> = None;
    let mut media_query: Option<String> = None;
    let mut media_mode: Option<String> = None;
    let mut media_files: Vec<String> = Vec::new();
    let mut media_cap_mb: Option<u64> = None;
    let mut media_nav_action: Option<String> = None;
    let mut media_video_value: Option<i64> = None;
    let mut media_tab_id: Option<String> = None;

    // Fetch the value following a value-taking flag at index `i`.
    fn value_at(args: &[String], i: usize, label: &str) -> Result<String, String> {
        args.get(i + 1)
            .cloned()
            .ok_or_else(|| format!("flag {label} requires a value"))
    }

    let mut i = 0;
    while i < args.len() {
        let flag = args[i].clone();
        match flag.as_str() {
            "--action-id" => {
                action_id = Some(value_at(args, i, "--action-id")?);
                i += 1;
            }
            "--actor" => {
                actor = Some(value_at(args, i, "--actor")?);
                i += 1;
            }
            "--project" => {
                project = Some(value_at(args, i, "--project")?);
                i += 1;
            }
            "--worktree" => {
                worktree = Some(value_at(args, i, "--worktree")?);
                i += 1;
            }
            "--tab" => {
                tab = Some(value_at(args, i, "--tab")?);
                i += 1;
            }
            "--run-id" => {
                run_id = Some(value_at(args, i, "--run-id")?);
                i += 1;
            }
            "--path" => {
                artifact_path = Some(value_at(args, i, "--path")?);
                i += 1;
            }
            "--feature" | "--features" => {
                features.push(value_at(args, i, "--feature")?);
                i += 1;
            }
            "--image" | "--images" => {
                images.push(value_at(args, i, "--image")?);
                i += 1;
            }
            "--dir" => {
                dir = Some(value_at(args, i, "--dir")?);
                i += 1;
            }
            "--in-place" => in_place = true,
            "--in-parent" => in_parent = true,
            "--session" => {
                session = Some(value_at(args, i, "--session")?);
                i += 1;
            }
            "--shards" => {
                let raw = value_at(args, i, "--shards")?;
                shards = Some(
                    raw.parse::<usize>()
                        .map_err(|_| format!("--shards expects a number, got '{raw}'"))?,
                );
                i += 1;
            }
            "--shard" => {
                let raw = value_at(args, i, "--shard")?;
                shard = Some(
                    raw.parse::<usize>()
                        .map_err(|_| format!("--shard expects a number, got '{raw}'"))?,
                );
                i += 1;
            }
            "--id" => {
                image_id = Some(value_at(args, i, "--id")?);
                i += 1;
            }
            "--decision" => {
                decision = Some(value_at(args, i, "--decision")?);
                i += 1;
            }
            "--reason" => {
                reason = Some(value_at(args, i, "--reason")?);
                i += 1;
            }
            "--steal" => steal = true,
            "--gate-manifest" => {
                gate_manifest = Some(value_at(args, i, "--gate-manifest")?);
                i += 1;
            }
            "--clusters" => {
                clusters = Some(value_at(args, i, "--clusters")?);
                i += 1;
            }
            "--page" => {
                let raw = value_at(args, i, "--page")?;
                page = Some(
                    raw.parse::<usize>()
                        .map_err(|_| format!("--page expects a number, got '{raw}'"))?,
                );
                i += 1;
            }
            "--face-crop" => face_crop = true,
            "--filter" => {
                filters.push(value_at(args, i, "--filter")?);
                i += 1;
            }
            "--out" => {
                out_dir = Some(value_at(args, i, "--out")?);
                i += 1;
            }
            "--repeats" => {
                let raw = value_at(args, i, "--repeats")?;
                repeats = Some(
                    raw.parse::<usize>()
                        .map_err(|_| format!("--repeats expects a number, got '{raw}'"))?,
                );
                i += 1;
            }
            "--name" => {
                name = Some(value_at(args, i, "--name")?);
                i += 1;
            }
            "--allow-partial" => allow_partial = true,
            "--threshold" => {
                let raw = value_at(args, i, "--threshold")?;
                threshold = Some(
                    raw.parse::<f32>()
                        .map_err(|_| format!("--threshold expects a number, got '{raw}'"))?,
                );
                i += 1;
            }
            "--lane-id" => {
                lane_id = Some(value_at(args, i, "--lane-id")?);
                i += 1;
            }
            "--lane-name" => {
                lane_name = Some(value_at(args, i, "--lane-name")?);
                i += 1;
            }
            "--lane-mode" => {
                lane_mode = Some(value_at(args, i, "--lane-mode")?);
                i += 1;
            }
            "--recursive" => lane_recursive = Some(true),
            "--no-recursive" => lane_recursive = Some(false),
            "--concurrency" | "--concurrency-limit" => {
                let raw = value_at(args, i, "--concurrency-limit")?;
                concurrency_limit =
                    Some(raw.parse::<usize>().map_err(|_| {
                        format!("--concurrency-limit expects a number, got '{raw}'")
                    })?);
                i += 1;
            }
            "--keep-dir" => {
                keep_dir = Some(value_at(args, i, "--keep-dir")?);
                i += 1;
            }
            "--cull-dir" => {
                cull_dir = Some(value_at(args, i, "--cull-dir")?);
                i += 1;
            }
            "--review-dir" => {
                review_dir = Some(value_at(args, i, "--review-dir")?);
                i += 1;
            }
            "--notes" => {
                media_notes = Some(value_at(args, i, "--notes")?);
                i += 1;
            }
            "--tags" => {
                media_tags = Some(value_at(args, i, "--tags")?);
                i += 1;
            }
            "--label" => {
                media_label = Some(value_at(args, i, "--label")?);
                i += 1;
            }
            "--hex" => {
                media_hex = Some(value_at(args, i, "--hex")?);
                i += 1;
            }
            "--confirm" | "--confirmed" => media_confirm = true,
            "--tag" => {
                media_tag_filter = Some(value_at(args, i, "--tag")?);
                i += 1;
            }
            "--query" => {
                media_query = Some(value_at(args, i, "--query")?);
                i += 1;
            }
            "--mode" => {
                media_mode = Some(value_at(args, i, "--mode")?);
                i += 1;
            }
            "--file" | "--files" => {
                media_files.push(value_at(args, i, "--file")?);
                i += 1;
            }
            "--cap-mb" => {
                let raw = value_at(args, i, "--cap-mb")?;
                media_cap_mb = Some(
                    raw.parse::<u64>()
                        .map_err(|_| format!("--cap-mb expects a number, got '{raw}'"))?,
                );
                i += 1;
            }
            "--action" => {
                media_nav_action = Some(value_at(args, i, "--action")?);
                i += 1;
            }
            "--tab-id" => {
                media_tab_id = Some(value_at(args, i, "--tab-id")?);
                i += 1;
            }
            "--value" => {
                let raw = value_at(args, i, "--value")?;
                media_video_value = Some(
                    raw.parse::<i64>()
                        .map_err(|_| format!("--value expects an integer, got '{raw}'"))?,
                );
                i += 1;
            }
            other => return Err(format!("unknown flag {other} for kind {kind}")),
        }
        i += 1;
    }

    let review_flags = ReviewFlags {
        session,
        shards,
        shard,
        id: image_id,
        decision,
        reason,
        steal,
        actor: actor.clone(),
        gate_manifest,
        clusters,
        page,
        face_crop,
        filters,
        out: out_dir,
        repeats,
        name,
        allow_partial,
        threshold,
        lane_id,
        lane_name,
        lane_mode,
        lane_recursive,
        concurrency_limit,
        media_notes,
        media_tags,
        media_label,
        media_hex,
        media_confirm,
        media_tag_filter,
        media_query,
        media_mode,
        media_files,
        media_cap_mb,
        media_nav_action,
        media_video_value,
        media_tab_id,
    };
    let command = command_kind_from_flags(
        kind,
        project,
        worktree,
        tab,
        run_id,
        artifact_path,
        features,
        images,
        dir,
        in_place,
        keep_dir,
        cull_dir,
        review_dir,
        in_parent,
        review_flags,
    )?;

    let action_id = action_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    Ok(Command {
        action_id,
        protocol_version: api::API_PROTOCOL_VERSION,
        actor,
        issued_at: Some(chrono::Utc::now().to_rfc3339()),
        command,
    })
}

/// Review-queue flag bundle (WP-016) so the builder signature stays sane.
struct ReviewFlags {
    session: Option<String>,
    shards: Option<usize>,
    shard: Option<usize>,
    id: Option<String>,
    decision: Option<String>,
    reason: Option<String>,
    steal: bool,
    actor: Option<String>,
    gate_manifest: Option<String>,
    clusters: Option<String>,
    page: Option<usize>,
    face_crop: bool,
    filters: Vec<String>,
    out: Option<String>,
    repeats: Option<usize>,
    name: Option<String>,
    allow_partial: bool,
    threshold: Option<f32>,
    lane_id: Option<String>,
    lane_name: Option<String>,
    lane_mode: Option<String>,
    lane_recursive: Option<bool>,
    concurrency_limit: Option<usize>,
    // media metadata + browser (WP-042)
    media_notes: Option<String>,
    media_tags: Option<String>,
    media_label: Option<String>,
    media_hex: Option<String>,
    media_confirm: bool,
    media_tag_filter: Option<String>,
    media_query: Option<String>,
    media_mode: Option<String>,
    media_files: Vec<String>,
    media_cap_mb: Option<u64>,
    media_nav_action: Option<String>,
    media_video_value: Option<i64>,
    media_tab_id: Option<String>,
}

/// Map a snake_case kind + collected flags onto a CommandKind variant.
#[allow(clippy::too_many_arguments)]
fn command_kind_from_flags(
    kind: &str,
    project: Option<String>,
    worktree: Option<String>,
    tab: Option<String>,
    run_id: Option<String>,
    artifact_path: Option<String>,
    features: Vec<String>,
    images: Vec<String>,
    dir: Option<String>,
    in_place: bool,
    keep_dir: Option<String>,
    cull_dir: Option<String>,
    review_dir: Option<String>,
    in_parent: bool,
    review: ReviewFlags,
) -> Result<CommandKind, String> {
    let kind = kind.to_ascii_lowercase();
    let need = |opt: Option<String>, label: &str| -> Result<String, String> {
        opt.ok_or_else(|| format!("{kind} requires {label}"))
    };
    match kind.as_str() {
        "list_features" => Ok(CommandKind::ListFeatures),
        "list_models" => Ok(CommandKind::ListModels),
        "list_worktrees" => Ok(CommandKind::ListWorktrees),
        "get_state" => Ok(CommandKind::GetState),
        "start_run" => Ok(CommandKind::StartRun {
            project_name: need(project, "--project")?,
            image_paths: images,
            feature_keys: features,
            worktree_path: worktree,
            in_place,
        }),
        "get_run_status" => Ok(CommandKind::GetRunStatus {
            run_id: need(run_id, "--run-id")?,
        }),
        "get_run_summary" => Ok(CommandKind::GetRunSummary {
            run_id: need(run_id, "--run-id")?,
        }),
        "list_artifacts" => Ok(CommandKind::ListArtifacts {
            run_id: need(run_id, "--run-id")?,
        }),
        "read_artifact" => Ok(CommandKind::ReadArtifact {
            path: need(artifact_path, "--path")?,
        }),
        "set_workspace_root" => Ok(CommandKind::SetWorkspaceRoot {
            path: need(artifact_path, "--path")?,
        }),
        "set_copy_location" => Ok(CommandKind::SetCopyLocation {
            path: need(artifact_path, "--path")?,
        }),
        "sort_run" => Ok(CommandKind::SortRun {
            run_id: need(run_id, "--run-id")?,
            in_parent,
            keep_dir: keep_dir.unwrap_or_default(),
            cull_dir: cull_dir.unwrap_or_default(),
            review_dir: review_dir.unwrap_or_default(),
        }),
        "identity_status" => Ok(CommandKind::IdentityStatus),
        "identity_gate" => Ok(CommandKind::IdentityGate {
            image: images
                .into_iter()
                .next()
                .or(artifact_path)
                .ok_or_else(|| format!("{kind} requires --image or --path"))?,
        }),
        "identity_gate_dir" => Ok(CommandKind::IdentityGateDir {
            dir: dir
                .or(artifact_path)
                .ok_or_else(|| format!("{kind} requires --dir or --path"))?,
        }),
        // identity tooling (WP-017/WP-018)
        "identity_dedup" => Ok(CommandKind::IdentityDedup {
            dir: dir
                .or(artifact_path)
                .ok_or_else(|| format!("{kind} requires --dir or --path"))?,
            threshold: review.threshold.unwrap_or(0.9),
        }),
        "render_eval" => Ok(CommandKind::RenderEval {
            dir: dir
                .or(artifact_path)
                .ok_or_else(|| format!("{kind} requires --dir or --path"))?,
        }),
        "calibrate_threshold" => Ok(CommandKind::CalibrateThreshold),
        "anchor_montage" => Ok(CommandKind::AnchorMontage {
            image: images
                .into_iter()
                .next()
                .or(artifact_path)
                .ok_or_else(|| format!("{kind} requires --image or --path"))?,
        }),
        // review queue (WP-016)
        "review_init" => Ok(CommandKind::ReviewInit {
            dir: dir
                .or(artifact_path)
                .ok_or_else(|| format!("{kind} requires --dir or --path"))?,
            shards: review.shards.unwrap_or(1),
            gate_manifest: review.gate_manifest,
            clusters: review.clusters,
        }),
        "review_montage" => Ok(CommandKind::ReviewMontage {
            session: need(review.session, "--session")?,
            shard: review.shard,
            page: review.page.unwrap_or(0),
            face_crop: review.face_crop,
            filters: review.filters,
        }),
        "review_export" => Ok(CommandKind::ReviewExport {
            session: need(review.session, "--session")?,
            out: need(review.out, "--out")?,
            repeats: review.repeats.unwrap_or(10),
            name: need(review.name, "--name")?,
            allow_partial: review.allow_partial,
        }),
        "review_claim" => Ok(CommandKind::ReviewClaim {
            session: need(review.session, "--session")?,
            shard: review.shard,
            actor: review.actor.unwrap_or_default(),
            steal: review.steal,
        }),
        "review_decide" => Ok(CommandKind::ReviewDecide {
            session: need(review.session, "--session")?,
            id: need(review.id, "--id")?,
            decision: need(review.decision, "--decision")?,
            reason: review.reason.unwrap_or_default(),
            actor: review.actor.unwrap_or_default(),
        }),
        "review_status" => Ok(CommandKind::ReviewStatus {
            session: need(review.session, "--session")?,
        }),
        // lane workspace (WP-029)
        "list_lanes" => Ok(CommandKind::ListLanes),
        "set_lane" => Ok(CommandKind::SetLane {
            lane_id: need(review.lane_id, "--lane-id")?,
            name: review.lane_name,
            mode: review.lane_mode,
            folder: dir.or(artifact_path),
            recursive: review.lane_recursive,
            steal: review.steal,
            feature_keys: if features.is_empty() {
                None
            } else {
                Some(features)
            },
        }),
        "scan_lane" => Ok(CommandKind::ScanLane {
            lane_id: need(review.lane_id, "--lane-id")?,
            steal: review.steal,
        }),
        "scan_all_lanes" => Ok(CommandKind::ScanAllLanes {
            steal: review.steal,
        }),
        "claim_lane" => Ok(CommandKind::ClaimLane {
            lane_id: need(review.lane_id, "--lane-id")?,
            actor: review.actor.unwrap_or_default(),
            steal: review.steal,
        }),
        "release_lane" => Ok(CommandKind::ReleaseLane {
            lane_id: need(review.lane_id, "--lane-id")?,
            actor: review.actor.unwrap_or_default(),
            steal: review.steal,
        }),
        "lane_status" => Ok(CommandKind::LaneStatus {
            lane_id: review.lane_id,
        }),
        "start_lane_batch" => Ok(CommandKind::StartLaneBatch {
            lane_id: need(review.lane_id, "--lane-id")?,
            project_name: project.unwrap_or_default(),
            feature_keys: features,
            in_place,
            steal: review.steal,
        }),
        "start_all_lane_batches" => Ok(CommandKind::StartAllLaneBatches {
            project_name: project.unwrap_or_default(),
            feature_keys: features,
            concurrency_limit: review.concurrency_limit.unwrap_or(2),
            in_place,
            steal: review.steal,
        }),
        // ui-intents
        "set_project" => Ok(CommandKind::SetProject {
            project_name: need(project, "--project")?,
        }),
        "set_worktree" => Ok(CommandKind::SetWorktree {
            worktree_path: need(worktree, "--worktree")?,
        }),
        "select_tab" => Ok(CommandKind::SelectTab {
            tab: need(tab, "--tab")?,
        }),
        "set_features" => Ok(CommandKind::SetFeatures {
            feature_keys: features,
        }),
        "set_in_place" => Ok(CommandKind::SetInPlace { in_place }),
        "import_paths" => Ok(CommandKind::ImportPaths {
            project_name: need(project, "--project")?,
            paths: images,
            in_place,
        }),
        "start_run_ui" => Ok(CommandKind::StartRunUi),
        "ui_snapshot" => Ok(CommandKind::UiSnapshot { output: review.out }),
        // media metadata + browser (WP-042)
        "media_meta_get" => Ok(CommandKind::MediaMetaGet {
            path: need(artifact_path, "--path")?,
        }),
        "media_meta_set" => Ok(CommandKind::MediaMetaSet {
            path: need(artifact_path, "--path")?,
            notes: review.media_notes,
            tags: review.media_tags,
            label: review.media_label,
        }),
        "media_meta_list" => Ok(CommandKind::MediaMetaList {
            tag: review.media_tag_filter,
            label: review.media_label,
        }),
        "media_labels_list" => Ok(CommandKind::MediaLabelsList),
        "media_label_configure" => Ok(CommandKind::MediaLabelConfigure {
            id: need(review.media_label, "--label")?,
            name: need(review.name, "--name")?,
            hex: need(review.media_hex, "--hex")?,
        }),
        "media_label_create" => Ok(CommandKind::MediaLabelCreate {
            name: need(review.name, "--name")?,
            hex: need(review.media_hex, "--hex")?,
            path: artifact_path,
        }),
        "media_label_update" => Ok(CommandKind::MediaLabelUpdate {
            id: need(review.media_label, "--label")?,
            name: review.name,
            hex: review.media_hex,
        }),
        "media_label_delete" => Ok(CommandKind::MediaLabelDelete {
            id: need(review.media_label, "--label")?,
            confirmed: review.media_confirm,
        }),
        "media_label_assign" => Ok(CommandKind::MediaLabelAssign {
            path: need(artifact_path, "--path")?,
            id: review.media_label,
            action: need(review.media_nav_action, "--action")?.to_ascii_lowercase(),
        }),
        "media_label_mutation" => Ok(CommandKind::MediaLabelMutation {
            action: need(review.media_nav_action, "--action")?.to_ascii_lowercase(),
            path: artifact_path,
            id: review.media_label,
            name: review.name,
            hex: review.media_hex,
            confirmed: review.media_confirm,
        }),
        "media_fav_add" => Ok(CommandKind::MediaFavAdd {
            path: need(artifact_path, "--path")?,
        }),
        "media_fav_remove" => Ok(CommandKind::MediaFavRemove {
            path: need(artifact_path, "--path")?,
        }),
        "media_fav_list" => Ok(CommandKind::MediaFavList),
        "thumbs_gc" => Ok(CommandKind::ThumbsGc {
            cap_mb: review.media_cap_mb,
        }),
        "media_index_build" => Ok(CommandKind::MediaIndexBuild {
            dir: dir
                .or(artifact_path)
                .ok_or_else(|| format!("{kind} requires --dir or --path"))?,
            recursive: review.lane_recursive.unwrap_or(false),
        }),
        "media_semantic_search" => Ok(CommandKind::MediaSemanticSearch {
            query: need(review.media_query, "--query")?,
            dir: dir
                .or(artifact_path)
                .ok_or_else(|| format!("{kind} requires --dir or --path"))?,
            limit: review.concurrency_limit,
        }),
        "media_set_folder" => Ok(CommandKind::MediaSetFolder {
            path: dir
                .or(artifact_path)
                .ok_or_else(|| format!("{kind} requires --dir or --path"))?,
        }),
        "media_search" => Ok(CommandKind::MediaSearch {
            query: need(review.media_query, "--query")?,
            mode: review.media_mode,
        }),
        "media_select" => {
            if review.media_files.is_empty() {
                return Err(format!("{kind} requires one or more --file PATH"));
            }
            Ok(CommandKind::MediaSelect {
                paths: review.media_files,
            })
        }
        "media_open_selected" => Ok(CommandKind::MediaOpenSelected),
        "media_tabs" => {
            // Silently dropping a flag the command does not use lets a model
            // believe it passed something that never arrived — e.g.
            // `--action open_collection --path labels --label Keepers` was
            // accepted with --label discarded (no-context Manual audit,
            // finding H). Refuse instead, naming the flag.
            if review.media_label.is_some() {
                return Err(format!(
                    "{kind} does not take --label; select a label with \
                     --action open_collection --path labels:LABEL_ID"
                ));
            }
            Ok(CommandKind::MediaTabs {
                action: need(review.media_nav_action, "--action")?.to_ascii_lowercase(),
                tab_id: review.media_tab_id,
                path: artifact_path,
            })
        }
        "media_folder_navigate" => Ok(CommandKind::MediaFolderNavigate {
            action: need(review.media_nav_action, "--action")?.to_ascii_lowercase(),
        }),
        "media_video_control" => Ok(CommandKind::MediaVideoControl {
            action: need(review.media_nav_action, "--action")?.to_ascii_lowercase(),
            value: review.media_video_value,
            output: review.out,
        }),
        other => Err(format!("unknown command kind: {other}")),
    }
}

fn print_cli_usage() {
    eprintln!(
        "facial-cli — headless CLI (file-based command + receipt protocol)\n\
\n\
USAGE:\n\
  facial-cli controller-probe             inspect the GUI controller backend without opening a window\n\
  facial-cli ui-inspect [--out DIR] [--tab VOCAB ...]\n\
                                          headless GUI snapshot -> .facial/ui-snapshots/<ts>/<tab>.png + .svg + .layout.json\n\
  facial-cli run-queue [--once | --watch [--poll-ms N]]\n\
                                          drain commands/ (default --once; --watch loops until <api_root>/stop)\n\
  facial-cli command <path>               parse + dispatch a command file, print receipt JSON\n\
  facial-cli command --json '<json>'      parse + dispatch an inline JSON command\n\
  facial-cli <kind> [--flags...]          convenience builder for a single command\n\
\n\
CONVENIENCE KINDS:\n\
  list_features | list_models | list_worktrees | get_state\n\
  start_run --project NAME [--feature plugin:feat ...] [--image PATH ...] [--worktree PATH] [--in-place]\n\
  get_run_status --run-id ID | get_run_summary --run-id ID | list_artifacts --run-id ID\n\
  read_artifact --path PATH\n\
  set_workspace_root --path DIR | set_copy_location --path DIR\n\
  sort_run --run-id ID [--in-parent --keep-dir DIR --review-dir DIR --cull-dir DIR]\n\
  identity_status | identity_gate --image PATH | identity_gate_dir --dir DIR\n\
  identity_dedup --dir DIR [--threshold 0.90]   near-dup groups by ArcFace cosine\n\
  render_eval --dir DIR                  score renders vs anchors, grouped by config key\n\
  calibrate_threshold                    recommend gate threshold from anchors + negatives\n\
  anchor_montage --image PATH            candidate-vs-anchors grid + similarity map\n\
  review_init --dir DIR [--shards N] [--gate-manifest PATH] [--clusters PATH]\n\
  review_claim --session S [--shard K] [--actor A] [--steal]\n\
  review_decide --session S --id ID --decision accept|reject|hold [--reason TEXT] [--actor A]\n\
  review_status --session S\n\
  review_montage --session S [--shard K] [--page N] [--face-crop] [--filter k=v ...]\n\
  review_export --session S --out DIR --name NAME [--repeats N] [--allow-partial]\n\
  list_lanes | lane_status [--lane-id ID]\n\
  set_lane --lane-id ID [--lane-name NAME] [--lane-mode compare|review|batch] [--dir DIR] [--recursive|--no-recursive] [--feature plugin:feat ...] [--steal]\n\
  scan_lane --lane-id ID [--steal] | scan_all_lanes [--steal]\n\
  claim_lane --lane-id ID --actor A [--steal] | release_lane --lane-id ID --actor A [--steal]\n\
  start_lane_batch --lane-id ID [--project NAME] [--feature plugin:feat ...] [--in-place] [--steal]\n\
  start_all_lane_batches [--project NAME] [--feature plugin:feat ...] [--concurrency-limit N] [--in-place] [--steal]\n\
  set_project --project NAME | set_worktree --worktree PATH | select_tab --tab project|quality_iq|identity|duplicates|run_debug|manual|media|compare|lanes|options\n\
  set_features [--feature plugin:feat ...] | set_in_place [--in-place]\n\
  import_paths --project NAME [--image PATH ...] [--in-place] | start_run_ui\n\
  ui_snapshot [--out FILE.png]            ui-intent: exact live UI PNG without foreground activation\n\
  media_meta_get --path PATH             notes/tags/labels/favorite for one file\n\
  media_meta_set --path PATH [--notes TEXT] [--tags a,b] [--label ID_OR_NAME]  legacy exclusive-label setter\n\
  media_meta_list [--tag TAG] [--label LABEL]   all rows with metadata (+ tag vocab)\n\
  media_labels_list                         stable label IDs, names, and backend hex values\n\
  media_label_configure --label ID --name NAME --hex \"#12ABEF\"  legacy update alias\n\
  media_label_create --name NAME --hex \"#12ABEF\" [--path PATH]\n\
  media_label_update --label ID [--name NAME] [--hex \"#12ABEF\"]\n\
  media_label_delete --label ID [--confirm]\n\
  media_label_assign --path PATH --action add|remove|clear [--label ID_OR_NAME]\n\
  media_label_mutation --action create|update|delete|add|remove|clear [label flags]   live UI intent\n\
  media_fav_add --path PATH | media_fav_remove --path PATH | media_fav_list\n\
  thumbs_gc [--cap-mb N]                 sweep the thumbnail disk cache (age + size caps)\n\
  media_index_build --dir DIR [--recursive]   embed images into the CLIP index (needs provisioned models)\n\
  media_semantic_search --query Q --dir DIR [--concurrency-limit N]   ranked cosine results from cached embeddings\n\
  media_set_folder --dir DIR             ui-intent: point the Media browser at a folder\n\
  media_search --query Q [--mode name|fuzzy|tags|notes|semantic]   ui-intent\n\
  media_select --file PATH [--file PATH ...] | media_open_selected  ui-intents\n\
  media_folder_navigate --action open|close|toggle|up|down|page_up|page_down|home|end|enter|parent|refresh|commit|open_new_tab\n\
  media_tabs --action list|labels|select|open|close|open_collection|set_scope|set_sort [--tab-id ID] [--path VALUE]\n\
             open_collection takes --path fav_videos|fav_images|labels\n\
  media_video_control --action status|play_pause|play|play_library|pause|stop|seek_ms|volume|audio_track|subtitle_track|loop|capture_frame [--value N] [--out FILE.png]\n\
\n\
COMMON FLAGS:\n\
  --action-id ID   join key (uuid auto-generated when omitted)\n\
  --actor ID       attribution (e.g. swarm model id)\n\
\n\
EXIT CODES: 0 = ok/accepted/applied; 1 = error/rejected/parse failure"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn background_viewport_overrides_persisted_fullscreen_and_activation() {
        let restored = eframe::egui::ViewportBuilder::default()
            .with_fullscreen(true)
            .with_active(true);
        let hardened = background_safe_viewport(restored);
        assert_eq!(hardened.fullscreen, Some(false));
        assert_eq!(hardened.active, Some(false));
    }

    #[test]
    fn lane_command_builders_accept_core_lane_verbs() {
        let list = command_kind_from_flags(
            "list_lanes",
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            Vec::new(),
            None,
            false,
            None,
            None,
            None,
            false,
            empty_review_flags(),
        )
        .expect("list_lanes should build");
        assert_eq!(list.id_str(), "list_lanes");

        let set = command_kind_from_flags(
            "set_lane",
            None,
            None,
            None,
            None,
            None,
            vec!["facet:quality_pass".to_string()],
            Vec::new(),
            Some("D:/shoot-a".to_string()),
            false,
            None,
            None,
            None,
            false,
            ReviewFlags {
                lane_id: Some("lane-001".to_string()),
                lane_name: Some("Shoot A".to_string()),
                lane_mode: Some("batch".to_string()),
                lane_recursive: Some(true),
                ..empty_review_flags()
            },
        )
        .expect("set_lane should build");
        assert_eq!(set.id_str(), "set_lane");

        let claim = command_kind_from_flags(
            "claim_lane",
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            Vec::new(),
            None,
            false,
            None,
            None,
            None,
            false,
            ReviewFlags {
                lane_id: Some("lane-001".to_string()),
                actor: Some("agent-a".to_string()),
                ..empty_review_flags()
            },
        )
        .expect("claim_lane should build");
        assert_eq!(claim.id_str(), "claim_lane");
    }

    #[test]
    fn dynamic_label_cli_flags_build_typed_commands() {
        let create = build_command_from_flags(
            "media_label_create",
            &[
                "--name".into(),
                "Selects".into(),
                "--hex".into(),
                "#123ABC".into(),
                "--path".into(),
                "D:\\asset.jpg".into(),
            ],
        )
        .unwrap();
        match create.command {
            CommandKind::MediaLabelCreate { name, hex, path } => {
                assert_eq!(name, "Selects");
                assert_eq!(hex, "#123ABC");
                assert_eq!(path.as_deref(), Some("D:\\asset.jpg"));
            }
            other => panic!("unexpected command: {}", other.id_str()),
        }

        let delete = build_command_from_flags(
            "media_label_delete",
            &["--label".into(), "label-abc".into(), "--confirm".into()],
        )
        .unwrap();
        assert!(matches!(
            delete.command,
            CommandKind::MediaLabelDelete {
                id,
                confirmed: true
            } if id == "label-abc"
        ));

        let assign = build_command_from_flags(
            "media_label_assign",
            &[
                "--path".into(),
                "D:\\asset.jpg".into(),
                "--action".into(),
                "ADD".into(),
                "--label".into(),
                "Selects".into(),
            ],
        )
        .unwrap();
        assert!(matches!(
            assign.command,
            CommandKind::MediaLabelAssign { action, .. } if action == "add"
        ));
    }

    fn empty_review_flags() -> ReviewFlags {
        ReviewFlags {
            session: None,
            shards: None,
            shard: None,
            id: None,
            decision: None,
            reason: None,
            steal: false,
            actor: None,
            gate_manifest: None,
            clusters: None,
            page: None,
            face_crop: false,
            filters: Vec::new(),
            out: None,
            repeats: None,
            name: None,
            allow_partial: false,
            threshold: None,
            lane_id: None,
            lane_name: None,
            lane_mode: None,
            lane_recursive: None,
            concurrency_limit: None,
            media_notes: None,
            media_tags: None,
            media_label: None,
            media_hex: None,
            media_confirm: false,
            media_tag_filter: None,
            media_query: None,
            media_mode: None,
            media_files: Vec::new(),
            media_cap_mb: None,
            media_nav_action: None,
            media_video_value: None,
            media_tab_id: None,
        }
    }
}
