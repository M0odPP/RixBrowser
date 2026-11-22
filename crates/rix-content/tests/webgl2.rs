//! Headless GPU reftest suite for `WebGL2RenderingContext` (and WebGL1 compat)
//! translating GLSL ES 1.00 / ES 3.00 through `naga` onto `wgpu` render passes
//! and zero-copy `DisplayItem::ExternalSurface` compositing.
//!
//! Reference: Spec 02 §4, Spec 06 §3, Issue #26.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use rix_content::ContentRuntime;
use rix_core::Size;
use rix_dom::Document;
use rix_ipc::PipelineId;
use rix_render::{Color, Compositor, VelloRenderer};
use rix_text::FontDatabase;

static GPU_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn webgl2_textured_3d_cube_depth_test_and_zero_rebuild_compositing() {
    let _guard = GPU_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let mut renderer = VelloRenderer::new();
    let shared_gpu = renderer
        .ensure_shared_gpu_device_blocking()
        .expect("shared headless wgpu device");

    let doc = Rc::new(RefCell::new(Document::new()));
    let mut rt = ContentRuntime::new(doc, Size::new(256.0, 256.0), Arc::new(FontDatabase::new()));
    rt.attach_shared_gpu(shared_gpu);
    rt.load_html_with_scripts(
        r#"<!doctype html>
        <html>
        <head><style>
            html, body { margin: 0; padding: 0; width: 256px; height: 256px; background: #000; }
            canvas { display: block; width: 256px; height: 256px; }
        </style></head>
        <body><canvas id="gl" width="256" height="256"></canvas></body>
        </html>"#,
    )
    .expect("load html");

    let info_v = rt
        .eval_script(
            r#"
            const canvas = document.getElementById("gl");
            const gl = canvas.getContext("webgl2");
            if (!gl) throw new Error("Failed to create WebGL2 context");

            const vsSrc = `#version 300 es
                in vec3 aPos;
                in vec2 aUv;
                uniform mat4 uMVP;
                out vec2 vUv;
                void main() {
                    vUv = aUv;
                    gl_Position = uMVP * vec4(aPos, 1.0);
                }
            `;
            const fsSrc = `#version 300 es
                precision mediump float;
                in vec2 vUv;
                uniform sampler2D uTex;
                out vec4 fragColor;
                void main() {
                    fragColor = texture(uTex, vUv);
                }
            `;

            function compile(type, src) {
                const s = gl.createShader(type);
                gl.shaderSource(s, src);
                gl.compileShader(s);
                if (!gl.getShaderParameter(s, gl.COMPILE_STATUS)) {
                    throw new Error("Shader error: " + gl.getShaderInfoLog(s));
                }
                return s;
            }
            const prog = gl.createProgram();
            gl.attachShader(prog, compile(gl.VERTEX_SHADER, vsSrc));
            gl.attachShader(prog, compile(gl.FRAGMENT_SHADER, fsSrc));
            gl.linkProgram(prog);
            if (!gl.getProgramParameter(prog, gl.LINK_STATUS)) {
                throw new Error("Link error: " + gl.getProgramInfoLog(prog));
            }
            gl.useProgram(prog);

            // 6-face unit cube (24 interleaved vertices: x, y, z, u, v)
            const verts = new Float32Array([
                // Front (+Z)
                -1, -1,  1,  0, 0,   1, -1,  1,  1, 0,   1,  1,  1,  1, 1,  -1,  1,  1,  0, 1,
                // Back (-Z)
                 1, -1, -1,  0, 0,  -1, -1, -1,  1, 0,  -1,  1, -1,  1, 1,   1,  1, -1,  0, 1,
                // Top (+Y)
                -1,  1,  1,  0, 0,   1,  1,  1,  1, 0,   1,  1, -1,  1, 1,  -1,  1, -1,  0, 1,
                // Bottom (-Y)
                -1, -1, -1,  0, 0,   1, -1, -1,  1, 0,   1, -1,  1,  1, 1,  -1, -1,  1,  0, 1,
                // Right (+X)
                 1, -1,  1,  0, 0,   1, -1, -1,  1, 0,   1,  1, -1,  1, 1,   1,  1,  1,  0, 1,
                // Left (-X)
                -1, -1, -1,  0, 0,  -1, -1,  1,  1, 0,  -1,  1,  1,  1, 1,  -1,  1, -1,  0, 1,
            ]);
            const indices = new Uint16Array([
                 0,  1,  2,   0,  2,  3,
                 4,  5,  6,   4,  6,  7,
                 8,  9, 10,   8, 10, 11,
                12, 13, 14,  12, 14, 15,
                16, 17, 18,  16, 18, 19,
                20, 21, 22,  20, 22, 23,
            ]);

            const vao = gl.createVertexArray();
            gl.bindVertexArray(vao);

            const vbo = gl.createBuffer();
            gl.bindBuffer(gl.ARRAY_BUFFER, vbo);
            gl.bufferData(gl.ARRAY_BUFFER, verts, gl.STATIC_DRAW);

            const ibo = gl.createBuffer();
            gl.bindBuffer(gl.ELEMENT_ARRAY_BUFFER, ibo);
            gl.bufferData(gl.ELEMENT_ARRAY_BUFFER, indices, gl.STATIC_DRAW);

            const locPos = gl.getAttribLocation(prog, "aPos");
            const locUv = gl.getAttribLocation(prog, "aUv");
            gl.enableVertexAttribArray(locPos);
            gl.vertexAttribPointer(locPos, 3, gl.FLOAT, false, 20, 0);
            gl.enableVertexAttribArray(locUv);
            gl.vertexAttribPointer(locUv, 2, gl.FLOAT, false, 20, 12);

            // 2x2 vivid checkerboard texture: Red, Green, Blue, Yellow
            const tex = gl.createTexture();
            gl.activeTexture(gl.TEXTURE0);
            gl.bindTexture(gl.TEXTURE_2D, tex);
            gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.NEAREST);
            gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.NEAREST);
            gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
            gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
            gl.texImage2D(
                gl.TEXTURE_2D, 0, gl.RGBA, 2, 2, 0, gl.RGBA, gl.UNSIGNED_BYTE,
                new Uint8Array([
                    255, 0,   0,   255,   0,   255, 0,   255,
                    0,   0,   255, 255,   255, 255, 0,   255,
                ])
            );
            gl.uniform1i(gl.getUniformLocation(prog, "uTex"), 0);

            function mul4(a, b) {
                const o = new Float32Array(16);
                for (let c = 0; c < 4; c++) {
                    for (let r = 0; r < 4; r++) {
                        o[c * 4 + r] =
                            a[0 * 4 + r] * b[c * 4 + 0] +
                            a[1 * 4 + r] * b[c * 4 + 1] +
                            a[2 * 4 + r] * b[c * 4 + 2] +
                            a[3 * 4 + r] * b[c * 4 + 3];
                    }
                }
                return o;
            }
            function perspective(fovRad, aspect, near, far) {
                const f = 1.0 / Math.tan(fovRad / 2);
                const nf = 1.0 / (near - far);
                return new Float32Array([
                    f / aspect, 0, 0, 0,
                    0, f, 0, 0,
                    0, 0, (far + near) * nf, -1,
                    0, 0, 2 * far * near * nf, 0,
                ]);
            }
            function translateZ(z) {
                return new Float32Array([1,0,0,0, 0,1,0,0, 0,0,1,0, 0,0,z,1]);
            }
            function rotY(a) {
                const c = Math.cos(a), s = Math.sin(a);
                return new Float32Array([c,0,-s,0, 0,1,0,0, s,0,c,0, 0,0,0,1]);
            }
            function rotX(a) {
                const c = Math.cos(a), s = Math.sin(a);
                return new Float32Array([1,0,0,0, 0,c,s,0, 0,-s,c,0, 0,0,0,1]);
            }

            const uMvp = gl.getUniformLocation(prog, "uMVP");
            gl.enable(gl.DEPTH_TEST);
            gl.enable(gl.CULL_FACE);
            gl.viewport(0, 0, 256, 256);
            gl.clearColor(16 / 255, 24 / 255, 40 / 255, 1.0);

            globalThis.__drawCube = function(ay, ax) {
                const mvp = mul4(perspective(Math.PI / 3, 1.0, 0.1, 20.0),
                    mul4(translateZ(-4.2), mul4(rotY(ay), rotX(ax))));
                gl.uniformMatrix4fv(uMvp, false, mvp);
                gl.clear(gl.COLOR_BUFFER_BIT | gl.DEPTH_BUFFER_BIT);
                gl.drawElements(gl.TRIANGLES, 36, gl.UNSIGNED_SHORT, 0);
            };

            globalThis.__drawCube(0.55, 0.40);

            const corner = new Uint8Array(4);
            gl.readPixels(4, 4, 1, 1, gl.RGBA, gl.UNSIGNED_BYTE, corner);
            const center = new Uint8Array(4);
            gl.readPixels(128, 128, 1, 1, gl.RGBA, gl.UNSIGNED_BYTE, center);
            JSON.stringify({
                version: gl.getParameter(gl.VERSION),
                err: gl.getError(),
                corner: Array.from(corner),
                center: Array.from(center),
                pngOk: canvas.toDataURL().startsWith("data:image/png;base64,"),
            });
            "#,
        )
        .expect("eval webgl2 cube script");

    let info: serde_json::Value =
        serde_json::from_str(&rt.vm.value_to_rust_string(info_v)).expect("valid json");
    assert!(info["version"].as_str().unwrap().contains("WebGL 2.0"));
    assert_eq!(info["err"], 0);
    assert_eq!(info["pngOk"], true);
    let corner = info["corner"].as_array().unwrap();
    assert!(
        (10..=25).contains(&corner[0].as_u64().unwrap())
            && (18..=32).contains(&corner[1].as_u64().unwrap())
            && (32..=48).contains(&corner[2].as_u64().unwrap()),
        "expected dark slate corner via readPixels, got {corner:?}"
    );

    let mut compositor = Compositor::new(Size::new(256.0, 256.0), 1.0);
    let pipeline = PipelineId(99);
    let (dl_rebuilt_1, presented_1) =
        rt.sync_frame_to_compositor(pipeline, 1, &mut compositor, Some(&mut renderer));
    assert!(dl_rebuilt_1);
    assert_eq!(presented_1, 1);

    let scene1 = compositor.compose_force().clone();
    let rgba1 = renderer
        .render_to_rgba_blocking(&scene1, 256, 256, Color::BLACK)
        .expect("render composited webgl2 frame 1");

    // Verify all 4 checkerboard colors from the 2x2 texture appear on the rendered 3D cube faces.
    let (mut red, mut green, mut blue, mut yellow) = (0usize, 0, 0, 0);
    for px in rgba1.as_chunks::<4>().0 {
        if px[0] > 230 && px[1] < 25 && px[2] < 25 {
            red += 1;
        } else if px[0] < 25 && px[1] > 230 && px[2] < 25 {
            green += 1;
        } else if px[0] < 25 && px[1] < 25 && px[2] > 230 {
            blue += 1;
        } else if px[0] > 230 && px[1] > 230 && px[2] < 25 {
            yellow += 1;
        }
    }
    assert!(
        red > 500 && green > 500 && blue > 500 && yellow > 500,
        "expected textured 3D cube with all 4 vivid texel colors, got R={red} G={green} B={blue} Y={yellow}"
    );

    // Frame 2: rotate cube — MUST NOT rebuild DisplayList and must produce distinct rotated pixels!
    rt.eval_script("globalThis.__drawCube(1.25, 0.75);")
        .expect("draw rotated cube frame 2");
    let (dl_rebuilt_2, presented_2) =
        rt.sync_frame_to_compositor(pipeline, 2, &mut compositor, Some(&mut renderer));
    assert!(
        !dl_rebuilt_2,
        "WebGL2 frame update must NOT rebuild page DisplayList"
    );
    assert_eq!(presented_2, 1);

    let scene2 = compositor.compose_force().clone();
    let rgba2 = renderer
        .render_to_rgba_blocking(&scene2, 256, 256, Color::BLACK)
        .expect("render composited webgl2 frame 2");
    let diff_pixels = rgba1
        .as_chunks::<4>()
        .0
        .iter()
        .zip(rgba2.as_chunks::<4>().0.iter())
        .filter(|(a, b)| a != b)
        .count();
    assert!(
        diff_pixels > 1000,
        "rotated cube frame 2 should differ substantially from frame 1, diff={diff_pixels}"
    );

    let _stats = rt.collect_garbage();
}

#[test]
fn webgl1_compat_texture2d_and_alpha_blending() {
    let _guard = GPU_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut rt = ContentRuntime::with_empty_document();
    rt.load_html_with_scripts(
        r#"<!doctype html><html><body><canvas id="c" width="64" height="64"></canvas></body></html>"#,
    )
    .expect("load html");

    let out_v = rt
        .eval_script(
            r#"
            const gl = document.getElementById("c").getContext("webgl");
            const vs = gl.createShader(gl.VERTEX_SHADER);
            gl.shaderSource(vs, `
                attribute vec2 aPos;
                attribute vec2 aUv;
                varying vec2 vUv;
                void main() {
                    vUv = aUv;
                    gl_Position = vec4(aPos, 0.0, 1.0);
                }
            `);
            gl.compileShader(vs);
            const fs = gl.createShader(gl.FRAGMENT_SHADER);
            gl.shaderSource(fs, `
                precision mediump float;
                varying vec2 vUv;
                uniform sampler2D uSampler;
                uniform vec4 uTint;
                void main() {
                    gl_FragColor = texture2D(uSampler, vUv) * uTint;
                }
            `);
            gl.compileShader(fs);
            const p = gl.createProgram();
            gl.attachShader(p, vs);
            gl.attachShader(p, fs);
            gl.linkProgram(p);
            gl.useProgram(p);

            const buf = gl.createBuffer();
            gl.bindBuffer(gl.ARRAY_BUFFER, buf);
            gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([
                -1, -1, 0, 0,   1, -1, 1, 0,   1,  1, 1, 1,
                -1, -1, 0, 0,   1,  1, 1, 1,  -1,  1, 0, 1,
            ]), gl.STATIC_DRAW);
            const lp = gl.getAttribLocation(p, "aPos");
            const lu = gl.getAttribLocation(p, "aUv");
            gl.enableVertexAttribArray(lp);
            gl.vertexAttribPointer(lp, 2, gl.FLOAT, false, 16, 0);
            gl.enableVertexAttribArray(lu);
            gl.vertexAttribPointer(lu, 2, gl.FLOAT, false, 16, 8);

            // Solid green 1x1 texture blended at alpha=0.5 over solid red clear color
            const t = gl.createTexture();
            gl.bindTexture(gl.TEXTURE_2D, t);
            gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, 1, 1, 0, gl.RGBA, gl.UNSIGNED_BYTE,
                new Uint8Array([0, 255, 0, 255]));
            gl.uniform1i(gl.getUniformLocation(p, "uSampler"), 0);
            gl.uniform4f(gl.getUniformLocation(p, "uTint"), 1.0, 1.0, 1.0, 0.5);

            gl.viewport(0, 0, 64, 64);
            gl.clearColor(1.0, 0.0, 0.0, 1.0);
            gl.clear(gl.COLOR_BUFFER_BIT);
            gl.enable(gl.BLEND);
            gl.blendFunc(gl.SRC_ALPHA, gl.ONE_MINUS_SRC_ALPHA);
            gl.drawArrays(gl.TRIANGLES, 0, 6);

            const px = new Uint8Array(4);
            gl.readPixels(32, 32, 1, 1, gl.RGBA, gl.UNSIGNED_BYTE, px);
            JSON.stringify({ ver: gl.getParameter(gl.VERSION), px: Array.from(px) });
            "#,
        )
        .expect("eval webgl1 script");

    let out: serde_json::Value =
        serde_json::from_str(&rt.vm.value_to_rust_string(out_v)).expect("valid json");
    assert!(out["ver"].as_str().unwrap().contains("WebGL 1.0"));
    let px = out["px"].as_array().unwrap();
    let (r, g, b) = (
        px[0].as_u64().unwrap(),
        px[1].as_u64().unwrap(),
        px[2].as_u64().unwrap(),
    );
    assert!(
        (120..=136).contains(&r) && (120..=136).contains(&g) && b < 10,
        "expected 50/50 blended red+green ~(127,127,0), got ({r},{g},{b})"
    );
}
