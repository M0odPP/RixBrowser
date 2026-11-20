# Spec 06: Modern Web Platform APIs & GPU Media Pipeline (`rix-net`, `rix-js`, `rix-media`)

**Status:** Draft (Phases 2–5)  
**Governing Architecture:** [docs/ARCHITECTURE.md](../ARCHITECTURE.md)  
**Unsafe Policy:** `#![deny(unsafe_code)]` across `rix-net`, `rix-media`, and Web API bindings.

---

## 1. WHATWG Event Loop (Zero Polling)

Reference: [WHATWG Event Loops](https://html.spec.whatwg.org/multipage/webappapis.html#event-loops).

- Each Content Process runs a reactive `tokio` + local single-threaded executor per browsing-context group:
  1. **Task Queues** (prioritized: User Input > Rendering/VSync > Networking > Timers > Idle).
  2. **Microtask Checkpoint** (`Promise` jobs, `queueMicrotask`, `MutationObserver` delivery) drained strictly after every task and script callback.
  3. **Rendering Opportunity** driven exclusively by Compositor `BeginFrame` VSync signals (runs `ResizeObserver`, `IntersectionObserver`, `requestAnimationFrame`, Style, Layout, Paint).
- **Zero 1ms/8ms polling:** pending `fetch` / `WebSocket` / `setTimeout` wake the Content Process immediately via async channel notifications or exact `tokio::time::Sleep` deadlines.

---

## 2. Networking, Fetch, CORS & WebSocket (`rix-net` + Network Process)

1. **Fetch API & Full CORS Enforcement** ([Fetch Standard](https://fetch.spec.whatwg.org/)):
   - Enforce `mode` (`cors`, `no-cors`, `same-origin`, `navigate`), `credentials` (`omit`, `same-origin`, `include`), `redirect` (`follow`, `error`, `manual`), and `AbortSignal`.
   - Automatic **CORS preflight (`OPTIONS`)** for non-simple methods/headers with preflight cache (`Access-Control-Max-Age`).
   - Validate `Access-Control-Allow-Origin`, `Access-Control-Allow-Credentials`, `Access-Control-Expose-Headers`, and produce opaque responses for `no-cors`.
   - Full `ReadableStream` / `WritableStream` / `TransformStream` streaming request & response bodies (no whole-body buffering required).
2. **WebSocket (RFC 6455):**
   - Implemented cleanly in `rix-net` over existing `tokio` + `rustls` (supporting `ws://` and `wss://`, subprotocols, binary `ArrayBuffer`/`Blob` and UTF-8 text frames, ping/pong keepalive, clean close codes).
3. **HTML Forms (GET & POST):**
   - Full support for `application/x-www-form-urlencoded`, `multipart/form-data` (including file uploads), and `text/plain` across both `GET` and `POST` navigations.

---

## 3. Core & Advanced Web APIs

### Phase 2 (Core Interactive Web)
- **DOM & Events:** Full bubbling/capturing/composed Shadow DOM event dispatch, `EventTarget`, `CustomEvent`, `PointerEvent`, `MouseEvent`, `KeyboardEvent`, `InputEvent`, `WheelEvent`, `ClipboardEvent`, `MutationObserver`, `ResizeObserver`, `IntersectionObserver`, `Range`/`Selection`.
- **Storage & Navigation:** `URL` / `URLSearchParams`, `History` (`pushState`/`replaceState`/`popstate`), `Location`, `localStorage` / `sessionStorage` (scoped by origin in `rix-storage`), `Blob`/`File`/`FileReader`, `TextEncoder`/`TextDecoder`, `structuredClone`, `Performance` & Navigation/Resource Timing.
- **GPU-Backed `Canvas2D`:**
  - Implemented over `vello::Scene` -> off-screen `wgpu::Texture` (`ExternalSurface`).
  - Full 2D drawing state stack (`save`/`restore`, affine transforms `translate`/`rotate`/`scale`/`setTransform`), paths (`beginPath`, `moveTo`, `lineTo`, `quadraticCurveTo`, `bezierCurveTo`, `arc`, `arcTo`, `rect`, `roundRect`, `closePath`, `fill`, `stroke`, `clip`), styles (`fillStyle`/`strokeStyle` colors, linear/radial gradients, patterns), `fillText`/`strokeText`/`measureText` (via `rix-text`), `drawImage`, `getImageData`/`putImageData`, `toDataURL`/`toBlob`.

### Phase 4 (Full Modern Web Platform)
- **OOPIF (`<iframe>`):** Real navigation of external `src` URLs in isolated browsing contexts (cross-origin iframes run in their own Content Process and composite via `DisplayItem::ExternalSurface`), with `window.postMessage` + `MessageChannel` across process boundaries.
- **Workers & Service Workers:** Dedicated `Worker`, `SharedWorker`, and `ServiceWorker` (fetch interception + `CacheStorage` backed by `rix-storage`), plus `BroadcastChannel` and `EventSource` (SSE).
- **IndexedDB:** Async transactional object stores, key ranges, cursors, and indices backed by `redb` inside the Storage Process.
- **Internationalization (`Intl`) via `icu4x`:** Genuine pure-Rust CLDR/Unicode formatting (`DateTimeFormat`, `NumberFormat`, `Collator`, `PluralRules`, `RelativeTimeFormat`, `Segmenter`, `ListFormat`, timezones)—replacing legacy JS stubs completely.
- **WebCrypto (`SubtleCrypto`):** Pure-Rust cryptographic primitives (`SHA-256/384/512`, `HMAC`, `AES-GCM/CBC/CTR`, `HKDF`, `PBKDF2`, `Ed25519`, `ECDSA`, `ECDH`, `RSA-OAEP/PSS`) via audited RustCrypto/`ring` crates.
- **WebGPU & WebGL:** Native `navigator.gpu` backed directly by `wgpu`, plus a `WebGL2` compatibility layer mapped onto `wgpu` rendering directly to `ExternalSurface`.

---

## 4. GPU Media Pipeline (`rix-media`, Phase 5)

### 4.1 Supported Containers & Pure-Rust Codecs
- **Demuxers:** Fragmented & standard MP4 (`isobmff`), WebM / Matroska (`ebml`), Ogg, WAV, MP3.
- **Video Decoders (Pure Rust, Full Inter-Frame P/B-Frame Support):**
  - **AV1:** `rav1d` (safe/audited Rust port of dav1d) — covers modern YouTube/web HD/4K streams.
  - **H.264 (AVC):** Full Baseline/Main/High profile decoding with proper DPB frame reordering.
  - **VP8 & VP9:** Complete keyframe + inter-frame decoding (explicitly eliminating the legacy keyframe-only drop behavior).
- **Audio Decoders (Pure Rust):** Opus, AAC-LC, Vorbis, MP3, FLAC, PCM (`symphonia` + specialized decoders).

### 4.2 Zero-Copy GPU Video Presentation & Audio Clock
1. Media decoding runs on dedicated worker threads inside the Content/Media pipeline—never blocking JS or UI.
2. Decoded video frames stay in **planar YUV420 / NV12** format and are written directly into a triple-buffered shared memory slot (`Spec 01`).
3. GPU/Compositor uploads `Y`, `U`, `V` planes to `wgpu::Texture`s (`R8Unorm`) and renders the `DisplayItem::ExternalSurface` via a lightweight **BT.709 / BT.601 YUV->RGBA fragment shader**.
   - **Zero CPU YUV->RGBA loops.**
   - **Zero DOM mutations or `DisplayList` rebuilds per video frame.**
4. **Master Audio Clock & WebAudio:** Audio output (`cpal` / PipeWire / PulseAudio) drives the master playback clock (`MediaClock`) with A/V drift tolerance `< 25 ms`, sharing the mixing graph with `AudioContext` (WebAudio nodes: `GainNode`, `BiquadFilterNode`, `AnalyserNode`, `AudioBufferSourceNode`, `AudioWorklet`).
5. **Media Source Extensions (MSE):** Full `MediaSource`, `SourceBuffer`, buffered range merging, quota eviction, and seamless codec/resolution switching without any site-specific protocol hardcodes.

---

## 5. Verification & Acceptance Criteria

- **WPT Subsets:** Pass WPT `fetch/api`, `cors`, `websockets`, `html/semantics/embedded-content/the-canvas-element`, `IndexedDB`, and `media-source` suites incrementally across Phases 2–5.
- **Media Performance:** 1080p60 AV1/H.264 playback sustains 60 FPS presentation with zero DisplayList rebuilds and < 15 ms A/V sync jitter.
