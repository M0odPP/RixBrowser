# Spec 01: Binary IPC, Process Model & Sandboxing (`rix-ipc`)

**Status:** Draft (Phase 1 Target)  
**Governing Architecture:** [docs/ARCHITECTURE.md](../ARCHITECTURE.md)  
**Unsafe Policy:** Isolated `unsafe` allowed strictly inside `rix-ipc::shmem` and `rix-ipc::sandbox` under `#![deny(unsafe_op_in_unsafe_fn)]` with explicit `// SAFETY:` comments. All framing, routing, and serialization modules must remain safe Rust.

---

## 1. Objectives & Non-Goals

### Objectives
- Replace legacy stdio JSON DOM snapshotting with a **low-latency binary IPC layer** over Unix Domain Sockets (`SEQPACKET` / length-prefixed `STREAM`) paired with **anonymous shared memory (`memfd_create` + `mmap`)** for large binary payloads (DisplayLists, decoded images, font data, YUV video frames).
- Enforce **Site Isolation**: each top-level site (eTLD+1 + scheme, or `opaque` origin) runs in its own sandboxed Content Process.
- Provide deterministic **process lifecycle management** and **tab crash recovery** so a crashing or hanging site never takes down the Browser Process or sibling tabs.

### Non-Goals
- Never serialize DOM trees or raw text HTML diffs across IPC on every mutation.
- Never poll sockets with sleep timers; all IPC endpoints integrate directly into `tokio`/epoll and `winit::EventLoopProxy` wakeups.

---

## 2. Wire Framing & Serialization

### 2.1 Control Channel Framing (UDS)
Every control message on a Unix Domain Socket uses a fixed 8-byte header followed by a `postcard`-encoded payload:

```text
+--------------------+--------------------+----------------------------------+
| payload_len: u32   | flags_and_ver: u32 | postcard payload (payload_len B) |
| (little-endian)    | ver[0..8], flags   |                                  |
+--------------------+--------------------+----------------------------------+
```

- **Max control frame size:** `4 MiB`. Payloads exceeding `64 KiB` (DisplayLists, decoded bitmaps, font blobs, network bodies) MUST be transferred via a `ShmemHandle` passed using `SCM_RIGHTS` FD passing or an index into a pre-mapped ring buffer.
- **Version check:** `PROTOCOL_VERSION = 1`. Version mismatch immediately terminates the child connection with a typed `IpcError::ProtocolMismatch`.
- **Zero-allocation receive path:** Reuse a per-connection `BytesMut` buffer; reject frames exceeding `MAX_CONTROL_FRAME_BYTES` before allocating.

### 2.2 Shared Memory Ring & Arena (`rix-ipc::shmem`)
- Backed by Linux `memfd_create("rix-shmem", MFD_CLOEXEC | MFD_ALLOW_SEALING)`.
- Supports two patterns:
  1. **Immutable Sealed Blobs** (`F_SEAL_WRITE | F_SEAL_SHRINK | F_SEAL_GROW`): used for transferring decoded images, font files, and immutable `DisplayList` snapshots to the GPU/Compositor safely without TOCTOU races.
  2. **Lock-Free Triple-Buffered Slot** (`AtomicU8` state machine: `Idle | Writing | Ready | Reading`): used for streaming YUV video planes and Canvas2D surfaces from producer to GPU uploader.

---

## 3. Process Topology & Message Schemas

### 3.1 Process Roles
`rix-app` ships as a single executable spawned with explicit sub-process arguments:
- Default (no flag): **Browser Process** (UI, tab graph, supervisor)
- `--process=network`: **Network & Storage Process**
- `--process=gpu`: **GPU / Compositor Process** (Phase 1 may run GPU on a dedicated OS thread inside Browser Process behind the exact same binary `CompositorChannel` trait, migrating out-of-process without changing callers)
- `--process=content --site-id=<u64>`: **Content Process**

### 3.2 Browser <-> Content Messages

```rust
pub struct TabId(pub u64);
pub struct FrameId(pub u64);
pub struct NavigationId(pub u64);
pub struct PipelineId(pub u64);

pub enum BrowserToContent {
    CreatePipeline { pipeline: PipelineId, tab: TabId, viewport: ViewportMetrics },
    DestroyPipeline { pipeline: PipelineId },
    CommitNavigation {
        pipeline: PipelineId,
        nav_id: NavigationId,
        final_url: String,
        mime_type: String,
        stream_id: u64,
    },
    ViewportChanged { pipeline: PipelineId, viewport: ViewportMetrics },
    InputEvent { pipeline: PipelineId, event: RoutedInputEvent },
    ScrollOffsetSync { pipeline: PipelineId, node_id: u64, x: f32, y: f32 },
    BeginFrame { pipeline: PipelineId, frame_seq: u64, vsync_ns: u64 },
    ThemeChanged { dark: bool },
    Shutdown,
}

pub enum ContentToBrowser {
    PipelineReady { pipeline: PipelineId },
    TitleChanged { pipeline: PipelineId, title: String },
    FaviconChanged { pipeline: PipelineId, url: String },
    LoadStateChanged { pipeline: PipelineId, state: LoadState },
    NavigationRequested {
        pipeline: PipelineId,
        url: String,
        method: HttpMethodKind,
        form_body: Option<Vec<u8>>,
        content_type: Option<String>,
    },
    CursorChanged { pipeline: PipelineId, cursor: CursorIcon },
    FocusChanged { pipeline: PipelineId, editable: bool, selection_text: Option<String> },
    RequestAnimationFrame { pipeline: PipelineId },
    TelemetrySample { pipeline: PipelineId, sample: FrameTelemetry },
    MissingFeatureReported { pipeline: PipelineId, kind: MissingFeatureKind, name: String },
}
```

### 3.3 Content <-> Network Streaming Messages

```rust
pub enum ContentToNetwork {
    FetchStart {
        request_id: u64,
        url: String,
        method: String,
        headers: Vec<(String, String)>,
        body: Option<ShmemRef>,
        mode: FetchMode,
        credentials: CredentialsMode,
        initiator_origin: String,
    },
    FetchCancel { request_id: u64 },
    WebSocketOpen { ws_id: u64, url: String, protocols: Vec<String>, origin: String },
    WebSocketSend { ws_id: u64, opcode: u8, payload: Vec<u8> },
    WebSocketClose { ws_id: u64, code: u16, reason: String },
}

pub enum NetworkToContent {
    ResponseHead {
        request_id: u64,
        status: u16,
        final_url: String,
        headers: Vec<(String, String)>,
        redirected: bool,
    },
    ResponseChunk { request_id: u64, bytes: Vec<u8> },
    ResponseEnd { request_id: u64, timing: NavigationTiming },
    ResponseError { request_id: u64, error: String },
    WebSocketOpened { ws_id: u64, protocol: String },
    WebSocketMessage { ws_id: u64, opcode: u8, payload: Vec<u8> },
    WebSocketClosed { ws_id: u64, code: u16, reason: String },
}
```

### 3.4 Content -> Compositor Frame Transactions
Defined in detail in [02-compositor-and-render.md](02-compositor-and-render.md):
- `SubmitFrame { pipeline: PipelineId, epoch: u64, display_list: ShmemRef, scroll_tree: ScrollTreeUpdate, stats: PaintStats }`
- `RegisterImage { image_id: ImageId, width: u32, height: u32, format: PixelFormat, shmem: ShmemRef }`
- `ReleaseImage { image_id: ImageId }`

---

## 4. Crash Recovery & Hang Watchdog

- Browser Process monitors child process exit status asynchronously (`tokio::process::Child::wait()`).
- If a Content Process terminates unexpectedly (`SIGSEGV`, `SIGABRT`, OOM kill, or broken IPC pipe):
  1. Browser marks all affected `PipelineId`s as `Crashed { exit_code, signal }`.
  2. Compositor drops that pipeline's layers and textures to reclaim GPU memory.
  3. Browser displays the built-in `rix://chrome/crashed.html` recovery view for that tab with a one-click **Reload** button (sibling tabs remain untouched).
- **Heartbeat:** Optional non-blocking ping every 5 s while user input is queued; if unanswered after 10 s, Browser surfaces an **"Unresponsive Page (Wait / Terminate)"** banner in Chrome UI rather than silently killing the tab.

---

## 5. Linux OS Sandboxing (Phase 1 -> Phase 4 Hardening)

Before executing untrusted HTML/CSS/JS, Content Processes initialize a sandbox boundary:
1. Drop access to the host filesystem (`pivot_root`/empty mount namespace or Landlock LSM on Linux 5.13+ allowing read-only access only to pre-opened font FDs / shared memory).
2. Deny direct `socket`/`connect`/`bind` syscalls via `seccomp-bpf` filter (all network access flows through pre-connected Network IPC FDs).
3. Enforce `RLIMIT_AS` / memory guardrails per Content Process so a runaway page triggers OOM inside the isolated tab rather than freezing the OS.

---

## 6. Verification & Acceptance Criteria

- **Unit tests:** Round-trip fuzz/property tests for framing, oversized frame rejection, EOF handling, sealed `memfd` read-only enforcement, and lock-free triple-buffer handoff under multi-threaded stress.
- **Integration tests:** Spawn real `--process=content` child process, send 1,000 binary frames + shared-memory blob, verify round-trip p99 latency < 0.5 ms, then kill child with `SIGKILL` and verify Browser supervisor cleanly transitions tab state to `Crashed` with zero panics or FD leaks.
