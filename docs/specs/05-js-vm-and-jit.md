# Spec 05: Custom Pure-Rust JavaScript Engine, Tiered JIT & WebAssembly (`rix-js`, `rix-jit`)

**Status:** Draft (Phase 2: Bytecode VM -> Phase 3: Tiered JIT + Wasm)  
**Governing Architecture:** [docs/ARCHITECTURE.md](../ARCHITECTURE.md)  
**Unsafe Policy:**
- `rix-js`: `#![deny(unsafe_code)]` across parser, compiler, bytecode VM, builtins, and DOM bindings.
- `rix-jit`: `#![deny(unsafe_op_in_unsafe_fn)]`; isolated `unsafe` restricted strictly to executable page allocation (`mmap`/`mprotect`) and calling finalized JIT code entries.

---

## 1. Rationale: Why a Clean Custom VM Instead of `boa_engine`

A modern browser running React/Vue/Next/YouTube spends the majority of CPU time in JS property lookup, object allocation, closures, strings, and DOM calls. Wrapping a spec-interpreter with an `i32`-only JIT that escapes on every float, string, object, or function call cannot reach modern web frame budgets.

RixBrowser therefore builds a **clean, pure-Rust JS engine designed from day one for Inline Caches, NaN-boxing, and full-type Cranelift JIT compilation**.

---

## 2. Value Representation & Garbage Collector

### 2.1 64-Bit NaN-Boxed `JsValue`
Every JS value fits in a single 8-byte `u64` (`#[repr(transparent)] pub struct JsValue(u64)`), enabling register-resident VM execution and zero-cost ABI between interpreter and JIT:

| Tag (High 16 bits) | Payload (Low 48 bits) | Meaning |
| :--- | :--- | :--- |
| `< 0xFFF8` | IEEE-754 double bits | `Number` (`f64`, canonical quiet NaN = `0x7FF8_0000_0000_0000`) |
| `0xFFF9` | `i32` (low 32 bits) | Fast signed 32-bit integer (`Smi` / `Int32`) |
| `0xFFFA` | `0` / `1` / `2` / `3` | `undefined` / `null` / `false` / `true` |
| `0xFFFB` | `HeapIndex(u32)` + gen | `JsString` (interned or rope/slice) |
| `0xFFFC` | `HeapIndex(u32)` + gen | `JsObject` (ordinary, Array, Function, TypedArray, Promise, Host/DOM) |
| `0xFFFD` | `SymbolId(u32)` | `Symbol` |
| `0xFFFE` | `HeapIndex(u32)` + gen | `BigInt` |

*Note:* Using typed 32-bit `HeapIndex` handles inside arena slabs lets the entire GC, heap, and NaN-box decoder remain **100% safe Rust** (no raw pointer forging) while maintaining dense 8-byte value slots!

### 2.2 Generational Mark-Sweep GC & Weak DOM Wrappers
- **Young Generation (Nursery):** Fast bump-index slab; minor GC collects short-lived objects (closures, iterators, temporary arrays, DOM rects) in sub-millisecond pauses.
- **Old Generation:** Incremental tri-color mark-sweep with write barriers.
- **Weak DOM Wrapper Table (`WeakMap<NodeId, WeakHeapHandle>`):**
  - Wrapping a DOM node NEVER pins it permanently in memory.
  - A JS wrapper stays alive if referenced from JS **OR** if the DOM node has active JS event listeners/expando properties; detached unreachable nodes are collected automatically, eliminating memory leaks on long-lived SPA tabs.

---

## 3. Object Model: Shapes (Hidden Classes) & Inline Caches (IC)

Reference: Classic V8 Maps / SpiderMonkey Shapes / JSC Structures.

1. **Immutable Shape Transition Tree (`ShapeId(u32)`):**
   - Each `JsObject` stores `shape: ShapeId` and a flat dense `slots: Vec<JsValue>` (plus separate `elements: ElementsStorage` for dense integer-indexed arrays: `PackedInt32`, `PackedFloat64`, `PackedValues`, `SparseDict`).
   - Adding property `"x"` to an object with `Shape #4` transitions it in O(1) via `shape_transitions[(4, intern("x"))] -> Shape #9`, assigning fixed slot offset `slot = 2`.
   - Objects exceeding dictionary threshold (`> 64` dynamic delete/add transitions) automatically switch to dictionary mode to prevent shape explosion.
2. **Inline Caches (Monomorphic -> Polymorphic -> Megamorphic):**
   - Every `GetProp`, `SetProp`, and `CallMethod` bytecode site carries an IC slot index (`ic_idx: u32`).
   - **Monomorphic hit:** single `obj.shape == ic.cached_shape` check -> direct `obj.slots[ic.offset]` read/write (2 instructions in JIT!).
   - **Polymorphic (up to 4 shapes):** linear scan of 4 `(ShapeId, SlotOffset)` entries.
   - **Megamorphic:** falls back to shared global `(ShapeId, PropertyKey) -> SlotOffset` hash cache.

---

## 4. Compiler & Execution Tiers

### 4.1 Frontend (Phase 2)
- Full ECMAScript 2024 Lexer, Recursive-Descent + Pratt Parser, strict-mode & early-error validation, and Scope/Upvalue Analyzer.
- **Register-Based Bytecode IR:** compact 32-bit fixed-width instructions (like V8 Ignition / Lua 5.4 / JSC LLInt) minimizing dispatch count vs stack machines.
- Persistent **Bytecode Cache** stored in `rix-storage` keyed by `(source_sha256, bytecode_version)` for instant repeat loads.

### 4.2 Tier 1: Register Bytecode Interpreter (Phase 2)
- Zero-allocation register window on a reusable `Vec<JsValue>` call stack.
- Full support for ES Modules (static graph linking, dynamic `import()`, **Import Maps**, **top-level `await`**), Promises/async-await, Generators, Proxies, Reflect, WeakRef/FinalizationRegistry, TypedArrays, RegExp, and native Host/DOM callbacks.
- Interrupt/budget flag checked at loop back-edges for cooperative scheduling and infinite-loop protection.

### 4.3 Tier 2: Baseline Cranelift JIT (Phase 3)
- Triggered when function call count reaches `BASELINE_THRESHOLD = 32` or loop back-edge count reaches `OSR_THRESHOLD = 256` (with **On-Stack Replacement** directly into running hot loops).
- **Handles ALL JS value types** via NaN-boxed 64-bit registers (`i64` in Cranelift IR):
  - Inline fast paths for `Int32` arithmetic (with overflow check -> `f64`), `f64` arithmetic, boolean/null/undefined checks, branches, direct IC property loads/stores, dense array element loads/stores, and direct JS-to-JS calls.
  - Complex operations (prototype-chain walks, string concatenation, allocation, exceptions) call clean `extern "C"` runtime helpers **without discarding the compiled frame or copying a shadow register file**.
- Proper code-memory lifetime management: `CompiledCode` pages are freed cleanly via `munmap` when functions/realms are garbage-collected (zero `Box::into_raw` leaks).

### 4.4 Tier 3: Optimizing Speculative JIT (Phase 3)
- Triggered at `OPT_THRESHOLD = 1_000` calls using type feedback collected by the Baseline IC slots.
- Performs type specialization (unboxed `i32`/`f64` loops, hoisted shape guards, loop-invariant property loads, small-function inlining) with clean deoptimization maps back to bytecode PCs on guard failure.

---

## 5. WebAssembly Runtime (Phase 3)

- Embed a pure-Rust WebAssembly execution engine (`wasmtime` / Cranelift Wasm compiler) exposed through the standard `WebAssembly` JS namespace (`compile`, `instantiate`, `instantiateStreaming`, `Module`, `Instance`, `Memory`, `Table`, `Global`).
- Shares linear memory buffers cleanly with JS `ArrayBuffer` / `TypedArray` views.

---

## 6. Conformance & Verification Criteria

- **Test262 Runner (automated in CI/local harness):**
  - Track exact pass percentage across `language/` and `built-ins/` in a committed baseline manifest so regressions fail CI.
  - Phase 2 exit target: **> 85% Test262 language + core built-ins** passing cleanly on Bytecode VM.
  - Phase 3 exit target: **> 92% Test262** with identical results across Interpreter, Baseline JIT, and Optimizing JIT.
- **Benchmarks (`criterion`):**
  - Monomorphic property loop (1M iterations), array map/filter, JSON parse/stringify, recursive AST/tree manipulation (simulating Virtual DOM diffing) showing **>= 5x speedup** from Tier 1 to Tier 3.
- **Memory Stress Test:** Creating and detaching 100,000 DOM wrapper objects in a loop stabilizes heap memory after GC with zero wrapper leaks.
