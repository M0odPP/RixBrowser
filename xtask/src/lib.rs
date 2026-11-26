#![deny(unsafe_code)]
//! Automated Visual Reftest, WPT (curated & official), html5lib, Test262, Chrome comparison,
//! and Criterion benchmark harness for RixBrowser.

pub mod bench;
pub mod compare_chrome;
pub mod compare_firefox;
pub mod diff_analyzer;
pub mod firefox_bidi;
pub mod html5lib;
pub mod reftest;
pub mod test262;
pub mod wpt;
pub mod wpt_official;
