//! Official WHATWG HTML5 (`html5lib`) tree-construction conformance runner
//! (`cargo xtask html5lib [--dir <path>] [--report-json <path>]`).
//!
//! Executes `.dat` test files from official `wpt/html/syntax/parsing/resources/`
//! against `rix_html::parse_html` / `rix_html::parse_html_fragment` and compares
//! the serialized DOM tree line-for-line with the official expected `#document` output.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use markup5ever::{LocalName, Namespace, QualName};
use rayon::prelude::*;
use rix_dom::{Document, NodeData, NodeId};
use rix_html::{parse_html_fragment_with_scripting, parse_html_with_scripting};
use serde::Serialize;

const HTML_NS: &str = "http://www.w3.org/1999/xhtml";
const SVG_NS: &str = "http://www.w3.org/2000/svg";
const MATHML_NS: &str = "http://www.w3.org/1998/Math/MathML";
const XLINK_NS: &str = "http://www.w3.org/1999/xlink";
const XML_NS: &str = "http://www.w3.org/XML/1998/namespace";
const XMLNS_NS: &str = "http://www.w3.org/2000/xmlns/";

#[derive(Debug, Clone)]
pub struct Html5libCase {
    pub file: String,
    pub index: usize,
    pub data: String,
    pub fragment_context: Option<String>,
    pub script_off: bool,
    pub expected_tree: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Html5libFileSummary {
    pub passed: usize,
    pub total: usize,
    pub skipped: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct Html5libFailure {
    pub file: String,
    pub index: usize,
    pub data: String,
    pub fragment_context: Option<String>,
    pub expected: String,
    pub actual: String,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct Html5libReport {
    pub total: usize,
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
    pub pass_rate_pct: f64,
    pub files: BTreeMap<String, Html5libFileSummary>,
    pub sample_failures: Vec<Html5libFailure>,
}

/// Default path for the local conformance cache of WPT `html5lib` `.dat` fixtures.
#[must_use]
pub fn default_html5lib_dir() -> PathBuf {
    if let Ok(base) = std::env::var("RIX_CONFORMANCE_DIR") {
        return PathBuf::from(base).join("wpt/html/syntax/parsing/resources");
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".cache/rix-conformance/wpt/html/syntax/parsing/resources")
}

/// Parses an official `html5lib` `.dat` tree-construction fixture file into test cases.
#[must_use]
pub fn parse_dat_file(file_name: &str, content: &str) -> Vec<Html5libCase> {
    let mut cases = Vec::new();
    let lines: Vec<&str> = content.split('\n').collect();
    let mut i = 0usize;
    let mut case_index = 0usize;

    while i < lines.len() {
        if lines[i] != "#data" {
            i += 1;
            continue;
        }
        i += 1;
        let mut data_lines = Vec::new();
        while i < lines.len() && !is_section_header(lines[i]) {
            data_lines.push(lines[i]);
            i += 1;
        }
        let data = data_lines.join("\n");

        let mut fragment_context: Option<String> = None;
        let mut script_off = false;
        let mut expected_lines: Vec<&str> = Vec::new();

        while i < lines.len() && lines[i] != "#data" {
            match lines[i] {
                "#errors" | "#new-errors" => {
                    i += 1;
                    while i < lines.len() && !is_section_header(lines[i]) {
                        i += 1;
                    }
                }
                "#script-on" => {
                    script_off = false;
                    i += 1;
                }
                "#script-off" => {
                    script_off = true;
                    i += 1;
                }
                "#document-fragment" => {
                    i += 1;
                    if i < lines.len() && !is_section_header(lines[i]) {
                        fragment_context = Some(lines[i].trim().to_string());
                        i += 1;
                    }
                    while i < lines.len() && !is_section_header(lines[i]) {
                        i += 1;
                    }
                }
                "#document" => {
                    i += 1;
                    while i < lines.len() && !is_section_header(lines[i]) {
                        expected_lines.push(lines[i]);
                        i += 1;
                    }
                }
                "#description" => {
                    i += 1;
                    while i < lines.len() && !is_section_header(lines[i]) {
                        i += 1;
                    }
                }
                _ => {
                    i += 1;
                }
            }
        }

        // Trim trailing empty separator lines between test cases.
        while expected_lines.last().is_some_and(|l| l.is_empty()) {
            expected_lines.pop();
        }
        if !expected_lines.is_empty() {
            cases.push(Html5libCase {
                file: file_name.to_string(),
                index: case_index,
                data,
                fragment_context,
                script_off,
                expected_tree: expected_lines.join("\n"),
            });
            case_index += 1;
        }
    }

    cases
}

fn is_section_header(line: &str) -> bool {
    matches!(
        line,
        "#data"
            | "#errors"
            | "#new-errors"
            | "#document-fragment"
            | "#script-on"
            | "#script-off"
            | "#document"
            | "#description"
    )
}

fn parse_fragment_context(spec: &str) -> QualName {
    if let Some(local) = spec.strip_prefix("svg ") {
        QualName::new(None, Namespace::from(SVG_NS), LocalName::from(local.trim()))
    } else if let Some(local) = spec.strip_prefix("math ") {
        QualName::new(
            None,
            Namespace::from(MATHML_NS),
            LocalName::from(local.trim()),
        )
    } else {
        QualName::new(None, Namespace::from(HTML_NS), LocalName::from(spec.trim()))
    }
}

/// Serializes a parsed `rix_dom::Document` subtree into official `html5lib` `#document` format.
#[must_use]
pub fn serialize_html5lib_tree(doc: &Document, roots: &[NodeId]) -> String {
    let mut out = Vec::new();
    for &node in roots {
        serialize_node(doc, node, 0, &mut out);
    }
    out.join("\n")
}

fn serialize_node(doc: &Document, node: NodeId, depth: usize, out: &mut Vec<String>) {
    let indent = "  ".repeat(depth);
    let Some(data) = doc.get(node) else {
        return;
    };
    match data {
        NodeData::Document | NodeData::DocumentFragment => {
            for child in doc.children(node) {
                serialize_node(doc, child, depth, out);
            }
        }
        NodeData::ShadowRoot { .. } => {
            out.push(format!("| {indent}#shadow-root"));
            for child in doc.children(node) {
                serialize_node(doc, child, depth + 1, out);
            }
        }
        NodeData::DocumentType {
            name,
            public_id,
            system_id,
        } => {
            if public_id.is_empty() && system_id.is_empty() {
                out.push(format!("| {indent}<!DOCTYPE {name}>"));
            } else {
                out.push(format!(
                    "| {indent}<!DOCTYPE {name} \"{public_id}\" \"{system_id}\">"
                ));
            }
        }
        NodeData::Text(text) | NodeData::CDataSection(text) => {
            out.push(format!("| {indent}\"{text}\""));
        }
        NodeData::Comment(text) => {
            out.push(format!("| {indent}<!-- {text} -->"));
        }
        NodeData::ProcessingInstruction { target, data } => {
            out.push(format!("| {indent}<?{target} {data}?>"));
        }
        NodeData::Element(elem) => {
            let prefix = match elem.name.ns.as_ref() {
                SVG_NS => "svg ",
                MATHML_NS => "math ",
                _ => "",
            };
            out.push(format!("| {indent}<{prefix}{}>", elem.name.local));

            let mut attrs: Vec<(String, &str)> = elem
                .attributes
                .iter()
                .map(|(k, v)| {
                    let attr_prefix = match k.ns.as_ref() {
                        XLINK_NS => "xlink ",
                        XML_NS => "xml ",
                        XMLNS_NS => "xmlns ",
                        _ => "",
                    };
                    (format!("{attr_prefix}{}", k.local), v.as_str())
                })
                .collect();
            attrs.sort_by(|a, b| a.0.cmp(&b.0));

            let attr_indent = "  ".repeat(depth + 1);
            for (k, v) in attrs {
                out.push(format!("| {attr_indent}{k}=\"{v}\""));
            }

            if &*elem.name.ns == HTML_NS && &*elem.name.local == "template" {
                if let Some(frag) = elem.template_contents {
                    out.push(format!("| {attr_indent}content"));
                    for child in doc.children(frag) {
                        serialize_node(doc, child, depth + 2, out);
                    }
                }
            }

            for child in doc.children(node) {
                serialize_node(doc, child, depth + 1, out);
            }
        }
    }
}

fn evaluate_case(case: &Html5libCase) -> std::result::Result<(), String> {
    let scripting = !case.script_off;
    let actual =
        if case.file.starts_with("scripted_") && case.fragment_context.is_none() && scripting {
            static FONT_DB: std::sync::OnceLock<std::sync::Arc<rix_text::FontDatabase>> =
                std::sync::OnceLock::new();
            let font_db = std::sync::Arc::clone(
                FONT_DB.get_or_init(|| std::sync::Arc::new(rix_text::FontDatabase::new())),
            );
            let doc_rc = std::rc::Rc::new(std::cell::RefCell::new(Document::new()));
            let mut rt = rix_content::ContentRuntime::new(
                std::rc::Rc::clone(&doc_rc),
                rix_core::Size::new(800.0, 600.0),
                font_db,
            );
            let _ = rt.load_html_with_scripts(&case.data);
            let doc = doc_rc.borrow();
            let roots: Vec<NodeId> = doc.children(doc.root()).collect();
            serialize_html5lib_tree(&doc, &roots)
        } else if let Some(ref ctx_str) = case.fragment_context {
            let ctx = parse_fragment_context(ctx_str);
            let (doc, nodes) =
                parse_html_fragment_with_scripting(&case.data, Some(&ctx), scripting);
            serialize_html5lib_tree(&doc, &nodes)
        } else {
            let doc = parse_html_with_scripting(&case.data, scripting);
            let roots: Vec<NodeId> = doc.children(doc.root()).collect();
            serialize_html5lib_tree(&doc, &roots)
        };

    if actual == case.expected_tree {
        Ok(())
    } else {
        Err(actual)
    }
}

/// Runs all official `html5lib` `.dat` tree-construction suites in `dir`.
pub fn run_html5lib_suite(
    dir: &Path,
    filter: Option<&str>,
    report_json: Option<&Path>,
) -> Result<Html5libReport> {
    if !dir.exists() {
        bail!(
            "html5lib test directory not found: {} (set --dir or clone WPT to ~/.cache/rix-conformance/wpt)",
            dir.display()
        );
    }

    let mut dat_files: Vec<PathBuf> = fs::read_dir(dir)
        .with_context(|| format!("read {}", dir.display()))?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("dat"))
        .collect();
    dat_files.sort();

    let mut all_cases = Vec::new();
    for path in &dat_files {
        let fname = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown.dat");
        if let Some(flt) = filter {
            if !fname.contains(flt) {
                continue;
            }
        }
        let raw = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        all_cases.extend(parse_dat_file(fname, &raw));
    }

    // Evaluate all cases (both #script-on and #script-off) in parallel with panic isolation.
    let outcomes: Vec<(&Html5libCase, Option<std::result::Result<(), String>>)> = all_cases
        .par_iter()
        .map(|case| {
            let res = std::panic::catch_unwind(|| evaluate_case(case))
                .unwrap_or_else(|_| Err("<PANIC during parse_html>".to_string()));
            (case, Some(res))
        })
        .collect();

    let mut report = Html5libReport::default();
    for (case, outcome) in outcomes {
        let entry = report
            .files
            .entry(case.file.clone())
            .or_insert(Html5libFileSummary {
                passed: 0,
                total: 0,
                skipped: 0,
            });
        match outcome {
            None => {
                report.skipped += 1;
                entry.skipped += 1;
            }
            Some(Ok(())) => {
                report.total += 1;
                report.passed += 1;
                entry.total += 1;
                entry.passed += 1;
            }
            Some(Err(actual)) => {
                report.total += 1;
                report.failed += 1;
                entry.total += 1;
                if report.sample_failures.len() < 25 {
                    report.sample_failures.push(Html5libFailure {
                        file: case.file.clone(),
                        index: case.index,
                        data: case.data.clone(),
                        fragment_context: case.fragment_context.clone(),
                        expected: case.expected_tree.clone(),
                        actual,
                    });
                }
            }
        }
    }

    report.pass_rate_pct = if report.total == 0 {
        100.0
    } else {
        (report.passed as f64 / report.total as f64) * 100.0
    };

    if let Some(json_path) = report_json {
        if let Some(parent) = json_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(json_path, serde_json::to_string_pretty(&report)? + "\n")?;
    }

    Ok(report)
}

pub fn print_report(report: &Html5libReport) {
    println!("=== Official WHATWG HTML5 (html5lib) Tree-Construction Suite ===");
    println!(
        "  Passed: {} / {} ({:.2}%) | Failed: {} | Skipped (#script-off): {}",
        report.passed, report.total, report.pass_rate_pct, report.failed, report.skipped
    );
    let mut imperfect: Vec<(&String, &Html5libFileSummary)> = report
        .files
        .iter()
        .filter(|(_, s)| s.passed < s.total)
        .collect();
    imperfect.sort_by_key(|a| std::cmp::Reverse(a.1.total - a.1.passed));
    if !imperfect.is_empty() {
        println!(
            "  Files with failures ({} / {} files):",
            imperfect.len(),
            report.files.len()
        );
        for (file, s) in imperfect.iter().take(15) {
            println!("    {:<34} {:>4}/{:<4}", file, s.passed, s.total);
        }
    }
    if !report.sample_failures.is_empty() {
        println!(
            "  Sample failures (first {}):",
            report.sample_failures.len().min(8)
        );
        for f in report.sample_failures.iter().take(8) {
            let data_preview: String = f.data.chars().take(60).collect();
            println!(
                "    {} #{} (ctx={:?}) data={:?}",
                f.file, f.index, f.fragment_context, data_preview
            );
        }
    }
}
