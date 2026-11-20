# RixBrowser

<div align="center">

**A modern, multi-process web browser engine built from first principles in pure Rust.**

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Rust: 1.88+](https://img.shields.io/badge/Rust-1.88%2B-orange.svg)](https://www.rust-lang.org)
[![Platform: Linux](https://img.shields.io/badge/Platform-Linux-brightgreen.svg)]()
[![Status: Phase 0 Foundation](https://img.shields.io/badge/Status-Phase%200%20Foundation-yellow.svg)]()

</div>

---

RixBrowser is a clean-slate, pure-Rust web browser engine designed for high frame-rate rendering, strict process isolation, and genuine web standards compliance—without bundling C/C++ browser engines (Blink, Gecko, WebKit, V8, or FFmpeg).

## Architecture Overview

RixBrowser uses a **4-process architecture** with site isolation, binary IPC, and an independent GPU compositor:

```text
+-------------------------------------------------------------------------+
|                       BROWSER PROCESS (Parent / UI)                     |
|   winit (windows, input) • tabs • navigation • history • Chrome UI      |
|   Owns window lifecycle & tab routing. Never stores page DOM or layout. |
+--------+--------------------------+----------------------------+--------+
         | Binary IPC (UDS)         | Binary IPC (UDS)           | UI cmds
         v                          v                            v
+-------------------+   +----------------------------+   +--------------------------+
| NETWORK & STORAGE |   | CONTENT PROCESS (per Site) |   | GPU / COMPOSITOR PROCESS |
|     PROCESS       |   |  (sandboxed, 1 per origin) |   |   (wgpu + Vello layers)  |
|                   |   |                            |   |                          |
| • rix-net (H1/2/3)|==>| • Streaming HTML5 + Preload|   | • Retained Layer Tree    |
| • CORS & Cookies  |   | • Arena DOM + Shadow DOM   |   | • Tile Cache + Culling   |
| • WebSocket       |   | • Typed CSS (Stylo-style)  |==>| • Async Pan/Zoom (APZ)   |
| • rix-storage     |   | • Box/Fragment Layout      |   | • VSync + rAF scheduler  |
|   (redb, IDB,     |   |   (+ Taffy for Flex/Grid)  |   | • GPU Video Layers       |
|   Cache API)      |   | • Custom JS VM + JIT + Wasm|   |   (wgpu YUV->RGBA shader)|
+-------------------+   | • Paint -> Binary DispList |   | • WebGPU / WebGL surface |
                        +----------------------------+   +--------------------------+
                           ^        ^
                           |        | Binary DisplayList + Shared Memory / Textures
                     (No JSON on hot paths. DOM never leaves the Content Process.)
```

Read the full architectural blueprint in [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) and component specifications under [docs/specs/](docs/specs/).

## Engineering Guarantees

- **Pure Rust Stack:** No V8, SpiderMonkey, Node.js, or FFmpeg. All core crates enforce `#![deny(unsafe_code)]`; `unsafe` is permitted only in explicitly allowlisted low-level modules (JIT executable memory allocation, shared memory mapping, OS sandboxing) under `#![deny(unsafe_op_in_unsafe_fn)]` with mandatory `// SAFETY:` proofs.
- **Zero Site-Specific Hacks:** No hardcoded domain logic, fake consent cookies, or per-site element workarounds. Every fix targets the underlying W3C, WHATWG, IETF, or ECMA standard.
- **Zero Polling Loops:** Reactive event-driven scheduling only (VSync, IPC readiness, OS input wakeups).
- **Crafted Browser Chrome (`unslop`):** The browser UI (`rix://chrome/*`) runs isolated in the Parent Process (like Firefox) and follows the [unslop](https://github.com/heydamir/unslop) design methodology (CC BY 4.0, Damir Sokolovsky) installed in `.agents/skills/unslop/`.
- **Strict PR Guardrails:** Automated CI enforces formatting, zero Clippy warnings, unit tests, safety/hygiene checks, and a hard PR size cap so changes remain focused and reviewable.

## Workspace Crates

### Active Foundation Crates (Phase 0)

| Crate | Responsibility |
| :--- | :--- |
| [`crates/rix-core`](crates/rix-core) | Core types, `RixUrl`, string interning, profile config, logging, errors. |
| [`crates/rix-storage`](crates/rix-storage) | Transactional storage on `redb`, OS-keychain cookie encryption, session/history/storage tables. |
| [`crates/rix-net`](crates/rix-net) | Async HTTP/1.1, HTTP/2, HTTP/3 (QUIC), Happy Eyeballs DNS, `rustls`, RFC 9111 HTTP cache, cookies, MIME sniffing. |
| [`crates/rix-dom`](crates/rix-dom) | Arena-backed HTML5/SVG DOM tree, Shadow DOM, scoped dirty-state tracking, selector AST. |
| [`crates/rix-html`](crates/rix-html) | Spec-compliant HTML5 tokenizer and tree builder powered by `html5ever`. |
| [`crates/rix-text`](crates/rix-text) | Font discovery, Fontconfig integration, HarfBuzz (`rustybuzz`) shaping, bidi, and line breaking. |

### Planned Crates (Phases 1–5)

| Crate | Phase | Specification |
| :--- | :--- | :--- |
| `rix-ipc` | Phase 1 | [01-ipc-and-processes.md](docs/specs/01-ipc-and-processes.md) |
| `rix-render` | Phase 1 | [02-compositor-and-render.md](docs/specs/02-compositor-and-render.md) |
| `rix-css` | Phase 1 | [04-css-and-layout.md](docs/specs/04-css-and-layout.md) |
| `rix-layout` | Phase 1 | [04-css-and-layout.md](docs/specs/04-css-and-layout.md) |
| `rix-chrome` & `rix-app` | Phase 1 | [07-chrome-ui-and-devtools.md](docs/specs/07-chrome-ui-and-devtools.md) |
| `rix-js` | Phase 2 | [05-js-vm-and-jit.md](docs/specs/05-js-vm-and-jit.md) |
| `rix-jit` | Phase 3 | [05-js-vm-and-jit.md](docs/specs/05-js-vm-and-jit.md) |
| `rix-media` | Phase 5 | [06-web-platform-and-media.md](docs/specs/06-web-platform-and-media.md) |

## Roadmap

Development is organized into vertical phases so the browser becomes **usable for manual testing as early as Phase 1**:

- **Phase 1 — Usable Fast HTML/CSS Browser:** Reactive window, GPU compositor with APZ scroll, binary IPC, streaming HTML5, typed CSS cascade, Box/Fragment layout (+ Taffy for Flex/Grid), `@font-face`, crafted Chrome UI, and built-in `F12` Developer HUD.
- **Phase 2 — Custom JS Bytecode VM & Core Web APIs:** 64-bit NaN-boxed VM, generational GC, Shapes/Inline Caches, Weak DOM bridge, `fetch` + CORS, WebSocket, Observers, GPU `Canvas2D`.
- **Phase 3 — Tiered Cranelift JIT & WebAssembly:** Baseline + Optimizing SSA JIT covering all JS types (`i31`, `f64`, strings, objects, arrays, closures, OSR) plus Wasm execution.
- **Phase 4 — Modern Web Platform:** Out-of-process `<iframe>` (OOPIF), Web/Service Workers, IndexedDB, SubtleCrypto, `icu4x` `Intl`, CSS animations/transforms/filters/sticky, WebGPU & WebGL over `wgpu`.
- **Phase 5 — GPU Media Pipeline:** MSE, pure-Rust H.264 / VP8 / VP9 / AV1 (`rav1d`) / Opus / AAC decoders, `wgpu` YUV-to-RGBA shader compositing, and WebAudio.

Track all live tasks via GitHub Issues filtered by `phase:*` and `area:*` labels.

## Quick Start

```bash
# Check formatting, lints, and run the full test suite
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for development workflow, PR size limits, and specification guidelines.

## Acknowledgments

- Browser Chrome UI craft guidelines are powered by [unslop](https://github.com/heydamir/unslop) by Damir Sokolovsky ([@heydamir](https://x.com/heydamir)), licensed under CC BY 4.0.

## License

MIT — see [LICENSE](LICENSE).
