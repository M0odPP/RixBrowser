//! Headless GPU integration & reftest suite for native W3C WebGPU (`navigator.gpu`)
//! over `wgpu` and zero-copy `DisplayItem::ExternalSurface` compositing.
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

fn run_async_js(rt: &mut ContentRuntime, script: &str) -> serde_json::Value {
    rt.eval_script(
        r#"
        globalThis.__done = false;
        globalThis.__err = "";
        globalThis.__out = null;
        "#,
    )
    .expect("prelude");
    let wrapped = format!(
        "(async () => {{ {script} }})().then((v) => {{ globalThis.__out = JSON.stringify(v); globalThis.__done = true; }}).catch((e) => {{ globalThis.__err = String(e && e.name ? e.name + ': ' + e.message : e); globalThis.__done = true; }});"
    );
    rt.eval_script(&wrapped).expect("eval async webgpu script");
    rt.microtask_checkpoint();

    let err_v = rt.eval_script("globalThis.__err").expect("err");
    let err = rt.vm.value_to_rust_string(err_v);
    assert!(err.is_empty(), "WebGPU JS error: {err}");
    assert_eq!(
        rt.eval_script("globalThis.__done").unwrap().as_bool(),
        Some(true),
        "WebGPU async promise did not settle"
    );
    let out_v = rt.eval_script("globalThis.__out").expect("out");
    serde_json::from_str(&rt.vm.value_to_rust_string(out_v)).expect("valid json output")
}

#[test]
fn webgpu_compute_shader_storage_buffer_readback() {
    let _guard = GPU_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut rt = ContentRuntime::with_empty_document();

    let res = run_async_js(
        &mut rt,
        r#"
        const adapter = await navigator.gpu.requestAdapter();
        if (!adapter) throw new Error("No WebGPU adapter available");
        const device = await adapter.requestDevice({ label: "compute-test-device" });

        const COUNT = 64;
        const BYTE_LEN = COUNT * 4;

        // Upload initial input data via mappedAtCreation
        const inputBuf = device.createBuffer({
            label: "input",
            size: BYTE_LEN,
            usage: GPUBufferUsage.STORAGE,
            mappedAtCreation: true,
        });
        const inputView = new Uint32Array(inputBuf.getMappedRange());
        for (let i = 0; i < COUNT; i++) {
            inputView[i] = i;
        }
        inputBuf.unmap();

        const outputBuf = device.createBuffer({
            label: "output",
            size: BYTE_LEN,
            usage: GPUBufferUsage.STORAGE | GPUBufferUsage.COPY_SRC,
        });
        const readbackBuf = device.createBuffer({
            label: "readback",
            size: BYTE_LEN,
            usage: GPUBufferUsage.MAP_READ | GPUBufferUsage.COPY_DST,
        });

        const module = device.createShaderModule({
            label: "affine-compute",
            code: `
                @group(0) @binding(0) var<storage, read> src: array<u32>;
                @group(0) @binding(1) var<storage, read_write> dst: array<u32>;

                @compute @workgroup_size(64)
                fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
                    let i = gid.x;
                    if (i < 64u) {
                        dst[i] = src[i] * 3u + 7u;
                    }
                }
            `,
        });
        const compInfo = await module.getCompilationInfo();

        const pipeline = device.createComputePipeline({
            layout: "auto",
            compute: { module, entryPoint: "main" },
        });

        const bindGroup = device.createBindGroup({
            layout: pipeline.getBindGroupLayout(0),
            entries: [
                { binding: 0, resource: { buffer: inputBuf } },
                { binding: 1, resource: { buffer: outputBuf } },
            ],
        });

        const encoder = device.createCommandEncoder();
        const pass = encoder.beginComputePass();
        pass.setPipeline(pipeline);
        pass.setBindGroup(0, bindGroup);
        pass.dispatchWorkgroups(1);
        pass.end();
        encoder.copyBufferToBuffer(outputBuf, 0, readbackBuf, 0, BYTE_LEN);
        device.queue.submit([encoder.finish()]);
        await device.queue.onSubmittedWorkDone();

        await readbackBuf.mapAsync(GPUMapMode.READ);
        const mappedState = readbackBuf.mapState;
        const result = Array.from(new Uint32Array(readbackBuf.getMappedRange()));
        readbackBuf.unmap();

        return {
            format: navigator.gpu.getPreferredCanvasFormat(),
            hasDeviceName: typeof adapter.info.device === "string",
            compErrors: compInfo.messages.length,
            mappedState,
            unmappedState: readbackBuf.mapState,
            result,
        };
        "#,
    );

    assert_eq!(res["format"], "rgba8unorm");
    assert_eq!(res["hasDeviceName"], true);
    assert_eq!(res["compErrors"], 0);
    assert_eq!(res["mappedState"], "mapped");
    assert_eq!(res["unmappedState"], "unmapped");
    let arr = res["result"].as_array().expect("result array");
    assert_eq!(arr.len(), 64);
    for (i, v) in arr.iter().enumerate() {
        let expected = (i as u64) * 3 + 7;
        assert_eq!(
            v.as_u64(),
            Some(expected),
            "mismatch at index {i}: got {v}, expected {expected}"
        );
    }
}

#[test]
fn webgpu_triangle_render_and_zero_copy_external_surface_compositing() {
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
        <body><canvas id="c" width="256" height="256"></canvas></body>
        </html>"#,
    )
    .expect("load html");

    // Frame 1: render a bright red centered triangle over dark blue clear color via WGSL.
    let setup = run_async_js(
        &mut rt,
        r#"
        const canvas = document.getElementById("c");
        const ctx = canvas.getContext("webgpu");
        const adapter = await navigator.gpu.requestAdapter();
        const device = await adapter.requestDevice();
        const format = navigator.gpu.getPreferredCanvasFormat();
        ctx.configure({ device, format });

        const uniformBuf = device.createBuffer({
            size: 16,
            usage: GPUBufferUsage.UNIFORM | GPUBufferUsage.COPY_DST,
        });
        const vertexData = new Float32Array([
             0.0,  0.75,
            -0.75, -0.75,
             0.75, -0.75,
        ]);
        const vertexBuf = device.createBuffer({
            size: vertexData.byteLength,
            usage: GPUBufferUsage.VERTEX | GPUBufferUsage.COPY_DST,
        });
        device.queue.writeBuffer(vertexBuf, 0, vertexData);

        const shader = device.createShaderModule({
            code: `
                @group(0) @binding(0) var<uniform> uColor: vec4<f32>;

                @vertex
                fn vs_main(@location(0) pos: vec2<f32>) -> @builtin(position) vec4<f32> {
                    return vec4<f32>(pos, 0.0, 1.0);
                }

                @fragment
                fn fs_main() -> @location(0) vec4<f32> {
                    return uColor;
                }
            `,
        });

        const pipeline = device.createRenderPipeline({
            layout: "auto",
            vertex: {
                module: shader,
                entryPoint: "vs_main",
                buffers: [{
                    arrayStride: 8,
                    attributes: [{ shaderLocation: 0, offset: 0, format: "float32x2" }],
                }],
            },
            fragment: {
                module: shader,
                entryPoint: "fs_main",
                targets: [{ format }],
            },
            primitive: { topology: "triangle-list" },
        });

        const bindGroup = device.createBindGroup({
            layout: pipeline.getBindGroupLayout(0),
            entries: [{ binding: 0, resource: { buffer: uniformBuf } }],
        });

        globalThis.__renderFrame = function(r, g, b) {
            device.queue.writeBuffer(uniformBuf, 0, new Float32Array([r, g, b, 1.0]));
            const encoder = device.createCommandEncoder();
            const pass = encoder.beginRenderPass({
                colorAttachments: [{
                    view: ctx.getCurrentTexture().createView(),
                    clearValue: { r: 0.0, g: 0.0, b: 0.5, a: 1.0 },
                    loadOp: "clear",
                    storeOp: "store",
                }],
            });
            pass.setPipeline(pipeline);
            pass.setBindGroup(0, bindGroup);
            pass.setVertexBuffer(0, vertexBuf);
            pass.draw(3);
            pass.end();
            device.queue.submit([encoder.finish()]);
        };

        globalThis.__renderFrame(1.0, 0.0, 0.0);
        return {
            hasCanvasRef: ctx.canvas === canvas,
            pngPrefix: canvas.toDataURL().slice(0, 22),
        };
        "#,
    );
    assert_eq!(setup["hasCanvasRef"], true);
    assert_eq!(setup["pngPrefix"], "data:image/png;base64,");

    let mut compositor = Compositor::new(Size::new(256.0, 256.0), 1.0);
    let pipeline = PipelineId(42);
    let (dl_rebuilt_1, presented_1) =
        rt.sync_frame_to_compositor(pipeline, 1, &mut compositor, Some(&mut renderer));
    assert!(dl_rebuilt_1, "initial frame builds page DisplayList once");
    assert_eq!(presented_1, 1, "WebGPU canvas presented on frame 1");

    let scene1 = compositor.compose_force().clone();
    let rgba1 = renderer
        .render_to_rgba_blocking(&scene1, 256, 256, Color::BLACK)
        .expect("render composited WebGPU frame 1");
    let px = |buf: &[u8], x: usize, y: usize| -> [u8; 4] {
        let i = (y * 256 + x) * 4;
        [buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]
    };

    let center1 = px(&rgba1, 128, 128);
    let corner1 = px(&rgba1, 8, 8);
    assert!(
        center1[0] > 220 && center1[1] < 30 && center1[2] < 30 && center1[3] > 240,
        "expected bright red triangle at center, got {center1:?}"
    );
    assert!(
        corner1[0] < 30 && corner1[1] < 30 && corner1[2] > 100 && corner1[3] > 240,
        "expected dark blue background at corner, got {corner1:?}"
    );

    // Frame 2: update uniform to bright green and re-render — MUST have ZERO DisplayList rebuilds!
    rt.eval_script("globalThis.__renderFrame(0.0, 1.0, 0.0);")
        .expect("render frame 2");
    let (dl_rebuilt_2, presented_2) =
        rt.sync_frame_to_compositor(pipeline, 2, &mut compositor, Some(&mut renderer));
    assert!(
        !dl_rebuilt_2,
        "WebGPU frame update must NOT rebuild page DisplayList"
    );
    assert_eq!(presented_2, 1, "WebGPU canvas presented on frame 2");

    let scene2 = compositor.compose_force().clone();
    let rgba2 = renderer
        .render_to_rgba_blocking(&scene2, 256, 256, Color::BLACK)
        .expect("render composited WebGPU frame 2");
    let center2 = px(&rgba2, 128, 128);
    assert!(
        center2[0] < 30 && center2[1] > 220 && center2[2] < 30 && center2[3] > 240,
        "expected bright green triangle at center on frame 2, got {center2:?}"
    );

    // Ensure Major GC sweeps cleanly without invalidating active WebGPU handles.
    let _stats = rt.collect_garbage();
}

#[test]
fn webgpu_textured_indexed_quad_and_error_scope() {
    let _guard = GPU_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut rt = ContentRuntime::with_empty_document();

    let res = run_async_js(
        &mut rt,
        r#"
        const adapter = await navigator.gpu.requestAdapter();
        const device = await adapter.requestDevice();

        // 1. Validate shader error reporting via getCompilationInfo
        const badMod = device.createShaderModule({ code: "fn broken( { }" });
        const badInfo = await badMod.getCompilationInfo();

        // 2. Upload a 2x2 RGBA checkerboard texture: [Red, Green; Blue, Yellow]
        const srcTex = device.createTexture({
            size: [2, 2, 1],
            format: "rgba8unorm",
            usage: GPUTextureUsage.TEXTURE_BINDING | GPUTextureUsage.COPY_DST,
        });
        const texels = new Uint8Array([
            255, 0,   0,   255,   0,   255, 0,   255,
            0,   0,   255, 255,   255, 255, 0,   255,
        ]);
        device.queue.writeTexture(
            { texture: srcTex },
            texels,
            { bytesPerRow: 8, rowsPerImage: 2 },
            { width: 2, height: 2, depthOrArrayLayers: 1 }
        );
        const sampler = device.createSampler({ magFilter: "nearest", minFilter: "nearest" });

        // 3. Offscreen 64x64 render target
        const target = device.createTexture({
            size: { width: 64, height: 64 },
            format: "rgba8unorm",
            usage: GPUTextureUsage.RENDER_ATTACHMENT | GPUTextureUsage.COPY_SRC,
        });

        const verts = new Float32Array([
            -1.0,  1.0, 0.0, 0.0,
             1.0,  1.0, 1.0, 0.0,
             1.0, -1.0, 1.0, 1.0,
            -1.0, -1.0, 0.0, 1.0,
        ]);
        const vbuf = device.createBuffer({
            size: verts.byteLength,
            usage: GPUBufferUsage.VERTEX | GPUBufferUsage.COPY_DST,
        });
        device.queue.writeBuffer(vbuf, 0, verts);

        const indices = new Uint16Array([0, 2, 1, 0, 3, 2]);
        const ibuf = device.createBuffer({
            size: 16,
            usage: GPUBufferUsage.INDEX | GPUBufferUsage.COPY_DST,
        });
        device.queue.writeBuffer(ibuf, 0, indices);

        const bgl = device.createBindGroupLayout({
            entries: [
                { binding: 0, visibility: GPUShaderStage.FRAGMENT, sampler: { type: "filtering" } },
                { binding: 1, visibility: GPUShaderStage.FRAGMENT, texture: { sampleType: "float" } },
            ],
        });
        const layout = device.createPipelineLayout({ bindGroupLayouts: [bgl] });

        const shader = device.createShaderModule({
            code: `
                struct VsOut {
                    @builtin(position) pos: vec4<f32>,
                    @location(0) uv: vec2<f32>,
                };
                @vertex
                fn vs(@location(0) p: vec2<f32>, @location(1) uv: vec2<f32>) -> VsOut {
                    var o: VsOut;
                    o.pos = vec4<f32>(p, 0.0, 1.0);
                    o.uv = uv;
                    return o;
                }
                @group(0) @binding(0) var samp: sampler;
                @group(0) @binding(1) var tex: texture_2d<f32>;
                @fragment
                fn fs(inp: VsOut) -> @location(0) vec4<f32> {
                    return textureSample(tex, samp, inp.uv);
                }
            `,
        });

        const pipe = device.createRenderPipeline({
            layout,
            vertex: {
                module: shader,
                entryPoint: "vs",
                buffers: [{
                    arrayStride: 16,
                    attributes: [
                        { shaderLocation: 0, offset: 0, format: "float32x2" },
                        { shaderLocation: 1, offset: 8, format: "float32x2" },
                    ],
                }],
            },
            fragment: {
                module: shader,
                entryPoint: "fs",
                targets: [{ format: "rgba8unorm" }],
            },
        });

        const bg = device.createBindGroup({
            layout: bgl,
            entries: [
                { binding: 0, resource: sampler },
                { binding: 1, resource: srcTex.createView() },
            ],
        });

        // bytesPerRow must be a multiple of 256 -> 64 * 4 = 256!
        const readBuf = device.createBuffer({
            size: 256 * 64,
            usage: GPUBufferUsage.COPY_DST | GPUBufferUsage.MAP_READ,
        });

        const enc = device.createCommandEncoder();
        const pass = enc.beginRenderPass({
            colorAttachments: [{
                view: target.createView(),
                clearValue: [0, 0, 0, 1],
                loadOp: "clear",
                storeOp: "store",
            }],
        });
        pass.setPipeline(pipe);
        pass.setBindGroup(0, bg);
        pass.setVertexBuffer(0, vbuf);
        pass.setIndexBuffer(ibuf, "uint16");
        pass.drawIndexed(6);
        pass.end();
        enc.copyTextureToBuffer(
            { texture: target },
            { buffer: readBuf, bytesPerRow: 256, rowsPerImage: 64 },
            [64, 64, 1]
        );
        device.queue.submit([enc.finish()]);
        await readBuf.mapAsync(GPUMapMode.READ);
        const px = new Uint8Array(readBuf.getMappedRange());
        const sample = (x, y) => {
            const i = y * 256 + x * 4;
            return [px[i], px[i + 1], px[i + 2], px[i + 3]];
        };
        const out = {
            badShaderErrors: badInfo.messages.length,
            tl: sample(16, 16),
            tr: sample(48, 16),
            bl: sample(16, 48),
            br: sample(48, 48),
        };
        readBuf.unmap();
        return out;
        "#,
    );

    assert!(res["badShaderErrors"].as_u64().unwrap_or(0) >= 1);
    assert_eq!(res["tl"], serde_json::json!([255, 0, 0, 255]));
    assert_eq!(res["tr"], serde_json::json!([0, 255, 0, 255]));
    assert_eq!(res["bl"], serde_json::json!([0, 0, 255, 255]));
    assert_eq!(res["br"], serde_json::json!([255, 255, 0, 255]));
}
