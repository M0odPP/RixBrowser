//! Integration tests for genuine ECMA-402 `Intl` backed by ICU4X 2.4 and `jiff` IANA TZDB.

use rix_js::JsVm;

fn eval_str(vm: &mut JsVm, code: &str) -> String {
    let val = vm.eval_script(code).expect("eval_script failed");
    vm.value_to_rust_string(val)
}

#[test]
fn intl_canonical_locales_supported_values_and_locale_maximize_minimize() {
    let mut vm = JsVm::new();
    let res = eval_str(
        &mut vm,
        r#"
        const canon = Intl.getCanonicalLocales(["en-us", "zh-hans-cn", "EN-US"]);
        const loc = new Intl.Locale("uk", { calendar: "gregory", numeric: true });
        const maxed = loc.maximize();
        const mined = maxed.minimize();
        const tzs = Intl.supportedValuesOf("timeZone");
        const cals = Intl.supportedValuesOf("calendar");
        let badTagErr = "";
        try { Intl.getCanonicalLocales("en_US"); } catch (e) { badTagErr = e.name; }
        let badKeyErr = "";
        try { Intl.supportedValuesOf("not-a-key"); } catch (e) { badKeyErr = e.name; }
        JSON.stringify({
            canon,
            lang: loc.language,
            cal: loc.calendar,
            num: loc.numeric,
            maxedBase: maxed.baseName,
            maxedScript: maxed.script,
            maxedRegion: maxed.region,
            minedBase: mined.baseName,
            hasKyiv: tzs.includes("Europe/Kyiv"),
            hasNewYork: tzs.includes("America/New_York"),
            hasGreg: cals.includes("gregory"),
            badTagErr,
            badKeyErr,
        })
        "#,
    );
    let v: serde_json::Value = serde_json::from_str(&res).expect("json");
    assert_eq!(v["canon"], serde_json::json!(["en-US", "zh-Hans-CN"]));
    assert_eq!(v["lang"], "uk");
    assert_eq!(v["cal"], "gregory");
    assert_eq!(v["num"], true);
    assert_eq!(v["maxedBase"], "uk-Cyrl-UA");
    assert_eq!(v["maxedScript"], "Cyrl");
    assert_eq!(v["maxedRegion"], "UA");
    assert_eq!(v["minedBase"], "uk");
    assert_eq!(v["hasKyiv"], true);
    assert_eq!(v["hasNewYork"], true);
    assert_eq!(v["hasGreg"], true);
    assert_eq!(v["badTagErr"], "RangeError");
    assert_eq!(v["badKeyErr"], "RangeError");
}

#[test]
fn intl_collator_tailoring_numeric_sensitivity_and_locale_compare() {
    let mut vm = JsVm::new();
    let res = eval_str(
        &mut vm,
        r#"
        const de = new Intl.Collator("de-DE");
        const sv = new Intl.Collator("sv-SE");
        const num = new Intl.Collator("en-US", { numeric: true });
        const base = new Intl.Collator("en-US", { sensitivity: "base" });
        const files = ["file10", "file2", "file1"].sort(num.compare);
        JSON.stringify({
            deOrder: de.compare("ä", "z") < 0,
            svOrder: sv.compare("ä", "z") > 0,
            files,
            baseEq: base.compare("E", "é") === 0,
            localeCompareSv: "ä".localeCompare("z", "sv-SE") > 0,
            resolvedNumeric: num.resolvedOptions().numeric,
        })
        "#,
    );
    let v: serde_json::Value = serde_json::from_str(&res).expect("json");
    assert_eq!(v["deOrder"], true);
    assert_eq!(v["svOrder"], true);
    assert_eq!(v["files"], serde_json::json!(["file1", "file2", "file10"]));
    assert_eq!(v["baseEq"], true);
    assert_eq!(v["localeCompareSv"], true);
    assert_eq!(v["resolvedNumeric"], true);
}

#[test]
fn intl_number_format_decimal_currency_percent_compact_and_parts() {
    let mut vm = JsVm::new();
    let res = eval_str(
        &mut vm,
        r#"
        const enDec = new Intl.NumberFormat("en-US");
        const deDec = new Intl.NumberFormat("de-DE");
        const usd = new Intl.NumberFormat("en-US", { style: "currency", currency: "USD" });
        const eurDe = new Intl.NumberFormat("de-DE", { style: "currency", currency: "EUR" });
        const jpy = new Intl.NumberFormat("ja-JP", { style: "currency", currency: "JPY" });
        const pct = new Intl.NumberFormat("en-US", { style: "percent" });
        const compact = new Intl.NumberFormat("en-US", { notation: "compact" });
        const parts = enDec.formatToParts(12345.67);
        const curParts = usd.formatToParts(1234.5);
        JSON.stringify({
            en: enDec.format(1234567.89),
            de: deDec.format(1234567.89),
            usd: usd.format(1234.5),
            eurDe: eurDe.format(1234.5),
            jpy: jpy.format(1234),
            pct: pct.format(0.42),
            compact: compact.format(1500000),
            partTypes: parts.map((p) => p.type),
            curHasCurrencyPart: curParts.some((p) => p.type === "currency" && p.value === "$"),
            numProto: (12345.6).toLocaleString("en-US"),
        })
        "#,
    );
    let v: serde_json::Value = serde_json::from_str(&res).expect("json");
    assert_eq!(v["en"], "1,234,567.89");
    assert_eq!(v["de"], "1.234.567,89");
    assert_eq!(v["usd"], "$1,234.50");
    assert!(
        v["eurDe"].as_str().unwrap().contains("1.234,50")
            && v["eurDe"].as_str().unwrap().contains('€'),
        "unexpected de-DE EUR: {}",
        v["eurDe"]
    );
    assert!(
        v["jpy"].as_str().unwrap().contains("1,234") && !v["jpy"].as_str().unwrap().contains('.'),
        "JPY should have 0 fraction digits: {}",
        v["jpy"]
    );
    assert_eq!(v["pct"], "42%");
    assert_eq!(v["compact"], "1.5M");
    assert_eq!(
        v["partTypes"],
        serde_json::json!(["integer", "group", "integer", "decimal", "fraction"])
    );
    assert_eq!(v["curHasCurrencyPart"], true);
    assert_eq!(v["numProto"], "12,345.6");
}

#[test]
fn intl_date_time_format_iana_timezones_dst_and_parts() {
    let mut vm = JsVm::new();
    let res = eval_str(
        &mut vm,
        r#"
        // 2025-01-15T15:30:00Z (winter: America/New_York is UTC-5 -> 10:30, Asia/Tokyo is UTC+9 -> 00:30 next day)
        const winterMs = Date.UTC(2025, 0, 15, 15, 30, 0);
        // 2025-07-15T15:30:00Z (summer DST: America/New_York is UTC-4 -> 11:30)
        const summerMs = Date.UTC(2025, 6, 15, 15, 30, 0);

        const nyFmt = new Intl.DateTimeFormat("en-US", {
            timeZone: "America/New_York",
            year: "numeric",
            month: "short",
            day: "numeric",
            hour: "2-digit",
            minute: "2-digit",
            hourCycle: "h23",
        });
        const tokyoFmt = new Intl.DateTimeFormat("ja-JP", {
            timeZone: "Asia/Tokyo",
            year: "numeric",
            month: "numeric",
            day: "numeric",
            hour: "2-digit",
            minute: "2-digit",
            hourCycle: "h23",
        });
        const winterParts = nyFmt.formatToParts(new Date(winterMs));
        let badTzErr = "";
        try { new Intl.DateTimeFormat("en-US", { timeZone: "Mars/Olympus_Mons" }); } catch (e) { badTzErr = e.name; }

        JSON.stringify({
            nyWinter: nyFmt.format(winterMs),
            nySummer: nyFmt.format(summerMs),
            tokyo: tokyoFmt.format(winterMs),
            resolvedTz: nyFmt.resolvedOptions().timeZone,
            hasYearPart: winterParts.some((p) => p.type === "year" && p.value === "2025"),
            hasHour10: winterParts.some((p) => p.type === "hour" && p.value === "10"),
            badTzErr,
        })
        "#,
    );
    let v: serde_json::Value = serde_json::from_str(&res).expect("json");
    let ny_w = v["nyWinter"].as_str().unwrap();
    let ny_s = v["nySummer"].as_str().unwrap();
    let tokyo = v["tokyo"].as_str().unwrap();
    assert!(
        ny_w.contains("Jan") && ny_w.contains("15") && ny_w.contains("10:30"),
        "nyWinter={ny_w}"
    );
    assert!(
        ny_s.contains("Jul") && ny_s.contains("15") && ny_s.contains("11:30"),
        "nySummer={ny_s}"
    );
    assert!(
        tokyo.contains("2025") && tokyo.contains("16") && tokyo.contains("00:30"),
        "tokyo={tokyo}"
    );
    assert_eq!(v["resolvedTz"], "America/New_York");
    assert_eq!(v["hasYearPart"], true);
    assert_eq!(v["hasHour10"], true);
    assert_eq!(v["badTzErr"], "RangeError");
}

#[test]
fn intl_plural_rules_list_format_segmenter_and_relative_time() {
    let mut vm = JsVm::new();
    let res = eval_str(
        &mut vm,
        r#"
        // PluralRules
        const ukCard = new Intl.PluralRules("uk-UA");
        const enOrd = new Intl.PluralRules("en-US", { type: "ordinal" });

        // ListFormat
        const enAnd = new Intl.ListFormat("en-US", { style: "long", type: "conjunction" });
        const esAnd = new Intl.ListFormat("es-ES", { style: "long", type: "conjunction" });
        const enOr = new Intl.ListFormat("en-US", { style: "long", type: "disjunction" });
        const listParts = enAnd.formatToParts(["A", "B", "C"]);

        // Segmenter (grapheme ZWJ emoji, word isWordLike, Japanese word, sentence)
        const gSeg = new Intl.Segmenter("en", { granularity: "grapheme" });
        const graphemes = Array.from(gSeg.segment("a👨‍👩‍👧‍👦b")).map((s) => ({
            seg: s.segment,
            idx: s.index,
            hasWordLike: "isWordLike" in s,
        }));
        const wSeg = new Intl.Segmenter("en", { granularity: "word" });
        const words = Array.from(wSeg.segment("Hello, world!"))
            .filter((s) => s.isWordLike)
            .map((s) => s.segment);
        const contained = wSeg.segment("Hello, world!").containing(8);
        const sSeg = new Intl.Segmenter("en", { granularity: "sentence" });
        const sentences = Array.from(sSeg.segment("First sentence. Second sentence.")).map((s) => s.segment.trim());

        // RelativeTimeFormat
        const rtfEn = new Intl.RelativeTimeFormat("en-US", { numeric: "auto" });
        const rtfEnAlways = new Intl.RelativeTimeFormat("en-US", { numeric: "always" });
        const rtfEs = new Intl.RelativeTimeFormat("es-ES", { numeric: "auto" });
        const rtfParts = rtfEnAlways.formatToParts(3, "day");

        JSON.stringify({
            ukPlurals: [ukCard.select(1), ukCard.select(2), ukCard.select(5), ukCard.select(21)],
            enOrd: [enOrd.select(1), enOrd.select(2), enOrd.select(3), enOrd.select(4)],
            enAnd: enAnd.format(["Apple", "Banana", "Cherry"]),
            esAnd: esAnd.format(["Uno", "Dos", "Tres"]),
            enOr: enOr.format(["Red", "Blue"]),
            listPartTypes: listParts.map((p) => p.type),
            graphemes,
            words,
            containedSeg: contained.segment,
            containedIdx: contained.index,
            sentences,
            rtfEnAuto: [rtfEn.format(-1, "day"), rtfEn.format(0, "day"), rtfEn.format(1, "day")],
            rtfEnAlways: rtfEnAlways.format(5, "seconds"),
            rtfEsToday: rtfEs.format(0, "day"),
            rtfPartsHasInt: rtfParts.some((p) => p.type === "integer" && p.value === "3" && p.unit === "day"),
        })
        "#,
    );
    let v: serde_json::Value = serde_json::from_str(&res).expect("json");
    assert_eq!(
        v["ukPlurals"],
        serde_json::json!(["one", "few", "many", "one"])
    );
    assert_eq!(
        v["enOrd"],
        serde_json::json!(["one", "two", "few", "other"])
    );
    assert_eq!(v["enAnd"], "Apple, Banana, and Cherry");
    assert_eq!(v["esAnd"], "Uno, Dos y Tres");
    assert_eq!(v["enOr"], "Red or Blue");
    assert_eq!(
        v["listPartTypes"],
        serde_json::json!(["element", "literal", "element", "literal", "element"])
    );
    assert_eq!(v["graphemes"].as_array().unwrap().len(), 3);
    assert_eq!(v["graphemes"][0]["seg"], "a");
    assert_eq!(v["graphemes"][1]["seg"], "👨‍👩‍👧‍👦");
    assert_eq!(v["graphemes"][1]["idx"], 1);
    assert_eq!(v["graphemes"][1]["hasWordLike"], false);
    assert_eq!(v["graphemes"][2]["seg"], "b");
    assert_eq!(v["graphemes"][2]["idx"], 12); // "a" (1 UTF-16) + ZWJ family emoji (11 UTF-16) = 12!
    assert_eq!(v["words"], serde_json::json!(["Hello", "world"]));
    assert_eq!(v["containedSeg"], "world");
    assert_eq!(v["containedIdx"], 7);
    assert_eq!(
        v["sentences"],
        serde_json::json!(["First sentence.", "Second sentence."])
    );
    assert_eq!(
        v["rtfEnAuto"],
        serde_json::json!(["yesterday", "today", "tomorrow"])
    );
    assert_eq!(v["rtfEnAlways"], "in 5 seconds");
    assert_eq!(v["rtfEsToday"], "hoy");
    assert_eq!(v["rtfPartsHasInt"], true);
}
