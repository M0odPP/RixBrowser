# RixBrowser Architecture Blueprint

This document is the master architectural specification for RixBrowser v0.2+. Every subsystem specification under [`docs/specs/`](specs/) elaborates on a concrete boundary defined here.

---

## 1. Design Goals & Hard Performance Budgets

RixBrowser is designed to reach modern browser responsiveness in **pure Rust**:

| Metric | Target Budget | Architectural Mechanism |
| :--- | :--- | :--- |
| Idle CPU usage (static tabs open) | **0% (0 Hz wakeups)** | Event-driven `winit` + async IPC (`ControlFlow::Wait`), zero fixed-interval polling |
| Compositor scroll / zoom / transform | **< 4 ms / frame (60–144 FPS)** | Async Pan/Zoom (APZ) on GPU compositor via layer transform matrix + tile cache |
| Full content frame (Style + Layout + Paint) | **< 16.6 ms (typical page)** | Typed CSS cascade, Bloom filter, scoped dirty-bit invalidation, incremental Fragment Tree |
| UI thread input latency | **< 2 ms** | Non-blocking input routing in Browser Process; zero `block_on` on UI thread |
| IPC scene transfer | **< 1 ms (binary zero-copy)** | Compact binary DisplayList & shared memory buffers; DOM never serialized across IPC |
| Fault isolation | **Per-site crash containment** | Site-isolated Content Processes; tab crashes render a recoverable crash screen |

---

## 2. Four-Process Architecture

```text
+-------------------------------------------------------------------------+
|                       1. BROWSER PROCESS (Parent)                       |
|  - OS window management (winit), input routing, clipboard, menus        |
|  - Tab graph, session history, bookmarks, downloads, permissions        |
|  - Privileged Chrome UI (rix://chrome/*) rendered in isolated UI doc    |
|  - Process supervisor (spawns/restarts Network, GPU, Content processes) |
+-------------+-------------------------+------------------------+--------+
              | UDS Control IPC         | UDS Control IPC        |
              v                         v                        v
+---------------------------+  +-----------------------+  +-------------------------+
| 2. NETWORK & STORAGE PROC |  | 3. CONTENT PROCESSES  |  | 4. GPU / COMPOSITOR     |
|                           |  |  (1 per Site / Origin)|  |    PROCESS (or thread)  |
| - rix-net (H1/H2/H3/QUIC) |  |                       |  |                         |
| - DNS Happy Eyeballs      |=>| - Streaming html5ever |  | - wgpu device & swap    |
| - rustls TLS & ALPN       |  | - Preload Scanner     |  | - Vello 2D vector GPU   |
| - RFC 9111 HTTP Cache     |  | - Arena DOM & Shadow  |=>| - Retained Layer Tree   |
| - Strict CORS / Cookies   |  | - Typed CSS Cascade   |  | - Viewport Culling      |
| - WebSocket client        |  | - Box/Fragment Layout |  | - APZ Scroll & Zoom     |
| - rix-storage (redb)      |  | - Custom JS VM + JIT  |  | - VSync / rAF clock     |
|   Cookies, LS, IDB, Cache |  | - Wasm runtime        |  | - Video YUV->RGB shader |
+---------------------------+  | - DisplayList builder |  | - WebGPU / WebGL surface|
                               +-----------------------+  +-------------------------+
```

### Strict Ownership Boundaries

1. **Browser Process owns** OS windows, tabs, navigation state, process supervision, and the Chrome UI document (`rix://chrome/*`). It **never** parses untrusted HTML, holds a page DOM, runs page JS, or computes page layout.
2. **Network & Storage Process owns** sockets, TLS sessions, connection pools, DNS cache, HTTP disk cache, cookie jar, CORS enforcement, WebSockets, and `redb` persistence. Content Processes have **no direct socket or filesystem access** once sandboxed.
3. **Content Process owns** a site's browsing contexts (documents, DOM trees, CSSOM, style cascade, layout trees, JS/Wasm heap, event loop, and DisplayList generation). It communicates with Network via streaming byte channels and with GPU/Compositor via binary DisplayList transactions and shared memory image/video buffers.
4. **GPU / Compositor Process owns** the `wgpu` instance/surface, Vello renderer, retained layer tree per frame, decoded image/canvas/video GPU textures, VSync presentation, and Async Pan/Zoom (APZ) hit-testing and scrolling.

---

## 3. Crate Dependency Graph (Strict Acyclic DAG)

Circular dependencies are prohibited. Lower-level crates must never import higher-level orchestration crates (e.g., `rix-net` must never reference `rix-chrome`).

```text
Level 0 (Foundation):
  rix-core

Level 1 (Data & Low-Level Primitives):
  rix-storage  --> rix-core
  rix-dom      --> rix-core
  rix-text     --> rix-core
  rix-ipc      --> rix-core
  rix-jit      --> rix-core  (isolated unsafe for executable code pages)

Level 2 (Parsing, Networking, Styling, Media):
  rix-net      --> rix-core, rix-storage
  rix-html     --> rix-core, rix-dom
  rix-css      --> rix-core, rix-dom
  rix-media    --> rix-core, rix-ipc

Level 3 (Layout, Rendering, Scripting):
  rix-layout   --> rix-core, rix-dom, rix-css, rix-text
  rix-render   --> rix-core, rix-text, rix-ipc
  rix-js       --> rix-core, rix-dom, rix-css, rix-jit, rix-ipc

Level 4 (Process Runtimes & Shell):
  rix-content  --> rix-html, rix-dom, rix-css, rix-layout, rix-js, rix-render, rix-ipc, rix-media
  rix-chrome   --> rix-core, rix-net, rix-storage, rix-render, rix-ipc (Chrome UI + Tab Manager)

Level 5 (Entrypoint Binary):
  rix-app      --> rix-chrome, rix-content, rix-render, rix-net, rix-storage, rix-ipc
```

*(During Phase 1 single-binary bring-up, `rix-app` spawns itself with `--process=content`, `--process=network`, or `--process=gpu` sub-modes, keeping deployment to one binary while preserving strict OS process isolation.)*

---

## 4. End-to-End Frame Lifecycle

### 4.1 Navigation & First Paint (No Duplicate Parsing)
1. User enters URL in Browser Process -> Browser assigns/reuses a Content Process for the origin and tells Network Process to start a streaming navigation fetch.
2. As the first response bytes arrive, Network streams chunks directly to the Content Process over binary IPC.
3. Content Process feeds bytes incrementally into `html5ever` (`rix-html`) **and** runs a lightweight **Speculative Preload Scanner** that immediately requests `<link rel="stylesheet">`, `<script>`, `@font-face`, and hero `<img>` resources in parallel.
4. Once render-blocking CSS is ready, Content Process runs **Typed Style Cascade -> Box/Fragment Layout -> Paint (Binary DisplayList)** and commits a `FrameTransaction` to the GPU/Compositor Process.
5. Compositor applies viewport culling, caches static tiles/layers, and submits the frame to `wgpu` on the next VSync.

### 4.2 Scrolling & Compositor Animations (APZ Fast Path)
1. User scrolls mouse wheel or trackpad -> Browser Process forwards scroll delta directly to GPU/Compositor.
2. Compositor updates the scroll node's offset in the Retained Layer Tree and re-presents immediately using cached layer scenes + `Affine::translate` + viewport culling.
3. Content Process is notified asynchronously (`ScrollOffsetChanged`) solely to fire JS `scroll` events and update `IntersectionObserver` / lazy-loading thresholds. Neither DOM, CSS, Layout, nor DisplayList rebuilding blocks scrolling.

### 4.3 DOM Mutation / JS Animation Frame
1. Compositor emits a VSync `BeginFrame` signal **only** when a Content Process has explicitly requested a frame (`requestAnimationFrame` or dirty DOM/style).
2. Content Process runs `rAF` callbacks -> drains microtasks -> computes scoped dirty-subtree style invalidation -> incremental layout reuse -> builds updated binary DisplayList -> submits `FrameTransaction`.

---

## 5. Subsystem Specifications

| # | Document | Covers |
| :--- | :--- | :--- |
| 01 | [01-ipc-and-processes.md](specs/01-ipc-and-processes.md) | Binary IPC, UDS framing, shared memory rings, Site Isolation, Linux sandbox, crash recovery |
| 02 | [02-compositor-and-render.md](specs/02-compositor-and-render.md) | `wgpu` + `Vello` compositor, Retained Layer Tree, DisplayList, APZ scroll, culling, WebGPU/WebGL |
| 03 | [03-dom-and-html.md](specs/03-dom-and-html.md) | Streaming `html5ever`, Preload Scanner, generational DOM, Shadow DOM, scoped style invalidation |
| 04 | [04-css-and-layout.md](specs/04-css-and-layout.md) | Typed CSS properties, Stylo-style Rule Tree + Bloom filter, `@font-face`, Box/Fragment Tree + Taffy |
| 05 | [05-js-vm-and-jit.md](specs/05-js-vm-and-jit.md) | Custom Pure-Rust JS VM (NaN-boxing, GC, Shapes/IC), Tiered Cranelift JIT, Test262, Wasm |
| 06 | [06-web-platform-and-media.md](specs/06-web-platform-and-media.md) | Event Loop, Fetch+CORS, WebSocket, OOPIF, Workers, IDB, `icu4x`, Canvas2D, MSE & GPU video |
| 07 | [07-chrome-ui-and-devtools.md](specs/07-chrome-ui-and-devtools.md) | Isolated Chrome UI (`unslop` design system), navigation UX, and built-in `F12` Developer HUD |
