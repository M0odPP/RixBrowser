//! WebAssembly Runtime & Standard W3C `WebAssembly` JS-API Integration (powered by `wasmtime`).
//!
//! Reference: Spec 05 §5 & Issue #21.
//!
//! Exposes the full standard `WebAssembly` namespace on any [`rix_js::JsVm`] realm:
//! - `WebAssembly.validate(bytes)`
//! - `WebAssembly.compile(bytes)` / `WebAssembly.compileStreaming(source)`
//! - `WebAssembly.instantiate(bytesOrModule, importObject?)`
//! - `WebAssembly.instantiateStreaming(source, importObject?)`
//! - `WebAssembly.Module` (`.exports()`, `.imports()`, `.customSections()`)
//! - `WebAssembly.Instance` (`.exports`)
//! - `WebAssembly.Memory` (`.buffer` bridged seamlessly to JS `ArrayBuffer`/`TypedArray`, `.grow()`)
//! - `WebAssembly.Table` (`.length`, `.get()`, `.set()`, `.grow()`)
//! - `WebAssembly.Global` (`.value` getter/setter respecting mutability, `i32`/`i64` BigInt/`f32`/`f64`)
//! - `WebAssembly.CompileError`, `WebAssembly.LinkError`, `WebAssembly.RuntimeError`

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use rix_js::promise::{
    alloc_promise, promise_resolve_value, promise_then, reject_promise, resolve_promise,
};
use rix_js::{
    HeapHandle, JsBigInt, JsException, JsObject, JsValue, JsVm, ObjectKind, PropertyAttrs,
};
use wasmtime::{
    AsContext, AsContextMut, Caller, Config, Engine, Extern, ExternType, Func, Global, GlobalType,
    Instance, Memory, MemoryType, Module, Mutability, Ref, RefType, Store, Table, TableType, Val,
    ValType,
};

/// Internal hidden slot keys stored on JS wrapper objects.
const MODULE_ID_KEY: &str = "__rix_wasm_module_id";
const INSTANCE_ID_KEY: &str = "__rix_wasm_instance_id";
const MEMORY_ID_KEY: &str = "__rix_wasm_memory_id";
const TABLE_ID_KEY: &str = "__rix_wasm_table_id";
const GLOBAL_ID_KEY: &str = "__rix_wasm_global_id";
const FUNC_ID_KEY: &str = "__rix_wasm_func_id";

/// Maximum WebAssembly 32-bit linear memory pages (64 KiB each = 4 GiB spec ceiling).
const MAX_WASM32_PAGES: u64 = 65_536;

/// Shared memory synchronization record bridging `wasmtime::Memory` <-> JS `ArrayBuffer`.
struct MemoryBridge {
    memory: Memory,
    js_buffer: HeapHandle,
    last_byte_len: usize,
}

/// Store user data carrying temporary active `JsVm` pointer during synchronous Wasm -> JS
/// imported function calls, stashing any JS exception thrown inside an imported callback,
/// and sharing memory bridge metadata across `Send + Sync` Wasm host closures.
struct WasmStoreData {
    vm_addr: usize,
    pending_js_exception: Option<JsException>,
    memories: Arc<Mutex<Vec<MemoryBridge>>>,
}

/// Shared WebAssembly runtime state owned by a single [`JsVm`] realm.
pub struct WasmHost {
    engine: Engine,
    store: RefCell<Store<WasmStoreData>>,
    modules: RefCell<Vec<(Module, Vec<u8>)>>,
    instances: RefCell<Vec<Instance>>,
    memories: Arc<Mutex<Vec<MemoryBridge>>>,
    tables: RefCell<Vec<Table>>,
    globals: RefCell<Vec<Global>>,
    funcs: RefCell<Vec<Func>>,
    /// Prototypes & error constructors installed in this realm.
    module_proto: HeapHandle,
    instance_proto: HeapHandle,
    memory_proto: HeapHandle,
    table_proto: HeapHandle,
    global_proto: HeapHandle,
    compile_error_proto: HeapHandle,
    link_error_proto: HeapHandle,
    runtime_error_proto: HeapHandle,
}

/// Handle to a realm's installed WebAssembly subsystem.
pub type SharedWasmHost = Rc<WasmHost>;

/// RAII guard restoring `WasmStoreData::vm_addr` even on panic/early return.
struct VmPtrGuard<'a> {
    store: &'a mut Store<WasmStoreData>,
    prev: usize,
}

impl<'a> VmPtrGuard<'a> {
    fn enter(store: &'a mut Store<WasmStoreData>, vm: &mut JsVm) -> Self {
        let prev = store.data().vm_addr;
        store.data_mut().vm_addr = std::ptr::from_mut(vm) as usize;
        store.data_mut().pending_js_exception = None;
        Self { store, prev }
    }
}

impl Drop for VmPtrGuard<'_> {
    fn drop(&mut self) {
        self.store.data_mut().vm_addr = self.prev;
    }
}

/// Installs the standard `WebAssembly` global object and all constructors/methods onto `vm`.
pub fn install_webassembly(vm: &mut JsVm) -> SharedWasmHost {
    let mut config = Config::new();
    config.cranelift_opt_level(wasmtime::OptLevel::Speed);
    let engine = Engine::new(&config).expect("initialize wasmtime Cranelift engine");
    let memories = Arc::new(Mutex::new(Vec::new()));
    let store = Store::new(
        &engine,
        WasmStoreData {
            vm_addr: 0,
            pending_js_exception: None,
            memories: Arc::clone(&memories),
        },
    );

    let obj_proto = vm.object_proto;
    let err_proto = vm.error_proto;

    let module_proto = alloc_ordinary(vm, Some(obj_proto));
    let instance_proto = alloc_ordinary(vm, Some(obj_proto));
    let memory_proto = alloc_ordinary(vm, Some(obj_proto));
    let table_proto = alloc_ordinary(vm, Some(obj_proto));
    let global_proto = alloc_ordinary(vm, Some(obj_proto));

    let compile_error_proto = alloc_wasm_error_proto(vm, err_proto, "CompileError");
    let link_error_proto = alloc_wasm_error_proto(vm, err_proto, "LinkError");
    let runtime_error_proto = alloc_wasm_error_proto(vm, err_proto, "RuntimeError");

    let host = Rc::new(WasmHost {
        engine,
        store: RefCell::new(store),
        modules: RefCell::new(Vec::new()),
        instances: RefCell::new(Vec::new()),
        memories,
        tables: RefCell::new(Vec::new()),
        globals: RefCell::new(Vec::new()),
        funcs: RefCell::new(Vec::new()),
        module_proto,
        instance_proto,
        memory_proto,
        table_proto,
        global_proto,
        compile_error_proto,
        link_error_proto,
        runtime_error_proto,
    });

    // Install prototype accessors & methods for Memory, Table, and Global.
    install_memory_prototype(vm, &host);
    install_table_prototype(vm, &host);
    install_global_prototype(vm, &host);

    // Build `WebAssembly` namespace object.
    let wasm_ns = alloc_ordinary(vm, Some(obj_proto));

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
        let ctor = vm.alloc_host_fn("Module", move |vm, _this, args| {
            let bytes =
                extract_buffer_source(vm, args.first().copied().unwrap_or(JsValue::UNDEFINED))?;
            compile_module_object(vm, &h, bytes)
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
        let exports_fn = vm.alloc_host_fn("exports", move |vm, _this, args| {
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
            PropertyAttrs::METHOD,
        );

        let h_imp = Rc::clone(&host);
        let imports_fn = vm.alloc_host_fn("imports", move |vm, _this, args| {
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
            PropertyAttrs::METHOD,
        );

        let h_cs = Rc::clone(&host);
        let custom_fn = vm.alloc_host_fn("customSections", move |vm, _this, args| {
            let mod_id = read_hidden_id(
                vm,
                args.first().copied().unwrap_or(JsValue::UNDEFINED),
                MODULE_ID_KEY,
            )
            .ok_or_else(|| vm.type_error("Argument 0 must be a WebAssembly.Module"))?;
            let target_name =
                vm.value_to_rust_string(args.get(1).copied().unwrap_or(JsValue::UNDEFINED));
            let sections: Vec<Vec<u8>> = {
                let modules = h_cs.modules.borrow();
                let (_, raw_bytes) = modules
                    .get(mod_id)
                    .ok_or_else(|| vm.type_error("Invalid WebAssembly.Module"))?;
                parse_custom_sections(raw_bytes, &target_name)
            };
            let arr = vm.alloc_array();
            for sec in sections {
                let ab = alloc_array_buffer(vm, sec);
                vm.heap.object_push_element(arr, JsValue::from_object(ab));
            }
            Ok(JsValue::from_object(arr))
        });
        define_prop(
            vm,
            ctor,
            "customSections",
            JsValue::from_object(custom_fn),
            PropertyAttrs::METHOD,
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

    // `WebAssembly.Instance` constructor.
    let inst_ctor = {
        let h = Rc::clone(&host);
        let ctor = vm.alloc_host_fn("Instance", move |vm, _this, args| {
            let mod_val = args.first().copied().unwrap_or(JsValue::UNDEFINED);
            let import_obj = args.get(1).copied().unwrap_or(JsValue::UNDEFINED);
            instantiate_module_object(vm, &h, mod_val, import_obj)
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

    // `WebAssembly.Memory` constructor.
    let mem_ctor = {
        let h = Rc::clone(&host);
        let ctor = vm.alloc_host_fn("Memory", move |vm, _this, args| {
            let desc = args.first().copied().unwrap_or(JsValue::UNDEFINED);
            let Some(desc_h) = desc.as_object() else {
                return Err(vm.type_error("WebAssembly.Memory descriptor must be an object"));
            };
            let k_init = vm.heap.intern_key("initial");
            let init_v = vm.get_property_by_key(desc_h, k_init)?;
            let initial = to_wasm_u32_limit(vm, init_v, "initial")?;
            let k_max = vm.heap.intern_key("maximum");
            let max_v = vm.get_property_by_key(desc_h, k_max)?;
            let maximum = if max_v.is_undefined() {
                None
            } else {
                let m = to_wasm_u32_limit(vm, max_v, "maximum")?;
                if m < initial {
                    return Err(vm.range_error("Memory maximum pages cannot be less than initial"));
                }
                if u64::from(m) > MAX_WASM32_PAGES {
                    return Err(vm.range_error("Memory maximum exceeds 65536 pages"));
                }
                Some(m)
            };
            if u64::from(initial) > MAX_WASM32_PAGES {
                return Err(vm.range_error("Memory initial exceeds 65536 pages"));
            }
            let mem_ty = MemoryType::new(initial, maximum);
            let mem = {
                let mut store = h.store.borrow_mut();
                Memory::new(&mut *store, mem_ty).map_err(|e| {
                    vm.range_error(format!("Failed to allocate WebAssembly.Memory: {e}"))
                })?
            };
            let mem_id = register_memory(vm, &h, mem);
            Ok(JsValue::from_object(wrap_memory_object(vm, &h, mem_id)))
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

    // `WebAssembly.Table` constructor.
    let table_ctor = {
        let h = Rc::clone(&host);
        let ctor = vm.alloc_host_fn("Table", move |vm, _this, args| {
            let desc = args.first().copied().unwrap_or(JsValue::UNDEFINED);
            let Some(desc_h) = desc.as_object() else {
                return Err(vm.type_error("WebAssembly.Table descriptor must be an object"));
            };
            let k_elem = vm.heap.intern_key("element");
            let elem_v = vm.get_property_by_key(desc_h, k_elem)?;
            let elem_s = vm.value_to_rust_string(elem_v);
            let (ref_ty, init_ref) = match elem_s.as_str() {
                "anyfunc" | "funcref" => (RefType::FUNCREF, Ref::Func(None)),
                "externref" => (RefType::EXTERNREF, Ref::Extern(None)),
                _ => {
                    return Err(vm.type_error(format!(
                        "Unsupported WebAssembly.Table element type '{elem_s}'"
                    )));
                }
            };
            let k_init = vm.heap.intern_key("initial");
            let init_v = vm.get_property_by_key(desc_h, k_init)?;
            let initial = to_wasm_u32_limit(vm, init_v, "initial")?;
            let k_max = vm.heap.intern_key("maximum");
            let max_v = vm.get_property_by_key(desc_h, k_max)?;
            let maximum = if max_v.is_undefined() {
                None
            } else {
                let m = to_wasm_u32_limit(vm, max_v, "maximum")?;
                if m < initial {
                    return Err(vm.range_error("Table maximum cannot be less than initial"));
                }
                Some(m)
            };
            let table = {
                let mut store = h.store.borrow_mut();
                Table::new(
                    &mut *store,
                    TableType::new(ref_ty, initial, maximum),
                    init_ref,
                )
                .map_err(|e| vm.range_error(format!("Failed to create WebAssembly.Table: {e}")))?
            };
            let tid = {
                let mut tables = h.tables.borrow_mut();
                let id = tables.len();
                tables.push(table);
                id
            };
            Ok(JsValue::from_object(wrap_table_object(vm, &h, tid)))
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

    // `WebAssembly.Global` constructor.
    let global_ctor = {
        let h = Rc::clone(&host);
        let ctor = vm.alloc_host_fn("Global", move |vm, _this, args| {
            let desc = args.first().copied().unwrap_or(JsValue::UNDEFINED);
            let Some(desc_h) = desc.as_object() else {
                return Err(vm.type_error("WebAssembly.Global descriptor must be an object"));
            };
            let k_val = vm.heap.intern_key("value");
            let ty_val = vm.get_property_by_key(desc_h, k_val)?;
            let ty_str = vm.value_to_rust_string(ty_val);
            let val_ty = match ty_str.as_str() {
                "i32" => ValType::I32,
                "i64" => ValType::I64,
                "f32" => ValType::F32,
                "f64" => ValType::F64,
                _ => {
                    return Err(vm.type_error(format!(
                        "Unsupported WebAssembly.Global value type '{ty_str}'"
                    )));
                }
            };
            let k_mut = vm.heap.intern_key("mutable");
            let mut_v = vm.get_property_by_key(desc_h, k_mut)?;
            let mutability = if is_js_truthy(vm, mut_v) {
                Mutability::Var
            } else {
                Mutability::Const
            };
            let init_js = args.get(1).copied().unwrap_or(JsValue::UNDEFINED);
            let init_val = if init_js.is_undefined() {
                default_wasm_val(&val_ty)
            } else {
                js_to_wasm_val(vm, init_js, &val_ty)?
            };
            let global = {
                let mut store = h.store.borrow_mut();
                Global::new(&mut *store, GlobalType::new(val_ty, mutability), init_val).map_err(
                    |e| vm.type_error(format!("Failed to create WebAssembly.Global: {e}")),
                )?
            };
            let gid = {
                let mut globals = h.globals.borrow_mut();
                let id = globals.len();
                globals.push(global);
                id
            };
            Ok(JsValue::from_object(wrap_global_object(vm, &h, gid)))
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

    // `WebAssembly.validate(bytes)`
    let validate_fn = {
        let h = Rc::clone(&host);
        vm.alloc_host_fn("validate", move |vm, _this, args| {
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
        PropertyAttrs::METHOD,
    );

    // `WebAssembly.compile(bytes) -> Promise<Module>`
    let compile_fn = {
        let h = Rc::clone(&host);
        vm.alloc_host_fn("compile", move |vm, _this, args| {
            let p = alloc_promise(vm);
            match extract_buffer_source(vm, args.first().copied().unwrap_or(JsValue::UNDEFINED))
                .and_then(|bytes| compile_module_object(vm, &h, bytes))
            {
                Ok(mod_val) => resolve_promise(vm, p, mod_val),
                Err(exc) => reject_promise(vm, p, exc.value),
            }
            Ok(JsValue::from_object(p))
        })
    };
    define_prop(
        vm,
        wasm_ns,
        "compile",
        JsValue::from_object(compile_fn),
        PropertyAttrs::METHOD,
    );

    // `WebAssembly.instantiate(bytesOrModule, importObject?) -> Promise<{module, instance} | Instance>`
    let instantiate_fn = {
        let h = Rc::clone(&host);
        vm.alloc_host_fn("instantiate", move |vm, _this, args| {
            let p = alloc_promise(vm);
            let first = args.first().copied().unwrap_or(JsValue::UNDEFINED);
            let imports = args.get(1).copied().unwrap_or(JsValue::UNDEFINED);
            if read_hidden_id(vm, first, MODULE_ID_KEY).is_some() {
                match instantiate_module_object(vm, &h, first, imports) {
                    Ok(inst) => resolve_promise(vm, p, inst),
                    Err(exc) => reject_promise(vm, p, exc.value),
                }
            } else {
                let res = extract_buffer_source(vm, first)
                    .and_then(|bytes| compile_module_object(vm, &h, bytes))
                    .and_then(|mod_val| {
                        let inst_val = instantiate_module_object(vm, &h, mod_val, imports)?;
                        let pair = alloc_ordinary(vm, Some(vm.object_proto));
                        define_prop(vm, pair, "module", mod_val, PropertyAttrs::DEFAULT);
                        define_prop(vm, pair, "instance", inst_val, PropertyAttrs::DEFAULT);
                        Ok(JsValue::from_object(pair))
                    });
                match res {
                    Ok(v) => resolve_promise(vm, p, v),
                    Err(exc) => reject_promise(vm, p, exc.value),
                }
            }
            Ok(JsValue::from_object(p))
        })
    };
    define_prop(
        vm,
        wasm_ns,
        "instantiate",
        JsValue::from_object(instantiate_fn),
        PropertyAttrs::METHOD,
    );

    // `WebAssembly.compileStreaming(source) -> Promise<Module>`
    let compile_streaming_fn = {
        let h = Rc::clone(&host);
        vm.alloc_host_fn("compileStreaming", move |vm, _this, args| {
            let source = args.first().copied().unwrap_or(JsValue::UNDEFINED);
            Ok(JsValue::from_object(compile_streaming_impl(vm, &h, source)))
        })
    };
    define_prop(
        vm,
        wasm_ns,
        "compileStreaming",
        JsValue::from_object(compile_streaming_fn),
        PropertyAttrs::METHOD,
    );

    // `WebAssembly.instantiateStreaming(source, importObject?) -> Promise<{module, instance}>`
    let instantiate_streaming_fn = {
        let h = Rc::clone(&host);
        vm.alloc_host_fn("instantiateStreaming", move |vm, _this, args| {
            let source = args.first().copied().unwrap_or(JsValue::UNDEFINED);
            let import_obj = args.get(1).copied().unwrap_or(JsValue::UNDEFINED);
            let mod_promise = compile_streaming_impl(vm, &h, source);
            let h_inst = Rc::clone(&h);
            let on_mod =
                vm.alloc_host_fn("wasmInstantiateStreamingThen", move |vm, _this, margs| {
                    let mod_val = margs.first().copied().unwrap_or(JsValue::UNDEFINED);
                    let inst_val = instantiate_module_object(vm, &h_inst, mod_val, import_obj)?;
                    let pair = alloc_ordinary(vm, Some(vm.object_proto));
                    define_prop(vm, pair, "module", mod_val, PropertyAttrs::DEFAULT);
                    define_prop(vm, pair, "instance", inst_val, PropertyAttrs::DEFAULT);
                    Ok(JsValue::from_object(pair))
                });
            let chained = promise_then(vm, mod_promise, Some(on_mod), None);
            Ok(JsValue::from_object(chained))
        })
    };
    define_prop(
        vm,
        wasm_ns,
        "instantiateStreaming",
        JsValue::from_object(instantiate_streaming_fn),
        PropertyAttrs::METHOD,
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
                    let ct = vm.value_to_rust_string(ct_v).to_ascii_lowercase();
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
            compile_module_object(vm, &h_mod, bytes)
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

fn compile_module_object(
    vm: &mut JsVm,
    host: &SharedWasmHost,
    bytes: Vec<u8>,
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
    let obj = alloc_ordinary(vm, Some(host.module_proto));
    define_hidden_id(vm, obj, MODULE_ID_KEY, mod_id);
    Ok(JsValue::from_object(obj))
}

fn instantiate_module_object(
    vm: &mut JsVm,
    host: &SharedWasmHost,
    mod_val: JsValue,
    import_obj: JsValue,
) -> Result<JsValue, JsException> {
    let mod_id = read_hidden_id(vm, mod_val, MODULE_ID_KEY)
        .ok_or_else(|| vm.type_error("Argument 0 must be a WebAssembly.Module"))?;
    let module = {
        let mods = host.modules.borrow();
        mods.get(mod_id)
            .map(|(m, _)| m.clone())
            .ok_or_else(|| vm.type_error("Invalid WebAssembly.Module"))?
    };

    let import_specs: Vec<(String, String, ExternType)> = module
        .imports()
        .map(|i| (i.module().to_string(), i.name().to_string(), i.ty()))
        .collect();

    if !import_specs.is_empty() && !import_obj.is_object() {
        return Err(vm.type_error("importObject must be an object when module has imports"));
    }

    // Resolve every declared import in module declaration order.
    let mut resolved_imports: Vec<Extern> = Vec::with_capacity(import_specs.len());
    let mut imported_mem_ids: Vec<(usize, Memory)> = Vec::new();
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
                    resolved_imports.push(Extern::Func(f));
                } else if item_val.is_object() {
                    let host_func = wrap_js_fn_as_wasm_import(host, item_val, func_ty);
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
                let mem = host
                    .memories
                    .lock()
                    .expect("wasm memories lock")
                    .get(mid)
                    .map(|b| b.memory)
                    .ok_or_else(|| {
                        make_wasm_error(
                            vm,
                            host.link_error_proto,
                            "LinkError",
                            "Invalid Memory import",
                        )
                    })?;
                imported_mem_ids.push((mid, mem));
                resolved_imports.push(Extern::Memory(mem));
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
                let tbl = *host.tables.borrow().get(tid).ok_or_else(|| {
                    make_wasm_error(
                        vm,
                        host.link_error_proto,
                        "LinkError",
                        "Invalid Table import",
                    )
                })?;
                resolved_imports.push(Extern::Table(tbl));
            }
            ExternType::Global(g_ty) => {
                if let Some(gid) = read_hidden_id(vm, item_val, GLOBAL_ID_KEY) {
                    let g = *host.globals.borrow().get(gid).ok_or_else(|| {
                        make_wasm_error(
                            vm,
                            host.link_error_proto,
                            "LinkError",
                            "Invalid Global import",
                        )
                    })?;
                    resolved_imports.push(Extern::Global(g));
                } else if g_ty.mutability() == Mutability::Const {
                    // W3C JS-API allows importing a plain JS Number/BigInt for immutable globals!
                    let val = js_to_wasm_val(vm, item_val, g_ty.content()).map_err(|_| {
                        make_wasm_error(
                            vm,
                            host.link_error_proto,
                            "LinkError",
                            format!(
                                "Invalid value for immutable global import '{imp_mod}.{imp_name}'"
                            ),
                        )
                    })?;
                    let g = {
                        let mut store = host.store.borrow_mut();
                        Global::new(&mut *store, g_ty, val).map_err(|e| {
                            make_wasm_error(vm, host.link_error_proto, "LinkError", e.to_string())
                        })?
                    };
                    resolved_imports.push(Extern::Global(g));
                } else {
                    return Err(make_wasm_error(
                        vm,
                        host.link_error_proto,
                        "LinkError",
                        format!("Mutable global import '{imp_mod}.{imp_name}' must be a WebAssembly.Global"),
                    ));
                }
            }
            ExternType::Tag(_) => {
                return Err(make_wasm_error(
                    vm,
                    host.link_error_proto,
                    "LinkError",
                    format!("Unsupported tag import '{imp_mod}.{imp_name}'"),
                ));
            }
        }
    }

    // Sync any imported JS memory buffers into Wasmtime BEFORE start-function / data segment init.
    sync_all_js_to_wasm(vm, host);

    let inst_result = {
        let mut store = host.store.borrow_mut();
        let guard = VmPtrGuard::enter(&mut store, vm);
        let res = Instance::new(&mut *guard.store, &module, &resolved_imports);
        let js_exc = guard.store.data_mut().pending_js_exception.take();
        (res, js_exc)
    };

    let instance = match inst_result {
        (Ok(inst), _) => inst,
        (Err(_err), Some(js_exc)) => {
            sync_all_wasm_to_js(vm, host);
            return Err(js_exc);
        }
        (Err(err), None) => {
            sync_all_wasm_to_js(vm, host);
            let msg = err.to_string();
            // Classify start-function traps as `RuntimeError`, linking/type mismatches as `LinkError`.
            if msg.contains("wasm trap")
                || msg.contains("unreachable")
                || msg.contains("out of bounds")
            {
                return Err(make_wasm_error(
                    vm,
                    host.runtime_error_proto,
                    "RuntimeError",
                    msg,
                ));
            }
            return Err(make_wasm_error(vm, host.link_error_proto, "LinkError", msg));
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

    // Build `instance.exports` namespace object.
    let exports_obj = alloc_ordinary(vm, None);
    let export_items: Vec<(String, Extern)> = {
        let mut store = host.store.borrow_mut();
        module
            .exports()
            .filter_map(|e| {
                let name = e.name().to_string();
                instance.get_export(&mut *store, &name).map(|ex| (name, ex))
            })
            .collect()
    };

    let mut mem_index_by_ptr: HashMap<usize, usize> = HashMap::new();
    {
        let store = host.store.borrow();
        for &(mid, mem) in &imported_mem_ids {
            mem_index_by_ptr.insert(mem.data(&*store).as_ptr() as usize, mid);
        }
    }

    for (name, ext) in export_items {
        let js_val = match ext {
            Extern::Func(f) => JsValue::from_object(wrap_exported_func(vm, host, &name, f)),
            Extern::Memory(mem) => {
                let ptr_key = {
                    let store = host.store.borrow();
                    mem.data(&*store).as_ptr() as usize
                };
                let mid = if let Some(&existing_mid) = mem_index_by_ptr.get(&ptr_key) {
                    existing_mid
                } else {
                    let new_id = register_memory(vm, host, mem);
                    mem_index_by_ptr.insert(ptr_key, new_id);
                    new_id
                };
                JsValue::from_object(wrap_memory_object(vm, host, mid))
            }
            Extern::Table(tbl) => {
                let tid = {
                    let mut tables = host.tables.borrow_mut();
                    let id = tables.len();
                    tables.push(tbl);
                    id
                };
                JsValue::from_object(wrap_table_object(vm, host, tid))
            }
            Extern::Global(g) => {
                let gid = {
                    let mut globals = host.globals.borrow_mut();
                    let id = globals.len();
                    globals.push(g);
                    id
                };
                JsValue::from_object(wrap_global_object(vm, host, gid))
            }
            Extern::Tag(_) | Extern::SharedMemory(_) => JsValue::UNDEFINED,
        };
        define_prop(vm, exports_obj, &name, js_val, PropertyAttrs::READONLY);
    }

    let inst_obj = alloc_ordinary(vm, Some(host.instance_proto));
    define_hidden_id(vm, inst_obj, INSTANCE_ID_KEY, inst_id);
    define_prop(
        vm,
        inst_obj,
        "exports",
        JsValue::from_object(exports_obj),
        PropertyAttrs::READONLY,
    );
    Ok(JsValue::from_object(inst_obj))
}

// --- Exported Wasm Function Wrapper & Imported JS Function Bridge ---

fn wrap_exported_func(vm: &mut JsVm, host: &SharedWasmHost, name: &str, func: Func) -> HeapHandle {
    let fid = {
        let mut funcs = host.funcs.borrow_mut();
        let id = funcs.len();
        funcs.push(func);
        id
    };
    let h = Rc::clone(host);
    let wrapper = vm.alloc_host_fn(name, move |vm, _this, args| {
        let (param_tys, result_tys) = {
            let store = h.store.borrow();
            let ty = func.ty(&*store);
            (
                ty.params().collect::<Vec<_>>(),
                ty.results().collect::<Vec<_>>(),
            )
        };

        let mut wasm_args = Vec::with_capacity(param_tys.len());
        for (i, pty) in param_tys.iter().enumerate() {
            let jv = args.get(i).copied().unwrap_or(JsValue::UNDEFINED);
            wasm_args.push(js_to_wasm_val(vm, jv, pty)?);
        }
        let mut wasm_results: Vec<Val> = result_tys.iter().map(default_wasm_val).collect();

        // Sync JS `ArrayBuffer` edits into Wasm linear memories before calling Wasm.
        sync_all_js_to_wasm(vm, &h);

        let call_outcome = {
            let mut store = h.store.borrow_mut();
            let guard = VmPtrGuard::enter(&mut store, vm);
            let res = func.call(&mut *guard.store, &wasm_args, &mut wasm_results);
            let js_exc = guard.store.data_mut().pending_js_exception.take();
            (res, js_exc)
        };

        // Always sync Wasm linear memory mutations (and any `memory.grow`) back to JS!
        sync_all_wasm_to_js(vm, &h);

        match call_outcome {
            (Ok(()), _) => match wasm_results.len() {
                0 => Ok(JsValue::UNDEFINED),
                1 => Ok(wasm_to_js_val(vm, &wasm_results[0])),
                _ => {
                    let arr = vm.alloc_array();
                    for r in &wasm_results {
                        let jv = wasm_to_js_val(vm, r);
                        vm.heap.object_push_element(arr, jv);
                    }
                    Ok(JsValue::from_object(arr))
                }
            },
            (Err(_), Some(js_exc)) => Err(js_exc),
            (Err(trap), None) => Err(make_wasm_error(
                vm,
                h.runtime_error_proto,
                "RuntimeError",
                trap.to_string(),
            )),
        }
    });
    define_hidden_id(vm, wrapper, FUNC_ID_KEY, fid);
    wrapper
}

fn wrap_js_fn_as_wasm_import(
    host: &SharedWasmHost,
    js_fn: JsValue,
    func_ty: wasmtime::FuncType,
) -> Func {
    let mut store = host.store.borrow_mut();
    let result_tys: Vec<ValType> = func_ty.results().collect();
    let js_fn_raw = js_fn.to_raw();
    Func::new(
        &mut *store,
        func_ty,
        move |mut caller: Caller<'_, WasmStoreData>, params, results| {
            let vm_addr = caller.data().vm_addr;
            if vm_addr == 0 {
                return Err(wasmtime::Error::msg(
                    "No active JsVm during Wasm host import call",
                ));
            }
            let memories = Arc::clone(&caller.data().memories);
            // SAFETY: `VmPtrGuard` guarantees `vm_addr` is a valid exclusive `&mut JsVm` pointer
            // for the exact synchronous duration of `func.call` / `Instance::new` on this thread.
            let vm = unsafe { &mut *(vm_addr as *mut JsVm) };

            // Sync Wasm linear memory -> JS ArrayBuffer before invoking JS import!
            sync_memories_with_ctx_to_js(vm, &memories, caller.as_context());

            let js_args: Vec<JsValue> = params.iter().map(|p| wasm_to_js_val(vm, p)).collect();
            let call_res =
                vm.call_value(JsValue::from_raw(js_fn_raw), JsValue::UNDEFINED, &js_args);

            // Sync any JS ArrayBuffer writes made by the import callback back to Wasm!
            sync_memories_with_ctx_from_js(vm, &memories, caller.as_context_mut());

            match call_res {
                Ok(ret_val) => {
                    match result_tys.len() {
                        0 => {}
                        1 => {
                            let wv =
                                js_to_wasm_val(vm, ret_val, &result_tys[0]).map_err(|exc| {
                                    caller.data_mut().pending_js_exception = Some(exc);
                                    wasmtime::Error::msg("__rix_js_exception__")
                                })?;
                            results[0] = wv;
                        }
                        n => {
                            let Some(arr_h) = ret_val.as_object() else {
                                let exc =
                                    vm.type_error("Multi-value Wasm import must return an array");
                                caller.data_mut().pending_js_exception = Some(exc);
                                return Err(wasmtime::Error::msg("__rix_js_exception__"));
                            };
                            for (i, rty) in result_tys.iter().enumerate().take(n) {
                                let elem = vm
                                    .heap
                                    .get_object(arr_h)
                                    .and_then(|o| o.elements.get(i as u32))
                                    .unwrap_or(JsValue::UNDEFINED);
                                let wv = js_to_wasm_val(vm, elem, rty).map_err(|exc| {
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
                    caller.data_mut().pending_js_exception = Some(exc);
                    Err(wasmtime::Error::msg("__rix_js_exception__"))
                }
            }
        },
    )
}

// --- Memory, Table, Global Prototypes & Linear Memory <-> ArrayBuffer Bridge ---

fn register_memory(vm: &mut JsVm, host: &SharedWasmHost, memory: Memory) -> usize {
    let bytes = {
        let store = host.store.borrow();
        memory.data(&*store).to_vec()
    };
    let byte_len = bytes.len();
    let js_buffer = alloc_array_buffer(vm, bytes);
    let mut mems = host.memories.lock().expect("wasm memories lock");
    let id = mems.len();
    mems.push(MemoryBridge {
        memory,
        js_buffer,
        last_byte_len: byte_len,
    });
    id
}

fn sync_all_js_to_wasm(vm: &JsVm, host: &SharedWasmHost) {
    let mut store = host.store.borrow_mut();
    sync_memories_with_ctx_from_js(vm, &host.memories, store.as_context_mut());
}

fn sync_all_wasm_to_js(vm: &mut JsVm, host: &SharedWasmHost) {
    let store = host.store.borrow();
    sync_memories_with_ctx_to_js(vm, &host.memories, store.as_context());
}

fn sync_memories_with_ctx_from_js(
    vm: &JsVm,
    memories: &Arc<Mutex<Vec<MemoryBridge>>>,
    mut ctx: wasmtime::StoreContextMut<'_, WasmStoreData>,
) {
    let mems = memories.lock().expect("wasm memories lock");
    for bridge in mems.iter() {
        if let Some(ObjectKind::ArrayBuffer(js_bytes)) =
            vm.heap.get_object(bridge.js_buffer).map(|o| &o.kind)
        {
            let dst = bridge.memory.data_mut(&mut ctx);
            if js_bytes.len() == dst.len() {
                dst.copy_from_slice(js_bytes);
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
        let src = bridge.memory.data(&ctx);
        if src.len() != bridge.last_byte_len {
            if let Some(ObjectKind::ArrayBuffer(old_buf)) = vm
                .heap
                .get_object_mut(bridge.js_buffer)
                .map(|o| &mut o.kind)
            {
                old_buf.clear();
            }
            bridge.js_buffer = alloc_array_buffer(vm, src.to_vec());
            bridge.last_byte_len = src.len();
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
    let get_buffer = vm.alloc_host_fn("get buffer", move |vm, this, _args| {
        let mid = read_hidden_id(vm, this, MEMORY_ID_KEY).ok_or_else(|| {
            vm.type_error("Memory.prototype.buffer called on incompatible receiver")
        })?;
        if let Ok(store) = h_buf.store.try_borrow() {
            let mut mems = h_buf.memories.lock().expect("wasm memories lock");
            if let Some(bridge) = mems.get_mut(mid) {
                let src = bridge.memory.data(&*store);
                if src.len() != bridge.last_byte_len {
                    if let Some(ObjectKind::ArrayBuffer(old_buf)) = vm
                        .heap
                        .get_object_mut(bridge.js_buffer)
                        .map(|o| &mut o.kind)
                    {
                        old_buf.clear();
                    }
                    bridge.js_buffer = alloc_array_buffer(vm, src.to_vec());
                    bridge.last_byte_len = src.len();
                }
            }
        }
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
    let grow_fn = vm.alloc_host_fn("grow", move |vm, this, args| {
        let mid = read_hidden_id(vm, this, MEMORY_ID_KEY).ok_or_else(|| {
            vm.type_error("Memory.prototype.grow called on incompatible receiver")
        })?;
        let delta = to_wasm_u32_limit(
            vm,
            args.first().copied().unwrap_or(JsValue::UNDEFINED),
            "delta",
        )?;

        sync_all_js_to_wasm(vm, &h_grow);

        let (prev_pages, new_bytes) = {
            let mut store = h_grow.store.borrow_mut();
            let mem = h_grow
                .memories
                .lock()
                .expect("wasm memories lock")
                .get(mid)
                .map(|b| b.memory)
                .ok_or_else(|| vm.type_error("Invalid WebAssembly.Memory"))?;
            let prev = mem
                .grow(&mut *store, u64::from(delta))
                .map_err(|e| vm.range_error(format!("WebAssembly.Memory.grow failed: {e}")))?;
            let bytes = mem.data(&*store).to_vec();
            (prev, bytes)
        };

        let new_len = new_bytes.len();
        let new_ab = alloc_array_buffer(vm, new_bytes);
        let mut mems = h_grow.memories.lock().expect("wasm memories lock");
        if let Some(bridge) = mems.get_mut(mid) {
            if let Some(ObjectKind::ArrayBuffer(old_buf)) = vm
                .heap
                .get_object_mut(bridge.js_buffer)
                .map(|o| &mut o.kind)
            {
                old_buf.clear();
            }
            bridge.js_buffer = new_ab;
            bridge.last_byte_len = new_len;
        }
        Ok(JsValue::from_number(prev_pages as f64))
    });
    define_prop(
        vm,
        proto,
        "grow",
        JsValue::from_object(grow_fn),
        PropertyAttrs::METHOD,
    );
}

fn install_table_prototype(vm: &mut JsVm, host: &SharedWasmHost) {
    let proto = host.table_proto;

    // `get Table.prototype.length`
    let h_len = Rc::clone(host);
    let get_len = vm.alloc_host_fn("get length", move |vm, this, _args| {
        let tid = read_hidden_id(vm, this, TABLE_ID_KEY).ok_or_else(|| {
            vm.type_error("Table.prototype.length called on incompatible receiver")
        })?;
        let store = h_len.store.borrow();
        let tbl = *h_len
            .tables
            .borrow()
            .get(tid)
            .ok_or_else(|| vm.type_error("Invalid WebAssembly.Table"))?;
        Ok(JsValue::from_number(tbl.size(&*store) as f64))
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
    let get_fn = vm.alloc_host_fn("get", move |vm, this, args| {
        let tid = read_hidden_id(vm, this, TABLE_ID_KEY)
            .ok_or_else(|| vm.type_error("Table.prototype.get called on incompatible receiver"))?;
        let idx = to_wasm_u32_limit(
            vm,
            args.first().copied().unwrap_or(JsValue::UNDEFINED),
            "index",
        )?;
        let elem = {
            let mut store = h_get.store.borrow_mut();
            let tbl = *h_get
                .tables
                .borrow()
                .get(tid)
                .ok_or_else(|| vm.type_error("Invalid WebAssembly.Table"))?;
            tbl.get(&mut *store, u64::from(idx))
                .ok_or_else(|| vm.range_error("WebAssembly.Table.get index out of bounds"))?
        };
        match elem {
            Ref::Func(Some(f)) => Ok(JsValue::from_object(wrap_exported_func(vm, &h_get, "", f))),
            Ref::Func(None) | Ref::Extern(None) => Ok(JsValue::NULL),
            _ => Ok(JsValue::UNDEFINED),
        }
    });
    define_prop(
        vm,
        proto,
        "get",
        JsValue::from_object(get_fn),
        PropertyAttrs::METHOD,
    );

    // `Table.prototype.set(index, value)`
    let h_set = Rc::clone(host);
    let set_fn = vm.alloc_host_fn("set", move |vm, this, args| {
        let tid = read_hidden_id(vm, this, TABLE_ID_KEY)
            .ok_or_else(|| vm.type_error("Table.prototype.set called on incompatible receiver"))?;
        let idx = to_wasm_u32_limit(
            vm,
            args.first().copied().unwrap_or(JsValue::UNDEFINED),
            "index",
        )?;
        let val = args.get(1).copied().unwrap_or(JsValue::NULL);
        let ref_val = if val.is_null() || val.is_undefined() {
            Ref::Func(None)
        } else if let Some(fid) = read_hidden_id(vm, val, FUNC_ID_KEY) {
            let f = *h_set
                .funcs
                .borrow()
                .get(fid)
                .ok_or_else(|| vm.type_error("Invalid Wasm function reference"))?;
            Ref::Func(Some(f))
        } else {
            return Err(
                vm.type_error("WebAssembly.Table.set value must be a Wasm function or null")
            );
        };
        let mut store = h_set.store.borrow_mut();
        let tbl = *h_set
            .tables
            .borrow()
            .get(tid)
            .ok_or_else(|| vm.type_error("Invalid WebAssembly.Table"))?;
        tbl.set(&mut *store, u64::from(idx), ref_val)
            .map_err(|e| vm.range_error(format!("WebAssembly.Table.set failed: {e}")))?;
        Ok(JsValue::UNDEFINED)
    });
    define_prop(
        vm,
        proto,
        "set",
        JsValue::from_object(set_fn),
        PropertyAttrs::METHOD,
    );

    // `Table.prototype.grow(delta, value?)`
    let h_grow = Rc::clone(host);
    let grow_fn = vm.alloc_host_fn("grow", move |vm, this, args| {
        let tid = read_hidden_id(vm, this, TABLE_ID_KEY)
            .ok_or_else(|| vm.type_error("Table.prototype.grow called on incompatible receiver"))?;
        let delta = to_wasm_u32_limit(
            vm,
            args.first().copied().unwrap_or(JsValue::UNDEFINED),
            "delta",
        )?;
        let init_v = args.get(1).copied().unwrap_or(JsValue::NULL);
        let init_ref = if init_v.is_null() || init_v.is_undefined() {
            Ref::Func(None)
        } else if let Some(fid) = read_hidden_id(vm, init_v, FUNC_ID_KEY) {
            let f = *h_grow
                .funcs
                .borrow()
                .get(fid)
                .ok_or_else(|| vm.type_error("Invalid Wasm function reference"))?;
            Ref::Func(Some(f))
        } else {
            return Err(
                vm.type_error("WebAssembly.Table.grow init value must be a Wasm function or null")
            );
        };
        let mut store = h_grow.store.borrow_mut();
        let tbl = *h_grow
            .tables
            .borrow()
            .get(tid)
            .ok_or_else(|| vm.type_error("Invalid WebAssembly.Table"))?;
        let prev = tbl
            .grow(&mut *store, u64::from(delta), init_ref)
            .map_err(|e| vm.range_error(format!("WebAssembly.Table.grow failed: {e}")))?;
        Ok(JsValue::from_number(prev as f64))
    });
    define_prop(
        vm,
        proto,
        "grow",
        JsValue::from_object(grow_fn),
        PropertyAttrs::METHOD,
    );
}

fn install_global_prototype(vm: &mut JsVm, host: &SharedWasmHost) {
    let proto = host.global_proto;

    let h_get = Rc::clone(host);
    let getter = vm.alloc_host_fn("get value", move |vm, this, _args| {
        let gid = read_hidden_id(vm, this, GLOBAL_ID_KEY).ok_or_else(|| {
            vm.type_error("Global.prototype.value called on incompatible receiver")
        })?;
        let wv = {
            let mut store = h_get.store.borrow_mut();
            let g = *h_get
                .globals
                .borrow()
                .get(gid)
                .ok_or_else(|| vm.type_error("Invalid WebAssembly.Global"))?;
            g.get(&mut *store)
        };
        Ok(wasm_to_js_val(vm, &wv))
    });

    let h_set = Rc::clone(host);
    let setter = vm.alloc_host_fn("set value", move |vm, this, args| {
        let gid = read_hidden_id(vm, this, GLOBAL_ID_KEY).ok_or_else(|| {
            vm.type_error("Global.prototype.value called on incompatible receiver")
        })?;
        let val_ty = {
            let store = h_set.store.borrow();
            let g = *h_set
                .globals
                .borrow()
                .get(gid)
                .ok_or_else(|| vm.type_error("Invalid WebAssembly.Global"))?;
            let ty = g.ty(&*store);
            if ty.mutability() != Mutability::Var {
                return Err(vm.type_error("Cannot modify immutable WebAssembly.Global"));
            }
            ty.content().clone()
        };
        let jv = args.first().copied().unwrap_or(JsValue::UNDEFINED);
        let wv = js_to_wasm_val(vm, jv, &val_ty)?;
        let mut store = h_set.store.borrow_mut();
        let g = *h_set
            .globals
            .borrow()
            .get(gid)
            .ok_or_else(|| vm.type_error("Invalid WebAssembly.Global"))?;
        g.set(&mut *store, wv)
            .map_err(|e| vm.type_error(format!("WebAssembly.Global.value set failed: {e}")))?;
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
    let value_of = vm.alloc_host_fn("valueOf", move |vm, this, _args| {
        let gid = read_hidden_id(vm, this, GLOBAL_ID_KEY).ok_or_else(|| {
            vm.type_error("Global.prototype.valueOf called on incompatible receiver")
        })?;
        let wv = {
            let mut store = h_vo.store.borrow_mut();
            let g = *h_vo
                .globals
                .borrow()
                .get(gid)
                .ok_or_else(|| vm.type_error("Invalid WebAssembly.Global"))?;
            g.get(&mut *store)
        };
        Ok(wasm_to_js_val(vm, &wv))
    });
    define_prop(
        vm,
        proto,
        "valueOf",
        JsValue::from_object(value_of),
        PropertyAttrs::METHOD,
    );
}

fn wrap_memory_object(vm: &mut JsVm, host: &SharedWasmHost, mem_id: usize) -> HeapHandle {
    let obj = alloc_ordinary(vm, Some(host.memory_proto));
    define_hidden_id(vm, obj, MEMORY_ID_KEY, mem_id);
    obj
}

fn wrap_table_object(vm: &mut JsVm, host: &SharedWasmHost, tid: usize) -> HeapHandle {
    let obj = alloc_ordinary(vm, Some(host.table_proto));
    define_hidden_id(vm, obj, TABLE_ID_KEY, tid);
    obj
}

fn wrap_global_object(vm: &mut JsVm, host: &SharedWasmHost, gid: usize) -> HeapHandle {
    let obj = alloc_ordinary(vm, Some(host.global_proto));
    define_hidden_id(vm, obj, GLOBAL_ID_KEY, gid);
    obj
}

// --- JS <-> Wasm Value Conversion & Binary Helpers ---

fn default_wasm_val(ty: &ValType) -> Val {
    match ty {
        ValType::I32 => Val::I32(0),
        ValType::I64 => Val::I64(0),
        ValType::F32 => Val::F32(0),
        ValType::F64 => Val::F64(0),
        _ => Val::I32(0),
    }
}

fn js_to_wasm_val(vm: &mut JsVm, val: JsValue, ty: &ValType) -> Result<Val, JsException> {
    match ty {
        ValType::I32 => {
            if val.is_bigint() {
                return Err(vm.type_error("Cannot convert BigInt to WebAssembly i32"));
            }
            let n = js_to_f64(vm, val)?;
            let i = if !n.is_finite() || n == 0.0 {
                0i32
            } else {
                (n.trunc() as i64) as i32
            };
            Ok(Val::I32(i))
        }
        ValType::I64 => {
            if let Some(bh) = val.as_bigint() {
                let bi = vm
                    .heap
                    .get_bigint(bh)
                    .cloned()
                    .unwrap_or_else(|| JsBigInt::from_i64(0));
                Ok(Val::I64(bi.to_i64_lossy()))
            } else {
                Err(vm.type_error("Cannot convert non-BigInt value to WebAssembly i64"))
            }
        }
        ValType::F32 => {
            if val.is_bigint() {
                return Err(vm.type_error("Cannot convert BigInt to WebAssembly f32"));
            }
            let n = js_to_f64(vm, val)? as f32;
            Ok(Val::F32(n.to_bits()))
        }
        ValType::F64 => {
            if val.is_bigint() {
                return Err(vm.type_error("Cannot convert BigInt to WebAssembly f64"));
            }
            let n = js_to_f64(vm, val)?;
            Ok(Val::F64(n.to_bits()))
        }
        _ => Err(vm.type_error(format!("Unsupported WebAssembly value type: {ty}"))),
    }
}

fn wasm_to_js_val(vm: &mut JsVm, val: &Val) -> JsValue {
    match val {
        Val::I32(i) => JsValue::from_i32(*i),
        Val::I64(i) => JsValue::from_bigint(vm.heap.alloc_bigint(JsBigInt::from_i64(*i))),
        Val::F32(bits) => JsValue::from_number(f64::from(f32::from_bits(*bits))),
        Val::F64(bits) => JsValue::from_number(f64::from_bits(*bits)),
        _ => JsValue::UNDEFINED,
    }
}

fn js_to_f64(vm: &mut JsVm, val: JsValue) -> Result<f64, JsException> {
    if let Some(n) = val.as_number() {
        return Ok(n);
    }
    if val.is_undefined() {
        return Ok(f64::NAN);
    }
    if val.is_null() {
        return Ok(0.0);
    }
    if let Some(b) = val.as_bool() {
        return Ok(if b { 1.0 } else { 0.0 });
    }
    if val.is_symbol() || val.is_bigint() {
        return Err(vm.type_error("Cannot convert Symbol or BigInt to number"));
    }
    let s = vm.value_to_rust_string(val);
    let t = s.trim();
    if t.is_empty() {
        return Ok(0.0);
    }
    Ok(t.parse::<f64>().unwrap_or(f64::NAN))
}

fn to_wasm_u32_limit(vm: &mut JsVm, val: JsValue, field: &str) -> Result<u32, JsException> {
    let n = js_to_f64(vm, val)?;
    if !n.is_finite() || n < 0.0 || n > f64::from(u32::MAX) {
        return Err(vm.type_error(format!(
            "WebAssembly '{field}' must be a non-negative 32-bit integer"
        )));
    }
    Ok(n.trunc() as u32)
}

fn extract_buffer_source(vm: &mut JsVm, val: JsValue) -> Result<Vec<u8>, JsException> {
    let Some(h) = val.as_object() else {
        return Err(vm.type_error("Expected BufferSource (ArrayBuffer, TypedArray, or DataView)"));
    };
    match vm.heap.get_object(h).map(|o| &o.kind) {
        Some(ObjectKind::ArrayBuffer(b)) => Ok((**b).clone()),
        Some(ObjectKind::TypedArray(ta)) => {
            let buf_h = ta.buffer;
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

fn alloc_ordinary(vm: &mut JsVm, proto: Option<HeapHandle>) -> HeapHandle {
    vm.heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, proto))
}

fn alloc_array_buffer(vm: &mut JsVm, bytes: Vec<u8>) -> HeapHandle {
    vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::ArrayBuffer(Box::new(bytes)),
        Some(vm.array_buffer_proto),
    ))
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
    let ctor = vm.alloc_host_fn(name, move |vm, _this, args| {
        let msg = args
            .first()
            .copied()
            .filter(|v| !v.is_undefined())
            .map(|v| vm.value_to_rust_string(v))
            .unwrap_or_default();
        let obj = alloc_ordinary(vm, Some(proto));
        let n = JsValue::from_string(vm.heap.alloc_string(name));
        let m = JsValue::from_string(vm.heap.alloc_string(msg));
        define_prop(vm, obj, "name", n, PropertyAttrs::METHOD);
        define_prop(vm, obj, "message", m, PropertyAttrs::METHOD);
        Ok(JsValue::from_object(obj))
    });
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
