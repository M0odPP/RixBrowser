#![deny(unsafe_code)]

pub mod browser_controller;
pub mod child_runtime;
pub mod forms;
pub mod inspector;
pub mod page_loader;
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
pub use inspector::{
    capture_page_snapshot, run_interactive_scenario, ElementSnapshot, FrameTimingStats,
    ImageAuditSummary, PageInspectionSnapshot, RectSnapshot, ScenarioReport,
    INSPECTED_CSS_PROPERTIES,
};
pub use page_loader::{
    load_page_subresources, CachedAsset, DecodedAssetCache, LoadedPageAssets, LoadedWebFont,
    PageLoadStats, SpeculativePreconnector,
};
pub use window::{
    populate_demo_layers, render_headless_demo, render_url_to_png, render_url_to_png_with_metrics,
    render_url_with_options, run_window_shell, AppWakeup, BrowserShell, RenderUrlMetrics,
    RenderUrlOptions, DEMO_CHROME_PIPELINE, DEMO_OVERLAY_PIPELINE, DEMO_WEB_PIPELINE,
};
