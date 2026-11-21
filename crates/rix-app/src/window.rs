use rix_core::{Point, Rect, Size, Vec2};
use rix_ipc::{FontResourceId, PipelineId, ScrollId, SpatialNodeId};
use rix_render::{
    smallvec, BlendMode, Color, ColorStop, Compositor, DisplayItem, DisplayList, GlyphInstance,
    LayerRole, Rgba8, SpatialNodeKind, VelloRenderer,
};
use rix_text::{FontDatabase, FontDescriptor, TextShaper};
use std::path::Path;
use std::sync::Arc;
use tracing::{error, info, warn};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::window::{Window, WindowId};

pub const DEMO_WEB_PIPELINE: PipelineId = PipelineId(1);
pub const DEMO_CHROME_PIPELINE: PipelineId = PipelineId(2);
pub const DEMO_OVERLAY_PIPELINE: PipelineId = PipelineId(3);

const BASE_CLEAR_COLOR: Color = Color::from_rgba8(15, 17, 23, 255);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppWakeup {
    IpcReady,
    FrameDirty,
    Shutdown,
}

pub struct BrowserShell {
    compositor: Compositor,
    renderer: VelloRenderer,
    window: Option<Arc<Window>>,
    present_count: u64,
    demo_populated: bool,
    needs_present: bool,
    cursor_pos: Point,
    _proxy: EventLoopProxy<AppWakeup>,
}

impl BrowserShell {
    pub fn new(proxy: EventLoopProxy<AppWakeup>) -> Self {
        Self {
            compositor: Compositor::new(Size::new(1280.0, 720.0), 1.0),
            renderer: VelloRenderer::new(),
            window: None,
            present_count: 0,
            demo_populated: false,
            needs_present: false,
            cursor_pos: Point::new(640.0, 360.0),
            _proxy: proxy,
        }
    }

    pub fn compositor(&self) -> &Compositor {
        &self.compositor
    }

    pub fn compositor_mut(&mut self) -> &mut Compositor {
        &mut self.compositor
    }

    pub fn present_count(&self) -> u64 {
        self.present_count
    }

    fn ensure_demo_layers(&mut self) {
        if self.demo_populated {
            return;
        }
        let vp = self.compositor.viewport_size();
        populate_demo_layers(&mut self.compositor, vp);
        self.demo_populated = true;
    }

    fn request_redraw_if_needed(&self) {
        if self.compositor.is_dirty() || self.needs_present {
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
                .with_title("RixBrowser v0.2 — Reactive Vello Compositor")
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
            // Initialize GPU surface once during startup/resumed, never inside window_event.
            if let Err(err) = pollster::block_on(self.renderer.ensure_surface(
                Arc::clone(&window),
                phys.width.max(1),
                phys.height.max(1),
            )) {
                error!(%err, "failed to initialize wgpu/Vello window surface");
                event_loop.exit();
                return;
            }
            self.compositor.set_viewport(logical, dpr);
            self.needs_present = true;
            self.window = Some(window);
        }

        self.ensure_demo_layers();
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
                // Synchronous swapchain reconfiguration (zero pollster::block_on in window events).
                let resized = self.renderer.resize(w, h);
                let dpr = self
                    .window
                    .as_ref()
                    .map_or(1.0, |win| win.scale_factor() as f32);
                let logical = Size::new(w as f32 / dpr.max(0.5), h as f32 / dpr.max(0.5));
                if self.compositor.set_viewport(logical, dpr) || resized {
                    if self.demo_populated {
                        populate_demo_layers(&mut self.compositor, logical);
                    }
                    self.needs_present = true;
                    self.request_redraw_if_needed();
                }
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                let (w, h) = self.renderer.size();
                let dpr = (scale_factor as f32).max(0.5);
                let logical = Size::new(w.max(1) as f32 / dpr, h.max(1) as f32 / dpr);
                if self.compositor.set_viewport(logical, dpr) {
                    if self.demo_populated {
                        populate_demo_layers(&mut self.compositor, logical);
                    }
                    self.needs_present = true;
                    self.request_redraw_if_needed();
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                let dpr = self.compositor.device_pixel_ratio();
                self.cursor_pos = Point::new(position.x as f32 / dpr, position.y as f32 / dpr);
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let dpr = self.compositor.device_pixel_ratio();
                let (dx, dy) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (-x * 48.0, -y * 48.0),
                    MouseScrollDelta::PixelDelta(pos) => {
                        (-(pos.x as f32) / dpr, -(pos.y as f32) / dpr)
                    }
                };
                if self.compositor.handle_wheel(self.cursor_pos, dx, dy) {
                    let _ = self.compositor.drain_scroll_syncs();
                    self.request_redraw_if_needed();
                }
            }
            WindowEvent::RedrawRequested => {
                // Idle guarantee: only compose when dirty, or re-present after swapchain resize.
                if let Some(scene) = self.compositor.compose() {
                    if let Err(err) = self.renderer.render(scene, BASE_CLEAR_COLOR) {
                        warn!(%err, "present failed");
                    } else {
                        self.present_count += 1;
                        self.needs_present = false;
                    }
                } else if self.needs_present {
                    let scene = self.compositor.last_composed_scene();
                    if let Err(err) = self.renderer.render(scene, BASE_CLEAR_COLOR) {
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

pub fn render_headless_demo(
    out_path: impl AsRef<Path>,
    width: u32,
    height: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    let width = width.max(1);
    let height = height.max(1);
    let mut compositor = Compositor::new(Size::new(width as f32, height as f32), 1.0);
    let vp = compositor.viewport_size();
    populate_demo_layers(&mut compositor, vp);
    let scene = compositor.compose_force();
    VelloRenderer::render_scene_to_png(scene, width, height, BASE_CLEAR_COLOR, out_path)?;
    Ok(())
}

fn shape_line_glyphs(
    font_db: &FontDatabase,
    compositor: &mut Compositor,
    text: &str,
    font_size: f32,
    origin: Point,
) -> Option<(FontResourceId, Rect, Vec<GlyphInstance>)> {
    let font_id = font_db
        .match_font(&FontDescriptor::new("sans-serif"))
        .or(font_db.default_sans_serif)?;
    let font = font_db.get_font(font_id)?;
    let resource_id = compositor.resources.register_rix_font(font);
    let shaper = TextShaper::new(font_db);
    let runs = shaper.shape_text(text, font_id, font_size);
    if runs.is_empty() {
        return None;
    }

    let mut pen_x = 0.0f32;
    let mut instances = Vec::new();
    for run in runs {
        for g in &run.glyphs {
            instances.push(GlyphInstance {
                glyph_id: g.glyph_id,
                x: origin.x + pen_x + g.x_offset,
                y: origin.y - g.y_offset,
            });
            pen_x += g.x_advance;
        }
    }
    let bounds = Rect::new(
        origin.x,
        origin.y - font_size,
        pen_x.max(1.0),
        font_size * 1.4,
    );
    Some((resource_id, bounds, instances))
}

pub fn populate_demo_layers(compositor: &mut Compositor, viewport: Size) {
    let mut font_db = FontDatabase::new();
    font_db.scan_system_fonts();

    let w = viewport.width.max(320.0);
    let h = viewport.height.max(240.0);
    let chrome_h = 84.0f32;

    // 1. Scrollable 50-screen WebSurface retained layer (z = 10)
    let vp_h = (h - chrome_h).max(1.0);
    let content_h = (h * 50.0).max(4000.0);
    let viewport_rect = Rect::new(0.0, chrome_h, w, vp_h);
    let content_rect = Rect::new(0.0, chrome_h, w, content_h);
    let mut web = DisplayList::new(content_rect);
    let root_scroll = web.spatial_tree.add_node(
        Some(SpatialNodeId(0)),
        SpatialNodeKind::ScrollFrame {
            content_rect,
            viewport_rect,
            external_scroll_id: ScrollId(1),
            current_offset: Vec2::ZERO,
        },
    );
    web.push(DisplayItem::PushStackingContext {
        spatial: root_scroll,
        bounds: content_rect,
        opacity: 1.0,
        mix_blend: BlendMode::Normal,
        filters: smallvec![],
    });
    web.push(DisplayItem::Rect {
        bounds: content_rect,
        color: Rgba8::new(22, 26, 35, 255),
        radii: [0.0; 4],
    });
    let card_bounds = Rect::new(48.0, chrome_h + 36.0, (w - 96.0).max(200.0), 220.0);
    web.push(DisplayItem::BoxShadow {
        bounds: card_bounds,
        offset: Vec2::new(0.0, 10.0),
        blur_radius: 24.0,
        spread_radius: 0.0,
        color: Rgba8::new(0, 0, 0, 120),
        radii: [14.0; 4],
        inset: false,
    });
    web.push(DisplayItem::LinearGradient {
        bounds: card_bounds,
        start: Point::new(card_bounds.min_x(), card_bounds.min_y()),
        end: Point::new(card_bounds.max_x(), card_bounds.max_y()),
        stops: smallvec![
            ColorStop {
                offset: 0.0,
                color: Rgba8::new(34, 41, 56, 255),
            },
            ColorStop {
                offset: 1.0,
                color: Rgba8::new(25, 30, 42, 255),
            },
        ],
        radii: [14.0; 4],
    });
    web.push(DisplayItem::Border {
        bounds: card_bounds,
        widths: [1.5; 4],
        colors: [Rgba8::new(66, 82, 110, 180); 4],
        radii: [14.0; 4],
    });
    if let Some((font_id, bounds, glyphs)) = shape_line_glyphs(
        &font_db,
        compositor,
        "RixBrowser v0.2 — Reactive GPU Compositor",
        24.0,
        Point::new(card_bounds.min_x() + 28.0, card_bounds.min_y() + 64.0),
    ) {
        web.push_glyph_run(
            bounds,
            font_id,
            24.0,
            Rgba8::new(236, 241, 248, 255),
            &glyphs,
        );
    }
    if let Some((font_id, bounds, glyphs)) = shape_line_glyphs(
        &font_db,
        compositor,
        "Zero-polling winit loop | PresentMode::AutoVsync | APZ strip cache",
        15.0,
        Point::new(card_bounds.min_x() + 28.0, card_bounds.min_y() + 108.0),
    ) {
        web.push_glyph_run(
            bounds,
            font_id,
            15.0,
            Rgba8::new(155, 168, 192, 255),
            &glyphs,
        );
    }
    // Additional scrollable sections spanning all 50 screens for interactive wheel check.
    for row in 0..140u32 {
        let y = card_bounds.max_y() + 24.0 + (row as f32) * 240.0;
        if y + 200.0 > content_rect.max_y() {
            break;
        }
        let stripe = Rect::new(48.0, y, (w - 96.0).max(200.0), 180.0);
        let shade = if row.is_multiple_of(2) { 30 } else { 36 };
        web.push(DisplayItem::Rect {
            bounds: stripe,
            color: Rgba8::new(shade, shade + 6, shade + 18, 255),
            radii: [10.0; 4],
        });
        web.push(DisplayItem::Border {
            bounds: stripe,
            widths: [1.0; 4],
            colors: [Rgba8::new(54, 66, 88, 180); 4],
            radii: [10.0; 4],
        });
    }
    web.push(DisplayItem::PopStackingContext);
    compositor.submit_display_list(DEMO_WEB_PIPELINE, LayerRole::WebSurface, 1, web);

    // 2. Chrome retained layer (z = 100)
    let mut chrome = DisplayList::new(Rect::new(0.0, 0.0, w, chrome_h));
    chrome.push(DisplayItem::Rect {
        bounds: Rect::new(0.0, 0.0, w, chrome_h),
        color: Rgba8::new(15, 18, 24, 255),
        radii: [0.0; 4],
    });
    let omnibox = Rect::new(16.0, 38.0, (w - 32.0).max(160.0), 34.0);
    chrome.push(DisplayItem::Rect {
        bounds: omnibox,
        color: Rgba8::new(26, 31, 41, 255),
        radii: [9.0; 4],
    });
    chrome.push(DisplayItem::Border {
        bounds: omnibox,
        widths: [1.0; 4],
        colors: [Rgba8::new(56, 66, 86, 255); 4],
        radii: [9.0; 4],
    });
    if let Some((font_id, bounds, glyphs)) = shape_line_glyphs(
        &font_db,
        compositor,
        "rix://welcome",
        14.0,
        Point::new(omnibox.min_x() + 14.0, omnibox.min_y() + 22.0),
    ) {
        chrome.push_glyph_run(
            bounds,
            font_id,
            14.0,
            Rgba8::new(210, 218, 232, 255),
            &glyphs,
        );
    }
    compositor.submit_display_list(DEMO_CHROME_PIPELINE, LayerRole::Chrome, 1, chrome);

    // 3. Overlay retained layer (z = 200)
    let mut overlay = DisplayList::new(Rect::new(w - 196.0, h - 42.0, 180.0, 28.0));
    let badge_bounds = Rect::new((w - 196.0).max(8.0), (h - 42.0).max(chrome_h), 180.0, 28.0);
    overlay.push(DisplayItem::PushStackingContext {
        spatial: SpatialNodeId(0),
        bounds: badge_bounds,
        opacity: 0.9,
        mix_blend: BlendMode::Normal,
        filters: smallvec![],
    });
    overlay.push(DisplayItem::Rect {
        bounds: badge_bounds,
        color: Rgba8::new(22, 163, 74, 230),
        radii: [8.0; 4],
    });
    if let Some((font_id, bounds, glyphs)) = shape_line_glyphs(
        &font_db,
        compositor,
        "GPU Vello Idle Ready",
        12.0,
        Point::new(badge_bounds.min_x() + 14.0, badge_bounds.min_y() + 18.0),
    ) {
        overlay.push_glyph_run(bounds, font_id, 12.0, Rgba8::WHITE, &glyphs);
    }
    overlay.push(DisplayItem::PopStackingContext);
    compositor.submit_display_list(DEMO_OVERLAY_PIPELINE, LayerRole::Overlay, 1, overlay);
}
