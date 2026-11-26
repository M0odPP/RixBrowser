//! Async ES Module graph loader, WHATWG Import Maps (`<script type="importmap">`),
//! live cyclic static bindings, `export *` re-exports, dynamic `import()`, and
//! top-level `await` DAG evaluation (zero blocking, zero polling).
//!
//! Reference: Spec 05 §6 & Issue #15.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use thiserror::Error;

use crate::ast::SourceType;
use crate::bytecode::{CompiledProgram, ModuleBlueprint, ModuleImportKind};
use crate::compiler::{compile, CompileError};
use crate::heap::{ClosureObject, JsObject, ObjectKind, PromiseReaction};
use crate::promise::{
    alloc_promise, attach_promise_reaction, create_resolving_functions, reject_promise,
    start_async_call,
};
use crate::shape::PropertyAttrs;
use crate::value::{HeapHandle, JsValue};
use crate::vm::{JsException, JsVm};

/// Errors produced during Import Map parsing or ES Module resolution/linking.
#[derive(Debug, Error)]
pub enum ModuleError {
    #[error("Invalid import map JSON: {0}")]
    InvalidImportMap(String),
    #[error("Failed to resolve module specifier '{specifier}': {reason}")]
    Resolve { specifier: String, reason: String },
    #[error("Failed to load module '{url}': {reason}")]
    Load { url: String, reason: String },
    #[error("Syntax/compile error in module '{url}': {source}")]
    Compile {
        url: String,
        #[source]
        source: CompileError,
    },
    #[error("Link error in module '{url}': {reason}")]
    Link { url: String, reason: String },
}

/// Parsed WHATWG `<script type="importmap">` specifier remapping table.
#[derive(Debug, Clone, Default)]
pub struct ImportMap {
    pub base_url: String,
    pub imports: Vec<(String, String)>,
    pub scopes: Vec<(String, Vec<(String, String)>)>,
}

impl ImportMap {
    /// Parses a JSON `<script type="importmap">` document relative to `base_url`.
    pub fn from_json(json: &str, base_url: &str) -> Result<Self, ModuleError> {
        let root: serde_json::Value =
            serde_json::from_str(json).map_err(|e| ModuleError::InvalidImportMap(e.to_string()))?;
        let obj = root
            .as_object()
            .ok_or_else(|| ModuleError::InvalidImportMap("root must be a JSON object".into()))?;

        let mut imports = Vec::new();
        if let Some(map_val) = obj.get("imports") {
            let map_obj = map_val.as_object().ok_or_else(|| {
                ModuleError::InvalidImportMap("'imports' must be a JSON object".into())
            })?;
            imports = parse_specifier_map(map_obj, base_url);
        }

        let mut scopes = Vec::new();
        if let Some(scopes_val) = obj.get("scopes") {
            let scopes_obj = scopes_val.as_object().ok_or_else(|| {
                ModuleError::InvalidImportMap("'scopes' must be a JSON object".into())
            })?;
            for (scope_prefix, inner_val) in scopes_obj {
                if let Some(inner_obj) = inner_val.as_object() {
                    let resolved_scope = resolve_url_like(scope_prefix, base_url);
                    let entries = parse_specifier_map(inner_obj, base_url);
                    scopes.push((resolved_scope, entries));
                }
            }
            // Longest scope prefix wins per WHATWG Import Maps spec.
            scopes.sort_by_key(|a| std::cmp::Reverse(a.0.len()));
        }

        Ok(Self {
            base_url: base_url.to_string(),
            imports,
            scopes,
        })
    }

    /// Resolves `specifier` imported from `referrer` through scopes -> top-level imports -> URL resolution.
    pub fn resolve(&self, specifier: &str, referrer: Option<&str>) -> Result<String, ModuleError> {
        let ref_url = referrer.unwrap_or(&self.base_url);
        let as_url = if is_url_like_specifier(specifier) {
            Some(resolve_url_like(specifier, ref_url))
        } else {
            None
        };
        let norm_spec = as_url.as_deref().unwrap_or(specifier);

        // 1. Check matching scopes (longest scope prefix first).
        for (scope_prefix, scope_map) in &self.scopes {
            if ref_url == scope_prefix
                || (scope_prefix.ends_with('/') && ref_url.starts_with(scope_prefix))
            {
                if let Some(mapped) = match_specifier_map(specifier, norm_spec, scope_map) {
                    return Ok(mapped);
                }
            }
        }

        // 2. Check top-level `imports`.
        if let Some(mapped) = match_specifier_map(specifier, norm_spec, &self.imports) {
            return Ok(mapped);
        }

        // 3. Bare specifiers not found in import map fail resolution.
        if let Some(url) = as_url {
            Ok(url)
        } else {
            Err(ModuleError::Resolve {
                specifier: specifier.to_string(),
                reason: "bare specifier was not remapped by any import map".into(),
            })
        }
    }
}

fn parse_specifier_map(
    obj: &serde_json::Map<String, serde_json::Value>,
    base_url: &str,
) -> Vec<(String, String)> {
    let mut entries = Vec::new();
    for (k, v) in obj {
        if let Some(addr) = v.as_str() {
            let norm_key = if is_url_like_specifier(k) {
                resolve_url_like(k, base_url)
            } else {
                k.clone()
            };
            let resolved_addr = resolve_url_like(addr, base_url);
            entries.push((norm_key, resolved_addr));
        }
    }
    // Longest key first so more specific prefix mappings take precedence.
    entries.sort_by_key(|a| std::cmp::Reverse(a.0.len()));
    entries
}

fn match_specifier_map(
    raw_spec: &str,
    norm_spec: &str,
    entries: &[(String, String)],
) -> Option<String> {
    for (key, addr) in entries {
        if key == norm_spec || key == raw_spec {
            return Some(addr.clone());
        }
        if key.ends_with('/') && addr.ends_with('/') {
            if let Some(rest) = norm_spec.strip_prefix(key.as_str()) {
                return Some(format!("{addr}{rest}"));
            }
            if let Some(rest) = raw_spec.strip_prefix(key.as_str()) {
                return Some(format!("{addr}{rest}"));
            }
        }
    }
    None
}

fn is_url_like_specifier(spec: &str) -> bool {
    spec.starts_with("./")
        || spec.starts_with("../")
        || spec.starts_with('/')
        || spec.contains("://")
}

/// Resolves relative (`./`, `../`, `/`) and absolute module URLs cleanly.
#[must_use]
pub fn resolve_url_like(spec: &str, base: &str) -> String {
    if spec.contains("://") {
        return normalize_url_path(spec);
    }
    let (scheme_host, base_path) = split_scheme_host(base);
    if spec.starts_with('/') {
        return format!("{scheme_host}{}", normalize_posix_path(spec));
    }
    let dir = match base_path.rfind('/') {
        Some(idx) => &base_path[..=idx],
        None => "/",
    };
    let joined = format!("{dir}{spec}");
    format!("{scheme_host}{}", normalize_posix_path(&joined))
}

fn split_scheme_host(url: &str) -> (&str, &str) {
    if let Some(pos) = url.find("://") {
        let after_scheme = pos + 3;
        if let Some(slash) = url[after_scheme..].find('/') {
            let split = after_scheme + slash;
            return (&url[..split], &url[split..]);
        }
        return (url, "/");
    }
    ("", url)
}

fn normalize_url_path(url: &str) -> String {
    let (scheme_host, path) = split_scheme_host(url);
    format!("{scheme_host}{}", normalize_posix_path(path))
}

fn normalize_posix_path(path: &str) -> String {
    let trailing_slash = path.ends_with('/') && path.len() > 1;
    let mut stack: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                stack.pop();
            }
            other => stack.push(other),
        }
    }
    let mut out = format!("/{}", stack.join("/"));
    if trailing_slash && !out.ends_with('/') {
        out.push('/');
    }
    out
}

/// Pluggable host module source provider (in-memory, VFS, or async network cache).
pub trait ModuleSourceProvider {
    fn load_source(&self, url: &str) -> Result<String, String>;

    /// Optional batch prefetch hook invoked before compiling a wave of sibling module imports
    /// so network-backed providers can fetch all dependencies in parallel instead of serially.
    fn prefetch_sources(&self, _urls: &[String]) {}
}

/// In-memory module source registry (ideal for unit tests and bundled apps).
#[derive(Debug, Default, Clone)]
pub struct MemoryModuleProvider {
    sources: HashMap<String, String>,
}

impl MemoryModuleProvider {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, url: impl Into<String>, source: impl Into<String>) {
        self.sources.insert(url.into(), source.into());
    }
}

impl ModuleSourceProvider for MemoryModuleProvider {
    fn load_source(&self, url: &str) -> Result<String, String> {
        self.sources
            .get(url)
            .cloned()
            .ok_or_else(|| format!("module not found: {url}"))
    }
}

/// Resolved live binding slot for a static `import`.
#[derive(Debug, Clone, Copy)]
pub enum ImportBinding {
    /// Live `UpvalueCell` owned by the exporting module.
    Cell(HeapHandle),
    /// Namespace object (`import * as ns from ...`).
    Namespace(HeapHandle),
    /// Unbound / side-effect only import.
    None,
}

/// Runtime module environment attached to a compiled `program_id` in [`JsVm`].
#[derive(Debug, Clone)]
pub struct ModuleEnv {
    pub url: String,
    pub import_bindings: Vec<ImportBinding>,
    pub export_cells: Vec<HeapHandle>,
    pub export_by_name: HashMap<String, HeapHandle>,
    pub namespace_obj: HeapHandle,
}

impl ModuleEnv {
    pub fn trace_roots(&self, out: &mut Vec<JsValue>) {
        for b in &self.import_bindings {
            match b {
                ImportBinding::Cell(h) | ImportBinding::Namespace(h) => {
                    out.push(JsValue::from_object(*h));
                }
                ImportBinding::None => {}
            }
        }
        for &c in &self.export_cells {
            out.push(JsValue::from_object(c));
        }
        for &c in self.export_by_name.values() {
            out.push(JsValue::from_object(c));
        }
        out.push(JsValue::from_object(self.namespace_obj));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModuleEvalStatus {
    Linked,
    Evaluating,
    Evaluated,
}

struct ModuleRecord {
    program_id: u32,
    entry_func: u32,
    blueprint: ModuleBlueprint,
    /// Direct static dependencies in source order (imports + re-exports).
    dependencies: Vec<String>,
    /// Hoisted zero-upvalue functions `(export_name, func_idx)` materialized at Link time.
    hoisted_exports: Vec<(String, u32)>,
    status: ModuleEvalStatus,
    eval_promise: Option<HeapHandle>,
}

/// Shared handle to an ES Module graph loader capable of static linking,
/// cyclic live bindings, Import Map resolution, dynamic `import()`, and
/// top-level `await` DAG evaluation.
pub struct ModuleLoader {
    provider: Box<dyn ModuleSourceProvider>,
    import_map: ImportMap,
    records: HashMap<String, ModuleRecord>,
}

impl ModuleLoader {
    #[must_use]
    pub fn new(provider: Box<dyn ModuleSourceProvider>, import_map: ImportMap) -> Self {
        Self {
            provider,
            import_map,
            records: HashMap::new(),
        }
    }

    /// Replaces the active document Import Map (HTML §8.1.6.7 `<script type="importmap">`).
    pub fn set_import_map(&mut self, import_map: ImportMap) {
        self.import_map = import_map;
    }

    /// Installs this loader into `vm` so `import(specifier)` (`Opcode::DynamicImport`)
    /// resolves and evaluates modules asynchronously through the same graph & Import Map.
    pub fn install_on_vm(loader: &Rc<RefCell<Self>>, vm: &mut JsVm) {
        let loader_rc = Rc::clone(loader);
        vm.set_dynamic_import_handler(Rc::new(move |vm, referrer, specifier| {
            let resolved = match loader_rc
                .borrow()
                .import_map
                .resolve(&specifier, referrer.as_deref())
            {
                Ok(u) => u,
                Err(e) => {
                    let p = alloc_promise(vm);
                    let err = vm.type_error(e.to_string());
                    reject_promise(vm, p, err.value);
                    return p;
                }
            };
            match Self::load_and_evaluate(&loader_rc, vm, &resolved) {
                Ok(p) => p,
                Err(e) => {
                    let p = alloc_promise(vm);
                    let err_kind = match &e {
                        ModuleError::Compile { .. } | ModuleError::Link { .. } => "SyntaxError",
                        ModuleError::Load { reason, .. } if reason.starts_with("SyntaxError:") => {
                            "SyntaxError"
                        }
                        _ => "TypeError",
                    };
                    let err = vm.make_error(err_kind, e.to_string());
                    reject_promise(vm, p, err.value);
                    p
                }
            }
        }));
    }

    /// Loads, links, and asynchronously evaluates the module graph rooted at `entry_url`.
    ///
    /// Returns a `Promise` handle that fulfills with the entry module's live namespace
    /// object once the entire dependency DAG (including any top-level `await` modules)
    /// completes. Call [`JsVm::drain_microtasks`] to drive microtask completion.
    pub fn load_and_evaluate(
        loader: &Rc<RefCell<Self>>,
        vm: &mut JsVm,
        entry_url: &str,
    ) -> Result<HeapHandle, ModuleError> {
        let resolved_entry = loader.borrow().import_map.resolve(entry_url, None)?;
        // 1. Recursively fetch, parse, compile, and allocate export cells for the graph.
        let mut newly_loaded = Vec::new();
        loader
            .borrow_mut()
            .fetch_and_compile_graph(vm, &resolved_entry, &mut newly_loaded)?;
        // 2. Link live import cells, `export *` re-exports, namespace getters, and hoisted functions.
        loader.borrow_mut().link_modules(vm, &newly_loaded)?;
        // 3. Kick off non-blocking DAG evaluation returning a Promise to the namespace object.
        let eval_p = Self::evaluate_module(loader, vm, &resolved_entry);
        let ns_obj = {
            let l = loader.borrow();
            let pid = l.records[&resolved_entry].program_id;
            vm.module_env(pid)
                .map(|e| e.namespace_obj)
                .expect("module env linked")
        };
        let out_p = alloc_promise(vm);
        let (res_out, rej_out) = create_resolving_functions(vm, out_p);
        let on_ful = vm.alloc_host_fn("mod_ns_ful", move |vm, _, _| {
            let _ = vm.call_value(
                JsValue::from_object(res_out),
                JsValue::UNDEFINED,
                &[JsValue::from_object(ns_obj)],
            );
            Ok(JsValue::UNDEFINED)
        });
        attach_promise_reaction(
            vm,
            eval_p,
            PromiseReaction {
                on_fulfilled: Some(on_ful),
                on_rejected: Some(rej_out),
                result_promise: None,
            },
        );
        Ok(out_p)
    }

    fn fetch_and_compile_graph(
        &mut self,
        vm: &mut JsVm,
        entry_url: &str,
        newly_loaded: &mut Vec<String>,
    ) -> Result<(), ModuleError> {
        let mut wave: Vec<String> = vec![entry_url.to_string()];
        let mut queued: HashSet<String> = HashSet::new();
        queued.insert(entry_url.to_string());

        while !wave.is_empty() {
            // Prefetch all uncompiled modules in this dependency level concurrently
            // (eliminating N-round-trip serial waterfalls on large ES module graphs).
            let to_prefetch: Vec<String> = wave
                .iter()
                .filter(|u| !self.records.contains_key(u.as_str()))
                .cloned()
                .collect();
            if to_prefetch.len() > 1 {
                self.provider.prefetch_sources(&to_prefetch);
            }

            let mut next_wave: Vec<String> = Vec::new();
            for url in wave {
                if self.records.contains_key(&url) {
                    continue;
                }
                let raw_source =
                    self.provider
                        .load_source(&url)
                        .map_err(|reason| ModuleError::Load {
                            url: url.clone(),
                            reason,
                        })?;
                let source = if url.ends_with(".json") {
                    let _: serde_json::Value =
                        serde_json::from_str(&raw_source).map_err(|e| ModuleError::Load {
                            url: url.clone(),
                            reason: format!("SyntaxError: invalid JSON module: {e}"),
                        })?;
                    let escaped =
                        serde_json::to_string(&raw_source).unwrap_or_else(|_| "\"\"".into());
                    format!("export default JSON.parse({escaped});")
                } else {
                    raw_source
                };
                let compiled: CompiledProgram =
                    compile(&source, SourceType::Module).map_err(|e| ModuleError::Compile {
                        url: url.clone(),
                        source: e,
                    })?;
                let blueprint = compiled.module.clone().unwrap_or_default();
                let entry_func = compiled.entry_func;

                // Collect hoisted zero-upvalue exported functions so cyclic top-level calls resolve.
                let mut hoisted_exports = Vec::new();
                for exp in &blueprint.exports {
                    if exp.reexport_from.is_none() && !exp.is_star {
                        if let Some(ref local) = exp.local_name {
                            if let Some((fi, _)) =
                                compiled.functions.iter().enumerate().find(|(_, f)| {
                                    f.name.as_deref() == Some(local.as_str())
                                        && f.upvalues.is_empty()
                                })
                            {
                                hoisted_exports.push((exp.exported_name.clone(), fi as u32));
                            }
                        }
                    }
                }

                // Resolve all static dependency URLs in source order (deduplicated).
                let mut dependencies = Vec::new();
                let mut seen_deps = HashSet::new();
                for imp in &blueprint.imports {
                    let dep_url = self.import_map.resolve(&imp.specifier, Some(&url))?;
                    if seen_deps.insert(dep_url.clone()) {
                        dependencies.push(dep_url);
                    }
                }
                for exp in &blueprint.exports {
                    if let Some(ref spec) = exp.reexport_from {
                        let dep_url = self.import_map.resolve(spec, Some(&url))?;
                        if seen_deps.insert(dep_url.clone()) {
                            dependencies.push(dep_url);
                        }
                    }
                }

                let program_id = vm.load_program(compiled);

                // Allocate live export cells (`UpvalueCell`) and empty namespace object right now
                // so cycles can point at them during the Link pass.
                let mut export_cells = Vec::with_capacity(blueprint.exports.len());
                let mut export_by_name = HashMap::new();
                for exp in &blueprint.exports {
                    let cell = alloc_module_cell(vm, JsValue::UNDEFINED);
                    export_cells.push(cell);
                    if !exp.is_star {
                        export_by_name.insert(exp.exported_name.clone(), cell);
                    }
                }
                let namespace_obj = vm.alloc_ordinary_object();
                vm.register_module_env(
                    program_id,
                    ModuleEnv {
                        url: url.clone(),
                        import_bindings: vec![ImportBinding::None; blueprint.imports.len()],
                        export_cells,
                        export_by_name,
                        namespace_obj,
                    },
                );

                for dep in &dependencies {
                    if !self.records.contains_key(dep) && queued.insert(dep.clone()) {
                        next_wave.push(dep.clone());
                    }
                }

                self.records.insert(
                    url.clone(),
                    ModuleRecord {
                        program_id,
                        entry_func,
                        blueprint,
                        dependencies,
                        hoisted_exports,
                        status: ModuleEvalStatus::Linked,
                        eval_promise: None,
                    },
                );
                newly_loaded.push(url);
            }
            wave = next_wave;
        }
        Ok(())
    }

    fn link_modules(&mut self, vm: &mut JsVm, urls: &[String]) -> Result<(), ModuleError> {
        // Pass 1: resolve re-exports (`export { a as b } from` and `export * from`) so
        // every module's `export_by_name` contains its full transitive export table.
        for url in urls {
            let (prog_id, exports) = {
                let rec = &self.records[url];
                (rec.program_id, rec.blueprint.exports.clone())
            };
            for exp in exports {
                let Some(ref spec) = exp.reexport_from else {
                    continue;
                };
                let target_url = self.import_map.resolve(spec, Some(url))?;
                if exp.is_star {
                    let mut visited = HashSet::new();
                    let star_exports = self.collect_all_exports(vm, &target_url, &mut visited);
                    if let Some(env) = vm.module_env_mut(prog_id) {
                        for (name, cell) in star_exports {
                            if name != "default" {
                                env.export_by_name.entry(name).or_insert(cell);
                            }
                        }
                    }
                } else {
                    let lookup_name = exp.local_name.as_deref().unwrap_or(&exp.exported_name);
                    let mut visited = HashSet::new();
                    let cell = self
                        .resolve_export_cell(vm, &target_url, lookup_name, &mut visited)
                        .ok_or_else(|| ModuleError::Link {
                            url: url.clone(),
                            reason: format!(
                                "re-exported binding '{lookup_name}' not found in '{target_url}'"
                            ),
                        })?;
                    if let Some(env) = vm.module_env_mut(prog_id) {
                        env.export_by_name.insert(exp.exported_name.clone(), cell);
                    }
                }
            }
        }

        // Pass 2: bind static `import` slots, populate hoisted zero-upvalue functions,
        // and install live getter accessors on each module's namespace object.
        for url in urls {
            let (prog_id, imports, hoisted) = {
                let rec = &self.records[url];
                (
                    rec.program_id,
                    rec.blueprint.imports.clone(),
                    rec.hoisted_exports.clone(),
                )
            };

            let mut resolved_bindings = Vec::with_capacity(imports.len());
            for imp in &imports {
                let target_url = self.import_map.resolve(&imp.specifier, Some(url))?;
                let binding = match &imp.imported_name {
                    ModuleImportKind::SideEffectOnly => ImportBinding::None,
                    ModuleImportKind::Namespace => {
                        let target_pid = self.records[&target_url].program_id;
                        let ns = vm.module_env(target_pid).expect("target env").namespace_obj;
                        ImportBinding::Namespace(ns)
                    }
                    ModuleImportKind::Default => {
                        let mut visited = HashSet::new();
                        let cell = self
                            .resolve_export_cell(vm, &target_url, "default", &mut visited)
                            .ok_or_else(|| ModuleError::Link {
                                url: url.clone(),
                                reason: format!("module '{target_url}' has no default export"),
                            })?;
                        ImportBinding::Cell(cell)
                    }
                    ModuleImportKind::Named(name) => {
                        let mut visited = HashSet::new();
                        let cell = self
                            .resolve_export_cell(vm, &target_url, name, &mut visited)
                            .ok_or_else(|| ModuleError::Link {
                                url: url.clone(),
                                reason: format!(
                                    "module '{target_url}' does not provide an export named '{name}'"
                                ),
                            })?;
                        ImportBinding::Cell(cell)
                    }
                };
                resolved_bindings.push(binding);
            }
            if let Some(env) = vm.module_env_mut(prog_id) {
                env.import_bindings = resolved_bindings;
            }

            // Pre-populate hoisted zero-upvalue exported functions so cyclic modules
            // calling them during top-level init succeed.
            for (exp_name, func_idx) in hoisted {
                let closure = vm.heap.alloc_object(JsObject::with_kind(
                    ObjectKind::Closure(Box::new(ClosureObject {
                        program_id: prog_id,
                        func_idx,
                        upvalues: Vec::new(),
                        bound_this: None,
                        super_ctor: None,
                        field_init: None,
                    })),
                    Some(vm.function_proto),
                ));
                if let Some(cell) = vm
                    .module_env(prog_id)
                    .and_then(|e| e.export_by_name.get(&exp_name).copied())
                {
                    write_module_cell(vm, cell, JsValue::from_object(closure));
                }
            }

            // Attach live getters on `namespace_obj` reading directly from each export cell.
            let (ns_obj, export_entries): (HeapHandle, Vec<(String, HeapHandle)>) = {
                let env = vm.module_env(prog_id).expect("env");
                (
                    env.namespace_obj,
                    env.export_by_name
                        .iter()
                        .map(|(k, &v)| (k.clone(), v))
                        .collect(),
                )
            };
            for (name, cell) in export_entries {
                let key = vm.heap.intern_key(&name);
                let getter =
                    vm.alloc_host_fn("ns_get", move |vm, _, _| Ok(read_module_cell(vm, cell)));
                vm.heap.object_define_accessor(
                    ns_obj,
                    key,
                    Some(getter),
                    None,
                    PropertyAttrs::ENUMERABLE,
                    &mut vm.shapes,
                );
            }
        }
        Ok(())
    }

    fn resolve_export_cell(
        &self,
        vm: &JsVm,
        url: &str,
        name: &str,
        visited: &mut HashSet<String>,
    ) -> Option<HeapHandle> {
        if !visited.insert(format!("{url}::{name}")) {
            return None;
        }
        let rec = self.records.get(url)?;
        if let Some(cell) = vm
            .module_env(rec.program_id)
            .and_then(|e| e.export_by_name.get(name).copied())
        {
            return Some(cell);
        }
        for exp in &rec.blueprint.exports {
            if exp.is_star {
                if let Some(ref spec) = exp.reexport_from {
                    if let Ok(target) = self.import_map.resolve(spec, Some(url)) {
                        if let Some(c) = self.resolve_export_cell(vm, &target, name, visited) {
                            return Some(c);
                        }
                    }
                }
            }
        }
        None
    }

    fn collect_all_exports(
        &self,
        vm: &JsVm,
        url: &str,
        visited: &mut HashSet<String>,
    ) -> Vec<(String, HeapHandle)> {
        if !visited.insert(url.to_string()) {
            return Vec::new();
        }
        let Some(rec) = self.records.get(url) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        if let Some(env) = vm.module_env(rec.program_id) {
            for (k, &c) in &env.export_by_name {
                out.push((k.clone(), c));
            }
        }
        for exp in &rec.blueprint.exports {
            if exp.is_star {
                if let Some(ref spec) = exp.reexport_from {
                    if let Ok(target) = self.import_map.resolve(spec, Some(url)) {
                        out.extend(self.collect_all_exports(vm, &target, visited));
                    }
                }
            }
        }
        out
    }

    /// Evaluates `url` in post-order dependency DAG order with full support for
    /// cyclic dependencies and top-level `await` (zero blocking / zero polling).
    fn evaluate_module(loader: &Rc<RefCell<Self>>, vm: &mut JsVm, url: &str) -> HeapHandle {
        // Fast path: already evaluated or currently evaluating (breaks cycles!).
        {
            let l = loader.borrow();
            if let Some(rec) = l.records.get(url) {
                if let Some(p) = rec.eval_promise {
                    return p;
                }
            }
        }

        let eval_promise = alloc_promise(vm);
        let (deps, prog_id, entry_func) = {
            let mut l = loader.borrow_mut();
            let rec = l.records.get_mut(url).expect("linked module record");
            rec.status = ModuleEvalStatus::Evaluating;
            rec.eval_promise = Some(eval_promise);
            (rec.dependencies.clone(), rec.program_id, rec.entry_func)
        };

        // Kick off evaluation of all dependencies that are not already in-flight (cycle-safe!).
        let mut wait_promises = Vec::new();
        for dep in deps {
            let already_active = loader
                .borrow()
                .records
                .get(&dep)
                .is_some_and(|r| r.status != ModuleEvalStatus::Linked);
            if !already_active {
                let dep_p = Self::evaluate_module(loader, vm, &dep);
                wait_promises.push(dep_p);
            } else {
                // If an already-active dependency has top-level await and isn't evaluated yet,
                // only wait if it is NOT an ancestor cycle (avoid deadlock on true cycles).
                let dep_done_or_async = loader.borrow().records.get(&dep).and_then(|r| {
                    if r.status == ModuleEvalStatus::Evaluated {
                        r.eval_promise
                    } else {
                        None
                    }
                });
                if let Some(p) = dep_done_or_async {
                    wait_promises.push(p);
                }
            }
        }

        let loader_rc = Rc::clone(loader);
        let url_owned = url.to_string();
        let run_body = move |vm: &mut JsVm| {
            let entry_closure = vm.heap.alloc_object(JsObject::with_kind(
                ObjectKind::Closure(Box::new(ClosureObject {
                    program_id: prog_id,
                    func_idx: entry_func,
                    upvalues: Vec::new(),
                    bound_this: Some(JsValue::UNDEFINED),
                    super_ctor: None,
                    field_init: None,
                })),
                Some(vm.function_proto),
            ));
            let body_p = start_async_call(
                vm,
                entry_closure,
                prog_id,
                entry_func,
                JsValue::UNDEFINED,
                &[],
            );
            let (res_fn, rej_fn) = create_resolving_functions(vm, eval_promise);
            let l_done = Rc::clone(&loader_rc);
            let u_done = url_owned.clone();
            let on_ful = vm.alloc_host_fn("mod_eval_done", move |vm, _, args| {
                if let Some(r) = l_done.borrow_mut().records.get_mut(&u_done) {
                    r.status = ModuleEvalStatus::Evaluated;
                }
                let _ = vm.call_value(JsValue::from_object(res_fn), JsValue::UNDEFINED, args);
                Ok(JsValue::UNDEFINED)
            });
            attach_promise_reaction(
                vm,
                body_p,
                PromiseReaction {
                    on_fulfilled: Some(on_ful),
                    on_rejected: Some(rej_fn),
                    result_promise: None,
                },
            );
        };

        if wait_promises.is_empty() {
            run_body(vm);
        } else {
            // Wait for every dependency promise to settle before executing this module body.
            let remaining = Rc::new(std::cell::Cell::new(wait_promises.len()));
            let failed = Rc::new(std::cell::Cell::new(false));
            let body_cell = Rc::new(RefCell::new(Some(run_body)));
            for dep_p in wait_promises {
                let rem = Rc::clone(&remaining);
                let fail_r = Rc::clone(&failed);
                let body_r = Rc::clone(&body_cell);
                let on_dep_ok = vm.alloc_host_fn("mod_dep_ok", move |vm, _, _| {
                    if fail_r.get() {
                        return Ok(JsValue::UNDEFINED);
                    }
                    let left = rem.get().saturating_sub(1);
                    rem.set(left);
                    if left == 0 {
                        if let Some(f) = body_r.borrow_mut().take() {
                            f(vm);
                        }
                    }
                    Ok(JsValue::UNDEFINED)
                });
                let fail_e = Rc::clone(&failed);
                let on_dep_err = vm.alloc_host_fn("mod_dep_err", move |vm, _, args| {
                    if !fail_e.replace(true) {
                        reject_promise(
                            vm,
                            eval_promise,
                            args.first().copied().unwrap_or(JsValue::UNDEFINED),
                        );
                    }
                    Ok(JsValue::UNDEFINED)
                });
                attach_promise_reaction(
                    vm,
                    dep_p,
                    PromiseReaction {
                        on_fulfilled: Some(on_dep_ok),
                        on_rejected: Some(on_dep_err),
                        result_promise: None,
                    },
                );
            }
        }

        eval_promise
    }

    /// Convenience helper for synchronously evaluating a single module source without external deps.
    pub fn eval_single_module(
        vm: &mut JsVm,
        url: &str,
        source: &str,
    ) -> Result<HeapHandle, ModuleError> {
        let mut prov = MemoryModuleProvider::new();
        prov.insert(url, source);
        let loader = Rc::new(RefCell::new(Self::new(
            Box::new(prov),
            ImportMap::default(),
        )));
        Self::install_on_vm(&loader, vm);
        Self::load_and_evaluate(&loader, vm, url)
    }
}

/// Allocates a single-slot live binding cell (`ObjectKind::UpvalueCell`).
pub fn alloc_module_cell(vm: &mut JsVm, initial: JsValue) -> HeapHandle {
    let mut cell = JsObject::with_kind(ObjectKind::UpvalueCell, None);
    cell.slots.push(initial);
    vm.heap.alloc_object(cell)
}

/// Reads the live value stored inside a module binding cell.
#[must_use]
pub fn read_module_cell(vm: &JsVm, cell: HeapHandle) -> JsValue {
    vm.heap
        .get_object(cell)
        .and_then(|o| o.slots.first().copied())
        .unwrap_or(JsValue::UNDEFINED)
}

/// Updates the live value stored inside a module binding cell.
pub fn write_module_cell(vm: &mut JsVm, cell: HeapHandle, val: JsValue) {
    if let Some(o) = vm.heap.get_object_mut(cell) {
        if o.slots.is_empty() {
            o.slots.push(val);
        } else {
            o.slots[0] = val;
        }
    }
    vm.heap.write_barrier(cell, val);
}

/// Helper invoked by `Opcode::GetModuleImport` inside `JsVm`.
pub fn vm_get_module_import(
    vm: &mut JsVm,
    program_id: u32,
    import_idx: u16,
) -> Result<JsValue, JsException> {
    let binding = vm
        .module_env(program_id)
        .and_then(|e| e.import_bindings.get(import_idx as usize).copied())
        .unwrap_or(ImportBinding::None);
    match binding {
        ImportBinding::Cell(c) => Ok(read_module_cell(vm, c)),
        ImportBinding::Namespace(ns) => Ok(JsValue::from_object(ns)),
        ImportBinding::None => Ok(JsValue::UNDEFINED),
    }
}

/// Helper invoked by `Opcode::SetModuleExport` inside `JsVm`.
pub fn vm_set_module_export(vm: &mut JsVm, program_id: u32, export_idx: u16, val: JsValue) {
    if let Some(cell) = vm
        .module_env(program_id)
        .and_then(|e| e.export_cells.get(export_idx as usize).copied())
    {
        write_module_cell(vm, cell, val);
    }
}
