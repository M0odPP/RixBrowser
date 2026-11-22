//! Standard WebGL1 / WebGL2 numeric constants and pure mapping helpers from
//! OpenGL / WebGL enums to `wgpu` pipeline, texture, and blend descriptors.
//!
//! Reference: Spec 06 §3, Issue #26.

use vello::wgpu;

pub const GL_NO_ERROR: u32 = 0;
pub const GL_INVALID_ENUM: u32 = 0x0500;
pub const GL_INVALID_VALUE: u32 = 0x0501;
pub const GL_INVALID_OPERATION: u32 = 0x0502;

pub const GL_DEPTH_BUFFER_BIT: u32 = 0x0100;
pub const GL_STENCIL_BUFFER_BIT: u32 = 0x0400;
pub const GL_COLOR_BUFFER_BIT: u32 = 0x4000;

pub const GL_POINTS: u32 = 0x0000;
pub const GL_LINES: u32 = 0x0001;
pub const GL_LINE_LOOP: u32 = 0x0002;
pub const GL_LINE_STRIP: u32 = 0x0003;
pub const GL_TRIANGLES: u32 = 0x0004;
pub const GL_TRIANGLE_STRIP: u32 = 0x0005;
pub const GL_TRIANGLE_FAN: u32 = 0x0006;

pub const GL_ZERO: u32 = 0;
pub const GL_ONE: u32 = 1;
pub const GL_SRC_COLOR: u32 = 0x0300;
pub const GL_ONE_MINUS_SRC_COLOR: u32 = 0x0301;
pub const GL_SRC_ALPHA: u32 = 0x0302;
pub const GL_ONE_MINUS_SRC_ALPHA: u32 = 0x0303;
pub const GL_DST_ALPHA: u32 = 0x0304;
pub const GL_ONE_MINUS_DST_ALPHA: u32 = 0x0305;
pub const GL_DST_COLOR: u32 = 0x0306;
pub const GL_ONE_MINUS_DST_COLOR: u32 = 0x0307;
pub const GL_SRC_ALPHA_SATURATE: u32 = 0x0308;

pub const GL_FUNC_ADD: u32 = 0x8006;
pub const GL_FUNC_SUBTRACT: u32 = 0x800A;
pub const GL_FUNC_REVERSE_SUBTRACT: u32 = 0x800B;
pub const GL_MIN: u32 = 0x8007;
pub const GL_MAX: u32 = 0x8008;

pub const GL_FRONT: u32 = 0x0404;
pub const GL_BACK: u32 = 0x0405;
pub const GL_FRONT_AND_BACK: u32 = 0x0408;
pub const GL_CW: u32 = 0x0900;
pub const GL_CCW: u32 = 0x0901;

pub const GL_CULL_FACE: u32 = 0x0B44;
pub const GL_DEPTH_TEST: u32 = 0x0B71;
pub const GL_STENCIL_TEST: u32 = 0x0B90;
pub const GL_BLEND: u32 = 0x0BE2;
pub const GL_SCISSOR_TEST: u32 = 0x0C11;

pub const GL_NEVER: u32 = 0x0200;
pub const GL_LESS: u32 = 0x0201;
pub const GL_EQUAL: u32 = 0x0202;
pub const GL_LEQUAL: u32 = 0x0203;
pub const GL_GREATER: u32 = 0x0204;
pub const GL_NOTEQUAL: u32 = 0x0205;
pub const GL_GEQUAL: u32 = 0x0206;
pub const GL_ALWAYS: u32 = 0x0207;

pub const GL_BYTE: u32 = 0x1400;
pub const GL_UNSIGNED_BYTE: u32 = 0x1401;
pub const GL_SHORT: u32 = 0x1402;
pub const GL_UNSIGNED_SHORT: u32 = 0x1403;
pub const GL_INT: u32 = 0x1404;
pub const GL_UNSIGNED_INT: u32 = 0x1405;
pub const GL_FLOAT: u32 = 0x1406;

pub const GL_ARRAY_BUFFER: u32 = 0x8892;
pub const GL_ELEMENT_ARRAY_BUFFER: u32 = 0x8893;
pub const GL_STATIC_DRAW: u32 = 0x88E4;
pub const GL_DYNAMIC_DRAW: u32 = 0x88E8;
pub const GL_STREAM_DRAW: u32 = 0x88E0;

pub const GL_FRAGMENT_SHADER: u32 = 0x8B30;
pub const GL_VERTEX_SHADER: u32 = 0x8B31;
pub const GL_COMPILE_STATUS: u32 = 0x8B81;
pub const GL_LINK_STATUS: u32 = 0x8B82;
pub const GL_VALIDATE_STATUS: u32 = 0x8B83;

pub const GL_TEXTURE_2D: u32 = 0x0DE1;
pub const GL_TEXTURE0: u32 = 0x84C0;
pub const GL_NEAREST: u32 = 0x2600;
pub const GL_LINEAR: u32 = 0x2601;
pub const GL_NEAREST_MIPMAP_NEAREST: u32 = 0x2700;
pub const GL_LINEAR_MIPMAP_NEAREST: u32 = 0x2701;
pub const GL_NEAREST_MIPMAP_LINEAR: u32 = 0x2702;
pub const GL_LINEAR_MIPMAP_LINEAR: u32 = 0x2703;
pub const GL_TEXTURE_MAG_FILTER: u32 = 0x2800;
pub const GL_TEXTURE_MIN_FILTER: u32 = 0x2801;
pub const GL_TEXTURE_WRAP_S: u32 = 0x2802;
pub const GL_TEXTURE_WRAP_T: u32 = 0x2803;
pub const GL_REPEAT: u32 = 0x2901;
pub const GL_CLAMP_TO_EDGE: u32 = 0x812F;
pub const GL_MIRRORED_REPEAT: u32 = 0x8370;

pub const GL_ALPHA: u32 = 0x1906;
pub const GL_RGB: u32 = 0x1907;
pub const GL_RGBA: u32 = 0x1908;
pub const GL_LUMINANCE: u32 = 0x1909;
pub const GL_LUMINANCE_ALPHA: u32 = 0x190A;
pub const GL_RGBA8: u32 = 0x8058;

pub const GL_UNPACK_FLIP_Y_WEBGL: u32 = 0x9240;
pub const GL_UNPACK_PREMULTIPLY_ALPHA_WEBGL: u32 = 0x9241;
pub const GL_VENDOR: u32 = 0x1F00;
pub const GL_RENDERER: u32 = 0x1F01;
pub const GL_VERSION: u32 = 0x1F02;
pub const GL_SHADING_LANGUAGE_VERSION: u32 = 0x8B8C;
pub const GL_MAX_TEXTURE_SIZE: u32 = 0x0D33;
pub const GL_MAX_VERTEX_ATTRIBS: u32 = 0x8869;

pub const WEBGL_CONSTANTS: &[(&str, u32)] = &[
    ("NO_ERROR", GL_NO_ERROR),
    ("INVALID_ENUM", GL_INVALID_ENUM),
    ("INVALID_VALUE", GL_INVALID_VALUE),
    ("INVALID_OPERATION", GL_INVALID_OPERATION),
    ("DEPTH_BUFFER_BIT", GL_DEPTH_BUFFER_BIT),
    ("STENCIL_BUFFER_BIT", GL_STENCIL_BUFFER_BIT),
    ("COLOR_BUFFER_BIT", GL_COLOR_BUFFER_BIT),
    ("POINTS", GL_POINTS),
    ("LINES", GL_LINES),
    ("LINE_LOOP", GL_LINE_LOOP),
    ("LINE_STRIP", GL_LINE_STRIP),
    ("TRIANGLES", GL_TRIANGLES),
    ("TRIANGLE_STRIP", GL_TRIANGLE_STRIP),
    ("TRIANGLE_FAN", GL_TRIANGLE_FAN),
    ("ZERO", GL_ZERO),
    ("ONE", GL_ONE),
    ("SRC_COLOR", GL_SRC_COLOR),
    ("ONE_MINUS_SRC_COLOR", GL_ONE_MINUS_SRC_COLOR),
    ("SRC_ALPHA", GL_SRC_ALPHA),
    ("ONE_MINUS_SRC_ALPHA", GL_ONE_MINUS_SRC_ALPHA),
    ("DST_ALPHA", GL_DST_ALPHA),
    ("ONE_MINUS_DST_ALPHA", GL_ONE_MINUS_DST_ALPHA),
    ("DST_COLOR", GL_DST_COLOR),
    ("ONE_MINUS_DST_COLOR", GL_ONE_MINUS_DST_COLOR),
    ("SRC_ALPHA_SATURATE", GL_SRC_ALPHA_SATURATE),
    ("FUNC_ADD", GL_FUNC_ADD),
    ("FUNC_SUBTRACT", GL_FUNC_SUBTRACT),
    ("FUNC_REVERSE_SUBTRACT", GL_FUNC_REVERSE_SUBTRACT),
    ("MIN", GL_MIN),
    ("MAX", GL_MAX),
    ("FRONT", GL_FRONT),
    ("BACK", GL_BACK),
    ("FRONT_AND_BACK", GL_FRONT_AND_BACK),
    ("CW", GL_CW),
    ("CCW", GL_CCW),
    ("CULL_FACE", GL_CULL_FACE),
    ("DEPTH_TEST", GL_DEPTH_TEST),
    ("STENCIL_TEST", GL_STENCIL_TEST),
    ("BLEND", GL_BLEND),
    ("SCISSOR_TEST", GL_SCISSOR_TEST),
    ("NEVER", GL_NEVER),
    ("LESS", GL_LESS),
    ("EQUAL", GL_EQUAL),
    ("LEQUAL", GL_LEQUAL),
    ("GREATER", GL_GREATER),
    ("NOTEQUAL", GL_NOTEQUAL),
    ("GEQUAL", GL_GEQUAL),
    ("ALWAYS", GL_ALWAYS),
    ("BYTE", GL_BYTE),
    ("UNSIGNED_BYTE", GL_UNSIGNED_BYTE),
    ("SHORT", GL_SHORT),
    ("UNSIGNED_SHORT", GL_UNSIGNED_SHORT),
    ("INT", GL_INT),
    ("UNSIGNED_INT", GL_UNSIGNED_INT),
    ("FLOAT", GL_FLOAT),
    ("ARRAY_BUFFER", GL_ARRAY_BUFFER),
    ("ELEMENT_ARRAY_BUFFER", GL_ELEMENT_ARRAY_BUFFER),
    ("STATIC_DRAW", GL_STATIC_DRAW),
    ("DYNAMIC_DRAW", GL_DYNAMIC_DRAW),
    ("STREAM_DRAW", GL_STREAM_DRAW),
    ("FRAGMENT_SHADER", GL_FRAGMENT_SHADER),
    ("VERTEX_SHADER", GL_VERTEX_SHADER),
    ("COMPILE_STATUS", GL_COMPILE_STATUS),
    ("LINK_STATUS", GL_LINK_STATUS),
    ("VALIDATE_STATUS", GL_VALIDATE_STATUS),
    ("TEXTURE_2D", GL_TEXTURE_2D),
    ("TEXTURE0", GL_TEXTURE0),
    ("TEXTURE1", GL_TEXTURE0 + 1),
    ("TEXTURE2", GL_TEXTURE0 + 2),
    ("TEXTURE3", GL_TEXTURE0 + 3),
    ("TEXTURE4", GL_TEXTURE0 + 4),
    ("TEXTURE5", GL_TEXTURE0 + 5),
    ("TEXTURE6", GL_TEXTURE0 + 6),
    ("TEXTURE7", GL_TEXTURE0 + 7),
    ("NEAREST", GL_NEAREST),
    ("LINEAR", GL_LINEAR),
    ("NEAREST_MIPMAP_NEAREST", GL_NEAREST_MIPMAP_NEAREST),
    ("LINEAR_MIPMAP_NEAREST", GL_LINEAR_MIPMAP_NEAREST),
    ("NEAREST_MIPMAP_LINEAR", GL_NEAREST_MIPMAP_LINEAR),
    ("LINEAR_MIPMAP_LINEAR", GL_LINEAR_MIPMAP_LINEAR),
    ("TEXTURE_MAG_FILTER", GL_TEXTURE_MAG_FILTER),
    ("TEXTURE_MIN_FILTER", GL_TEXTURE_MIN_FILTER),
    ("TEXTURE_WRAP_S", GL_TEXTURE_WRAP_S),
    ("TEXTURE_WRAP_T", GL_TEXTURE_WRAP_T),
    ("REPEAT", GL_REPEAT),
    ("CLAMP_TO_EDGE", GL_CLAMP_TO_EDGE),
    ("MIRRORED_REPEAT", GL_MIRRORED_REPEAT),
    ("ALPHA", GL_ALPHA),
    ("RGB", GL_RGB),
    ("RGBA", GL_RGBA),
    ("LUMINANCE", GL_LUMINANCE),
    ("LUMINANCE_ALPHA", GL_LUMINANCE_ALPHA),
    ("RGBA8", GL_RGBA8),
    ("UNPACK_FLIP_Y_WEBGL", GL_UNPACK_FLIP_Y_WEBGL),
    (
        "UNPACK_PREMULTIPLY_ALPHA_WEBGL",
        GL_UNPACK_PREMULTIPLY_ALPHA_WEBGL,
    ),
    ("VENDOR", GL_VENDOR),
    ("RENDERER", GL_RENDERER),
    ("VERSION", GL_VERSION),
    ("SHADING_LANGUAGE_VERSION", GL_SHADING_LANGUAGE_VERSION),
    ("MAX_TEXTURE_SIZE", GL_MAX_TEXTURE_SIZE),
    ("MAX_VERTEX_ATTRIBS", GL_MAX_VERTEX_ATTRIBS),
];

/// Maps a WebGL `(size, gl_type, normalized)` tuple to `(wgpu::VertexFormat, tightly_packed_stride)`.
#[must_use]
pub fn gl_to_vertex_format(
    size: u32,
    gl_type: u32,
    normalized: bool,
) -> Option<(wgpu::VertexFormat, u64)> {
    Some(match (gl_type, size, normalized) {
        (GL_FLOAT, 1, _) => (wgpu::VertexFormat::Float32, 4),
        (GL_FLOAT, 2, _) => (wgpu::VertexFormat::Float32x2, 8),
        (GL_FLOAT, 3, _) => (wgpu::VertexFormat::Float32x3, 12),
        (GL_FLOAT, 4, _) => (wgpu::VertexFormat::Float32x4, 16),
        (GL_UNSIGNED_BYTE, 2, true) => (wgpu::VertexFormat::Unorm8x2, 2),
        (GL_UNSIGNED_BYTE, 4, true) => (wgpu::VertexFormat::Unorm8x4, 4),
        (GL_UNSIGNED_BYTE, 2, false) => (wgpu::VertexFormat::Uint8x2, 2),
        (GL_UNSIGNED_BYTE, 4, false) => (wgpu::VertexFormat::Uint8x4, 4),
        (GL_SHORT, 2, true) => (wgpu::VertexFormat::Snorm16x2, 4),
        (GL_SHORT, 4, true) => (wgpu::VertexFormat::Snorm16x4, 8),
        (GL_SHORT, 2, false) => (wgpu::VertexFormat::Sint16x2, 4),
        (GL_SHORT, 4, false) => (wgpu::VertexFormat::Sint16x4, 8),
        (GL_UNSIGNED_SHORT, 2, true) => (wgpu::VertexFormat::Unorm16x2, 4),
        (GL_UNSIGNED_SHORT, 4, true) => (wgpu::VertexFormat::Unorm16x4, 8),
        (GL_UNSIGNED_SHORT, 2, false) => (wgpu::VertexFormat::Uint16x2, 4),
        (GL_UNSIGNED_SHORT, 4, false) => (wgpu::VertexFormat::Uint16x4, 8),
        (GL_INT, 1, _) => (wgpu::VertexFormat::Sint32, 4),
        (GL_INT, 2, _) => (wgpu::VertexFormat::Sint32x2, 8),
        (GL_INT, 3, _) => (wgpu::VertexFormat::Sint32x3, 12),
        (GL_INT, 4, _) => (wgpu::VertexFormat::Sint32x4, 16),
        (GL_UNSIGNED_INT, 1, _) => (wgpu::VertexFormat::Uint32, 4),
        (GL_UNSIGNED_INT, 2, _) => (wgpu::VertexFormat::Uint32x2, 8),
        (GL_UNSIGNED_INT, 3, _) => (wgpu::VertexFormat::Uint32x3, 12),
        (GL_UNSIGNED_INT, 4, _) => (wgpu::VertexFormat::Uint32x4, 16),
        _ => return None,
    })
}

#[must_use]
pub fn gl_to_compare(func: u32) -> wgpu::CompareFunction {
    match func {
        GL_NEVER => wgpu::CompareFunction::Never,
        GL_LESS => wgpu::CompareFunction::Less,
        GL_EQUAL => wgpu::CompareFunction::Equal,
        GL_LEQUAL => wgpu::CompareFunction::LessEqual,
        GL_GREATER => wgpu::CompareFunction::Greater,
        GL_NOTEQUAL => wgpu::CompareFunction::NotEqual,
        GL_GEQUAL => wgpu::CompareFunction::GreaterEqual,
        _ => wgpu::CompareFunction::Always,
    }
}

#[must_use]
pub fn gl_to_blend_factor(f: u32) -> wgpu::BlendFactor {
    match f {
        GL_ZERO => wgpu::BlendFactor::Zero,
        GL_ONE => wgpu::BlendFactor::One,
        GL_SRC_COLOR => wgpu::BlendFactor::Src,
        GL_ONE_MINUS_SRC_COLOR => wgpu::BlendFactor::OneMinusSrc,
        GL_SRC_ALPHA => wgpu::BlendFactor::SrcAlpha,
        GL_ONE_MINUS_SRC_ALPHA => wgpu::BlendFactor::OneMinusSrcAlpha,
        GL_DST_ALPHA => wgpu::BlendFactor::DstAlpha,
        GL_ONE_MINUS_DST_ALPHA => wgpu::BlendFactor::OneMinusDstAlpha,
        GL_DST_COLOR => wgpu::BlendFactor::Dst,
        GL_ONE_MINUS_DST_COLOR => wgpu::BlendFactor::OneMinusDst,
        GL_SRC_ALPHA_SATURATE => wgpu::BlendFactor::SrcAlphaSaturated,
        _ => wgpu::BlendFactor::One,
    }
}

#[must_use]
pub fn gl_to_blend_op(eq: u32) -> wgpu::BlendOperation {
    match eq {
        GL_FUNC_SUBTRACT => wgpu::BlendOperation::Subtract,
        GL_FUNC_REVERSE_SUBTRACT => wgpu::BlendOperation::ReverseSubtract,
        GL_MIN => wgpu::BlendOperation::Min,
        GL_MAX => wgpu::BlendOperation::Max,
        _ => wgpu::BlendOperation::Add,
    }
}

#[must_use]
pub fn gl_to_filter(f: u32) -> wgpu::FilterMode {
    match f {
        GL_NEAREST | GL_NEAREST_MIPMAP_NEAREST | GL_NEAREST_MIPMAP_LINEAR => {
            wgpu::FilterMode::Nearest
        }
        _ => wgpu::FilterMode::Linear,
    }
}

#[must_use]
pub fn gl_to_wrap(w: u32) -> wgpu::AddressMode {
    match w {
        GL_CLAMP_TO_EDGE => wgpu::AddressMode::ClampToEdge,
        GL_MIRRORED_REPEAT => wgpu::AddressMode::MirrorRepeat,
        _ => wgpu::AddressMode::Repeat,
    }
}

/// Expands WebGL `RGBA`, `RGB`, `ALPHA`, `LUMINANCE`, or `LUMINANCE_ALPHA` source bytes into
/// tightly packed RGBA8 with optional Y-flip (`UNPACK_FLIP_Y_WEBGL`) and premultiplication.
#[must_use]
pub fn expand_webgl_pixels_to_rgba8(
    raw: &[u8],
    width: u32,
    height: u32,
    format: u32,
    flip_y: bool,
    premul: bool,
) -> Vec<u8> {
    let w = width as usize;
    let h = height as usize;
    let mut out = vec![0u8; w * h * 4];
    let src_bpp = match format {
        GL_RGB => 3,
        GL_LUMINANCE | GL_ALPHA => 1,
        GL_LUMINANCE_ALPHA => 2,
        _ => 4,
    };
    for y in 0..h {
        let dst_y = if flip_y { h - 1 - y } else { y };
        for x in 0..w {
            let si = (y * w + x) * src_bpp;
            let di = (dst_y * w + x) * 4;
            if si + src_bpp > raw.len() {
                continue;
            }
            let (mut r, mut g, mut b, a) = match format {
                GL_RGB => (raw[si], raw[si + 1], raw[si + 2], 255),
                GL_LUMINANCE => (raw[si], raw[si], raw[si], 255),
                GL_ALPHA => (0, 0, 0, raw[si]),
                GL_LUMINANCE_ALPHA => (raw[si], raw[si], raw[si], raw[si + 1]),
                _ => (raw[si], raw[si + 1], raw[si + 2], raw[si + 3]),
            };
            if premul && a < 255 {
                let af = u16::from(a);
                r = ((u16::from(r) * af + 127) / 255) as u8;
                g = ((u16::from(g) * af + 127) / 255) as u8;
                b = ((u16::from(b) * af + 127) / 255) as u8;
            }
            out[di] = r;
            out[di + 1] = g;
            out[di + 2] = b;
            out[di + 3] = a;
        }
    }
    out
}
