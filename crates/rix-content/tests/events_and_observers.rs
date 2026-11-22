//! Integration tests for Shadow-DOM-aware Events (`EventTarget`, `Event`,
//! `CustomEvent`, `MouseEvent`, `PointerEvent`, `KeyboardEvent`, `InputEvent`)
//! and Web Observers (`MutationObserver`, `ResizeObserver`, `IntersectionObserver`).

use rix_content::ContentRuntime;

#[test]
fn shadow_dom_event_propagation_retargeting_composed_once_and_passive() {
    let mut rt = ContentRuntime::with_empty_document();
    rt.load_html_with_scripts(
        r#"<!doctype html><html><body><div id="outer"><div id="host"></div></div></body></html>"#,
    )
    .expect("load html");

    let res = rt
        .eval_script(
            r##"
            const log = [];
            const outer = document.getElementById("outer");
            const host = document.getElementById("host");
            const sr = host.attachShadow({ mode: "open" });
            sr.innerHTML = "<button id='btn'>Click</button>";
            const btn = sr.getElementById("btn");

            outer.addEventListener("ping", e => log.push("outer:cap:" + e.eventPhase + ":" + e.target.id), true);
            outer.addEventListener("ping", e => log.push("outer:bub:" + e.eventPhase + ":" + e.target.id));
            host.addEventListener("ping", e => log.push("host:bub:" + e.target.id));
            sr.addEventListener("ping", e => log.push("sr:bub:" + e.target.id));
            btn.addEventListener("ping", e => log.push("btn:target:" + e.eventPhase + ":" + e.target.id));

            // 1. Composed + bubbling event crosses ShadowRoot and retargets `e.target` to `host` outside!
            const ev1 = new CustomEvent("ping", { bubbles: true, composed: true, detail: 99 });
            btn.dispatchEvent(ev1);

            // 2. Non-composed bubbling event stops at ShadowRoot boundary (outer & host do NOT fire).
            log.push("---");
            const ev2 = new Event("ping", { bubbles: true, composed: false });
            btn.dispatchEvent(ev2);

            // 3. `once`, `passive`, `preventDefault`, `stopImmediatePropagation`, and typed events.
            let onceCount = 0;
            outer.addEventListener("tap", () => { onceCount++; }, { once: true });
            outer.dispatchEvent(new PointerEvent("tap", { clientX: 12, clientY: 34, pointerType: "touch" }));
            outer.dispatchEvent(new PointerEvent("tap", { clientX: 12, clientY: 34 }));

            const passiveTarget = new EventTarget();
            passiveTarget.addEventListener("wheel", e => { e.preventDefault(); }, { passive: true });
            const passiveNotCanceled = passiveTarget.dispatchEvent(new Event("wheel", { cancelable: true }));

            const immTarget = new EventTarget();
            let secondRan = false;
            immTarget.addEventListener("k", e => { e.stopImmediatePropagation(); });
            immTarget.addEventListener("k", () => { secondRan = true; });
            immTarget.dispatchEvent(new KeyboardEvent("k", { key: "Enter", code: "Enter" }));

            const inp = new InputEvent("input", { data: "x", inputType: "insertText" });
            const mouse = new MouseEvent("click", { clientX: 50, clientY: 60, ctrlKey: true });

            [
                log.join("|"),
                ev1.detail,
                onceCount,
                passiveNotCanceled,
                secondRan,
                inp.data + ":" + inp.inputType,
                mouse.clientX + "," + mouse.ctrlKey,
            ].join("||");
            "##,
        )
        .expect("events script");

    assert_eq!(
        rt.vm.value_to_rust_string(res),
        "outer:cap:1:host|btn:target:2:btn|sr:bub:btn|host:bub:host|outer:bub:3:host|---|btn:target:2:btn|sr:bub:btn||99||1||true||false||x:insertText||50,true"
    );
}

#[test]
fn mutation_resize_and_intersection_observers_deliver_accurate_records() {
    let mut rt = ContentRuntime::with_empty_document();
    rt.load_html_with_scripts(
        r#"<!doctype html>
        <html>
          <head>
            <style>
              body { margin: 0; }
              #box { display: block; width: 120px; height: 60px; }
              #far { display: block; width: 100px; height: 100px; margin-top: 2000px; }
            </style>
          </head>
          <body>
            <div id="box">initial</div>
            <div id="far">offscreen</div>
          </body>
        </html>"#,
    )
    .expect("load html");

    // 1. MutationObserver batches childList, attributes (+ oldValue + filter), and characterData at microtask checkpoint!
    rt.eval_script(
        r##"
        var mutLog = [];
        const box = document.getElementById("box");
        const mo = new MutationObserver(records => {
            for (const r of records) {
                mutLog.push(r.type + ":" + (r.attributeName || "") + ":" + (r.oldValue || "") + ":" + r.addedNodes.length);
            }
        });
        mo.observe(box, {
            childList: true,
            attributes: true,
            attributeOldValue: true,
            attributeFilter: ["data-state"],
            characterData: true,
            characterDataOldValue: true,
            subtree: true,
        });
        box.setAttribute("data-ignored", "nope");
        box.setAttribute("data-state", "open");
        box.setAttribute("data-state", "closed");
        const span = document.createElement("span");
        box.appendChild(span);
        box.firstChild.textContent = "updated";
        "##,
    )
    .expect("mutation setup");

    // microtask checkpoint ran automatically after eval_script!
    let mut_out = rt.eval_script("mutLog.join('|')").expect("mutLog");
    assert_eq!(
        rt.vm.value_to_rust_string(mut_out),
        "attributes:data-state::0|attributes:data-state:open:0|childList:::1|characterData::initial:0"
    );

    // 2. ResizeObserver & IntersectionObserver on live layout geometry across VSync frames!
    rt.eval_script(
        r##"
        var resizeLog = [];
        var interLog = [];
        const box2 = document.getElementById("box");
        const far = document.getElementById("far");
        const ro = new ResizeObserver(entries => {
            for (const e of entries) {
                resizeLog.push(e.target.id + ":" + e.contentRect.width + "x" + e.contentRect.height);
            }
        });
        ro.observe(box2);

        const io = new IntersectionObserver(entries => {
            for (const e of entries) {
                interLog.push(e.target.id + ":" + e.isIntersecting + ":" + Math.round(e.intersectionRatio * 100));
            }
        }, { threshold: [0, 0.5, 1.0] });
        io.observe(box2);
        io.observe(far);
        "##,
    )
    .expect("observers setup");

    // Frame 1: initial observations fire (`box` is 120x60 intersecting 100%, `far` is offscreen at y>2000 -> false:0).
    rt.fire_vsync(16.0);
    // Frame 2 with no changes: ZERO duplicate resize/intersection callbacks!
    rt.fire_vsync(32.0);
    // Resize `box` to 240x90 -> Frame 3 fires ResizeObserver with new size!
    rt.eval_script("document.getElementById('box').style.width = '240px'; document.getElementById('box').style.height = '90px';")
        .expect("resize box");
    rt.fire_vsync(48.0);

    let obs_out = rt
        .eval_script("resizeLog.join('|') + '||' + interLog.join('|')")
        .expect("obs_out");
    assert_eq!(
        rt.vm.value_to_rust_string(obs_out),
        "box:120x60|box:240x90||box:true:100|far:false:0"
    );
}
