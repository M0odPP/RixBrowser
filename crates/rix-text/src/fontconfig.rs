use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

const MAX_INCLUDE_DEPTH: usize = 8;
const MAX_CONFIG_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Default)]
struct AliasLists {
    prefer: Vec<String>,
    accept: Vec<String>,
    default: Vec<String>,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct FamilyAliases {
    families: HashMap<String, Vec<String>>,
}

impl FamilyAliases {
    pub fn from_system() -> Self {
        let root = std::env::var_os("FONTCONFIG_FILE")
            .map(PathBuf::from)
            .filter(|p| p.is_file())
            .unwrap_or_else(|| PathBuf::from("/etc/fonts/fonts.conf"));
        Self::from_config_file(&root)
    }

    pub fn from_config_file(path: &Path) -> Self {
        let mut aliases: HashMap<String, AliasLists> = HashMap::new();
        let mut visited = HashSet::new();
        load_path(path, 0, &mut visited, &mut aliases);
        let families = aliases
            .into_iter()
            .map(|(generic, lists)| {
                let mut seen = HashSet::new();
                let ordered = lists
                    .prefer
                    .into_iter()
                    .chain(lists.accept)
                    .chain(lists.default)
                    .filter(|family| seen.insert(family.to_lowercase()))
                    .collect();
                (generic, ordered)
            })
            .collect();
        Self { families }
    }

    pub fn families(&self, generic: &str) -> &[String] {
        self.families
            .get(&generic.to_ascii_lowercase())
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
}

fn load_path(
    path: &Path,
    depth: usize,
    visited: &mut HashSet<PathBuf>,
    aliases: &mut HashMap<String, AliasLists>,
) {
    if depth > MAX_INCLUDE_DEPTH {
        return;
    }
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if !visited.insert(canonical) {
        return;
    }
    if path.is_dir() {
        let Ok(entries) = std::fs::read_dir(path) else {
            return;
        };
        let mut files: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|file| file.extension().is_some_and(|ext| ext == "conf") && file.is_file())
            .collect();
        files.sort();
        for file in files {
            load_path(&file, depth + 1, visited, aliases);
        }
        return;
    }
    let too_large = std::fs::metadata(path).map_or(true, |meta| meta.len() > MAX_CONFIG_BYTES);
    if too_large {
        return;
    }
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    let options = roxmltree::ParsingOptions {
        allow_dtd: true,
        ..Default::default()
    };
    let Ok(document) = roxmltree::Document::parse_with_options(&text, options) else {
        tracing::debug!("Ignoring unparsable fontconfig file {}", path.display());
        return;
    };
    let base = path.parent().unwrap_or(Path::new("/"));
    for node in document
        .root_element()
        .children()
        .filter(|n| n.is_element())
    {
        match node.tag_name().name() {
            "include" => {
                if let Some(target) = include_target(node, base) {
                    load_path(&target, depth + 1, visited, aliases);
                }
            }
            "alias" => read_alias(node, aliases),
            "match" => read_match_alias(node, aliases),
            _ => {}
        }
    }
}

fn include_target(node: roxmltree::Node<'_, '_>, base: &Path) -> Option<PathBuf> {
    let text = node.text()?.trim();
    if text.is_empty() {
        return None;
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let target = match node.attribute("prefix") {
        Some("xdg") => std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| home.map(|home| home.join(".config")))?
            .join(text),
        _ if text.starts_with("~/") => home?.join(&text[2..]),
        _ if Path::new(text).is_absolute() => PathBuf::from(text),
        _ => base.join(text),
    };
    Some(target)
}

fn family_names(node: roxmltree::Node<'_, '_>) -> Vec<String> {
    node.children()
        .filter(|child| child.has_tag_name("family"))
        .filter_map(|child| child.text())
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
        .collect()
}

fn read_alias(node: roxmltree::Node<'_, '_>, aliases: &mut HashMap<String, AliasLists>) {
    let generics = family_names(node);
    let section = |name: &str| {
        node.children()
            .find(|child| child.has_tag_name(name))
            .map(family_names)
            .unwrap_or_default()
    };
    let (prefer, accept, default) = (section("prefer"), section("accept"), section("default"));
    for generic in generics {
        let lists = aliases.entry(generic.to_lowercase()).or_default();
        lists.prefer.extend(prefer.iter().cloned());
        lists.accept.extend(accept.iter().cloned());
        lists.default.extend(default.iter().cloned());
    }
}

fn read_match_alias(node: roxmltree::Node<'_, '_>, aliases: &mut HashMap<String, AliasLists>) {
    if node
        .attribute("target")
        .is_some_and(|target| target != "pattern")
    {
        return;
    }
    let mut tested_families = Vec::new();
    let mut family_test_count = 0usize;
    let mut has_non_family_or_negated_test = false;
    for test in node.children().filter(|c| c.has_tag_name("test")) {
        if test.attribute("name") != Some("family") {
            has_non_family_or_negated_test = true;
            continue;
        }
        if test
            .attribute("compare")
            .is_some_and(|cmp| !cmp.eq_ignore_ascii_case("eq"))
        {
            has_non_family_or_negated_test = true;
            continue;
        }
        family_test_count += 1;
        for s in test.children().filter(|c| c.has_tag_name("string")) {
            if let Some(txt) = s.text().map(str::trim).filter(|t| !t.is_empty()) {
                tested_families.push(txt.to_lowercase());
            }
        }
    }
    if tested_families.is_empty() || has_non_family_or_negated_test || family_test_count != 1 {
        return;
    }
    for edit in node.children().filter(|c| c.has_tag_name("edit")) {
        if edit.attribute("name") != Some("family") {
            continue;
        }
        let mode = edit.attribute("mode").unwrap_or("assign");
        let replacements: Vec<String> = edit
            .children()
            .filter(|c| c.has_tag_name("string"))
            .filter_map(|c| c.text())
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect();
        if replacements.is_empty() {
            continue;
        }
        for fam in &tested_families {
            let lists = aliases.entry(fam.clone()).or_default();
            match mode {
                "prepend" | "prepend_first" | "assign" | "assign_replace" => {
                    lists.prefer.extend(replacements.iter().cloned());
                }
                _ => {
                    lists.accept.extend(replacements.iter().cloned());
                }
            }
        }
    }
}
