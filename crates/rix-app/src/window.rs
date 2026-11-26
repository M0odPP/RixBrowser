use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rix_chrome::ThemeMode;
use rix_core::{Point, Size};
use rix_render::{Compositor, VelloRenderer};
use tracing::{error, info, warn};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Window, WindowId};

use crate::browser_controller::{
    BrowserController, BrowserKey, KeyModifiers, CHROME_PIPELINE, OVERLAY_PIPELINE, WEB_PIPELINE,
};

pub const DEMO_WEB_PIPELINE: rix_ipc::PipelineId = WEB_PIPELINE;
pub const DEMO_CHROME_PIPELINE: rix_ipc::PipelineId = CHROME_PIPELINE;
pub const DEMO_OVERLAY_PIPELINE: rix_ipc::PipelineId = OVERLAY_PIPELINE;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppWakeup {
    IpcReady,
    FrameDirty,
    Shutdown,
}

pub struct BrowserShell {
    controller: BrowserController,
    renderer: VelloRenderer,
    window: Option<Arc<Window>>,
    present_count: u64,
    needs_present: bool,
    cursor_pos: Point,
    modifiers: ModifiersState,
    _proxy: EventLoopProxy<AppWakeup>,
}

impl BrowserShell {
    pub fn new(proxy: EventLoopProxy<AppWakeup>) -> Self {
        Self {
            controller: BrowserController::with_settings_path(
                Size::new(1280.0, 720.0),
                1.0,
                rix_chrome::BrowserSettings::default_config_path(),
            ),
            renderer: VelloRenderer::new(),
            window: None,
            present_count: 0,
            needs_present: true,
            cursor_pos: Point::new(640.0, 360.0),
            modifiers: ModifiersState::empty(),
            _proxy: proxy,
        }
    }

    pub fn compositor(&self) -> &Compositor {
        &self.controller.compositor
    }

    pub fn compositor_mut(&mut self) -> &mut Compositor {
        &mut self.controller.compositor
    }

    pub fn controller(&self) -> &BrowserController {
        &self.controller
    }

    pub fn controller_mut(&mut self) -> &mut BrowserController {
        &mut self.controller
    }

    pub fn present_count(&self) -> u64 {
        self.present_count
    }

    fn request_redraw_if_needed(&self) {
        if self.controller.compositor.is_dirty() || self.needs_present {
            if let Some(window) = &self.window {
                window.request_redraw();
            }
        }
    }
}

impl ApplicationHandler<AppWakeup> for BrowserShell {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        event_loop.set_control_flow(ControlFlow::Wait);

        if self.window.is_none() {
            let attrs = Window::default_attributes()
                .with_title("RixBrowser")
                .with_inner_size(LogicalSize::new(1280.0, 720.0));
            let window = match event_loop.create_window(attrs) {
                Ok(w) => Arc::new(w),
                Err(err) => {
                    error!(%err, "failed to create browser window");
                    event_loop.exit();
                    return;
                }
            };
            let phys = window.inner_size();
            let dpr = window.scale_factor() as f32;
            let logical = Size::new(
                (phys.width.max(1) as f32) / dpr.max(0.5),
                (phys.height.max(1) as f32) / dpr.max(0.5),
            );
            if let Err(err) = pollster::block_on(self.renderer.ensure_surface(
                Arc::clone(&window),
                phys.width.max(1),
                phys.height.max(1),
            )) {
                error!(%err, "failed to initialize wgpu/Vello window surface");
                event_loop.exit();
                return;
            }
            self.controller.set_viewport(logical, dpr);
            self.needs_present = true;
            self.window = Some(window);
        }

        self.request_redraw_if_needed();
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        if self.window.as_ref().is_none_or(|w| w.id() != window_id) {
            return;
        }
        match event {
            WindowEvent::CloseRequested => {
                event_loop.exit();
            }
            WindowEvent::Resized(physical_size) => {
                let w = physical_size.width.max(1);
                let h = physical_size.height.max(1);
                let resized = self.renderer.resize(w, h);
                let dpr = self
                    .window
                    .as_ref()
                    .map_or(1.0, |win| win.scale_factor() as f32);
                let logical = Size::new(w as f32 / dpr.max(0.5), h as f32 / dpr.max(0.5));
                if self.controller.set_viewport(logical, dpr) || resized {
                    self.needs_present = true;
                    self.request_redraw_if_needed();
                }
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                let (w, h) = self.renderer.size();
                let dpr = (scale_factor as f32).max(0.5);
                let logical = Size::new(w.max(1) as f32 / dpr, h.max(1) as f32 / dpr);
                if self.controller.set_viewport(logical, dpr) {
                    self.needs_present = true;
                    self.request_redraw_if_needed();
                }
            }
            WindowEvent::ModifiersChanged(new_mods) => {
                self.modifiers = new_mods.state();
            }
            WindowEvent::CursorMoved { position, .. } => {
                let dpr = self.controller.compositor.device_pixel_ratio();
                self.cursor_pos = Point::new(position.x as f32 / dpr, position.y as f32 / dpr);
                if self.controller.handle_hover(self.cursor_pos) {
                    self.request_redraw_if_needed();
                }
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button,
                ..
            } => {
                let changed = match button {
                    MouseButton::Left => self.controller.handle_left_click(self.cursor_pos),
                    MouseButton::Middle => self.controller.handle_middle_click(self.cursor_pos),
                    MouseButton::Right => self.controller.handle_right_click(self.cursor_pos),
                    _ => false,
                };
                if changed {
                    self.request_redraw_if_needed();
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let dpr = self.controller.compositor.device_pixel_ratio();
                let (dx, dy) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (-x * 48.0, -y * 48.0),
                    MouseScrollDelta::PixelDelta(pos) => {
                        (-(pos.x as f32) / dpr, -(pos.y as f32) / dpr)
                    }
                };
                if self.controller.handle_wheel(self.cursor_pos, dx, dy) {
                    self.request_redraw_if_needed();
                }
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                let mods = KeyModifiers {
                    ctrl: self.modifiers.control_key() || self.modifiers.super_key(),
                    alt: self.modifiers.alt_key(),
                    shift: self.modifiers.shift_key(),
                };
                let mut handled = false;
                if let Some(bkey) = map_winit_key(&event.logical_key) {
                    handled = self.controller.handle_key(mods, bkey);
                }
                if !handled && !mods.ctrl && !mods.alt {
                    if let Some(text) = &event.text {
                        if !text.is_empty() && !text.chars().all(char::is_control) {
                            handled = self.controller.handle_text_input(text.as_str());
                        }
                    }
                }
                if handled {
                    self.request_redraw_if_needed();
                }
            }
            WindowEvent::RedrawRequested => {
                let clear = self.controller.clear_color();
                if self.controller.compositor.compose().is_some() {
                    let scene = self.controller.compositor.last_composed_scene();
                    let plan = self.controller.compositor.backdrop_plan();
                    if let Err(err) = self.renderer.render_with_backdrop_plan(scene, plan, clear) {
                        warn!(%err, "present failed");
                    } else {
                        self.present_count += 1;
                        self.needs_present = false;
                    }
                } else if self.needs_present {
                    let scene = self.controller.compositor.last_composed_scene();
                    let plan = self.controller.compositor.backdrop_plan();
                    if let Err(err) = self.renderer.render_with_backdrop_plan(scene, plan, clear) {
                        warn!(%err, "present failed");
                    } else {
                        self.present_count += 1;
                        self.needs_present = false;
                    }
                }
            }
            _ => {}
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: AppWakeup) {
        match event {
            AppWakeup::IpcReady | AppWakeup::FrameDirty => {
                self.request_redraw_if_needed();
            }
            AppWakeup::Shutdown => {
                event_loop.exit();
            }
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if let Some(delay) = self.controller.tick_active_tab() {
            self.request_redraw_if_needed();
            event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + delay));
        } else {
            self.request_redraw_if_needed();
            // Zero active timers, zero rAF callbacks, zero in-flight async I/O -> IDLE 0 Hz!
            event_loop.set_control_flow(ControlFlow::Wait);
        }
    }
}

fn map_winit_key(key: &Key) -> Option<BrowserKey> {
    match key {
        Key::Named(NamedKey::Enter) => Some(BrowserKey::Enter),
        Key::Named(NamedKey::Escape) => Some(BrowserKey::Escape),
        Key::Named(NamedKey::Backspace) => Some(BrowserKey::Backspace),
        Key::Named(NamedKey::Delete) => Some(BrowserKey::Delete),
        Key::Named(NamedKey::Tab) => Some(BrowserKey::Tab),
        Key::Named(NamedKey::ArrowLeft) => Some(BrowserKey::ArrowLeft),
        Key::Named(NamedKey::ArrowRight) => Some(BrowserKey::ArrowRight),
        Key::Named(NamedKey::ArrowUp) => Some(BrowserKey::ArrowUp),
        Key::Named(NamedKey::ArrowDown) => Some(BrowserKey::ArrowDown),
        Key::Named(NamedKey::F12) => Some(BrowserKey::F12),
        Key::Character(s) => s.chars().next().map(BrowserKey::Char),
        _ => None,
    }
}

pub fn run_window_shell() -> Result<(), Box<dyn std::error::Error>> {
    let event_loop = EventLoop::<AppWakeup>::with_user_event().build()?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = event_loop.create_proxy();
    let mut app = BrowserShell::new(proxy);
    event_loop.run_app(&mut app)?;
    info!(
        frames_rendered = app.compositor().frames_rendered(),
        presents = app.present_count(),
        "window shell exited cleanly"
    );
    Ok(())
}

/// Populates a [`Compositor`] with the live unslop-crafted RixBrowser Chrome UI and New Tab.
pub fn populate_demo_layers(compositor: &mut Compositor, viewport: Size) {
    let mut controller = BrowserController::new(viewport, compositor.device_pixel_ratio());
    controller.sync_layers();
    let _ = controller.compositor.compose_force();
    *compositor = controller.compositor;
}

/// Renders the live unslop-crafted RixBrowser Chrome UI + New Tab page headlessly to PNG.
pub fn render_headless_demo(
    out_path: impl AsRef<Path>,
    width: u32,
    height: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    let width = width.max(1);
    let height = height.max(1);
    let mut controller = BrowserController::new(Size::new(width as f32, height as f32), 1.0);
    let clear = controller.clear_color();
    let scene = controller.compositor.compose_force();
    VelloRenderer::render_scene_to_png(scene, width, height, clear, out_path)?;
    Ok(())
}

/// Machine-readable telemetry captured during a headless [`render_url_to_png_with_metrics`] run.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RenderUrlMetrics {
    pub url: String,
    pub final_url: String,
    pub title: String,
    pub width: u32,
    pub height: u32,
    pub web_viewport_x: u32,
    pub web_viewport_y: u32,
    pub web_viewport_width: u32,
    pub web_viewport_height: u32,
    pub startup_ms: f32,
    pub navigate_ms: f32,
    pub settle_ms: f32,
    pub render_ms: f32,
    pub total_ms: f32,
    #[serde(default)]
    pub warm_total_ms: Option<f32>,
    pub wave1_ms: f32,
    pub wave2_ms: f32,
    pub css_parse_ms: f32,
    pub image_decode_ms: f32,
    pub js_ms: f32,
    pub subresources_fetched: usize,
    pub cache_hits: usize,
    pub js_error_count: usize,
    pub js_errors: Vec<String>,
    pub peak_rss_kib: u64,
    pub current_rss_kib: u64,
    #[serde(default)]
    pub cpu_time_ms: f64,
    #[serde(default)]
    pub idle_frames: u64,
    #[serde(default)]
    pub scenario: Option<crate::inspector::ScenarioReport>,
}

/// Configuration for [`render_url_with_options`].
#[derive(Debug, Clone)]
pub struct RenderUrlOptions {
    pub url: String,
    pub out_path: std::path::PathBuf,
    pub dump_dom_layout: Option<std::path::PathBuf>,
    pub post_action_png: Option<std::path::PathBuf>,
    pub run_interactive: bool,
    pub measure_warm: bool,
    pub width: u32,
    pub height: u32,
    pub dpr: f32,
    pub theme: ThemeMode,
}

fn read_proc_status_kib(key: &str) -> u64 {
    let Ok(text) = std::fs::read_to_string("/proc/self/status") else {
        return 0;
    };
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix(key) {
            if let Some(num) = rest.split_whitespace().next() {
                if let Ok(v) = num.parse::<u64>() {
                    return v;
                }
            }
        }
    }
    0
}

/// Reads total process CPU time (`utime + stime`) in milliseconds from `/proc/self/stat`.
fn read_proc_self_cpu_ms() -> f64 {
    let Ok(text) = std::fs::read_to_string("/proc/self/stat") else {
        return 0.0;
    };
    let Some(rparen) = text.rfind(')') else {
        return 0.0;
    };
    let fields: Vec<&str> = text[rparen + 1..].split_whitespace().collect();
    // After `comm)`, field 0 is state (3rd overall), so utime (14th) is index 11 and stime (15th) is 12.
    let utime: u64 = fields.get(11).and_then(|s| s.parse().ok()).unwrap_or(0);
    let stime: u64 = fields.get(12).and_then(|s| s.parse().ok()).unwrap_or(0);
    // Standard Linux USER_HZ is 100 ticks/sec => 10 ms per tick.
    (utime + stime) as f64 * 10.0
}

fn settle_active_tab(controller: &mut BrowserController) -> f32 {
    let t_settle = Instant::now();
    let fast_deadline = t_settle + Duration::from_millis(250);
    let io_deadline = t_settle + Duration::from_millis(1200);
    for _ in 0..160 {
        let Some(delay) = controller.tick_active_tab() else {
            break;
        };
        let now = Instant::now();
        let has_io = controller
            .active_tab_doc()
            .and_then(|d| d.runtime.as_ref())
            .is_some_and(|rt| rt.net.borrow().has_pending_io());
        let deadline = if has_io { io_deadline } else { fast_deadline };
        if now >= deadline {
            break;
        }
        let sleep_for = delay.min(Duration::from_millis(10));
        if !sleep_for.is_zero() {
            std::thread::sleep(sleep_for);
        }
    }
    t_settle.elapsed().as_secs_f32() * 1000.0
}

/// Navigates a headless [`BrowserController`] to `url` (executing external CSS, images, SVGs,
/// `@font-face` web fonts, JavaScript, Canvas2D, and initial async `fetch()`/timers) and renders
/// the full composed frame to a PNG file via Vello GPU.
pub fn render_url_to_png(
    url: &str,
    out_path: impl AsRef<Path>,
    width: u32,
    height: u32,
    theme: ThemeMode,
) -> Result<(), Box<dyn std::error::Error>> {
    let _ = render_url_to_png_with_metrics(url, out_path, width, height, theme)?;
    Ok(())
}

/// Same as [`render_url_to_png`], returning structured [`RenderUrlMetrics`] for conformance
/// harnesses and Chrome/Firefox head-to-head benchmarks.
pub fn render_url_to_png_with_metrics(
    url: &str,
    out_path: impl AsRef<Path>,
    width: u32,
    height: u32,
    theme: ThemeMode,
) -> Result<RenderUrlMetrics, Box<dyn std::error::Error>> {
    render_url_with_options(&RenderUrlOptions {
        url: url.to_string(),
        out_path: out_path.as_ref().to_path_buf(),
        dump_dom_layout: None,
        post_action_png: None,
        run_interactive: false,
        measure_warm: false,
        width,
        height,
        dpr: 1.0,
        theme,
    })
}

/// Full-featured headless render, DOM/computed-style/layout inspector, warm-reload, and
/// interactive smooth-scroll GPU frame-pacing runner.
pub fn render_url_with_options(
    opts: &RenderUrlOptions,
) -> Result<RenderUrlMetrics, Box<dyn std::error::Error>> {
    let cpu_start = read_proc_self_cpu_ms();
    let t_total = Instant::now();
    let width = opts.width.max(1);
    let height = opts.height.max(1);
    let dpr = opts.dpr.clamp(0.5, 4.0);

    // Overlap GPU device initialization and Vello compute shader pipeline compilation with
    // initial browser setup and network I/O on a dedicated GPU worker thread. Keeping
    // `VelloRenderer` owned solely by the worker avoids moving wgpu handles across threads.
    type GpuJob = (
        rix_render::Scene,
        u32,
        u32,
        rix_render::Color,
        Option<std::path::PathBuf>,
        std::sync::mpsc::SyncSender<Result<(), String>>,
    );
    let (gpu_tx, gpu_rx) = std::sync::mpsc::sync_channel::<GpuJob>(1);
    let gpu_worker = std::thread::Builder::new()
        .name("rix-gpu-headless".to_string())
        .spawn(move || -> Result<(), String> {
            let mut renderer = VelloRenderer::new();
            renderer
                .warm_up_headless_blocking()
                .map_err(|e| e.to_string())?;
            while let Ok((scene, w, h, clear, path, done_tx)) = gpu_rx.recv() {
                let res = match path {
                    Some(p) => renderer
                        .render_scene_to_png_warm(&scene, w, h, clear, p)
                        .map_err(|e| e.to_string()),
                    None => renderer
                        .render_to_rgba_blocking(&scene, w, h, clear)
                        .map(|_| ())
                        .map_err(|e| e.to_string()),
                };
                let _ = done_tx.send(res);
            }
            Ok(())
        })?;

    let submit_gpu = |scene: rix_render::Scene,
                      w: u32,
                      h: u32,
                      clear: rix_render::Color,
                      path: Option<std::path::PathBuf>|
     -> Result<(), String> {
        let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);
        gpu_tx
            .send((scene, w, h, clear, path, done_tx))
            .map_err(|_| "headless GPU worker channel closed".to_string())?;
        done_rx
            .recv()
            .map_err(|_| "headless GPU worker stopped".to_string())?
    };

    let t_start = Instant::now();
    let logical_size = Size::new(width as f32 / dpr, height as f32 / dpr);
    let mut controller = BrowserController::new(logical_size, dpr);
    controller.set_theme(opts.theme);
    let startup_ms = t_start.elapsed().as_secs_f32() * 1000.0;

    let t_nav = Instant::now();
    controller.navigate_active(&opts.url);
    let navigate_ms = t_nav.elapsed().as_secs_f32() * 1000.0;

    let settle_ms = settle_active_tab(&mut controller);

    let t_render = Instant::now();
    controller.sync_layers();
    let clear = controller.clear_color();
    let scene = controller.compositor.compose_force().clone();
    if let Err(err) = submit_gpu(scene, width, height, clear, Some(opts.out_path.clone())) {
        drop(gpu_tx);
        let _ = gpu_worker.join();
        return Err(err.into());
    }
    let render_ms = t_render.elapsed().as_secs_f32() * 1000.0;
    let total_ms = t_total.elapsed().as_secs_f32() * 1000.0;
    let cpu_time_ms = (read_proc_self_cpu_ms() - cpu_start).max(0.0);

    // Measure strict IDLE 0 Hz right after clean compose before running optional interactive steps.
    let frames_before_idle = controller.compositor.frames_rendered();
    for _ in 0..5 {
        let _ = controller.compositor.compose();
    }
    let idle_frames = controller
        .compositor
        .frames_rendered()
        .saturating_sub(frames_before_idle);

    // Dump structured DOM/Style/Layout snapshot BEFORE interactive mutations so it matches
    // the rendered PNG 1-to-1 and adds zero time to `total_ms`.
    if let Some(ref dump_path) = opts.dump_dom_layout {
        let snapshot = crate::inspector::capture_page_snapshot(&controller);
        let json = serde_json::to_string_pretty(&snapshot)?;
        std::fs::write(dump_path, json)?;
    }

    let peak_rss_kib = read_proc_status_kib("VmHWM:");
    let current_rss_kib = read_proc_status_kib("VmRSS:");
    let web_rect = controller.web_viewport_rect();
    let active_tab_url = controller.chrome.active().url.clone();
    let active_tab_title = controller.chrome.active().title.clone();
    let (stats, js_errors) = controller
        .active_tab_doc()
        .map(|d| (d.last_load_stats, d.last_js_errors.clone()))
        .unwrap_or_default();

    let scenario = if opts.run_interactive {
        Some(crate::inspector::run_interactive_scenario(
            &mut controller,
            width,
            height,
            opts.post_action_png.as_deref(),
            |sc, w, h, clr, path| {
                submit_gpu(sc, w, h, clr, path.map(std::path::Path::to_path_buf)).is_ok()
            },
        ))
    } else {
        None
    };

    let warm_total_ms = if opts.measure_warm {
        let t_warm = Instant::now();
        controller.navigate_active(&opts.url);
        let _ = settle_active_tab(&mut controller);
        controller.sync_layers();
        let warm_scene = controller.compositor.compose_force().clone();
        let _ = submit_gpu(warm_scene, width, height, clear, None);
        Some(t_warm.elapsed().as_secs_f32() * 1000.0)
    } else {
        None
    };

    drop(gpu_tx);
    match gpu_worker.join() {
        Ok(Ok(())) => {}
        Ok(Err(err)) => return Err(err.into()),
        Err(_) => return Err("headless GPU render thread panicked".into()),
    }

    Ok(RenderUrlMetrics {
        url: opts.url.clone(),
        final_url: active_tab_url,
        title: active_tab_title,
        width,
        height,
        web_viewport_x: (web_rect.origin.x * dpr).max(0.0).round() as u32,
        web_viewport_y: (web_rect.origin.y * dpr).max(0.0).round() as u32,
        web_viewport_width: (web_rect.size.width * dpr).max(0.0).round() as u32,
        web_viewport_height: (web_rect.size.height * dpr).max(0.0).round() as u32,
        startup_ms,
        navigate_ms,
        settle_ms,
        render_ms,
        total_ms,
        warm_total_ms,
        wave1_ms: stats.wave1_ms,
        wave2_ms: stats.wave2_ms,
        css_parse_ms: stats.css_parse_ms,
        image_decode_ms: stats.image_decode_ms,
        js_ms: stats.js_ms,
        subresources_fetched: stats.subresources_fetched,
        cache_hits: stats.network_cache_hits + stats.decoded_asset_cache_hits,
        js_error_count: stats.js_error_count,
        js_errors,
        peak_rss_kib,
        current_rss_kib,
        cpu_time_ms,
        idle_frames,
        scenario,
    })
}
