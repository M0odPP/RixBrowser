//! WHATWG `URLPattern` Standard implementation (`new URLPattern(input, baseURL, options)`).
//!
//! Supports string shorthand and `URLPatternInit` dictionary inputs, base URL inheritance,
//! named (`:name`), custom regex (`:name(regex)`), unnamed (`(regex)`), wildcard (`*`),
//! optional/repeating modifiers (`?`, `+`, `*`), and grouped prefix/suffix segments
//! (`{/:name}?`), plus `.test()`, `.exec()`, and component getters.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use regex::Regex;
use rix_js::{HeapHandle, JsException, JsObject, JsValue, JsVm, ObjectKind, PropertyAttrs};
use url::Url;

const COMPONENT_NAMES: [&str; 8] = [
    "protocol", "username", "password", "hostname", "port", "pathname", "search", "hash",
];

type ComponentExecMatch = (&'static str, String, Vec<(String, Option<String>)>);

#[derive(Debug, Clone)]
struct CompiledComponent {
    pattern: String,
    regex: Regex,
    group_names: Vec<String>,
    has_regexp_groups: bool,
}

#[derive(Debug, Clone)]
pub struct UrlPatternRecord {
    protocol: CompiledComponent,
    username: CompiledComponent,
    password: CompiledComponent,
    hostname: CompiledComponent,
    port: CompiledComponent,
    pathname: CompiledComponent,
    search: CompiledComponent,
    hash: CompiledComponent,
    has_regexp_groups: bool,
}

#[derive(Debug, Clone, Default)]
struct PatternInit {
    protocol: Option<String>,
    username: Option<String>,
    password: Option<String>,
    hostname: Option<String>,
    port: Option<String>,
    pathname: Option<String>,
    search: Option<String>,
    hash: Option<String>,
    base_url: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct UrlComponents {
    protocol: String,
    username: String,
    password: String,
    hostname: String,
    port: String,
    pathname: String,
    search: String,
    hash: String,
}

fn default_port_for_scheme(scheme: &str) -> Option<&'static str> {
    match scheme {
        "http" | "ws" => Some("80"),
        "https" | "wss" => Some("443"),
        "ftp" => Some("21"),
        _ => None,
    }
}

fn is_special_scheme(scheme: &str) -> bool {
    matches!(scheme, "http" | "https" | "ws" | "wss" | "ftp" | "file")
}

fn canonicalize_protocol(raw: &str) -> String {
    raw.trim_end_matches(':').to_ascii_lowercase()
}

fn canonicalize_port(raw: &str, protocol: &str) -> String {
    let trimmed = raw.trim();
    if default_port_for_scheme(protocol) == Some(trimmed) {
        String::new()
    } else {
        trimmed.to_string()
    }
}

fn canonicalize_search(raw: &str) -> String {
    raw.strip_prefix('?').unwrap_or(raw).to_string()
}

fn canonicalize_hash(raw: &str) -> String {
    raw.strip_prefix('#').unwrap_or(raw).to_string()
}

fn components_from_url(u: &Url) -> UrlComponents {
    let protocol = u.scheme().to_ascii_lowercase();
    let port = u
        .port()
        .map(|p| canonicalize_port(&p.to_string(), &protocol))
        .unwrap_or_default();
    UrlComponents {
        protocol,
        username: u.username().to_string(),
        password: u.password().unwrap_or("").to_string(),
        hostname: u.host_str().unwrap_or("").to_ascii_lowercase(),
        port,
        pathname: u.path().to_string(),
        search: u.query().unwrap_or("").to_string(),
        hash: u.fragment().unwrap_or("").to_string(),
    }
}

/// Parses a WHATWG URLPattern constructor string into a [`PatternInit`].
fn parse_constructor_string(input: &str, base_url: Option<&str>) -> Result<PatternInit, String> {
    let mut init = PatternInit {
        base_url: base_url.map(str::to_owned),
        ..PatternInit::default()
    };

    // Strip `#hash` (respecting balanced `{...}` and `(...)`).
    let (rest, hash) = split_top_level(input, '#');
    if let Some(h) = hash {
        init.hash = Some(h.to_string());
    }
    // Strip `?search` (only when not immediately following a pattern group/modifier).
    let (rest, search) = split_query_top_level(rest);
    if let Some(q) = search {
        init.search = Some(q.to_string());
    }

    // Check for `protocol:` prefix at top level.
    let mut after_proto = rest;
    if let Some(colon_idx) = find_protocol_colon(rest) {
        init.protocol = Some(rest[..colon_idx].to_string());
        after_proto = &rest[colon_idx + 1..];
    }

    if let Some(after_slashes) = after_proto.strip_prefix("//") {
        // Authority + optional pathname.
        let slash_idx = find_top_level_char(after_slashes, '/');
        let (authority, path_part) = match slash_idx {
            Some(i) => (&after_slashes[..i], Some(&after_slashes[i..])),
            None => (after_slashes, None),
        };
        let (userinfo, hostport) = match find_top_level_char(authority, '@') {
            Some(i) => (Some(&authority[..i]), &authority[i + 1..]),
            None => (None, authority),
        };
        if let Some(ui) = userinfo {
            match find_top_level_char(ui, ':') {
                Some(i) => {
                    init.username = Some(ui[..i].to_string());
                    init.password = Some(ui[i + 1..].to_string());
                }
                None => {
                    init.username = Some(ui.to_string());
                }
            }
        }
        let (host, port) = split_host_port(hostport);
        init.hostname = Some(host.to_string());
        if let Some(p) = port {
            init.port = Some(p.to_string());
        } else {
            init.port = Some(String::new());
        }
        if let Some(p) = path_part {
            init.pathname = Some(p.to_string());
        } else if init
            .protocol
            .as_deref()
            .is_some_and(|pr| is_special_scheme(&canonicalize_protocol(pr)))
        {
            init.pathname = Some("/".to_string());
        }
    } else if !after_proto.is_empty() {
        init.pathname = Some(after_proto.to_string());
    } else if init.protocol.is_none() && init.search.is_none() && init.hash.is_none() {
        init.pathname = Some(String::new());
    }

    Ok(init)
}

fn find_protocol_colon(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut paren = 0u32;
    let mut brace = 0u32;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\\' {
            i += 2;
            continue;
        }
        match b {
            b'(' => paren += 1,
            b')' => paren = paren.saturating_sub(1),
            b'{' => brace += 1,
            b'}' => brace = brace.saturating_sub(1),
            b'/' | b'?' | b'#' if paren == 0 && brace == 0 => return None,
            b':' if paren == 0 && brace == 0 => {
                // Distinguish `protocol:` from named group `:name` (no leading token before `:`).
                if i == 0 {
                    return None;
                }
                // Check if prefix looks like a valid scheme or pattern (not starting with `/`).
                return Some(i);
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn find_top_level_char(s: &str, needle: char) -> Option<usize> {
    let mut paren = 0u32;
    let mut brace = 0u32;
    let mut bracket = 0u32;
    let mut escaped = false;
    for (idx, ch) in s.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        match ch {
            '(' => paren += 1,
            ')' => paren = paren.saturating_sub(1),
            '{' => brace += 1,
            '}' => brace = brace.saturating_sub(1),
            '[' => bracket += 1,
            ']' => bracket = bracket.saturating_sub(1),
            c if c == needle && paren == 0 && brace == 0 && bracket == 0 => return Some(idx),
            _ => {}
        }
    }
    None
}

fn split_top_level(s: &str, needle: char) -> (&str, Option<&str>) {
    match find_top_level_char(s, needle) {
        Some(i) => (&s[..i], Some(&s[i + 1..])),
        None => (s, None),
    }
}

/// Splits `?search` at top-level, ignoring `?` used as a pattern quantifier immediately
/// after `)`, `}`, `*`, or a `:name` identifier.
fn split_query_top_level(s: &str) -> (&str, Option<&str>) {
    let bytes = s.as_bytes();
    let mut paren = 0u32;
    let mut brace = 0u32;
    let mut in_named_group = false;
    let mut prev_is_group_end = false;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\\' {
            i += 2;
            prev_is_group_end = false;
            in_named_group = false;
            continue;
        }
        if paren == 0 && brace == 0 {
            if b == b'?' {
                if prev_is_group_end || in_named_group {
                    prev_is_group_end = false;
                    in_named_group = false;
                    i += 1;
                    continue;
                }
                return (&s[..i], Some(&s[i + 1..]));
            }
            if b == b':' {
                in_named_group = true;
                prev_is_group_end = false;
            } else if in_named_group && (b.is_ascii_alphanumeric() || b == b'_') {
                // stay in named group
            } else {
                in_named_group = false;
                prev_is_group_end = b == b'*';
            }
        }
        match b {
            b'(' => {
                paren += 1;
                prev_is_group_end = false;
                in_named_group = false;
            }
            b')' => {
                paren = paren.saturating_sub(1);
                if paren == 0 && brace == 0 {
                    prev_is_group_end = true;
                }
            }
            b'{' => {
                brace += 1;
                prev_is_group_end = false;
                in_named_group = false;
            }
            b'}' => {
                brace = brace.saturating_sub(1);
                if paren == 0 && brace == 0 {
                    prev_is_group_end = true;
                }
            }
            _ => {}
        }
        i += 1;
    }
    (s, None)
}

fn split_host_port(hostport: &str) -> (&str, Option<&str>) {
    let mut paren = 0u32;
    let mut brace = 0u32;
    let mut bracket = 0u32;
    let mut escaped = false;
    for (idx, ch) in hostport.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        match ch {
            '(' => paren += 1,
            ')' => paren = paren.saturating_sub(1),
            '{' => brace += 1,
            '}' => brace = brace.saturating_sub(1),
            '[' => bracket += 1,
            ']' => bracket = bracket.saturating_sub(1),
            ':' if paren == 0 && brace == 0 && bracket == 0 => {
                // Ensure `:` is a host:port separator and not a `:name` pattern inside hostname.
                let after = &hostport[idx + 1..];
                if after.is_empty()
                    || after.starts_with('*')
                    || after.starts_with('(')
                    || after.starts_with('{')
                    || after.chars().next().is_some_and(|c| c.is_ascii_digit())
                {
                    return (&hostport[..idx], Some(after));
                }
            }
            _ => {}
        }
    }
    (hostport, None)
}

fn escape_pattern_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if matches!(ch, '\\' | '+' | '*' | '?' | ':' | '{' | '}' | '(' | ')') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// Compiles a single WHATWG URLPattern component string into a Rust `Regex` and capture group list.
fn compile_component(
    raw_pattern: &str,
    segment_delim: Option<char>,
    ignore_case: bool,
) -> Result<CompiledComponent, String> {
    let mut regex_src = String::from(if ignore_case { "^(?is)" } else { "^(?s)" });
    let mut group_names: Vec<String> = Vec::new();
    let mut anon_counter: usize = 0;
    let mut has_regexp_groups = false;

    compile_tokens(
        raw_pattern,
        segment_delim,
        &mut regex_src,
        &mut group_names,
        &mut anon_counter,
        &mut has_regexp_groups,
    )?;
    regex_src.push('$');

    let regex =
        Regex::new(&regex_src).map_err(|e| format!("Invalid URLPattern '{raw_pattern}': {e}"))?;
    Ok(CompiledComponent {
        pattern: raw_pattern.to_string(),
        regex,
        group_names,
        has_regexp_groups,
    })
}

fn compile_tokens(
    src: &str,
    segment_delim: Option<char>,
    out: &mut String,
    group_names: &mut Vec<String>,
    anon_counter: &mut usize,
    has_regexp_groups: &mut bool,
) -> Result<(), String> {
    let chars: Vec<char> = src.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '\\' => {
                i += 1;
                if i >= chars.len() {
                    return Err("Trailing backslash in URLPattern".into());
                }
                push_regex_escaped_char(chars[i], out);
                i += 1;
            }
            '{' => {
                let start = i + 1;
                let mut depth = 1u32;
                i += 1;
                while i < chars.len() && depth > 0 {
                    if chars[i] == '\\' {
                        i += 2;
                        continue;
                    }
                    if chars[i] == '{' {
                        depth += 1;
                    } else if chars[i] == '}' {
                        depth -= 1;
                    }
                    i += 1;
                }
                if depth != 0 {
                    return Err("Unclosed '{' group in URLPattern".into());
                }
                let inner: String = chars[start..i - 1].iter().collect();
                out.push_str("(?:");
                compile_tokens(
                    &inner,
                    segment_delim,
                    out,
                    group_names,
                    anon_counter,
                    has_regexp_groups,
                )?;
                out.push(')');
                if i < chars.len() && matches!(chars[i], '?' | '+' | '*') {
                    out.push(chars[i]);
                    i += 1;
                }
            }
            ':' => {
                i += 1;
                let name_start = i;
                while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                if i == name_start {
                    return Err("Empty named group ':' in URLPattern".into());
                }
                let name: String = chars[name_start..i].iter().collect();
                let mut custom_re: Option<String> = None;
                if i < chars.len() && chars[i] == '(' {
                    let (re_body, next_i) = read_balanced_parens(&chars, i)?;
                    custom_re = Some(re_body);
                    *has_regexp_groups = true;
                    i = next_i;
                }
                let modifier = if i < chars.len() && matches!(chars[i], '?' | '+' | '*') {
                    let m = chars[i];
                    i += 1;
                    Some(m)
                } else {
                    None
                };
                let body = custom_re.unwrap_or_else(|| match segment_delim {
                    Some('/') => "[^/]+?".to_string(),
                    Some('.') => "[^.]+?".to_string(),
                    Some(d) => format!("[^{}]+?", regex::escape(&d.to_string())),
                    None => ".+?".to_string(),
                });
                group_names.push(name);
                match modifier {
                    Some('?') => {
                        out.push_str("(?:(");
                        out.push_str(&body);
                        out.push_str("))?");
                    }
                    Some('+') => {
                        out.push('(');
                        out.push_str(&body);
                        if let Some(d) = segment_delim {
                            out.push_str("(?:");
                            push_regex_escaped_char(d, out);
                            out.push_str(&body);
                            out.push_str(")*");
                        } else {
                            out.push('+');
                        }
                        out.push(')');
                    }
                    Some('*') => {
                        out.push_str("(?:(");
                        out.push_str(&body);
                        if let Some(d) = segment_delim {
                            out.push_str("(?:");
                            push_regex_escaped_char(d, out);
                            out.push_str(&body);
                            out.push_str(")*");
                        }
                        out.push_str("))?");
                    }
                    _ => {
                        out.push('(');
                        out.push_str(&body);
                        out.push(')');
                    }
                }
            }
            '(' => {
                let (re_body, next_i) = read_balanced_parens(&chars, i)?;
                *has_regexp_groups = true;
                i = next_i;
                let modifier = if i < chars.len() && matches!(chars[i], '?' | '+' | '*') {
                    let m = chars[i];
                    i += 1;
                    Some(m)
                } else {
                    None
                };
                let gname = anon_counter.to_string();
                *anon_counter += 1;
                group_names.push(gname);
                match modifier {
                    Some('?') => {
                        out.push_str("(?:(");
                        out.push_str(&re_body);
                        out.push_str("))?");
                    }
                    Some(m) => {
                        out.push('(');
                        out.push_str(&re_body);
                        out.push(')');
                        out.push(m);
                    }
                    None => {
                        out.push('(');
                        out.push_str(&re_body);
                        out.push(')');
                    }
                }
            }
            '*' => {
                let gname = anon_counter.to_string();
                *anon_counter += 1;
                group_names.push(gname);
                out.push_str("(.*)");
                i += 1;
            }
            ch => {
                push_regex_escaped_char(ch, out);
                i += 1;
            }
        }
    }
    Ok(())
}

fn read_balanced_parens(chars: &[char], open_idx: usize) -> Result<(String, usize), String> {
    let mut i = open_idx + 1;
    let start = i;
    let mut depth = 1u32;
    let mut in_class = false;
    while i < chars.len() && depth > 0 {
        if chars[i] == '\\' {
            i += 2;
            continue;
        }
        match chars[i] {
            '[' => in_class = true,
            ']' => in_class = false,
            '(' if !in_class => depth += 1,
            ')' if !in_class => depth -= 1,
            _ => {}
        }
        i += 1;
    }
    if depth != 0 {
        return Err("Unclosed '(' regex group in URLPattern".into());
    }
    let body: String = chars[start..i - 1].iter().collect();
    Ok((body, i))
}

fn push_regex_escaped_char(ch: char, out: &mut String) {
    if matches!(
        ch,
        '\\' | '.' | '+' | '*' | '?' | '(' | ')' | '|' | '[' | ']' | '{' | '}' | '^' | '$'
    ) {
        out.push('\\');
    }
    out.push(ch);
}

impl UrlPatternRecord {
    fn from_init(init: PatternInit, ignore_case: bool) -> Result<Self, String> {
        let base_comp = if let Some(ref b) = init.base_url {
            let parsed = Url::parse(b).map_err(|e| format!("Invalid baseURL '{b}': {e}"))?;
            Some(components_from_url(&parsed))
        } else {
            None
        };

        let had_pathname = init.pathname.is_some();
        let had_search = init.search.is_some();
        let mut p_proto = init.protocol.map(|s| canonicalize_protocol(&s));
        let mut p_user = init.username;
        let mut p_pass = init.password;
        let mut p_host = init.hostname.map(|s| s.to_ascii_lowercase());
        let mut p_port = init.port;
        let mut p_path = init.pathname;
        let mut p_search = init.search.map(|s| canonicalize_search(&s));
        let mut p_hash = init.hash.map(|s| canonicalize_hash(&s));

        if let Some(bc) = base_comp {
            if p_proto.is_none() {
                p_proto = Some(escape_pattern_literal(&bc.protocol));
            }
            if p_user.is_none() && p_host.is_none() && p_port.is_none() {
                p_user = Some(escape_pattern_literal(&bc.username));
                p_pass = Some(escape_pattern_literal(&bc.password));
            }
            if p_host.is_none() {
                p_host = Some(escape_pattern_literal(&bc.hostname));
            }
            if p_port.is_none() {
                p_port = Some(escape_pattern_literal(&bc.port));
            }
            if p_path.is_none() {
                p_path = Some(escape_pattern_literal(&bc.pathname));
            } else if let Some(ref path_val) = p_path {
                if !path_val.starts_with('/') && !path_val.starts_with('*') && !path_val.is_empty()
                {
                    let base_dir = match bc.pathname.rfind('/') {
                        Some(i) => &bc.pathname[..=i],
                        None => "/",
                    };
                    p_path = Some(format!("{}{path_val}", escape_pattern_literal(base_dir)));
                }
            }
            if p_search.is_none() && !had_pathname && p_hash.is_none() {
                p_search = Some(escape_pattern_literal(&bc.search));
            }
            if p_hash.is_none() && !had_pathname && !had_search {
                p_hash = Some(escape_pattern_literal(&bc.hash));
            }
        }

        let proto_str = p_proto.unwrap_or_else(|| "*".to_string());
        let port_str = p_port
            .map(|p| canonicalize_port(&p, &proto_str))
            .unwrap_or_else(|| "*".to_string());

        let protocol = compile_component(&proto_str, None, true)?;
        let username = compile_component(&p_user.unwrap_or_else(|| "*".to_string()), None, false)?;
        let password = compile_component(&p_pass.unwrap_or_else(|| "*".to_string()), None, false)?;
        let hostname =
            compile_component(&p_host.unwrap_or_else(|| "*".to_string()), Some('.'), true)?;
        let port = compile_component(&port_str, None, false)?;
        let pathname = compile_component(
            &p_path.unwrap_or_else(|| "*".to_string()),
            Some('/'),
            ignore_case,
        )?;
        let search = compile_component(
            &p_search.unwrap_or_else(|| "*".to_string()),
            None,
            ignore_case,
        )?;
        let hash = compile_component(
            &p_hash.unwrap_or_else(|| "*".to_string()),
            None,
            ignore_case,
        )?;

        let has_regexp_groups = protocol.has_regexp_groups
            || username.has_regexp_groups
            || password.has_regexp_groups
            || hostname.has_regexp_groups
            || port.has_regexp_groups
            || pathname.has_regexp_groups
            || search.has_regexp_groups
            || hash.has_regexp_groups;

        Ok(Self {
            protocol,
            username,
            password,
            hostname,
            port,
            pathname,
            search,
            hash,
            has_regexp_groups,
        })
    }

    fn component_by_name(&self, name: &str) -> Option<&CompiledComponent> {
        match name {
            "protocol" => Some(&self.protocol),
            "username" => Some(&self.username),
            "password" => Some(&self.password),
            "hostname" => Some(&self.hostname),
            "port" => Some(&self.port),
            "pathname" => Some(&self.pathname),
            "search" => Some(&self.search),
            "hash" => Some(&self.hash),
            _ => None,
        }
    }

    fn exec_components(&self, comp: &UrlComponents) -> Option<Vec<ComponentExecMatch>> {
        let pairs: [(&'static str, &CompiledComponent, &str); 8] = [
            ("protocol", &self.protocol, &comp.protocol),
            ("username", &self.username, &comp.username),
            ("password", &self.password, &comp.password),
            ("hostname", &self.hostname, &comp.hostname),
            ("port", &self.port, &comp.port),
            ("pathname", &self.pathname, &comp.pathname),
            ("search", &self.search, &comp.search),
            ("hash", &self.hash, &comp.hash),
        ];
        let mut results = Vec::with_capacity(8);
        for (name, compiled, input_str) in pairs {
            let caps = compiled.regex.captures(input_str)?;
            let mut groups = Vec::with_capacity(compiled.group_names.len());
            for (idx, gname) in compiled.group_names.iter().enumerate() {
                let val = caps.get(idx + 1).map(|m| m.as_str().to_string());
                groups.push((gname.clone(), val));
            }
            results.push((name, input_str.to_string(), groups));
        }
        Some(results)
    }
}

fn read_opt_string_prop(
    vm: &mut JsVm,
    obj: HeapHandle,
    prop: &str,
) -> Result<Option<String>, JsException> {
    let key = vm.heap.intern_key(prop);
    let val = vm.get_property_by_key(obj, key)?;
    if val.is_undefined() {
        Ok(None)
    } else {
        Ok(Some(vm.value_to_rust_string(val)))
    }
}

fn read_pattern_init_dict(vm: &mut JsVm, obj: HeapHandle) -> Result<PatternInit, JsException> {
    Ok(PatternInit {
        protocol: read_opt_string_prop(vm, obj, "protocol")?,
        username: read_opt_string_prop(vm, obj, "username")?,
        password: read_opt_string_prop(vm, obj, "password")?,
        hostname: read_opt_string_prop(vm, obj, "hostname")?,
        port: read_opt_string_prop(vm, obj, "port")?,
        pathname: read_opt_string_prop(vm, obj, "pathname")?,
        search: read_opt_string_prop(vm, obj, "search")?,
        hash: read_opt_string_prop(vm, obj, "hash")?,
        base_url: read_opt_string_prop(vm, obj, "baseURL")?,
    })
}

fn resolve_input_to_components(
    vm: &mut JsVm,
    args: &[JsValue],
) -> Result<Option<UrlComponents>, JsException> {
    let arg0 = args.first().copied().unwrap_or(JsValue::UNDEFINED);
    let arg1 = args.get(1).copied().unwrap_or(JsValue::UNDEFINED);

    if let Some(obj) = arg0.as_object() {
        if !arg1.is_undefined() {
            return Err(vm.type_error(
                "URLPattern: baseURL cannot be specified when input is a URLPatternInit dictionary",
            ));
        }
        let init = read_pattern_init_dict(vm, obj)?;
        let mut comp = if let Some(ref base) = init.base_url {
            match Url::parse(base) {
                Ok(u) => components_from_url(&u),
                Err(_) => return Ok(None),
            }
        } else {
            UrlComponents::default()
        };
        if let Some(v) = init.protocol {
            comp.protocol = canonicalize_protocol(&v);
        }
        if let Some(v) = init.username {
            comp.username = v;
        }
        if let Some(v) = init.password {
            comp.password = v;
        }
        if let Some(v) = init.hostname {
            comp.hostname = v.to_ascii_lowercase();
        }
        if let Some(v) = init.port {
            comp.port = canonicalize_port(&v, &comp.protocol);
        }
        if let Some(v) = init.pathname {
            comp.pathname = v;
        }
        if let Some(v) = init.search {
            comp.search = canonicalize_search(&v);
        }
        if let Some(v) = init.hash {
            comp.hash = canonicalize_hash(&v);
        }
        return Ok(Some(comp));
    }

    if arg0.is_undefined() {
        return Ok(Some(UrlComponents::default()));
    }
    let input_str = vm.value_to_rust_string(arg0);
    let parsed = if !arg1.is_undefined() {
        let base_str = vm.value_to_rust_string(arg1);
        let Ok(base) = Url::parse(&base_str) else {
            return Ok(None);
        };
        match base.join(&input_str) {
            Ok(u) => u,
            Err(_) => return Ok(None),
        }
    } else {
        match Url::parse(&input_str) {
            Ok(u) => u,
            Err(_) => return Ok(None),
        }
    };
    Ok(Some(components_from_url(&parsed)))
}

/// Installs the global WHATWG `URLPattern` constructor and prototype on `vm`.
pub(crate) fn install_url_pattern(vm: &mut JsVm) {
    let store: Rc<RefCell<HashMap<HeapHandle, UrlPatternRecord>>> =
        Rc::new(RefCell::new(HashMap::new()));

    let proto = vm.alloc_ordinary_object();
    if let Some(o) = vm.heap.get_object_mut(proto) {
        o.is_prototype = true;
    }

    // Readonly getters for the 8 components + `hasRegExpGroups`.
    for &comp_name in &COMPONENT_NAMES {
        let st = Rc::clone(&store);
        let getter_name = format!("get {comp_name}");
        let getter = vm.alloc_host_fn(&getter_name, move |vm, this_val, _| {
            let Some(h) = this_val.as_object() else {
                return Err(vm.type_error("Incompatible URLPattern receiver"));
            };
            let pat = st
                .borrow()
                .get(&h)
                .and_then(|r| r.component_by_name(comp_name))
                .map(|c| c.pattern.clone())
                .ok_or_else(|| vm.type_error("Incompatible URLPattern receiver"))?;
            Ok(JsValue::from_string(vm.heap.alloc_string(&pat)))
        });
        let key = vm.heap.intern_key(comp_name);
        vm.heap.object_define_accessor(
            proto,
            key,
            Some(getter),
            None,
            PropertyAttrs::CONFIGURABLE,
            &mut vm.shapes,
        );
    }
    {
        let st = Rc::clone(&store);
        let getter = vm.alloc_host_fn("get hasRegExpGroups", move |vm, this_val, _| {
            let Some(h) = this_val.as_object() else {
                return Err(vm.type_error("Incompatible URLPattern receiver"));
            };
            let flag = st
                .borrow()
                .get(&h)
                .map(|r| r.has_regexp_groups)
                .ok_or_else(|| vm.type_error("Incompatible URLPattern receiver"))?;
            Ok(JsValue::from_bool(flag))
        });
        let key = vm.heap.intern_key("hasRegExpGroups");
        vm.heap.object_define_accessor(
            proto,
            key,
            Some(getter),
            None,
            PropertyAttrs::CONFIGURABLE,
            &mut vm.shapes,
        );
    }

    // `URLPattern.prototype.test(input, baseURL)`
    {
        let st = Rc::clone(&store);
        let f = vm.alloc_host_fn("test", move |vm, this_val, args| {
            let Some(h) = this_val.as_object() else {
                return Err(vm.type_error("Incompatible URLPattern receiver"));
            };
            let rec = st
                .borrow()
                .get(&h)
                .cloned()
                .ok_or_else(|| vm.type_error("Incompatible URLPattern receiver"))?;
            let Some(comp) = resolve_input_to_components(vm, args)? else {
                return Ok(JsValue::FALSE);
            };
            Ok(JsValue::from_bool(rec.exec_components(&comp).is_some()))
        });
        let key = vm.heap.intern_key("test");
        vm.heap.object_define_data(
            proto,
            key,
            JsValue::from_object(f),
            PropertyAttrs::DEFAULT,
            &mut vm.shapes,
        );
    }

    // `URLPattern.prototype.exec(input, baseURL)`
    {
        let st = Rc::clone(&store);
        let f = vm.alloc_host_fn("exec", move |vm, this_val, args| {
            let Some(h) = this_val.as_object() else {
                return Err(vm.type_error("Incompatible URLPattern receiver"));
            };
            let rec = st
                .borrow()
                .get(&h)
                .cloned()
                .ok_or_else(|| vm.type_error("Incompatible URLPattern receiver"))?;
            let Some(comp) = resolve_input_to_components(vm, args)? else {
                return Ok(JsValue::NULL);
            };
            let Some(matched) = rec.exec_components(&comp) else {
                return Ok(JsValue::NULL);
            };

            let res_obj = vm.alloc_ordinary_object();
            let inputs_arr = vm.alloc_array();
            for &a in args.iter().take(2) {
                if !a.is_undefined() {
                    vm.heap.object_push_element(inputs_arr, a);
                }
            }
            let k_inputs = vm.heap.intern_key("inputs");
            vm.heap.object_define_data(
                res_obj,
                k_inputs,
                JsValue::from_object(inputs_arr),
                PropertyAttrs::DEFAULT,
                &mut vm.shapes,
            );
            let k_input = vm.heap.intern_key("input");
            let k_groups = vm.heap.intern_key("groups");
            for (cname, input_str, groups) in matched {
                let comp_obj = vm.alloc_ordinary_object();
                let inp_val = JsValue::from_string(vm.heap.alloc_string(&input_str));
                vm.heap.object_define_data(
                    comp_obj,
                    k_input,
                    inp_val,
                    PropertyAttrs::DEFAULT,
                    &mut vm.shapes,
                );
                let grp_obj = vm.alloc_ordinary_object();
                for (gname, gval) in groups {
                    let gk = vm.heap.intern_key(&gname);
                    let gv = match gval {
                        Some(s) => JsValue::from_string(vm.heap.alloc_string(&s)),
                        None => JsValue::UNDEFINED,
                    };
                    vm.heap.object_define_data(
                        grp_obj,
                        gk,
                        gv,
                        PropertyAttrs::DEFAULT,
                        &mut vm.shapes,
                    );
                }
                vm.heap.object_define_data(
                    comp_obj,
                    k_groups,
                    JsValue::from_object(grp_obj),
                    PropertyAttrs::DEFAULT,
                    &mut vm.shapes,
                );
                let ck = vm.heap.intern_key(cname);
                vm.heap.object_define_data(
                    res_obj,
                    ck,
                    JsValue::from_object(comp_obj),
                    PropertyAttrs::DEFAULT,
                    &mut vm.shapes,
                );
            }
            Ok(JsValue::from_object(res_obj))
        });
        let key = vm.heap.intern_key("exec");
        vm.heap.object_define_data(
            proto,
            key,
            JsValue::from_object(f),
            PropertyAttrs::DEFAULT,
            &mut vm.shapes,
        );
    }

    // `new URLPattern(input, baseURL, options)` constructor
    let st = Rc::clone(&store);
    let ctor = vm.alloc_host_fn("URLPattern", move |vm, _this_val, args| {
        let arg0 = args.first().copied().unwrap_or(JsValue::UNDEFINED);
        let arg1 = args.get(1).copied().unwrap_or(JsValue::UNDEFINED);
        let arg2 = args.get(2).copied().unwrap_or(JsValue::UNDEFINED);

        let (init, opts_val) = if arg0.is_undefined() {
            (PatternInit::default(), arg1)
        } else if let Some(obj) = arg0.as_object() {
            if !arg1.is_undefined() && arg1.as_object().is_none() {
                return Err(vm.type_error(
                    "URLPattern: baseURL cannot be passed as second argument with a dictionary",
                ));
            }
            let init = read_pattern_init_dict(vm, obj)?;
            (init, arg1)
        } else {
            let pat_str = vm.value_to_rust_string(arg0);
            let (base_opt, opts) = if arg1.is_undefined() {
                (None, arg2)
            } else if arg1.as_object().is_some() {
                (None, arg1)
            } else {
                (Some(vm.value_to_rust_string(arg1)), arg2)
            };
            let init = parse_constructor_string(&pat_str, base_opt.as_deref())
                .map_err(|e| vm.type_error(e))?;
            (init, opts)
        };

        let ignore_case = if let Some(oh) = opts_val.as_object() {
            let k = vm.heap.intern_key("ignoreCase");
            vm.get_property_by_key(oh, k)
                .map(|v| vm.to_boolean(v))
                .unwrap_or(false)
        } else {
            false
        };

        let record =
            UrlPatternRecord::from_init(init, ignore_case).map_err(|e| vm.type_error(e))?;
        let inst = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
        st.borrow_mut().insert(inst, record);
        Ok(JsValue::from_object(inst))
    });
    vm.mark_constructor(ctor);
    let k_proto = vm.heap.intern_key("prototype");
    let k_ctor = vm.heap.intern_key("constructor");
    vm.heap.object_define_data(
        ctor,
        k_proto,
        JsValue::from_object(proto),
        PropertyAttrs::empty(),
        &mut vm.shapes,
    );
    vm.heap.object_define_data(
        proto,
        k_ctor,
        JsValue::from_object(ctor),
        PropertyAttrs::CONFIGURABLE | PropertyAttrs::WRITABLE,
        &mut vm.shapes,
    );
    vm.add_host_root_collector(move |roots| {
        roots.push(JsValue::from_object(proto));
    });
    crate::web_apis::define_global(vm, "URLPattern", JsValue::from_object(ctor));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_pattern_string_dict_and_exec_match_whatwg_spec() {
        let mut vm = JsVm::new();
        install_url_pattern(&mut vm);
        let res = vm
            .eval_script(
                r#"
                const p1 = new URLPattern("https://example.com/books/:id(\\d+)");
                const m1 = p1.exec("https://example.com/books/42");
                const p2 = new URLPattern({ pathname: "/docs/:section/*" });
                const m2 = p2.exec("https://docs.rs/docs/guide/intro.html");
                const p3 = new URLPattern("/api/:ver{/:item}?", "https://api.example.com");
                const m3 = p3.exec("https://api.example.com/api/v2");
                const m4 = p3.exec("https://api.example.com/api/v2/users");
                const p4 = new URLPattern({ pathname: "/Case" }, { ignoreCase: true });
                [
                    p1.test("https://example.com/books/42"),
                    p1.test("https://example.com/books/abc"),
                    p1.hasRegExpGroups,
                    m1.pathname.groups.id,
                    m2.pathname.groups.section,
                    m2.pathname.groups["0"],
                    m3 !== null && m3.pathname.groups.ver === "v2" && m3.pathname.groups.item === undefined,
                    m4 !== null && m4.pathname.groups.item === "users",
                    p4.test("https://example.com/case"),
                    p1.protocol,
                    p1.hostname,
                ].join("|");
                "#,
            )
            .expect("URLPattern script must succeed");
        assert_eq!(
            vm.value_to_rust_string(res),
            "true|false|true|42|guide|intro.html|true|true|true|https|example.com"
        );
    }
}
