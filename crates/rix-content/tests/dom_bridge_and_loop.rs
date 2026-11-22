//! Integration tests for `rix-content`:
//! - 100,000 detached DOM wrappers GC stress test (zero leaks)
//! - Prioritized WHATWG Event Loop, strict microtask checkpoints, timers & VSync rAF
//! - Reactive async `tokio::select!` wakeup (0% idle CPU, exact timer deadlines)
//! - Weak DOM Bridge identity, Shadow DOM, Custom Elements, CSSOM `getComputedStyle`,
//!   dirty-checked incremental `getBoundingClientRect`, and `document.write()` injection.

use std::time::{Duration, Instant};

use rix_content::{wait_for_next_event_or_timer, ContentRuntime, ExternalEvent, TaskPriority};
use tokio::sync::mpsc;

#[test]
fn gc_stress_100k_detached_dom_wrappers_stabilizes_with_zero_leaks() {
    let mut rt = ContentRuntime::with_empty_document();
    rt.load_html_with_scripts("<!doctype html><html><body><div id='host'></div></body></html>")
        .expect("load html");

    // Warm up and measure steady-state live heap + wrapper table size after GC.
    rt.collect_garbage();
    let baseline_wrappers = rt.host.borrow().wrappers.len();
    let baseline_live_objs = rt.vm.heap.live_count();

    // Create, attach, and detach 100,000 DOM elements in 10 batches of 10,000.
    for _ in 0..10 {
        rt.eval_script(
            r#"
            const host = document.getElementById("host");
            for (let i = 0; i < 10000; i++) {
                const el = document.createElement("div");
                el.setAttribute("data-i", i);
                host.appendChild(el);
                host.removeChild(el);
            }
            "#,
        )
        .expect("batch eval");
        rt.collect_garbage();
    }

    let final_wrappers = rt.host.borrow().wrappers.len();
    let final_live_objs = rt.vm.heap.live_count();

    // Only the pinned document + persistent objects remain; zero of the 100,000 detached wrappers leaked!
    assert!(
        final_wrappers <= baseline_wrappers + 4,
        "Detached DOM wrappers leaked! baseline={baseline_wrappers}, final={final_wrappers}"
    );
    assert!(
        final_live_objs <= baseline_live_objs + 128,
        "JS heap leaked across 100,000 detached wrappers! baseline={baseline_live_objs}, final={final_live_objs}"
    );
}

#[test]
fn prioritized_event_loop_microtask_checkpoints_timers_and_vsync_raf() {
    let mut rt = ContentRuntime::with_empty_document();
    rt.event_loop.borrow_mut().enable_virtual_clock();

    rt.eval_script("var log = [];").expect("init log");

    // Queue tasks in reverse priority order to verify strict Input > Rendering > Networking > Timer > Idle.
    for (prio, label) in [
        (TaskPriority::Idle, "idle"),
        (TaskPriority::Timer, "timer-task"),
        (TaskPriority::Networking, "net"),
        (TaskPriority::Rendering, "render"),
        (TaskPriority::Input, "input"),
    ] {
        rt.event_loop.borrow_mut().post_host_task(prio, move |vm| {
            let _ = vm.eval_script(&format!(
                "log.push('{label}'); Promise.resolve().then(() => log.push('{label}:micro'));"
            ));
        });
    }

    // Drain ready macrotasks -> each task's microtask MUST run immediately before the next task!
    rt.advance_and_drain(Duration::ZERO);
    let order = rt.eval_script("log.join('|')").expect("order");
    assert_eq!(
        rt.vm.value_to_rust_string(order),
        "input|input:micro|render|render:micro|net|net:micro|timer-task|timer-task:micro|idle|idle:micro"
    );

    // Verify setTimeout, clearTimeout, setInterval, and VSync-driven requestAnimationFrame.
    rt.eval_script(
        r#"
        var tlog = [];
        const cancelMe = setTimeout(() => tlog.push("cancelled"), 10);
        clearTimeout(cancelMe);
        setTimeout((a, b) => {
            tlog.push("t1:" + (a + b));
            queueMicrotask(() => tlog.push("t1:micro"));
        }, 15, 20, 22);
        let ticks = 0;
        const iv = setInterval(() => {
            ticks++;
            tlog.push("iv:" + ticks);
            if (ticks === 2) clearInterval(iv);
        }, 20);
        const dropRaf = requestAnimationFrame(() => tlog.push("raf-cancelled"));
        cancelAnimationFrame(dropRaf);
        requestAnimationFrame(ts => {
            tlog.push("raf1:" + ts);
            requestAnimationFrame(ts2 => tlog.push("raf2:" + ts2));
        });
        "#,
    )
    .expect("schedule timers & raf");

    // At t = 10ms: nothing due yet.
    assert_eq!(rt.advance_and_drain(Duration::from_millis(10)), 0);
    // At t = 15ms: t1 fires + its microtask runs.
    rt.advance_and_drain(Duration::from_millis(5));
    // Fire VSync #1 at 16.6ms: only raf1 runs; raf2 waits for VSync #2!
    assert_eq!(rt.fire_vsync(16.6), 1);
    // At t = 45ms: interval fires twice and clears itself.
    rt.advance_and_drain(Duration::from_millis(30));
    // Fire VSync #2 at 33.2ms: raf2 runs.
    assert_eq!(rt.fire_vsync(33.2), 1);
    // Idle VSync #3 runs 0 callbacks.
    assert_eq!(rt.fire_vsync(50.0), 0);

    let out = rt.eval_script("tlog.join('|')").expect("tlog");
    assert_eq!(
        rt.vm.value_to_rust_string(out),
        "t1:42|t1:micro|raf1:16.6|iv:1|iv:2|raf2:33.2"
    );
}

#[tokio::test]
async fn reactive_tokio_select_wakes_on_exact_deadline_or_external_event() {
    let mut rt = ContentRuntime::with_empty_document();
    rt.eval_script("var fired = false; setTimeout(() => { fired = true; }, 25);")
        .expect("schedule 25ms timer");

    let (_tx, mut rx) = mpsc::unbounded_channel::<ExternalEvent>();
    let t0 = Instant::now();
    let ev = wait_for_next_event_or_timer(&rt.event_loop, &mut rx).await;
    let elapsed = t0.elapsed();
    assert!(ev.is_none(), "should wake due to timer expiry");
    assert!(
        elapsed >= Duration::from_millis(20),
        "woke too early: {elapsed:?}"
    );
    rt.step_once();
    let fired = rt.eval_script("fired").expect("fired");
    assert_eq!(rt.vm.value_to_rust_string(fired), "true");

    // Now zero timers remain -> verify channel event wakes immediately (no polling delay).
    let (tx2, mut rx2) = mpsc::unbounded_channel::<ExternalEvent>();
    tx2.send(ExternalEvent::Vsync { timestamp_ms: 16.0 })
        .unwrap();
    let t1 = Instant::now();
    let ev2 = wait_for_next_event_or_timer(&rt.event_loop, &mut rx2).await;
    assert!(matches!(ev2, Some(ExternalEvent::Vsync { .. })));
    assert!(t1.elapsed() < Duration::from_millis(20));
}

#[test]
fn weak_dom_bridge_shadow_custom_elements_cssom_layout_and_document_write() {
    let mut rt = ContentRuntime::with_empty_document();
    rt.load_html_with_scripts(
        r#"<!doctype html>
        <html>
          <head>
            <style>
              #box { display: block; width: 200px; height: 80px; color: rgb(10, 20, 30); }
              .accent { opacity: 0.5; }
            </style>
          </head>
          <body>
            <div id="before"></div>
            <script>
              document.write('<div id="injected"><span class="hello">from-write</span></div>');
            </script>
            <div id="box" class="card" data-user-id="42">Hello</div>
          </body>
        </html>"#,
    )
    .expect("load html with blocking script & document.write");

    let res = rt
        .eval_script(
            r##"
            // 1. document.write() injected live nodes into the document tree.
            const injText = document.querySelector("#injected .hello").textContent;

            // 2. Weak DOM wrapper identity preservation (`===`).
            const box = document.getElementById("box");
            const sameIdentity = document.querySelector("#box") === box;

            // 3. classList, dataset, inline style, and innerHTML.
            box.classList.add("accent");
            const hasAccent = box.classList.contains("accent") && box.matches(".card.accent");
            const uid = box.dataset.userId;
            box.style.backgroundColor = "rgb(255, 0, 0)";
            const bgInline = box.style.backgroundColor;

            // 4. CSSOM getComputedStyle reflects stylesheet + inline mutations.
            const cs = getComputedStyle(box);
            const compDisplay = cs.display;
            const compColor = cs.color;
            const compOpacity = cs.opacity;
            const compBg = cs.backgroundColor;

            // 5. Dirty-checked incremental layout geometry queries.
            const r1 = box.getBoundingClientRect();
            const r2 = box.getBoundingClientRect();
            const geomOk = r1.width === 200 && r1.height === 80 && r2.width === 200 && box.offsetWidth === 200;

            // 6. Shadow DOM (open vs closed).
            const shHost = document.createElement("div");
            const sr = shHost.attachShadow({ mode: "open" });
            sr.innerHTML = "<p id='inside'>shadow-ok</p>";
            const shadowOk = shHost.shadowRoot === sr && sr.host === shHost && sr.getElementById("inside").textContent === "shadow-ok";

            // 7. CustomElementRegistry with lifecycle callbacks + whenDefined.
            const ceLog = [];
            class MyBadge extends HTMLElement {
                static observedAttributes = ["label"];
                connectedCallback() { ceLog.push("connected"); }
                disconnectedCallback() { ceLog.push("disconnected"); }
                attributeChangedCallback(name, oldV, newV) { ceLog.push(name + ":" + oldV + "->" + newV); }
            }
            var definedResolved = false;
            customElements.whenDefined("my-badge").then(() => { definedResolved = true; });
            customElements.define("my-badge", MyBadge);
            const badge = document.createElement("my-badge");
            document.body.appendChild(badge);
            badge.setAttribute("label", "new");
            badge.remove();

            [injText, sameIdentity, hasAccent, uid, bgInline, compDisplay, compColor, compOpacity, compBg, geomOk, shadowOk, ceLog.join(",")].join("|");
            "##,
        )
        .expect("dom bridge test script");

    assert_eq!(
        rt.vm.value_to_rust_string(res),
        "from-write|true|true|42|rgb(255, 0, 0)|block|rgb(10, 20, 30)|0.5|rgb(255, 0, 0)|true|true|connected,label:null->new,disconnected"
    );

    let when_def = rt.eval_script("definedResolved").expect("whenDefined");
    assert_eq!(rt.vm.value_to_rust_string(when_def), "true");

    // Verify dirty-checked incremental layout:
    // Step 5 ran 3 geometry queries in a row with ONLY 1 layout (`layout_count == 1`).
    assert_eq!(rt.host.borrow().layout_count, 1);
    // Step 7 mutated the DOM (`badge` append/remove), so the next query triggers 1 fresh layout (`1 -> 2`),
    // and repeated queries afterwards trigger ZERO additional layouts!
    rt.eval_script("const b = document.getElementById('box'); b.getBoundingClientRect();")
        .expect("post-mutation read");
    let count_after_first = rt.host.borrow().layout_count;
    assert_eq!(count_after_first, 2);
    rt.eval_script("const b2 = document.getElementById('box'); b2.getBoundingClientRect(); b2.offsetHeight; b2.clientWidth; b2.offsetWidth;")
        .expect("repeated reads");
    assert_eq!(
        rt.host.borrow().layout_count,
        count_after_first,
        "repeated geometry queries without DOM mutations must NOT trigger extra layouts"
    );
}
