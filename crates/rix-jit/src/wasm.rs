//! WebAssembly 2.0 + Modern Proposals & W3C `WebAssembly` JS-API Integration (powered by `wasmtime`).
//!
//! Reference: Spec 05 §5, W3C WebAssembly Core 2.0 + JS-API, Issues #21 & #154.
//!
//! Exposes the full standard `WebAssembly` namespace on any [`rix_js::JsVm`] realm:
//! - `WebAssembly.validate(bytes)`
//! - `WebAssembly.compile(bytes)` / `WebAssembly.compileStreaming(source)`
//! - `WebAssembly.instantiate(bytesOrModule, importObject?)`
//! - `WebAssembly.instantiateStreaming(source, importObject?)`
//! - `WebAssembly.Module` (`.exports()`, `.imports()`, `.customSections()`)
//! - `WebAssembly.Instance` (prototype `.exports` getter, frozen null-proto exports, canonical wrapper caching)
//! - `WebAssembly.Memory` (32-bit & `memory64` `address: "i64"`, `shared`, `.buffer`, `.grow()`, `.type()`, `.toResizableBuffer()`, `.toFixedLengthBuffer()`)
//! - `WebAssembly.Table` (32-bit & `table64` `address: "i64"`, `funcref`/`externref`/`anyref`/`eqref`/`exnref`, `.length`, `.get()`, `.set()`, `.grow()`, `.type()`)
//! - `WebAssembly.Global` (`.value` getter/setter, `.valueOf()`, `.type()`, numeric & reference types)
//! - `WebAssembly.Tag` (`.type()`) & `WebAssembly.Exception` (`.is()`, `.getArg()`, `.stack`)
//! - `WebAssembly.CompileError`, `WebAssembly.LinkError`, `WebAssembly.RuntimeError`

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use rix_js::builtins_atomics::{ab_is_shared, to_bigint_i64};
use rix_js::heap::SharedBufferBacking;
use rix_js::promise::{alloc_promise, promise_resolve_value, promise_then, reject_promise};
use rix_js::{
    HeapHandle, JsBigInt, JsException, JsObject, JsValue, JsVm, ObjectKind, PropertyAttrs,
};
use wasmtime::{
    AnyRef, AsContext, AsContextMut, Caller, Config, Engine, ExnRef, ExnRefPre, ExnType, Extern,
    ExternRef, ExternType, Func, FuncType, Global, GlobalType, HeapType, Instance, Memory,
    MemoryType, Module, Mutability, OwnedRooted, Ref, RefType, SharedMemory, Store, StoreLimits,
    StoreLimitsBuilder, Table, TableType, Tag, TagType, Trap, Val, ValType, I31,
};

/// Internal hidden slot keys stored on JS wrapper objects.
const MODULE_ID_KEY: &str = "__rix_wasm_module_id";
const INSTANCE_ID_KEY: &str = "__rix_wasm_instance_id";
const INSTANCE_EXPORTS_KEY: &str = "__rix_wasm_instance_exports";
const MEMORY_ID_KEY: &str = "__rix_wasm_memory_id";
const TABLE_ID_KEY: &str = "__rix_wasm_table_id";
const GLOBAL_ID_KEY: &str = "__rix_wasm_global_id";
const TAG_ID_KEY: &str = "__rix_wasm_tag_id";
const EXN_ID_KEY: &str = "__rix_wasm_exn_id";
const FUNC_ID_KEY: &str = "__rix_wasm_func_id";
const ANYREF_ID_KEY: &str = "__rix_wasm_anyref_id";
const ROOTS_KEY: &str = "__rix_wasm_roots";

/// Maximum WebAssembly 32-bit linear memory pages (64 KiB each = 4 GiB spec ceiling).
const MAX_WASM32_PAGES: u64 = 65_536;
/// Maximum WebAssembly 64-bit linear memory pages (2^48 bytes / 64 KiB).
const MAX_WASM64_PAGES: u64 = 1u64 << 32;
/// WebAssembly page size in bytes.
const WASM_PAGE_SIZE: usize = 65_536;

/// Opaque payload stored inside `wasmtime::ExternRef` carrying a GC-rooted `JsValue` raw bit pattern.
#[derive(Debug, Clone, Copy)]
struct JsExternVal(u64);

/// Backing kind for a `WebAssembly.Memory` (either unshared `Memory` or threads `SharedMemory`).
enum WasmMemKind {
    Unshared(Memory),
    Shared(SharedMemory),
}

/// Shared memory synchronization record bridging `wasmtime::Memory` / `SharedMemory` <-> JS `ArrayBuffer`/`SharedArrayBuffer`.
struct MemoryBridge {
    kind: WasmMemKind,
    js_buffer: HeapHandle,
    shared_backing: Option<Arc<SharedBufferBacking>>,
    last_byte_len: usize,
    is_resizable: bool,
    is_64: bool,
    max_pages: Option<u64>,
    js_wrapper: Option<HeapHandle>,
}

struct TableEntry {
    table: Table,
    js_wrapper: Option<HeapHandle>,
}

struct GlobalEntry {
    global: Global,
    js_wrapper: Option<HeapHandle>,
}

struct TagEntry {
    tag: Tag,
    js_wrapper: Option<HeapHandle>,
}

struct ExnEntry {
    exn: OwnedRooted<ExnRef>,
    stack: Option<String>,
}

/// Store user data carrying temporary active `JsVm` pointer during synchronous Wasm -> JS
/// imported function calls, stashing any JS exception thrown inside an imported callback,
/// and sharing memory bridge metadata across `Send + Sync` Wasm host closures.
struct WasmStoreData {
    vm_addr: usize,
    pending_js_exception: Option<JsException>,
    memories: Arc<Mutex<Vec<MemoryBridge>>>,
    /// Shared handle to `WasmHost` raw pointer valid during active JS->Wasm invocations so
    /// host-import closures can wrap/unwrap `funcref`/`externref`/`anyref`/`exnref`.
    host_addr: usize,
    /// Wasmtime resource limits so huge `table.grow` / `memory.grow` return `-1` instead of OOM trapping.
    limits: StoreLimits,
}

/// Shared WebAssembly runtime state owned by a single [`JsVm`] realm.
pub struct WasmHost {
    engine: Engine,
    store: RefCell<Store<WasmStoreData>>,
    /// Pointer to stack-pinned `StoreContextMut` when inside a re-entrant Wasm -> JS host callback.
    active_ctx_ptr: Cell<usize>,
    modules: RefCell<Vec<(Module, Vec<u8>)>>,
    instances: RefCell<Vec<Instance>>,
    memories: Arc<Mutex<Vec<MemoryBridge>>>,
    tables: RefCell<Vec<TableEntry>>,
    globals: RefCell<Vec<GlobalEntry>>,
    tags: RefCell<Vec<TagEntry>>,
    exceptions: RefCell<Vec<ExnEntry>>,
    funcs: RefCell<Vec<Func>>,
    /// Canonical exported JS function wrapper keyed by `Func::to_raw(&mut store) as usize`.
    func_wrappers: RefCell<HashMap<usize, HeapHandle>>,
    /// GC-rooted `AnyRef` objects exposed to JS as opaque objects.
    anyrefs: RefCell<Vec<OwnedRooted<wasmtime::AnyRef>>>,
    /// Hidden JS Array attached to `globalThis.WebAssembly` rooting JS values referenced by Wasm.
    roots_arr: HeapHandle,
    /// Prototypes & error constructors installed in this realm.
    module_proto: HeapHandle,
    instance_proto: HeapHandle,
    memory_proto: HeapHandle,
    table_proto: HeapHandle,
    global_proto: HeapHandle,
    tag_proto: HeapHandle,
    exception_proto: HeapHandle,
    compile_error_proto: HeapHandle,
    link_error_proto: HeapHandle,
    runtime_error_proto: HeapHandle,
}

/// Handle to a realm's installed WebAssembly subsystem.
pub type SharedWasmHost = Rc<WasmHost>;

/// RAII guard restoring `WasmHost::active_ctx_ptr` on drop.
struct ActiveCtxGuard<'a> {
    host: &'a WasmHost,
    prev: usize,
}

impl Drop for ActiveCtxGuard<'_> {
    fn drop(&mut self) {
        self.host.active_ctx_ptr.set(self.prev);
    }
}

impl WasmHost {
    /// Exposes an active Wasm `Caller`'s `StoreContextMut` across a synchronous JS callback
    /// so re-entrant JS -> Wasm calls reuse the borrowed store cleanly without `RefCell` panic.
    fn enter_reentrant_ctx<'a, R>(
        &self,
        mut ctx: wasmtime::StoreContextMut<'a, WasmStoreData>,
        f: impl FnOnce() -> R,
    ) -> R {
        let prev = self.active_ctx_ptr.get();
        let ptr = (&mut ctx as *mut wasmtime::StoreContextMut<'a, WasmStoreData>) as usize;
        self.active_ctx_ptr.set(ptr);
        let _guard = ActiveCtxGuard { host: self, prev };
        f()
    }

    /// Runs `f` with shared `StoreContext` access, transparently supporting re-entrant JS -> Wasm calls.
    fn with_store<R>(&self, f: impl FnOnce(wasmtime::StoreContext<'_, WasmStoreData>) -> R) -> R {
        let ptr = self.active_ctx_ptr.get();
        if ptr != 0 {
            self.active_ctx_ptr.set(0);
            let _guard = ActiveCtxGuard {
                host: self,
                prev: ptr,
            };
            // SAFETY: `enter_reentrant_ctx` pins the `StoreContextMut` on this thread's stack for the
            // exact duration `active_ctx_ptr == ptr`, and clearing it prevents aliasing mutable borrows.
            let ctx_ref = unsafe { &*(ptr as *const wasmtime::StoreContextMut<'_, WasmStoreData>) };
            f(ctx_ref.as_context())
        } else {
            let store = self.store.borrow();
            f(store.as_context())
        }
    }

    /// Runs `f` with exclusive `StoreContextMut` access, transparently supporting re-entrant JS -> Wasm calls.
    fn with_store_mut<R>(
        &self,
        f: impl FnOnce(wasmtime::StoreContextMut<'_, WasmStoreData>) -> R,
    ) -> R {
        let ptr = self.active_ctx_ptr.get();
        if ptr != 0 {
            self.active_ctx_ptr.set(0);
            let _guard = ActiveCtxGuard {
                host: self,
                prev: ptr,
            };
            // SAFETY: `enter_reentrant_ctx` pins the `StoreContextMut` on this thread's stack for the
            // exact duration `active_ctx_ptr == ptr`, and clearing it guarantees strict LIFO exclusive access.
            let ctx_ref =
                unsafe { &mut *(ptr as *mut wasmtime::StoreContextMut<'_, WasmStoreData>) };
            f(ctx_ref.as_context_mut())
        } else {
            let mut store = self.store.borrow_mut();
            f(store.as_context_mut())
        }
    }
}

/// RAII guard restoring `WasmStoreData::vm_addr`, `host_addr`, and `pending_js_exception` on drop.
struct VmPtrGuard<'a> {
    store: wasmtime::StoreContextMut<'a, WasmStoreData>,
    prev_vm: usize,
    prev_host: usize,
    prev_pending_exc: Option<JsException>,
}

impl<'a> VmPtrGuard<'a> {
    fn enter(
        mut store: wasmtime::StoreContextMut<'a, WasmStoreData>,
        vm: &mut JsVm,
        host: &WasmHost,
    ) -> Self {
        let prev_vm = store.data().vm_addr;
        let prev_host = store.data().host_addr;
        let prev_pending_exc = store.data_mut().pending_js_exception.take();
        store.data_mut().vm_addr = std::ptr::from_mut(vm) as usize;
        store.data_mut().host_addr = std::ptr::from_ref(host) as usize;
        Self {
            store,
            prev_vm,
            prev_host,
            prev_pending_exc,
        }
    }
}

impl Drop for VmPtrGuard<'_> {
    fn drop(&mut self) {
        self.store.data_mut().vm_addr = self.prev_vm;
        self.store.data_mut().host_addr = self.prev_host;
        if self.store.data().pending_js_exception.is_none() {
            self.store.data_mut().pending_js_exception = self.prev_pending_exc.take();
        }
    }
}

/// Installs the standard `WebAssembly` global object and all constructors/methods onto `vm`.
pub fn install_webassembly(vm: &mut JsVm) -> SharedWasmHost {
    let mut config = Config::new();
    config.cranelift_opt_level(wasmtime::OptLevel::Speed);
    // Enable full W3C WebAssembly 2.0 + modern standardized proposals.
    config.wasm_simd(true);
    config.wasm_relaxed_simd(true);
    config.wasm_bulk_memory(true);
    config.wasm_multi_value(true);
    config.wasm_multi_memory(true);
    config.wasm_reference_types(true);
    config.wasm_function_references(true);
    config.wasm_tail_call(true);
    config.wasm_extended_const(true);
    config.wasm_memory64(true);
    config.wasm_exceptions(true);
    config.wasm_gc(true);
    config.wasm_threads(true);
    config.shared_memory(true);
    // Keep Wasm virtual address reservations compact so sandboxed Content Processes
    // with strict `RLIMIT_AS` budgets (2 GiB) instantiate Wasm modules cleanly.
    config.memory_reservation(64 * 1024 * 1024);
    config.memory_guard_size(64 * 1024);
    let engine = Engine::new(&config).expect("initialize wasmtime Cranelift engine");
    let memories = Arc::new(Mutex::new(Vec::new()));
    let limits = StoreLimitsBuilder::new()
        .table_elements(10_000_000)
        .trap_on_grow_failure(false)
        .build();
    let mut store = Store::new(
        &engine,
        WasmStoreData {
            vm_addr: 0,
            pending_js_exception: None,
            memories: Arc::clone(&memories),
            host_addr: 0,
            limits,
        },
    );
    store.limiter(|data| &mut data.limits);

    let obj_proto = vm.object_proto;
    let err_proto = vm.error_proto;

    let module_proto = alloc_ordinary(vm, Some(obj_proto));
    let instance_proto = alloc_ordinary(vm, Some(obj_proto));
    let memory_proto = alloc_ordinary(vm, Some(obj_proto));
    let table_proto = alloc_ordinary(vm, Some(obj_proto));
    let global_proto = alloc_ordinary(vm, Some(obj_proto));
    let tag_proto = alloc_ordinary(vm, Some(obj_proto));
    let exception_proto = alloc_ordinary(vm, Some(obj_proto));

    set_to_string_tag(vm, module_proto, "WebAssembly.Module");
    set_to_string_tag(vm, instance_proto, "WebAssembly.Instance");
    set_to_string_tag(vm, memory_proto, "WebAssembly.Memory");
    set_to_string_tag(vm, table_proto, "WebAssembly.Table");
    set_to_string_tag(vm, global_proto, "WebAssembly.Global");
    set_to_string_tag(vm, tag_proto, "WebAssembly.Tag");
    set_to_string_tag(vm, exception_proto, "WebAssembly.Exception");

    let compile_error_proto = alloc_wasm_error_proto(vm, err_proto, "CompileError");
    let link_error_proto = alloc_wasm_error_proto(vm, err_proto, "LinkError");
    let runtime_error_proto = alloc_wasm_error_proto(vm, err_proto, "RuntimeError");

    let roots_arr = vm.alloc_array();

    let host = Rc::new(WasmHost {
        engine,
        store: RefCell::new(store),
        active_ctx_ptr: Cell::new(0),
        modules: RefCell::new(Vec::new()),
        instances: RefCell::new(Vec::new()),
        memories,
        tables: RefCell::new(Vec::new()),
        globals: RefCell::new(Vec::new()),
        tags: RefCell::new(Vec::new()),
        exceptions: RefCell::new(Vec::new()),
        funcs: RefCell::new(Vec::new()),
        func_wrappers: RefCell::new(HashMap::new()),
        anyrefs: RefCell::new(Vec::new()),
        roots_arr,
        module_proto,
        instance_proto,
        memory_proto,
        table_proto,
        global_proto,
        tag_proto,
        exception_proto,
        compile_error_proto,
        link_error_proto,
        runtime_error_proto,
    });

    // Install prototype accessors & methods.
    install_instance_prototype(vm, &host);
    install_memory_prototype(vm, &host);
    install_table_prototype(vm, &host);
    install_global_prototype(vm, &host);
    install_tag_prototype(vm, &host);
    install_exception_prototype(vm, &host);

    // Build `WebAssembly` namespace object.
    let wasm_ns = alloc_ordinary(vm, Some(obj_proto));
    set_to_string_tag(vm, wasm_ns, "WebAssembly");
    define_prop(
        vm,
        wasm_ns,
        ROOTS_KEY,
        JsValue::from_object(roots_arr),
        PropertyAttrs::empty(),
    );

    // Error constructors (`CompileError`, `LinkError`, `RuntimeError`).
    let ce_ctor = make_wasm_error_ctor(vm, "CompileError", compile_error_proto);
    let le_ctor = make_wasm_error_ctor(vm, "LinkError", link_error_proto);
    let re_ctor = make_wasm_error_ctor(vm, "RuntimeError", runtime_error_proto);
    define_prop(
        vm,
        wasm_ns,
        "CompileError",
        JsValue::from_object(ce_ctor),
        PropertyAttrs::METHOD,
    );
    define_prop(
        vm,
        wasm_ns,
        "LinkError",
        JsValue::from_object(le_ctor),
        PropertyAttrs::METHOD,
    );
    define_prop(
        vm,
        wasm_ns,
        "RuntimeError",
        JsValue::from_object(re_ctor),
        PropertyAttrs::METHOD,
    );

    // `WebAssembly.Module` constructor + static `.exports()`, `.imports()`, `.customSections()`.
    let mod_ctor = {
        let h = Rc::clone(&host);
        let ctor = alloc_host_fn_len(vm, "Module", 1, move |vm, _this, args| {
            let proto = require_construct_proto(vm, h.module_proto, "Module")?;
            let bytes =
                extract_buffer_source(vm, args.first().copied().unwrap_or(JsValue::UNDEFINED))?;
            compile_module_object_with_proto(vm, &h, bytes, proto)
        });
        define_prop(
            vm,
            ctor,
            "prototype",
            JsValue::from_object(module_proto),
            PropertyAttrs::empty(),
        );
        define_prop(
            vm,
            module_proto,
            "constructor",
            JsValue::from_object(ctor),
            PropertyAttrs::METHOD,
        );

        let h_exp = Rc::clone(&host);
        let exports_fn = alloc_host_fn_len(vm, "exports", 1, move |vm, _this, args| {
            let mod_id = read_hidden_id(
                vm,
                args.first().copied().unwrap_or(JsValue::UNDEFINED),
                MODULE_ID_KEY,
            )
            .ok_or_else(|| vm.type_error("Argument 0 must be a WebAssembly.Module"))?;
            let modules = h_exp.modules.borrow();
            let (module, _) = modules
                .get(mod_id)
                .ok_or_else(|| vm.type_error("Invalid WebAssembly.Module"))?;
            let entries: Vec<(String, &'static str)> = module
                .exports()
                .map(|e| (e.name().to_string(), extern_kind_name(&e.ty())))
                .collect();
            drop(modules);
            let arr = vm.alloc_array();
            for (name, kind) in entries {
                let obj = alloc_ordinary(vm, Some(vm.object_proto));
                let s_name = JsValue::from_string(vm.heap.alloc_string(name));
                let s_kind = JsValue::from_string(vm.heap.alloc_string(kind));
                define_prop(vm, obj, "name", s_name, PropertyAttrs::DEFAULT);
                define_prop(vm, obj, "kind", s_kind, PropertyAttrs::DEFAULT);
                vm.heap.object_push_element(arr, JsValue::from_object(obj));
            }
            Ok(JsValue::from_object(arr))
        });
        define_prop(
            vm,
            ctor,
            "exports",
            JsValue::from_object(exports_fn),
            PropertyAttrs::DEFAULT,
        );

        let h_imp = Rc::clone(&host);
        let imports_fn = alloc_host_fn_len(vm, "imports", 1, move |vm, _this, args| {
            let mod_id = read_hidden_id(
                vm,
                args.first().copied().unwrap_or(JsValue::UNDEFINED),
                MODULE_ID_KEY,
            )
            .ok_or_else(|| vm.type_error("Argument 0 must be a WebAssembly.Module"))?;
            let modules = h_imp.modules.borrow();
            let (module, _) = modules
                .get(mod_id)
                .ok_or_else(|| vm.type_error("Invalid WebAssembly.Module"))?;
            let entries: Vec<(String, String, &'static str)> = module
                .imports()
                .map(|i| {
                    (
                        i.module().to_string(),
                        i.name().to_string(),
                        extern_kind_name(&i.ty()),
                    )
                })
                .collect();
            drop(modules);
            let arr = vm.alloc_array();
            for (mod_name, name, kind) in entries {
                let obj = alloc_ordinary(vm, Some(vm.object_proto));
                let s_mod = JsValue::from_string(vm.heap.alloc_string(mod_name));
                let s_name = JsValue::from_string(vm.heap.alloc_string(name));
                let s_kind = JsValue::from_string(vm.heap.alloc_string(kind));
                define_prop(vm, obj, "module", s_mod, PropertyAttrs::DEFAULT);
                define_prop(vm, obj, "name", s_name, PropertyAttrs::DEFAULT);
                define_prop(vm, obj, "kind", s_kind, PropertyAttrs::DEFAULT);
                vm.heap.object_push_element(arr, JsValue::from_object(obj));
            }
            Ok(JsValue::from_object(arr))
        });
        define_prop(
            vm,
            ctor,
            "imports",
            JsValue::from_object(imports_fn),
            PropertyAttrs::DEFAULT,
        );

        let h_cs = Rc::clone(&host);
        let custom_fn = alloc_host_fn_len(vm, "customSections", 2, move |vm, _this, args| {
            if args.len() < 2 {
                return Err(vm.type_error("WebAssembly.Module.customSections requires 2 arguments"));
            }
            let mod_id = read_hidden_id(vm, args[0], MODULE_ID_KEY)
                .ok_or_else(|| vm.type_error("Argument 0 must be a WebAssembly.Module"))?;
            let target_name = vm.to_js_string(args[1])?;
            let sections: Vec<Vec<u8>> = {
                let modules = h_cs.modules.borrow();
                let (_, raw_bytes) = modules
                    .get(mod_id)
                    .ok_or_else(|| vm.type_error("Invalid WebAssembly.Module"))?;
                parse_custom_sections(raw_bytes, &target_name)
            };
            let arr = vm.alloc_array();
            for sec in sections {
                let ab = alloc_array_buffer(vm, sec, None, false);
                vm.heap.object_push_element(arr, JsValue::from_object(ab));
            }
            Ok(JsValue::from_object(arr))
        });
        define_prop(
            vm,
            ctor,
            "customSections",
            JsValue::from_object(custom_fn),
            PropertyAttrs::DEFAULT,
        );
        ctor
    };
    define_prop(
        vm,
        wasm_ns,
        "Module",
        JsValue::from_object(mod_ctor),
        PropertyAttrs::METHOD,
    );
    vm.mark_constructor(mod_ctor);

    // `WebAssembly.Instance` constructor.
    let inst_ctor = {
        let h = Rc::clone(&host);
        let ctor = alloc_host_fn_len(vm, "Instance", 1, move |vm, _this, args| {
            let proto = require_construct_proto(vm, h.instance_proto, "Instance")?;
            let mod_val = args.first().copied().unwrap_or(JsValue::UNDEFINED);
            let import_obj = args.get(1).copied().unwrap_or(JsValue::UNDEFINED);
            instantiate_module_object_with_proto(vm, &h, mod_val, import_obj, proto)
        });
        define_prop(
            vm,
            ctor,
            "prototype",
            JsValue::from_object(instance_proto),
            PropertyAttrs::empty(),
        );
        define_prop(
            vm,
            instance_proto,
            "constructor",
            JsValue::from_object(ctor),
            PropertyAttrs::METHOD,
        );
        ctor
    };
    define_prop(
        vm,
        wasm_ns,
        "Instance",
        JsValue::from_object(inst_ctor),
        PropertyAttrs::METHOD,
    );
    vm.mark_constructor(inst_ctor);

    // `WebAssembly.Memory` constructor.
    let mem_ctor = {
        let h = Rc::clone(&host);
        let ctor = alloc_host_fn_len(vm, "Memory", 1, move |vm, _this, args| {
            let proto = require_construct_proto(vm, h.memory_proto, "Memory")?;
            let desc = args.first().copied().unwrap_or(JsValue::UNDEFINED);
            let Some(desc_h) = desc.as_object() else {
                return Err(vm.type_error("WebAssembly.Memory descriptor must be an object"));
            };

            // WebIDL dictionary member access in alphabetical order:
            // address, initial, maximum, minimum, shared
            let k_addr = vm.heap.intern_key("address");
            let addr_v = vm.get_property_by_key(desc_h, k_addr)?;
            let is_64 = parse_address_type(vm, addr_v)?;

            let k_init = vm.heap.intern_key("initial");
            let init_v = vm.get_property_by_key(desc_h, k_init)?;
            let initial_opt = if init_v.is_undefined() {
                None
            } else {
                Some(to_wasm_index_limit(vm, init_v, is_64, "initial")?)
            };

            let k_max = vm.heap.intern_key("maximum");
            let max_v = vm.get_property_by_key(desc_h, k_max)?;
            let maximum = if max_v.is_undefined() {
                None
            } else {
                Some(to_wasm_index_limit(vm, max_v, is_64, "maximum")?)
            };

            let k_min = vm.heap.intern_key("minimum");
            let min_v = vm.get_property_by_key(desc_h, k_min)?;
            let minimum_opt = if min_v.is_undefined() {
                None
            } else {
                Some(to_wasm_index_limit(vm, min_v, is_64, "minimum")?)
            };

            let k_shared = vm.heap.intern_key("shared");
            let shared_v = vm.get_property_by_key(desc_h, k_shared)?;
            let is_shared = is_js_truthy(vm, shared_v);

            let initial = match (initial_opt, minimum_opt) {
                (Some(_), Some(_)) => {
                    return Err(vm.type_error(
                        "WebAssembly.Memory descriptor cannot specify both 'initial' and 'minimum'",
                    ));
                }
                (None, None) => {
                    return Err(vm.type_error(
                        "WebAssembly.Memory descriptor requires 'initial' (or 'minimum')",
                    ));
                }
                (Some(v), None) | (None, Some(v)) => v,
            };

            let page_ceiling = if is_64 {
                MAX_WASM64_PAGES
            } else {
                MAX_WASM32_PAGES
            };
            if initial > page_ceiling {
                return Err(vm.range_error("Memory initial exceeds maximum page limit"));
            }
            if let Some(m) = maximum {
                if m < initial {
                    return Err(vm.range_error("Memory maximum pages cannot be less than initial"));
                }
                if m > page_ceiling {
                    return Err(vm.range_error("Memory maximum exceeds maximum page limit"));
                }
            }
            if is_shared && maximum.is_none() {
                return Err(vm.type_error("Shared WebAssembly.Memory requires a 'maximum' limit"));
            }

            let mem_id = if is_shared {
                let max_p = maximum.unwrap_or(initial);
                let mut builder = wasmtime::MemoryTypeBuilder::new();
                builder.min(initial).max(Some(max_p)).shared(true);
                if is_64 {
                    builder.memory64(true);
                }
                let mem_ty = builder.build().map_err(|e| {
                    vm.range_error(format!("Invalid shared WebAssembly.Memory type: {e}"))
                })?;
                let smem = SharedMemory::new(&h.engine, mem_ty).map_err(|e| {
                    vm.range_error(format!("Failed to allocate shared WebAssembly.Memory: {e}"))
                })?;
                register_shared_memory(vm, &h, smem, is_64, maximum)
            } else {
                let mem_ty = if is_64 {
                    MemoryType::new64(initial, maximum)
                } else {
                    MemoryType::new(initial as u32, maximum.map(|m| m as u32))
                };
                let mem = h.with_store_mut(|mut store| {
                    Memory::new(&mut store, mem_ty).map_err(|e| {
                        vm.range_error(format!("Failed to allocate WebAssembly.Memory: {e}"))
                    })
                })?;
                register_memory(vm, &h, mem)
            };

            let wrapper = wrap_memory_object_with_proto(vm, &h, mem_id, proto);
            Ok(JsValue::from_object(wrapper))
        });
        define_prop(
            vm,
            ctor,
            "prototype",
            JsValue::from_object(memory_proto),
            PropertyAttrs::empty(),
        );
        define_prop(
            vm,
            memory_proto,
            "constructor",
            JsValue::from_object(ctor),
            PropertyAttrs::METHOD,
        );
        ctor
    };
    define_prop(
        vm,
        wasm_ns,
        "Memory",
        JsValue::from_object(mem_ctor),
        PropertyAttrs::METHOD,
    );
    vm.mark_constructor(mem_ctor);

    // `WebAssembly.Table` constructor.
    let table_ctor = {
        let h = Rc::clone(&host);
        let ctor = alloc_host_fn_len(vm, "Table", 1, move |vm, _this, args| {
            let proto = require_construct_proto(vm, h.table_proto, "Table")?;
            let desc = args.first().copied().unwrap_or(JsValue::UNDEFINED);
            let Some(desc_h) = desc.as_object() else {
                return Err(vm.type_error("WebAssembly.Table descriptor must be an object"));
            };

            // WebIDL dictionary order: address, element, initial, maximum, minimum
            let k_addr = vm.heap.intern_key("address");
            let addr_v = vm.get_property_by_key(desc_h, k_addr)?;
            let is_64 = parse_address_type(vm, addr_v)?;

            let k_elem = vm.heap.intern_key("element");
            let elem_v = vm.get_property_by_key(desc_h, k_elem)?;
            if elem_v.is_undefined() {
                return Err(vm.type_error("WebAssembly.Table descriptor requires 'element'"));
            }
            let elem_s = vm.to_js_string(elem_v)?;
            let ref_ty = parse_table_ref_type(vm, &elem_s)?;

            let k_init = vm.heap.intern_key("initial");
            let init_v = vm.get_property_by_key(desc_h, k_init)?;
            let initial_opt = if init_v.is_undefined() {
                None
            } else {
                Some(to_wasm_index_limit(vm, init_v, is_64, "initial")?)
            };

            let k_max = vm.heap.intern_key("maximum");
            let max_v = vm.get_property_by_key(desc_h, k_max)?;
            let maximum = if max_v.is_undefined() {
                None
            } else {
                Some(to_wasm_index_limit(vm, max_v, is_64, "maximum")?)
            };

            let k_min = vm.heap.intern_key("minimum");
            let min_v = vm.get_property_by_key(desc_h, k_min)?;
            let minimum_opt = if min_v.is_undefined() {
                None
            } else {
                Some(to_wasm_index_limit(vm, min_v, is_64, "minimum")?)
            };

            let initial = match (initial_opt, minimum_opt) {
                (Some(_), Some(_)) => {
                    return Err(vm.type_error(
                        "WebAssembly.Table descriptor cannot specify both 'initial' and 'minimum'",
                    ));
                }
                (None, None) => {
                    return Err(vm.type_error(
                        "WebAssembly.Table descriptor requires 'initial' (or 'minimum')",
                    ));
                }
                (Some(v), None) | (None, Some(v)) => v,
            };

            if let Some(m) = maximum {
                if m < initial {
                    return Err(vm.range_error("Table maximum cannot be less than initial"));
                }
            }

            let init_ref = if args.len() < 2 {
                default_wasm_ref(vm, &h, &ref_ty)
            } else {
                js_to_wasm_ref(vm, &h, args[1], &ref_ty)?
            };

            let table_ty = if is_64 {
                TableType::new64(ref_ty, initial, maximum)
            } else {
                TableType::new(ref_ty, initial as u32, maximum.map(|m| m as u32))
            };
            let table = h.with_store_mut(|mut store| {
                Table::new(&mut store, table_ty, init_ref)
                    .map_err(|e| vm.range_error(format!("Failed to create WebAssembly.Table: {e}")))
            })?;
            let tid = register_table(&h, table);
            let wrapper = wrap_table_object_with_proto(vm, &h, tid, proto);
            Ok(JsValue::from_object(wrapper))
        });
        define_prop(
            vm,
            ctor,
            "prototype",
            JsValue::from_object(table_proto),
            PropertyAttrs::empty(),
        );
        define_prop(
            vm,
            table_proto,
            "constructor",
            JsValue::from_object(ctor),
            PropertyAttrs::METHOD,
        );
        ctor
    };
    define_prop(
        vm,
        wasm_ns,
        "Table",
        JsValue::from_object(table_ctor),
        PropertyAttrs::METHOD,
    );
    vm.mark_constructor(table_ctor);

    // `WebAssembly.Global` constructor.
    let global_ctor = {
        let h = Rc::clone(&host);
        let ctor = alloc_host_fn_len(vm, "Global", 1, move |vm, _this, args| {
            let proto = require_construct_proto(vm, h.global_proto, "Global")?;
            let desc = args.first().copied().unwrap_or(JsValue::UNDEFINED);
            let Some(desc_h) = desc.as_object() else {
                return Err(vm.type_error("WebAssembly.Global descriptor must be an object"));
            };
            // WebIDL dictionary order: mutable, then value!
            let k_mut = vm.heap.intern_key("mutable");
            let mut_v = vm.get_property_by_key(desc_h, k_mut)?;
            let mutability = if is_js_truthy(vm, mut_v) {
                Mutability::Var
            } else {
                Mutability::Const
            };
            let k_val = vm.heap.intern_key("value");
            let ty_val = vm.get_property_by_key(desc_h, k_val)?;
            if ty_val.is_undefined() {
                return Err(vm.type_error("WebAssembly.Global descriptor requires 'value'"));
            }
            let ty_str = vm.to_js_string(ty_val)?;
            let val_ty = parse_js_val_type(vm, &ty_str, false)?;
            let init_val = if args.len() < 2 || args[1].is_undefined() {
                default_wasm_val_js(vm, &h, &val_ty)
            } else {
                js_to_wasm_val(vm, &h, args[1], &val_ty)?
            };
            let global = h.with_store_mut(|mut store| {
                Global::new(&mut store, GlobalType::new(val_ty, mutability), init_val)
                    .map_err(|e| vm.type_error(format!("Failed to create WebAssembly.Global: {e}")))
            })?;
            let gid = register_global(&h, global);
            let wrapper = wrap_global_object_with_proto(vm, &h, gid, proto);
            Ok(JsValue::from_object(wrapper))
        });
        define_prop(
            vm,
            ctor,
            "prototype",
            JsValue::from_object(global_proto),
            PropertyAttrs::empty(),
        );
        define_prop(
            vm,
            global_proto,
            "constructor",
            JsValue::from_object(ctor),
            PropertyAttrs::METHOD,
        );
        ctor
    };
    define_prop(
        vm,
        wasm_ns,
        "Global",
        JsValue::from_object(global_ctor),
        PropertyAttrs::METHOD,
    );
    vm.mark_constructor(global_ctor);

    // `WebAssembly.Tag` constructor.
    let tag_ctor = {
        let h = Rc::clone(&host);
        let ctor = alloc_host_fn_len(vm, "Tag", 1, move |vm, _this, args| {
            let proto = require_construct_proto(vm, h.tag_proto, "Tag")?;
            let desc = args.first().copied().unwrap_or(JsValue::UNDEFINED);
            let Some(desc_h) = desc.as_object() else {
                return Err(vm.type_error("WebAssembly.Tag descriptor must be an object"));
            };
            let k_params = vm.heap.intern_key("parameters");
            let params_v = vm.get_property_by_key(desc_h, k_params)?;
            if params_v.is_undefined() || params_v.is_null() {
                return Err(vm.type_error("WebAssembly.Tag descriptor requires 'parameters'"));
            }
            let items = vm.collect_iterable(params_v)?;
            let mut param_tys = Vec::with_capacity(items.len());
            for item in items {
                let s = vm.to_js_string(item)?;
                param_tys.push(parse_js_val_type(vm, &s, true)?);
            }
            let tag_ty = TagType::new(FuncType::new(&h.engine, param_tys, []));
            let tag = h.with_store_mut(|mut store| {
                Tag::new(&mut store, &tag_ty)
                    .map_err(|e| vm.type_error(format!("Failed to create WebAssembly.Tag: {e}")))
            })?;
            let tid = register_tag(&h, tag);
            let wrapper = wrap_tag_object_with_proto(vm, &h, tid, proto);
            Ok(JsValue::from_object(wrapper))
        });
        define_prop(
            vm,
            ctor,
            "prototype",
            JsValue::from_object(tag_proto),
            PropertyAttrs::empty(),
        );
        define_prop(
            vm,
            tag_proto,
            "constructor",
            JsValue::from_object(ctor),
            PropertyAttrs::METHOD,
        );
        ctor
    };
    define_prop(
        vm,
        wasm_ns,
        "Tag",
        JsValue::from_object(tag_ctor),
        PropertyAttrs::METHOD,
    );
    vm.mark_constructor(tag_ctor);

    // `WebAssembly.Exception` constructor.
    let exn_ctor = {
        let h = Rc::clone(&host);
        let ctor = alloc_host_fn_len(vm, "Exception", 2, move |vm, _this, args| {
            let proto = require_construct_proto(vm, h.exception_proto, "Exception")?;
            if args.len() < 2 {
                return Err(vm.type_error("WebAssembly.Exception requires tag and payload"));
            }
            let tag_id = read_hidden_id(vm, args[0], TAG_ID_KEY)
                .ok_or_else(|| vm.type_error("Argument 0 must be a WebAssembly.Tag"))?;
            let (tag, param_tys) = {
                let tags = h.tags.borrow();
                let t = tags
                    .get(tag_id)
                    .ok_or_else(|| vm.type_error("Invalid WebAssembly.Tag"))?
                    .tag;
                let tys: Vec<ValType> = h.with_store(|store| t.ty(&store).ty().params().collect());
                (t, tys)
            };
            let payload_items = vm.collect_iterable(args[1])?;
            if payload_items.len() != param_tys.len() {
                return Err(vm.type_error("WebAssembly.Exception payload length mismatch"));
            }
            let mut wasm_fields = Vec::with_capacity(param_tys.len());
            for (val, ty) in payload_items.into_iter().zip(param_tys.iter()) {
                if matches!(ty, ValType::V128) {
                    return Err(vm.type_error("Cannot construct WebAssembly.Exception with v128"));
                }
                wasm_fields.push(js_to_wasm_val(vm, &h, val, ty)?);
            }

            let trace_stack = if let Some(opts) = args.get(2).copied() {
                if opts.is_undefined() || opts.is_null() {
                    false
                } else if let Some(oh) = opts.as_object() {
                    let k_ts = vm.heap.intern_key("traceStack");
                    let v = vm.get_property_by_key(oh, k_ts)?;
                    is_js_truthy(vm, v)
                } else {
                    return Err(vm.type_error("WebAssembly.Exception options must be an object"));
                }
            } else {
                false
            };

            let exn_ty = ExnType::new(&h.engine, param_tys)
                .map_err(|e| vm.type_error(format!("Invalid exception type: {e}")))?;
            let owned = h.with_store_mut(|mut store| {
                let pre = ExnRefPre::new(&mut store, exn_ty);
                let rooted = ExnRef::new(&mut store, &pre, &tag, &wasm_fields).map_err(|e| {
                    vm.type_error(format!("Failed to create WebAssembly.Exception: {e}"))
                })?;
                rooted
                    .to_owned_rooted(&mut store)
                    .map_err(|e| vm.type_error(format!("Failed to root exception: {e}")))
            })?;
            let stack = trace_stack.then(|| "WebAssembly.Exception".to_string());
            let eid = register_exception(&h, owned, stack);
            let obj = alloc_ordinary(vm, Some(proto));
            define_hidden_id(vm, obj, EXN_ID_KEY, eid);
            Ok(JsValue::from_object(obj))
        });
        define_prop(
            vm,
            ctor,
            "prototype",
            JsValue::from_object(exception_proto),
            PropertyAttrs::empty(),
        );
        define_prop(
            vm,
            exception_proto,
            "constructor",
            JsValue::from_object(ctor),
            PropertyAttrs::METHOD,
        );
        ctor
    };
    define_prop(
        vm,
        wasm_ns,
        "Exception",
        JsValue::from_object(exn_ctor),
        PropertyAttrs::METHOD,
    );
    vm.mark_constructor(exn_ctor);

    // `WebAssembly.validate(bytes)`
    let validate_fn = {
        let h = Rc::clone(&host);
        alloc_host_fn_len(vm, "validate", 1, move |vm, _this, args| {
            let bytes =
                extract_buffer_source(vm, args.first().copied().unwrap_or(JsValue::UNDEFINED))?;
            let valid = Module::validate(&h.engine, &bytes).is_ok();
            Ok(JsValue::from_bool(valid))
        })
    };
    define_prop(
        vm,
        wasm_ns,
        "validate",
        JsValue::from_object(validate_fn),
        PropertyAttrs::DEFAULT,
    );

    // `WebAssembly.compile(bytes, options?) -> Promise<Module>`
    let compile_fn = {
        let h = Rc::clone(&host);
        alloc_host_fn_len(vm, "compile", 1, move |vm, _this, args| {
            let bytes = match extract_buffer_source(
                vm,
                args.first().copied().unwrap_or(JsValue::UNDEFINED),
            ) {
                Ok(b) => b,
                Err(exc) => {
                    let p = alloc_promise(vm);
                    reject_promise(vm, p, exc.value);
                    return Ok(JsValue::from_object(p));
                }
            };
            let tick = promise_resolve_value(vm, JsValue::UNDEFINED);
            let h_mod = Rc::clone(&h);
            let bytes_cell = RefCell::new(Some(bytes));
            let on_tick = vm.alloc_host_fn("wasmCompileTask", move |vm, _this, _a| {
                let raw = bytes_cell.borrow_mut().take().unwrap_or_default();
                compile_module_object_with_proto(vm, &h_mod, raw, h_mod.module_proto)
            });
            let chained = promise_then(vm, tick, Some(on_tick), None);
            Ok(JsValue::from_object(chained))
        })
    };
    define_prop(
        vm,
        wasm_ns,
        "compile",
        JsValue::from_object(compile_fn),
        PropertyAttrs::DEFAULT,
    );

    // `WebAssembly.instantiate(bytesOrModule, importObject?, options?) -> Promise<{module, instance} | Instance>`
    let instantiate_fn = {
        let h = Rc::clone(&host);
        alloc_host_fn_len(vm, "instantiate", 1, move |vm, _this, args| {
            let first = args.first().copied().unwrap_or(JsValue::UNDEFINED);
            let imports = args.get(1).copied().unwrap_or(JsValue::UNDEFINED);
            if read_hidden_id(vm, first, MODULE_ID_KEY).is_some() {
                // W3C JS-API: `WebAssembly.instantiate(module, imports)` reads `imports` synchronously!
                let p = alloc_promise(vm);
                match instantiate_module_object_with_proto(vm, &h, first, imports, h.instance_proto)
                {
                    Ok(inst_val) => {
                        let resolved = promise_resolve_value(vm, inst_val);
                        Ok(JsValue::from_object(resolved))
                    }
                    Err(exc) => {
                        reject_promise(vm, p, exc.value);
                        Ok(JsValue::from_object(p))
                    }
                }
            } else {
                let bytes = match extract_buffer_source(vm, first) {
                    Ok(b) => b,
                    Err(exc) => {
                        let p = alloc_promise(vm);
                        reject_promise(vm, p, exc.value);
                        return Ok(JsValue::from_object(p));
                    }
                };
                if !imports.is_undefined() && !imports.is_object() {
                    let p = alloc_promise(vm);
                    let exc = vm.type_error("importObject must be an object or undefined");
                    reject_promise(vm, p, exc.value);
                    return Ok(JsValue::from_object(p));
                }
                let tick = promise_resolve_value(vm, JsValue::UNDEFINED);
                let h_inst = Rc::clone(&h);
                let bytes_cell = RefCell::new(Some(bytes));
                let on_tick = vm.alloc_host_fn_rooted(
                    "wasmInstantiateBytesTask",
                    &[imports],
                    move |vm, _t, _a| {
                        let raw = bytes_cell.borrow_mut().take().unwrap_or_default();
                        let mod_val = compile_module_object_with_proto(
                            vm,
                            &h_inst,
                            raw,
                            h_inst.module_proto,
                        )?;
                        let inst_val = instantiate_module_object_with_proto(
                            vm,
                            &h_inst,
                            mod_val,
                            imports,
                            h_inst.instance_proto,
                        )?;
                        let pair = alloc_ordinary(vm, Some(vm.object_proto));
                        define_prop(vm, pair, "module", mod_val, PropertyAttrs::DEFAULT);
                        define_prop(vm, pair, "instance", inst_val, PropertyAttrs::DEFAULT);
                        Ok(JsValue::from_object(pair))
                    },
                );
                let chained = promise_then(vm, tick, Some(on_tick), None);
                Ok(JsValue::from_object(chained))
            }
        })
    };
    define_prop(
        vm,
        wasm_ns,
        "instantiate",
        JsValue::from_object(instantiate_fn),
        PropertyAttrs::DEFAULT,
    );

    // `WebAssembly.compileStreaming(source) -> Promise<Module>`
    let compile_streaming_fn = {
        let h = Rc::clone(&host);
        alloc_host_fn_len(vm, "compileStreaming", 1, move |vm, _this, args| {
            let source = args.first().copied().unwrap_or(JsValue::UNDEFINED);
            Ok(JsValue::from_object(compile_streaming_impl(vm, &h, source)))
        })
    };
    define_prop(
        vm,
        wasm_ns,
        "compileStreaming",
        JsValue::from_object(compile_streaming_fn),
        PropertyAttrs::DEFAULT,
    );

    // `WebAssembly.instantiateStreaming(source, importObject?) -> Promise<{module, instance}>`
    let instantiate_streaming_fn = {
        let h = Rc::clone(&host);
        alloc_host_fn_len(vm, "instantiateStreaming", 1, move |vm, _this, args| {
            let source = args.first().copied().unwrap_or(JsValue::UNDEFINED);
            let import_obj = args.get(1).copied().unwrap_or(JsValue::UNDEFINED);
            let mod_promise = compile_streaming_impl(vm, &h, source);
            let h_inst = Rc::clone(&h);
            let on_mod = vm.alloc_host_fn_rooted(
                "wasmInstantiateStreamingThen",
                &[import_obj],
                move |vm, _this, margs| {
                    let mod_val = margs.first().copied().unwrap_or(JsValue::UNDEFINED);
                    let inst_val = instantiate_module_object_with_proto(
                        vm,
                        &h_inst,
                        mod_val,
                        import_obj,
                        h_inst.instance_proto,
                    )?;
                    let pair = alloc_ordinary(vm, Some(vm.object_proto));
                    define_prop(vm, pair, "module", mod_val, PropertyAttrs::DEFAULT);
                    define_prop(vm, pair, "instance", inst_val, PropertyAttrs::DEFAULT);
                    Ok(JsValue::from_object(pair))
                },
            );
            let chained = promise_then(vm, mod_promise, Some(on_mod), None);
            Ok(JsValue::from_object(chained))
        })
    };
    define_prop(
        vm,
        wasm_ns,
        "instantiateStreaming",
        JsValue::from_object(instantiate_streaming_fn),
        PropertyAttrs::DEFAULT,
    );

    // Attach `WebAssembly` onto global object.
    let g = vm.global;
    define_prop(
        vm,
        g,
        "WebAssembly",
        JsValue::from_object(wasm_ns),
        PropertyAttrs::METHOD,
    );

    host
}

// --- Streaming Compilation Helper (`Response` / `Promise<Response>`) ---

fn compile_streaming_impl(vm: &mut JsVm, host: &SharedWasmHost, source: JsValue) -> HeapHandle {
    let source_promise = promise_resolve_value(vm, source);
    let h_compile = Rc::clone(host);
    let on_response = vm.alloc_host_fn("wasmCompileStreamingResponse", move |vm, _this, args| {
        let resp = args.first().copied().unwrap_or(JsValue::UNDEFINED);
        let Some(resp_h) = resp.as_object() else {
            return Err(vm.type_error("WebAssembly streaming source must resolve to a Response"));
        };
        let k_ok = vm.heap.intern_key("ok");
        let ok_v = vm.get_property_by_key(resp_h, k_ok)?;
        if !ok_v.is_undefined() && !is_js_truthy(vm, ok_v) {
            return Err(vm.type_error("WebAssembly streaming Response is not OK"));
        }
        // Validate MIME type if `response.headers.get("content-type")` is present.
        let k_headers = vm.heap.intern_key("headers");
        let headers_v = vm.get_property_by_key(resp_h, k_headers)?;
        if let Some(hdr_h) = headers_v.as_object() {
            let k_get = vm.heap.intern_key("get");
            let get_fn = vm.get_property_by_key(hdr_h, k_get)?;
            if get_fn.is_object() {
                let ct_key = JsValue::from_string(vm.heap.alloc_string("content-type"));
                let ct_v = vm.call_value(get_fn, headers_v, &[ct_key])?;
                if !ct_v.is_null() && !ct_v.is_undefined() {
                    let ct = vm.to_js_string(ct_v)?.to_ascii_lowercase();
                    let mime = ct.split(';').next().unwrap_or("").trim();
                    if !mime.is_empty() && mime != "application/wasm" {
                        return Err(vm.type_error(format!(
                            "Incorrect response MIME type '{mime}'. Expected 'application/wasm'"
                        )));
                    }
                }
            }
        }
        let k_ab = vm.heap.intern_key("arrayBuffer");
        let ab_fn = vm.get_property_by_key(resp_h, k_ab)?;
        if !ab_fn.is_object() {
            return Err(vm.type_error("Streaming Response missing arrayBuffer() method"));
        }
        let ab_prom_val = vm.call_value(ab_fn, resp, &[])?;
        let ab_prom = promise_resolve_value(vm, ab_prom_val);
        let h_mod = Rc::clone(&h_compile);
        let on_buf = vm.alloc_host_fn("wasmCompileStreamingBytes", move |vm, _this, bargs| {
            let bytes =
                extract_buffer_source(vm, bargs.first().copied().unwrap_or(JsValue::UNDEFINED))?;
            compile_module_object_with_proto(vm, &h_mod, bytes, h_mod.module_proto)
        });
        Ok(JsValue::from_object(promise_then(
            vm,
            ab_prom,
            Some(on_buf),
            None,
        )))
    });
    promise_then(vm, source_promise, Some(on_response), None)
}

// --- Module Compilation & Instantiation ---

fn compile_module_object_with_proto(
    vm: &mut JsVm,
    host: &SharedWasmHost,
    bytes: Vec<u8>,
    proto: HeapHandle,
) -> Result<JsValue, JsException> {
    let module = Module::new(&host.engine, &bytes).map_err(|e| {
        make_wasm_error(vm, host.compile_error_proto, "CompileError", e.to_string())
    })?;
    let mod_id = {
        let mut mods = host.modules.borrow_mut();
        let id = mods.len();
        mods.push((module, bytes));
        id
    };
    let obj = alloc_ordinary(vm, Some(proto));
    define_hidden_id(vm, obj, MODULE_ID_KEY, mod_id);
    Ok(JsValue::from_object(obj))
}

fn instantiate_module_object_with_proto(
    vm: &mut JsVm,
    host: &SharedWasmHost,
    mod_val: JsValue,
    import_obj: JsValue,
    proto: HeapHandle,
) -> Result<JsValue, JsException> {
    let mod_id = read_hidden_id(vm, mod_val, MODULE_ID_KEY)
        .ok_or_else(|| vm.type_error("Argument 0 must be a WebAssembly.Module"))?;
    if !import_obj.is_undefined() && !import_obj.is_object() {
        return Err(vm.type_error("importObject must be an object or undefined"));
    }
    let (module, raw_bytes) = {
        let mods = host.modules.borrow();
        mods.get(mod_id)
            .map(|(m, b)| (m.clone(), b.clone()))
            .ok_or_else(|| vm.type_error("Invalid WebAssembly.Module"))?
    };

    let import_specs: Vec<(String, String, ExternType)> = module
        .imports()
        .map(|i| (i.module().to_string(), i.name().to_string(), i.ty()))
        .collect();

    if !import_specs.is_empty() && !import_obj.is_object() {
        return Err(vm.type_error("importObject must be an object when module has imports"));
    }

    // Track imported entity IDs in index order so re-exports preserve exact JS object identity.
    let mut resolved_imports: Vec<Extern> = Vec::with_capacity(import_specs.len());
    let mut imported_func_wrappers: Vec<Option<HeapHandle>> = Vec::new();
    let mut imported_mem_ids: Vec<usize> = Vec::new();
    let mut imported_table_ids: Vec<usize> = Vec::new();
    let mut imported_global_ids: Vec<Option<usize>> = Vec::new();
    let mut imported_tag_ids: Vec<usize> = Vec::new();

    for (imp_mod, imp_name, imp_ty) in import_specs {
        let mod_h = import_obj
            .as_object()
            .ok_or_else(|| vm.type_error(format!("Missing import namespace '{imp_mod}'")))?;
        let k_mod = vm.heap.intern_key(&imp_mod);
        let ns_val = vm.get_property_by_key(mod_h, k_mod)?;
        let Some(ns_h) = ns_val.as_object() else {
            return Err(vm.type_error(format!("Import namespace '{imp_mod}' is not an object")));
        };
        let k_name = vm.heap.intern_key(&imp_name);
        let item_val = vm.get_property_by_key(ns_h, k_name)?;

        match imp_ty {
            ExternType::Func(func_ty) => {
                if let Some(fid) = read_hidden_id(vm, item_val, FUNC_ID_KEY) {
                    let f = *host.funcs.borrow().get(fid).ok_or_else(|| {
                        make_wasm_error(
                            vm,
                            host.link_error_proto,
                            "LinkError",
                            "Invalid Wasm function import",
                        )
                    })?;
                    imported_func_wrappers.push(item_val.as_object());
                    resolved_imports.push(Extern::Func(f));
                } else if item_val
                    .as_object()
                    .is_some_and(|h| vm.is_callable_handle(h))
                {
                    root_js_value(vm, host, item_val);
                    let host_func = wrap_js_fn_as_wasm_import(host, item_val, func_ty);
                    imported_func_wrappers.push(None);
                    resolved_imports.push(Extern::Func(host_func));
                } else {
                    return Err(make_wasm_error(
                        vm,
                        host.link_error_proto,
                        "LinkError",
                        format!("Import '{imp_mod}.{imp_name}' must be a callable function"),
                    ));
                }
            }
            ExternType::Memory(_) => {
                let Some(mid) = read_hidden_id(vm, item_val, MEMORY_ID_KEY) else {
                    return Err(make_wasm_error(
                        vm,
                        host.link_error_proto,
                        "LinkError",
                        format!("Import '{imp_mod}.{imp_name}' must be a WebAssembly.Memory"),
                    ));
                };
                let ext = {
                    let mems = host.memories.lock().expect("wasm memories lock");
                    match mems.get(mid).map(|b| &b.kind) {
                        Some(WasmMemKind::Unshared(m)) => Extern::Memory(*m),
                        Some(WasmMemKind::Shared(sm)) => Extern::SharedMemory(sm.clone()),
                        None => {
                            return Err(make_wasm_error(
                                vm,
                                host.link_error_proto,
                                "LinkError",
                                "Invalid Memory import",
                            ));
                        }
                    }
                };
                imported_mem_ids.push(mid);
                resolved_imports.push(ext);
            }
            ExternType::Table(_) => {
                let Some(tid) = read_hidden_id(vm, item_val, TABLE_ID_KEY) else {
                    return Err(make_wasm_error(
                        vm,
                        host.link_error_proto,
                        "LinkError",
                        format!("Import '{imp_mod}.{imp_name}' must be a WebAssembly.Table"),
                    ));
                };
                let tbl = host
                    .tables
                    .borrow()
                    .get(tid)
                    .map(|e| e.table)
                    .ok_or_else(|| {
                        make_wasm_error(
                            vm,
                            host.link_error_proto,
                            "LinkError",
                            "Invalid Table import",
                        )
                    })?;
                imported_table_ids.push(tid);
                resolved_imports.push(Extern::Table(tbl));
            }
            ExternType::Global(g_ty) => {
                if let Some(gid) = read_hidden_id(vm, item_val, GLOBAL_ID_KEY) {
                    let g = host
                        .globals
                        .borrow()
                        .get(gid)
                        .map(|e| e.global)
                        .ok_or_else(|| {
                            make_wasm_error(
                                vm,
                                host.link_error_proto,
                                "LinkError",
                                "Invalid Global import",
                            )
                        })?;
                    imported_global_ids.push(Some(gid));
                    resolved_imports.push(Extern::Global(g));
                } else if g_ty.mutability() == Mutability::Const {
                    // W3C JS-API: immutable global imports accept plain JS Number / BigInt / Ref,
                    // throwing LinkError on mismatched primitive type.
                    let val = js_to_wasm_import_global(vm, host, item_val, g_ty.content())?;
                    let g = host.with_store_mut(|mut store| {
                        Global::new(&mut store, g_ty, val).map_err(|e| {
                            make_wasm_error(vm, host.link_error_proto, "LinkError", e.to_string())
                        })
                    })?;
                    imported_global_ids.push(None);
                    resolved_imports.push(Extern::Global(g));
                } else {
                    return Err(make_wasm_error(
                        vm,
                        host.link_error_proto,
                        "LinkError",
                        format!(
                            "Mutable global import '{imp_mod}.{imp_name}' must be a WebAssembly.Global"
                        ),
                    ));
                }
            }
            ExternType::Tag(_) => {
                let Some(tid) = read_hidden_id(vm, item_val, TAG_ID_KEY) else {
                    return Err(make_wasm_error(
                        vm,
                        host.link_error_proto,
                        "LinkError",
                        format!("Import '{imp_mod}.{imp_name}' must be a WebAssembly.Tag"),
                    ));
                };
                let tag = host.tags.borrow().get(tid).map(|e| e.tag).ok_or_else(|| {
                    make_wasm_error(vm, host.link_error_proto, "LinkError", "Invalid Tag import")
                })?;
                imported_tag_ids.push(tid);
                resolved_imports.push(Extern::Tag(tag));
            }
        }
    }

    // Sync any imported JS memory buffers into Wasmtime BEFORE start-function / data segment init.
    sync_all_js_to_wasm(vm, host);

    let inst_result = host.with_store_mut(|store| {
        let mut guard = VmPtrGuard::enter(store, vm, host);
        let res = Instance::new(&mut guard.store, &module, &resolved_imports);
        let js_exc = guard.store.data_mut().pending_js_exception.take();
        let wasm_exn = guard
            .store
            .take_pending_exception()
            .and_then(|r| r.to_owned_rooted(&mut guard.store).ok());
        (res, js_exc, wasm_exn)
    });

    let instance = match inst_result {
        (Ok(inst), _, _) => inst,
        (Err(_), Some(js_exc), _) => {
            sync_all_wasm_to_js(vm, host);
            return Err(js_exc);
        }
        (Err(_), None, Some(wasm_exn)) => {
            sync_all_wasm_to_js(vm, host);
            return Err(wrap_thrown_wasm_exception(vm, host, wasm_exn));
        }
        (Err(err), None, None) => {
            sync_all_wasm_to_js(vm, host);
            return Err(classify_instantiation_error(vm, host, err));
        }
    };

    // Sync Wasm data-segment writes back to imported JS `ArrayBuffer`s.
    sync_all_wasm_to_js(vm, host);

    let inst_id = {
        let mut insts = host.instances.borrow_mut();
        let id = insts.len();
        insts.push(instance);
        id
    };

    // Parse binary export section to map each export name to its exact `(kind, index)` for
    // W3C JS-API exported function `.name` (stringified index) and canonical re-export identity.
    let export_indices = parse_export_indices(&raw_bytes);

    let exports_obj = alloc_ordinary(vm, None);
    let export_items: Vec<(String, Extern)> = host.with_store_mut(|mut store| {
        module
            .exports()
            .filter_map(|e| {
                let name = e.name().to_string();
                instance.get_export(&mut store, &name).map(|ex| (name, ex))
            })
            .collect()
    });

    // Track local (module-defined) entity indices so multiple exports of the same internal item
    // share the exact same JS wrapper object.
    let mut local_func_wrappers: HashMap<u32, HeapHandle> = HashMap::new();
    let mut local_mem_ids: HashMap<u32, usize> = HashMap::new();
    let mut local_table_ids: HashMap<u32, usize> = HashMap::new();
    let mut local_global_ids: HashMap<u32, usize> = HashMap::new();
    let mut local_tag_ids: HashMap<u32, usize> = HashMap::new();

    for (name, ext) in export_items {
        let idx_opt = export_indices.get(&name).map(|&(_, idx)| idx);
        let js_val = match ext {
            Extern::Func(f) => {
                let wrapper = if let Some(idx) = idx_opt {
                    if let Some(&Some(imp_w)) = imported_func_wrappers.get(idx as usize) {
                        imp_w
                    } else if let Some(&loc_w) = local_func_wrappers.get(&idx) {
                        loc_w
                    } else {
                        let fn_name = idx.to_string();
                        let w = get_or_wrap_exported_func(vm, host, &fn_name, f);
                        local_func_wrappers.insert(idx, w);
                        w
                    }
                } else {
                    get_or_wrap_exported_func(vm, host, &name, f)
                };
                JsValue::from_object(wrapper)
            }
            Extern::Memory(mem) => {
                let mid = if let Some(idx) = idx_opt {
                    if let Some(&imp_id) = imported_mem_ids.get(idx as usize) {
                        imp_id
                    } else if let Some(&loc_id) = local_mem_ids.get(&idx) {
                        loc_id
                    } else {
                        let new_id = register_memory(vm, host, mem);
                        local_mem_ids.insert(idx, new_id);
                        new_id
                    }
                } else {
                    register_memory(vm, host, mem)
                };
                JsValue::from_object(get_or_wrap_memory_object(vm, host, mid))
            }
            Extern::SharedMemory(smem) => {
                let mid = if let Some(idx) = idx_opt {
                    if let Some(&imp_id) = imported_mem_ids.get(idx as usize) {
                        imp_id
                    } else if let Some(&loc_id) = local_mem_ids.get(&idx) {
                        loc_id
                    } else {
                        let ty = smem.ty();
                        let new_id =
                            register_shared_memory(vm, host, smem, ty.is_64(), ty.maximum());
                        local_mem_ids.insert(idx, new_id);
                        new_id
                    }
                } else {
                    let ty = smem.ty();
                    register_shared_memory(vm, host, smem, ty.is_64(), ty.maximum())
                };
                JsValue::from_object(get_or_wrap_memory_object(vm, host, mid))
            }
            Extern::Table(tbl) => {
                let tid = if let Some(idx) = idx_opt {
                    if let Some(&imp_id) = imported_table_ids.get(idx as usize) {
                        imp_id
                    } else if let Some(&loc_id) = local_table_ids.get(&idx) {
                        loc_id
                    } else {
                        let new_id = register_table(host, tbl);
                        local_table_ids.insert(idx, new_id);
                        new_id
                    }
                } else {
                    register_table(host, tbl)
                };
                JsValue::from_object(get_or_wrap_table_object(vm, host, tid))
            }
            Extern::Global(g) => {
                let gid = if let Some(idx) = idx_opt {
                    if let Some(&Some(imp_id)) = imported_global_ids.get(idx as usize) {
                        imp_id
                    } else if let Some(&loc_id) = local_global_ids.get(&idx) {
                        loc_id
                    } else {
                        let new_id = register_global(host, g);
                        local_global_ids.insert(idx, new_id);
                        new_id
                    }
                } else {
                    register_global(host, g)
                };
                JsValue::from_object(get_or_wrap_global_object(vm, host, gid))
            }
            Extern::Tag(t) => {
                let tid = if let Some(idx) = idx_opt {
                    if let Some(&imp_id) = imported_tag_ids.get(idx as usize) {
                        imp_id
                    } else if let Some(&loc_id) = local_tag_ids.get(&idx) {
                        loc_id
                    } else {
                        let new_id = register_tag(host, t);
                        local_tag_ids.insert(idx, new_id);
                        new_id
                    }
                } else {
                    register_tag(host, t)
                };
                JsValue::from_object(get_or_wrap_tag_object(vm, host, tid))
            }
        };
        define_prop(vm, exports_obj, &name, js_val, PropertyAttrs::READONLY);
    }

    // W3C JS-API: `instance.exports` is a frozen (non-extensible) null-prototype object.
    if let Some(o) = vm.heap.get_object_mut(exports_obj) {
        o.extensible = false;
    }

    let inst_obj = alloc_ordinary(vm, Some(proto));
    define_hidden_id(vm, inst_obj, INSTANCE_ID_KEY, inst_id);
    define_prop(
        vm,
        inst_obj,
        INSTANCE_EXPORTS_KEY,
        JsValue::from_object(exports_obj),
        PropertyAttrs::empty(),
    );
    Ok(JsValue::from_object(inst_obj))
}

fn classify_instantiation_error(
    vm: &mut JsVm,
    host: &WasmHost,
    err: wasmtime::Error,
) -> JsException {
    if let Some(trap) = err.downcast_ref::<Trap>() {
        if *trap == Trap::StackOverflow {
            return vm.range_error("Maximum call stack size exceeded");
        }
        return make_wasm_error(
            vm,
            host.runtime_error_proto,
            "RuntimeError",
            err.to_string(),
        );
    }
    let msg = err.to_string();
    if msg.contains("wasm trap")
        || msg.contains("unreachable")
        || msg.contains("out of bounds")
        || msg.contains("call stack exhausted")
    {
        if msg.contains("call stack exhausted") {
            return vm.range_error("Maximum call stack size exceeded");
        }
        return make_wasm_error(vm, host.runtime_error_proto, "RuntimeError", msg);
    }
    make_wasm_error(vm, host.link_error_proto, "LinkError", msg)
}

fn classify_trap_error(vm: &mut JsVm, host: &WasmHost, err: wasmtime::Error) -> JsException {
    if let Some(trap) = err.downcast_ref::<Trap>() {
        if *trap == Trap::StackOverflow {
            return vm.range_error("Maximum call stack size exceeded");
        }
    }
    let msg = err.to_string();
    if msg.contains("call stack exhausted") {
        return vm.range_error("Maximum call stack size exceeded");
    }
    make_wasm_error(vm, host.runtime_error_proto, "RuntimeError", msg)
}

fn wrap_thrown_wasm_exception(
    vm: &mut JsVm,
    host: &WasmHost,
    exn: OwnedRooted<ExnRef>,
) -> JsException {
    let eid = register_exception(host, exn, None);
    let obj = alloc_ordinary(vm, Some(host.exception_proto));
    define_hidden_id(vm, obj, EXN_ID_KEY, eid);
    JsException {
        value: JsValue::from_object(obj),
        message: "WebAssembly.Exception".to_string(),
    }
}

// --- Exported Wasm Function Wrapper & Imported JS Function Bridge ---

fn get_or_wrap_exported_func(
    vm: &mut JsVm,
    host: &SharedWasmHost,
    name: &str,
    func: Func,
) -> HeapHandle {
    let raw_key = host.with_store_mut(|mut store| func.to_raw(&mut store) as usize);
    if let Some(&cached) = host.func_wrappers.borrow().get(&raw_key) {
        return cached;
    }

    let wrapper = wrap_exported_func_with_proto(vm, host, name, func, vm.function_proto);
    host.func_wrappers.borrow_mut().insert(raw_key, wrapper);
    wrapper
}

fn wrap_exported_func_with_proto(
    vm: &mut JsVm,
    host: &SharedWasmHost,
    name: &str,
    func: Func,
    proto: HeapHandle,
) -> HeapHandle {
    let raw_key = host.with_store_mut(|mut store| func.to_raw(&mut store) as usize);
    let (param_len, fid) = {
        let plen = host.with_store(|store| func.ty(&store).params().len() as i32);
        let mut funcs = host.funcs.borrow_mut();
        let id = funcs.len();
        funcs.push(func);
        (plen, id)
    };

    let h = Rc::clone(host);
    let wrapper = alloc_host_fn_len(vm, name, param_len, move |vm, _this, args| {
        let (param_tys, result_tys) = h.with_store(|store| {
            let ty = func.ty(&store);
            (
                ty.params().collect::<Vec<_>>(),
                ty.results().collect::<Vec<_>>(),
            )
        });

        // W3C JS-API: calling a Wasm function with v128 param or result throws TypeError.
        if param_tys.iter().any(|t| matches!(t, ValType::V128))
            || result_tys.iter().any(|t| matches!(t, ValType::V128))
        {
            return Err(vm.type_error("Cannot pass or return v128 across JS/Wasm boundary"));
        }

        let mut wasm_args = Vec::with_capacity(param_tys.len());
        for (i, pty) in param_tys.iter().enumerate() {
            let jv = args.get(i).copied().unwrap_or(JsValue::UNDEFINED);
            wasm_args.push(js_to_wasm_val(vm, &h, jv, pty)?);
        }
        let mut wasm_results: Vec<Val> = result_tys.iter().map(default_wasm_val_raw).collect();

        // Sync JS `ArrayBuffer` edits into Wasm linear memories before calling Wasm.
        sync_all_js_to_wasm(vm, &h);

        let call_outcome = h.with_store_mut(|store| {
            let mut guard = VmPtrGuard::enter(store, vm, &h);
            let res = func.call(&mut guard.store, &wasm_args, &mut wasm_results);
            let js_exc = guard.store.data_mut().pending_js_exception.take();
            let wasm_exn = guard
                .store
                .take_pending_exception()
                .and_then(|r| r.to_owned_rooted(&mut guard.store).ok());
            (res, js_exc, wasm_exn)
        });

        // Always sync Wasm linear memory mutations (and any `memory.grow`) back to JS!
        sync_all_wasm_to_js(vm, &h);

        match call_outcome {
            (Ok(()), _, _) => match wasm_results.len() {
                0 => Ok(JsValue::UNDEFINED),
                1 => wasm_to_js_val(vm, &h, &wasm_results[0]),
                _ => {
                    let arr = vm.alloc_array();
                    for r in &wasm_results {
                        let jv = wasm_to_js_val(vm, &h, r)?;
                        vm.heap.object_push_element(arr, jv);
                    }
                    Ok(JsValue::from_object(arr))
                }
            },
            (Err(_), Some(js_exc), _) => Err(js_exc),
            (Err(_), None, Some(wasm_exn)) => Err(wrap_thrown_wasm_exception(vm, &h, wasm_exn)),
            (Err(trap), None, None) => Err(classify_trap_error(vm, &h, trap)),
        }
    });
    if proto != vm.function_proto {
        vm.heap
            .object_set_prototype(wrapper, Some(proto), &mut vm.shapes);
    }
    define_hidden_id(vm, wrapper, FUNC_ID_KEY, fid);
    root_js_value(vm, host, JsValue::from_object(wrapper));
    host.func_wrappers.borrow_mut().insert(raw_key, wrapper);
    wrapper
}

fn wrap_js_fn_as_wasm_import(host: &SharedWasmHost, js_fn: JsValue, func_ty: FuncType) -> Func {
    let param_tys: Vec<ValType> = func_ty.params().collect();
    let result_tys: Vec<ValType> = func_ty.results().collect();
    let js_fn_raw = js_fn.to_raw();
    host.with_store_mut(|mut store| {
        Func::new(
            &mut store,
            func_ty,
            move |mut caller: Caller<'_, WasmStoreData>, params, results| {
                let vm_addr = caller.data().vm_addr;
                let host_addr = caller.data().host_addr;
                if vm_addr == 0 || host_addr == 0 {
                    return Err(wasmtime::Error::msg(
                        "No active JsVm during Wasm host import call",
                    ));
                }
                let memories = Arc::clone(&caller.data().memories);
                // SAFETY: `VmPtrGuard` guarantees `vm_addr` and `host_addr` are valid for the exact
                // synchronous duration of `func.call` / `Instance::new` on this thread.
                let vm = unsafe { &mut *(vm_addr as *mut JsVm) };
                // SAFETY: `host_addr` points to the live `WasmHost` whose `store` is currently borrowed
                // via `caller`; we only read non-`store` fields or pass `caller.as_context_mut()` explicitly.
                let host_ref = unsafe { &*(host_addr as *const WasmHost) };

                if param_tys.iter().any(|t| matches!(t, ValType::V128))
                    || result_tys.iter().any(|t| matches!(t, ValType::V128))
                {
                    let exc = vm.type_error("Cannot pass or return v128 across JS/Wasm boundary");
                    caller.data_mut().pending_js_exception = Some(exc);
                    return Err(wasmtime::Error::msg("__rix_js_exception__"));
                }

                // Sync Wasm linear memory -> JS ArrayBuffer before invoking JS import!
                sync_memories_with_ctx_to_js(vm, &memories, caller.as_context());

                let mut js_args = Vec::with_capacity(params.len());
                for p in params {
                    match wasm_to_js_val_with_ctx(vm, host_ref, caller.as_context_mut(), p) {
                        Ok(v) => js_args.push(v),
                        Err(exc) => {
                            caller.data_mut().pending_js_exception = Some(exc);
                            return Err(wasmtime::Error::msg("__rix_js_exception__"));
                        }
                    }
                }

                let call_res = host_ref.enter_reentrant_ctx(caller.as_context_mut(), || {
                    vm.call_value(JsValue::from_raw(js_fn_raw), JsValue::UNDEFINED, &js_args)
                });

                // Sync any JS ArrayBuffer writes made by the import callback back to Wasm!
                sync_memories_with_ctx_from_js(vm, &memories, caller.as_context_mut());

                match call_res {
                    Ok(ret_val) => {
                        match result_tys.len() {
                            0 => {}
                            1 => {
                                let wv = host_ref
                                    .enter_reentrant_ctx(caller.as_context_mut(), || {
                                        js_to_wasm_val(vm, host_ref, ret_val, &result_tys[0])
                                    })
                                    .map_err(|exc| {
                                        caller.data_mut().pending_js_exception = Some(exc);
                                        wasmtime::Error::msg("__rix_js_exception__")
                                    })?;
                                results[0] = wv;
                            }
                            n => {
                                let items = host_ref
                                    .enter_reentrant_ctx(caller.as_context_mut(), || {
                                        vm.collect_iterable(ret_val)
                                    })
                                    .map_err(|exc| {
                                        caller.data_mut().pending_js_exception = Some(exc);
                                        wasmtime::Error::msg("__rix_js_exception__")
                                    })?;
                                if items.len() != n {
                                    let exc = vm.type_error(format!(
                                        "Multi-value Wasm import expected {n} values, got {}",
                                        items.len()
                                    ));
                                    caller.data_mut().pending_js_exception = Some(exc);
                                    return Err(wasmtime::Error::msg("__rix_js_exception__"));
                                }
                                for (i, (elem, rty)) in
                                    items.into_iter().zip(result_tys.iter()).enumerate()
                                {
                                    let wv = host_ref
                                        .enter_reentrant_ctx(caller.as_context_mut(), || {
                                            js_to_wasm_val(vm, host_ref, elem, rty)
                                        })
                                        .map_err(|exc| {
                                            caller.data_mut().pending_js_exception = Some(exc);
                                            wasmtime::Error::msg("__rix_js_exception__")
                                        })?;
                                    results[i] = wv;
                                }
                            }
                        }
                        Ok(())
                    }
                    Err(exc) => {
                        // If JS threw a `WebAssembly.Exception`, rethrow it into Wasm so `catch`/`try_table` handles it!
                        if let Some(eid) = read_hidden_id(vm, exc.value, EXN_ID_KEY) {
                            let rooted_opt = host_ref
                                .exceptions
                                .borrow()
                                .get(eid)
                                .map(|e| e.exn.to_rooted(caller.as_context_mut()));
                            if let Some(rooted) = rooted_opt {
                                return caller.as_context_mut().throw(rooted);
                            }
                        }
                        caller.data_mut().pending_js_exception = Some(exc);
                        Err(wasmtime::Error::msg("__rix_js_exception__"))
                    }
                }
            },
        )
    })
}

// --- Instance, Memory, Table, Global, Tag, Exception Prototypes ---

fn install_instance_prototype(vm: &mut JsVm, host: &SharedWasmHost) {
    let proto = host.instance_proto;
    let get_exports = alloc_host_fn_len(vm, "get exports", 0, move |vm, this, _args| {
        if read_hidden_id(vm, this, INSTANCE_ID_KEY).is_none() {
            return Err(vm.type_error("Instance.prototype.exports called on incompatible receiver"));
        }
        let h = this
            .as_object()
            .ok_or_else(|| vm.type_error("Invalid WebAssembly.Instance"))?;
        let k = vm.heap.intern_key(INSTANCE_EXPORTS_KEY);
        vm.get_property_by_key(h, k)
    });
    let k_exp = vm.heap.intern_key("exports");
    vm.heap.object_define_accessor(
        proto,
        k_exp,
        Some(get_exports),
        None,
        PropertyAttrs::CONFIGURABLE | PropertyAttrs::ENUMERABLE,
        &mut vm.shapes,
    );
}

fn register_memory(vm: &mut JsVm, host: &SharedWasmHost, memory: Memory) -> usize {
    let (bytes, is_64, max_pages) = host.with_store(|store| {
        let ty = memory.ty(&store);
        (memory.data(&store).to_vec(), ty.is_64(), ty.maximum())
    });
    let byte_len = bytes.len();
    let js_buffer = alloc_array_buffer(vm, bytes, None, true);
    root_js_value(vm, host, JsValue::from_object(js_buffer));
    let mut mems = host.memories.lock().expect("wasm memories lock");
    let id = mems.len();
    mems.push(MemoryBridge {
        kind: WasmMemKind::Unshared(memory),
        js_buffer,
        shared_backing: None,
        last_byte_len: byte_len,
        is_resizable: false,
        is_64,
        max_pages,
        js_wrapper: None,
    });
    id
}

fn register_shared_memory(
    vm: &mut JsVm,
    host: &SharedWasmHost,
    memory: SharedMemory,
    is_64: bool,
    max_pages: Option<u64>,
) -> usize {
    let byte_len = memory.data_size();
    let max_bytes = max_pages.and_then(|m| {
        usize::try_from(m)
            .ok()
            .and_then(|p| p.checked_mul(WASM_PAGE_SIZE))
    });
    let backing = Arc::new(SharedBufferBacking::new(vec![0u8; byte_len], max_bytes));
    let js_buffer = vm.alloc_shared_array_buffer_from_backing(Arc::clone(&backing));
    if let Some(o) = vm.heap.get_object_mut(js_buffer) {
        // Shared WebAssembly.Memory buffer is non-extensible (frozen) per W3C JS-API tests.
        o.extensible = false;
        let r = o.rare_mut();
        r.ab_max_byte_length = None;
        r.ab_fixed_byte_len = Some(byte_len);
        r.ab_wasm_memory = true;
    }
    root_js_value(vm, host, JsValue::from_object(js_buffer));
    let mut mems = host.memories.lock().expect("wasm memories lock");
    let id = mems.len();
    mems.push(MemoryBridge {
        kind: WasmMemKind::Shared(memory),
        js_buffer,
        shared_backing: Some(backing),
        last_byte_len: byte_len,
        is_resizable: false,
        is_64,
        max_pages,
        js_wrapper: None,
    });
    id
}

fn register_table(host: &WasmHost, table: Table) -> usize {
    let mut tables = host.tables.borrow_mut();
    let id = tables.len();
    tables.push(TableEntry {
        table,
        js_wrapper: None,
    });
    id
}

fn register_global(host: &WasmHost, global: Global) -> usize {
    let mut globals = host.globals.borrow_mut();
    let id = globals.len();
    globals.push(GlobalEntry {
        global,
        js_wrapper: None,
    });
    id
}

fn register_tag(host: &WasmHost, tag: Tag) -> usize {
    let mut tags = host.tags.borrow_mut();
    let id = tags.len();
    tags.push(TagEntry {
        tag,
        js_wrapper: None,
    });
    id
}

fn register_exception(host: &WasmHost, exn: OwnedRooted<ExnRef>, stack: Option<String>) -> usize {
    let mut exns = host.exceptions.borrow_mut();
    let id = exns.len();
    exns.push(ExnEntry { exn, stack });
    id
}

fn sync_all_js_to_wasm(vm: &JsVm, host: &SharedWasmHost) {
    host.with_store_mut(|store| {
        sync_memories_with_ctx_from_js(vm, &host.memories, store);
    });
}

fn sync_all_wasm_to_js(vm: &mut JsVm, host: &SharedWasmHost) {
    host.with_store(|store| {
        sync_memories_with_ctx_to_js(vm, &host.memories, store);
    });
}

fn sync_memories_with_ctx_from_js(
    vm: &JsVm,
    memories: &Arc<Mutex<Vec<MemoryBridge>>>,
    mut ctx: wasmtime::StoreContextMut<'_, WasmStoreData>,
) {
    let mut mems = memories.lock().expect("wasm memories lock");
    for bridge in mems.iter_mut() {
        if let WasmMemKind::Unshared(mem) = &bridge.kind {
            if let Some(ObjectKind::ArrayBuffer(js_bytes)) =
                vm.heap.get_object(bridge.js_buffer).map(|o| &o.kind)
            {
                let cur_wasm_len = mem.data(&ctx).len();
                if js_bytes.len() > cur_wasm_len && js_bytes.len() % WASM_PAGE_SIZE == 0 {
                    let delta = ((js_bytes.len() - cur_wasm_len) / WASM_PAGE_SIZE) as u64;
                    let _ = mem.grow(&mut ctx, delta);
                    bridge.last_byte_len = js_bytes.len();
                }
                let dst = mem.data_mut(&mut ctx);
                if js_bytes.len() == dst.len() {
                    dst.copy_from_slice(js_bytes);
                }
            }
        }
    }
}

fn sync_memories_with_ctx_to_js(
    vm: &mut JsVm,
    memories: &Arc<Mutex<Vec<MemoryBridge>>>,
    ctx: wasmtime::StoreContext<'_, WasmStoreData>,
) {
    let mut mems = memories.lock().expect("wasm memories lock");
    for bridge in mems.iter_mut() {
        let WasmMemKind::Unshared(mem) = &bridge.kind else {
            continue;
        };
        let src = mem.data(&ctx);
        if src.len() != bridge.last_byte_len {
            if bridge.is_resizable {
                if let Some(ObjectKind::ArrayBuffer(dst)) = vm
                    .heap
                    .get_object_mut(bridge.js_buffer)
                    .map(|o| &mut o.kind)
                {
                    dst.resize(src.len(), 0);
                    dst.copy_from_slice(src);
                }
                bridge.last_byte_len = src.len();
            } else {
                let _ = vm.detach_array_buffer(bridge.js_buffer);
                bridge.js_buffer = alloc_array_buffer(vm, src.to_vec(), None, true);
                bridge.last_byte_len = src.len();
            }
        } else if let Some(ObjectKind::ArrayBuffer(dst)) = vm
            .heap
            .get_object_mut(bridge.js_buffer)
            .map(|o| &mut o.kind)
        {
            if dst.len() == src.len() {
                dst.copy_from_slice(src);
            }
        }
    }
}

fn install_memory_prototype(vm: &mut JsVm, host: &SharedWasmHost) {
    let proto = host.memory_proto;

    // `get Memory.prototype.buffer`
    let h_buf = Rc::clone(host);
    let get_buffer = alloc_host_fn_len(vm, "get buffer", 0, move |vm, this, _args| {
        let mid = read_hidden_id(vm, this, MEMORY_ID_KEY).ok_or_else(|| {
            vm.type_error("Memory.prototype.buffer called on incompatible receiver")
        })?;
        h_buf.with_store(|store| {
            let mut mems = h_buf.memories.lock().expect("wasm memories lock");
            if let Some(bridge) = mems.get_mut(mid) {
                if let WasmMemKind::Unshared(mem) = &bridge.kind {
                    let src = mem.data(&store);
                    if src.len() != bridge.last_byte_len {
                        if bridge.is_resizable {
                            if let Some(ObjectKind::ArrayBuffer(dst)) = vm
                                .heap
                                .get_object_mut(bridge.js_buffer)
                                .map(|o| &mut o.kind)
                            {
                                dst.resize(src.len(), 0);
                                dst.copy_from_slice(src);
                            }
                        } else {
                            let _ = vm.detach_array_buffer(bridge.js_buffer);
                            bridge.js_buffer = alloc_array_buffer(vm, src.to_vec(), None, true);
                        }
                        bridge.last_byte_len = src.len();
                    }
                }
            }
        });
        let ab = h_buf
            .memories
            .lock()
            .expect("wasm memories lock")
            .get(mid)
            .map(|b| b.js_buffer)
            .ok_or_else(|| vm.type_error("Invalid WebAssembly.Memory"))?;
        Ok(JsValue::from_object(ab))
    });
    let k_buf = vm.heap.intern_key("buffer");
    vm.heap.object_define_accessor(
        proto,
        k_buf,
        Some(get_buffer),
        None,
        PropertyAttrs::CONFIGURABLE | PropertyAttrs::ENUMERABLE,
        &mut vm.shapes,
    );

    // `Memory.prototype.grow(delta)`
    let h_grow = Rc::clone(host);
    let grow_fn = alloc_host_fn_len(vm, "grow", 1, move |vm, this, args| {
        // Brand check MUST happen before touching `delta`.
        let mid = read_hidden_id(vm, this, MEMORY_ID_KEY).ok_or_else(|| {
            vm.type_error("Memory.prototype.grow called on incompatible receiver")
        })?;
        let is_64 = h_grow
            .memories
            .lock()
            .expect("wasm memories lock")
            .get(mid)
            .map(|b| b.is_64)
            .ok_or_else(|| vm.type_error("Invalid WebAssembly.Memory"))?;
        if args.is_empty() {
            return Err(vm.type_error("Memory.prototype.grow requires 1 argument"));
        }
        let delta = to_wasm_index_limit(vm, args[0], is_64, "delta")?;

        sync_all_js_to_wasm(vm, &h_grow);

        let (prev_pages, new_bytes_opt, new_byte_len) = h_grow.with_store_mut(|mut store| {
            let mems = h_grow.memories.lock().expect("wasm memories lock");
            let bridge = mems
                .get(mid)
                .ok_or_else(|| vm.type_error("Invalid WebAssembly.Memory"))?;
            match &bridge.kind {
                WasmMemKind::Unshared(mem) => {
                    let prev = mem.grow(&mut store, delta).map_err(|e| {
                        vm.range_error(format!("WebAssembly.Memory.grow failed: {e}"))
                    })?;
                    let bytes = mem.data(&store).to_vec();
                    let len = bytes.len();
                    Ok((prev, Some(bytes), len))
                }
                WasmMemKind::Shared(smem) => {
                    let prev = smem.grow(delta).map_err(|e| {
                        vm.range_error(format!("WebAssembly.Memory.grow failed: {e}"))
                    })?;
                    Ok((prev, None, smem.data_size()))
                }
            }
        })?;

        let mut mems = h_grow.memories.lock().expect("wasm memories lock");
        if let Some(bridge) = mems.get_mut(mid) {
            if let Some(new_bytes) = new_bytes_opt {
                if bridge.is_resizable {
                    if let Some(ObjectKind::ArrayBuffer(buf)) = vm
                        .heap
                        .get_object_mut(bridge.js_buffer)
                        .map(|o| &mut o.kind)
                    {
                        buf.resize(new_byte_len, 0);
                        buf.copy_from_slice(&new_bytes);
                    }
                    bridge.last_byte_len = new_byte_len;
                } else {
                    let _ = vm.detach_array_buffer(bridge.js_buffer);
                    let new_ab = alloc_array_buffer(vm, new_bytes, None, true);
                    root_js_value(vm, &h_grow, JsValue::from_object(new_ab));
                    bridge.js_buffer = new_ab;
                    bridge.last_byte_len = new_byte_len;
                }
            } else if let Some(ref backing) = bridge.shared_backing {
                let _ = backing.grow(new_byte_len);
                if !bridge.is_resizable {
                    // W3C JS-API: growing fixed-length shared Memory returns a fresh SharedArrayBuffer
                    // wrapper without detaching the previous buffer.
                    let new_sab = vm.alloc_shared_array_buffer_from_backing(Arc::clone(backing));
                    if let Some(o) = vm.heap.get_object_mut(new_sab) {
                        o.extensible = false;
                        let r = o.rare_mut();
                        r.ab_max_byte_length = None;
                        r.ab_fixed_byte_len = Some(new_byte_len);
                        r.ab_wasm_memory = true;
                    }
                    root_js_value(vm, &h_grow, JsValue::from_object(new_sab));
                    bridge.js_buffer = new_sab;
                }
                bridge.last_byte_len = new_byte_len;
            }
        }
        Ok(wasm_index_to_js(vm, prev_pages, is_64))
    });
    define_prop(
        vm,
        proto,
        "grow",
        JsValue::from_object(grow_fn),
        PropertyAttrs::DEFAULT,
    );

    // `Memory.prototype.toResizableBuffer()`
    let h_res = Rc::clone(host);
    let to_res_fn = alloc_host_fn_len(vm, "toResizableBuffer", 0, move |vm, this, _args| {
        let mid = read_hidden_id(vm, this, MEMORY_ID_KEY).ok_or_else(|| {
            vm.type_error("Memory.prototype.toResizableBuffer called on incompatible receiver")
        })?;
        sync_all_js_to_wasm(vm, &h_res);
        let mut mems = h_res.memories.lock().expect("wasm memories lock");
        let bridge = mems
            .get_mut(mid)
            .ok_or_else(|| vm.type_error("Invalid WebAssembly.Memory"))?;
        if bridge.is_resizable {
            return Ok(JsValue::from_object(bridge.js_buffer));
        }
        let max_pages = bridge.max_pages.ok_or_else(|| {
            vm.type_error("Cannot create resizable buffer for Memory without maximum")
        })?;
        let max_bytes = usize::try_from(max_pages)
            .ok()
            .and_then(|p| p.checked_mul(WASM_PAGE_SIZE))
            .unwrap_or(usize::MAX);
        match &bridge.kind {
            WasmMemKind::Unshared(mem) => {
                let bytes = h_res.with_store(|store| mem.data(&store).to_vec());
                let _ = vm.detach_array_buffer(bridge.js_buffer);
                let new_ab = alloc_array_buffer(vm, bytes, Some(max_bytes), true);
                root_js_value(vm, &h_res, JsValue::from_object(new_ab));
                bridge.js_buffer = new_ab;
                bridge.is_resizable = true;
                Ok(JsValue::from_object(new_ab))
            }
            WasmMemKind::Shared(_) => {
                if let Some(ref backing) = bridge.shared_backing {
                    let new_sab = vm.alloc_shared_array_buffer_from_backing(Arc::clone(backing));
                    if let Some(o) = vm.heap.get_object_mut(new_sab) {
                        o.extensible = false;
                        let r = o.rare_mut();
                        r.ab_max_byte_length = Some(max_bytes);
                        r.ab_fixed_byte_len = None;
                        r.ab_wasm_memory = true;
                    }
                    root_js_value(vm, &h_res, JsValue::from_object(new_sab));
                    bridge.js_buffer = new_sab;
                    bridge.is_resizable = true;
                    Ok(JsValue::from_object(new_sab))
                } else {
                    Err(vm.type_error("Invalid shared WebAssembly.Memory"))
                }
            }
        }
    });
    define_prop(
        vm,
        proto,
        "toResizableBuffer",
        JsValue::from_object(to_res_fn),
        PropertyAttrs::DEFAULT,
    );

    // `Memory.prototype.toFixedLengthBuffer()`
    let h_fix = Rc::clone(host);
    let to_fix_fn = alloc_host_fn_len(vm, "toFixedLengthBuffer", 0, move |vm, this, _args| {
        let mid = read_hidden_id(vm, this, MEMORY_ID_KEY).ok_or_else(|| {
            vm.type_error("Memory.prototype.toFixedLengthBuffer called on incompatible receiver")
        })?;
        sync_all_js_to_wasm(vm, &h_fix);
        let mut mems = h_fix.memories.lock().expect("wasm memories lock");
        let bridge = mems
            .get_mut(mid)
            .ok_or_else(|| vm.type_error("Invalid WebAssembly.Memory"))?;
        if !bridge.is_resizable {
            return Ok(JsValue::from_object(bridge.js_buffer));
        }
        match &bridge.kind {
            WasmMemKind::Unshared(mem) => {
                let bytes = h_fix.with_store(|store| mem.data(&store).to_vec());
                let _ = vm.detach_array_buffer(bridge.js_buffer);
                let new_ab = alloc_array_buffer(vm, bytes, None, true);
                root_js_value(vm, &h_fix, JsValue::from_object(new_ab));
                bridge.js_buffer = new_ab;
                bridge.is_resizable = false;
                Ok(JsValue::from_object(new_ab))
            }
            WasmMemKind::Shared(_) => {
                if let Some(ref backing) = bridge.shared_backing {
                    let cur_len = backing.byte_len();
                    let new_sab = vm.alloc_shared_array_buffer_from_backing(Arc::clone(backing));
                    if let Some(o) = vm.heap.get_object_mut(new_sab) {
                        o.extensible = false;
                        let r = o.rare_mut();
                        r.ab_max_byte_length = None;
                        r.ab_fixed_byte_len = Some(cur_len);
                        r.ab_wasm_memory = true;
                    }
                    root_js_value(vm, &h_fix, JsValue::from_object(new_sab));
                    bridge.js_buffer = new_sab;
                    bridge.is_resizable = false;
                    Ok(JsValue::from_object(new_sab))
                } else {
                    Err(vm.type_error("Invalid shared WebAssembly.Memory"))
                }
            }
        }
    });
    define_prop(
        vm,
        proto,
        "toFixedLengthBuffer",
        JsValue::from_object(to_fix_fn),
        PropertyAttrs::DEFAULT,
    );

    // `Memory.prototype.type()`
    let h_ty = Rc::clone(host);
    let type_fn = alloc_host_fn_len(vm, "type", 0, move |vm, this, _args| {
        let mid = read_hidden_id(vm, this, MEMORY_ID_KEY).ok_or_else(|| {
            vm.type_error("Memory.prototype.type called on incompatible receiver")
        })?;
        let (min_p, max_p, is_shared, is_64) = h_ty.with_store(|store| {
            let mems = h_ty.memories.lock().expect("wasm memories lock");
            let bridge = mems
                .get(mid)
                .ok_or_else(|| vm.type_error("Invalid WebAssembly.Memory"))?;
            Ok(match &bridge.kind {
                WasmMemKind::Unshared(m) => {
                    let ty = m.ty(&store);
                    (ty.minimum(), ty.maximum(), false, ty.is_64())
                }
                WasmMemKind::Shared(sm) => {
                    let ty = sm.ty();
                    (ty.minimum(), ty.maximum(), true, ty.is_64())
                }
            })
        })?;
        let obj = alloc_ordinary(vm, Some(vm.object_proto));
        let min_v = wasm_index_to_js(vm, min_p, is_64);
        define_prop(vm, obj, "minimum", min_v, PropertyAttrs::DEFAULT);
        if let Some(mp) = max_p {
            let max_v = wasm_index_to_js(vm, mp, is_64);
            define_prop(vm, obj, "maximum", max_v, PropertyAttrs::DEFAULT);
        }
        define_prop(
            vm,
            obj,
            "shared",
            JsValue::from_bool(is_shared),
            PropertyAttrs::DEFAULT,
        );
        let addr_str = if is_64 { "i64" } else { "i32" };
        let addr_v = JsValue::from_string(vm.heap.alloc_string(addr_str));
        define_prop(vm, obj, "address", addr_v, PropertyAttrs::DEFAULT);
        Ok(JsValue::from_object(obj))
    });
    define_prop(
        vm,
        proto,
        "type",
        JsValue::from_object(type_fn),
        PropertyAttrs::DEFAULT,
    );
}

fn install_table_prototype(vm: &mut JsVm, host: &SharedWasmHost) {
    let proto = host.table_proto;

    // `get Table.prototype.length`
    let h_len = Rc::clone(host);
    let get_len = alloc_host_fn_len(vm, "get length", 0, move |vm, this, _args| {
        let tid = read_hidden_id(vm, this, TABLE_ID_KEY).ok_or_else(|| {
            vm.type_error("Table.prototype.length called on incompatible receiver")
        })?;
        let (sz, is_64) = h_len.with_store(|store| {
            let tbl = h_len
                .tables
                .borrow()
                .get(tid)
                .map(|e| e.table)
                .ok_or_else(|| vm.type_error("Invalid WebAssembly.Table"))?;
            Ok((tbl.size(&store), tbl.ty(&store).is_64()))
        })?;
        Ok(wasm_index_to_js(vm, sz, is_64))
    });
    let k_len = vm.heap.intern_key("length");
    vm.heap.object_define_accessor(
        proto,
        k_len,
        Some(get_len),
        None,
        PropertyAttrs::CONFIGURABLE | PropertyAttrs::ENUMERABLE,
        &mut vm.shapes,
    );

    // `Table.prototype.get(index)`
    let h_get = Rc::clone(host);
    let get_fn = alloc_host_fn_len(vm, "get", 1, move |vm, this, args| {
        let tid = read_hidden_id(vm, this, TABLE_ID_KEY)
            .ok_or_else(|| vm.type_error("Table.prototype.get called on incompatible receiver"))?;
        let (tbl, is_64) = h_get.with_store(|store| {
            let tbl = h_get
                .tables
                .borrow()
                .get(tid)
                .map(|e| e.table)
                .ok_or_else(|| vm.type_error("Invalid WebAssembly.Table"))?;
            Ok((tbl, tbl.ty(&store).is_64()))
        })?;
        if args.is_empty() {
            return Err(vm.type_error("Table.prototype.get requires 1 argument"));
        }
        let idx = to_wasm_index_limit(vm, args[0], is_64, "index")?;
        let elem = h_get.with_store_mut(|mut store| {
            tbl.get(&mut store, idx)
                .ok_or_else(|| vm.range_error("WebAssembly.Table.get index out of bounds"))
        })?;
        wasm_ref_to_js(vm, &h_get, &elem)
    });
    define_prop(
        vm,
        proto,
        "get",
        JsValue::from_object(get_fn),
        PropertyAttrs::DEFAULT,
    );

    // `Table.prototype.set(index, value?)`
    let h_set = Rc::clone(host);
    let set_fn = alloc_host_fn_len(vm, "set", 1, move |vm, this, args| {
        let tid = read_hidden_id(vm, this, TABLE_ID_KEY)
            .ok_or_else(|| vm.type_error("Table.prototype.set called on incompatible receiver"))?;
        let (tbl, is_64, elem_ty) = h_set.with_store(|store| {
            let tbl = h_set
                .tables
                .borrow()
                .get(tid)
                .map(|e| e.table)
                .ok_or_else(|| vm.type_error("Invalid WebAssembly.Table"))?;
            let tty = tbl.ty(&store);
            Ok((tbl, tty.is_64(), tty.element().clone()))
        })?;
        if args.is_empty() {
            return Err(vm.type_error("Table.prototype.set requires at least 1 argument"));
        }
        // Convert index BEFORE converting value per WebIDL order.
        let idx = to_wasm_index_limit(vm, args[0], is_64, "index")?;
        let ref_val = if args.len() < 2 {
            default_wasm_ref(vm, &h_set, &elem_ty)
        } else {
            js_to_wasm_ref(vm, &h_set, args[1], &elem_ty)?
        };
        h_set.with_store_mut(|mut store| {
            tbl.set(&mut store, idx, ref_val)
                .map_err(|e| vm.range_error(format!("WebAssembly.Table.set failed: {e}")))
        })?;
        Ok(JsValue::UNDEFINED)
    });
    define_prop(
        vm,
        proto,
        "set",
        JsValue::from_object(set_fn),
        PropertyAttrs::DEFAULT,
    );

    // `Table.prototype.grow(delta, value?)`
    let h_grow = Rc::clone(host);
    let grow_fn = alloc_host_fn_len(vm, "grow", 1, move |vm, this, args| {
        let tid = read_hidden_id(vm, this, TABLE_ID_KEY)
            .ok_or_else(|| vm.type_error("Table.prototype.grow called on incompatible receiver"))?;
        let (tbl, is_64, elem_ty) = h_grow.with_store(|store| {
            let tbl = h_grow
                .tables
                .borrow()
                .get(tid)
                .map(|e| e.table)
                .ok_or_else(|| vm.type_error("Invalid WebAssembly.Table"))?;
            let tty = tbl.ty(&store);
            Ok((tbl, tty.is_64(), tty.element().clone()))
        })?;
        if args.is_empty() {
            return Err(vm.type_error("Table.prototype.grow requires at least 1 argument"));
        }
        let delta = to_wasm_index_limit(vm, args[0], is_64, "delta")?;
        let init_ref = if args.len() < 2 {
            default_wasm_ref(vm, &h_grow, &elem_ty)
        } else {
            js_to_wasm_ref(vm, &h_grow, args[1], &elem_ty)?
        };
        let prev = h_grow.with_store_mut(|mut store| {
            tbl.grow(&mut store, delta, init_ref)
                .map_err(|e| vm.range_error(format!("WebAssembly.Table.grow failed: {e}")))
        })?;
        Ok(wasm_index_to_js(vm, prev, is_64))
    });
    define_prop(
        vm,
        proto,
        "grow",
        JsValue::from_object(grow_fn),
        PropertyAttrs::DEFAULT,
    );

    // `Table.prototype.type()`
    let h_ty = Rc::clone(host);
    let type_fn = alloc_host_fn_len(vm, "type", 0, move |vm, this, _args| {
        let tid = read_hidden_id(vm, this, TABLE_ID_KEY)
            .ok_or_else(|| vm.type_error("Table.prototype.type called on incompatible receiver"))?;
        let (min_s, max_s, elem_name, is_64) = h_ty.with_store(|store| {
            let tbl = h_ty
                .tables
                .borrow()
                .get(tid)
                .map(|e| e.table)
                .ok_or_else(|| vm.type_error("Invalid WebAssembly.Table"))?;
            let tty = tbl.ty(&store);
            Ok((
                tty.minimum(),
                tty.maximum(),
                ref_type_js_name(tty.element()),
                tty.is_64(),
            ))
        })?;
        let obj = alloc_ordinary(vm, Some(vm.object_proto));
        let elem_v = JsValue::from_string(vm.heap.alloc_string(elem_name));
        define_prop(vm, obj, "element", elem_v, PropertyAttrs::DEFAULT);
        let min_v = wasm_index_to_js(vm, min_s, is_64);
        define_prop(vm, obj, "minimum", min_v, PropertyAttrs::DEFAULT);
        if let Some(ms) = max_s {
            let max_v = wasm_index_to_js(vm, ms, is_64);
            define_prop(vm, obj, "maximum", max_v, PropertyAttrs::DEFAULT);
        }
        Ok(JsValue::from_object(obj))
    });
    define_prop(
        vm,
        proto,
        "type",
        JsValue::from_object(type_fn),
        PropertyAttrs::DEFAULT,
    );
}

fn install_global_prototype(vm: &mut JsVm, host: &SharedWasmHost) {
    let proto = host.global_proto;

    let h_get = Rc::clone(host);
    let getter = alloc_host_fn_len(vm, "get value", 0, move |vm, this, _args| {
        let gid = read_hidden_id(vm, this, GLOBAL_ID_KEY).ok_or_else(|| {
            vm.type_error("Global.prototype.value called on incompatible receiver")
        })?;
        let (wv, is_v128) = h_get.with_store_mut(|mut store| {
            let g = h_get
                .globals
                .borrow()
                .get(gid)
                .map(|e| e.global)
                .ok_or_else(|| vm.type_error("Invalid WebAssembly.Global"))?;
            let is_v128 = matches!(g.ty(&store).content(), ValType::V128);
            Ok((g.get(&mut store), is_v128))
        })?;
        if is_v128 {
            return Err(vm.type_error("Cannot read v128 WebAssembly.Global from JS"));
        }
        wasm_to_js_val(vm, &h_get, &wv)
    });

    let h_set = Rc::clone(host);
    let setter = alloc_host_fn_len(vm, "set value", 1, move |vm, this, args| {
        let gid = read_hidden_id(vm, this, GLOBAL_ID_KEY).ok_or_else(|| {
            vm.type_error("Global.prototype.value called on incompatible receiver")
        })?;
        // Check mutability BEFORE converting argument so immutable set has no ToNumber side effects.
        let val_ty = h_set.with_store(|store| {
            let g = h_set
                .globals
                .borrow()
                .get(gid)
                .map(|e| e.global)
                .ok_or_else(|| vm.type_error("Invalid WebAssembly.Global"))?;
            let ty = g.ty(&store);
            if ty.mutability() != Mutability::Var {
                return Err(vm.type_error("Cannot modify immutable WebAssembly.Global"));
            }
            if matches!(ty.content(), ValType::V128) {
                return Err(vm.type_error("Cannot modify v128 WebAssembly.Global from JS"));
            }
            Ok(ty.content().clone())
        })?;
        let jv = args.first().copied().unwrap_or(JsValue::UNDEFINED);
        let wv = js_to_wasm_val(vm, &h_set, jv, &val_ty)?;
        h_set.with_store_mut(|mut store| {
            let g = h_set
                .globals
                .borrow()
                .get(gid)
                .map(|e| e.global)
                .ok_or_else(|| vm.type_error("Invalid WebAssembly.Global"))?;
            g.set(&mut store, wv)
                .map_err(|e| vm.type_error(format!("WebAssembly.Global.value set failed: {e}")))
        })?;
        Ok(JsValue::UNDEFINED)
    });

    let k_val = vm.heap.intern_key("value");
    vm.heap.object_define_accessor(
        proto,
        k_val,
        Some(getter),
        Some(setter),
        PropertyAttrs::CONFIGURABLE | PropertyAttrs::ENUMERABLE,
        &mut vm.shapes,
    );

    // `Global.prototype.valueOf()`
    let h_vo = Rc::clone(host);
    let value_of = alloc_host_fn_len(vm, "valueOf", 0, move |vm, this, _args| {
        let gid = read_hidden_id(vm, this, GLOBAL_ID_KEY).ok_or_else(|| {
            vm.type_error("Global.prototype.valueOf called on incompatible receiver")
        })?;
        let (wv, is_v128) = h_vo.with_store_mut(|mut store| {
            let g = h_vo
                .globals
                .borrow()
                .get(gid)
                .map(|e| e.global)
                .ok_or_else(|| vm.type_error("Invalid WebAssembly.Global"))?;
            let is_v128 = matches!(g.ty(&store).content(), ValType::V128);
            Ok((g.get(&mut store), is_v128))
        })?;
        if is_v128 {
            return Err(vm.type_error("Cannot read v128 WebAssembly.Global from JS"));
        }
        wasm_to_js_val(vm, &h_vo, &wv)
    });
    define_prop(
        vm,
        proto,
        "valueOf",
        JsValue::from_object(value_of),
        PropertyAttrs::DEFAULT,
    );

    // `Global.prototype.type()`
    let h_ty = Rc::clone(host);
    let type_fn = alloc_host_fn_len(vm, "type", 0, move |vm, this, _args| {
        let gid = read_hidden_id(vm, this, GLOBAL_ID_KEY).ok_or_else(|| {
            vm.type_error("Global.prototype.type called on incompatible receiver")
        })?;
        let (val_name, is_mut) = h_ty.with_store(|store| {
            let g = h_ty
                .globals
                .borrow()
                .get(gid)
                .map(|e| e.global)
                .ok_or_else(|| vm.type_error("Invalid WebAssembly.Global"))?;
            let ty = g.ty(&store);
            Ok((
                val_type_js_name(ty.content()),
                ty.mutability() == Mutability::Var,
            ))
        })?;
        let obj = alloc_ordinary(vm, Some(vm.object_proto));
        define_prop(
            vm,
            obj,
            "mutable",
            JsValue::from_bool(is_mut),
            PropertyAttrs::DEFAULT,
        );
        let v_str = JsValue::from_string(vm.heap.alloc_string(val_name));
        define_prop(vm, obj, "value", v_str, PropertyAttrs::DEFAULT);
        Ok(JsValue::from_object(obj))
    });
    define_prop(
        vm,
        proto,
        "type",
        JsValue::from_object(type_fn),
        PropertyAttrs::DEFAULT,
    );
}

fn install_tag_prototype(vm: &mut JsVm, host: &SharedWasmHost) {
    let proto = host.tag_proto;
    let h_ty = Rc::clone(host);
    let type_fn = alloc_host_fn_len(vm, "type", 0, move |vm, this, _args| {
        let tid = read_hidden_id(vm, this, TAG_ID_KEY)
            .ok_or_else(|| vm.type_error("Tag.prototype.type called on incompatible receiver"))?;
        let param_names: Vec<&'static str> = h_ty.with_store(|store| {
            let tag = h_ty
                .tags
                .borrow()
                .get(tid)
                .map(|e| e.tag)
                .ok_or_else(|| vm.type_error("Invalid WebAssembly.Tag"))?;
            Ok(tag
                .ty(&store)
                .ty()
                .params()
                .map(|t| val_type_js_name(&t))
                .collect())
        })?;
        let arr = vm.alloc_array();
        for name in param_names {
            let s = JsValue::from_string(vm.heap.alloc_string(name));
            vm.heap.object_push_element(arr, s);
        }
        let obj = alloc_ordinary(vm, Some(vm.object_proto));
        define_prop(
            vm,
            obj,
            "parameters",
            JsValue::from_object(arr),
            PropertyAttrs::DEFAULT,
        );
        Ok(JsValue::from_object(obj))
    });
    define_prop(
        vm,
        proto,
        "type",
        JsValue::from_object(type_fn),
        PropertyAttrs::DEFAULT,
    );
}

fn install_exception_prototype(vm: &mut JsVm, host: &SharedWasmHost) {
    let proto = host.exception_proto;

    // `Exception.prototype.is(tag)`
    let h_is = Rc::clone(host);
    let is_fn = alloc_host_fn_len(vm, "is", 1, move |vm, this, args| {
        let eid = read_hidden_id(vm, this, EXN_ID_KEY).ok_or_else(|| {
            vm.type_error("Exception.prototype.is called on incompatible receiver")
        })?;
        let tag_arg = args.first().copied().unwrap_or(JsValue::UNDEFINED);
        let tid = read_hidden_id(vm, tag_arg, TAG_ID_KEY)
            .ok_or_else(|| vm.type_error("Argument 0 must be a WebAssembly.Tag"))?;
        let tag = h_is
            .tags
            .borrow()
            .get(tid)
            .map(|e| e.tag)
            .ok_or_else(|| vm.type_error("Invalid WebAssembly.Tag"))?;
        let exns = h_is.exceptions.borrow();
        let entry = exns
            .get(eid)
            .ok_or_else(|| vm.type_error("Invalid WebAssembly.Exception"))?;
        let eq = h_is.with_store_mut(|mut store| {
            let rooted = entry.exn.to_rooted(&mut store);
            let exn_tag = rooted
                .tag(&mut store)
                .map_err(|e| vm.type_error(format!("Failed to inspect exception tag: {e}")))?;
            Ok(Tag::eq(&exn_tag, &tag, &store))
        })?;
        Ok(JsValue::from_bool(eq))
    });
    define_prop(
        vm,
        proto,
        "is",
        JsValue::from_object(is_fn),
        PropertyAttrs::DEFAULT,
    );

    // `Exception.prototype.getArg(tag, index)`
    let h_arg = Rc::clone(host);
    let get_arg_fn = alloc_host_fn_len(vm, "getArg", 2, move |vm, this, args| {
        let eid = read_hidden_id(vm, this, EXN_ID_KEY).ok_or_else(|| {
            vm.type_error("Exception.prototype.getArg called on incompatible receiver")
        })?;
        if args.len() < 2 {
            return Err(vm.type_error("Exception.prototype.getArg requires 2 arguments"));
        }
        let tid = read_hidden_id(vm, args[0], TAG_ID_KEY)
            .ok_or_else(|| vm.type_error("Argument 0 must be a WebAssembly.Tag"))?;
        let idx_num = vm.to_number(args[1])?;
        if !idx_num.is_finite() || idx_num < 0.0 || idx_num > f64::from(u32::MAX) {
            return Err(vm.range_error("WebAssembly.Exception getArg index out of range"));
        }
        let index = idx_num.trunc() as usize;
        let tag = h_arg
            .tags
            .borrow()
            .get(tid)
            .map(|e| e.tag)
            .ok_or_else(|| vm.type_error("Invalid WebAssembly.Tag"))?;
        let val = {
            let exns = h_arg.exceptions.borrow();
            let entry = exns
                .get(eid)
                .ok_or_else(|| vm.type_error("Invalid WebAssembly.Exception"))?;
            h_arg.with_store_mut(|mut store| {
                let rooted = entry.exn.to_rooted(&mut store);
                let exn_tag = rooted
                    .tag(&mut store)
                    .map_err(|e| vm.type_error(format!("Failed to inspect exception tag: {e}")))?;
                if !Tag::eq(&exn_tag, &tag, &store) {
                    return Err(vm.type_error("WebAssembly.Exception tag mismatch"));
                }
                let param_count = tag.ty(&store).ty().params().len();
                if index >= param_count {
                    return Err(vm.range_error("WebAssembly.Exception getArg index out of range"));
                }
                let field_val = rooted
                    .field(&mut store, index)
                    .map_err(|e| vm.range_error(format!("Failed to read exception field: {e}")))?;
                if matches!(field_val, Val::V128(_)) {
                    return Err(vm.type_error("Cannot read v128 exception payload from JS"));
                }
                Ok(field_val)
            })?
        };
        wasm_to_js_val(vm, &h_arg, &val)
    });
    define_prop(
        vm,
        proto,
        "getArg",
        JsValue::from_object(get_arg_fn),
        PropertyAttrs::DEFAULT,
    );

    // `get Exception.prototype.stack`
    let h_st = Rc::clone(host);
    let get_stack = alloc_host_fn_len(vm, "get stack", 0, move |vm, this, _args| {
        let eid = read_hidden_id(vm, this, EXN_ID_KEY).ok_or_else(|| {
            vm.type_error("Exception.prototype.stack called on incompatible receiver")
        })?;
        let stack = h_st
            .exceptions
            .borrow()
            .get(eid)
            .and_then(|e| e.stack.clone());
        Ok(match stack {
            Some(s) => JsValue::from_string(vm.heap.alloc_string(s)),
            None => JsValue::UNDEFINED,
        })
    });
    let k_stack = vm.heap.intern_key("stack");
    vm.heap.object_define_accessor(
        proto,
        k_stack,
        Some(get_stack),
        None,
        PropertyAttrs::CONFIGURABLE | PropertyAttrs::ENUMERABLE,
        &mut vm.shapes,
    );
}

fn wrap_memory_object_with_proto(
    vm: &mut JsVm,
    host: &SharedWasmHost,
    mem_id: usize,
    proto: HeapHandle,
) -> HeapHandle {
    let obj = alloc_ordinary(vm, Some(proto));
    define_hidden_id(vm, obj, MEMORY_ID_KEY, mem_id);
    root_js_value(vm, host, JsValue::from_object(obj));
    if let Some(b) = host
        .memories
        .lock()
        .expect("wasm memories lock")
        .get_mut(mem_id)
    {
        if b.js_wrapper.is_none() {
            b.js_wrapper = Some(obj);
        }
    }
    obj
}

fn get_or_wrap_memory_object(vm: &mut JsVm, host: &SharedWasmHost, mem_id: usize) -> HeapHandle {
    if let Some(existing) = host
        .memories
        .lock()
        .expect("wasm memories lock")
        .get(mem_id)
        .and_then(|b| b.js_wrapper)
    {
        return existing;
    }
    wrap_memory_object_with_proto(vm, host, mem_id, host.memory_proto)
}

fn wrap_table_object_with_proto(
    vm: &mut JsVm,
    host: &SharedWasmHost,
    tid: usize,
    proto: HeapHandle,
) -> HeapHandle {
    let obj = alloc_ordinary(vm, Some(proto));
    define_hidden_id(vm, obj, TABLE_ID_KEY, tid);
    root_js_value(vm, host, JsValue::from_object(obj));
    if let Some(e) = host.tables.borrow_mut().get_mut(tid) {
        if e.js_wrapper.is_none() {
            e.js_wrapper = Some(obj);
        }
    }
    obj
}

fn get_or_wrap_table_object(vm: &mut JsVm, host: &SharedWasmHost, tid: usize) -> HeapHandle {
    if let Some(existing) = host.tables.borrow().get(tid).and_then(|e| e.js_wrapper) {
        return existing;
    }
    wrap_table_object_with_proto(vm, host, tid, host.table_proto)
}

fn wrap_global_object_with_proto(
    vm: &mut JsVm,
    host: &SharedWasmHost,
    gid: usize,
    proto: HeapHandle,
) -> HeapHandle {
    let obj = alloc_ordinary(vm, Some(proto));
    define_hidden_id(vm, obj, GLOBAL_ID_KEY, gid);
    root_js_value(vm, host, JsValue::from_object(obj));
    if let Some(e) = host.globals.borrow_mut().get_mut(gid) {
        if e.js_wrapper.is_none() {
            e.js_wrapper = Some(obj);
        }
    }
    obj
}

fn get_or_wrap_global_object(vm: &mut JsVm, host: &SharedWasmHost, gid: usize) -> HeapHandle {
    if let Some(existing) = host.globals.borrow().get(gid).and_then(|e| e.js_wrapper) {
        return existing;
    }
    wrap_global_object_with_proto(vm, host, gid, host.global_proto)
}

fn wrap_tag_object_with_proto(
    vm: &mut JsVm,
    host: &SharedWasmHost,
    tid: usize,
    proto: HeapHandle,
) -> HeapHandle {
    let obj = alloc_ordinary(vm, Some(proto));
    define_hidden_id(vm, obj, TAG_ID_KEY, tid);
    root_js_value(vm, host, JsValue::from_object(obj));
    if let Some(e) = host.tags.borrow_mut().get_mut(tid) {
        if e.js_wrapper.is_none() {
            e.js_wrapper = Some(obj);
        }
    }
    obj
}

fn get_or_wrap_tag_object(vm: &mut JsVm, host: &SharedWasmHost, tid: usize) -> HeapHandle {
    if let Some(existing) = host.tags.borrow().get(tid).and_then(|e| e.js_wrapper) {
        return existing;
    }
    wrap_tag_object_with_proto(vm, host, tid, host.tag_proto)
}

// --- JS <-> Wasm Value & Reference Conversions ---

fn default_wasm_val_raw(ty: &ValType) -> Val {
    match ty {
        ValType::I32 => Val::I32(0),
        ValType::I64 => Val::I64(0),
        ValType::F32 => Val::F32(0),
        ValType::F64 => Val::F64(0),
        ValType::V128 => Val::V128(0u128.into()),
        ValType::Ref(rt) => match rt.heap_type() {
            HeapType::Func | HeapType::ConcreteFunc(_) | HeapType::NoFunc => Val::FuncRef(None),
            HeapType::Extern | HeapType::NoExtern => Val::ExternRef(None),
            HeapType::Exn | HeapType::NoExn => Val::ExnRef(None),
            _ => Val::AnyRef(None),
        },
    }
}

fn default_wasm_val_js(vm: &mut JsVm, host: &SharedWasmHost, ty: &ValType) -> Val {
    match ty {
        ValType::Ref(rt) => {
            let r = default_wasm_ref(vm, host, rt);
            ref_to_val(r)
        }
        _ => default_wasm_val_raw(ty),
    }
}

fn default_wasm_ref(_vm: &mut JsVm, host: &WasmHost, rt: &RefType) -> Ref {
    match rt.heap_type() {
        HeapType::Func | HeapType::ConcreteFunc(_) | HeapType::NoFunc => Ref::Func(None),
        HeapType::Extern | HeapType::NoExtern => {
            // W3C JS-API: omitted JS externref value defaults to boxed `undefined`.
            let er = host.with_store_mut(|mut store| {
                ExternRef::new(&mut store, JsExternVal(JsValue::UNDEFINED.to_raw())).ok()
            });
            Ref::Extern(er)
        }
        HeapType::Exn | HeapType::NoExn => Ref::Exn(None),
        _ => Ref::Any(None),
    }
}

fn js_val_to_i31(val: JsValue) -> Option<I31> {
    if let Some(i) = val.as_i32() {
        return I31::new_i32(i);
    }
    if let Some(f) = val.as_f64() {
        if f.is_finite() && f.fract() == 0.0 && (-1_073_741_824.0..=1_073_741_823.0).contains(&f) {
            return I31::new_i32(f as i32);
        }
    }
    None
}

fn ref_to_val(r: Ref) -> Val {
    match r {
        Ref::Func(f) => Val::FuncRef(f),
        Ref::Extern(e) => Val::ExternRef(e),
        Ref::Any(a) => Val::AnyRef(a),
        Ref::Exn(e) => Val::ExnRef(e),
    }
}

fn js_to_wasm_ref(
    vm: &mut JsVm,
    host: &WasmHost,
    val: JsValue,
    rt: &RefType,
) -> Result<Ref, JsException> {
    host.with_store_mut(|store| js_to_wasm_ref_with_ctx(vm, host, store, val, rt))
}

fn js_to_wasm_ref_with_ctx(
    vm: &mut JsVm,
    host: &WasmHost,
    mut ctx: wasmtime::StoreContextMut<'_, WasmStoreData>,
    val: JsValue,
    rt: &RefType,
) -> Result<Ref, JsException> {
    if val.is_null() {
        if !rt.is_nullable() {
            return Err(vm.type_error("Cannot convert null to non-nullable WebAssembly reference"));
        }
        return Ok(match rt.heap_type() {
            HeapType::Func | HeapType::ConcreteFunc(_) | HeapType::NoFunc => Ref::Func(None),
            HeapType::Extern | HeapType::NoExtern => Ref::Extern(None),
            HeapType::Exn | HeapType::NoExn => Ref::Exn(None),
            _ => Ref::Any(None),
        });
    }
    match rt.heap_type() {
        HeapType::Func | HeapType::ConcreteFunc(_) => {
            if let Some(fid) = read_hidden_id(vm, val, FUNC_ID_KEY) {
                let f = *host
                    .funcs
                    .borrow()
                    .get(fid)
                    .ok_or_else(|| vm.type_error("Invalid Wasm function reference"))?;
                Ok(Ref::Func(Some(f)))
            } else {
                Err(vm.type_error("Expected exported WebAssembly function or null for funcref"))
            }
        }
        HeapType::Extern => {
            root_js_value(vm, host, val);
            let er = ExternRef::new(&mut ctx, JsExternVal(val.to_raw()))
                .map_err(|e| vm.type_error(format!("Failed to allocate externref: {e}")))?;
            Ok(Ref::Extern(Some(er)))
        }
        HeapType::Exn => {
            if let Some(eid) = read_hidden_id(vm, val, EXN_ID_KEY) {
                let exns = host.exceptions.borrow();
                let entry = exns
                    .get(eid)
                    .ok_or_else(|| vm.type_error("Invalid WebAssembly.Exception"))?;
                Ok(Ref::Exn(Some(entry.exn.to_rooted(&mut ctx))))
            } else {
                Err(vm.type_error("Expected WebAssembly.Exception or null for exnref"))
            }
        }
        _ => {
            if let Some(aid) = read_hidden_id(vm, val, ANYREF_ID_KEY) {
                let anyrefs = host.anyrefs.borrow();
                let owned = anyrefs
                    .get(aid)
                    .ok_or_else(|| vm.type_error("Invalid WebAssembly GC reference"))?;
                Ok(Ref::Any(Some(owned.to_rooted(&mut ctx))))
            } else if let Some(i31) = js_val_to_i31(val) {
                Ok(Ref::Any(Some(AnyRef::from_i31(&mut ctx, i31))))
            } else {
                root_js_value(vm, host, val);
                let er = ExternRef::new(&mut ctx, JsExternVal(val.to_raw()))
                    .map_err(|e| vm.type_error(format!("Failed to box JS value as anyref: {e}")))?;
                let ar = AnyRef::convert_extern(&mut ctx, er).map_err(|e| {
                    vm.type_error(format!("Failed to convert externref to anyref: {e}"))
                })?;
                Ok(Ref::Any(Some(ar)))
            }
        }
    }
}

fn wrap_wasm_gc_anyref(
    vm: &mut JsVm,
    host: &WasmHost,
    mut ctx: wasmtime::StoreContextMut<'_, WasmStoreData>,
    ar: wasmtime::Rooted<AnyRef>,
) -> Result<JsValue, JsException> {
    if let Ok(Some(i31)) = ar.as_i31(&ctx) {
        return Ok(JsValue::from_i32(i31.get_i32()));
    }
    if !ar.is_eqref(&ctx).unwrap_or(false) {
        if let Ok(er) = ExternRef::convert_any(&mut ctx, ar) {
            if let Some(raw) = er
                .data(&ctx)
                .ok()
                .flatten()
                .and_then(|d| d.downcast_ref::<JsExternVal>())
                .map(|v| v.0)
            {
                return Ok(JsValue::from_raw(raw));
            }
        }
    }
    let owned = ar
        .to_owned_rooted(&mut ctx)
        .map_err(|e| vm.type_error(format!("Failed to root anyref: {e}")))?;
    let aid = {
        let mut list = host.anyrefs.borrow_mut();
        let id = list.len();
        list.push(owned);
        id
    };
    let obj = alloc_ordinary(vm, None);
    define_hidden_id(vm, obj, ANYREF_ID_KEY, aid);
    if let Some(o) = vm.heap.get_object_mut(obj) {
        o.extensible = false;
        o.rare_mut().wasm_gc_object = true;
    }
    Ok(JsValue::from_object(obj))
}

fn wasm_ref_to_js(vm: &mut JsVm, host: &SharedWasmHost, r: &Ref) -> Result<JsValue, JsException> {
    match r {
        Ref::Func(None) | Ref::Extern(None) | Ref::Any(None) | Ref::Exn(None) => Ok(JsValue::NULL),
        Ref::Func(Some(f)) => Ok(JsValue::from_object(get_or_wrap_exported_func(
            vm, host, "", *f,
        ))),
        Ref::Extern(Some(er)) => host.with_store_mut(|mut store| {
            if let Some(raw) = er
                .data(&store)
                .ok()
                .flatten()
                .and_then(|d| d.downcast_ref::<JsExternVal>())
                .map(|v| v.0)
            {
                return Ok(JsValue::from_raw(raw));
            }
            // Externalized Wasm GC object (`extern.convert_any`): unwrap through `AnyRef` so i31/struct/array export cleanly!
            if let Ok(ar) = AnyRef::convert_extern(&mut store, *er) {
                return wrap_wasm_gc_anyref(vm, host, store, ar);
            }
            Ok(JsValue::UNDEFINED)
        }),
        Ref::Any(Some(ar)) => {
            host.with_store_mut(|store| wrap_wasm_gc_anyref(vm, host, store, *ar))
        }
        Ref::Exn(Some(er)) => {
            let owned = host.with_store_mut(|mut store| {
                er.to_owned_rooted(&mut store)
                    .map_err(|e| vm.type_error(format!("Failed to root exnref: {e}")))
            })?;
            let eid = register_exception(host, owned, None);
            let obj = alloc_ordinary(vm, Some(host.exception_proto));
            define_hidden_id(vm, obj, EXN_ID_KEY, eid);
            Ok(JsValue::from_object(obj))
        }
    }
}

fn js_to_wasm_val(
    vm: &mut JsVm,
    host: &WasmHost,
    val: JsValue,
    ty: &ValType,
) -> Result<Val, JsException> {
    match ty {
        ValType::I32 => {
            let n = vm.to_number(val)?;
            let i = if !n.is_finite() || n == 0.0 {
                0i32
            } else {
                // ECMA-262 ToInt32
                let int = n.trunc().rem_euclid(4_294_967_296.0);
                (int as u32) as i32
            };
            Ok(Val::I32(i))
        }
        ValType::I64 => {
            let i = to_bigint_i64(vm, val)?;
            Ok(Val::I64(i))
        }
        ValType::F32 => {
            let n = vm.to_number(val)? as f32;
            Ok(Val::F32(n.to_bits()))
        }
        ValType::F64 => {
            let n = vm.to_number(val)?;
            Ok(Val::F64(n.to_bits()))
        }
        ValType::V128 => Err(vm.type_error("Cannot convert JS value to WebAssembly v128")),
        ValType::Ref(rt) => Ok(ref_to_val(js_to_wasm_ref(vm, host, val, rt)?)),
    }
}

/// Strict JS-API conversion for plain-value immutable global imports (throws `LinkError` on wrong type).
fn js_to_wasm_import_global(
    vm: &mut JsVm,
    host: &WasmHost,
    val: JsValue,
    ty: &ValType,
) -> Result<Val, JsException> {
    match ty {
        ValType::I32 | ValType::F32 | ValType::F64 => {
            if !val.is_number() {
                return Err(make_wasm_error(
                    vm,
                    host.link_error_proto,
                    "LinkError",
                    format!("Immutable global import of type {ty} must be a Number"),
                ));
            }
            js_to_wasm_val(vm, host, val, ty)
        }
        ValType::I64 => {
            if !val.is_bigint() {
                return Err(make_wasm_error(
                    vm,
                    host.link_error_proto,
                    "LinkError",
                    "Immutable global import of type i64 must be a BigInt",
                ));
            }
            js_to_wasm_val(vm, host, val, ty)
        }
        ValType::V128 => Err(make_wasm_error(
            vm,
            host.link_error_proto,
            "LinkError",
            "Cannot import non-Global value for v128 global",
        )),
        ValType::Ref(rt) => js_to_wasm_ref(vm, host, val, rt)
            .map(ref_to_val)
            .map_err(|e| make_wasm_error(vm, host.link_error_proto, "LinkError", e.message)),
    }
}

fn wasm_to_js_val(vm: &mut JsVm, host: &SharedWasmHost, val: &Val) -> Result<JsValue, JsException> {
    match val {
        Val::I32(i) => Ok(JsValue::from_i32(*i)),
        Val::I64(i) => Ok(JsValue::from_bigint(
            vm.heap.alloc_bigint(JsBigInt::from_i64(*i)),
        )),
        Val::F32(bits) => Ok(JsValue::from_number(f64::from(f32::from_bits(*bits)))),
        Val::F64(bits) => Ok(JsValue::from_number(f64::from_bits(*bits))),
        Val::V128(_) => Err(vm.type_error("Cannot convert WebAssembly v128 to JS value")),
        Val::FuncRef(f) => wasm_ref_to_js(vm, host, &Ref::Func(*f)),
        Val::ExternRef(e) => wasm_ref_to_js(vm, host, &Ref::Extern(*e)),
        Val::AnyRef(a) => wasm_ref_to_js(vm, host, &Ref::Any(*a)),
        Val::ExnRef(e) => wasm_ref_to_js(vm, host, &Ref::Exn(*e)),
        Val::ContRef(_) => Ok(JsValue::UNDEFINED),
    }
}

fn wasm_to_js_val_with_ctx(
    vm: &mut JsVm,
    host: &WasmHost,
    mut ctx: wasmtime::StoreContextMut<'_, WasmStoreData>,
    val: &Val,
) -> Result<JsValue, JsException> {
    match val {
        Val::I32(i) => Ok(JsValue::from_i32(*i)),
        Val::I64(i) => Ok(JsValue::from_bigint(
            vm.heap.alloc_bigint(JsBigInt::from_i64(*i)),
        )),
        Val::F32(bits) => Ok(JsValue::from_number(f64::from(f32::from_bits(*bits)))),
        Val::F64(bits) => Ok(JsValue::from_number(f64::from_bits(*bits))),
        Val::V128(_) => Err(vm.type_error("Cannot convert WebAssembly v128 to JS value")),
        Val::FuncRef(None) | Val::ExternRef(None) | Val::AnyRef(None) | Val::ExnRef(None) => {
            Ok(JsValue::NULL)
        }
        Val::FuncRef(Some(f)) => {
            let raw_key = f.to_raw(&mut ctx) as usize;
            if let Some(&cached) = host.func_wrappers.borrow().get(&raw_key) {
                Ok(JsValue::from_object(cached))
            } else {
                // Return a non-null placeholder function handle if not yet exported.
                Ok(JsValue::NULL)
            }
        }
        Val::ExternRef(Some(er)) => {
            if let Some(raw) = er
                .data(&ctx)
                .ok()
                .flatten()
                .and_then(|d| d.downcast_ref::<JsExternVal>())
                .map(|v| v.0)
            {
                return Ok(JsValue::from_raw(raw));
            }
            if let Ok(ar) = AnyRef::convert_extern(&mut ctx, *er) {
                return wrap_wasm_gc_anyref(vm, host, ctx, ar);
            }
            Ok(JsValue::UNDEFINED)
        }
        Val::AnyRef(Some(ar)) => wrap_wasm_gc_anyref(vm, host, ctx, *ar),
        Val::ExnRef(Some(er)) => {
            let owned = er
                .to_owned_rooted(&mut ctx)
                .map_err(|e| vm.type_error(format!("Failed to root exnref: {e}")))?;
            let eid = register_exception(host, owned, None);
            let obj = alloc_ordinary(vm, Some(host.exception_proto));
            define_hidden_id(vm, obj, EXN_ID_KEY, eid);
            Ok(JsValue::from_object(obj))
        }
        Val::ContRef(_) => Ok(JsValue::UNDEFINED),
    }
}

// --- WebIDL Type & Index Helpers ---

fn parse_address_type(vm: &mut JsVm, val: JsValue) -> Result<bool, JsException> {
    if val.is_undefined() {
        return Ok(false);
    }
    let s = vm.to_js_string(val)?;
    match s.as_str() {
        "i32" => Ok(false),
        "i64" => Ok(true),
        _ => Err(vm.type_error(format!("Invalid WebAssembly address type '{s}'"))),
    }
}

fn parse_table_ref_type(vm: &mut JsVm, s: &str) -> Result<RefType, JsException> {
    match s {
        "anyfunc" | "funcref" => Ok(RefType::FUNCREF),
        "externref" => Ok(RefType::EXTERNREF),
        "anyref" => Ok(RefType::ANYREF),
        "eqref" => Ok(RefType::EQREF),
        "exnref" => Ok(RefType::EXNREF),
        _ => Err(vm.type_error(format!("Unsupported WebAssembly.Table element type '{s}'"))),
    }
}

fn parse_js_val_type(vm: &mut JsVm, s: &str, allow_v128: bool) -> Result<ValType, JsException> {
    match s {
        "i32" => Ok(ValType::I32),
        "i64" => Ok(ValType::I64),
        "f32" => Ok(ValType::F32),
        "f64" => Ok(ValType::F64),
        "v128" if allow_v128 => Ok(ValType::V128),
        "anyfunc" | "funcref" => Ok(ValType::FUNCREF),
        "externref" => Ok(ValType::EXTERNREF),
        "anyref" => Ok(ValType::ANYREF),
        "eqref" => Ok(ValType::EQREF),
        "exnref" => Ok(ValType::EXNREF),
        _ => Err(vm.type_error(format!("Unsupported WebAssembly value type '{s}'"))),
    }
}

fn ref_type_js_name(rt: &RefType) -> &'static str {
    match rt.heap_type() {
        HeapType::Func | HeapType::ConcreteFunc(_) | HeapType::NoFunc => "funcref",
        HeapType::Extern | HeapType::NoExtern => "externref",
        HeapType::Exn | HeapType::NoExn => "exnref",
        HeapType::Eq => "eqref",
        _ => "anyref",
    }
}

fn val_type_js_name(vt: &ValType) -> &'static str {
    match vt {
        ValType::I32 => "i32",
        ValType::I64 => "i64",
        ValType::F32 => "f32",
        ValType::F64 => "f64",
        ValType::V128 => "v128",
        ValType::Ref(rt) => ref_type_js_name(rt),
    }
}

fn to_wasm_u32_limit(vm: &mut JsVm, val: JsValue, field: &str) -> Result<u32, JsException> {
    // WebIDL [EnforceRange] unsigned long
    let n = vm.to_number(val)?;
    if !n.is_finite() || n < 0.0 || n > f64::from(u32::MAX) {
        return Err(vm.type_error(format!(
            "WebAssembly '{field}' must be a non-negative 32-bit integer"
        )));
    }
    Ok(n.trunc() as u32)
}

fn to_wasm_u64_limit(vm: &mut JsVm, val: JsValue, field: &str) -> Result<u64, JsException> {
    // WebIDL [EnforceRange] unsigned long long for i64 AddressType: requires BigInt!
    let Some(bh) = val.as_bigint() else {
        return Err(vm.type_error(format!("WebAssembly 64-bit '{field}' must be a BigInt")));
    };
    let bi = vm
        .heap
        .get_bigint(bh)
        .cloned()
        .unwrap_or_else(|| JsBigInt::from_i64(0));
    let s = bi.to_string();
    if s.starts_with('-') {
        return Err(vm.type_error(format!(
            "WebAssembly 64-bit '{field}' must be a non-negative BigInt"
        )));
    }
    s.parse::<u64>().map_err(|_| {
        vm.type_error(format!(
            "WebAssembly 64-bit '{field}' exceeds 64-bit unsigned range"
        ))
    })
}

fn to_wasm_index_limit(
    vm: &mut JsVm,
    val: JsValue,
    is_64: bool,
    field: &str,
) -> Result<u64, JsException> {
    if is_64 {
        to_wasm_u64_limit(vm, val, field)
    } else {
        Ok(u64::from(to_wasm_u32_limit(vm, val, field)?))
    }
}

fn wasm_index_to_js(vm: &mut JsVm, val: u64, is_64: bool) -> JsValue {
    if is_64 {
        let bi = JsBigInt::parse_str(&val.to_string()).unwrap_or_else(|| JsBigInt::from_i64(0));
        JsValue::from_bigint(vm.heap.alloc_bigint(bi))
    } else {
        JsValue::from_number(val as f64)
    }
}

fn extract_buffer_source(vm: &mut JsVm, val: JsValue) -> Result<Vec<u8>, JsException> {
    let Some(h) = val.as_object() else {
        return Err(vm.type_error("Expected BufferSource (ArrayBuffer, TypedArray, or DataView)"));
    };
    match vm.heap.get_object(h).map(|o| &o.kind) {
        Some(ObjectKind::ArrayBuffer(b)) => {
            if ab_is_shared(vm, h) {
                return Err(vm.type_error("WebAssembly BufferSource cannot be a SharedArrayBuffer"));
            }
            Ok((**b).clone())
        }
        Some(ObjectKind::TypedArray(ta)) => {
            let buf_h = ta.buffer;
            if ab_is_shared(vm, buf_h) {
                return Err(
                    vm.type_error("WebAssembly BufferSource cannot view a SharedArrayBuffer")
                );
            }
            let off = ta.byte_offset as usize;
            let len = (ta.length as usize).saturating_mul(ta.kind.bytes_per_element());
            if let Some(ObjectKind::ArrayBuffer(b)) = vm.heap.get_object(buf_h).map(|o| &o.kind) {
                let end = off.saturating_add(len).min(b.len());
                Ok(b[off.min(end)..end].to_vec())
            } else {
                Ok(Vec::new())
            }
        }
        Some(ObjectKind::DataView(dv)) => {
            let buf_h = dv.buffer;
            if ab_is_shared(vm, buf_h) {
                return Err(
                    vm.type_error("WebAssembly BufferSource cannot view a SharedArrayBuffer")
                );
            }
            let off = dv.byte_offset as usize;
            let len = dv.byte_length as usize;
            if let Some(ObjectKind::ArrayBuffer(b)) = vm.heap.get_object(buf_h).map(|o| &o.kind) {
                let end = off.saturating_add(len).min(b.len());
                Ok(b[off.min(end)..end].to_vec())
            } else {
                Ok(Vec::new())
            }
        }
        _ => Err(vm.type_error("Expected BufferSource (ArrayBuffer, TypedArray, or DataView)")),
    }
}

// --- Object Allocation, Prototype, Descriptor & Binary Section Helpers ---

fn root_js_value(vm: &mut JsVm, host: &WasmHost, val: JsValue) {
    if val.is_object() || val.is_string() || val.is_bigint() {
        vm.heap.object_push_element(host.roots_arr, val);
    }
}

fn alloc_host_fn_len(
    vm: &mut JsVm,
    name: &str,
    length: i32,
    f: impl Fn(&mut JsVm, JsValue, &[JsValue]) -> Result<JsValue, JsException> + 'static,
) -> HeapHandle {
    let h = vm.alloc_host_fn(name, f);
    let k_len = vm.key_length;
    vm.heap.object_define_data(
        h,
        k_len,
        JsValue::from_i32(length),
        PropertyAttrs::CONFIGURABLE,
        &mut vm.shapes,
    );
    h
}

fn require_construct_proto(
    vm: &mut JsVm,
    default_proto: HeapHandle,
    name: &str,
) -> Result<HeapHandle, JsException> {
    let nt = vm.native_new_target;
    if !nt.is_object() {
        return Err(vm.type_error(format!(
            "WebAssembly.{name} constructor cannot be invoked without 'new'"
        )));
    }
    let proto = nt
        .as_object()
        .and_then(|nt_h| {
            let k_proto = vm.heap.intern_key("prototype");
            vm.get_property_by_key(nt_h, k_proto).ok()
        })
        .and_then(|v| v.as_object())
        .unwrap_or(default_proto);
    Ok(proto)
}

fn set_to_string_tag(vm: &mut JsVm, obj: HeapHandle, tag: &str) {
    let k_tag = vm.key_symbol_to_string_tag;
    let v = JsValue::from_string(vm.heap.alloc_string(tag));
    vm.heap
        .object_define_data(obj, k_tag, v, PropertyAttrs::CONFIGURABLE, &mut vm.shapes);
}

fn alloc_ordinary(vm: &mut JsVm, proto: Option<HeapHandle>) -> HeapHandle {
    vm.heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, proto))
}

fn alloc_array_buffer(
    vm: &mut JsVm,
    bytes: Vec<u8>,
    max_byte_length: Option<usize>,
    is_wasm_memory: bool,
) -> HeapHandle {
    let h = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::ArrayBuffer(Box::new(bytes)),
        Some(vm.array_buffer_proto),
    ));
    if max_byte_length.is_some() || is_wasm_memory {
        if let Some(o) = vm.heap.get_object_mut(h) {
            let r = o.rare_mut();
            r.ab_max_byte_length = max_byte_length;
            r.ab_wasm_memory = is_wasm_memory;
        }
    }
    h
}

fn alloc_wasm_error_proto(vm: &mut JsVm, err_proto: HeapHandle, name: &str) -> HeapHandle {
    let proto = alloc_ordinary(vm, Some(err_proto));
    let n = JsValue::from_string(vm.heap.alloc_string(name));
    let empty = JsValue::from_string(vm.heap.alloc_string(""));
    define_prop(vm, proto, "name", n, PropertyAttrs::METHOD);
    define_prop(vm, proto, "message", empty, PropertyAttrs::METHOD);
    proto
}

fn make_wasm_error_ctor(vm: &mut JsVm, name: &'static str, proto: HeapHandle) -> HeapHandle {
    let ctor = alloc_host_fn_len(vm, name, 1, move |vm, _this, args| {
        let target_proto = vm
            .native_new_target
            .as_object()
            .and_then(|nt| {
                let k_p = vm.heap.intern_key("prototype");
                vm.get_property_by_key(nt, k_p).ok()
            })
            .and_then(|v| v.as_object())
            .unwrap_or(proto);
        let msg = args
            .first()
            .copied()
            .filter(|v| !v.is_undefined())
            .map(|v| vm.to_js_string(v))
            .transpose()?
            .unwrap_or_default();
        let obj = alloc_ordinary(vm, Some(target_proto));
        if let Some(o) = vm.heap.get_object_mut(obj) {
            o.rare_mut().is_error_data = true;
        }
        let n = JsValue::from_string(vm.heap.alloc_string(name));
        let m = JsValue::from_string(vm.heap.alloc_string(msg));
        define_prop(vm, obj, "name", n, PropertyAttrs::METHOD);
        define_prop(vm, obj, "message", m, PropertyAttrs::METHOD);
        Ok(JsValue::from_object(obj))
    });
    vm.mark_constructor(ctor);
    define_prop(
        vm,
        ctor,
        "prototype",
        JsValue::from_object(proto),
        PropertyAttrs::empty(),
    );
    define_prop(
        vm,
        proto,
        "constructor",
        JsValue::from_object(ctor),
        PropertyAttrs::METHOD,
    );
    ctor
}

fn make_wasm_error(
    vm: &mut JsVm,
    proto: HeapHandle,
    name: &str,
    msg: impl Into<String>,
) -> JsException {
    let msg = msg.into();
    let obj = alloc_ordinary(vm, Some(proto));
    if let Some(o) = vm.heap.get_object_mut(obj) {
        o.rare_mut().is_error_data = true;
    }
    let n = JsValue::from_string(vm.heap.alloc_string(name));
    let m = JsValue::from_string(vm.heap.alloc_string(msg.clone()));
    define_prop(vm, obj, "name", n, PropertyAttrs::METHOD);
    define_prop(vm, obj, "message", m, PropertyAttrs::METHOD);
    JsException {
        value: JsValue::from_object(obj),
        message: format!("{name}: {msg}"),
    }
}

fn extern_kind_name(ty: &ExternType) -> &'static str {
    match ty {
        ExternType::Func(_) => "function",
        ExternType::Table(_) => "table",
        ExternType::Memory(_) => "memory",
        ExternType::Global(_) => "global",
        ExternType::Tag(_) => "tag",
    }
}

/// Parses WebAssembly binary export section (section ID `7`) mapping `export_name -> (kind, index)`.
fn parse_export_indices(bytes: &[u8]) -> HashMap<String, (u8, u32)> {
    let mut out = HashMap::new();
    if bytes.len() < 8 || &bytes[0..4] != b"\0asm" {
        return out;
    }
    let mut pos = 8usize;
    while pos < bytes.len() {
        let sec_id = bytes[pos];
        pos += 1;
        let Some((sec_len, consumed)) = read_leb128_u32(&bytes[pos..]) else {
            break;
        };
        pos += consumed;
        let sec_end = pos.saturating_add(sec_len as usize);
        if sec_end > bytes.len() {
            break;
        }
        if sec_id == 7 {
            let mut p = pos;
            if let Some((count, c)) = read_leb128_u32(&bytes[p..sec_end]) {
                p += c;
                for _ in 0..count {
                    let Some((name_len, nc)) = read_leb128_u32(&bytes[p..sec_end]) else {
                        break;
                    };
                    p += nc;
                    let name_end = p.saturating_add(name_len as usize);
                    if name_end >= sec_end {
                        break;
                    }
                    let name = std::str::from_utf8(&bytes[p..name_end])
                        .unwrap_or("")
                        .to_string();
                    p = name_end;
                    let kind = bytes[p];
                    p += 1;
                    let Some((idx, ic)) = read_leb128_u32(&bytes[p..sec_end]) else {
                        break;
                    };
                    p += ic;
                    out.insert(name, (kind, idx));
                }
            }
            break;
        }
        pos = sec_end;
    }
    out
}

/// Parses WebAssembly binary custom sections (section ID `0`) matching `target_name`.
fn parse_custom_sections(bytes: &[u8], target_name: &str) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    if bytes.len() < 8 || &bytes[0..4] != b"\0asm" {
        return out;
    }
    let mut pos = 8usize;
    while pos < bytes.len() {
        let sec_id = bytes[pos];
        pos += 1;
        let Some((sec_len, consumed)) = read_leb128_u32(&bytes[pos..]) else {
            break;
        };
        pos += consumed;
        let sec_end = pos.saturating_add(sec_len as usize);
        if sec_end > bytes.len() {
            break;
        }
        if sec_id == 0 {
            let payload = &bytes[pos..sec_end];
            if let Some((name_len, n_consumed)) = read_leb128_u32(payload) {
                let name_end = n_consumed.saturating_add(name_len as usize);
                if name_end <= payload.len() {
                    if let Ok(name) = std::str::from_utf8(&payload[n_consumed..name_end]) {
                        if name == target_name {
                            out.push(payload[name_end..].to_vec());
                        }
                    }
                }
            }
        }
        pos = sec_end;
    }
    out
}

fn read_leb128_u32(data: &[u8]) -> Option<(u32, usize)> {
    let mut result = 0u32;
    let mut shift = 0u32;
    for (i, &byte) in data.iter().take(5).enumerate() {
        result |= u32::from(byte & 0x7F) << shift;
        if (byte & 0x80) == 0 {
            return Some((result, i + 1));
        }
        shift += 7;
    }
    None
}

fn define_prop(vm: &mut JsVm, obj: HeapHandle, name: &str, val: JsValue, attrs: PropertyAttrs) {
    let k = vm.heap.intern_key(name);
    vm.heap
        .object_define_data(obj, k, val, attrs, &mut vm.shapes);
}

fn define_hidden_id(vm: &mut JsVm, obj: HeapHandle, key: &str, id: usize) {
    let k = vm.heap.intern_key(key);
    vm.heap.object_define_data(
        obj,
        k,
        JsValue::from_i32(id as i32),
        PropertyAttrs::empty(),
        &mut vm.shapes,
    );
}

fn read_hidden_id(vm: &mut JsVm, val: JsValue, key: &str) -> Option<usize> {
    let h = val.as_object()?;
    let k = vm.heap.intern_key(key);
    let v = vm.get_property_by_key(h, k).ok()?;
    v.as_i32().and_then(|i| usize::try_from(i).ok())
}

fn is_js_truthy(vm: &JsVm, v: JsValue) -> bool {
    if v.is_undefined() || v.is_null() {
        return false;
    }
    if let Some(b) = v.as_bool() {
        return b;
    }
    if let Some(i) = v.as_i32() {
        return i != 0;
    }
    if let Some(f) = v.as_f64() {
        return f != 0.0 && !f.is_nan();
    }
    if let Some(s) = v.as_string() {
        return vm.heap.get_string(s).is_some_and(|st| !st.is_empty());
    }
    true
}
