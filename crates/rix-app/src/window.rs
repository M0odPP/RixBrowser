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
                if let Some(scene) = self.controller.compositor.compose() {
                    if let Err(err) = self.renderer.render(scene, clear) {
                        warn!(%err, "present failed");
                    } else {
                        self.present_count += 1;
                        self.needs_present = false;
                    }
                } else if self.needs_present {
                    let scene = self.controller.compositor.last_composed_scene();
                    if let Err(err) = self.renderer.render(scene, clear) {
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
    let width = width.max(1);
    let height = height.max(1);
    let mut controller = BrowserController::new(Size::new(width as f32, height as f32), 1.0);
    controller.set_theme(theme);
    controller.navigate_active(url);

    // Allow initial async `fetch()` / short timers / first rAF frame to settle cleanly (bounded).
    let settle_deadline = Instant::now() + Duration::from_millis(250);
    for _ in 0..32 {
        let Some(delay) = controller.tick_active_tab() else {
            break;
        };
        if Instant::now() >= settle_deadline {
            break;
        }
        let sleep_for = delay.min(Duration::from_millis(10));
        if !sleep_for.is_zero() {
            std::thread::sleep(sleep_for);
        }
    }
    controller.sync_layers();
    let clear = controller.clear_color();
    let scene = controller.compositor.compose_force();
    VelloRenderer::render_scene_to_png(scene, width, height, clear, out_path)?;
    Ok(())
}
