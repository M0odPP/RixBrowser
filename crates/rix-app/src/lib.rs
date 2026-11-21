#![deny(unsafe_code)]

pub mod child_runtime;

pub use child_runtime::{run_content_process, run_network_process};
