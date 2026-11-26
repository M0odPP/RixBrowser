#![deny(unsafe_code)]

use rix_app::{
    render_headless_demo, render_url_with_options, run_content_process, run_network_process,
    run_window_shell, RenderUrlOptions,
};
use rix_chrome::ThemeMode;
use rix_core::logging::{init_logging, init_logging_to_stderr};
use rix_ipc::{parse_process_args, ProcessRole};
use std::path::PathBuf;
use std::process::ExitCode;
use tracing::{error, info};

struct RenderUrlCli {
    url: String,
    out_path: String,
    metrics_json: Option<String>,
    dump_dom_layout: Option<String>,
    post_action_png: Option<String>,
    run_interactive: bool,
    measure_warm: bool,
    width: u32,
    height: u32,
    dpr: f32,
    theme: ThemeMode,
}

fn parse_render_url_args(args: &[String]) -> Option<RenderUrlCli> {
    let mut url = None;
    let mut out_path = "rix-page.png".to_string();
    let mut metrics_json = None;
    let mut dump_dom_layout = None;
    let mut post_action_png = None;
    let mut run_interactive = false;
    let mut measure_warm = false;
    let mut width = 1280u32;
    let mut height = 800u32;
    let mut dpr = 1.0f32;
    let mut theme = ThemeMode::Dark;

    let mut idx = 0;
    while idx < args.len() {
        let arg = &args[idx];
        if let Some(v) = arg.strip_prefix("--render-url=") {
            url = Some(v.to_string());
        } else if arg == "--render-url" {
            url = args.get(idx + 1).cloned();
            idx += 1;
        } else if let Some(v) = arg.strip_prefix("--out=") {
            out_path = v.to_string();
        } else if arg == "--out" {
            if let Some(v) = args.get(idx + 1) {
                out_path = v.clone();
                idx += 1;
            }
        } else if let Some(v) = arg.strip_prefix("--metrics-json=") {
            metrics_json = Some(v.to_string());
        } else if arg == "--metrics-json" {
            if let Some(v) = args.get(idx + 1) {
                metrics_json = Some(v.clone());
                idx += 1;
            }
        } else if let Some(v) = arg.strip_prefix("--dump-dom-layout=") {
            dump_dom_layout = Some(v.to_string());
        } else if arg == "--dump-dom-layout" {
            if let Some(v) = args.get(idx + 1) {
                dump_dom_layout = Some(v.clone());
                idx += 1;
            }
        } else if let Some(v) = arg.strip_prefix("--post-out=") {
            post_action_png = Some(v.to_string());
        } else if arg == "--post-out" {
            if let Some(v) = args.get(idx + 1) {
                post_action_png = Some(v.clone());
                idx += 1;
            }
        } else if arg == "--interactive" {
            run_interactive = true;
        } else if arg == "--warm" {
            measure_warm = true;
        } else if let Some(v) = arg.strip_prefix("--width=") {
            if let Ok(w) = v.parse::<u32>() {
                width = w.max(64);
            }
        } else if let Some(v) = arg.strip_prefix("--height=") {
            if let Ok(h) = v.parse::<u32>() {
                height = h.max(64);
            }
        } else if let Some(v) = arg.strip_prefix("--dpr=") {
            if let Ok(scale) = v.parse::<f32>() {
                dpr = scale.clamp(0.5, 4.0);
            }
        } else if arg == "--light" {
            theme = ThemeMode::Light;
        } else if arg == "--dark" {
            theme = ThemeMode::Dark;
        }
        idx += 1;
    }

    url.map(|url| RenderUrlCli {
        url,
        out_path,
        metrics_json,
        dump_dom_layout,
        post_action_png,
        run_interactive,
        measure_warm,
        width,
        height,
        dpr,
        theme,
    })
}

fn build_child_runtime() -> std::io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
}

fn parse_headless_demo_path(args: &[String]) -> Option<String> {
    let mut idx = 0;
    while idx < args.len() {
        let arg = &args[idx];
        if let Some(path) = arg.strip_prefix("--headless-demo=") {
            return Some(path.to_string());
        }
        if arg == "--headless-demo" {
            return Some(
                args.get(idx + 1)
                    .cloned()
                    .unwrap_or_else(|| "rix-demo.png".to_string()),
            );
        }
        idx += 1;
    }
    None
}

fn main() -> ExitCode {
    let raw_args: Vec<String> = std::env::args().skip(1).collect();
    match parse_process_args(&raw_args) {
        Ok(Some(launch)) => {
            init_logging_to_stderr("warn");
            let runtime = match build_child_runtime() {
                Ok(rt) => rt,
                Err(err) => {
                    error!(%err, "failed to create child tokio runtime");
                    return ExitCode::FAILURE;
                }
            };
            let res = match launch.role {
                ProcessRole::Content { site_id } => {
                    runtime.block_on(run_content_process(site_id, &launch.ipc_socket))
                }
                ProcessRole::Network => runtime.block_on(run_network_process(&launch.ipc_socket)),
                ProcessRole::Browser => unreachable!("parse_process_args never yields Browser"),
            };
            if let Err(err) = res {
                error!(%err, "child process runtime failed");
                return ExitCode::FAILURE;
            }
            ExitCode::SUCCESS
        }
        Ok(None) => {
            init_logging("info");
            if let Some(cli) = parse_render_url_args(&raw_args) {
                info!(url = %cli.url, path = %cli.out_path, width = cli.width, height = cli.height, dpr = cli.dpr, "rendering URL headlessly");
                let opts = RenderUrlOptions {
                    url: cli.url,
                    out_path: PathBuf::from(cli.out_path),
                    dump_dom_layout: cli.dump_dom_layout.map(PathBuf::from),
                    post_action_png: cli.post_action_png.map(PathBuf::from),
                    run_interactive: cli.run_interactive,
                    measure_warm: cli.measure_warm,
                    width: cli.width,
                    height: cli.height,
                    dpr: cli.dpr,
                    theme: cli.theme,
                };
                match render_url_with_options(&opts) {
                    Ok(metrics) => {
                        if let Some(ref mpath) = cli.metrics_json {
                            if let Ok(json) = serde_json::to_string_pretty(&metrics) {
                                if let Err(err) = std::fs::write(mpath, json) {
                                    error!(%err, path = %mpath, "failed to write metrics JSON");
                                    return ExitCode::FAILURE;
                                }
                            }
                        }
                    }
                    Err(err) => {
                        error!(%err, "headless URL render failed");
                        return ExitCode::FAILURE;
                    }
                }
                return ExitCode::SUCCESS;
            }
            if let Some(out_path) = parse_headless_demo_path(&raw_args) {
                info!(path = %out_path, "rendering headless Vello compositor demo");
                if let Err(err) = render_headless_demo(&out_path, 1280, 720) {
                    error!(%err, "headless demo render failed");
                    return ExitCode::FAILURE;
                }
                return ExitCode::SUCCESS;
            }
            if let Err(err) = run_window_shell() {
                error!(%err, "window shell failed");
                return ExitCode::FAILURE;
            }
            ExitCode::SUCCESS
        }
        Err(err) => {
            init_logging_to_stderr("warn");
            error!(%err, "invalid process launch arguments");
            ExitCode::from(2)
        }
    }
}
