//! WebGL1 (GLSL ES 1.00) & WebGL2 (GLSL ES 3.00) to WGSL translator and reflector
//! powered by `naga` (`naga::front::glsl` -> `naga::valid::Validator` -> `naga::back::wgsl`).
//!
//! Translates bare WebGL uniforms into a unified `set = 0, binding = 0` std140 UBO,
//! rewrites `sampler2D` / `texture2D` / `texture` into Vulkan GLSL 450 `texture2D` +
//! `sampler` pairs at bindings `1 + 2*i` / `2 + 2*i`, aligns vertex/fragment varying
//! locations by name, maps OpenGL clip-space Z `[-1, 1]` to WebGPU `[0, 1]`, and
//! compiles both stages through Naga into validated WGSL.
//!
//! Reference: Spec 06 §3, Issue #26.

use std::collections::{HashMap, HashSet};

use naga::back::wgsl::{self, WriterFlags};
use naga::front::glsl::{Frontend as GlslFrontend, Options as GlslOptions};
use naga::valid::{Capabilities, ValidationFlags, Validator};
use naga::{ShaderStage, TypeInner};

/// Reflected scalar/vector/matrix type of a WebGL uniform variable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlUniformType {
    Float,
    Vec2,
    Vec3,
    Vec4,
    Int,
    Ivec2,
    Ivec3,
    Ivec4,
    Uint,
    Bool,
    Mat2,
    Mat3,
    Mat4,
}

impl GlUniformType {
    fn parse(ty: &str) -> Option<Self> {
        Some(match ty {
            "float" => Self::Float,
            "vec2" => Self::Vec2,
            "vec3" => Self::Vec3,
            "vec4" => Self::Vec4,
            "int" => Self::Int,
            "ivec2" => Self::Ivec2,
            "ivec3" => Self::Ivec3,
            "ivec4" => Self::Ivec4,
            "uint" => Self::Uint,
            "bool" => Self::Bool,
            "mat2" => Self::Mat2,
            "mat3" => Self::Mat3,
            "mat4" => Self::Mat4,
            _ => return None,
        })
    }

    #[must_use]
    pub const fn glsl_name(self) -> &'static str {
        match self {
            Self::Float => "float",
            Self::Vec2 => "vec2",
            Self::Vec3 => "vec3",
            Self::Vec4 => "vec4",
            Self::Int => "int",
            Self::Ivec2 => "ivec2",
            Self::Ivec3 => "ivec3",
            Self::Ivec4 => "ivec4",
            Self::Uint => "uint",
            Self::Bool => "bool",
            Self::Mat2 => "mat2",
            Self::Mat3 => "mat3",
            Self::Mat4 => "mat4",
        }
    }
}

/// Reflected vertex attribute (`name` -> `location`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlAttribReflection {
    pub name: String,
    pub location: u32,
}

/// Reflected non-sampler uniform field packed into the `set = 0, binding = 0` UBO.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlUniformReflection {
    pub name: String,
    pub ty: GlUniformType,
    pub offset: u32,
}

/// Reflected `sampler2D` uniform bound at `(tex_binding, samp_binding)` in bind group 0.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlSamplerReflection {
    pub name: String,
    pub tex_binding: u32,
    pub samp_binding: u32,
    pub default_unit: u32,
}

/// Result of translating and linking a WebGL1/WebGL2 vertex + fragment shader pair via Naga.
#[derive(Debug, Clone)]
pub struct CompiledWebGlProgram {
    pub vertex_wgsl: String,
    pub fragment_wgsl: String,
    pub attributes: Vec<GlAttribReflection>,
    pub uniforms: Vec<GlUniformReflection>,
    pub ubo_size: u32,
    pub samplers: Vec<GlSamplerReflection>,
}

/// Strips `//` and `/* ... */` comments while preserving newlines for clear diagnostics.
fn strip_comments(src: &str) -> String {
    let bytes = src.as_bytes();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    while i < bytes.len() {
        if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'/' {
            i += 2;
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
        } else if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                if bytes[i] == b'\n' {
                    out.push('\n');
                }
                i += 1;
            }
            i = (i + 2).min(bytes.len());
        } else {
            let ch = src[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

fn is_ident_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Replaces whole-word identifier `from` with `to`.
fn replace_ident(src: &str, from: &str, to: &str) -> String {
    if from.is_empty() || !src.contains(from) {
        return src.to_string();
    }
    let bytes = src.as_bytes();
    let needle = from.as_bytes();
    let mut out = String::with_capacity(src.len() + 32);
    let mut i = 0;
    while i < bytes.len() {
        if i + needle.len() <= bytes.len() && &bytes[i..i + needle.len()] == needle {
            let prev_ok = i == 0 || !is_ident_char(bytes[i - 1]);
            let next_ok =
                i + needle.len() == bytes.len() || !is_ident_char(bytes[i + needle.len()]);
            if prev_ok && next_ok {
                out.push_str(to);
                i += needle.len();
                continue;
            }
        }
        let ch = src[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Rewrites `texture2D(sampler_name, ...)` and `texture(sampler_name, ...)` into Vulkan GLSL 450
/// `texture(sampler2D(_rix_tex_N, _rix_samp_N), ...)`.
fn rewrite_sampler_calls(src: &str, sampler_name: &str, idx: usize) -> String {
    let mut out = src.to_string();
    let replacement = format!("texture(sampler2D(_rix_tex_{idx}, _rix_samp_{idx})");
    for fn_name in ["texture2D", "texture"] {
        let mut rebuilt = String::with_capacity(out.len() + 32);
        let bytes = out.as_bytes();
        let fn_b = fn_name.as_bytes();
        let s_b = sampler_name.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if i + fn_b.len() < bytes.len()
                && &bytes[i..i + fn_b.len()] == fn_b
                && (i == 0 || !is_ident_char(bytes[i - 1]))
            {
                let mut j = i + fn_b.len();
                while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                    j += 1;
                }
                if j < bytes.len() && bytes[j] == b'(' {
                    j += 1;
                    while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                        j += 1;
                    }
                    if j + s_b.len() <= bytes.len() && &bytes[j..j + s_b.len()] == s_b {
                        let after = j + s_b.len();
                        let end_ok = after == bytes.len() || !is_ident_char(bytes[after]);
                        if end_ok {
                            rebuilt.push_str(&replacement);
                            i = after;
                            continue;
                        }
                    }
                }
            }
            let ch = out[i..].chars().next().unwrap();
            rebuilt.push(ch);
            i += ch.len_utf8();
        }
        out = rebuilt;
    }
    out
}

#[derive(Debug)]
struct ParsedDecl {
    layout_location: Option<u32>,
    storage: String,
    ty: String,
    name: String,
}

fn parse_layout_location(prefix: &str) -> Option<u32> {
    let idx = prefix.find("location")?;
    let rest = &prefix[idx + "location".len()..];
    let eq = rest.find('=')?;
    let digits: String = rest[eq + 1..]
        .chars()
        .skip_while(|c| c.is_ascii_whitespace())
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse::<u32>().ok()
}

fn try_parse_top_level_decl(stmt: &str) -> Option<ParsedDecl> {
    let trimmed = stmt.trim().trim_end_matches(';').trim();
    if trimmed.is_empty() || trimmed.contains('{') || trimmed.contains('=') {
        return None;
    }
    let mut layout_location = None;
    let mut rest = trimmed;
    if let Some(after_layout) = rest.strip_prefix("layout") {
        let lp = after_layout.find('(')?;
        let rp = after_layout.find(')')?;
        layout_location = parse_layout_location(&after_layout[lp + 1..rp]);
        rest = after_layout[rp + 1..].trim();
    }
    let tokens: Vec<&str> = rest
        .split_whitespace()
        .filter(|t| {
            !matches!(
                *t,
                "highp" | "mediump" | "lowp" | "flat" | "smooth" | "centroid"
            )
        })
        .collect();
    if tokens.len() != 3 {
        return None;
    }
    let storage = tokens[0];
    if !matches!(storage, "uniform" | "attribute" | "varying" | "in" | "out") {
        return None;
    }
    let ty = tokens[1];
    let name = tokens[2];
    if !name.bytes().all(is_ident_char) {
        return None;
    }
    Some(ParsedDecl {
        layout_location,
        storage: storage.to_string(),
        ty: ty.to_string(),
        name: name.to_string(),
    })
}

/// Inserts `gl_Position.z = gl_Position.z * 0.5 + gl_Position.w * 0.5;` before the closing `}`
/// of `void main(...)` so OpenGL clip-space Z `[-1, 1]` maps accurately to WebGPU `[0, 1]`.
fn inject_ndc_z_fixup(src: &str) -> String {
    let Some(main_pos) = src.find("main") else {
        return src.to_string();
    };
    let Some(open_rel) = src[main_pos..].find('{') else {
        return src.to_string();
    };
    let open = main_pos + open_rel;
    let bytes = src.as_bytes();
    let mut depth = 0usize;
    let mut close = None;
    for (i, &b) in bytes.iter().enumerate().skip(open) {
        match b {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(i);
                    break;
                }
            }
            _ => {}
        }
    }
    let Some(close_idx) = close else {
        return src.to_string();
    };
    let mut out = String::with_capacity(src.len() + 64);
    out.push_str(&src[..close_idx]);
    out.push_str("\n    gl_Position.z = gl_Position.z * 0.5 + gl_Position.w * 0.5;\n");
    out.push_str(&src[close_idx..]);
    out
}

#[derive(Default)]
struct StageExtraction {
    body_lines: Vec<String>,
    decls: Vec<ParsedDecl>,
}

fn extract_stage(src: &str) -> StageExtraction {
    let cleaned = strip_comments(src);
    let mut out = StageExtraction::default();
    let mut brace_depth = 0usize;

    for raw_line in cleaned.lines() {
        let trimmed = raw_line.trim();
        if trimmed.starts_with("#version") {
            continue;
        }
        if brace_depth == 0 && trimmed.starts_with("precision ") && trimmed.ends_with(';') {
            continue;
        }
        if brace_depth == 0 {
            if let Some(decl) = try_parse_top_level_decl(trimmed) {
                out.decls.push(decl);
                continue;
            }
        }
        for b in trimmed.bytes() {
            match b {
                b'{' => brace_depth += 1,
                b'}' => brace_depth = brace_depth.saturating_sub(1),
                _ => {}
            }
        }
        out.body_lines.push(raw_line.to_string());
    }
    out
}

fn compile_glsl450_to_wgsl(
    glsl: &str,
    stage: ShaderStage,
) -> Result<(String, naga::Module), String> {
    let mut frontend = GlslFrontend::default();
    let module = frontend
        .parse(&GlslOptions::from(stage), glsl)
        .map_err(|errs| {
            let msgs: Vec<String> = errs.errors.iter().map(ToString::to_string).collect();
            format!("GLSL {stage:?} parse error: {}", msgs.join("; "))
        })?;
    let mut validator = Validator::new(ValidationFlags::all(), Capabilities::all());
    let info = validator
        .validate(&module)
        .map_err(|e| format!("Naga {stage:?} validation error: {e}"))?;
    let wgsl_src = wgsl::write_string(&module, &info, WriterFlags::empty())
        .map_err(|e| format!("Naga {stage:?} WGSL generation error: {e}"))?;
    Ok((wgsl_src, module))
}

fn extract_ubo_reflection(
    module: &naga::Module,
    ordered_uniforms: &[(String, GlUniformType)],
) -> (Vec<GlUniformReflection>, u32) {
    for (_, gv) in module.global_variables.iter() {
        if let Some(ref b) = gv.binding {
            if b.group == 0 && b.binding == 0 {
                if let TypeInner::Struct { ref members, span } = module.types[gv.ty].inner {
                    let mut out = Vec::new();
                    for member in members {
                        if let Some(ref mname) = member.name {
                            if let Some((_, ty)) = ordered_uniforms.iter().find(|(n, _)| n == mname)
                            {
                                out.push(GlUniformReflection {
                                    name: mname.clone(),
                                    ty: *ty,
                                    offset: member.offset,
                                });
                            }
                        }
                    }
                    let aligned_span = span.div_ceil(16) * 16;
                    return (out, aligned_span.max(16));
                }
            }
        }
    }
    (Vec::new(), 0)
}

/// Translates and links a WebGL1/WebGL2 vertex + fragment GLSL shader pair via Naga into
/// validated WGSL modules plus complete attribute/uniform/sampler reflection metadata.
pub fn compile_webgl_program(
    vertex_src: &str,
    fragment_src: &str,
    bound_attrib_locations: &HashMap<String, u32>,
) -> Result<CompiledWebGlProgram, String> {
    let vs = extract_stage(vertex_src);
    let fs = extract_stage(fragment_src);

    // 1. Collect unified uniforms and samplers across both stages.
    let mut ordered_uniforms: Vec<(String, GlUniformType)> = Vec::new();
    let mut seen_uniforms: HashSet<String> = HashSet::new();
    let mut samplers: Vec<GlSamplerReflection> = Vec::new();
    let mut seen_samplers: HashSet<String> = HashSet::new();

    for decl in vs.decls.iter().chain(fs.decls.iter()) {
        if decl.storage != "uniform" {
            continue;
        }
        if decl.ty == "sampler2D" {
            if seen_samplers.insert(decl.name.clone()) {
                let idx = samplers.len() as u32;
                samplers.push(GlSamplerReflection {
                    name: decl.name.clone(),
                    tex_binding: 1 + idx * 2,
                    samp_binding: 2 + idx * 2,
                    default_unit: 0,
                });
            }
        } else if let Some(uty) = GlUniformType::parse(&decl.ty) {
            if seen_uniforms.insert(decl.name.clone()) {
                ordered_uniforms.push((decl.name.clone(), uty));
            }
        }
    }

    // 2. Assign vertex attribute locations (respecting explicit bindAttribLocation & layout).
    let mut attributes: Vec<GlAttribReflection> = Vec::new();
    let mut used_attr_locs: HashSet<u32> = HashSet::new();
    let mut vs_in_lines: Vec<String> = Vec::new();
    for decl in &vs.decls {
        if matches!(decl.storage.as_str(), "attribute" | "in") {
            let loc = if let Some(&bound) = bound_attrib_locations.get(&decl.name) {
                bound
            } else if let Some(explicit) = decl.layout_location {
                explicit
            } else {
                let mut next = 0u32;
                while used_attr_locs.contains(&next) {
                    next += 1;
                }
                next
            };
            used_attr_locs.insert(loc);
            attributes.push(GlAttribReflection {
                name: decl.name.clone(),
                location: loc,
            });
            vs_in_lines.push(format!(
                "layout(location = {loc}) in {} {};",
                decl.ty, decl.name
            ));
        }
    }

    // 3. Match vertex out / varying with fragment in / varying by variable name.
    let mut varying_locs: HashMap<String, u32> = HashMap::new();
    let mut next_varying_loc = 0u32;
    let mut vs_out_lines: Vec<String> = Vec::new();
    for decl in &vs.decls {
        if matches!(decl.storage.as_str(), "varying" | "out") {
            let loc = decl.layout_location.unwrap_or_else(|| {
                let v = next_varying_loc;
                next_varying_loc += 1;
                v
            });
            varying_locs.insert(decl.name.clone(), loc);
            vs_out_lines.push(format!(
                "layout(location = {loc}) out {} {};",
                decl.ty, decl.name
            ));
        }
    }

    let mut fs_in_lines: Vec<String> = Vec::new();
    let mut fs_out_lines: Vec<String> = Vec::new();
    for decl in &fs.decls {
        if matches!(decl.storage.as_str(), "varying" | "in") {
            let loc = *varying_locs.entry(decl.name.clone()).or_insert_with(|| {
                let v = next_varying_loc;
                next_varying_loc += 1;
                v
            });
            fs_in_lines.push(format!(
                "layout(location = {loc}) in {} {};",
                decl.ty, decl.name
            ));
        } else if decl.storage == "out" {
            let loc = decl.layout_location.unwrap_or(0);
            fs_out_lines.push(format!(
                "layout(location = {loc}) out {} {};",
                decl.ty, decl.name
            ));
        }
    }

    // 4. Shared UBO and sampler declarations.
    let mut ubo_block = String::new();
    if !ordered_uniforms.is_empty() {
        ubo_block.push_str("layout(set = 0, binding = 0, std140) uniform _RixGlUniforms {\n");
        for (name, ty) in &ordered_uniforms {
            ubo_block.push_str(&format!("    {} {name};\n", ty.glsl_name()));
        }
        ubo_block.push_str("};\n");
    }
    let mut sampler_decls = String::new();
    for (idx, samp) in samplers.iter().enumerate() {
        sampler_decls.push_str(&format!(
            "layout(set = 0, binding = {}) uniform texture2D _rix_tex_{idx};\n",
            samp.tex_binding
        ));
        sampler_decls.push_str(&format!(
            "layout(set = 0, binding = {}) uniform sampler _rix_samp_{idx};\n",
            samp.samp_binding
        ));
    }

    // 5. Assemble vertex GLSL 450
    let mut vs_body = vs.body_lines.join("\n");
    for (idx, samp) in samplers.iter().enumerate() {
        vs_body = rewrite_sampler_calls(&vs_body, &samp.name, idx);
    }
    vs_body = inject_ndc_z_fixup(&vs_body);
    let vs_glsl450 = format!(
        "#version 450\n{ubo_block}{sampler_decls}{}\n{}\n{vs_body}\n",
        vs_in_lines.join("\n"),
        vs_out_lines.join("\n")
    );

    // 6. Assemble fragment GLSL 450 (supporting WebGL1 gl_FragColor and WebGL2 out vec4)
    let mut fs_body = fs.body_lines.join("\n");
    for (idx, samp) in samplers.iter().enumerate() {
        fs_body = rewrite_sampler_calls(&fs_body, &samp.name, idx);
    }
    if fs_out_lines.is_empty() && fs_body.contains("gl_FragColor") {
        fs_out_lines.push("layout(location = 0) out vec4 _rix_frag_color;".to_string());
        fs_body = replace_ident(&fs_body, "gl_FragColor", "_rix_frag_color");
    }
    let fs_glsl450 = format!(
        "#version 450\n{ubo_block}{sampler_decls}{}\n{}\n{fs_body}\n",
        fs_in_lines.join("\n"),
        fs_out_lines.join("\n")
    );

    // 7. Compile and validate both stages through Naga!
    let (vertex_wgsl, vs_module) = compile_glsl450_to_wgsl(&vs_glsl450, ShaderStage::Vertex)?;
    let (fragment_wgsl, fs_module) = compile_glsl450_to_wgsl(&fs_glsl450, ShaderStage::Fragment)?;

    let (mut uniforms, mut ubo_size) = extract_ubo_reflection(&vs_module, &ordered_uniforms);
    if uniforms.is_empty() && !ordered_uniforms.is_empty() {
        let (fu, fsz) = extract_ubo_reflection(&fs_module, &ordered_uniforms);
        uniforms = fu;
        ubo_size = fsz;
    }

    Ok(CompiledWebGlProgram {
        vertex_wgsl,
        fragment_wgsl,
        attributes,
        uniforms,
        ubo_size,
        samplers,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translates_webgl2_textured_cube_shaders_via_naga_to_wgsl() {
        let vs = r#"#version 300 es
            in vec3 aPos;
            in vec2 aUv;
            uniform mat4 uMVP;
            out vec2 vUv;
            void main() {
                vUv = aUv;
                gl_Position = uMVP * vec4(aPos, 1.0);
            }
        "#;
        let fs = r#"#version 300 es
            precision mediump float;
            in vec2 vUv;
            uniform sampler2D uTex;
            uniform vec4 uTint;
            out vec4 fragColor;
            void main() {
                fragColor = texture(uTex, vUv) * uTint;
            }
        "#;
        let prog = compile_webgl_program(vs, fs, &HashMap::new()).expect("naga compile");
        assert!(prog.vertex_wgsl.contains("@vertex"));
        assert!(prog.fragment_wgsl.contains("@fragment"));
        assert_eq!(prog.attributes.len(), 2);
        assert_eq!(prog.attributes[0].name, "aPos");
        assert_eq!(prog.attributes[1].name, "aUv");
        assert_eq!(prog.uniforms.len(), 2);
        assert_eq!(prog.uniforms[0].name, "uMVP");
        assert_eq!(prog.uniforms[0].offset, 0);
        assert_eq!(prog.uniforms[1].name, "uTint");
        assert_eq!(prog.uniforms[1].offset, 64);
        assert_eq!(prog.ubo_size, 80);
        assert_eq!(prog.samplers.len(), 1);
        assert_eq!(prog.samplers[0].name, "uTex");
    }

    #[test]
    fn translates_webgl1_attribute_varying_gl_frag_color_via_naga() {
        let vs = r#"
            attribute vec2 aPosition;
            attribute vec2 aTexCoord;
            varying vec2 vTexCoord;
            void main() {
                vTexCoord = aTexCoord;
                gl_Position = vec4(aPosition, 0.0, 1.0);
            }
        "#;
        let fs = r#"
            precision mediump float;
            varying vec2 vTexCoord;
            uniform sampler2D uSampler;
            void main() {
                gl_FragColor = texture2D(uSampler, vTexCoord);
            }
        "#;
        let prog = compile_webgl_program(vs, fs, &HashMap::new()).expect("webgl1 compile");
        assert!(prog.vertex_wgsl.contains("@vertex"));
        assert!(prog.fragment_wgsl.contains("@fragment"));
        assert_eq!(prog.samplers.len(), 1);
    }
}
