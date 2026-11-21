#![deny(unsafe_code)]

use rix_app::{render_headless_demo, run_content_process, run_network_process, run_window_shell};
use rix_core::logging::{init_logging, init_logging_to_stderr};
use rix_ipc::{parse_process_args, ProcessRole};
use std::process::ExitCode;
use tracing::{error, info};

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
