#![deny(unsafe_code)]

use rix_app::{run_content_process, run_network_process};
use rix_core::logging::{init_logging, init_logging_to_stderr};
use rix_ipc::{parse_process_args, ProcessRole};
use std::process::ExitCode;
use tracing::{error, info};

fn build_child_runtime() -> std::io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
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
            info!("RixBrowser Phase 1 supervisor runtime ready (window mounts in #4)");
            ExitCode::SUCCESS
        }
        Err(err) => {
            init_logging_to_stderr("warn");
            error!(%err, "invalid process launch arguments");
            ExitCode::from(2)
        }
    }
}
