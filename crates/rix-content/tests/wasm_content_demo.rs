//! End-to-end WebAssembly demo inside `ContentRuntime`:
//! - Loads a real WebAssembly pixel-shader module via `WebAssembly.instantiateStreaming`
//!   wrapping a Fetch `Response` with `Content-Type: application/wasm`.
//! - Renders a 64x64 RGBA checkerboard/gradient directly in Wasm linear memory.
//! - Blits Wasm linear memory into `<canvas>` via `new ImageData(new Uint8ClampedArray(memory.buffer, ...), w, h)`
//!   and `ctx.putImageData(...)`.
//! - Exercises JIT-compiled JS calling Wasm exports across >1,500 iterations.
//! - Composites the resulting frame through Vello + `Compositor` and asserts rendered pixels.

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

fn pixel_at(rgba: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
    let idx = ((y * width + x) * 4) as usize;
    [rgba[idx], rgba[idx + 1], rgba[idx + 2], rgba[idx + 3]]
}

#[test]
fn wasm_pixel_shader_canvas_streaming_demo_and_jit_interop() {
    let _gpu = GPU_LOCK.lock().expect("gpu lock");

    // Compile a real WebAssembly pixel shader module that fills linear memory
    // with a deterministic 64x64 RGBA pattern:
    //   R = (x * 4 + phase) & 255
    //   G = (y * 4) & 255
    //   B = ((x ^ y) * 4) & 255
    //   A = 255
    // Plus a small `mix(a, b)` helper called from a hot JIT-compiled JS loop.
    let wasm_bytes = wat::parse_str(
        r#"
        (module
            (memory (export "memory") 1)
            (func (export "render") (param $w i32) (param $h i32) (param $phase i32)
                (local $y i32)
                (local $x i32)
                (local $off i32)
                (local.set $y (i32.const 0))
                (block $break_y
                    (loop $loop_y
                        (br_if $break_y (i32.ge_u (local.get $y) (local.get $h)))
                        (local.set $x (i32.const 0))
                        (block $break_x
                            (loop $loop_x
                                (br_if $break_x (i32.ge_u (local.get $x) (local.get $w)))
                                (local.set $off
                                    (i32.shl
                                        (i32.add (i32.mul (local.get $y) (local.get $w)) (local.get $x))
                                        (i32.const 2)))
                                ;; R = (x * 4 + phase) & 0xff
                                (i32.store8 (local.get $off)
                                    (i32.add (i32.shl (local.get $x) (i32.const 2)) (local.get $phase)))
                                ;; G = (y * 4) & 0xff
                                (i32.store8 (i32.add (local.get $off) (i32.const 1))
                                    (i32.shl (local.get $y) (i32.const 2)))
                                ;; B = ((x ^ y) * 4) & 0xff
                                (i32.store8 (i32.add (local.get $off) (i32.const 2))
                                    (i32.shl (i32.xor (local.get $x) (local.get $y)) (i32.const 2)))
                                ;; A = 255
                                (i32.store8 (i32.add (local.get $off) (i32.const 3))
                                    (i32.const 255))
                                (local.set $x (i32.add (local.get $x) (i32.const 1)))
                                (br $loop_x)
                            )
                        )
                        (local.set $y (i32.add (local.get $y) (i32.const 1)))
                        (br $loop_y)
                    )
                )
            )
            (func (export "mix") (param $a i32) (param $b i32) (result i32)
                (i32.add (i32.mul (local.get $a) (i32.const 3)) (local.get $b))
            )
        )
        "#,
    )
    .expect("compile wat");

    let mut font_db = FontDatabase::new();
    font_db.scan_system_fonts();
    let doc = Rc::new(RefCell::new(Document::new()));
    let mut rt = ContentRuntime::new(doc, Size::new(128.0, 128.0), Arc::new(font_db));

    rt.load_html_with_scripts(
        r#"<!doctype html>
        <html>
        <head><style>body { margin: 0; padding: 0; background: #000000; } canvas { display: block; }</style></head>
        <body><canvas id="screen" width="64" height="64"></canvas></body>
        </html>"#,
    )
    .expect("load html");

    let bytes_csv: String = wasm_bytes
        .iter()
        .map(u8::to_string)
        .collect::<Vec<_>>()
        .join(",");

    let script = format!(
        r#"
        var wasmBytes = new Uint8Array([{bytes_csv}]);
        var demoResult = null;
        var resp = new Response(wasmBytes, {{
            status: 200,
            headers: {{ "Content-Type": "application/wasm" }}
        }});

        function hotDriver(mixFn) {{
            var acc = 0;
            for (var i = 0; i < 1500; i++) {{
                acc = (acc + mixFn(i, 7)) | 0;
            }}
            return acc;
        }}

        WebAssembly.instantiateStreaming(Promise.resolve(resp)).then(function(res) {{
            var exp = res.instance.exports;
            exp.render(64, 64, 16);

            var view = new Uint8ClampedArray(exp.memory.buffer, 0, 64 * 64 * 4);
            var canvas = document.getElementById("screen");
            var ctx = canvas.getContext("2d");
            var img = new ImageData(view, 64, 64);
            ctx.putImageData(img, 0, 0);

            var sample = ctx.getImageData(10, 20, 1, 1).data;
            var jitSum = hotDriver(exp.mix);
            demoResult = JSON.stringify({{
                ok: true,
                sampleR: sample[0],
                sampleG: sample[1],
                sampleB: sample[2],
                sampleA: sample[3],
                jitSum: jitSum
            }});
        }}).catch(function(e) {{
            demoResult = "ERR:" + String(e);
        }});
        "#
    );

    rt.eval_script(&script).expect("eval wasm demo");
    rt.vm.drain_microtasks();

    let res = rt.eval_script("demoResult").expect("read demoResult");
    let json = rt.vm.value_to_rust_string(res);
    // Expected pixel at (10, 20) with phase=16:
    // R = (10*4 + 16) = 56, G = 20*4 = 80, B = (10^20)*4 = 30*4 = 120, A = 255
    // Expected hotDriver sum: sum_{i=0..1499} (3*i + 7) = 3 * (1499*1500/2) + 7*1500 = 3372750 + 10500 = 3383250
    assert!(
        json.contains(r#""ok":true"#)
            && json.contains(r#""sampleR":56"#)
            && json.contains(r#""sampleG":80"#)
            && json.contains(r#""sampleB":120"#)
            && json.contains(r#""sampleA":255"#)
            && json.contains(r#""jitSum":3383250"#),
        "unexpected wasm demo result: {json}"
    );
    assert!(
        rt.jit.stats().compiled_functions >= 1,
        "TieredJit should compile hot JS-Wasm interop driver inside ContentRuntime"
    );

    // Composite the page through Vello + Compositor and verify the Wasm-drawn pixel
    // appears directly in the headless GPU framebuffer.
    let pipeline = PipelineId(1);
    let mut compositor = Compositor::new(Size::new(128.0, 128.0), 1.0);
    let mut gpu = VelloRenderer::new();
    let (dl_built, presented) =
        rt.sync_frame_to_compositor(pipeline, 1, &mut compositor, Some(&mut gpu));
    assert!(dl_built, "initial frame must build DisplayList");
    assert_eq!(presented, 1, "canvas surface should be presented");
    let composed = compositor.compose().expect("compose wasm canvas frame");
    let rgba = gpu
        .render_to_rgba_blocking(composed, 128, 128, Color::BLACK)
        .expect("render wasm canvas frame");
    let px = pixel_at(&rgba, 128, 10, 20);
    assert!(
        (i16::from(px[0]) - 56).abs() <= 2
            && (i16::from(px[1]) - 80).abs() <= 2
            && (i16::from(px[2]) - 120).abs() <= 2
            && px[3] == 255,
        "GPU composited framebuffer pixel at (10, 20) mismatch: {px:?}"
    );
}
