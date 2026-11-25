//! W3C Geometry Interfaces Level 1 (`DOMPoint*`, `DOMRect*`, `DOMRectList`, `DOMQuad`,
//! `DOMMatrix*`), CSS Transforms L2 4x4 projection, `FragmentBox` / SVG box collection,
//! `Element.prototype.getClientRects`, `Range.prototype.{getClientRects, getBoundingClientRect}`,
//! and `GeometryUtils` (`getBoxQuads`, `convert*FromNode`).

use std::collections::HashMap;
use std::rc::Rc;

use rix_core::{Point, Rect, Size};
use rix_css::{ComputedStyle, DisplayMode, EvalContext, PositionMode, TransformOp};
use rix_dom::{NodeData, NodeId};
use rix_js::{HeapHandle, JsException, JsObject, JsValue, JsVm, ObjectKind, PropertyAttrs};
use rix_layout::{EdgeInsets, Fragment, FragmentKind};

use crate::dom_bridge::{
    alloc_dom_rect, arg, define_global, define_host_accessor, define_host_method, require_node,
    SharedHost,
};

/// GC-rooted prototypes for W3C Geometry Interfaces Level 1 objects.
#[derive(Default)]
pub(crate) struct CssomGeometryState {
    pub(crate) dom_point_readonly_proto: Option<HeapHandle>,
    pub(crate) dom_point_proto: Option<HeapHandle>,
    pub(crate) dom_rect_readonly_proto: Option<HeapHandle>,
    pub(crate) dom_rect_proto: Option<HeapHandle>,
    pub(crate) dom_rect_list_proto: Option<HeapHandle>,
    pub(crate) dom_quad_proto: Option<HeapHandle>,
    pub(crate) dom_matrix_readonly_proto: Option<HeapHandle>,
    pub(crate) dom_matrix_proto: Option<HeapHandle>,
}

impl CssomGeometryState {
    pub(crate) fn collect_roots(&self, out: &mut Vec<JsValue>) {
        for p in [
            self.dom_point_readonly_proto,
            self.dom_point_proto,
            self.dom_rect_readonly_proto,
            self.dom_rect_proto,
            self.dom_rect_list_proto,
            self.dom_quad_proto,
            self.dom_matrix_readonly_proto,
            self.dom_matrix_proto,
        ]
        .into_iter()
        .flatten()
        {
            out.push(JsValue::from_object(p));
        }
    }
}

pub(crate) fn define_host_getter(
    vm: &mut JsVm,
    target: HeapHandle,
    name: &str,
    f: impl Fn(&mut JsVm, JsValue, &[JsValue]) -> Result<JsValue, JsException> + 'static,
) {
    let g = vm.alloc_host_fn(name, f);
    define_host_accessor(vm, target, name, Some(g), None);
}

pub(crate) fn alloc_host_ctor(
    vm: &mut JsVm,
    name: &str,
    f: impl Fn(&mut JsVm, JsValue, &[JsValue]) -> Result<JsValue, JsException> + 'static,
) -> HeapHandle {
    let h = vm.alloc_host_fn(name, f);
    if let Some(o) = vm.heap.get_object_mut(h) {
        o.is_constructor = true;
    }
    h
}

pub(crate) fn alloc_array_from(vm: &mut JsVm, items: Vec<JsValue>) -> HeapHandle {
    let arr = vm.alloc_array();
    for v in items {
        vm.heap.object_push_element(arr, v);
    }
    arr
}

fn root_of_node(doc: &rix_dom::Document, mut n: NodeId) -> NodeId {
    while let Some(p) = doc.parent(n) {
        n = p;
    }
    n
}

// ---------------------------------------------------------------------------
// 4x4 Homogeneous Matrix Math (column-major)
// ---------------------------------------------------------------------------

const MAT4_ID: [f64; 16] = [
    1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
];

fn mat4_mul(a: &[f64; 16], b: &[f64; 16]) -> [f64; 16] {
    let mut r = [0.0f64; 16];
    for col in 0..4 {
        for row in 0..4 {
            r[col * 4 + row] = a[row] * b[col * 4]
                + a[4 + row] * b[col * 4 + 1]
                + a[8 + row] * b[col * 4 + 2]
                + a[12 + row] * b[col * 4 + 3];
        }
    }
    r
}

fn mat4_translate(tx: f64, ty: f64, tz: f64) -> [f64; 16] {
    [
        1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, tx, ty, tz, 1.0,
    ]
}

fn mat4_scale(sx: f64, sy: f64, sz: f64) -> [f64; 16] {
    [
        sx, 0.0, 0.0, 0.0, 0.0, sy, 0.0, 0.0, 0.0, 0.0, sz, 0.0, 0.0, 0.0, 0.0, 1.0,
    ]
}

fn mat4_rotate_z(deg: f64) -> [f64; 16] {
    let (s, c) = deg.to_radians().sin_cos();
    [
        c, s, 0.0, 0.0, -s, c, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
    ]
}

fn mat4_rotate_x(deg: f64) -> [f64; 16] {
    let (s, c) = deg.to_radians().sin_cos();
    [
        1.0, 0.0, 0.0, 0.0, 0.0, c, s, 0.0, 0.0, -s, c, 0.0, 0.0, 0.0, 0.0, 1.0,
    ]
}

fn mat4_rotate_y(deg: f64) -> [f64; 16] {
    let (s, c) = deg.to_radians().sin_cos();
    [
        c, 0.0, -s, 0.0, 0.0, 1.0, 0.0, 0.0, s, 0.0, c, 0.0, 0.0, 0.0, 0.0, 1.0,
    ]
}

fn mat4_rotate_3d(rx: f64, ry: f64, rz: f64, deg: f64) -> [f64; 16] {
    let len = (rx * rx + ry * ry + rz * rz).sqrt();
    if len < 1e-9 {
        return MAT4_ID;
    }
    let (ux, uy, uz) = (rx / len, ry / len, rz / len);
    let (s, c) = deg.to_radians().sin_cos();
    let t = 1.0 - c;
    [
        t * ux * ux + c,
        t * ux * uy + uz * s,
        t * ux * uz - uy * s,
        0.0,
        t * ux * uy - uz * s,
        t * uy * uy + c,
        t * uy * uz + ux * s,
        0.0,
        t * ux * uz + uy * s,
        t * uy * uz - ux * s,
        t * uz * uz + c,
        0.0,
        0.0,
        0.0,
        0.0,
        1.0,
    ]
}

fn mat4_transform_point(m: &[f64; 16], x: f64, y: f64, z: f64, w: f64) -> (f64, f64, f64, f64) {
    let ox = m[0] * x + m[4] * y + m[8] * z + m[12] * w;
    let oy = m[1] * x + m[5] * y + m[9] * z + m[13] * w;
    let oz = m[2] * x + m[6] * y + m[10] * z + m[14] * w;
    let ow = m[3] * x + m[7] * y + m[11] * z + m[15] * w;
    (ox, oy, oz, ow)
}

pub(crate) fn mat4_project_2d(m: &[f64; 16], x: f64, y: f64) -> (f64, f64) {
    let (ox, oy, _, ow) = mat4_transform_point(m, x, y, 0.0, 1.0);
    if ow.abs() > 1e-9 && (ow - 1.0).abs() > 1e-9 {
        (ox / ow, oy / ow)
    } else {
        (ox, oy)
    }
}

/// Inverts a 4x4 column-major matrix (returns `None` when singular).
fn mat4_invert(m: &[f64; 16]) -> Option<[f64; 16]> {
    let mut inv = [0.0f64; 16];
    inv[0] = m[5] * m[10] * m[15] - m[5] * m[11] * m[14] - m[9] * m[6] * m[15]
        + m[9] * m[7] * m[14]
        + m[13] * m[6] * m[11]
        - m[13] * m[7] * m[10];
    inv[4] = -m[4] * m[10] * m[15] + m[4] * m[11] * m[14] + m[8] * m[6] * m[15]
        - m[8] * m[7] * m[14]
        - m[12] * m[6] * m[11]
        + m[12] * m[7] * m[10];
    inv[8] = m[4] * m[9] * m[15] - m[4] * m[11] * m[13] - m[8] * m[5] * m[15]
        + m[8] * m[7] * m[13]
        + m[12] * m[5] * m[11]
        - m[12] * m[7] * m[9];
    inv[12] = -m[4] * m[9] * m[14] + m[4] * m[10] * m[13] + m[8] * m[5] * m[14]
        - m[8] * m[6] * m[13]
        - m[12] * m[5] * m[10]
        + m[12] * m[6] * m[9];
    inv[1] = -m[1] * m[10] * m[15] + m[1] * m[11] * m[14] + m[9] * m[2] * m[15]
        - m[9] * m[3] * m[14]
        - m[13] * m[2] * m[11]
        + m[13] * m[3] * m[10];
    inv[5] = m[0] * m[10] * m[15] - m[0] * m[11] * m[14] - m[8] * m[2] * m[15]
        + m[8] * m[3] * m[14]
        + m[12] * m[2] * m[11]
        - m[12] * m[3] * m[10];
    inv[9] = -m[0] * m[9] * m[15] + m[0] * m[11] * m[13] + m[8] * m[1] * m[15]
        - m[8] * m[3] * m[13]
        - m[12] * m[1] * m[11]
        + m[12] * m[3] * m[9];
    inv[13] = m[0] * m[9] * m[14] - m[0] * m[10] * m[13] - m[8] * m[1] * m[14]
        + m[8] * m[2] * m[13]
        + m[12] * m[1] * m[10]
        - m[12] * m[2] * m[9];
    inv[2] = m[1] * m[6] * m[15] - m[1] * m[7] * m[14] - m[5] * m[2] * m[15]
        + m[5] * m[3] * m[14]
        + m[13] * m[2] * m[7]
        - m[13] * m[3] * m[6];
    inv[6] = -m[0] * m[6] * m[15] + m[0] * m[7] * m[14] + m[4] * m[2] * m[15]
        - m[4] * m[3] * m[14]
        - m[12] * m[2] * m[7]
        + m[12] * m[3] * m[6];
    inv[10] = m[0] * m[5] * m[15] - m[0] * m[7] * m[13] - m[4] * m[1] * m[15]
        + m[4] * m[3] * m[13]
        + m[12] * m[1] * m[7]
        - m[12] * m[3] * m[5];
    inv[14] = -m[0] * m[5] * m[14] + m[0] * m[6] * m[13] + m[4] * m[1] * m[14]
        - m[4] * m[2] * m[13]
        - m[12] * m[1] * m[6]
        + m[12] * m[2] * m[5];
    inv[3] = -m[1] * m[6] * m[11] + m[1] * m[7] * m[10] + m[5] * m[2] * m[11]
        - m[5] * m[3] * m[10]
        - m[9] * m[2] * m[7]
        + m[9] * m[3] * m[6];
    inv[7] = m[0] * m[6] * m[11] - m[0] * m[7] * m[10] - m[4] * m[2] * m[11]
        + m[4] * m[3] * m[10]
        + m[8] * m[2] * m[7]
        - m[8] * m[3] * m[6];
    inv[11] = -m[0] * m[5] * m[11] + m[0] * m[7] * m[9] + m[4] * m[1] * m[11]
        - m[4] * m[3] * m[9]
        - m[8] * m[1] * m[7]
        + m[8] * m[3] * m[5];
    inv[15] = m[0] * m[5] * m[10] - m[0] * m[6] * m[9] - m[4] * m[1] * m[10]
        + m[4] * m[2] * m[9]
        + m[8] * m[1] * m[6]
        - m[8] * m[2] * m[5];
    let det = m[0] * inv[0] + m[1] * inv[4] + m[2] * inv[8] + m[3] * inv[12];
    if det.abs() < 1e-12 || !det.is_finite() {
        return None;
    }
    let inv_det = 1.0 / det;
    for v in &mut inv {
        *v *= inv_det;
    }
    Some(inv)
}

fn mat4_is_2d(m: &[f64; 16]) -> bool {
    const EPS: f64 = 1e-12;
    m[2].abs() < EPS
        && m[3].abs() < EPS
        && m[6].abs() < EPS
        && m[7].abs() < EPS
        && m[8].abs() < EPS
        && m[9].abs() < EPS
        && (m[10] - 1.0).abs() < EPS
        && m[11].abs() < EPS
        && m[14].abs() < EPS
        && (m[15] - 1.0).abs() < EPS
}

fn mat4_is_identity(m: &[f64; 16]) -> bool {
    m.iter()
        .zip(MAT4_ID.iter())
        .all(|(a, b)| (a - b).abs() < 1e-12)
}

/// Composes CSS Transforms L2 (`translate`, `rotate`, `scale`, `transform`) + `zoom`
/// around `transform-origin` in the element's local border-box coordinate frame.
fn local_style_transform_mat4(style: &ComputedStyle, border_size: Size) -> Option<[f64; 16]> {
    let zoom = f64::from(style.visual.zoom);
    let has_zoom = (zoom - 1.0).abs() > 1e-6;
    let effects = style.effects.as_deref();
    let has_fx = effects.is_some_and(|e| {
        !e.transform.is_empty()
            || !matches!(e.translate, rix_css::values_math::TranslateSpec::None)
            || !matches!(e.rotate, rix_css::values_math::RotateSpec::None)
            || !matches!(e.scale, rix_css::values_math::ScaleSpec::None)
    });
    if !has_zoom && !has_fx {
        return None;
    }
    let eval = EvalContext {
        font_size_px: style.text.font_size_px,
        ..EvalContext::default()
    };
    let mut m = MAT4_ID;
    if has_zoom {
        m = mat4_mul(&m, &mat4_scale(zoom, zoom, 1.0));
    }
    if let Some(fx) = effects {
        let ox = f64::from(fx.transform_origin.x.resolve_px(border_size.width, &eval));
        let oy = f64::from(fx.transform_origin.y.resolve_px(border_size.height, &eval));
        let mut t = MAT4_ID;
        // 1. Individual `translate`
        if let rix_css::values_math::TranslateSpec::Values { x, y, z } = &fx.translate {
            let tx = x.eval_scalar(&eval, Some((f64::from(border_size.width), false)));
            let ty = y.as_ref().map_or(0.0, |n| {
                n.eval_scalar(&eval, Some((f64::from(border_size.height), false)))
            });
            let tz = z.as_ref().map_or(0.0, |n| n.eval_scalar(&eval, None));
            t = mat4_mul(&t, &mat4_translate(tx, ty, tz));
        }
        // 2. Individual `rotate`
        if let rix_css::values_math::RotateSpec::Angle { axis, angle } = &fx.rotate {
            let deg = angle.eval_scalar(&eval, None);
            let rm = if let Some([ax, ay, az]) = axis {
                mat4_rotate_3d(
                    ax.eval_scalar(&eval, None),
                    ay.eval_scalar(&eval, None),
                    az.eval_scalar(&eval, None),
                    deg,
                )
            } else {
                mat4_rotate_z(deg)
            };
            t = mat4_mul(&t, &rm);
        }
        // 3. Individual `scale`
        if let rix_css::values_math::ScaleSpec::Values(vals) = &fx.scale {
            let sx = vals
                .first()
                .map_or(1.0, |n| n.eval_scalar(&eval, Some((1.0, true))));
            let sy = vals
                .get(1)
                .map_or(sx, |n| n.eval_scalar(&eval, Some((1.0, true))));
            let sz = vals
                .get(2)
                .map_or(1.0, |n| n.eval_scalar(&eval, Some((1.0, true))));
            t = mat4_mul(&t, &mat4_scale(sx, sy, sz));
        }
        // 4. `transform` list
        for op in &fx.transform {
            let next = match op {
                TransformOp::Translate(tx, ty) => mat4_translate(
                    f64::from(tx.resolve_px(border_size.width, &eval)),
                    f64::from(ty.resolve_px(border_size.height, &eval)),
                    0.0,
                ),
                TransformOp::Translate3d(tx, ty, tz) => mat4_translate(
                    f64::from(tx.resolve_px(border_size.width, &eval)),
                    f64::from(ty.resolve_px(border_size.height, &eval)),
                    f64::from(*tz),
                ),
                TransformOp::TranslateZ(tz) => mat4_translate(0.0, 0.0, f64::from(*tz)),
                TransformOp::Scale(sx, sy) => mat4_scale(f64::from(*sx), f64::from(*sy), 1.0),
                TransformOp::Scale3d(sx, sy, sz) => {
                    mat4_scale(f64::from(*sx), f64::from(*sy), f64::from(*sz))
                }
                TransformOp::ScaleZ(sz) => mat4_scale(1.0, 1.0, f64::from(*sz)),
                TransformOp::RotateDeg(deg) => mat4_rotate_z(f64::from(*deg)),
                TransformOp::RotateXDeg(deg) => mat4_rotate_x(f64::from(*deg)),
                TransformOp::RotateYDeg(deg) => mat4_rotate_y(f64::from(*deg)),
                TransformOp::Rotate3d(rx, ry, rz, deg) => mat4_rotate_3d(
                    f64::from(*rx),
                    f64::from(*ry),
                    f64::from(*rz),
                    f64::from(*deg),
                ),
                TransformOp::SkewDeg(ax, ay) => [
                    1.0,
                    f64::from(ay.to_radians().tan()),
                    0.0,
                    0.0,
                    f64::from(ax.to_radians().tan()),
                    1.0,
                    0.0,
                    0.0,
                    0.0,
                    0.0,
                    1.0,
                    0.0,
                    0.0,
                    0.0,
                    0.0,
                    1.0,
                ],
                TransformOp::Perspective(d) => {
                    let inv = if *d > 1e-3 { -1.0 / f64::from(*d) } else { 0.0 };
                    [
                        1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, inv, 0.0, 0.0, 0.0,
                        1.0,
                    ]
                }
                TransformOp::Matrix(arr) => [
                    f64::from(arr[0]),
                    f64::from(arr[1]),
                    0.0,
                    0.0,
                    f64::from(arr[2]),
                    f64::from(arr[3]),
                    0.0,
                    0.0,
                    0.0,
                    0.0,
                    1.0,
                    0.0,
                    f64::from(arr[4]),
                    f64::from(arr[5]),
                    0.0,
                    1.0,
                ],
                TransformOp::Matrix3d(arr) => arr.map(f64::from),
            };
            t = mat4_mul(&t, &next);
        }
        let centered = mat4_mul(
            &mat4_mul(&mat4_translate(ox, oy, 0.0), &t),
            &mat4_translate(-ox, -oy, 0.0),
        );
        m = mat4_mul(&m, &centered);
    }
    Some(m)
}

// ---------------------------------------------------------------------------
// Fragment Box & Transform Tree Traversal
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
pub(crate) enum CssBoxType {
    Margin,
    Border,
    Padding,
    Content,
}

impl CssBoxType {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "margin" => Some(Self::Margin),
            "border" => Some(Self::Border),
            "padding" => Some(Self::Padding),
            "content" => Some(Self::Content),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct FragmentBox {
    /// Transform mapping local border-box `(0, 0)` into viewport coordinates.
    pub(crate) to_viewport: [f64; 16],
    pub(crate) border_size: Size,
    pub(crate) padding: EdgeInsets,
    pub(crate) border: EdgeInsets,
    pub(crate) margin: EdgeInsets,
    pub(crate) text: Option<String>,
}

impl FragmentBox {
    pub(crate) fn local_rect(&self, box_type: CssBoxType) -> Rect {
        match box_type {
            CssBoxType::Border => {
                Rect::new(0.0, 0.0, self.border_size.width, self.border_size.height)
            }
            CssBoxType::Margin => Rect::new(
                -self.margin.left,
                -self.margin.top,
                self.border_size.width + self.margin.horizontal(),
                self.border_size.height + self.margin.vertical(),
            ),
            CssBoxType::Padding => Rect::new(
                self.border.left,
                self.border.top,
                (self.border_size.width - self.border.horizontal()).max(0.0),
                (self.border_size.height - self.border.vertical()).max(0.0),
            ),
            CssBoxType::Content => Rect::new(
                self.border.left + self.padding.left,
                self.border.top + self.padding.top,
                (self.border_size.width - self.border.horizontal() - self.padding.horizontal())
                    .max(0.0),
                (self.border_size.height - self.border.vertical() - self.padding.vertical())
                    .max(0.0),
            ),
        }
    }

    fn quad_in_viewport(&self, box_type: CssBoxType) -> [(f64, f64); 4] {
        let r = self.local_rect(box_type);
        let x0 = f64::from(r.origin.x);
        let y0 = f64::from(r.origin.y);
        let x1 = x0 + f64::from(r.size.width);
        let y1 = y0 + f64::from(r.size.height);
        [
            mat4_project_2d(&self.to_viewport, x0, y0),
            mat4_project_2d(&self.to_viewport, x1, y0),
            mat4_project_2d(&self.to_viewport, x1, y1),
            mat4_project_2d(&self.to_viewport, x0, y1),
        ]
    }

    pub(crate) fn aabb_in_viewport(&self, box_type: CssBoxType) -> Rect {
        let pts = self.quad_in_viewport(box_type);
        let mut min_x = f64::INFINITY;
        let mut min_y = f64::INFINITY;
        let mut max_x = f64::NEG_INFINITY;
        let mut max_y = f64::NEG_INFINITY;
        for (x, y) in pts {
            min_x = min_x.min(x);
            min_y = min_y.min(y);
            max_x = max_x.max(x);
            max_y = max_y.max(y);
        }
        Rect::new(
            min_x as f32,
            min_y as f32,
            (max_x - min_x).max(0.0) as f32,
            (max_y - min_y).max(0.0) as f32,
        )
    }
}

fn collect_fragment_boxes_rec(
    frag: &Fragment,
    parent_mat: &[f64; 16],
    scroll_map: &HashMap<NodeId, Point>,
    target: NodeId,
    is_root: bool,
    out: &mut Vec<FragmentBox>,
) {
    let base_mat = if !is_root && frag.style.position == PositionMode::Fixed {
        MAT4_ID
    } else {
        *parent_mat
    };
    let mut cur_mat = mat4_mul(
        &base_mat,
        &mat4_translate(
            f64::from(frag.border_rect.origin.x),
            f64::from(frag.border_rect.origin.y),
            0.0,
        ),
    );
    if let Some(local_tf) = local_style_transform_mat4(&frag.style, frag.border_rect.size) {
        cur_mat = mat4_mul(&cur_mat, &local_tf);
    }
    if frag.dom_node == Some(target) {
        let text = match &frag.kind {
            FragmentKind::TextRun(run) => Some(run.text.clone()),
            _ => None,
        };
        out.push(FragmentBox {
            to_viewport: cur_mat,
            border_size: frag.border_rect.size,
            padding: frag.padding,
            border: frag.border,
            margin: frag.margin,
            text,
        });
    }
    let child_mat = if let Some(nid) = frag.dom_node {
        if let Some(&sc) = scroll_map.get(&nid) {
            if !is_root {
                mat4_mul(
                    &cur_mat,
                    &mat4_translate(-f64::from(sc.x), -f64::from(sc.y), 0.0),
                )
            } else {
                cur_mat
            }
        } else {
            cur_mat
        }
    } else {
        cur_mat
    };
    for ch in frag.children.iter() {
        collect_fragment_boxes_rec(ch, &child_mat, scroll_map, target, false, out);
    }
}

pub(crate) fn collect_boxes_for_node(host: &SharedHost, nid: NodeId) -> Vec<FragmentBox> {
    let mut hb = host.borrow_mut();
    let is_doc = matches!(hb.document.borrow().get(nid), Some(NodeData::Document));
    if is_doc {
        let vp = hb.viewport_for_doc(nid);
        return vec![FragmentBox {
            to_viewport: MAT4_ID,
            border_size: vp,
            padding: EdgeInsets::ZERO,
            border: EdgeInsets::ZERO,
            margin: EdgeInsets::ZERO,
            text: None,
        }];
    }
    let owner_doc = hb.owner_doc_of(nid);
    let main_root = hb.document.borrow().root();
    let mut out = Vec::new();
    if owner_doc != main_root {
        let sub_scroll = hb
            .subdoc_scroll_offsets
            .get(&owner_doc)
            .copied()
            .unwrap_or(Point::ZERO);
        let scroll_map = hb.element_scroll_offsets.clone();
        if let Some((tree, _, _)) = hb.layout_subdoc(owner_doc) {
            if let Some(root) = tree.root.as_ref() {
                let root_mat =
                    mat4_translate(-f64::from(sub_scroll.x), -f64::from(sub_scroll.y), 0.0);
                collect_fragment_boxes_rec(root, &root_mat, &scroll_map, nid, true, &mut out);
            }
        }
        return out;
    }
    let root_scroll = hb.scroll_offset;
    let scroll_map = hb.element_scroll_offsets.clone();
    if let Some(tree) = hb.ensure_layout() {
        if let Some(root) = tree.root.as_ref() {
            let root_mat =
                mat4_translate(-f64::from(root_scroll.x), -f64::from(root_scroll.y), 0.0);
            collect_fragment_boxes_rec(root, &root_mat, &scroll_map, nid, true, &mut out);
        }
    }
    drop(hb);
    if out.is_empty() {
        if let Some(svg_box) = synthesize_svg_element_box(host, nid) {
            out.push(svg_box);
        }
    }
    out
}

fn parse_svg_transform_attr(raw: &str) -> [f64; 16] {
    let mut m = MAT4_ID;
    let mut rest = raw.trim();
    while let Some(lp) = rest.find('(') {
        let name = rest[..lp].trim().trim_start_matches(',').trim();
        let after = &rest[lp + 1..];
        let Some(rp) = after.find(')') else { break };
        let args_str = &after[..rp];
        let nums: Vec<f64> = args_str
            .split(|c: char| c == ',' || c.is_ascii_whitespace())
            .filter(|s| !s.is_empty())
            .filter_map(|s| s.parse::<f64>().ok())
            .collect();
        let next = match name {
            "translate" if !nums.is_empty() => {
                mat4_translate(nums[0], nums.get(1).copied().unwrap_or(0.0), 0.0)
            }
            "scale" if !nums.is_empty() => {
                mat4_scale(nums[0], nums.get(1).copied().unwrap_or(nums[0]), 1.0)
            }
            "rotate" if !nums.is_empty() => {
                if nums.len() >= 3 {
                    let cx = nums[1];
                    let cy = nums[2];
                    mat4_mul(
                        &mat4_mul(&mat4_translate(cx, cy, 0.0), &mat4_rotate_z(nums[0])),
                        &mat4_translate(-cx, -cy, 0.0),
                    )
                } else {
                    mat4_rotate_z(nums[0])
                }
            }
            "matrix" if nums.len() >= 6 => [
                nums[0], nums[1], 0.0, 0.0, nums[2], nums[3], 0.0, 0.0, 0.0, 0.0, 1.0, 0.0,
                nums[4], nums[5], 0.0, 1.0,
            ],
            "skewX" if !nums.is_empty() => [
                1.0,
                0.0,
                0.0,
                0.0,
                nums[0].to_radians().tan(),
                1.0,
                0.0,
                0.0,
                0.0,
                0.0,
                1.0,
                0.0,
                0.0,
                0.0,
                0.0,
                1.0,
            ],
            "skewY" if !nums.is_empty() => [
                1.0,
                nums[0].to_radians().tan(),
                0.0,
                0.0,
                0.0,
                1.0,
                0.0,
                0.0,
                0.0,
                0.0,
                1.0,
                0.0,
                0.0,
                0.0,
                0.0,
                1.0,
            ],
            _ => MAT4_ID,
        };
        m = mat4_mul(&m, &next);
        rest = &after[rp + 1..];
    }
    m
}

fn parse_svg_len(raw: &str, pct_basis: f32, vp: Size, font_size: f32) -> Option<f32> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    let lower = s.to_ascii_lowercase();
    if let Some(v) = lower.strip_suffix("px") {
        return v.trim().parse::<f32>().ok();
    }
    if let Some(v) = lower.strip_suffix("vh") {
        return v.trim().parse::<f32>().ok().map(|n| n * vp.height / 100.0);
    }
    if let Some(v) = lower.strip_suffix("vw") {
        return v.trim().parse::<f32>().ok().map(|n| n * vp.width / 100.0);
    }
    if let Some(v) = lower.strip_suffix("vmin") {
        return v
            .trim()
            .parse::<f32>()
            .ok()
            .map(|n| n * vp.width.min(vp.height) / 100.0);
    }
    if let Some(v) = lower.strip_suffix("vmax") {
        return v
            .trim()
            .parse::<f32>()
            .ok()
            .map(|n| n * vp.width.max(vp.height) / 100.0);
    }
    if let Some(v) = lower.strip_suffix('%') {
        return v.trim().parse::<f32>().ok().map(|n| n * pct_basis / 100.0);
    }
    if let Some(v) = lower.strip_suffix("rem") {
        return v.trim().parse::<f32>().ok().map(|n| n * 16.0);
    }
    if let Some(v) = lower.strip_suffix("em") {
        return v.trim().parse::<f32>().ok().map(|n| n * font_size);
    }
    if let Some(v) = lower.strip_suffix("pt") {
        return v.trim().parse::<f32>().ok().map(|n| n * 96.0 / 72.0);
    }
    if let Some(v) = lower.strip_suffix("pc") {
        return v.trim().parse::<f32>().ok().map(|n| n * 16.0);
    }
    if let Some(v) = lower.strip_suffix("in") {
        return v.trim().parse::<f32>().ok().map(|n| n * 96.0);
    }
    if let Some(v) = lower.strip_suffix("cm") {
        return v.trim().parse::<f32>().ok().map(|n| n * 96.0 / 2.54);
    }
    if let Some(v) = lower.strip_suffix("mm") {
        return v.trim().parse::<f32>().ok().map(|n| n * 96.0 / 25.4);
    }
    lower.parse::<f32>().ok()
}

fn svg_local_bbox_rec(
    doc: &rix_dom::Document,
    nid: NodeId,
    vp: Size,
    parent_box: Size,
) -> Option<Rect> {
    let NodeData::Element(el) = doc.get(nid)? else {
        return None;
    };
    let fs = el
        .get_attribute("font-size")
        .and_then(|s| parse_svg_len(s, 16.0, vp, 16.0))
        .unwrap_or(16.0);
    let num_x = |a: &str| {
        el.get_attribute(a)
            .and_then(|s| parse_svg_len(s, parent_box.width, vp, fs))
    };
    let num_y = |a: &str| {
        el.get_attribute(a)
            .and_then(|s| parse_svg_len(s, parent_box.height, vp, fs))
    };
    let tag = el.name.local.as_str();
    match tag {
        "rect" | "image" | "foreignObject" | "foreignobject" => Some(Rect::new(
            num_x("x").unwrap_or(0.0),
            num_y("y").unwrap_or(0.0),
            num_x("width").unwrap_or(0.0).max(0.0),
            num_y("height").unwrap_or(0.0).max(0.0),
        )),
        "polygon" | "polyline" => {
            let pts = el.get_attribute("points")?;
            let nums: Vec<f32> = pts
                .split(|c: char| c == ',' || c.is_ascii_whitespace())
                .filter(|s| !s.is_empty())
                .filter_map(|s| s.parse::<f32>().ok())
                .collect();
            if nums.len() < 2 {
                return None;
            }
            let (mut min_x, mut min_y, mut max_x, mut max_y) = (
                f32::INFINITY,
                f32::INFINITY,
                f32::NEG_INFINITY,
                f32::NEG_INFINITY,
            );
            for &[x, y] in nums.as_chunks::<2>().0 {
                min_x = min_x.min(x);
                max_x = max_x.max(x);
                min_y = min_y.min(y);
                max_y = max_y.max(y);
            }
            Some(Rect::new(
                min_x,
                min_y,
                (max_x - min_x).max(0.0),
                (max_y - min_y).max(0.0),
            ))
        }
        "path" => {
            let d = el.get_attribute("d")?;
            let nums: Vec<f32> = d
                .split(|c: char| !c.is_ascii_digit() && c != '.' && c != '-')
                .filter(|s| !s.is_empty() && *s != "-")
                .filter_map(|s| s.parse::<f32>().ok())
                .collect();
            if nums.len() < 2 {
                return None;
            }
            let (mut min_x, mut min_y, mut max_x, mut max_y) = (
                f32::INFINITY,
                f32::INFINITY,
                f32::NEG_INFINITY,
                f32::NEG_INFINITY,
            );
            for &[x, y] in nums.as_chunks::<2>().0 {
                min_x = min_x.min(x);
                max_x = max_x.max(x);
                min_y = min_y.min(y);
                max_y = max_y.max(y);
            }
            Some(Rect::new(
                min_x,
                min_y,
                (max_x - min_x).max(0.0),
                (max_y - min_y).max(0.0),
            ))
        }
        "circle" => {
            let cx = num_x("cx").unwrap_or(0.0);
            let cy = num_y("cy").unwrap_or(0.0);
            let r = num_x("r").unwrap_or(0.0).max(0.0);
            Some(Rect::new(cx - r, cy - r, r * 2.0, r * 2.0))
        }
        "ellipse" => {
            let cx = num_x("cx").unwrap_or(0.0);
            let cy = num_y("cy").unwrap_or(0.0);
            let rx = num_x("rx").unwrap_or(0.0).max(0.0);
            let ry = num_y("ry").unwrap_or(0.0).max(0.0);
            Some(Rect::new(cx - rx, cy - ry, rx * 2.0, ry * 2.0))
        }
        "line" => {
            let x1 = num_x("x1").unwrap_or(0.0);
            let y1 = num_y("y1").unwrap_or(0.0);
            let x2 = num_x("x2").unwrap_or(0.0);
            let y2 = num_y("y2").unwrap_or(0.0);
            Some(Rect::new(
                x1.min(x2),
                y1.min(y2),
                (x2 - x1).abs(),
                (y2 - y1).abs(),
            ))
        }
        "text" | "tspan" => {
            let x = num_x("x").unwrap_or(0.0);
            let y = num_y("y").unwrap_or(fs);
            let is_ahem = el
                .get_attribute("font-family")
                .is_some_and(|ff| ff.to_ascii_lowercase().contains("ahem"));
            let advance = if is_ahem { fs } else { fs * 0.6 };
            let chars = doc.inner_text(nid).trim().chars().count().max(1) as f32;
            Some(Rect::new(x, y - fs * 0.8, chars * advance, fs))
        }
        "svg" => {
            let x = num_x("x").unwrap_or(0.0);
            let y = num_y("y").unwrap_or(0.0);
            if let (Some(w), Some(h)) = (num_x("width"), num_y("height")) {
                return Some(Rect::new(x, y, w.max(0.0), h.max(0.0)));
            }
            let mut min_x = f32::INFINITY;
            let mut min_y = f32::INFINITY;
            let mut max_x = f32::NEG_INFINITY;
            let mut max_y = f32::NEG_INFINITY;
            for ch in doc.children(nid) {
                if let Some(r) = svg_local_bbox_rec(doc, ch, vp, parent_box) {
                    min_x = min_x.min(r.origin.x);
                    min_y = min_y.min(r.origin.y);
                    max_x = max_x.max(r.max_x());
                    max_y = max_y.max(r.max_y());
                }
            }
            if min_x.is_finite() {
                Some(Rect::new(
                    x + min_x,
                    y + min_y,
                    (max_x - min_x).max(0.0),
                    (max_y - min_y).max(0.0),
                ))
            } else {
                Some(Rect::new(
                    x,
                    y,
                    num_x("width").unwrap_or(0.0),
                    num_y("height").unwrap_or(0.0),
                ))
            }
        }
        "g" | "a" => {
            let mut min_x = f32::INFINITY;
            let mut min_y = f32::INFINITY;
            let mut max_x = f32::NEG_INFINITY;
            let mut max_y = f32::NEG_INFINITY;
            for ch in doc.children(nid) {
                if let Some(r) = svg_local_bbox_rec(doc, ch, vp, parent_box) {
                    min_x = min_x.min(r.origin.x);
                    min_y = min_y.min(r.origin.y);
                    max_x = max_x.max(r.max_x());
                    max_y = max_y.max(r.max_y());
                }
            }
            if min_x.is_finite() {
                Some(Rect::new(
                    min_x,
                    min_y,
                    (max_x - min_x).max(0.0),
                    (max_y - min_y).max(0.0),
                ))
            } else {
                None
            }
        }
        _ => None,
    }
}

fn find_outermost_svg_ancestor(
    doc: &rix_dom::Document,
    nid: NodeId,
) -> Option<(NodeId, Vec<NodeId>)> {
    let svg_ns = rix_dom::Namespace::from("http://www.w3.org/2000/svg");
    let NodeData::Element(el) = doc.get(nid)? else {
        return None;
    };
    if el.name.ns != svg_ns {
        return None;
    }
    let mut chain = vec![nid];
    let mut cur = doc.parent(nid);
    let mut outermost_svg = None;
    while let Some(p) = cur {
        if let Some(NodeData::Element(pel)) = doc.get(p) {
            if pel.name.ns != svg_ns {
                break;
            }
            if pel.name.local.as_str().eq_ignore_ascii_case("svg") {
                outermost_svg = Some(p);
                // Keep walking in case this `<svg>` is nested inside another `<svg>` / `<foreignObject>`.
            }
            chain.push(p);
        } else {
            break;
        }
        cur = doc.parent(p);
    }
    let root_svg = outermost_svg?;
    // Trim `chain` so it holds nodes strictly below `root_svg` down to `nid` in top-down order.
    if let Some(pos) = chain.iter().position(|&x| x == root_svg) {
        chain.truncate(pos);
    }
    chain.reverse();
    Some((root_svg, chain))
}

fn synthesize_svg_element_box(host: &SharedHost, nid: NodeId) -> Option<FragmentBox> {
    let owner_doc = host.borrow().owner_doc_of(nid);
    let vp = host.borrow_mut().viewport_for_doc(owner_doc);
    let (svg_root, chain, view_box) = {
        let hb = host.borrow();
        let doc = hb.document.borrow();
        let (root_svg, chain) = find_outermost_svg_ancestor(&doc, nid)?;
        let vb = match doc.get(root_svg) {
            Some(NodeData::Element(pel)) => pel
                .get_attribute("viewBox")
                .or_else(|| pel.get_attribute("viewbox"))
                .and_then(|s| {
                    let nums: Vec<f64> = s
                        .split(|c: char| c == ',' || c.is_ascii_whitespace())
                        .filter(|t| !t.is_empty())
                        .filter_map(|t| t.parse::<f64>().ok())
                        .collect();
                    (nums.len() == 4 && nums[2] > 0.0 && nums[3] > 0.0)
                        .then_some((nums[0], nums[1], nums[2], nums[3]))
                }),
            _ => None,
        };
        (root_svg, chain, vb)
    };
    let root_boxes = collect_boxes_for_node(host, svg_root);
    let svg_box = root_boxes.first()?;
    let c_rect = svg_box.local_rect(CssBoxType::Content);
    let mut parent_box = view_box.map_or(c_rect.size, |(_, _, vw, vh)| {
        Size::new(vw as f32, vh as f32)
    });
    // If `nid` is inside a `<foreignObject>` ancestor, resolve percentage attributes against its size.
    {
        let hb = host.borrow();
        let doc = hb.document.borrow();
        for &anc in &chain[..chain.len().saturating_sub(1)] {
            if let Some(NodeData::Element(el)) = doc.get(anc) {
                let tag = el.name.local.as_str();
                if tag.eq_ignore_ascii_case("foreignobject") || tag.eq_ignore_ascii_case("svg") {
                    if let Some(r) = svg_local_bbox_rec(&doc, anc, vp, parent_box) {
                        if r.size.width > 0.0 && r.size.height > 0.0 {
                            parent_box = r.size;
                        }
                    }
                }
            }
        }
    }
    let local_bbox = {
        let hb = host.borrow();
        let doc = hb.document.borrow();
        svg_local_bbox_rec(&doc, nid, vp, parent_box)?
    };
    let mut mat = mat4_mul(
        &svg_box.to_viewport,
        &mat4_translate(f64::from(c_rect.origin.x), f64::from(c_rect.origin.y), 0.0),
    );
    if let Some((vx, vy, vw, vh)) = view_box {
        let sx = f64::from(c_rect.size.width) / vw;
        let sy = f64::from(c_rect.size.height) / vh;
        let s = sx.min(sy);
        mat = mat4_mul(&mat, &mat4_scale(s, s, 1.0));
        mat = mat4_mul(&mat, &mat4_translate(-vx, -vy, 0.0));
    }
    for &node in &chain {
        let tf_attr = {
            let hb = host.borrow();
            let doc = hb.document.borrow();
            match doc.get(node) {
                Some(NodeData::Element(el)) => {
                    el.get_attribute("transform").map(ToString::to_string)
                }
                _ => None,
            }
        };
        if let Some(attr) = tf_attr {
            mat = mat4_mul(&mat, &parse_svg_transform_attr(&attr));
        }
        if let Some(st) = host.borrow_mut().computed_style_for_node(node) {
            if st.display == DisplayMode::None {
                return None;
            }
            if let Some(css_tf) = local_style_transform_mat4(&st, local_bbox.size) {
                mat = mat4_mul(&mat, &css_tf);
            }
        }
    }
    mat = mat4_mul(
        &mat,
        &mat4_translate(
            f64::from(local_bbox.origin.x),
            f64::from(local_bbox.origin.y),
            0.0,
        ),
    );
    Some(FragmentBox {
        to_viewport: mat,
        border_size: local_bbox.size,
        padding: EdgeInsets::ZERO,
        border: EdgeInsets::ZERO,
        margin: EdgeInsets::ZERO,
        text: None,
    })
}

/// Returns the viewport-relative clipping rect of the outermost `<svg>` viewport when `nid`
/// is an SVG child element and that `<svg>` clips overflow (SVG UA default `overflow: hidden`).
pub(crate) fn svg_outer_root_clip_rect(host: &SharedHost, nid: NodeId) -> Option<Rect> {
    let root_svg = {
        let hb = host.borrow();
        let doc = hb.document.borrow();
        let (root_svg, _) = find_outermost_svg_ancestor(&doc, nid)?;
        root_svg
    };
    let st_opt = host.borrow_mut().computed_style_for_node(root_svg);
    if let Some(st) = st_opt {
        // Explicit `overflow: visible` disables SVG viewport clipping only if author set it,
        // wait: CSS default is Visible unless UA sets Hidden; SVG spec says non-root `<svg>`
        // clips its viewport unless explicitly `overflow: visible` in author styles.
        // We check `overflow == Visible` only if author explicitly set inline/authored overflow;
        // otherwise default to clipping to the `<svg>` content box.
        let explicit_visible = {
            let hb = host.borrow();
            let doc = hb.document.borrow();
            matches!(
                doc.get(root_svg),
                Some(NodeData::Element(el))
                    if el.get_attribute("overflow").is_some_and(|v| v.eq_ignore_ascii_case("visible"))
                        || el.get_attribute("style").is_some_and(|s| s.to_ascii_lowercase().contains("overflow") && s.to_ascii_lowercase().contains("visible"))
            )
        };
        if explicit_visible
            && st.overflow.x == rix_css::OverflowMode::Visible
            && st.overflow.y == rix_css::OverflowMode::Visible
        {
            return None;
        }
    }
    let boxes = collect_boxes_for_node(host, root_svg);
    boxes
        .first()
        .map(|b| b.aabb_in_viewport(CssBoxType::Content))
}

/// Returns half of the SVG `stroke-width` in viewport CSS pixels for SVG geometric primitives
/// when a visible `stroke` is active (for IntersectionObserver visual bounding box).
pub(crate) fn svg_stroke_expansion(host: &SharedHost, nid: NodeId) -> f32 {
    let svg_ns = rix_dom::Namespace::from("http://www.w3.org/2000/svg");
    let (stroke_w_local, root_svg) = {
        let hb = host.borrow();
        let doc = hb.document.borrow();
        let Some(NodeData::Element(el)) = doc.get(nid) else {
            return 0.0;
        };
        if el.name.ns != svg_ns {
            return 0.0;
        }
        let tag = el.name.local.as_str();
        if !matches!(
            tag,
            "rect" | "circle" | "ellipse" | "line" | "polygon" | "polyline" | "path"
        ) {
            return 0.0;
        }
        let has_stroke = el
            .get_attribute("stroke")
            .is_some_and(|s| !s.trim().is_empty() && !s.trim().eq_ignore_ascii_case("none"));
        if !has_stroke {
            return 0.0;
        }
        let sw = el
            .get_attribute("stroke-width")
            .and_then(|s| s.trim().trim_end_matches("px").parse::<f32>().ok())
            .unwrap_or(1.0);
        if sw <= 0.0 {
            return 0.0;
        }
        let root_svg = find_outermost_svg_ancestor(&doc, nid).map(|(r, _)| r);
        (sw, root_svg)
    };
    let mut scale = 1.0f32;
    if let Some(rs) = root_svg {
        let vb = {
            let hb = host.borrow();
            let doc = hb.document.borrow();
            match doc.get(rs) {
                Some(NodeData::Element(pel)) => pel
                    .get_attribute("viewBox")
                    .or_else(|| pel.get_attribute("viewbox"))
                    .and_then(|s| {
                        let nums: Vec<f32> = s
                            .split(|c: char| c == ',' || c.is_ascii_whitespace())
                            .filter(|t| !t.is_empty())
                            .filter_map(|t| t.parse::<f32>().ok())
                            .collect();
                        (nums.len() == 4 && nums[2] > 0.0 && nums[3] > 0.0)
                            .then_some((nums[2], nums[3]))
                    }),
                _ => None,
            }
        };
        if let Some((vw, vh)) = vb {
            if let Some(b) = collect_boxes_for_node(host, rs).first() {
                let cr = b.local_rect(CssBoxType::Content);
                scale = (cr.size.width / vw).min(cr.size.height / vh);
            }
        }
    }
    stroke_w_local * 0.5 * scale
}

/// Computes the CSSOM View transformed bounding client rect of `nid` (union of all fragment boxes).
pub(crate) fn transformed_bounding_rect_for_node(host: &SharedHost, nid: NodeId) -> Rect {
    let boxes = collect_boxes_for_node(host, nid);
    if boxes.is_empty() {
        return Rect::ZERO;
    }
    let mut min_x = f32::INFINITY;
    let mut min_y = f32::INFINITY;
    let mut max_x = f32::NEG_INFINITY;
    let mut max_y = f32::NEG_INFINITY;
    for b in &boxes {
        let r = b.aabb_in_viewport(CssBoxType::Border);
        min_x = min_x.min(r.origin.x);
        min_y = min_y.min(r.origin.y);
        max_x = max_x.max(r.max_x());
        max_y = max_y.max(r.max_y());
    }
    if !min_x.is_finite() {
        Rect::ZERO
    } else {
        Rect::new(
            min_x,
            min_y,
            (max_x - min_x).max(0.0),
            (max_y - min_y).max(0.0),
        )
    }
}

// ---------------------------------------------------------------------------
// Geometry Interface Object Allocators & Extractors
// ---------------------------------------------------------------------------

fn throw_dom_exception(vm: &mut JsVm, name: &str, msg: &str) -> JsException {
    let k = vm.heap.intern_key("DOMException");
    if let Ok(ctor) = vm.get_property_by_key(vm.global, k) {
        if ctor.as_object().is_some() {
            let ms = JsValue::from_string(vm.heap.alloc_string(msg));
            let ns = JsValue::from_string(vm.heap.alloc_string(name));
            if let Ok(inst) = vm.construct_value(ctor, &[ms, ns], ctor) {
                return JsException {
                    value: inst,
                    message: format!("{name}: {msg}"),
                };
            }
        }
    }
    vm.type_error(format!("{name}: {msg}"))
}

fn read_opt_f64(
    vm: &mut JsVm,
    obj: HeapHandle,
    key: &str,
    default: f64,
) -> Result<f64, JsException> {
    let k = vm.heap.intern_key(key);
    let v = vm.get_property_by_key(obj, k)?;
    if v.is_undefined() {
        Ok(default)
    } else {
        vm.to_number(v)
    }
}

pub(crate) fn set_f64_prop(vm: &mut JsVm, obj: HeapHandle, key: &str, val: f64) {
    let k = vm.heap.intern_key(key);
    vm.heap.object_define_data(
        obj,
        k,
        JsValue::from_f64(val),
        PropertyAttrs::DEFAULT,
        &mut vm.shapes,
    );
}

pub(crate) fn alloc_dom_point(
    vm: &mut JsVm,
    host: &SharedHost,
    readonly: bool,
    x: f64,
    y: f64,
    z: f64,
    w: f64,
) -> HeapHandle {
    let proto = {
        let st = host.borrow();
        let cv = st.cssom_geometry_state.borrow();
        if readonly {
            cv.dom_point_readonly_proto
        } else {
            cv.dom_point_proto
        }
        .unwrap_or(vm.object_proto)
    };
    let obj = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
    set_f64_prop(vm, obj, "x", x);
    set_f64_prop(vm, obj, "y", y);
    set_f64_prop(vm, obj, "z", z);
    set_f64_prop(vm, obj, "w", w);
    obj
}

fn extract_point_init(vm: &mut JsVm, val: JsValue) -> Result<(f64, f64, f64, f64), JsException> {
    if let Some(oh) = val.as_object() {
        Ok((
            read_opt_f64(vm, oh, "x", 0.0)?,
            read_opt_f64(vm, oh, "y", 0.0)?,
            read_opt_f64(vm, oh, "z", 0.0)?,
            read_opt_f64(vm, oh, "w", 1.0)?,
        ))
    } else {
        Ok((0.0, 0.0, 0.0, 1.0))
    }
}

pub(crate) fn alloc_dom_rect_f64(
    vm: &mut JsVm,
    host: &SharedHost,
    readonly: bool,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
) -> HeapHandle {
    let proto = {
        let st = host.borrow();
        let cv = st.cssom_geometry_state.borrow();
        if readonly {
            cv.dom_rect_readonly_proto
        } else {
            cv.dom_rect_proto
        }
        .unwrap_or(vm.object_proto)
    };
    let obj = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
    set_f64_prop(vm, obj, "x", x);
    set_f64_prop(vm, obj, "y", y);
    set_f64_prop(vm, obj, "width", w);
    set_f64_prop(vm, obj, "height", h);
    obj
}

pub(crate) fn alloc_dom_rect_list(vm: &mut JsVm, host: &SharedHost, rects: &[Rect]) -> HeapHandle {
    let proto = host
        .borrow()
        .cssom_geometry_state
        .borrow()
        .dom_rect_list_proto
        .unwrap_or(vm.object_proto);
    let obj = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
    for (i, r) in rects.iter().enumerate() {
        let rh = alloc_dom_rect(vm, host, *r);
        let k = vm.heap.intern_key(&i.to_string());
        vm.heap.object_define_data(
            obj,
            k,
            JsValue::from_object(rh),
            PropertyAttrs::ENUMERABLE | PropertyAttrs::CONFIGURABLE,
            &mut vm.shapes,
        );
    }
    let k_len = vm.heap.intern_key("length");
    vm.heap.object_define_data(
        obj,
        k_len,
        JsValue::from_f64(rects.len() as f64),
        PropertyAttrs::CONFIGURABLE,
        &mut vm.shapes,
    );
    obj
}

fn alloc_dom_quad(vm: &mut JsVm, host: &SharedHost, pts: [(f64, f64, f64, f64); 4]) -> HeapHandle {
    let proto = host
        .borrow()
        .cssom_geometry_state
        .borrow()
        .dom_quad_proto
        .unwrap_or(vm.object_proto);
    let obj = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
    for (name, (x, y, z, w)) in [
        ("p1", pts[0]),
        ("p2", pts[1]),
        ("p3", pts[2]),
        ("p4", pts[3]),
    ] {
        let ph = alloc_dom_point(vm, host, false, x, y, z, w);
        let k = vm.heap.intern_key(name);
        vm.heap.object_define_data(
            obj,
            k,
            JsValue::from_object(ph),
            PropertyAttrs::DEFAULT,
            &mut vm.shapes,
        );
    }
    obj
}

pub(crate) fn alloc_dom_matrix(
    vm: &mut JsVm,
    host: &SharedHost,
    readonly: bool,
    m: [f64; 16],
) -> HeapHandle {
    let proto = {
        let st = host.borrow();
        let cv = st.cssom_geometry_state.borrow();
        if readonly {
            cv.dom_matrix_readonly_proto
        } else {
            cv.dom_matrix_proto
        }
        .unwrap_or(vm.object_proto)
    };
    let obj = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
    let names = [
        "m11", "m12", "m13", "m14", "m21", "m22", "m23", "m24", "m31", "m32", "m33", "m34", "m41",
        "m42", "m43", "m44",
    ];
    for (name, &val) in names.iter().zip(m.iter()) {
        set_f64_prop(vm, obj, name, val);
    }
    let is2d = mat4_is_2d(&m);
    let k_is2d = vm.heap.intern_key("is2D");
    vm.heap.object_define_data(
        obj,
        k_is2d,
        JsValue::from_bool(is2d),
        PropertyAttrs::CONFIGURABLE,
        &mut vm.shapes,
    );
    obj
}

pub(crate) fn read_dom_matrix(vm: &mut JsVm, obj: HeapHandle) -> Result<[f64; 16], JsException> {
    let mut m = MAT4_ID;
    let names = [
        ("m11", 0, 1.0),
        ("m12", 1, 0.0),
        ("m13", 2, 0.0),
        ("m14", 3, 0.0),
        ("m21", 4, 0.0),
        ("m22", 5, 1.0),
        ("m23", 6, 0.0),
        ("m24", 7, 0.0),
        ("m31", 8, 0.0),
        ("m32", 9, 0.0),
        ("m33", 10, 1.0),
        ("m34", 11, 0.0),
        ("m41", 12, 0.0),
        ("m42", 13, 0.0),
        ("m43", 14, 0.0),
        ("m44", 15, 1.0),
    ];
    for (name, idx, def) in names {
        let k = vm.heap.intern_key(name);
        let v = vm.get_property_by_key(obj, k)?;
        m[idx] = if v.is_undefined() {
            def
        } else {
            vm.to_number(v)?
        };
    }
    for (alias, idx) in [("a", 0), ("b", 1), ("c", 4), ("d", 5), ("e", 12), ("f", 13)] {
        let k = vm.heap.intern_key(alias);
        if let Some(prop) = vm.heap.object_get_own(obj, k, &vm.shapes) {
            if !prop.value.is_undefined() {
                m[idx] = vm.to_number(prop.value)?;
            }
        }
    }
    Ok(m)
}

pub(crate) fn install_geometry_interfaces(vm: &mut JsVm, host: &SharedHost) {
    // --- DOMPointReadOnly & DOMPoint ---
    let dpro = vm.alloc_ordinary_object();
    let dp = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(dpro)));
    {
        let h = Rc::clone(host);
        define_host_method(vm, dpro, "matrixTransform", move |vm, t, args| {
            let (x, y, z, w) = extract_point_init(vm, t)?;
            let m = if let Some(oh) = arg(args, 0).as_object() {
                read_dom_matrix(vm, oh)?
            } else {
                MAT4_ID
            };
            let (nx, ny, nz, nw) = mat4_transform_point(&m, x, y, z, w);
            Ok(JsValue::from_object(alloc_dom_point(
                vm, &h, false, nx, ny, nz, nw,
            )))
        });
        define_host_method(vm, dpro, "toJSON", |vm, t, _| {
            let (x, y, z, w) = extract_point_init(vm, t)?;
            let o = vm.alloc_ordinary_object();
            set_f64_prop(vm, o, "x", x);
            set_f64_prop(vm, o, "y", y);
            set_f64_prop(vm, o, "z", z);
            set_f64_prop(vm, o, "w", w);
            Ok(JsValue::from_object(o))
        });
    }
    let h_dpro = Rc::clone(host);
    let dpro_ctor = alloc_host_ctor(vm, "DOMPointReadOnly", move |vm, _, args| {
        let x = if arg(args, 0).is_undefined() {
            0.0
        } else {
            vm.to_number(arg(args, 0))?
        };
        let y = if arg(args, 1).is_undefined() {
            0.0
        } else {
            vm.to_number(arg(args, 1))?
        };
        let z = if arg(args, 2).is_undefined() {
            0.0
        } else {
            vm.to_number(arg(args, 2))?
        };
        let w = if arg(args, 3).is_undefined() {
            1.0
        } else {
            vm.to_number(arg(args, 3))?
        };
        Ok(JsValue::from_object(alloc_dom_point(
            vm, &h_dpro, true, x, y, z, w,
        )))
    });
    let h_dp = Rc::clone(host);
    let dp_ctor = alloc_host_ctor(vm, "DOMPoint", move |vm, _, args| {
        let x = if arg(args, 0).is_undefined() {
            0.0
        } else {
            vm.to_number(arg(args, 0))?
        };
        let y = if arg(args, 1).is_undefined() {
            0.0
        } else {
            vm.to_number(arg(args, 1))?
        };
        let z = if arg(args, 2).is_undefined() {
            0.0
        } else {
            vm.to_number(arg(args, 2))?
        };
        let w = if arg(args, 3).is_undefined() {
            1.0
        } else {
            vm.to_number(arg(args, 3))?
        };
        Ok(JsValue::from_object(alloc_dom_point(
            vm, &h_dp, false, x, y, z, w,
        )))
    });
    for (ctor, proto, ro) in [(dpro_ctor, dpro, true), (dp_ctor, dp, false)] {
        let k_p = vm.heap.intern_key("prototype");
        let k_c = vm.heap.intern_key("constructor");
        vm.heap.object_define_data(
            ctor,
            k_p,
            JsValue::from_object(proto),
            PropertyAttrs::empty(),
            &mut vm.shapes,
        );
        vm.heap.object_define_data(
            proto,
            k_c,
            JsValue::from_object(ctor),
            PropertyAttrs::METHOD,
            &mut vm.shapes,
        );
        let hf = Rc::clone(host);
        define_host_method(vm, ctor, "fromPoint", move |vm, _, args| {
            let (x, y, z, w) = extract_point_init(vm, arg(args, 0))?;
            Ok(JsValue::from_object(alloc_dom_point(
                vm, &hf, ro, x, y, z, w,
            )))
        });
    }
    define_global(vm, "DOMPointReadOnly", JsValue::from_object(dpro_ctor));
    define_global(vm, "DOMPoint", JsValue::from_object(dp_ctor));

    // --- DOMRectReadOnly & DOMRect ---
    let drro = vm.alloc_ordinary_object();
    let dr = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(drro)));
    for (name, kind) in [("top", 0u8), ("right", 1), ("bottom", 2), ("left", 3)] {
        define_host_getter(vm, drro, name, move |vm, t, _| {
            let Some(oh) = t.as_object() else {
                return Err(vm.type_error("Illegal invocation"));
            };
            let x = read_opt_f64(vm, oh, "x", 0.0)?;
            let y = read_opt_f64(vm, oh, "y", 0.0)?;
            let w = read_opt_f64(vm, oh, "width", 0.0)?;
            let h = read_opt_f64(vm, oh, "height", 0.0)?;
            let v = match kind {
                0 => y.min(y + h),
                1 => x.max(x + w),
                2 => y.max(y + h),
                _ => x.min(x + w),
            };
            Ok(JsValue::from_f64(v))
        });
    }
    define_host_method(vm, drro, "toJSON", |vm, t, _| {
        let Some(oh) = t.as_object() else {
            return Err(vm.type_error("Illegal invocation"));
        };
        let x = read_opt_f64(vm, oh, "x", 0.0)?;
        let y = read_opt_f64(vm, oh, "y", 0.0)?;
        let w = read_opt_f64(vm, oh, "width", 0.0)?;
        let h = read_opt_f64(vm, oh, "height", 0.0)?;
        let o = vm.alloc_ordinary_object();
        set_f64_prop(vm, o, "x", x);
        set_f64_prop(vm, o, "y", y);
        set_f64_prop(vm, o, "width", w);
        set_f64_prop(vm, o, "height", h);
        set_f64_prop(vm, o, "top", y.min(y + h));
        set_f64_prop(vm, o, "right", x.max(x + w));
        set_f64_prop(vm, o, "bottom", y.max(y + h));
        set_f64_prop(vm, o, "left", x.min(x + w));
        Ok(JsValue::from_object(o))
    });
    let h_drro = Rc::clone(host);
    let drro_ctor = alloc_host_ctor(vm, "DOMRectReadOnly", move |vm, _, args| {
        let x = if arg(args, 0).is_undefined() {
            0.0
        } else {
            vm.to_number(arg(args, 0))?
        };
        let y = if arg(args, 1).is_undefined() {
            0.0
        } else {
            vm.to_number(arg(args, 1))?
        };
        let w = if arg(args, 2).is_undefined() {
            0.0
        } else {
            vm.to_number(arg(args, 2))?
        };
        let h = if arg(args, 3).is_undefined() {
            0.0
        } else {
            vm.to_number(arg(args, 3))?
        };
        Ok(JsValue::from_object(alloc_dom_rect_f64(
            vm, &h_drro, true, x, y, w, h,
        )))
    });
    let h_dr = Rc::clone(host);
    let dr_ctor = alloc_host_ctor(vm, "DOMRect", move |vm, _, args| {
        let x = if arg(args, 0).is_undefined() {
            0.0
        } else {
            vm.to_number(arg(args, 0))?
        };
        let y = if arg(args, 1).is_undefined() {
            0.0
        } else {
            vm.to_number(arg(args, 1))?
        };
        let w = if arg(args, 2).is_undefined() {
            0.0
        } else {
            vm.to_number(arg(args, 2))?
        };
        let h = if arg(args, 3).is_undefined() {
            0.0
        } else {
            vm.to_number(arg(args, 3))?
        };
        Ok(JsValue::from_object(alloc_dom_rect_f64(
            vm, &h_dr, false, x, y, w, h,
        )))
    });
    for (ctor, proto, ro) in [(drro_ctor, drro, true), (dr_ctor, dr, false)] {
        let k_p = vm.heap.intern_key("prototype");
        let k_c = vm.heap.intern_key("constructor");
        vm.heap.object_define_data(
            ctor,
            k_p,
            JsValue::from_object(proto),
            PropertyAttrs::empty(),
            &mut vm.shapes,
        );
        vm.heap.object_define_data(
            proto,
            k_c,
            JsValue::from_object(ctor),
            PropertyAttrs::METHOD,
            &mut vm.shapes,
        );
        let hf = Rc::clone(host);
        define_host_method(vm, ctor, "fromRect", move |vm, _, args| {
            let (x, y, w, h) = if let Some(oh) = arg(args, 0).as_object() {
                (
                    read_opt_f64(vm, oh, "x", 0.0)?,
                    read_opt_f64(vm, oh, "y", 0.0)?,
                    read_opt_f64(vm, oh, "width", 0.0)?,
                    read_opt_f64(vm, oh, "height", 0.0)?,
                )
            } else {
                (0.0, 0.0, 0.0, 0.0)
            };
            Ok(JsValue::from_object(alloc_dom_rect_f64(
                vm, &hf, ro, x, y, w, h,
            )))
        });
    }
    define_global(vm, "DOMRectReadOnly", JsValue::from_object(drro_ctor));
    define_global(vm, "DOMRect", JsValue::from_object(dr_ctor));

    // --- DOMRectList ---
    let drl_proto = vm.alloc_ordinary_object();
    define_host_method(vm, drl_proto, "item", |vm, t, args| {
        let Some(oh) = t.as_object() else {
            return Err(vm.type_error("Illegal invocation"));
        };
        let idx = vm.to_number(arg(args, 0)).unwrap_or(-1.0);
        if !idx.is_finite() || idx < 0.0 {
            return Ok(JsValue::NULL);
        }
        let k_len = vm.heap.intern_key("length");
        let len = vm
            .get_property_by_key(oh, k_len)
            .ok()
            .and_then(|v| vm.to_number(v).ok())
            .unwrap_or(0.0) as usize;
        let i = idx as usize;
        if i >= len {
            return Ok(JsValue::NULL);
        }
        let k = vm.heap.intern_key(&i.to_string());
        Ok(vm.get_property_by_key(oh, k).unwrap_or(JsValue::NULL))
    });
    // Attach Array.prototype[Symbol.iterator] so `for (const r of elem.getClientRects())` works.
    let iter_sym = vm.key_symbol_iterator;
    if let Ok(arr_iter) = vm.get_property_by_key(vm.array_proto, iter_sym) {
        vm.heap.object_define_data(
            drl_proto,
            iter_sym,
            arr_iter,
            PropertyAttrs::METHOD,
            &mut vm.shapes,
        );
    }
    let drl_ctor = vm.alloc_host_fn("DOMRectList", |vm, _, _| {
        Err(vm.type_error("Illegal constructor"))
    });
    let k_p = vm.heap.intern_key("prototype");
    let k_c = vm.heap.intern_key("constructor");
    vm.heap.object_define_data(
        drl_ctor,
        k_p,
        JsValue::from_object(drl_proto),
        PropertyAttrs::empty(),
        &mut vm.shapes,
    );
    vm.heap.object_define_data(
        drl_proto,
        k_c,
        JsValue::from_object(drl_ctor),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
    define_global(vm, "DOMRectList", JsValue::from_object(drl_ctor));

    // --- DOMQuad ---
    let dq_proto = vm.alloc_ordinary_object();
    let h_gb = Rc::clone(host);
    define_host_method(vm, dq_proto, "getBounds", move |vm, t, _| {
        let Some(oh) = t.as_object() else {
            return Err(vm.type_error("Illegal invocation"));
        };
        let mut min_x = f64::INFINITY;
        let mut min_y = f64::INFINITY;
        let mut max_x = f64::NEG_INFINITY;
        let mut max_y = f64::NEG_INFINITY;
        for name in ["p1", "p2", "p3", "p4"] {
            let k = vm.heap.intern_key(name);
            let pv = vm.get_property_by_key(oh, k)?;
            let (px, py, _, _) = extract_point_init(vm, pv)?;
            min_x = min_x.min(px);
            min_y = min_y.min(py);
            max_x = max_x.max(px);
            max_y = max_y.max(py);
        }
        Ok(JsValue::from_object(alloc_dom_rect_f64(
            vm,
            &h_gb,
            false,
            min_x,
            min_y,
            max_x - min_x,
            max_y - min_y,
        )))
    });
    define_host_method(vm, dq_proto, "toJSON", |vm, t, _| {
        let Some(oh) = t.as_object() else {
            return Err(vm.type_error("Illegal invocation"));
        };
        let out = vm.alloc_ordinary_object();
        for name in ["p1", "p2", "p3", "p4"] {
            let k = vm.heap.intern_key(name);
            let pv = vm.get_property_by_key(oh, k)?;
            vm.heap
                .object_define_data(out, k, pv, PropertyAttrs::DEFAULT, &mut vm.shapes);
        }
        Ok(JsValue::from_object(out))
    });
    let h_dq = Rc::clone(host);
    let dq_ctor = alloc_host_ctor(vm, "DOMQuad", move |vm, _, args| {
        let p1 = extract_point_init(vm, arg(args, 0))?;
        let p2 = if arg(args, 1).is_undefined() {
            (1.0, 0.0, 0.0, 1.0)
        } else {
            extract_point_init(vm, arg(args, 1))?
        };
        let p3 = if arg(args, 2).is_undefined() {
            (1.0, 1.0, 0.0, 1.0)
        } else {
            extract_point_init(vm, arg(args, 2))?
        };
        let p4 = if arg(args, 3).is_undefined() {
            (0.0, 1.0, 0.0, 1.0)
        } else {
            extract_point_init(vm, arg(args, 3))?
        };
        Ok(JsValue::from_object(alloc_dom_quad(
            vm,
            &h_dq,
            [p1, p2, p3, p4],
        )))
    });
    vm.heap.object_define_data(
        dq_ctor,
        k_p,
        JsValue::from_object(dq_proto),
        PropertyAttrs::empty(),
        &mut vm.shapes,
    );
    vm.heap.object_define_data(
        dq_proto,
        k_c,
        JsValue::from_object(dq_ctor),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
    let h_dq_fr = Rc::clone(host);
    define_host_method(vm, dq_ctor, "fromRect", move |vm, _, args| {
        let (x, y, w, h) = if let Some(oh) = arg(args, 0).as_object() {
            (
                read_opt_f64(vm, oh, "x", 0.0)?,
                read_opt_f64(vm, oh, "y", 0.0)?,
                read_opt_f64(vm, oh, "width", 0.0)?,
                read_opt_f64(vm, oh, "height", 0.0)?,
            )
        } else {
            (0.0, 0.0, 0.0, 0.0)
        };
        Ok(JsValue::from_object(alloc_dom_quad(
            vm,
            &h_dq_fr,
            [
                (x, y, 0.0, 1.0),
                (x + w, y, 0.0, 1.0),
                (x + w, y + h, 0.0, 1.0),
                (x, y + h, 0.0, 1.0),
            ],
        )))
    });
    let h_dq_fq = Rc::clone(host);
    define_host_method(vm, dq_ctor, "fromQuad", move |vm, _, args| {
        let Some(oh) = arg(args, 0).as_object() else {
            return Ok(JsValue::from_object(alloc_dom_quad(
                vm,
                &h_dq_fq,
                [(0.0, 0.0, 0.0, 1.0); 4],
            )));
        };
        let get_p = |vm: &mut JsVm, kname: &str| -> Result<(f64, f64, f64, f64), JsException> {
            let k = vm.heap.intern_key(kname);
            let v = vm.get_property_by_key(oh, k)?;
            extract_point_init(vm, v)
        };
        let pts = [
            get_p(vm, "p1")?,
            get_p(vm, "p2")?,
            get_p(vm, "p3")?,
            get_p(vm, "p4")?,
        ];
        Ok(JsValue::from_object(alloc_dom_quad(vm, &h_dq_fq, pts)))
    });
    define_global(vm, "DOMQuad", JsValue::from_object(dq_ctor));

    // --- DOMMatrixReadOnly & DOMMatrix ---
    let dmro = vm.alloc_ordinary_object();
    let dm = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(dmro)));
    for (alias, backing) in [
        ("a", "m11"),
        ("b", "m12"),
        ("c", "m21"),
        ("d", "m22"),
        ("e", "m41"),
        ("f", "m42"),
    ] {
        let b1 = backing;
        let g = vm.alloc_host_fn(alias, move |vm, t, _| {
            let Some(oh) = t.as_object() else {
                return Err(vm.type_error("Illegal invocation"));
            };
            let k = vm.heap.intern_key(b1);
            vm.get_property_by_key(oh, k)
        });
        let b2 = backing;
        let s = vm.alloc_host_fn(alias, move |vm, t, args| {
            let Some(oh) = t.as_object() else {
                return Err(vm.type_error("Illegal invocation"));
            };
            let n = vm.to_number(arg(args, 0))?;
            set_f64_prop(vm, oh, b2, n);
            Ok(JsValue::UNDEFINED)
        });
        define_host_accessor(vm, dmro, alias, Some(g), Some(s));
    }
    define_host_getter(vm, dmro, "isIdentity", |vm, t, _| {
        let Some(oh) = t.as_object() else {
            return Err(vm.type_error("Illegal invocation"));
        };
        let m = read_dom_matrix(vm, oh)?;
        Ok(JsValue::from_bool(mat4_is_identity(&m)))
    });
    let h_tp = Rc::clone(host);
    define_host_method(vm, dmro, "transformPoint", move |vm, t, args| {
        let Some(oh) = t.as_object() else {
            return Err(vm.type_error("Illegal invocation"));
        };
        let m = read_dom_matrix(vm, oh)?;
        let (x, y, z, w) = extract_point_init(vm, arg(args, 0))?;
        let (nx, ny, nz, nw) = mat4_transform_point(&m, x, y, z, w);
        Ok(JsValue::from_object(alloc_dom_point(
            vm, &h_tp, false, nx, ny, nz, nw,
        )))
    });
    let h_inv = Rc::clone(host);
    define_host_method(vm, dmro, "inverse", move |vm, t, _| {
        let Some(oh) = t.as_object() else {
            return Err(vm.type_error("Illegal invocation"));
        };
        let m = read_dom_matrix(vm, oh)?;
        let inv = mat4_invert(&m).unwrap_or([f64::NAN; 16]);
        Ok(JsValue::from_object(alloc_dom_matrix(
            vm, &h_inv, false, inv,
        )))
    });
    let h_mul = Rc::clone(host);
    define_host_method(vm, dmro, "multiply", move |vm, t, args| {
        let Some(oh) = t.as_object() else {
            return Err(vm.type_error("Illegal invocation"));
        };
        let a = read_dom_matrix(vm, oh)?;
        let b = if let Some(bh) = arg(args, 0).as_object() {
            read_dom_matrix(vm, bh)?
        } else {
            MAT4_ID
        };
        Ok(JsValue::from_object(alloc_dom_matrix(
            vm,
            &h_mul,
            false,
            mat4_mul(&a, &b),
        )))
    });
    let h_tr = Rc::clone(host);
    define_host_method(vm, dmro, "translate", move |vm, t, args| {
        let Some(oh) = t.as_object() else {
            return Err(vm.type_error("Illegal invocation"));
        };
        let a = read_dom_matrix(vm, oh)?;
        let tx = if arg(args, 0).is_undefined() {
            0.0
        } else {
            vm.to_number(arg(args, 0))?
        };
        let ty = if arg(args, 1).is_undefined() {
            0.0
        } else {
            vm.to_number(arg(args, 1))?
        };
        let tz = if arg(args, 2).is_undefined() {
            0.0
        } else {
            vm.to_number(arg(args, 2))?
        };
        Ok(JsValue::from_object(alloc_dom_matrix(
            vm,
            &h_tr,
            false,
            mat4_mul(&a, &mat4_translate(tx, ty, tz)),
        )))
    });
    let h_sc = Rc::clone(host);
    define_host_method(vm, dmro, "scale", move |vm, t, args| {
        let Some(oh) = t.as_object() else {
            return Err(vm.type_error("Illegal invocation"));
        };
        let a = read_dom_matrix(vm, oh)?;
        let sx = if arg(args, 0).is_undefined() {
            1.0
        } else {
            vm.to_number(arg(args, 0))?
        };
        let sy = if arg(args, 1).is_undefined() {
            sx
        } else {
            vm.to_number(arg(args, 1))?
        };
        let sz = if arg(args, 2).is_undefined() {
            1.0
        } else {
            vm.to_number(arg(args, 2))?
        };
        Ok(JsValue::from_object(alloc_dom_matrix(
            vm,
            &h_sc,
            false,
            mat4_mul(&a, &mat4_scale(sx, sy, sz)),
        )))
    });
    let h_rot = Rc::clone(host);
    define_host_method(vm, dmro, "rotate", move |vm, t, args| {
        let Some(oh) = t.as_object() else {
            return Err(vm.type_error("Illegal invocation"));
        };
        let a = read_dom_matrix(vm, oh)?;
        let deg = if arg(args, 0).is_undefined() {
            0.0
        } else {
            vm.to_number(arg(args, 0))?
        };
        Ok(JsValue::from_object(alloc_dom_matrix(
            vm,
            &h_rot,
            false,
            mat4_mul(&a, &mat4_rotate_z(deg)),
        )))
    });

    let parse_matrix_init = |vm: &mut JsVm, init: JsValue| -> Result<[f64; 16], JsException> {
        if init.is_undefined() {
            return Ok(MAT4_ID);
        }
        if let Some(oh) = init.as_object() {
            let k_len = vm.heap.intern_key("length");
            if let Ok(lv) = vm.get_property_by_key(oh, k_len) {
                if !lv.is_undefined() {
                    let len = vm.to_number(lv)? as usize;
                    let mut vals = Vec::with_capacity(len);
                    for i in 0..len {
                        let k = vm.heap.intern_key(&i.to_string());
                        let v = vm.get_property_by_key(oh, k)?;
                        vals.push(vm.to_number(v)?);
                    }
                    if vals.len() == 6 {
                        return Ok([
                            vals[0], vals[1], 0.0, 0.0, vals[2], vals[3], 0.0, 0.0, 0.0, 0.0, 1.0,
                            0.0, vals[4], vals[5], 0.0, 1.0,
                        ]);
                    }
                    if vals.len() == 16 {
                        let mut arr = MAT4_ID;
                        arr.copy_from_slice(&vals);
                        return Ok(arr);
                    }
                    return Err(vm.type_error("DOMMatrix sequence must have 6 or 16 elements"));
                }
            }
            return read_dom_matrix(vm, oh);
        }
        Ok(MAT4_ID)
    };
    let h_dmro = Rc::clone(host);
    let dmro_ctor = alloc_host_ctor(vm, "DOMMatrixReadOnly", move |vm, _, args| {
        let m = parse_matrix_init(vm, arg(args, 0))?;
        Ok(JsValue::from_object(alloc_dom_matrix(vm, &h_dmro, true, m)))
    });
    let h_dm = Rc::clone(host);
    let dm_ctor = alloc_host_ctor(vm, "DOMMatrix", move |vm, _, args| {
        let m = parse_matrix_init(vm, arg(args, 0))?;
        Ok(JsValue::from_object(alloc_dom_matrix(vm, &h_dm, false, m)))
    });
    for (ctor, proto, ro) in [(dmro_ctor, dmro, true), (dm_ctor, dm, false)] {
        vm.heap.object_define_data(
            ctor,
            k_p,
            JsValue::from_object(proto),
            PropertyAttrs::empty(),
            &mut vm.shapes,
        );
        vm.heap.object_define_data(
            proto,
            k_c,
            JsValue::from_object(ctor),
            PropertyAttrs::METHOD,
            &mut vm.shapes,
        );
        let hf = Rc::clone(host);
        define_host_method(vm, ctor, "fromMatrix", move |vm, _, args| {
            let m = if let Some(oh) = arg(args, 0).as_object() {
                read_dom_matrix(vm, oh)?
            } else {
                MAT4_ID
            };
            Ok(JsValue::from_object(alloc_dom_matrix(vm, &hf, ro, m)))
        });
    }
    define_global(vm, "DOMMatrixReadOnly", JsValue::from_object(dmro_ctor));
    define_global(vm, "DOMMatrix", JsValue::from_object(dm_ctor));

    // Store prototypes in CssomViewState for fast object allocation & GC rooting.
    let st = host.borrow();
    let mut cv = st.cssom_geometry_state.borrow_mut();
    cv.dom_point_readonly_proto = Some(dpro);
    cv.dom_point_proto = Some(dp);
    cv.dom_rect_readonly_proto = Some(drro);
    cv.dom_rect_proto = Some(dr);
    cv.dom_rect_list_proto = Some(drl_proto);
    cv.dom_quad_proto = Some(dq_proto);
    cv.dom_matrix_readonly_proto = Some(dmro);
    cv.dom_matrix_proto = Some(dm);
}

// ---------------------------------------------------------------------------
// Element.getClientRects & Range.{getClientRects, getBoundingClientRect}
// ---------------------------------------------------------------------------

fn utf16_slice_char_bounds(text: &str, start_u16: usize, end_u16: usize) -> (usize, usize) {
    let mut u16_pos = 0usize;
    let mut s_byte = text.len();
    let mut e_byte = text.len();
    for (bi, ch) in text.char_indices() {
        if u16_pos == start_u16 {
            s_byte = bi;
        }
        if u16_pos == end_u16 {
            e_byte = bi;
            break;
        }
        u16_pos += ch.len_utf16();
    }
    if start_u16 >= u16_pos {
        s_byte = text.len();
    }
    if end_u16 >= u16_pos {
        e_byte = text.len();
    }
    if s_byte > e_byte {
        (e_byte, e_byte)
    } else {
        (s_byte, e_byte)
    }
}

/// Computes visual width weight of `s`, omitting soft hyphens (`\u{00AD}`) when not at a line break.
fn visible_char_count(s: &str, keep_trailing_shy: bool) -> usize {
    let chars: Vec<char> = s.chars().collect();
    let mut count = 0usize;
    for (i, &c) in chars.iter().enumerate() {
        if c == '\u{00AD}' {
            if keep_trailing_shy && i + 1 == chars.len() {
                count += 1;
            }
        } else {
            count += 1;
        }
    }
    count
}

fn text_subrange_rects(
    host: &SharedHost,
    text_nid: NodeId,
    start_u16: usize,
    end_u16: usize,
) -> Vec<Rect> {
    let boxes = collect_boxes_for_node(host, text_nid);
    if boxes.is_empty() {
        return Vec::new();
    }
    let full_text = {
        let hb = host.borrow();
        let doc = hb.document.borrow();
        match doc.get(text_nid) {
            Some(NodeData::Text(t)) => t.clone(),
            _ => String::new(),
        }
    };
    let total_u16: usize = full_text.encode_utf16().count();
    let s_u16 = start_u16.min(total_u16);
    let e_u16 = end_u16.min(total_u16).max(s_u16);
    let multi_line = boxes.len() > 1;
    let mut rects = Vec::new();
    let mut cursor_u16 = 0usize;

    for (bi, bx) in boxes.iter().enumerate() {
        let run_text = bx.text.as_deref().unwrap_or(&full_text);
        let run_u16: usize = run_text.encode_utf16().count();
        let run_start = cursor_u16;
        let run_end = run_start + run_u16;
        cursor_u16 = run_end;
        if e_u16 < run_start || s_u16 > run_end {
            continue;
        }
        let loc_s = s_u16.saturating_sub(run_start).min(run_u16);
        let loc_e = e_u16.saturating_sub(run_start).min(run_u16);
        let aabb = bx.aabb_in_viewport(CssBoxType::Border);
        let keep_end_shy = multi_line && bi + 1 < boxes.len();
        let total_vis = visible_char_count(run_text, keep_end_shy);
        if total_vis == 0 {
            if s_u16 == e_u16 && loc_s == loc_e {
                rects.push(Rect::new(
                    aabb.origin.x,
                    aabb.origin.y,
                    0.0,
                    aabb.size.height,
                ));
            }
            continue;
        }
        let (sb, eb) = utf16_slice_char_bounds(run_text, loc_s, loc_e);
        let pre_vis = visible_char_count(&run_text[..sb], false);
        let slice_is_end_of_line = keep_end_shy && eb == run_text.len();
        let sel_vis = visible_char_count(&run_text[sb..eb], slice_is_end_of_line);
        let x0 = aabb.origin.x + aabb.size.width * (pre_vis as f32 / total_vis as f32);
        let w = aabb.size.width * (sel_vis as f32 / total_vis as f32);
        rects.push(Rect::new(x0, aabb.origin.y, w, aabb.size.height));
    }
    rects
}

pub(crate) fn range_client_rects(
    host: &SharedHost,
    range: crate::selection::RangeData,
) -> Vec<Rect> {
    let connected = {
        let hb = host.borrow();
        let doc = hb.document.borrow();
        doc.is_connected(range.start_container)
            && doc.is_connected(range.end_container)
            && root_of_node(&doc, range.start_container) == root_of_node(&doc, range.end_container)
    };
    if !connected {
        return Vec::new();
    }
    if range.start_container == range.end_container {
        let is_text = matches!(
            host.borrow().document.borrow().get(range.start_container),
            Some(NodeData::Text(_))
        );
        if is_text {
            return text_subrange_rects(
                host,
                range.start_container,
                range.start_offset,
                range.end_offset,
            );
        }
        let children: Vec<NodeId> = host
            .borrow()
            .document
            .borrow()
            .children(range.start_container)
            .collect();
        if range.start_offset == range.end_offset {
            if let Some(&ch) = children.get(range.start_offset) {
                let r = transformed_bounding_rect_for_node(host, ch);
                if r.size.width > 0.0 || r.size.height > 0.0 {
                    return vec![Rect::new(r.origin.x, r.origin.y, 0.0, r.size.height)];
                }
            }
            return Vec::new();
        }
        let mut rects = Vec::new();
        for &ch in children
            .iter()
            .skip(range.start_offset)
            .take(range.end_offset.saturating_sub(range.start_offset))
        {
            for b in collect_boxes_for_node(host, ch) {
                rects.push(b.aabb_in_viewport(CssBoxType::Border));
            }
        }
        return rects;
    }

    // General multi-node range: collect all nodes in document order between start and end.
    let nodes: Vec<(NodeId, bool)> = {
        let hb = host.borrow();
        let doc = hb.document.borrow();
        let root = root_of_node(&doc, range.start_container);
        let mut out = Vec::new();
        let mut inside = false;
        for n in doc.descendants(root) {
            if n == range.start_container {
                inside = true;
            }
            if inside {
                let is_text = matches!(doc.get(n), Some(NodeData::Text(_)));
                out.push((n, is_text));
            }
            if n == range.end_container {
                break;
            }
        }
        out
    };
    let mut rects = Vec::new();
    for (n, is_text) in nodes {
        if n == range.start_container && is_text {
            rects.extend(text_subrange_rects(host, n, range.start_offset, usize::MAX));
        } else if n == range.end_container && is_text {
            rects.extend(text_subrange_rects(host, n, 0, range.end_offset));
        } else if is_text {
            rects.extend(text_subrange_rects(host, n, 0, usize::MAX));
        } else if n != range.start_container && n != range.end_container {
            for b in collect_boxes_for_node(host, n) {
                rects.push(b.aabb_in_viewport(CssBoxType::Border));
            }
        }
    }
    rects
}

// ---------------------------------------------------------------------------
// GeometryUtils (`getBoxQuads`, `convert*FromNode`)
// ---------------------------------------------------------------------------

fn parse_box_option(
    vm: &mut JsVm,
    opts_val: JsValue,
    key: &str,
) -> Result<CssBoxType, JsException> {
    let Some(oh) = opts_val.as_object() else {
        return Ok(CssBoxType::Border);
    };
    let k = vm.heap.intern_key(key);
    let v = vm.get_property_by_key(oh, k)?;
    if v.is_undefined() {
        return Ok(CssBoxType::Border);
    }
    let s = vm.value_to_rust_string(v);
    CssBoxType::parse(&s).ok_or_else(|| vm.type_error(format!("Invalid CSSBoxType '{s}'")))
}

fn parse_relative_to(
    vm: &mut JsVm,
    host: &SharedHost,
    this_nid: NodeId,
    opts_val: JsValue,
) -> Result<NodeId, JsException> {
    if let Some(oh) = opts_val.as_object() {
        let k = vm.heap.intern_key("relativeTo");
        let v = vm.get_property_by_key(oh, k)?;
        if !v.is_undefined() && !v.is_null() {
            return require_node(vm, host, v);
        }
    }
    let hb = host.borrow();
    if matches!(hb.document.borrow().get(this_nid), Some(NodeData::Document)) {
        Ok(this_nid)
    } else {
        Ok(hb.owner_doc_of(this_nid))
    }
}

fn box_local_to_viewport_mat(b: &FragmentBox, box_type: CssBoxType) -> [f64; 16] {
    let r = b.local_rect(box_type);
    mat4_mul(
        &b.to_viewport,
        &mat4_translate(f64::from(r.origin.x), f64::from(r.origin.y), 0.0),
    )
}

pub(crate) fn install_geometry_utils_on(vm: &mut JsVm, host: &SharedHost, proto: HeapHandle) {
    // getBoxQuads(options?)
    let h = Rc::clone(host);
    define_host_method(vm, proto, "getBoxQuads", move |vm, t, args| {
        let this_nid = require_node(vm, &h, t)?;
        let opts = arg(args, 0);
        let box_type = parse_box_option(vm, opts, "box")?;
        let rel_nid = parse_relative_to(vm, &h, this_nid, opts)?;
        let src_boxes = collect_boxes_for_node(&h, this_nid);
        if src_boxes.is_empty() {
            return Ok(JsValue::from_object(vm.alloc_array()));
        }
        let rel_boxes = collect_boxes_for_node(&h, rel_nid);
        let Some(rel_box) = rel_boxes.first() else {
            return Err(throw_dom_exception(
                vm,
                "NotFoundError",
                "relativeTo node has no CSS box",
            ));
        };
        let Some(rel_inv) = mat4_invert(&rel_box.to_viewport) else {
            return Ok(JsValue::from_object(vm.alloc_array()));
        };
        let mut quads = Vec::with_capacity(src_boxes.len());
        for sb in &src_boxes {
            let vp_pts = sb.quad_in_viewport(box_type);
            let mut out_pts = [(0.0, 0.0, 0.0, 1.0); 4];
            for (i, (vx, vy)) in vp_pts.into_iter().enumerate() {
                let (rx, ry) = mat4_project_2d(&rel_inv, vx, vy);
                out_pts[i] = (rx, ry, 0.0, 1.0);
            }
            quads.push(JsValue::from_object(alloc_dom_quad(vm, &h, out_pts)));
        }
        Ok(JsValue::from_object(alloc_array_from(vm, quads)))
    });

    // convertPointFromNode(point, from, options?)
    let h = Rc::clone(host);
    define_host_method(vm, proto, "convertPointFromNode", move |vm, t, args| {
        let to_nid = require_node(vm, &h, t)?;
        let (px, py, pz, pw) = extract_point_init(vm, arg(args, 0))?;
        let from_nid = require_node(vm, &h, arg(args, 1))?;
        let opts = arg(args, 2);
        let from_box = parse_box_option(vm, opts, "fromBox")?;
        let to_box = parse_box_option(vm, opts, "toBox")?;
        let from_boxes = collect_boxes_for_node(&h, from_nid);
        let to_boxes = collect_boxes_for_node(&h, to_nid);
        let (Some(fb), Some(tb)) = (from_boxes.first(), to_boxes.first()) else {
            return Err(throw_dom_exception(
                vm,
                "NotFoundError",
                "Node has no CSS box",
            ));
        };
        let from_m = box_local_to_viewport_mat(fb, from_box);
        let to_m = box_local_to_viewport_mat(tb, to_box);
        let Some(to_inv) = mat4_invert(&to_m) else {
            return Err(throw_dom_exception(
                vm,
                "NotFoundError",
                "Target transform is singular",
            ));
        };
        let xform = mat4_mul(&to_inv, &from_m);
        let (nx, ny) = mat4_project_2d(&xform, px, py);
        Ok(JsValue::from_object(alloc_dom_point(
            vm, &h, false, nx, ny, pz, pw,
        )))
    });

    // convertRectFromNode(rect, from, options?)
    let h = Rc::clone(host);
    define_host_method(vm, proto, "convertRectFromNode", move |vm, t, args| {
        let to_nid = require_node(vm, &h, t)?;
        let rect_v = arg(args, 0);
        let Some(roh) = rect_v.as_object() else {
            return Err(vm.type_error("convertRectFromNode requires a DOMRectReadOnly"));
        };
        let rx = read_opt_f64(vm, roh, "x", 0.0)?;
        let ry = read_opt_f64(vm, roh, "y", 0.0)?;
        let rw = read_opt_f64(vm, roh, "width", 0.0)?;
        let rh = read_opt_f64(vm, roh, "height", 0.0)?;
        let from_nid = require_node(vm, &h, arg(args, 1))?;
        let opts = arg(args, 2);
        let from_box = parse_box_option(vm, opts, "fromBox")?;
        let to_box = parse_box_option(vm, opts, "toBox")?;
        let from_boxes = collect_boxes_for_node(&h, from_nid);
        let to_boxes = collect_boxes_for_node(&h, to_nid);
        let (Some(fb), Some(tb)) = (from_boxes.first(), to_boxes.first()) else {
            return Err(throw_dom_exception(
                vm,
                "NotFoundError",
                "Node has no CSS box",
            ));
        };
        let from_m = box_local_to_viewport_mat(fb, from_box);
        let to_m = box_local_to_viewport_mat(tb, to_box);
        let Some(to_inv) = mat4_invert(&to_m) else {
            return Err(throw_dom_exception(
                vm,
                "NotFoundError",
                "Target transform is singular",
            ));
        };
        let xform = mat4_mul(&to_inv, &from_m);
        let corners = [(rx, ry), (rx + rw, ry), (rx + rw, ry + rh), (rx, ry + rh)];
        let mut pts = [(0.0, 0.0, 0.0, 1.0); 4];
        for (i, (cx, cy)) in corners.into_iter().enumerate() {
            let (nx, ny) = mat4_project_2d(&xform, cx, cy);
            pts[i] = (nx, ny, 0.0, 1.0);
        }
        Ok(JsValue::from_object(alloc_dom_quad(vm, &h, pts)))
    });

    // convertQuadFromNode(quad, from, options?)
    let h = Rc::clone(host);
    define_host_method(vm, proto, "convertQuadFromNode", move |vm, t, args| {
        let to_nid = require_node(vm, &h, t)?;
        let qv = arg(args, 0);
        let Some(qoh) = qv.as_object() else {
            return Err(vm.type_error("convertQuadFromNode requires a DOMQuadInit"));
        };
        let mut src_pts = [(0.0, 0.0, 0.0, 1.0); 4];
        for (i, name) in ["p1", "p2", "p3", "p4"].into_iter().enumerate() {
            let k = vm.heap.intern_key(name);
            let pv = vm.get_property_by_key(qoh, k)?;
            src_pts[i] = extract_point_init(vm, pv)?;
        }
        let from_nid = require_node(vm, &h, arg(args, 1))?;
        let opts = arg(args, 2);
        let from_box = parse_box_option(vm, opts, "fromBox")?;
        let to_box = parse_box_option(vm, opts, "toBox")?;
        let from_boxes = collect_boxes_for_node(&h, from_nid);
        let to_boxes = collect_boxes_for_node(&h, to_nid);
        let (Some(fb), Some(tb)) = (from_boxes.first(), to_boxes.first()) else {
            return Err(throw_dom_exception(
                vm,
                "NotFoundError",
                "Node has no CSS box",
            ));
        };
        let from_m = box_local_to_viewport_mat(fb, from_box);
        let to_m = box_local_to_viewport_mat(tb, to_box);
        let Some(to_inv) = mat4_invert(&to_m) else {
            return Err(throw_dom_exception(
                vm,
                "NotFoundError",
                "Target transform is singular",
            ));
        };
        let xform = mat4_mul(&to_inv, &from_m);
        let mut dst_pts = [(0.0, 0.0, 0.0, 1.0); 4];
        for (i, (cx, cy, cz, cw)) in src_pts.into_iter().enumerate() {
            let (nx, ny) = mat4_project_2d(&xform, cx, cy);
            dst_pts[i] = (nx, ny, cz, cw);
        }
        Ok(JsValue::from_object(alloc_dom_quad(vm, &h, dst_pts)))
    });
}

/// Installs all W3C Geometry Interfaces L1 constructors and `GeometryUtils` / `getClientRects`
/// methods on `Element`, `Text`, `Document`, and `Range`.
pub fn install_cssom_geometry(vm: &mut JsVm, host: &SharedHost) {
    install_geometry_interfaces(vm, host);
    let (ep, tp, dp, rp) = {
        let hb = host.borrow();
        (
            hb.prototypes.element_proto,
            hb.prototypes.text_proto,
            hb.prototypes.document_proto,
            hb.selection.range_proto,
        )
    };
    install_geometry_utils_on(vm, host, ep);
    install_geometry_utils_on(vm, host, tp);
    install_geometry_utils_on(vm, host, dp);

    let h = Rc::clone(host);
    define_host_method(vm, ep, "getClientRects", move |vm, t, _| {
        let nid = require_node(vm, &h, t)?;
        let boxes = collect_boxes_for_node(&h, nid);
        let rects: Vec<Rect> = boxes
            .iter()
            .map(|b| b.aabb_in_viewport(CssBoxType::Border))
            .collect();
        Ok(JsValue::from_object(alloc_dom_rect_list(vm, &h, &rects)))
    });

    if let Some(range_proto) = rp {
        let h = Rc::clone(host);
        define_host_method(vm, range_proto, "getClientRects", move |vm, t, _| {
            let Some(oh) = t.as_object() else {
                return Err(vm.type_error("Illegal invocation"));
            };
            let Some(rd) = h.borrow().selection.ranges.get(&oh).copied() else {
                return Err(vm.type_error("Illegal invocation"));
            };
            let rects = range_client_rects(&h, rd);
            Ok(JsValue::from_object(alloc_dom_rect_list(vm, &h, &rects)))
        });
        let h = Rc::clone(host);
        define_host_method(vm, range_proto, "getBoundingClientRect", move |vm, t, _| {
            let Some(oh) = t.as_object() else {
                return Err(vm.type_error("Illegal invocation"));
            };
            let Some(rd) = h.borrow().selection.ranges.get(&oh).copied() else {
                return Err(vm.type_error("Illegal invocation"));
            };
            let rects = range_client_rects(&h, rd);
            if rects.is_empty() {
                return Ok(JsValue::from_object(alloc_dom_rect(vm, &h, Rect::ZERO)));
            }
            let mut min_x = f32::INFINITY;
            let mut min_y = f32::INFINITY;
            let mut max_x = f32::NEG_INFINITY;
            let mut max_y = f32::NEG_INFINITY;
            for r in &rects {
                min_x = min_x.min(r.origin.x);
                min_y = min_y.min(r.origin.y);
                max_x = max_x.max(r.max_x());
                max_y = max_y.max(r.max_y());
            }
            let union = Rect::new(
                min_x,
                min_y,
                (max_x - min_x).max(0.0),
                (max_y - min_y).max(0.0),
            );
            Ok(JsValue::from_object(alloc_dom_rect(vm, &h, union)))
        });
    }
}
