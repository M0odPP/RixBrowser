//! End-to-end JS Canvas2D integration tests and 60 FPS `requestAnimationFrame`
//! GPU animation benchmark verifying zero DisplayList rebuilds and zero DOM mutations.

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
fn js_canvas2d_full_api_paths_gradients_patterns_text_image_data_and_export() {
    let _gpu = GPU_LOCK.lock().expect("gpu lock");
    let mut font_db = FontDatabase::new();
    font_db.scan_system_fonts();
    let doc = Rc::new(RefCell::new(Document::new()));
    let mut rt = ContentRuntime::new(doc, Size::new(400.0, 300.0), Arc::new(font_db));

    rt.load_html_with_scripts(
        r#"<!doctype html>
        <html><body>
            <canvas id="c" width="128" height="128"></canvas>
            <canvas id="src" width="32" height="32"></canvas>
        </body></html>"#,
    )
    .expect("load html");

    let script = r##"
        var c = document.getElementById("c");
        var ctx = c.getContext("2d");
        var sameCtx = c.getContext("2d") === ctx;
        var dimsOk = c.width === 128 && c.height === 128 && ctx.canvas === c;

        // 1. Linear & radial gradients + transforms + save/restore + clip
        var lg = ctx.createLinearGradient(0, 0, 128, 0);
        lg.addColorStop(0, "#0000ff");
        lg.addColorStop(1, "#00ff00");
        ctx.fillStyle = lg;
        ctx.fillRect(0, 0, 128, 128);

        ctx.save();
        ctx.translate(64, 64);
        ctx.rotate(0.25);
        ctx.scale(1, 1);
        var mat = ctx.getTransform();
        var matOk = Math.abs(mat.e - 64) < 0.01 && Math.abs(mat.f - 64) < 0.01;
        ctx.restore();

        // 2. Paths: arc, bezier, roundRect, isPointInPath
        ctx.beginPath();
        ctx.rect(10, 10, 30, 30);
        var hitOk = ctx.isPointInPath(20, 20) && !ctx.isPointInPath(5, 5);
        ctx.fillStyle = "#ff0000";
        ctx.fill();

        // 3. Draw another canvas via GPU drawImage
        var src = document.getElementById("src");
        var sctx = src.getContext("2d");
        sctx.fillStyle = "#ffff00";
        sctx.fillRect(0, 0, 32, 32);
        ctx.drawImage(src, 80, 10, 32, 32);

        // 4. Text measurement & rendering via rix-text
        ctx.font = "bold 16px sans-serif";
        ctx.fillStyle = "#ffffff";
        var tm = ctx.measureText("Rix");
        ctx.fillText("Rix", 10, 100);

        // 5. putImageData + getImageData roundtrip
        var patch = ctx.createImageData(2, 2);
        for (var i = 0; i < patch.data.length; i += 4) {
            patch.data[i] = 12;
            patch.data[i + 1] = 34;
            patch.data[i + 2] = 56;
            patch.data[i + 3] = 255;
        }
        ctx.putImageData(patch, 4, 110);
        var readBack = ctx.getImageData(4, 110, 1, 1);
        var pxOk = readBack.data[0] === 12 && readBack.data[1] === 34 && readBack.data[2] === 56;

        // Verify red rect pixel at (20, 20) and yellow drawImage pixel at (90, 20)
        var redPx = ctx.getImageData(20, 20, 1, 1);
        var yelPx = ctx.getImageData(90, 20, 1, 1);
        var drawOk = redPx.data[0] === 255 && redPx.data[1] === 0 && yelPx.data[0] === 255 && yelPx.data[1] === 255;

        // 6. toDataURL + toBlob
        var dataUrl = c.toDataURL("image/png");
        var blobSize = 0;
        var blobType = "";
        c.toBlob(function(b) { blobSize = b.size; blobType = b.type; });

        JSON.stringify({
            sameCtx, dimsOk, matOk, hitOk,
            textW: tm.width > 5,
            pxOk, drawOk,
            dataUrlOk: dataUrl.indexOf("data:image/png;base64,") === 0 && dataUrl.length > 80,
            blobOk: blobSize > 50 && blobType === "image/png"
        });
    "##;

    let res = rt.eval_script(script).expect("eval canvas2d script");
    let s = rt.vm.value_to_rust_string(res);
    for expected in [
        r#""sameCtx":true"#,
        r#""dimsOk":true"#,
        r#""matOk":true"#,
        r#""hitOk":true"#,
        r#""textW":true"#,
        r#""pxOk":true"#,
        r#""drawOk":true"#,
        r#""dataUrlOk":true"#,
        r#""blobOk":true"#,
    ] {
        assert!(s.contains(expected), "missing {expected} in {s}");
    }
}

#[test]
fn js_60fps_raf_canvas2d_animation_zero_display_list_rebuilds_and_zero_dom_mutations() {
    let _gpu = GPU_LOCK.lock().expect("gpu lock");
    let doc = Rc::new(RefCell::new(Document::new()));
    let mut rt = ContentRuntime::new(doc, Size::new(320.0, 200.0), Arc::new(FontDatabase::new()));
    rt.event_loop.borrow_mut().enable_virtual_clock();

    rt.load_html_with_scripts(
        r#"<!doctype html>
        <html>
          <head><style>body { margin: 0; padding: 0; background: #101018; }</style></head>
          <body><canvas id="anim" width="240" height="120"></canvas></body>
        </html>"#,
    )
    .expect("load html");

    rt.eval_script(
        r##"
        var canvas = document.getElementById("anim");
        var ctx = canvas.getContext("2d");
        var frameCount = 0;
        function tick(ts) {
            ctx.clearRect(0, 0, 240, 120);
            ctx.fillStyle = "#0b132b";
            ctx.fillRect(0, 0, 240, 120);
            ctx.beginPath();
            ctx.arc(20 + frameCount * 3, 60, 14, 0, Math.PI * 2);
            ctx.fillStyle = "#00f5d4";
            ctx.fill();
            frameCount += 1;
            if (frameCount < 60) {
                requestAnimationFrame(tick);
            }
        }
        requestAnimationFrame(tick);
        "##,
    )
    .expect("install raf loop");

    let pipeline = PipelineId(1);
    let mut compositor = Compositor::new(Size::new(320.0, 200.0), 1.0);
    let mut gpu = VelloRenderer::new();

    // Initial layout & DisplayList build (1st and ONLY DisplayList build!).
    let (dl_built_init, _) =
        rt.sync_frame_to_compositor(pipeline, 1, &mut compositor, Some(&mut gpu));
    assert!(
        dl_built_init,
        "initial frame must build the page DisplayList once"
    );
    assert!(compositor.compose().is_some());
    assert_eq!(rt.host.borrow().display_list_build_count, 1);
    assert_eq!(compositor.layer(pipeline).unwrap().encode_generation(), 1);

    let initial_dom_rev = rt.host.borrow().document.borrow().revision();
    let initial_style_rev = rt.host.borrow().document.borrow().style_revision();
    let mut frame1_rgba = Vec::new();
    let mut frame60_rgba = Vec::new();
    let mut dl_rebuilds_during_anim = 0u64;

    // Drive 60 consecutive 60Hz VSync frames via JS requestAnimationFrame!
    for f in 0..60u32 {
        let ran = rt.fire_vsync(f64::from(f + 1) * 16.666);
        assert_eq!(ran, 1);
        let (dl_rebuilt, presented) = rt.sync_frame_to_compositor(
            pipeline,
            u64::from(f + 2),
            &mut compositor,
            Some(&mut gpu),
        );
        if dl_rebuilt {
            dl_rebuilds_during_anim += 1;
        }
        assert_eq!(
            presented, 1,
            "each animation frame should swap 1 dirty canvas surface"
        );
        let composed = compositor
            .compose()
            .expect("compositor updates external surface");
        if f == 0 {
            frame1_rgba = gpu
                .render_to_rgba_blocking(composed, 320, 200, Color::BLACK)
                .expect("frame 1 render");
        } else if f == 59 {
            frame60_rgba = gpu
                .render_to_rgba_blocking(composed, 320, 200, Color::BLACK)
                .expect("frame 60 render");
        }
    }

    // Issue #18 Acceptance Criteria: ZERO DisplayList rebuilds, ZERO DOM mutations, ONE GPU texture!
    assert_eq!(
        dl_rebuilds_during_anim, 0,
        "60 FPS animation must trigger 0 DisplayList rebuilds"
    );
    assert_eq!(rt.host.borrow().display_list_build_count, 1);
    assert_eq!(compositor.layer(pipeline).unwrap().encode_generation(), 1);
    assert_eq!(
        rt.host.borrow().document.borrow().revision(),
        initial_dom_rev,
        "60 FPS Canvas2D animation must produce 0 DOM mutations"
    );
    assert_eq!(
        rt.host.borrow().document.borrow().style_revision(),
        initial_style_rev
    );
    assert_eq!(gpu.gpu_surface_allocations(), 1);

    // Confirm animated cyan ball genuinely moved from x=20 (frame 1) to x=197 (frame 60)!
    let f1_start = pixel_at(&frame1_rgba, 320, 20, 60);
    let f60_start = pixel_at(&frame60_rgba, 320, 20, 60);
    let f60_end = pixel_at(&frame60_rgba, 320, 197, 60);
    assert!(
        f1_start[1] > 200,
        "ball must be at start in frame 1, got {f1_start:?}"
    );
    assert!(
        f60_start[1] < 40,
        "ball must have moved away from start in frame 60, got {f60_start:?}"
    );
    assert!(
        f60_end[1] > 200,
        "ball must reach end position in frame 60, got {f60_end:?}"
    );
}
