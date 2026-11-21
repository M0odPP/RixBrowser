#![deny(unsafe_code)]

pub mod browser_controller;
pub mod child_runtime;
pub mod forms;
pub mod window;

pub use browser_controller::{
    BrowserController, BrowserKey, KeyModifiers, TabDocumentState, CHROME_PIPELINE,
    OVERLAY_PIPELINE, WEB_PIPELINE,
};
pub use child_runtime::{run_content_process, run_network_process};
pub use forms::{
    collect_element_text, collect_form_submission, encode_form_urlencoded, resolve_relative_url,
    ClipboardBridge, FormMethod, FormSubmission, InPageInteractionState, PageClickOutcome,
};
pub use window::{
    populate_demo_layers, render_headless_demo, run_window_shell, AppWakeup, BrowserShell,
    DEMO_CHROME_PIPELINE, DEMO_OVERLAY_PIPELINE, DEMO_WEB_PIPELINE,
};
