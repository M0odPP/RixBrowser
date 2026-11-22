//! GPU-backed HTML5 `CanvasRenderingContext2D` rendering via `vello::Scene`
//! into GPU-resident `ExternalSurface` textures (zero RGBA pixel arrays stored
//! in DOM nodes!).
//!
//! Reference: Spec 02 §4, Spec 06 §3, Issue #18.

use std::f64::consts::{FRAC_PI_2, PI, TAU};
use std::sync::Arc;

use rix_ipc::SurfaceId;
use rix_text::{FontDatabase, FontDescriptor, FontStyle, FontWeight, TextShaper};
use vello::kurbo::{
    Affine, Arc as KurboArc, BezPath, Cap, Join, Point, Rect, RoundedRect, RoundedRectRadii, Shape,
    Stroke, Vec2,
};
use vello::peniko::{
    BlendMode as PenikoBlendMode, Blob, Brush, Color, Compose, Extend, Fill,
    FontData as PenikoFontData, Gradient, ImageAlphaType, ImageBrush, ImageData, ImageFormat,
    InterpolationAlphaSpace, Mix, StyleRef,
};
use vello::{Glyph, Scene};

/// Canvas2D fill rule (`"nonzero"` or `"evenodd"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CanvasFillRule {
    #[default]
    NonZero,
    EvenOdd,
}

impl CanvasFillRule {
    #[must_use]
    pub fn from_str_rule(s: &str) -> Self {
        if s.eq_ignore_ascii_case("evenodd") {
            Self::EvenOdd
        } else {
            Self::NonZero
        }
    }

    #[must_use]
    fn to_peniko(self) -> Fill {
        match self {
            Self::NonZero => Fill::NonZero,
            Self::EvenOdd => Fill::EvenOdd,
        }
    }
}

/// Canvas2D horizontal text alignment (`ctx.textAlign`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CanvasTextAlign {
    #[default]
    Start,
    Left,
    Center,
    Right,
    End,
}

impl CanvasTextAlign {
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "start" => Some(Self::Start),
            "left" => Some(Self::Left),
            "center" => Some(Self::Center),
            "right" => Some(Self::Right),
            "end" => Some(Self::End),
            _ => None,
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Left => "left",
            Self::Center => "center",
            Self::Right => "right",
            Self::End => "end",
        }
    }
}

/// Canvas2D vertical text baseline (`ctx.textBaseline`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CanvasTextBaseline {
    Top,
    Hanging,
    Middle,
    #[default]
    Alphabetic,
    Ideographic,
    Bottom,
}

impl CanvasTextBaseline {
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "top" => Some(Self::Top),
            "hanging" => Some(Self::Hanging),
            "middle" => Some(Self::Middle),
            "alphabetic" => Some(Self::Alphabetic),
            "ideographic" => Some(Self::Ideographic),
            "bottom" => Some(Self::Bottom),
            _ => None,
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Top => "top",
            Self::Hanging => "hanging",
            Self::Middle => "middle",
            Self::Alphabetic => "alphabetic",
            Self::Ideographic => "ideographic",
            Self::Bottom => "bottom",
        }
    }
}

/// Pattern repetition mode (`ctx.createPattern(img, repetition)`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CanvasPatternRepeat {
    #[default]
    Repeat,
    RepeatX,
    RepeatY,
    NoRepeat,
}

impl CanvasPatternRepeat {
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "repeat" => Some(Self::Repeat),
            "repeat-x" => Some(Self::RepeatX),
            "repeat-y" => Some(Self::RepeatY),
            "no-repeat" => Some(Self::NoRepeat),
            _ => None,
        }
    }
}

/// Linear or two-point radial Canvas2D gradient (`CanvasGradient`).
#[derive(Debug, Clone)]
pub enum CanvasGradientKind {
    Linear {
        x0: f64,
        y0: f64,
        x1: f64,
        y1: f64,
    },
    Radial {
        x0: f64,
        y0: f64,
        r0: f64,
        x1: f64,
        y1: f64,
        r1: f64,
    },
}

#[derive(Debug, Clone)]
pub struct CanvasGradient {
    pub kind: CanvasGradientKind,
    pub stops: Vec<(f32, Color)>,
}

impl CanvasGradient {
    #[must_use]
    pub fn linear(x0: f64, y0: f64, x1: f64, y1: f64) -> Self {
        Self {
            kind: CanvasGradientKind::Linear { x0, y0, x1, y1 },
            stops: Vec::new(),
        }
    }

    #[must_use]
    pub fn radial(x0: f64, y0: f64, r0: f64, x1: f64, y1: f64, r1: f64) -> Self {
        Self {
            kind: CanvasGradientKind::Radial {
                x0,
                y0,
                r0: r0.max(0.0),
                x1,
                y1,
                r1: r1.max(0.0),
            },
            stops: Vec::new(),
        }
    }

    pub fn add_color_stop(&mut self, offset: f32, color: Color) -> bool {
        if !offset.is_finite() || !(0.0..=1.0).contains(&offset) {
            return false;
        }
        self.stops.push((offset, color));
        self.stops.sort_by(|a, b| a.0.total_cmp(&b.0));
        true
    }

    fn to_vello_gradient(&self, alpha: f32) -> Gradient {
        let stops: Vec<(f32, Color)> = if self.stops.is_empty() {
            vec![(0.0, Color::TRANSPARENT), (1.0, Color::TRANSPARENT)]
        } else {
            self.stops
                .iter()
                .map(|&(off, col)| (off, col.multiply_alpha(alpha)))
                .collect()
        };
        let base = match self.kind {
            CanvasGradientKind::Linear { x0, y0, x1, y1 } => {
                Gradient::new_linear((x0, y0), (x1, y1))
            }
            CanvasGradientKind::Radial {
                x0,
                y0,
                r0,
                x1,
                y1,
                r1,
            } => Gradient::new_two_point_radial((x0, y0), r0 as f32, (x1, y1), r1 as f32),
        };
        base.with_interpolation_alpha_space(InterpolationAlphaSpace::Premultiplied)
            .with_extend(Extend::Pad)
            .with_stops(stops.as_slice())
    }
}

/// Repeating or clamped raster image pattern (`CanvasPattern`).
#[derive(Debug, Clone)]
pub struct CanvasPattern {
    pub image: ImageData,
    pub repeat: CanvasPatternRepeat,
    pub transform: Affine,
}

impl CanvasPattern {
    #[must_use]
    pub fn from_rgba8(width: u32, height: u32, rgba: Vec<u8>, repeat: CanvasPatternRepeat) -> Self {
        let image = ImageData {
            data: Blob::new(Arc::new(rgba)),
            format: ImageFormat::Rgba8,
            alpha_type: ImageAlphaType::Alpha,
            width: width.max(1),
            height: height.max(1),
        };
        Self {
            image,
            repeat,
            transform: Affine::IDENTITY,
        }
    }

    fn to_brush(&self, alpha: f32) -> Brush {
        let (x_ext, y_ext) = match self.repeat {
            CanvasPatternRepeat::Repeat => (Extend::Repeat, Extend::Repeat),
            CanvasPatternRepeat::RepeatX => (Extend::Repeat, Extend::Pad),
            CanvasPatternRepeat::RepeatY => (Extend::Pad, Extend::Repeat),
            CanvasPatternRepeat::NoRepeat => (Extend::Pad, Extend::Pad),
        };
        Brush::Image(
            ImageBrush::new(self.image.clone())
                .with_x_extend(x_ext)
                .with_y_extend(y_ext)
                .with_alpha(alpha),
        )
    }
}

/// Active fill or stroke paint style.
#[derive(Debug, Clone)]
pub enum CanvasPaint {
    Color(Color),
    Gradient(CanvasGradient),
    Pattern(CanvasPattern),
}

impl Default for CanvasPaint {
    fn default() -> Self {
        Self::Color(Color::BLACK)
    }
}

impl CanvasPaint {
    fn to_brush(&self, alpha: f32) -> (Brush, Option<Affine>) {
        let a = alpha.clamp(0.0, 1.0);
        match self {
            Self::Color(c) => (Brush::Solid(c.multiply_alpha(a)), None),
            Self::Gradient(g) => (Brush::Gradient(g.to_vello_gradient(a)), None),
            Self::Pattern(p) => (
                p.to_brush(a),
                (p.transform != Affine::IDENTITY).then_some(p.transform),
            ),
        }
    }
}

/// Measured text metrics returned by `ctx.measureText(text)`.
#[derive(Debug, Clone, Copy, Default)]
pub struct CanvasTextMetrics {
    pub width: f64,
    pub actual_bounding_box_ascent: f64,
    pub actual_bounding_box_descent: f64,
    pub font_bounding_box_ascent: f64,
    pub font_bounding_box_descent: f64,
}

/// Saved Canvas2D drawing state frame (`save()` / `restore()`).
#[derive(Debug, Clone)]
pub struct CanvasState {
    pub transform: Affine,
    pub fill_style: CanvasPaint,
    pub stroke_style: CanvasPaint,
    pub global_alpha: f32,
    pub line_width: f64,
    pub line_cap: Cap,
    pub line_join: Join,
    pub miter_limit: f64,
    pub line_dash: Vec<f64>,
    pub line_dash_offset: f64,
    pub composite_op: String,
    pub font_raw: String,
    pub font_size: f32,
    pub font_family: String,
    pub font_weight: u16,
    pub font_italic: bool,
    pub text_align: CanvasTextAlign,
    pub text_baseline: CanvasTextBaseline,
    /// Number of active Vello clip layers pushed during this save frame.
    clip_layers_in_frame: usize,
}

impl Default for CanvasState {
    fn default() -> Self {
        Self {
            transform: Affine::IDENTITY,
            fill_style: CanvasPaint::Color(Color::BLACK),
            stroke_style: CanvasPaint::Color(Color::BLACK),
            global_alpha: 1.0,
            line_width: 1.0,
            line_cap: Cap::Butt,
            line_join: Join::Miter,
            miter_limit: 10.0,
            line_dash: Vec::new(),
            line_dash_offset: 0.0,
            composite_op: "source-over".to_string(),
            font_raw: "10px sans-serif".to_string(),
            font_size: 10.0,
            font_family: "sans-serif".to_string(),
            font_weight: 400,
            font_italic: false,
            text_align: CanvasTextAlign::Start,
            text_baseline: CanvasTextBaseline::Alphabetic,
            clip_layers_in_frame: 0,
        }
    }
}

/// GPU-backed `CanvasRenderingContext2D` accumulating vector commands into a
/// `vello::Scene` for off-screen `wgpu::Texture` rendering under `surface_id`.
pub struct Canvas2DContext {
    surface_id: SurfaceId,
    width: u32,
    height: u32,
    scene: Scene,
    state: CanvasState,
    state_stack: Vec<CanvasState>,
    current_path: BezPath,
    current_point: Option<Point>,
    subpath_start: Option<Point>,
    total_clip_layers: usize,
    frame_generation: u64,
    dirty: bool,
}

impl Canvas2DContext {
    /// Creates a fresh Canvas2D context bound to `surface_id` with pixel dimensions `width x height`.
    #[must_use]
    pub fn new(surface_id: SurfaceId, width: u32, height: u32) -> Self {
        Self {
            surface_id,
            width: width.max(1),
            height: height.max(1),
            scene: Scene::new(),
            state: CanvasState::default(),
            state_stack: Vec::new(),
            current_path: BezPath::new(),
            current_point: None,
            subpath_start: None,
            total_clip_layers: 0,
            frame_generation: 0,
            dirty: true,
        }
    }

    #[must_use]
    pub const fn surface_id(&self) -> SurfaceId {
        self.surface_id
    }

    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    #[must_use]
    pub const fn frame_generation(&self) -> u64 {
        self.frame_generation
    }

    #[must_use]
    pub const fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn clear_dirty(&mut self) {
        self.dirty = false;
    }

    #[must_use]
    pub fn state(&self) -> &CanvasState {
        &self.state
    }

    /// Resizes the backing canvas, clearing its scene and resetting drawing state per HTML5 spec.
    pub fn resize(&mut self, width: u32, height: u32) {
        self.width = width.max(1);
        self.height = height.max(1);
        self.scene.reset();
        self.state = CanvasState::default();
        self.state_stack.clear();
        self.current_path = BezPath::new();
        self.current_point = None;
        self.subpath_start = None;
        self.total_clip_layers = 0;
        self.mark_dirty();
    }

    /// Returns a balanced `vello::Scene` snapshot ready for GPU off-screen texture rendering
    /// (balancing any open clip layers cleanly without mutating the live drawing state).
    #[must_use]
    pub fn snapshot_scene(&self) -> Scene {
        let mut out = Scene::new();
        out.append(&self.scene, None);
        for _ in 0..self.total_clip_layers {
            out.pop_layer();
        }
        out
    }

    fn mark_dirty(&mut self) {
        self.dirty = true;
        self.frame_generation = self.frame_generation.wrapping_add(1);
    }

    // --- State Stack ---

    pub fn save(&mut self) {
        let mut saved = self.state.clone();
        saved.clip_layers_in_frame = self.state.clip_layers_in_frame;
        self.state_stack.push(saved);
        self.state.clip_layers_in_frame = 0;
    }

    pub fn restore(&mut self) {
        if let Some(prev) = self.state_stack.pop() {
            for _ in 0..self.state.clip_layers_in_frame {
                self.scene.pop_layer();
                self.total_clip_layers = self.total_clip_layers.saturating_sub(1);
            }
            self.state = prev;
        }
    }

    // --- Affine Transforms ---

    pub fn translate(&mut self, x: f64, y: f64) {
        if x.is_finite() && y.is_finite() {
            self.state.transform *= Affine::translate((x, y));
        }
    }

    pub fn rotate(&mut self, angle: f64) {
        if angle.is_finite() {
            self.state.transform *= Affine::rotate(angle);
        }
    }

    pub fn scale(&mut self, x: f64, y: f64) {
        if x.is_finite() && y.is_finite() {
            self.state.transform *= Affine::scale_non_uniform(x, y);
        }
    }

    pub fn transform(&mut self, a: f64, b: f64, c: f64, d: f64, e: f64, f: f64) {
        if [a, b, c, d, e, f].iter().all(|v| v.is_finite()) {
            self.state.transform *= Affine::new([a, b, c, d, e, f]);
        }
    }

    pub fn set_transform(&mut self, a: f64, b: f64, c: f64, d: f64, e: f64, f: f64) {
        if [a, b, c, d, e, f].iter().all(|v| v.is_finite()) {
            self.state.transform = Affine::new([a, b, c, d, e, f]);
        }
    }

    pub fn reset_transform(&mut self) {
        self.state.transform = Affine::IDENTITY;
    }

    #[must_use]
    pub fn get_transform(&self) -> [f64; 6] {
        self.state.transform.as_coeffs()
    }

    // --- Style Setters ---

    pub fn set_fill_style(&mut self, paint: CanvasPaint) {
        self.state.fill_style = paint;
    }

    pub fn set_stroke_style(&mut self, paint: CanvasPaint) {
        self.state.stroke_style = paint;
    }

    pub fn set_fill_css(&mut self, css: &str) -> bool {
        if let Some(c) = parse_css_color(css) {
            self.state.fill_style = CanvasPaint::Color(c);
            true
        } else {
            false
        }
    }

    pub fn set_stroke_css(&mut self, css: &str) -> bool {
        if let Some(c) = parse_css_color(css) {
            self.state.stroke_style = CanvasPaint::Color(c);
            true
        } else {
            false
        }
    }

    pub fn set_global_alpha(&mut self, alpha: f32) {
        if alpha.is_finite() && (0.0..=1.0).contains(&alpha) {
            self.state.global_alpha = alpha;
        }
    }

    pub fn set_line_width(&mut self, w: f64) {
        if w.is_finite() && w > 0.0 {
            self.state.line_width = w;
        }
    }

    pub fn set_line_cap(&mut self, cap: &str) {
        match cap.trim().to_ascii_lowercase().as_str() {
            "butt" => self.state.line_cap = Cap::Butt,
            "round" => self.state.line_cap = Cap::Round,
            "square" => self.state.line_cap = Cap::Square,
            _ => {}
        }
    }

    pub fn set_line_join(&mut self, join: &str) {
        match join.trim().to_ascii_lowercase().as_str() {
            "miter" => self.state.line_join = Join::Miter,
            "round" => self.state.line_join = Join::Round,
            "bevel" => self.state.line_join = Join::Bevel,
            _ => {}
        }
    }

    pub fn set_miter_limit(&mut self, limit: f64) {
        if limit.is_finite() && limit > 0.0 {
            self.state.miter_limit = limit;
        }
    }

    pub fn set_line_dash(&mut self, mut segments: Vec<f64>) {
        if segments.iter().any(|v| !v.is_finite() || *v < 0.0) {
            return;
        }
        if segments.len() % 2 == 1 {
            let copy = segments.clone();
            segments.extend(copy);
        }
        self.state.line_dash = segments;
    }

    pub fn set_line_dash_offset(&mut self, offset: f64) {
        if offset.is_finite() {
            self.state.line_dash_offset = offset;
        }
    }

    pub fn set_global_composite_operation(&mut self, op: &str) {
        let lower = op.trim().to_ascii_lowercase();
        if parse_composite_op(&lower).is_some() {
            self.state.composite_op = lower;
        }
    }

    pub fn set_font(&mut self, font: &str) {
        if let Some((size, weight, italic, family)) = parse_canvas_font(font) {
            self.state.font_raw = font.to_string();
            self.state.font_size = size;
            self.state.font_weight = weight;
            self.state.font_italic = italic;
            self.state.font_family = family;
        }
    }

    pub fn set_text_align(&mut self, align: &str) {
        if let Some(a) = CanvasTextAlign::parse(align) {
            self.state.text_align = a;
        }
    }

    pub fn set_text_baseline(&mut self, baseline: &str) {
        if let Some(b) = CanvasTextBaseline::parse(baseline) {
            self.state.text_baseline = b;
        }
    }

    // --- Path Building (points stored in canvas-space transformed at build time per HTML5 spec!) ---

    pub fn begin_path(&mut self) {
        self.current_path = BezPath::new();
        self.current_point = None;
        self.subpath_start = None;
    }

    pub fn close_path(&mut self) {
        if self.current_point.is_some() {
            self.current_path.close_path();
            self.current_point = self.subpath_start;
        }
    }

    pub fn move_to(&mut self, x: f64, y: f64) {
        if !x.is_finite() || !y.is_finite() {
            return;
        }
        let p = self.state.transform * Point::new(x, y);
        self.current_path.move_to(p);
        self.current_point = Some(p);
        self.subpath_start = Some(p);
    }

    pub fn line_to(&mut self, x: f64, y: f64) {
        if !x.is_finite() || !y.is_finite() {
            return;
        }
        let p = self.state.transform * Point::new(x, y);
        if self.current_point.is_none() {
            self.current_path.move_to(p);
            self.subpath_start = Some(p);
        } else {
            self.current_path.line_to(p);
        }
        self.current_point = Some(p);
    }

    pub fn quadratic_curve_to(&mut self, cpx: f64, cpy: f64, x: f64, y: f64) {
        if ![cpx, cpy, x, y].iter().all(|v| v.is_finite()) {
            return;
        }
        let cp = self.state.transform * Point::new(cpx, cpy);
        let p = self.state.transform * Point::new(x, y);
        if self.current_point.is_none() {
            self.current_path.move_to(cp);
            self.subpath_start = Some(cp);
        }
        self.current_path.quad_to(cp, p);
        self.current_point = Some(p);
    }

    pub fn bezier_curve_to(&mut self, cp1x: f64, cp1y: f64, cp2x: f64, cp2y: f64, x: f64, y: f64) {
        if ![cp1x, cp1y, cp2x, cp2y, x, y].iter().all(|v| v.is_finite()) {
            return;
        }
        let c1 = self.state.transform * Point::new(cp1x, cp1y);
        let c2 = self.state.transform * Point::new(cp2x, cp2y);
        let p = self.state.transform * Point::new(x, y);
        if self.current_point.is_none() {
            self.current_path.move_to(c1);
            self.subpath_start = Some(c1);
        }
        self.current_path.curve_to(c1, c2, p);
        self.current_point = Some(p);
    }

    pub fn arc(
        &mut self,
        x: f64,
        y: f64,
        radius: f64,
        start_angle: f64,
        end_angle: f64,
        counterclockwise: bool,
    ) -> Result<(), &'static str> {
        if ![x, y, radius, start_angle, end_angle]
            .iter()
            .all(|v| v.is_finite())
        {
            return Ok(());
        }
        if radius < 0.0 {
            return Err("IndexSizeError: radius must be non-negative");
        }
        self.ellipse(
            x,
            y,
            radius,
            radius,
            0.0,
            start_angle,
            end_angle,
            counterclockwise,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn ellipse(
        &mut self,
        x: f64,
        y: f64,
        rx: f64,
        ry: f64,
        rotation: f64,
        start_angle: f64,
        end_angle: f64,
        counterclockwise: bool,
    ) -> Result<(), &'static str> {
        if ![x, y, rx, ry, rotation, start_angle, end_angle]
            .iter()
            .all(|v| v.is_finite())
        {
            return Ok(());
        }
        if rx < 0.0 || ry < 0.0 {
            return Err("IndexSizeError: ellipse radii must be non-negative");
        }
        let mut sweep = end_angle - start_angle;
        if counterclockwise {
            if sweep <= -TAU {
                sweep = -TAU;
            } else if sweep >= 0.0 {
                sweep = (sweep % TAU) - TAU;
            }
        } else if sweep >= TAU {
            sweep = TAU;
        } else if sweep < 0.0 {
            sweep = (sweep % TAU) + TAU;
        }

        let arc = KurboArc::new(
            Point::new(x, y),
            Vec2::new(rx, ry),
            start_angle,
            sweep,
            rotation,
        );
        let mut first = true;
        let tf = self.state.transform;
        for el in arc.path_elements(0.1) {
            match el {
                vello::kurbo::PathEl::MoveTo(pt) => {
                    let tp = tf * pt;
                    if first {
                        if self.current_point.is_none() {
                            self.current_path.move_to(tp);
                            self.subpath_start = Some(tp);
                        } else {
                            self.current_path.line_to(tp);
                        }
                        first = false;
                    } else {
                        self.current_path.line_to(tp);
                    }
                    self.current_point = Some(tp);
                }
                vello::kurbo::PathEl::LineTo(pt) => {
                    let tp = tf * pt;
                    self.current_path.line_to(tp);
                    self.current_point = Some(tp);
                }
                vello::kurbo::PathEl::QuadTo(p1, p2) => {
                    let tp2 = tf * p2;
                    self.current_path.quad_to(tf * p1, tp2);
                    self.current_point = Some(tp2);
                }
                vello::kurbo::PathEl::CurveTo(p1, p2, p3) => {
                    let tp3 = tf * p3;
                    self.current_path.curve_to(tf * p1, tf * p2, tp3);
                    self.current_point = Some(tp3);
                }
                vello::kurbo::PathEl::ClosePath => {}
            }
        }
        Ok(())
    }

    pub fn arc_to(
        &mut self,
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
        radius: f64,
    ) -> Result<(), &'static str> {
        if ![x1, y1, x2, y2, radius].iter().all(|v| v.is_finite()) {
            return Ok(());
        }
        if radius < 0.0 {
            return Err("IndexSizeError: arcTo radius must be non-negative");
        }
        let inv = self.state.transform.inverse();
        let p0 = match self.current_point {
            Some(cp) => inv * cp,
            None => {
                self.move_to(x1, y1);
                return Ok(());
            }
        };
        let p1 = Point::new(x1, y1);
        let p2 = Point::new(x2, y2);
        let v1 = p0 - p1;
        let v2 = p2 - p1;
        let len1 = v1.hypot();
        let len2 = v2.hypot();
        if len1 < 1e-9 || len2 < 1e-9 || radius == 0.0 {
            self.line_to(x1, y1);
            return Ok(());
        }
        let u1 = v1 / len1;
        let u2 = v2 / len2;
        let cross = u1.cross(u2);
        if cross.abs() < 1e-9 {
            self.line_to(x1, y1);
            return Ok(());
        }
        let dot = u1.dot(u2).clamp(-1.0, 1.0);
        let half_angle = dot.acos() * 0.5;
        let dist = radius / half_angle.tan();
        let t1 = p1 + u1 * dist;
        let t2 = p1 + u2 * dist;
        let bisector = (u1 + u2).normalize();
        let center = p1 + bisector * (radius / half_angle.sin());
        let start_angle = (t1.y - center.y).atan2(t1.x - center.x);
        let end_angle = (t2.y - center.y).atan2(t2.x - center.x);
        let ccw = cross > 0.0;
        self.line_to(t1.x, t1.y);
        self.arc(center.x, center.y, radius, start_angle, end_angle, ccw)
    }

    pub fn rect(&mut self, x: f64, y: f64, w: f64, h: f64) {
        if ![x, y, w, h].iter().all(|v| v.is_finite()) {
            return;
        }
        let tf = self.state.transform;
        let p0 = tf * Point::new(x, y);
        let p1 = tf * Point::new(x + w, y);
        let p2 = tf * Point::new(x + w, y + h);
        let p3 = tf * Point::new(x, y + h);
        self.current_path.move_to(p0);
        self.current_path.line_to(p1);
        self.current_path.line_to(p2);
        self.current_path.line_to(p3);
        self.current_path.close_path();
        self.current_point = Some(p0);
        self.subpath_start = Some(p0);
    }

    pub fn round_rect(&mut self, x: f64, y: f64, w: f64, h: f64, radii: [f64; 4]) {
        if ![x, y, w, h].iter().all(|v| v.is_finite()) {
            return;
        }
        let (rx0, rx1) = if w >= 0.0 { (x, x + w) } else { (x + w, x) };
        let (ry0, ry1) = if h >= 0.0 { (y, y + h) } else { (y + h, y) };
        let max_r = (rx1 - rx0).min(ry1 - ry0) * 0.5;
        let rr = RoundedRect::new(
            rx0,
            ry0,
            rx1,
            ry1,
            RoundedRectRadii::new(
                radii[0].clamp(0.0, max_r),
                radii[1].clamp(0.0, max_r),
                radii[2].clamp(0.0, max_r),
                radii[3].clamp(0.0, max_r),
            ),
        );
        let tf = self.state.transform;
        let path = tf * rr.to_path(0.1);
        self.current_path.extend(path.elements().iter().copied());
        let p0 = tf * Point::new(rx0, ry0);
        self.current_point = Some(p0);
        self.subpath_start = Some(p0);
    }

    // --- Drawing Operations ---

    fn build_stroke(&self) -> Stroke {
        let mut s = Stroke::new(self.state.line_width)
            .with_caps(self.state.line_cap)
            .with_join(self.state.line_join)
            .with_miter_limit(self.state.miter_limit);
        if !self.state.line_dash.is_empty() {
            s = s.with_dashes(self.state.line_dash_offset, self.state.line_dash.clone());
        }
        s
    }

    fn with_composite_layer(&mut self, f: impl FnOnce(&mut Scene, &CanvasState)) {
        let blend = parse_composite_op(&self.state.composite_op);
        let needs_layer = blend.is_some_and(|b| b != PenikoBlendMode::default());
        if needs_layer {
            let bounds = Rect::new(0.0, 0.0, f64::from(self.width), f64::from(self.height));
            self.scene.push_layer(
                Fill::NonZero,
                blend.unwrap_or_default(),
                1.0,
                Affine::IDENTITY,
                &bounds,
            );
        }
        f(&mut self.scene, &self.state);
        if needs_layer {
            self.scene.pop_layer();
        }
        self.mark_dirty();
    }

    pub fn fill(&mut self, rule: CanvasFillRule) {
        if self.current_path.elements().is_empty() {
            return;
        }
        let path = self.current_path.clone();
        self.with_composite_layer(|scene, state| {
            let (brush, brush_tf) = state.fill_style.to_brush(state.global_alpha);
            let combined_brush_tf = brush_tf
                .map(|bt| state.transform * bt)
                .or(Some(state.transform));
            scene.fill(
                rule.to_peniko(),
                Affine::IDENTITY,
                &brush,
                combined_brush_tf,
                &path,
            );
        });
    }

    pub fn stroke(&mut self) {
        if self.current_path.elements().is_empty() {
            return;
        }
        let path = self.current_path.clone();
        let stroke = self.build_stroke();
        self.with_composite_layer(|scene, state| {
            let (brush, brush_tf) = state.stroke_style.to_brush(state.global_alpha);
            let combined_brush_tf = brush_tf
                .map(|bt| state.transform * bt)
                .or(Some(state.transform));
            scene.stroke(&stroke, Affine::IDENTITY, &brush, combined_brush_tf, &path);
        });
    }

    pub fn clip(&mut self, rule: CanvasFillRule) {
        if self.current_path.elements().is_empty() {
            return;
        }
        let path = self.current_path.clone();
        self.scene
            .push_clip_layer(rule.to_peniko(), Affine::IDENTITY, &path);
        self.state.clip_layers_in_frame += 1;
        self.total_clip_layers += 1;
    }

    #[must_use]
    pub fn is_point_in_path(&self, x: f64, y: f64, rule: CanvasFillRule) -> bool {
        if !x.is_finite() || !y.is_finite() {
            return false;
        }
        let pt = Point::new(x, y);
        let winding = self.current_path.winding(pt);
        match rule {
            CanvasFillRule::NonZero => winding != 0,
            CanvasFillRule::EvenOdd => winding % 2 != 0,
        }
    }

    pub fn fill_rect(&mut self, x: f64, y: f64, w: f64, h: f64) {
        if ![x, y, w, h].iter().all(|v| v.is_finite()) || w == 0.0 || h == 0.0 {
            return;
        }
        let r = Rect::from_origin_size((x, y), (w, h)).abs();
        self.with_composite_layer(|scene, state| {
            let (brush, brush_tf) = state.fill_style.to_brush(state.global_alpha);
            scene.fill(Fill::NonZero, state.transform, &brush, brush_tf, &r);
        });
    }

    pub fn stroke_rect(&mut self, x: f64, y: f64, w: f64, h: f64) {
        if ![x, y, w, h].iter().all(|v| v.is_finite()) || (w == 0.0 && h == 0.0) {
            return;
        }
        let r = Rect::from_origin_size((x, y), (w, h)).abs();
        let stroke = self.build_stroke();
        self.with_composite_layer(|scene, state| {
            let (brush, brush_tf) = state.stroke_style.to_brush(state.global_alpha);
            scene.stroke(&stroke, state.transform, &brush, brush_tf, &r);
        });
    }

    pub fn clear_rect(&mut self, x: f64, y: f64, w: f64, h: f64) {
        if ![x, y, w, h].iter().all(|v| v.is_finite()) || w == 0.0 || h == 0.0 {
            return;
        }
        let r = Rect::from_origin_size((x, y), (w, h)).abs();
        // Fast path: full-canvas clear with identity transform and no active clip resets scene cleanly!
        if self.total_clip_layers == 0
            && self.state.transform == Affine::IDENTITY
            && r.x0 <= 0.0
            && r.y0 <= 0.0
            && r.x1 >= f64::from(self.width)
            && r.y1 >= f64::from(self.height)
        {
            self.scene.reset();
            self.mark_dirty();
            return;
        }
        let clear_blend = PenikoBlendMode::new(Mix::Normal, Compose::Clear);
        self.scene
            .push_layer(Fill::NonZero, clear_blend, 1.0, self.state.transform, &r);
        self.scene
            .fill(Fill::NonZero, self.state.transform, Color::BLACK, None, &r);
        self.scene.pop_layer();
        self.mark_dirty();
    }

    // --- Text Rendering & Measurement via rix-text ---

    fn resolve_font_id(&self, font_db: &FontDatabase) -> Option<rix_text::FontId> {
        let style = if self.state.font_italic {
            FontStyle::Italic
        } else {
            FontStyle::Normal
        };
        let desc = FontDescriptor::new(&self.state.font_family)
            .with_weight(FontWeight(self.state.font_weight))
            .with_style(style);
        font_db
            .match_font(&desc)
            .or_else(|| font_db.match_font(&FontDescriptor::new("sans-serif")))
    }

    #[must_use]
    pub fn measure_text(&self, font_db: &FontDatabase, text: &str) -> CanvasTextMetrics {
        let size = self.state.font_size.max(1.0);
        let Some(font_id) = self.resolve_font_id(font_db) else {
            return CanvasTextMetrics {
                width: text.chars().count() as f64 * f64::from(size) * 0.6,
                actual_bounding_box_ascent: f64::from(size) * 0.8,
                actual_bounding_box_descent: f64::from(size) * 0.2,
                font_bounding_box_ascent: f64::from(size) * 0.8,
                font_bounding_box_descent: f64::from(size) * 0.2,
            };
        };
        let shaper = TextShaper::new(font_db);
        let runs = shaper.shape_text(text, font_id, size);
        let total_w: f32 = runs.iter().map(|r| r.width).sum();
        let (asc, desc) = font_db
            .get_font(font_id)
            .map(|f| {
                (
                    f64::from(f.metrics.ascender_px(size)),
                    f64::from(f.metrics.descender_px(size).abs()),
                )
            })
            .unwrap_or((f64::from(size) * 0.8, f64::from(size) * 0.2));
        CanvasTextMetrics {
            width: f64::from(total_w),
            actual_bounding_box_ascent: asc,
            actual_bounding_box_descent: desc,
            font_bounding_box_ascent: asc,
            font_bounding_box_descent: desc,
        }
    }

    pub fn fill_text(
        &mut self,
        font_db: &FontDatabase,
        text: &str,
        x: f64,
        y: f64,
        max_width: Option<f64>,
    ) {
        self.draw_text_inner(font_db, text, x, y, max_width, false);
    }

    pub fn stroke_text(
        &mut self,
        font_db: &FontDatabase,
        text: &str,
        x: f64,
        y: f64,
        max_width: Option<f64>,
    ) {
        self.draw_text_inner(font_db, text, x, y, max_width, true);
    }

    fn draw_text_inner(
        &mut self,
        font_db: &FontDatabase,
        text: &str,
        x: f64,
        y: f64,
        max_width: Option<f64>,
        is_stroke: bool,
    ) {
        if text.is_empty() || !x.is_finite() || !y.is_finite() {
            return;
        }
        let Some(font_id) = self.resolve_font_id(font_db) else {
            return;
        };
        let size = self.state.font_size.max(1.0);
        let shaper = TextShaper::new(font_db);
        let runs = shaper.shape_text(text, font_id, size);
        if runs.is_empty() {
            return;
        }
        let total_w: f64 = runs.iter().map(|r| f64::from(r.width)).sum();
        let (asc, desc) = font_db
            .get_font(font_id)
            .map(|f| {
                (
                    f64::from(f.metrics.ascender_px(size)),
                    f64::from(f.metrics.descender_px(size).abs()),
                )
            })
            .unwrap_or((f64::from(size) * 0.8, f64::from(size) * 0.2));

        let scale_x = match max_width {
            Some(mw) if mw.is_finite() && mw > 0.0 && total_w > mw => mw / total_w,
            Some(mw) if mw <= 0.0 => return,
            _ => 1.0,
        };
        let effective_w = total_w * scale_x;
        let dx = match self.state.text_align {
            CanvasTextAlign::Start | CanvasTextAlign::Left => 0.0,
            CanvasTextAlign::Center => -effective_w * 0.5,
            CanvasTextAlign::Right | CanvasTextAlign::End => -effective_w,
        };
        let dy = match self.state.text_baseline {
            CanvasTextBaseline::Top | CanvasTextBaseline::Hanging => asc,
            CanvasTextBaseline::Middle => (asc - desc) * 0.5,
            CanvasTextBaseline::Alphabetic | CanvasTextBaseline::Ideographic => 0.0,
            CanvasTextBaseline::Bottom => -desc,
        };

        let stroke = self.build_stroke();
        let base_tf = self.state.transform
            * Affine::translate((x + dx, y + dy))
            * Affine::scale_non_uniform(scale_x, 1.0);

        let mut prepared: Vec<(PenikoFontData, Vec<i16>, Vec<Glyph>)> =
            Vec::with_capacity(runs.len());
        let mut cursor_x = 0.0f32;
        for run in &runs {
            let Some(f_data) = font_db.get_font(run.font_id) else {
                cursor_x += run.width;
                continue;
            };
            let peniko_font = PenikoFontData::new(f_data.blob().clone(), f_data.face_index);
            let mut glyphs = Vec::with_capacity(run.glyphs.len());
            for g in &run.glyphs {
                glyphs.push(Glyph {
                    id: g.glyph_id,
                    x: cursor_x + g.x_offset,
                    y: -g.y_offset,
                });
                cursor_x += g.x_advance;
            }
            prepared.push((peniko_font, f_data.normalized_coords.clone(), glyphs));
        }

        self.with_composite_layer(|scene, state| {
            let paint = if is_stroke {
                &state.stroke_style
            } else {
                &state.fill_style
            };
            let (brush, _) = paint.to_brush(state.global_alpha);
            for (font, coords, glyphs) in &prepared {
                let builder = scene
                    .draw_glyphs(font)
                    .font_size(size)
                    .normalized_coords(coords)
                    .hint(true)
                    .transform(base_tf)
                    .brush(&brush);
                if is_stroke {
                    builder.draw(StyleRef::Stroke(&stroke), glyphs.iter().copied());
                } else {
                    builder.draw(StyleRef::Fill(Fill::NonZero), glyphs.iter().copied());
                }
            }
        });
    }

    // --- Bitmaps & Canvas-to-Canvas Composition ---

    /// Draws an RGBA8 image into `(dx, dy, dw, dh)` optionally cropped to `(sx, sy, sw, sh)`.
    #[allow(clippy::too_many_arguments)]
    pub fn draw_image_rgba(
        &mut self,
        src_w: u32,
        src_h: u32,
        rgba: Vec<u8>,
        sx: f64,
        sy: f64,
        sw: f64,
        sh: f64,
        dx: f64,
        dy: f64,
        dw: f64,
        dh: f64,
    ) {
        if src_w == 0 || src_h == 0 || sw <= 0.0 || sh <= 0.0 || dw == 0.0 || dh == 0.0 {
            return;
        }
        let image = ImageData {
            data: Blob::new(Arc::new(rgba)),
            format: ImageFormat::Rgba8,
            alpha_type: ImageAlphaType::Alpha,
            width: src_w,
            height: src_h,
        };
        let scale_x = dw / sw;
        let scale_y = dh / sh;
        let dst_rect = Rect::from_origin_size((dx, dy), (dw, dh)).abs();
        let img_tf = Affine::translate((dx - sx * scale_x, dy - sy * scale_y))
            * Affine::scale_non_uniform(scale_x, scale_y);

        self.with_composite_layer(|scene, state| {
            let brush = ImageBrush::new(image).with_alpha(state.global_alpha.clamp(0.0, 1.0));
            scene.push_clip_layer(Fill::NonZero, state.transform, &dst_rect);
            scene.draw_image(&brush, state.transform * img_tf);
            scene.pop_layer();
        });
    }

    /// Directly composites another GPU-backed `Canvas2DContext`'s `vello::Scene` without CPU readback!
    #[allow(clippy::too_many_arguments)]
    pub fn draw_canvas(
        &mut self,
        source: &Canvas2DContext,
        sx: f64,
        sy: f64,
        sw: f64,
        sh: f64,
        dx: f64,
        dy: f64,
        dw: f64,
        dh: f64,
    ) {
        if sw <= 0.0 || sh <= 0.0 || dw == 0.0 || dh == 0.0 {
            return;
        }
        let src_scene = source.snapshot_scene();
        let scale_x = dw / sw;
        let scale_y = dh / sh;
        let dst_rect = Rect::from_origin_size((dx, dy), (dw, dh)).abs();
        let tf = self.state.transform
            * Affine::translate((dx - sx * scale_x, dy - sy * scale_y))
            * Affine::scale_non_uniform(scale_x, scale_y);
        self.with_composite_layer(|scene, state| {
            scene.push_clip_layer(Fill::NonZero, state.transform, &dst_rect);
            if state.global_alpha < 1.0 {
                scene.push_layer(
                    Fill::NonZero,
                    PenikoBlendMode::default(),
                    state.global_alpha.clamp(0.0, 1.0),
                    state.transform,
                    &dst_rect,
                );
                scene.append(&src_scene, Some(tf));
                scene.pop_layer();
            } else {
                scene.append(&src_scene, Some(tf));
            }
            scene.pop_layer();
        });
    }

    /// Writes `ImageData` RGBA8 pixels directly at `(dx, dy)` bypassing current transform & alpha per spec.
    pub fn put_image_data(&mut self, width: u32, height: u32, rgba: Vec<u8>, dx: f64, dy: f64) {
        if width == 0 || height == 0 || rgba.len() != (width as usize) * (height as usize) * 4 {
            return;
        }
        let dst_rect = Rect::from_origin_size((dx, dy), (f64::from(width), f64::from(height)));
        let image = ImageData {
            data: Blob::new(Arc::new(rgba)),
            format: ImageFormat::Rgba8,
            alpha_type: ImageAlphaType::Alpha,
            width,
            height,
        };
        let clear_blend = PenikoBlendMode::new(Mix::Normal, Compose::Clear);
        self.scene
            .push_layer(Fill::NonZero, clear_blend, 1.0, Affine::IDENTITY, &dst_rect);
        self.scene.fill(
            Fill::NonZero,
            Affine::IDENTITY,
            Color::BLACK,
            None,
            &dst_rect,
        );
        self.scene.pop_layer();
        let brush = ImageBrush::new(image);
        self.scene.draw_image(&brush, Affine::translate((dx, dy)));
        self.mark_dirty();
    }
}

/// Encodes an RGBA8 buffer to PNG bytes in memory.
pub fn encode_rgba_to_png(
    width: u32,
    height: u32,
    rgba: &[u8],
) -> Result<Vec<u8>, image::ImageError> {
    let mut out = Vec::new();
    let encoder = image::codecs::png::PngEncoder::new(&mut out);
    image::ImageEncoder::write_image(
        encoder,
        rgba,
        width,
        height,
        image::ExtendedColorType::Rgba8,
    )?;
    Ok(out)
}

/// Encodes an RGBA8 buffer to a standard `"data:image/png;base64,..."` URI.
pub fn encode_rgba_to_png_data_url(
    width: u32,
    height: u32,
    rgba: &[u8],
) -> Result<String, image::ImageError> {
    let png = encode_rgba_to_png(width, height, rgba)?;
    Ok(format!("data:image/png;base64,{}", encode_base64(&png)))
}

fn encode_base64(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = chunk.get(1).copied().map_or(0, u32::from);
        let b2 = chunk.get(2).copied().map_or(0, u32::from);
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((triple >> 18) & 0x3F) as usize] as char);
        out.push(TABLE[((triple >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[((triple >> 6) & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(triple & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

/// Parses CSS color strings accepted by `CanvasRenderingContext2D.fillStyle` / `strokeStyle`.
#[must_use]
pub fn parse_css_color(raw: &str) -> Option<Color> {
    let s = raw.trim().to_ascii_lowercase();
    if let Some(hex) = s.strip_prefix('#') {
        let bytes = hex.as_bytes();
        let hex_digit = |b: u8| -> Option<u8> {
            match b {
                b'0'..=b'9' => Some(b - b'0'),
                b'a'..=b'f' => Some(b - b'a' + 10),
                _ => None,
            }
        };
        let byte2 = |hi: u8, lo: u8| -> Option<u8> { Some((hex_digit(hi)? << 4) | hex_digit(lo)?) };
        return match bytes.len() {
            3 => {
                let r = hex_digit(bytes[0])? * 17;
                let g = hex_digit(bytes[1])? * 17;
                let b = hex_digit(bytes[2])? * 17;
                Some(Color::from_rgba8(r, g, b, 255))
            }
            4 => {
                let r = hex_digit(bytes[0])? * 17;
                let g = hex_digit(bytes[1])? * 17;
                let b = hex_digit(bytes[2])? * 17;
                let a = hex_digit(bytes[3])? * 17;
                Some(Color::from_rgba8(r, g, b, a))
            }
            6 => Some(Color::from_rgba8(
                byte2(bytes[0], bytes[1])?,
                byte2(bytes[2], bytes[3])?,
                byte2(bytes[4], bytes[5])?,
                255,
            )),
            8 => Some(Color::from_rgba8(
                byte2(bytes[0], bytes[1])?,
                byte2(bytes[2], bytes[3])?,
                byte2(bytes[4], bytes[5])?,
                byte2(bytes[6], bytes[7])?,
            )),
            _ => None,
        };
    }

    if let Some(inner) = s
        .strip_prefix("rgba(")
        .or_else(|| s.strip_prefix("rgb("))
        .and_then(|r| r.strip_suffix(')'))
    {
        let parts: Vec<&str> = inner
            .split([',', '/'])
            .flat_map(str::split_whitespace)
            .filter(|p| !p.is_empty())
            .collect();
        if parts.len() == 3 || parts.len() == 4 {
            let parse_ch = |p: &str| -> Option<u8> {
                if let Some(pct) = p.strip_suffix('%') {
                    Some((pct.parse::<f32>().ok()?.clamp(0.0, 100.0) * 2.55).round() as u8)
                } else {
                    Some(p.parse::<f32>().ok()?.clamp(0.0, 255.0).round() as u8)
                }
            };
            let r = parse_ch(parts[0])?;
            let g = parse_ch(parts[1])?;
            let b = parse_ch(parts[2])?;
            let a = if let Some(ap) = parts.get(3) {
                if let Some(pct) = ap.strip_suffix('%') {
                    (pct.parse::<f32>().ok()?.clamp(0.0, 100.0) * 2.55).round() as u8
                } else {
                    (ap.parse::<f32>().ok()?.clamp(0.0, 1.0) * 255.0).round() as u8
                }
            } else {
                255
            };
            return Some(Color::from_rgba8(r, g, b, a));
        }
    }

    if let Some(inner) = s
        .strip_prefix("hsla(")
        .or_else(|| s.strip_prefix("hsl("))
        .and_then(|r| r.strip_suffix(')'))
    {
        let parts: Vec<&str> = inner
            .split([',', '/'])
            .flat_map(str::split_whitespace)
            .filter(|p| !p.is_empty())
            .collect();
        if parts.len() == 3 || parts.len() == 4 {
            let h = parts[0]
                .trim_end_matches("deg")
                .parse::<f32>()
                .ok()?
                .rem_euclid(360.0)
                / 360.0;
            let sat = parts[1]
                .strip_suffix('%')?
                .parse::<f32>()
                .ok()?
                .clamp(0.0, 100.0)
                / 100.0;
            let lig = parts[2]
                .strip_suffix('%')?
                .parse::<f32>()
                .ok()?
                .clamp(0.0, 100.0)
                / 100.0;
            let a = if let Some(ap) = parts.get(3) {
                (ap.parse::<f32>().ok()?.clamp(0.0, 1.0) * 255.0).round() as u8
            } else {
                255
            };
            let (r, g, b) = hsl_to_rgb(h, sat, lig);
            return Some(Color::from_rgba8(r, g, b, a));
        }
    }

    let rgba = match s.as_str() {
        "transparent" => (0, 0, 0, 0),
        "black" => (0, 0, 0, 255),
        "white" => (255, 255, 255, 255),
        "red" => (255, 0, 0, 255),
        "green" => (0, 128, 0, 255),
        "lime" => (0, 255, 0, 255),
        "blue" => (0, 0, 255, 255),
        "yellow" => (255, 255, 0, 255),
        "cyan" | "aqua" => (0, 255, 255, 255),
        "magenta" | "fuchsia" => (255, 0, 255, 255),
        "orange" => (255, 165, 0, 255),
        "purple" => (128, 0, 128, 255),
        "pink" => (255, 192, 203, 255),
        "gray" | "grey" => (128, 128, 128, 255),
        "silver" => (192, 192, 192, 255),
        "navy" => (0, 0, 128, 255),
        "teal" => (0, 128, 128, 255),
        "maroon" => (128, 0, 0, 255),
        "olive" => (128, 128, 0, 255),
        "gold" => (255, 215, 0, 255),
        "coral" => (255, 127, 80, 255),
        "crimson" => (220, 20, 60, 255),
        "indigo" => (75, 0, 130, 255),
        "skyblue" => (135, 206, 235, 255),
        "steelblue" => (70, 130, 180, 255),
        "rebeccapurple" => (102, 51, 153, 255),
        _ => return None,
    };
    Some(Color::from_rgba8(rgba.0, rgba.1, rgba.2, rgba.3))
}

fn hsl_to_rgb(h: f32, s: f32, l: f32) -> (u8, u8, u8) {
    if s == 0.0 {
        let v = (l * 255.0).round() as u8;
        return (v, v, v);
    }
    let q = if l < 0.5 {
        l * (1.0 + s)
    } else {
        l + s - l * s
    };
    let p = 2.0 * l - q;
    let hue_to_rgb = |mut t: f32| -> f32 {
        if t < 0.0 {
            t += 1.0;
        }
        if t > 1.0 {
            t -= 1.0;
        }
        if t < 1.0 / 6.0 {
            p + (q - p) * 6.0 * t
        } else if t < 0.5 {
            q
        } else if t < 2.0 / 3.0 {
            p + (q - p) * (2.0 / 3.0 - t) * 6.0
        } else {
            p
        }
    };
    (
        (hue_to_rgb(h + 1.0 / 3.0) * 255.0).round() as u8,
        (hue_to_rgb(h) * 255.0).round() as u8,
        (hue_to_rgb(h - 1.0 / 3.0) * 255.0).round() as u8,
    )
}

fn parse_composite_op(op: &str) -> Option<PenikoBlendMode> {
    let mode = match op {
        "source-over" => PenikoBlendMode::new(Mix::Normal, Compose::SrcOver),
        "source-in" => PenikoBlendMode::new(Mix::Normal, Compose::SrcIn),
        "source-out" => PenikoBlendMode::new(Mix::Normal, Compose::SrcOut),
        "source-atop" => PenikoBlendMode::new(Mix::Normal, Compose::SrcAtop),
        "destination-over" => PenikoBlendMode::new(Mix::Normal, Compose::DestOver),
        "destination-in" => PenikoBlendMode::new(Mix::Normal, Compose::DestIn),
        "destination-out" => PenikoBlendMode::new(Mix::Normal, Compose::DestOut),
        "destination-atop" => PenikoBlendMode::new(Mix::Normal, Compose::DestAtop),
        "lighter" => PenikoBlendMode::new(Mix::Normal, Compose::Plus),
        "copy" => PenikoBlendMode::new(Mix::Normal, Compose::Copy),
        "xor" => PenikoBlendMode::new(Mix::Normal, Compose::Xor),
        "multiply" => PenikoBlendMode::new(Mix::Multiply, Compose::SrcOver),
        "screen" => PenikoBlendMode::new(Mix::Screen, Compose::SrcOver),
        "overlay" => PenikoBlendMode::new(Mix::Overlay, Compose::SrcOver),
        "darken" => PenikoBlendMode::new(Mix::Darken, Compose::SrcOver),
        "lighten" => PenikoBlendMode::new(Mix::Lighten, Compose::SrcOver),
        "difference" => PenikoBlendMode::new(Mix::Difference, Compose::SrcOver),
        "exclusion" => PenikoBlendMode::new(Mix::Exclusion, Compose::SrcOver),
        _ => return None,
    };
    Some(mode)
}

fn parse_canvas_font(raw: &str) -> Option<(f32, u16, bool, String)> {
    let tokens: Vec<&str> = raw.split_whitespace().collect();
    if tokens.is_empty() {
        return None;
    }
    let mut weight = 400u16;
    let mut italic = false;
    let mut size_idx = None;
    let mut size_px = 10.0f32;
    for (i, &tok) in tokens.iter().enumerate() {
        let lower = tok.to_ascii_lowercase();
        if lower == "italic" || lower == "oblique" {
            italic = true;
            continue;
        }
        if lower == "bold" || lower == "bolder" {
            weight = 700;
            continue;
        }
        if let Ok(w) = lower.parse::<u16>() {
            if (100..=900).contains(&w) {
                weight = w;
                continue;
            }
        }
        let sz_part = lower.split('/').next().unwrap_or("");
        if let Some(px) = sz_part
            .strip_suffix("px")
            .and_then(|v| v.parse::<f32>().ok())
        {
            size_px = px.max(1.0);
            size_idx = Some(i);
            break;
        }
        if let Some(pt) = sz_part
            .strip_suffix("pt")
            .and_then(|v| v.parse::<f32>().ok())
        {
            size_px = (pt * (4.0 / 3.0)).max(1.0);
            size_idx = Some(i);
            break;
        }
        if let Some(em) = sz_part
            .strip_suffix("em")
            .and_then(|v| v.parse::<f32>().ok())
        {
            size_px = (em * 16.0).max(1.0);
            size_idx = Some(i);
            break;
        }
    }
    let idx = size_idx?;
    let family = tokens[idx + 1..]
        .join(" ")
        .trim_matches(['"', '\''])
        .to_string();
    let family = if family.is_empty() {
        "sans-serif".to_string()
    } else {
        family
    };
    Some((size_px, weight, italic, family))
}

/// Silence unused import warnings for math constants used by callers.
pub const CANVAS_QUARTER_TURN: f64 = FRAC_PI_2;
pub const CANVAS_HALF_TURN: f64 = PI;
