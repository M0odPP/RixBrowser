#![deny(unsafe_code)]

pub mod child_runtime;
pub mod window;

pub use child_runtime::{run_content_process, run_network_process};
pub use window::{
    populate_demo_layers, render_headless_demo, run_window_shell, AppWakeup, BrowserShell,
    DEMO_CHROME_PIPELINE, DEMO_OVERLAY_PIPELINE, DEMO_WEB_PIPELINE,
};
