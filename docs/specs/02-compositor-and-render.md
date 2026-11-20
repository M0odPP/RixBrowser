# Spec 02: GPU Compositor, Retained Layer Tree & Rendering (`rix-render`)

**Status:** Draft (Phase 1 Target)  
**Governing Architecture:** [docs/ARCHITECTURE.md](../ARCHITECTURE.md)  
**Unsafe Policy:** `#![deny(unsafe_code)]`. All GPU calls go through safe `wgpu` and `vello` APIs.

---

## 1. Objectives & Core Invariants

1. **Zero DisplayList Cloning on Scroll:** Scrolling, pinch-zooming, and compositor-driven animations (`transform`, `opacity`) MUST be applied strictly as GPU layer transforms in the Compositor. The page `DisplayList` is NOT rebuilt or cloned when scroll offsets change.
2. **Event-Driven VSync Presentation:** Present with `PresentMode::AutoVsync` (or `Fifo` fallback). Never run `AutoNoVsync` in interactive mode; never spin in a busy loop when no layer is dirty.
3. **Viewport Culling:** Commands outside the visible clip + overscan margin (`128 px`) are skipped before encoding into `vello::Scene`.
4. **Correct Stacking Contexts:** Replace flat `items.sort_by_key(|i| i.z_index)` with a true hierarchical **Stacking Context Tree** so `PushClip`/`PopClip`, `PushLayer` (`opacity`/`transform`/`blend`/`filter`), and `z-index` boundaries can never interleave or corrupt each other.
5. **Responsive Anti-Aliasing:** Default to `vello::AaConfig::Area` (analytic area AA) rather than `Msaa16`, keeping GPU compute cost low across HiDPI displays while preserving sharp text and vector curves.

---

## 2. Retained Layer Tree & Scroll Tree

Every pipeline (`Chrome UI`, active `Tab`, or `OOPIF`) submits a compact binary scene composed of two trees:

### 2.1 Spatial & Scroll Tree (`SpatialTree`)
Each scrollable container (root viewport, `overflow: auto/scroll`, `position: fixed`, `position: sticky`, or CSS `transform` node) gets a `SpatialNodeId(u32)`:

```rust
pub struct SpatialNode {
    pub id: SpatialNodeId,
    pub parent: Option<SpatialNodeId>,
    pub kind: SpatialNodeKind,
}

pub enum SpatialNodeKind {
    Root,
    ScrollFrame {
        content_rect: Rect,
        viewport_rect: Rect,
        external_scroll_id: ScrollId,
        current_offset: Vec2,
    },
    StickyFrame {
        bounds: Rect,
        margins: StickyMargins,
        scroll_parent: SpatialNodeId,
    },
    Transform {
        matrix: [f32; 6], // 2D affine (upgradable to 4x4 for 3D transforms in Phase 4)
        origin: Point,
        animation_id: Option<AnimationId>,
    },
}
```

When a wheel/trackpad event hits a `ScrollFrame`:
- Compositor clamps `current_offset` directly within `content_rect - viewport_rect` (plus smooth momentum physics if enabled).
- Compositor marks the frame dirty and composites on the next VSync immediately.
- Content Process receives an async `ScrollOffsetSync` notification at most once per VSync.

### 2.2 Hierarchical DisplayList (`DisplayList`)
Instead of a flat vector sorted globally by `z-index`, the Content Process emits a **pre-ordered stacking-context tree** serialized into shared memory:

```rust
pub enum DisplayItem {
    PushStackingContext {
        spatial: SpatialNodeId,
        bounds: Rect,
        opacity: f32,           // Layer clip uses `bounds` (NEVER -1e6..1e6!)
        mix_blend: BlendMode,
        filters: SmallVec<[FilterOp; 2]>,
    },
    PopStackingContext,
    PushClip {
        spatial: SpatialNodeId,
        rect: Rect,
        radii: [f32; 4],
    },
    PopClip,
    Rect {
        bounds: Rect,
        color: Rgba8,
        radii: [f32; 4],
    },
    LinearGradient {
        bounds: Rect,
        start: Point,
        end: Point,
        stops: SmallVec<[ColorStop; 4]>,
        radii: [f32; 4],
    },
    Border {
        bounds: Rect,
        widths: [f32; 4],
        colors: [Rgba8; 4],
        radii: [f32; 4],
    },
    BoxShadow {
        bounds: Rect,
        offset: Vec2,
        blur_radius: f32,
        spread_radius: f32,
        color: Rgba8,
        radii: [f32; 4],
        inset: bool,
    },
    GlyphRun {
        bounds: Rect,
        font_id: FontResourceId,
        font_size: f32,
        color: Rgba8,
        glyphs: Range<u32>, // index into flat GlyphInstance side-buffer
    },
    Image {
        bounds: Rect,
        image_id: ImageId,
        sampling: ImageSampling,
        radii: [f32; 4],
    },
    VectorPath {
        bounds: Rect,
        path_id: PathResourceId,
        fill: Option<Rgba8>,
        stroke: Option<(Rgba8, f32)>,
    },
    ExternalSurface {
        bounds: Rect,
        surface_id: SurfaceId, // OOPIF, Canvas2D GPU texture, WebGL/WebGPU, or Video layer
        radii: [f32; 4],
    },
}
```

**Critical rule for `PushStackingContext`:** Every `vello::Scene::push_layer` MUST pass the element's real finite `bounds` (intersected with the active viewport clip) rather than unbounded coordinates (`-1e6..1e6`), avoiding catastrophic GPU tile-allocation blowups.

---

## 3. Caching, Viewport Culling & Tile Strategy

1. **Per-Pipeline Retained Scene Cache:**
   - Chrome UI layer (`z = 100`), Web Surface layer (`z = 10`), Overlay/DevHUD layer (`z = 200`) are cached independently.
   - Updating the active tab never re-encodes the Chrome UI `vello::Scene`; hovering a Chrome tab never re-encodes the Web page `vello::Scene`.
2. **Viewport Culling During Encoding:**
   - Maintain an active world-space visible rectangle `visible_rect = viewport.inflate(128.0)` transformed into each `SpatialNode`'s local space.
   - Skip any `DisplayItem` whose `bounds` does not intersect `visible_rect`, and skip entire clipped subtrees when a `PushClip` rect is completely outside `visible_rect`.
3. **Chunked Content Strips / Tile Caching (Phase 1 -> Phase 2 Optimization):**
   - Divide tall scrollable content (`height > 2 * viewport_height`) into fixed vertical strips (e.g., `512 px` or `1024 px` tall `vello::Scene` chunks).
   - During APZ scroll, only the 2–4 strips intersecting the viewport are appended with `Affine::translate(0, -scroll_y)`—never touching off-screen strips.
4. **GPU Image & Glyph Resources:**
   - Decoded images are uploaded once to GPU textures / `vello::peniko::Image` keyed by `(PipelineId, ImageId)` and evicted via LRU budget (`256 MiB` default).
   - Font bytes (`Arc<Vec<u8>>`) are deduplicated across pipelines by content hash (`FontResourceId`).

---

## 4. External Surfaces: Video, Canvas2D, OOPIF, WebGPU/WebGL

`DisplayItem::ExternalSurface` unifies all GPU-resident surfaces without polluting the DOM or rebuilding DisplayLists per frame:
- **Video Frames (Phase 5):** Media decoder writes YUV420 / NV12 planes into a triple-buffered shared memory slot -> GPU process uploads planes to `R8Unorm` / `Rg8Unorm` `wgpu::Texture`s -> a dedicated `wgpu` fragment shader performs BT.709/BT.601 YUV->RGBA conversion directly onto the compositor target at 60 FPS with zero CPU pixel loops and zero DisplayList rebuilds.
- **Canvas2D (Phase 2):** Commands render into an off-screen `wgpu::Texture` via Vello; presenting a new canvas frame only swaps the texture handle.
- **OOPIF `<iframe>` (Phase 4):** Child Content Process submits its own `DisplayList` under its `PipelineId`; parent references it via `ExternalSurface { surface_id }`.
- **WebGPU / WebGL (Phase 4):** Swapchain texture is referenced directly by `SurfaceId`.

---

## 5. Windowing & Event Loop Contract (`rix-app`)

- `winit` runs with `ControlFlow::Wait` by default.
- Wakeups happen **exclusively** via:
  - OS input/window events (`CursorMoved`, `MouseWheel`, `KeyboardInput`, `Resized`, `RedrawRequested`),
  - `EventLoopProxy<AppWakeup>` signaled when an IPC message arrives from Content/Network/GPU,
  - A single next-deadline timer ONLY when a scheduled Chrome animation or explicit timer exists.
- **Zero `pollster::block_on`** inside `window_event` handlers. GPU surface resize and initialization happen non-blockingly or once at startup/reconfiguration.

---

## 6. Verification & Acceptance Criteria

- **Benchmarks (`criterion`):** Encoding a 10,000-item `DisplayList` with viewport culling takes < 1.5 ms; APZ scroll frame update takes < 1 ms CPU time.
- **Visual Reftests:** Headless `wgpu` renderer produces deterministic PNGs verifying nested clips, opacity layers, borders, linear gradients, rounded rects, shadows, and glyph runs.
- **Manual Check:** Scrolling a 50-screen HTML document at 60–120 Hz shows flat frame times in the `F12` HUD and 0% CPU when idle.
