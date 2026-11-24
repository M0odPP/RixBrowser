use rix_js::JsVm;

#[test]
fn string_number_math_boolean_and_symbol_builtins() {
    let mut vm = JsVm::new();
    let res = vm
        .eval_script(
            r#"
            const s = "  Hello, RixBrowser!  ".trim().toLowerCase();
            const parts = s.split(", ");
            const replaced = "foo-123-bar-456".replace(/\d+/g, m => "[" + m + "]");
            const padded = "42".padStart(5, "0") + ":" + "x".repeat(3);

            const n1 = parseInt("0xff", 16) + parseFloat("3.25px");
            const fixed = (3.14159).toFixed(2);
            const hex = (255).toString(16);
            const m = Math.round(Math.hypot(3, 4)) + Math.imul(7, 6) + Math.clz32(1);

            const sym1 = Symbol.for("rix.token");
            const sym2 = Symbol.for("rix.token");
            const symOk = (sym1 === sym2) && (Symbol.keyFor(sym1) === "rix.token")
                && (Symbol("a") !== Symbol("a")) && (typeof Symbol.iterator === "symbol");

            const uri = decodeURIComponent(encodeURIComponent("a b/c?d=1"));

            [
                parts[0],
                parts[1].startsWith("rix") && parts[1].endsWith("!"),
                replaced,
                padded,
                n1,
                fixed,
                hex,
                m,
                symOk,
                uri,
                Number.isSafeInteger(42),
                Boolean("ok"),
            ].join("|");
            "#,
        )
        .expect("primitive builtins should evaluate");

    assert_eq!(
        vm.value_to_rust_string(res),
        "hello|true|foo-[123]-bar-[456]|00042:xxx|258.25|3.14|ff|78|true|a b/c?d=1|true|true"
    );
}

#[test]
fn regexp_date_json_and_extended_array_object_builtins() {
    let mut vm = JsVm::new();
    let res = vm
        .eval_script(
            r#"
            // RegExp test, exec with capture groups, and global lastIndex advancement
            const re = /(\w+)=(\d+)/g;
            const m1 = re.exec("a=10 b=25");
            const m2 = re.exec("a=10 b=25");
            const m3 = re.exec("a=10 b=25");
            const reOk = m1[1] === "a" && m1[2] === "10" && m2[1] === "b" && m2[2] === "25" && m3 === null;

            // Date UTC & ISO round-trip
            const d = new Date("2026-07-04T12:34:56.789Z");
            const iso = d.toISOString();
            const utc = Date.UTC(2026, 6, 4, 12, 34, 56, 789);
            const dateOk = iso === "2026-07-04T12:34:56.789Z" && d.getTime() === utc && d.getUTCFullYear() === 2026;

            // JSON parse with reviver + stringify + cycle rejection
            const parsed = JSON.parse('{"x":2,"arr":[1,2,3]}', (k, v) => k === "x" ? v * 10 : v);
            const jsonStr = JSON.stringify(parsed);
            let cycleThrew = false;
            try {
                const cyc = {};
                cyc.self = cyc;
                JSON.stringify(cyc);
            } catch (e) {
                cycleThrew = e.name === "TypeError";
            }

            // Extended Array & Object
            const sorted = [10, 2, 30, 5].sort((a, b) => a - b);
            const flat = [1, [2, [3]]].flat(2);
            const fromEnt = Object.fromEntries([["k1", 7], ["k2", 8]]);
            const descObj = {};
            Object.defineProperty(descObj, "ro", { value: 99, writable: false, enumerable: true, configurable: true });
            const desc = Object.getOwnPropertyDescriptor(descObj, "ro");

            [
                reOk,
                dateOk,
                jsonStr,
                cycleThrew,
                sorted.join(","),
                flat.join(","),
                fromEnt.k1 + fromEnt.k2,
                desc.value,
                desc.writable,
            ].join("|");
            "#,
        )
        .expect("regexp/date/json/array/object builtins should evaluate");

    assert_eq!(
        vm.value_to_rust_string(res),
        "true|true|{\"x\":20,\"arr\":[1,2,3]}|true|2,5,10,30|1,2,3|15|99|false"
    );
}
