//! Official W3C Web Platform Tests (WPT) conformance runner
//! (`cargo xtask wpt-official --suite <subpath> [--wpt-dir <path>] [--limit <n>]`).
//!
//! Executes unmodified upstream WPT `.html` tests from a local WPT checkout:
//! - `testharness.js` script tests (DOM, Shadow DOM, Custom Elements, CSSOM, CSS Values/Selectors, HTML)
//! - Visual `<link rel="match"|"mismatch">` reftests (CSS Flexbox, Grid, Position, Tables, Display, Text, Cascade)

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use rayon::prelude::*;
use rix_content::ContentRuntime;
use rix_core::Size;
use rix_css::{CascadeOrigin, MediaContext, StyleContext, Stylesheet};
use rix_dom::{Document, NodeData};
use rix_html::parse_html;
use rix_ipc::PipelineId;
use rix_layout::LayoutEngine;
use rix_net::{FetchEngine, HttpClient};
use rix_render::{encode_display_list, EncodeOptions, ResourceRegistry, VelloRenderer};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use vello::peniko::Color;
use vello::Scene;

use crate::reftest::{compare_rgba, extract_style_blocks};

/// Spec-compatible WPT `testharness.js` bridge capturing exact subtest pass/fail outcomes.
const WPT_TESTHARNESS_SHIM: &str = r#"
(function(global) {
    const results = [];
    let harnessStatus = 0;
    let harnessErrors = [];
    let pendingAsync = 0;

    function format_value(val) {
        if (val === null) return "null";
        if (val === undefined) return "undefined";
        if (typeof val === "string") return JSON.stringify(val);
        if (typeof val === "number") return Object.is(val, -0) ? "-0" : String(val);
        if (typeof val === "bigint") return String(val) + "n";
        if (typeof val === "symbol") return String(val);
        try { return String(val); } catch (_) { return "<object>"; }
    }

    function AssertionError(msg) {
        this.name = "AssertionError";
        this.message = msg || "Assertion failed";
    }
    AssertionError.prototype = Object.create(Error.prototype);
    AssertionError.prototype.constructor = AssertionError;
    AssertionError.prototype.toString = function() {
        return this.name + ": " + this.message;
    };

    function OptionalFeatureUnsupportedError(msg) {
        this.name = "OptionalFeatureUnsupportedError";
        this.message = msg || "Optional feature unsupported";
    }
    OptionalFeatureUnsupportedError.prototype = Object.create(Error.prototype);
    OptionalFeatureUnsupportedError.prototype.constructor = OptionalFeatureUnsupportedError;
    global.OptionalFeatureUnsupportedError = OptionalFeatureUnsupportedError;

    function fail(msg) { throw new AssertionError(msg); }

    function assert_true(actual, desc) {
        if (actual !== true) fail((desc ? desc + ": " : "") + "expected true got " + format_value(actual));
    }
    function assert_false(actual, desc) {
        if (actual !== false) fail((desc ? desc + ": " : "") + "expected false got " + format_value(actual));
    }
    function assert_equals(actual, expected, desc) {
        if (!Object.is(actual, expected)) {
            if (!(typeof actual === "number" && typeof expected === "number" && actual === expected && actual === 0)) {
                fail((desc ? desc + ": " : "") + "expected " + format_value(expected) + " but got " + format_value(actual));
            }
        }
    }
    function assert_not_equals(actual, expected, desc) {
        if (Object.is(actual, expected) || (actual === 0 && expected === 0)) {
            fail((desc ? desc + ": " : "") + "got disallowed value " + format_value(actual));
        }
    }
    function assert_approx_equals(actual, expected, epsilon, desc) {
        if (typeof actual !== "number" || Math.abs(actual - expected) > epsilon) {
            fail((desc ? desc + ": " : "") + "expected " + expected + " +/- " + epsilon + " got " + format_value(actual));
        }
    }
    function assert_less_than(a, b, desc) {
        if (!(a < b)) fail((desc ? desc + ": " : "") + format_value(a) + " not < " + format_value(b));
    }
    function assert_greater_than(a, b, desc) {
        if (!(a > b)) fail((desc ? desc + ": " : "") + format_value(a) + " not > " + format_value(b));
    }
    function assert_less_than_equal(a, b, desc) {
        if (!(a <= b)) fail((desc ? desc + ": " : "") + format_value(a) + " not <= " + format_value(b));
    }
    function assert_greater_than_equal(a, b, desc) {
        if (!(a >= b)) fail((desc ? desc + ": " : "") + "expected " + format_value(a) + " >= " + format_value(b));
    }
    function assert_between_exclusive(actual, lower, upper, desc) {
        if (!(actual > lower && actual < upper)) {
            fail((desc ? desc + ": " : "") + "expected " + format_value(actual) + " strictly between " + format_value(lower) + " and " + format_value(upper));
        }
    }
    function assert_between_inclusive(actual, lower, upper, desc) {
        if (!(actual >= lower && actual <= upper)) {
            fail((desc ? desc + ": " : "") + "expected " + format_value(actual) + " between " + format_value(lower) + " and " + format_value(upper));
        }
    }
    function assert_array_equals(actual, expected, desc) {
        if (!actual || actual.length !== expected.length) {
            fail((desc ? desc + ": " : "") + "lengths differ: " + (actual ? actual.length : "null") + " vs " + expected.length);
        }
        for (let i = 0; i < expected.length; i++) {
            assert_equals(actual[i], expected[i], (desc ? desc + " " : "") + "at index " + i);
        }
    }
    function assert_array_approx_equals(actual, expected, epsilon, desc) {
        if (!actual || actual.length !== expected.length) {
            fail((desc ? desc + ": " : "") + "lengths differ: " + (actual ? actual.length : "null") + " vs " + expected.length);
        }
        for (let i = 0; i < expected.length; i++) {
            assert_approx_equals(actual[i], expected[i], epsilon, (desc ? desc + " " : "") + "at index " + i);
        }
    }
    function assert_object_equals(actual, expected, desc) {
        if (typeof actual !== "object" || actual === null) {
            fail((desc ? desc + ": " : "") + "value is " + format_value(actual) + ", expected object");
        }
        function check_equal(a, e, stack) {
            stack.push(a);
            for (var p in a) {
                if (!e || !Object.prototype.hasOwnProperty.call(e, p)) {
                    fail((desc ? desc + ": " : "") + "unexpected property " + p);
                }
                if (typeof a[p] === "object" && a[p] !== null) {
                    if (stack.indexOf(a[p]) === -1) check_equal(a[p], e[p], stack);
                } else {
                    assert_equals(a[p], e[p], (desc ? desc + " " : "") + "property " + p);
                }
            }
            for (var p in e) {
                if (!Object.prototype.hasOwnProperty.call(a, p)) {
                    fail((desc ? desc + ": " : "") + "expected property " + p + " missing");
                }
            }
            stack.pop();
        }
        check_equal(actual, expected, []);
    }
    function assert_in_array(actual, expected, desc) {
        if (!expected || expected.indexOf(actual) === -1) {
            fail((desc ? desc + ": " : "") + format_value(actual) + " not in array");
        }
    }
    function assert_any(assert_func, actual, expected_array, ...extra) {
        let errors = [];
        for (let i = 0; i < expected_array.length; i++) {
            try {
                assert_func(actual, expected_array[i], ...extra);
                return;
            } catch (e) {
                errors.push(e);
            }
        }
        fail("assert_any failed: " + (errors[0] ? errors[0].message : format_value(actual)));
    }
    function assert_own_property(obj, prop, desc) {
        if (!Object.prototype.hasOwnProperty.call(obj, prop)) {
            fail((desc ? desc + ": " : "") + "missing own property " + format_value(prop));
        }
    }
    function assert_not_own_property(obj, prop, desc) {
        if (Object.prototype.hasOwnProperty.call(obj, prop)) {
            fail((desc ? desc + ": " : "") + "unexpected own property " + format_value(prop));
        }
    }
    function assert_inherits(obj, prop, desc) {
        if (!(prop in obj) || Object.prototype.hasOwnProperty.call(obj, prop)) {
            fail((desc ? desc + ": " : "") + "expected inherited property " + prop);
        }
    }
    function assert_idl_attribute(obj, prop, desc) {
        if (!(prop in obj)) fail((desc ? desc + ": " : "") + "missing IDL attribute " + prop);
    }
    function assert_readonly(obj, prop, desc) {
        const before = obj[prop];
        try { obj[prop] = "__rix_ro_probe__"; } catch (_) {}
        if (obj[prop] !== before) {
            fail((desc ? desc + ": " : "") + "property " + prop + " is not readonly");
        }
    }
    function assert_class_string(obj, expected, desc) {
        const actual = Object.prototype.toString.call(obj).slice(8, -1);
        assert_equals(actual, expected, desc);
    }
    function assert_regexp_match(actual, expected, desc) {
        if (!expected.test(actual)) {
            fail((desc ? desc + ": " : "") + format_value(actual) + " did not match " + expected);
        }
    }
    function assert_unreached(desc) {
        fail("Reached unreachable code" + (desc ? ": " + desc : ""));
    }
    function assert_throws_js(constructor, fn, desc) {
        let threw = false;
        try { fn(); } catch (e) {
            threw = true;
            if (constructor && !(e instanceof constructor) && (!e || e.name !== constructor.name)) {
                fail((desc ? desc + ": " : "") + "expected " + constructor.name + " got " + (e && e.name ? e.name : e));
            }
        }
        if (!threw) fail((desc ? desc + ": " : "") + "did not throw");
    }
    function assert_throws_dom(type, fnOrCtor, maybeFn, desc) {
        const fn = typeof maybeFn === "function" ? maybeFn : fnOrCtor;
        const message = typeof maybeFn === "function" ? desc : maybeFn;
        const expectedCode =
            typeof type === "string" &&
            typeof DOMException !== "undefined" &&
            typeof DOMException[type] === "number"
                ? DOMException[type]
                : null;
        let threw = false;
        try { fn(); } catch (e) {
            threw = true;
            if (typeof type === "string" && e && e.name && e.name !== type && e.code !== type && (expectedCode === null || e.code !== expectedCode)) {
                fail((message ? message + ": " : "") + "expected DOMException " + type + " got " + e.name);
            } else if (typeof type === "number" && e && e.code !== type) {
                fail((message ? message + ": " : "") + "expected DOMException code " + type + " got " + (e.code !== undefined ? e.code : e.name));
            }
        }
        if (!threw) fail((message ? message + ": " : "") + "did not throw DOMException " + type);
    }
    function assert_throws_exactly(expected, fn, desc) {
        let threw = false;
        try { fn(); } catch (e) {
            threw = true;
            assert_equals(e, expected, desc);
        }
        if (!threw) fail((desc ? desc + ": " : "") + "did not throw");
    }
    function assert_implements(cond, desc) {
        if (!cond) fail("Not implemented" + (desc ? ": " + desc : ""));
    }
    function assert_implements_optional(cond, desc) {
        if (!cond) fail("Optional feature not implemented" + (desc ? ": " + desc : ""));
    }

    function TestObj(name) {
        this.name = name || "(unnamed)";
        this._cleanups = [];
        this._done = false;
        this._status = 0;
        this.status = this.NOTRUN;
        this._message = null;
    }
    TestObj.prototype.PASS = 0;
    TestObj.prototype.FAIL = 1;
    TestObj.prototype.TIMEOUT = 2;
    TestObj.prototype.NOTRUN = 3;
    TestObj.prototype.PRECONDITION_FAILED = 4;
    TestObj.prototype.add_cleanup = function(fn) { this._cleanups.push(fn); };
    TestObj.prototype._run_cleanups = function() {
        while (this._cleanups.length) {
            try { this._cleanups.pop()(); } catch (_) {}
        }
    };
    TestObj.prototype.step = function(fn, thisObj) {
        if (this._done) return;
        const args = Array.prototype.slice.call(arguments, 2);
        try {
            return fn.apply(thisObj || this, args);
        } catch (e) {
            this._fail(e);
        }
    };
    TestObj.prototype.step_func = function(fn, thisObj) {
        const self = this;
        return function() {
            if (self._done) return;
            try { return fn.apply(thisObj || self, arguments); }
            catch (e) { self._fail(e); }
        };
    };
    TestObj.prototype.step_func_done = function(fn, thisObj) {
        const self = this;
        return function() {
            if (self._done) return;
            try {
                if (fn) fn.apply(thisObj || self, arguments);
                self.done();
            } catch (e) { self._fail(e); }
        };
    };
    TestObj.prototype.unreached_func = function(desc) {
        return this.step_func(function() { assert_unreached(desc); });
    };
    TestObj.prototype.step_timeout = function(fn, timeout) {
        const self = this;
        return setTimeout(function() { self.step(fn); }, timeout || 0);
    };
    globalThis.step_timeout = function(fn, timeout) {
        var args = Array.prototype.slice.call(arguments, 2);
        return setTimeout(function() { fn.apply(globalThis, args); }, timeout || 0);
    };
    TestObj.prototype._fail = function(e) {
        if (this._done) return;
        this._done = true;
        this._status = 1;
        this.status = this.FAIL;
        this._message = e && e.message ? (e.name ? e.name + ": " + e.message : e.message) : String(e);
        this._run_cleanups();
        if (this._isAsync) pendingAsync = Math.max(0, pendingAsync - 1);
        results.push({ name: this.name, pass: false, message: this._message });
    };
    TestObj.prototype.done = function() {
        if (this._done) return;
        this._done = true;
        this._status = 0;
        this.status = this.PASS;
        this._run_cleanups();
        if (this._isAsync) pendingAsync = Math.max(0, pendingAsync - 1);
        results.push({ name: this.name, pass: true, message: "" });
    };

    function test(fn, name) {
        const t = new TestObj(name);
        try {
            fn.call(t, t);
            if (!t._done) t.done();
        } catch (e) {
            t._fail(e);
        }
    }
    function async_test(fn, name) {
        if (typeof fn === "string") { name = fn; fn = null; }
        const t = new TestObj(name);
        t._isAsync = true;
        pendingAsync++;
        if (fn) {
            try { fn.call(t, t); } catch (e) { t._fail(e); }
        }
        return t;
    }
    let promiseChain = Promise.resolve();
    function promise_test(fn, name) {
        const t = new TestObj(name);
        t._isAsync = true;
        pendingAsync++;
        promiseChain = promiseChain.then(function() {
            try {
                return Promise.resolve(fn.call(t, t)).then(
                    function() { t.done(); },
                    function(e) { t._fail(e); }
                );
            } catch (e) {
                t._fail(e);
            }
        });
    }
    function promise_setup(fn, props) {
        promiseChain = promiseChain.then(function() {
            return Promise.resolve().then(fn);
        }).catch(function(e) {
            harnessStatus = 1;
            harnessErrors.push(String(e));
        });
    }
    function promise_rejects_js(t, ctor, promise, desc) {
        return promise.then(
            function() { assert_unreached((desc ? desc + ": " : "") + "promise fulfilled instead of rejecting"); },
            function(e) {
                if (ctor && !(e instanceof ctor) && (!e || e.name !== ctor.name)) {
                    fail((desc ? desc + ": " : "") + "expected " + ctor.name + " got " + (e && e.name ? e.name : e));
                }
            }
        );
    }
    function promise_rejects_dom(t, type, ctorOrPromise, maybePromise, maybeDesc) {
        let promise = ctorOrPromise;
        let desc = maybePromise;
        if (maybePromise && typeof maybePromise.then === "function") {
            promise = maybePromise;
            desc = maybeDesc;
        }
        const expectedCode =
            typeof type === "string" &&
            typeof DOMException !== "undefined" &&
            typeof DOMException[type] === "number"
                ? DOMException[type]
                : null;
        return promise.then(
            function() { assert_unreached((desc ? desc + ": " : "") + "promise fulfilled instead of rejecting"); },
            function(e) {
                if (typeof type === "string" && e && e.name && e.name !== type && e.code !== type && (expectedCode === null || e.code !== expectedCode)) {
                    fail((desc ? desc + ": " : "") + "expected DOMException " + type + " got " + e.name);
                } else if (typeof type === "number" && e && e.code !== type) {
                    fail((desc ? desc + ": " : "") + "expected DOMException code " + type + " got " + (e.code !== undefined ? e.code : e.name));
                }
            }
        );
    }
    function promise_rejects_exactly(t, expected, promise, desc) {
        return promise.then(
            function() { assert_unreached((desc ? desc + ": " : "") + "promise fulfilled instead of rejecting"); },
            function(e) {
                assert_equals(e, expected, desc);
            }
        );
    }
    function on_event(obj, event, cb) {
        obj.addEventListener(event, cb, false);
    }
    function EventWatcher(testObj, watchedNode, eventTypes, timeoutPromise) {
        if (typeof eventTypes === "string") eventTypes = [eventTypes];
        let waitingFor = null;
        let recordedEvents = null;
        const handler = function(evt) {
            if (!waitingFor) return;
            if (evt.type !== waitingFor.types[0]) {
                const rejectFunc = waitingFor.reject;
                waitingFor = null;
                recordedEvents = null;
                rejectFunc(new AssertionError("Expected " + (evt && evt.type) + " to match expected event"));
                return;
            }
            if (Array.isArray(recordedEvents)) {
                recordedEvents.push(evt);
            }
            if (waitingFor.types.length > 1) {
                waitingFor.types.shift();
                return;
            }
            const resolveFunc = waitingFor.resolve;
            waitingFor = null;
            const result = recordedEvents || evt;
            recordedEvents = null;
            resolveFunc(result);
        };
        for (const ty of eventTypes) watchedNode.addEventListener(ty, handler, false);
        this.stop_watching = function() {
            for (const ty of eventTypes) watchedNode.removeEventListener(ty, handler, false);
        };
        if (testObj && typeof testObj.add_cleanup === "function") {
            testObj.add_cleanup(this.stop_watching);
        }
        this.wait_for = function(types, options) {
            if (waitingFor) {
                return Promise.reject(new Error("Already waiting for an event or events"));
            }
            if (typeof types === "string") types = [types];
            else types = types.slice();
            if (options && options.record === "all") {
                recordedEvents = [];
            }
            return new Promise(function(resolve, reject) {
                if (timeoutPromise) {
                    const tp = typeof timeoutPromise === "function" ? timeoutPromise() : timeoutPromise;
                    if (tp && typeof tp.then === "function") {
                        tp.then(function() {
                            if (!waitingFor || waitingFor.resolve !== resolve) return;
                            const missing = waitingFor.types.join(", ");
                            waitingFor = null;
                            recordedEvents = null;
                            reject(new AssertionError("Timed out waiting for " + missing));
                        });
                    }
                }
                waitingFor = { types: types, resolve: resolve, reject: reject };
            });
        };
    }
    function generate_tests(fn, params) {
        if (!Array.isArray(params)) return;
        for (let i = 0; i < params.length; i++) {
            const entry = params[i];
            const name = entry[0];
            const rest = entry.slice(1);
            test(function() { fn.apply(this, rest); }, name);
        }
    }
    function setup(fnOrProps, maybeProps) {
        if (typeof fnOrProps === "function") {
            try { fnOrProps(); } catch (e) { harnessStatus = 1; harnessErrors.push(String(e)); }
        }
    }
    function done() {}
    function add_start_callback(fn) { try { fn(); } catch (_) {} }
    function add_completion_callback() {}
    function add_result_callback() {}
    function fetch_tests_from_worker(worker) {
        const t = async_test("Worker tests");
        const port = (worker && worker.port) ? worker.port : worker;
        if (!port || typeof port.addEventListener !== "function") {
            t.step(function() { assert_unreached("Invalid worker for fetch_tests_from_worker"); });
            return;
        }
        if (typeof port.start === "function") port.start();
        port.addEventListener("message", function(e) {
            const data = e.data;
            if (!data || typeof data !== "object") return;
            if (data.type === "complete") {
                const sub = Array.isArray(data.tests) ? data.tests : [];
                for (let i = 0; i < sub.length; i++) {
                    results.push({
                        name: String(sub[i].name || "(worker subtest)"),
                        pass: sub[i].status === 0,
                        message: sub[i].message ? String(sub[i].message) : ""
                    });
                }
                if (data.status && data.status.status !== 0) {
                    harnessStatus = 1;
                    harnessErrors.push(String(data.status.message || "Worker harness error"));
                }
                t._done = true;
                pendingAsync = Math.max(0, pendingAsync - 1);
            }
        });
        if (worker && typeof worker.addEventListener === "function") {
            worker.addEventListener("error", function(e) {
                t._fail(new Error(e && e.message ? e.message : "Worker error"));
            });
        }
    }

    global.test = test;
    global.async_test = async_test;
    global.promise_test = promise_test;
    global.promise_setup = promise_setup;
    global.generate_tests = generate_tests;
    global.promise_rejects_js = promise_rejects_js;
    global.promise_rejects_dom = promise_rejects_dom;
    global.promise_rejects_exactly = promise_rejects_exactly;
    global.on_event = on_event;
    global.EventWatcher = EventWatcher;
    global.setup = setup;
    global.done = done;
    global.fetch_tests_from_worker = fetch_tests_from_worker;
    global.add_start_callback = add_start_callback;
    global.add_completion_callback = add_completion_callback;
    global.add_result_callback = add_result_callback;
    global.format_value = format_value;
    global.assert_true = assert_true;
    global.assert_false = assert_false;
    global.assert_equals = assert_equals;
    global.assert_not_equals = assert_not_equals;
    global.assert_approx_equals = assert_approx_equals;
    global.assert_less_than = assert_less_than;
    global.assert_greater_than = assert_greater_than;
    global.assert_less_than_equal = assert_less_than_equal;
    global.assert_greater_than_equal = assert_greater_than_equal;
    global.assert_between_exclusive = assert_between_exclusive;
    global.assert_between_inclusive = assert_between_inclusive;
    global.assert_array_equals = assert_array_equals;
    global.assert_array_approx_equals = assert_array_approx_equals;
    global.assert_object_equals = assert_object_equals;
    global.assert_in_array = assert_in_array;
    global.assert_any = assert_any;
    global.assert_own_property = assert_own_property;
    global.assert_not_own_property = assert_not_own_property;
    global.assert_inherits = assert_inherits;
    global.assert_idl_attribute = assert_idl_attribute;
    global.assert_readonly = assert_readonly;
    global.assert_class_string = assert_class_string;
    global.assert_regexp_match = assert_regexp_match;
    global.assert_unreached = assert_unreached;
    global.assert_throws_js = assert_throws_js;
    global.assert_throws_dom = assert_throws_dom;
    global.assert_throws_exactly = assert_throws_exactly;
    global.assert_implements = assert_implements;
    global.assert_implements_optional = assert_implements_optional;
    global.GLOBAL = {
        isWindow: function() { return true; },
        isWorker: function() { return false; },
        isShadowRealm: function() { return false; }
    };
    global.__rix_wpt_pending = function() { return pendingAsync; };
    global.__rix_wpt_summary = function() {
        let passed = 0, failed = 0, firstErr = "";
        const failedList = [];
        for (let i = 0; i < results.length; i++) {
            if (results[i].pass) passed++;
            else {
                failed++;
                const msg = results[i].message || "";
                if (!firstErr) firstErr = results[i].name + " => " + msg;
                if (failedList.length < 256) {
                    failedList.push([String(results[i].name || ""), String(msg).slice(0, 200)]);
                }
            }
        }
        return JSON.stringify({
            total: results.length,
            passed: passed,
            failed: failed,
            pending: pendingAsync,
            status: harnessStatus,
            first_error: firstErr || (harnessErrors[0] || ""),
            failed_subtests: failedList
        });
    };
})(globalThis);
"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OfficialWptKind {
    All,
    Testharness,
    Reftest,
}

#[derive(Debug, Clone)]
pub struct OfficialWptOptions {
    pub wpt_dir: PathBuf,
    pub suite: String,
    pub kind: OfficialWptKind,
    pub limit: Option<usize>,
    pub report_json: Option<PathBuf>,
    pub compare_firefox: bool,
    pub firefox_bin: PathBuf,
}

/// A single WPT subtest that PASSES in Mozilla Firefox 161.0 while FAILING in RixBrowser.
#[derive(Debug, Clone, Default, Serialize)]
pub struct FirefoxWptGap {
    pub file: String,
    pub subtest: String,
    pub rix_error: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct OfficialWptFailure {
    pub file: String,
    pub kind: String,
    pub reason: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct OfficialWptReport {
    pub suite: String,
    pub files_total: usize,
    pub files_passed: usize,
    pub files_failed: usize,
    pub files_panicked: usize,
    pub reftests_total: usize,
    pub reftests_passed: usize,
    pub testharness_files_total: usize,
    pub testharness_files_passed: usize,
    pub subtests_total: usize,
    pub subtests_passed: usize,
    pub file_pass_rate_pct: f64,
    pub subtest_pass_rate_pct: f64,
    pub firefox_checked_files: usize,
    pub firefox_pass_rix_fail_files: usize,
    pub firefox_pass_rix_fail_subtests: usize,
    pub firefox_only_gaps: Vec<FirefoxWptGap>,
    pub top_failure_reasons: Vec<(String, usize)>,
    pub sample_failures: Vec<OfficialWptFailure>,
}

#[derive(Debug, Deserialize)]
struct JsWptSummary {
    total: usize,
    passed: usize,
    failed: usize,
    pending: usize,
    status: i32,
    first_error: String,
    #[serde(default)]
    failed_subtests: Vec<(String, String)>,
}

#[must_use]
pub fn default_official_wpt_dir() -> PathBuf {
    if let Ok(base) = std::env::var("RIX_CONFORMANCE_DIR") {
        return PathBuf::from(base).join("wpt");
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".cache/rix-conformance/wpt")
}

fn collect_wpt_html_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    if dir.is_file() {
        out.push(dir.to_path_buf());
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if path.is_dir() {
            if matches!(
                name,
                "support" | "reference" | "references" | "resources" | "crashtests" | "tools"
            ) {
                continue;
            }
            collect_wpt_html_files(&path, out)?;
        } else if ((name.ends_with(".html") || name.ends_with(".htm"))
            && !name.ends_with("-ref.html")
            && !name.ends_with("-ref.htm")
            && !name.ends_with("-notref.html")
            && !name.ends_with("-manual.html")
            && !name.contains("manual."))
            || name.ends_with(".any.js")
            || name.ends_with(".window.js")
        {
            out.push(path);
        }
    }
    Ok(())
}

/// Synthesizes a standard WPT testharness HTML wrapper for `*.any.js` and `*.window.js` tests,
/// honoring `// META: global=...` and `// META: script=...` directives per WPT test specification.
/// Returns `None` if `// META: global=...` excludes the `window` global.
fn synthesize_wpt_js_html(path: &Path, js_src: &str) -> Option<String> {
    let fname = path.file_name().and_then(|n| n.to_str())?;
    let is_any = fname.ends_with(".any.js");
    let is_window = fname.ends_with(".window.js");
    if !is_any && !is_window {
        return None;
    }

    let mut scripts: Vec<String> = Vec::new();
    let mut globals_spec: Option<String> = None;
    for line in js_src.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Some(rest) = trimmed
            .strip_prefix("//")
            .map(str::trim_start)
            .and_then(|s| s.strip_prefix("META:"))
        else {
            // WPT metadata comments appear only at the top of the file.
            if !trimmed.starts_with("//") {
                break;
            }
            continue;
        };
        let meta = rest.trim();
        if let Some(val) = meta.strip_prefix("global=") {
            globals_spec = Some(val.trim().to_string());
        } else if let Some(val) = meta.strip_prefix("script=") {
            let s = val.trim();
            if !s.is_empty() {
                scripts.push(s.to_string());
            }
        }
    }

    if is_any {
        if let Some(ref g) = globals_spec {
            let tokens: Vec<&str> = g
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect();
            let includes_window = tokens.iter().any(|t| *t == "window" || *t == "default")
                || (tokens.iter().all(|t| t.starts_with('!')) && !tokens.contains(&"!window"));
            if !includes_window {
                return None;
            }
        }
    }

    let mut html = String::from(
        "<!doctype html><meta charset=utf-8>\n\
         <script src=\"/resources/testharness.js\"></script>\n\
         <script src=\"/resources/testharnessreport.js\"></script>\n\
         <div id=log></div>\n",
    );
    for s in scripts {
        html.push_str(&format!("<script src=\"{s}\"></script>\n"));
    }
    html.push_str(&format!("<script src=\"{fname}\"></script>\n"));
    Some(html)
}

fn resolve_wpt_path(wpt_root: &Path, doc_path: &Path, href: &str) -> PathBuf {
    let clean = href.split(['?', '#']).next().unwrap_or(href).trim();
    if clean == "/resources/WebIDLParser.js" {
        return wpt_root.join("resources/webidl2/lib/webidl2.js");
    }
    if let Some(rel) = clean.strip_prefix('/') {
        wpt_root.join(rel)
    } else {
        doc_path.parent().unwrap_or(wpt_root).join(clean)
    }
}

/// Extracts static ES module specifiers from `import ... from "..."`, `export ... from "..."`,
/// and bare `import "..."` statements so offline WPT tests can preload local modules.
fn extract_es_module_specifiers(code: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = code.as_bytes();
    let len = bytes.len();
    let mut i = 0usize;
    while i < len {
        // Skip line comments
        if i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'/' {
            while i < len && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        // Skip block comments
        if i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < len && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i += 2;
            continue;
        }
        // Match `from` or `import` followed by whitespace and string literal (pure byte slicing
        // so non-ASCII UTF-8 bytes never panic on char boundary checks).
        let matched_kw = if bytes[i..].starts_with(b"from")
            && (i == 0
                || !(bytes[i - 1].is_ascii_alphanumeric()
                    || bytes[i - 1] == b'_'
                    || bytes[i - 1] == b'$'))
            && i + 4 < len
            && (bytes[i + 4].is_ascii_whitespace() || bytes[i + 4] == b'"' || bytes[i + 4] == b'\'')
        {
            Some(4usize)
        } else if bytes[i..].starts_with(b"import")
            && (i == 0
                || !(bytes[i - 1].is_ascii_alphanumeric()
                    || bytes[i - 1] == b'_'
                    || bytes[i - 1] == b'$'))
            && i + 6 < len
            && (bytes[i + 6].is_ascii_whitespace() || bytes[i + 6] == b'"' || bytes[i + 6] == b'\'')
        {
            Some(6usize)
        } else {
            None
        };
        if let Some(kw_len) = matched_kw {
            let mut j = i + kw_len;
            while j < len && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if j < len && (bytes[j] == b'"' || bytes[j] == b'\'') {
                let q = bytes[j];
                let start = j + 1;
                let mut end = start;
                while end < len && bytes[end] != q && bytes[end] != b'\n' {
                    if bytes[end] == b'\\' {
                        end += 2;
                    } else {
                        end += 1;
                    }
                }
                if end < len && bytes[end] == q {
                    let spec = &code[start..end];
                    if !spec.is_empty() {
                        out.push(spec.to_string());
                    }
                    i = end + 1;
                    continue;
                }
            }
        }
        i += 1;
    }
    out
}

enum ClassifiedTest {
    Reftest { is_match: bool, ref_path: PathBuf },
    Testharness,
    Unknown,
}

fn classify_wpt_file(wpt_root: &Path, path: &Path, html: &str) -> ClassifiedTest {
    let doc = parse_html(html);
    let mut has_testharness = false;
    let mut reftest_link: Option<(bool, String)> = None;

    for node in doc.descendants(doc.root()) {
        let Some(NodeData::Element(elem)) = doc.get(node) else {
            continue;
        };
        match elem.name.local.as_ref() {
            "link" => {
                if let (Some(rel), Some(href)) =
                    (elem.get_attribute("rel"), elem.get_attribute("href"))
                {
                    if rel.eq_ignore_ascii_case("match") {
                        reftest_link = Some((true, href.to_string()));
                    } else if rel.eq_ignore_ascii_case("mismatch") {
                        reftest_link = Some((false, href.to_string()));
                    }
                }
            }
            "script" => {
                if let Some(src) = elem.get_attribute("src") {
                    if src.contains("testharness.js") {
                        has_testharness = true;
                    }
                }
            }
            _ => {}
        }
    }

    if let Some((is_match, href)) = reftest_link {
        let ref_path = resolve_wpt_path(wpt_root, path, &href);
        if ref_path.exists() {
            return ClassifiedTest::Reftest { is_match, ref_path };
        }
    }
    if has_testharness {
        return ClassifiedTest::Testharness;
    }
    ClassifiedTest::Unknown
}

fn collect_local_stylesheets(
    wpt_root: &Path,
    doc_path: &Path,
    doc: &Document,
    media: &MediaContext,
) -> Vec<Arc<Stylesheet>> {
    let mut sheets = Vec::new();
    for node in doc.descendants(doc.root()) {
        let Some(NodeData::Element(elem)) = doc.get(node) else {
            continue;
        };
        if &*elem.name.local == "link" {
            let is_style = elem.get_attribute("rel").is_some_and(|r| {
                r.split_ascii_whitespace()
                    .any(|w| w.eq_ignore_ascii_case("stylesheet"))
            });
            if is_style {
                if let Some(href) = elem.get_attribute("href") {
                    let css_path = resolve_wpt_path(wpt_root, doc_path, href);
                    if let Ok(css_text) = fs::read_to_string(&css_path) {
                        sheets.push(Stylesheet::parse_cached(
                            &css_text,
                            CascadeOrigin::Author,
                            media,
                        ));
                    }
                }
            }
        }
    }
    sheets
}

fn preload_local_wpt_images(
    wpt_root: &Path,
    doc_path: &Path,
    doc: &Document,
    assets: &mut rix_layout::AssetRegistry,
) {
    #[derive(Clone)]
    enum CachedImg {
        Raster {
            width: u32,
            height: u32,
            rgba: Arc<Vec<u8>>,
        },
        Svg(rix_layout::DecodedSvg),
        Failed,
    }
    static IMG_CACHE: std::sync::OnceLock<std::sync::Mutex<HashMap<PathBuf, CachedImg>>> =
        std::sync::OnceLock::new();
    let cache = IMG_CACHE.get_or_init(|| std::sync::Mutex::new(HashMap::new()));

    for node in doc.descendants(doc.root()) {
        let Some(NodeData::Element(elem)) = doc.get(node) else {
            continue;
        };
        if !elem.name.local.as_str().eq_ignore_ascii_case("img") {
            continue;
        }
        let Some(src) = elem.get_attribute("src") else {
            continue;
        };
        let src_trimmed = src.trim();
        if src_trimmed.is_empty()
            || src_trimmed.starts_with("data:")
            || src_trimmed.starts_with("http://")
            || src_trimmed.starts_with("https://")
        {
            continue;
        }
        if assets.get_by_src(src_trimmed).is_some() || assets.get_svg_by_src(src_trimmed).is_some()
        {
            continue;
        }
        let img_path = resolve_wpt_path(wpt_root, doc_path, src_trimmed);
        let canonical = fs::canonicalize(&img_path).unwrap_or(img_path);
        let entry = {
            let guard = cache.lock().expect("img cache lock");
            guard.get(&canonical).cloned()
        };
        let decoded = if let Some(hit) = entry {
            hit
        } else {
            let loaded = if canonical
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("svg"))
            {
                fs::read_to_string(&canonical)
                    .ok()
                    .and_then(|svg_txt| {
                        let svg_doc = parse_html(&svg_txt);
                        rix_layout::extract_svg_from_document(&svg_doc, rix_css::ColorRgba::BLACK)
                    })
                    .map_or(CachedImg::Failed, CachedImg::Svg)
            } else {
                fs::read(&canonical)
                    .ok()
                    .and_then(|bytes| rix_layout::decode_single_raster(&bytes).ok())
                    .map_or(CachedImg::Failed, |d| CachedImg::Raster {
                        width: d.width,
                        height: d.height,
                        rgba: d.rgba,
                    })
            };
            if let Ok(mut guard) = cache.lock() {
                guard.insert(canonical, loaded.clone());
            }
            loaded
        };
        match decoded {
            CachedImg::Raster {
                width,
                height,
                rgba,
            } => {
                let _ = assets.register_rgba8_shared(src_trimmed.to_string(), width, height, rgba);
            }
            CachedImg::Svg(svg) => {
                assets.register_svg(src_trimmed.to_string(), svg);
            }
            CachedImg::Failed => {}
        }
    }
}

fn render_wpt_reftest_rgba(
    renderer: &mut VelloRenderer,
    wpt_root: &Path,
    doc_path: &Path,
    html: &str,
) -> Result<Vec<u8>> {
    let width = 800u32;
    let height = 600u32;
    let viewport = Size::new(width as f32, height as f32);
    let media = MediaContext {
        viewport_width: viewport.width,
        viewport_height: viewport.height,
        ..MediaContext::default()
    };
    let mut doc = parse_html(html);
    let mut styles = StyleContext::new(media);
    for sheet in collect_local_stylesheets(wpt_root, doc_path, &doc, &media) {
        styles.add_stylesheet(sheet);
    }
    for css in extract_style_blocks(&doc) {
        styles.add_stylesheet(Stylesheet::parse_cached(
            &css,
            CascadeOrigin::Author,
            &media,
        ));
    }
    styles.cascade_full(&mut doc);

    let mut engine = LayoutEngine::new();
    preload_local_wpt_images(wpt_root, doc_path, &doc, &mut engine.assets);
    let pipeline = PipelineId(1);
    let mut resources = ResourceRegistry::default();
    let font_db = wpt_font_db(wpt_root);
    let frame =
        engine.layout_and_paint(&doc, &styles, viewport, &font_db, pipeline, &mut resources);
    let mut scene = Scene::new();
    encode_display_list(
        pipeline,
        &frame.display_list,
        &mut resources,
        &EncodeOptions::default(),
        &mut scene,
    );
    renderer
        .render_to_rgba_blocking(&scene, width, height, Color::WHITE)
        .map_err(|err| anyhow::anyhow!("vello render failed: {err}"))
}

struct ScriptRunOutcome {
    passed_file: bool,
    subtests_passed: usize,
    subtests_total: usize,
    reason: Option<String>,
    failed_subtests: Vec<(String, String)>,
}

fn wpt_font_db(wpt_root: &Path) -> Arc<rix_text::FontDatabase> {
    static FONT_DB: std::sync::OnceLock<Arc<rix_text::FontDatabase>> = std::sync::OnceLock::new();
    Arc::clone(FONT_DB.get_or_init(|| {
        let mut db = rix_text::FontDatabase::new();
        db.scan_system_fonts();
        let ahem = wpt_root.join("fonts").join("Ahem.ttf");
        if ahem.exists() {
            let _ = db.load_font_file(&ahem);
        }
        Arc::new(db)
    }))
}

/// Lightweight local loopback HTTP/1.1 server exposing `wpt_root` over `127.0.0.1:<port>`
/// so WPT `fetch()`, `<iframe src>`, and `idl_test` exercise the real `rix-net` FetchEngine.
struct LocalWptServer {
    rt: tokio::runtime::Runtime,
    port: u16,
    fetch_engine: Arc<FetchEngine>,
}

fn wpt_mime_type(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("html" | "htm") => "text/html",
        Some("xhtml" | "xht") => "application/xhtml+xml",
        Some("xml") => "application/xml",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("json") => "application/json",
        Some("css") => "text/css; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("wasm") => "application/wasm",
        Some("idl" | "txt" | "vtt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

fn percent_decode_path(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3]) {
                if let Ok(v) = u8::from_str_radix(hex, 16) {
                    out.push(v);
                    i += 3;
                    continue;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn start_local_wpt_server(wpt_root: PathBuf) -> Result<LocalWptServer> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .context("build local WPT tokio runtime")?;
    let (listener, port) = rt.block_on(async {
        let l = TcpListener::bind("127.0.0.1:0").await?;
        let p = l.local_addr()?.port();
        Ok::<_, anyhow::Error>((l, p))
    })?;
    let root_arc = Arc::new(wpt_root);
    let stash: Arc<std::sync::Mutex<HashMap<String, Vec<serde_json::Value>>>> =
        Arc::new(std::sync::Mutex::new(HashMap::new()));
    rt.spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let root = Arc::clone(&root_arc);
            let stash_c = Arc::clone(&stash);
            tokio::spawn(async move {
                let mut raw_buf = Vec::with_capacity(8192);
                let mut tmp = [0u8; 8192];
                let head_end;
                loop {
                    match tokio::time::timeout(Duration::from_secs(2), stream.read(&mut tmp)).await
                    {
                        Ok(Ok(0)) | Err(_) | Ok(Err(_)) => return,
                        Ok(Ok(n)) => {
                            raw_buf.extend_from_slice(&tmp[..n]);
                            if let Some(pos) = raw_buf.windows(4).position(|w| w == b"\r\n\r\n") {
                                head_end = pos;
                                break;
                            }
                            if raw_buf.len() > 256 * 1024 {
                                return;
                            }
                        }
                    }
                }
                let head_bytes = &raw_buf[..head_end];
                let mut head_lines = head_bytes.split(|&b| b == b'\n');
                let first_raw = head_lines.next().unwrap_or(b"");
                let first_line = String::from_utf8_lossy(
                    first_raw.strip_suffix(b"\r").unwrap_or(first_raw),
                );
                let mut parts = first_line.split_whitespace();
                let method = parts.next().unwrap_or("GET").to_string();
                let raw_target = parts.next().unwrap_or("/").to_string();

                let mut req_headers_raw: Vec<(String, Vec<u8>)> = Vec::new();
                let mut content_len = 0usize;
                for raw_line in head_lines {
                    let line = raw_line.strip_suffix(b"\r").unwrap_or(raw_line);
                    if let Some(colon) = line.iter().position(|&b| b == b':') {
                        let k = String::from_utf8_lossy(&line[..colon])
                            .trim()
                            .to_ascii_lowercase();
                        let mut v = &line[colon + 1..];
                        while let Some((&f, r)) = v.split_first() {
                            if f == b' ' || f == b'\t' {
                                v = r;
                            } else {
                                break;
                            }
                        }
                        while let Some((&l, r)) = v.split_last() {
                            if l == b' ' || l == b'\t' {
                                v = r;
                            } else {
                                break;
                            }
                        }
                        if k == "content-length" {
                            content_len = String::from_utf8_lossy(v)
                                .trim()
                                .parse::<usize>()
                                .unwrap_or(0);
                        }
                        req_headers_raw.push((k, v.to_vec()));
                    }
                }
                // Drain remaining request body bytes if `Content-Length` is present.
                let body_start = head_end + 4;
                let want_total = body_start.saturating_add(content_len.min(4 * 1024 * 1024));
                while raw_buf.len() < want_total {
                    match tokio::time::timeout(Duration::from_secs(2), stream.read(&mut tmp)).await
                    {
                        Ok(Ok(0)) | Err(_) | Ok(Err(_)) => break,
                        Ok(Ok(n)) => raw_buf.extend_from_slice(&tmp[..n]),
                    }
                }
                let req_body = raw_buf
                    .get(body_start..want_total.min(raw_buf.len()))
                    .unwrap_or(b"");

                let get_req_hdr = |name: &str| -> Option<Vec<u8>> {
                    let lower = name.to_ascii_lowercase();
                    let mut combined: Option<Vec<u8>> = None;
                    for (k, v) in &req_headers_raw {
                        if k == &lower {
                            match &mut combined {
                                Some(existing) => {
                                    existing.extend_from_slice(b", ");
                                    existing.extend_from_slice(v);
                                }
                                None => combined = Some(v.clone()),
                            }
                        }
                    }
                    combined
                };

                let (path_only, query_only) = match raw_target.split_once('?') {
                    Some((p, q)) => (p, q.split('#').next().unwrap_or("")),
                    None => (raw_target.split('#').next().unwrap_or("/"), ""),
                };
                let decoded = percent_decode_path(path_only);
                let rel = decoded.trim_start_matches('/');
                if rel.split('/').any(|seg| seg == "..") {
                    let _ = stream
                        .write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        .await;
                    return;
                }

                let mut query_params: Vec<(String, String)> = Vec::new();
                for pair in query_only.split('&') {
                    if pair.is_empty() {
                        continue;
                    }
                    if let Some((k, v)) = pair.split_once('=') {
                        query_params.push((percent_decode_path(k), percent_decode_path(v)));
                    } else {
                        query_params.push((percent_decode_path(pair), String::new()));
                    }
                }
                let query_first = |key: &str| -> Option<String> {
                    query_params
                        .iter()
                        .find(|(k, _)| k == key)
                        .map(|(_, v)| v.clone())
                };
                let query_has = |key: &str| -> bool { query_params.iter().any(|(k, _)| k == key) };

                // RFC 6455 WebSocket upgrade handler for official WPT `websockets/` endpoints.
                let is_ws_upgrade = get_req_hdr("upgrade")
                    .is_some_and(|v| String::from_utf8_lossy(&v).eq_ignore_ascii_case("websocket"))
                    && get_req_hdr("sec-websocket-key").is_some();
                if is_ws_upgrade {
                    let sec_key = String::from_utf8_lossy(
                        &get_req_hdr("sec-websocket-key").unwrap_or_default(),
                    )
                    .trim()
                    .to_string();
                    let req_proto = get_req_hdr("sec-websocket-protocol")
                        .map(|v| String::from_utf8_lossy(&v).trim().to_string());
                    let req_origin = get_req_hdr("origin")
                        .map(|v| String::from_utf8_lossy(&v).trim().to_string())
                        .unwrap_or_default();
                    let req_referer = get_req_hdr("referer")
                        .map(|v| String::from_utf8_lossy(&v).trim().to_string());
                    let req_cookie = get_req_hdr("cookie")
                        .map(|v| String::from_utf8_lossy(&v).trim().to_string());
                    let req_auth = get_req_hdr("authorization")
                        .map(|v| String::from_utf8_lossy(&v).trim().to_string());
                    let leftover = raw_buf.get(body_start..).unwrap_or(b"").to_vec();
                    handle_wpt_websocket_connection(
                        stream,
                        leftover,
                        rel,
                        query_only,
                        &query_params,
                        &sec_key,
                        req_proto.as_deref(),
                        &req_origin,
                        req_referer.as_deref(),
                        req_cookie.as_deref(),
                        req_auth.as_deref(),
                    )
                    .await;
                    return;
                }

                // WPT dynamic handler: `websockets/security/check.py`
                if rel == "websockets/security/check.py" {
                    let body: &[u8] = if get_req_hdr("sec-websocket-key").is_some() {
                        b"FAIL"
                    } else {
                        b"PASS"
                    };
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(head.as_bytes()).await;
                    let _ = stream.write_all(body).await;
                    let _ = stream.shutdown().await;
                    return;
                }

                // WPT dynamic handler: `dom/nodes/encoding.py?label=<enc>`
                if rel == "dom/nodes/encoding.py" {
                    let label = query_first("label")
                        .unwrap_or_default()
                        .replace('&', "&amp;")
                        .replace('"', "&quot;")
                        .replace('<', "&lt;")
                        .replace('>', "&gt;");
                    let body = format!("<!doctype html><meta charset=\"{label}\">").into_bytes();
                    let resp_head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(resp_head.as_bytes()).await;
                    let _ = stream.write_all(&body).await;
                    let _ = stream.shutdown().await;
                    return;
                }

                // WPT dynamic handler: `fetch/api/resources/inspect-headers.py`
                if rel == "fetch/api/resources/inspect-headers.py" {
                    let mut out = Vec::<u8>::new();
                    out.extend_from_slice(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n");
                    let mut checked: Vec<String> = Vec::new();
                    if let Some(hdr_list) = query_first("headers") {
                        for h in hdr_list.split('|') {
                            checked.push(h.to_string());
                            if let Some(val_bytes) = get_req_hdr(h) {
                                out.extend_from_slice(format!("x-request-{h}: ").as_bytes());
                                out.extend_from_slice(&val_bytes);
                                out.extend_from_slice(b"\r\n");
                            }
                        }
                    }
                    if query_has("cors") {
                        if let Some(orig) = get_req_hdr("origin") {
                            out.extend_from_slice(b"Access-Control-Allow-Origin: ");
                            out.extend_from_slice(&orig);
                            out.extend_from_slice(b"\r\n");
                        } else {
                            out.extend_from_slice(b"Access-Control-Allow-Origin: *\r\n");
                        }
                        out.extend_from_slice(
                            b"Access-Control-Allow-Credentials: true\r\nAccess-Control-Allow-Methods: GET, POST, HEAD\r\n",
                        );
                        let exposed: Vec<String> =
                            checked.iter().map(|h| format!("x-request-{h}")).collect();
                        out.extend_from_slice(
                            format!("Access-Control-Expose-Headers: {}\r\n", exposed.join(", "))
                                .as_bytes(),
                        );
                        if let Some(allow) = query_first("allow_headers") {
                            out.extend_from_slice(
                                format!("Access-Control-Allow-Headers: {allow}\r\n").as_bytes(),
                            );
                        } else {
                            let names: Vec<&str> =
                                req_headers_raw.iter().map(|(k, _)| k.as_str()).collect();
                            out.extend_from_slice(
                                format!("Access-Control-Allow-Headers: {}\r\n", names.join(", "))
                                    .as_bytes(),
                            );
                        }
                    } else {
                        out.extend_from_slice(b"Access-Control-Allow-Origin: *\r\n");
                    }
                    out.extend_from_slice(b"Content-Length: 0\r\nConnection: close\r\n\r\n");
                    let _ = stream.write_all(&out).await;
                    let _ = stream.shutdown().await;
                    return;
                }

                // WPT dynamic handler: `fetch/api/resources/echo-header-bytes.py`
                if rel == "fetch/api/resources/echo-header-bytes.py" {
                    let mut lines = Vec::new();
                    if let Some(hdr_list) = query_first("headers") {
                        for h in hdr_list.split('|') {
                            let raw = get_req_hdr(h).unwrap_or_default();
                            let hex: String = raw.iter().map(|b| format!("{b:02x}")).collect();
                            lines.push(format!("{h}: {hex}"));
                        }
                    }
                    let body = lines.join("\n").into_bytes();
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(head.as_bytes()).await;
                    let _ = stream.write_all(&body).await;
                    let _ = stream.shutdown().await;
                    return;
                }

                // WPT dynamic handler: `fetch/api/resources/trickle.py`
                if rel == "fetch/api/resources/trickle.py" {
                    let delay_ms = query_first("ms")
                        .and_then(|s| s.parse::<f64>().ok())
                        .unwrap_or(500.0);
                    let count = query_first("count")
                        .and_then(|s| s.parse::<usize>().ok())
                        .unwrap_or(50)
                        .min(200);
                    let capped_ms = (delay_ms * 0.1).clamp(0.0, 400.0) as u64;
                    if capped_ms > 0 {
                        tokio::time::sleep(Duration::from_millis(capped_ms)).await;
                    }
                    let body = "TEST_TRICKLE\n".repeat(count).into_bytes();
                    let ct_line = if query_has("notype") {
                        ""
                    } else {
                        "Content-Type: text/plain\r\n"
                    };
                    let head = format!(
                        "HTTP/1.1 200 OK\r\n{ct_line}Access-Control-Allow-Origin: *\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(head.as_bytes()).await;
                    let _ = stream.write_all(&body).await;
                    let _ = stream.shutdown().await;
                    return;
                }

                // WPT dynamic handler: `fetch/api/resources/stashed-chunk.py`
                if rel == "fetch/api/resources/stashed-chunk.py" {
                    let mut body = Vec::with_capacity(4096 * 3);
                    body.resize(4096, b'A');
                    body.resize(8192, b'B');
                    body.resize(12288, b'C');
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nCache-Control: no-store\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(head.as_bytes()).await;
                    let _ = stream.write_all(&body).await;
                    let _ = stream.shutdown().await;
                    return;
                }

                // WPT dynamic handler: `fetch/api/request/resources/cache.py`
                if rel == "fetch/api/request/resources/cache.py" {
                    let token = query_first("token").unwrap_or_default();
                    if query_has("querystate") {
                        let states = stash_c
                            .lock()
                            .ok()
                            .and_then(|mut g| g.remove(&token))
                            .unwrap_or_default();
                        let json = serde_json::to_string(&states).unwrap_or_else(|_| "[]".into());
                        let head = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            json.len()
                        );
                        let _ = stream.write_all(head.as_bytes()).await;
                        let _ = stream.write_all(json.as_bytes()).await;
                        let _ = stream.shutdown().await;
                        return;
                    }
                    let content = query_first("content").unwrap_or_default();
                    let tag = query_first("tag").map(|t| format!("\"{t}\""));
                    let date = query_first("date");
                    let expires = query_first("expires");
                    let vary = query_first("vary");
                    let cc = query_first("cache_control");
                    let redirect = query_first("redirect");
                    let ignore = query_has("ignore");

                    let inm = get_req_hdr("if-none-match")
                        .map(|b| b.into_iter().map(char::from).collect::<String>());
                    let ims = get_req_hdr("if-modified-since")
                        .map(|b| b.into_iter().map(char::from).collect::<String>());
                    let pragma = get_req_hdr("pragma")
                        .map(|b| b.into_iter().map(char::from).collect::<String>());
                    let req_cc = get_req_hdr("cache-control")
                        .map(|b| b.into_iter().map(char::from).collect::<String>());

                    let mut obj = serde_json::Map::new();
                    if !ignore {
                        if let Some(ref v) = inm {
                            obj.insert("If-None-Match".into(), serde_json::Value::String(v.clone()));
                        }
                        if let Some(ref v) = ims {
                            obj.insert(
                                "If-Modified-Since".into(),
                                serde_json::Value::String(v.clone()),
                            );
                        }
                        if let Some(ref v) = pragma {
                            obj.insert("Pragma".into(), serde_json::Value::String(v.clone()));
                        }
                        if let Some(ref v) = req_cc {
                            obj.insert("Cache-Control".into(), serde_json::Value::String(v.clone()));
                        }
                    }
                    if let Ok(mut g) = stash_c.lock() {
                        g.entry(token).or_default().push(serde_json::Value::Object(obj));
                    }

                    let mut hdrs = String::new();
                    if let Some(ref t) = tag {
                        hdrs.push_str(&format!("ETag: {t}\r\n"));
                    } else if let Some(ref d) = date {
                        hdrs.push_str(&format!("Last-Modified: {d}\r\n"));
                    }
                    if let Some(ref e) = expires {
                        hdrs.push_str(&format!("Expires: {e}\r\n"));
                    }
                    if let Some(ref v) = vary {
                        hdrs.push_str(&format!("Vary: {v}\r\n"));
                    }
                    if let Some(ref c) = cc {
                        hdrs.push_str(&format!("Cache-Control: {c}\r\n"));
                    }
                    hdrs.push_str("Access-Control-Allow-Origin: *\r\n");

                    let (status_line, body_bytes) = if let Some(ref loc) = redirect {
                        hdrs.push_str(&format!("Location: {loc}\r\n"));
                        ("HTTP/1.1 302 Redirect", Vec::new())
                    } else if (inm.is_some() && inm == tag) || (ims.is_some() && ims == date) {
                        ("HTTP/1.1 304 Not Modified", Vec::new())
                    } else {
                        hdrs.push_str("Content-Type: text/plain\r\n");
                        ("HTTP/1.1 200 OK", content.into_bytes())
                    };
                    let head = format!(
                        "{status_line}\r\n{hdrs}Content-Length: {}\r\nConnection: close\r\n\r\n",
                        body_bytes.len()
                    );
                    let _ = stream.write_all(head.as_bytes()).await;
                    let _ = stream.write_all(&body_bytes).await;
                    let _ = stream.shutdown().await;
                    return;
                }

                // WPT dynamic handler: `fetch/api/resources/cache.py`
                if rel == "fetch/api/resources/cache.py" {
                    let etag = "\"123abc\"";
                    let inm = get_req_hdr("if-none-match")
                        .map(|b| String::from_utf8_lossy(&b).into_owned());
                    if inm.as_deref() == Some(etag) {
                        let head = "HTTP/1.1 304 Not Modified\r\nX-HTTP-STATUS: 304\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                        let _ = stream.write_all(head.as_bytes()).await;
                        let _ = stream.shutdown().await;
                        return;
                    }
                    let body = b"lorem ipsum dolor sit amet";
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nETag: {etag}\r\nContent-Type: text/plain\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(head.as_bytes()).await;
                    let _ = stream.write_all(body).await;
                    let _ = stream.shutdown().await;
                    return;
                }

                // WPT dynamic handler: `fetch/api/resources/echo-content.py`
                if rel == "fetch/api/resources/echo-content.py" {
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        req_body.len()
                    );
                    let _ = stream.write_all(head.as_bytes()).await;
                    let _ = stream.write_all(req_body).await;
                    let _ = stream.shutdown().await;
                    return;
                }

                // WPT dynamic handler: `fetch/api/resources/method.py`
                if rel == "fetch/api/resources/method.py" {
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nx-request-method: {method}\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Expose-Headers: x-request-method\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    );
                    let _ = stream.write_all(head.as_bytes()).await;
                    let _ = stream.shutdown().await;
                    return;
                }

                // WPT dynamic handlers for `workers/` redirects, credentials, referer, MIME, encodings.
                if let Some(stem) = rel
                    .strip_prefix("workers/baseurl/beta/")
                    .and_then(|s| s.strip_suffix(".py"))
                {
                    let target = format!("../gamma/{stem}.js");
                    let body = b"postMessage('executed redirecting script');";
                    let head = format!(
                        "HTTP/1.1 302 Moved\r\nLocation: {target}\r\nContent-Type: text/javascript\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(head.as_bytes()).await;
                    let _ = stream.write_all(body).await;
                    let _ = stream.shutdown().await;
                    return;
                }
                if rel == "workers/interfaces/WorkerGlobalScope/location/helper-redirect.py" {
                    let head = "HTTP/1.1 302 Found\r\nLocation: post-location-members.js?a\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                    let _ = stream.write_all(head.as_bytes()).await;
                    let _ = stream.shutdown().await;
                    return;
                }
                if rel == "workers/modules/resources/redirect.py" {
                    let loc = query_first("location").unwrap_or_default();
                    let head = format!(
                        "HTTP/1.1 302 Found\r\nLocation: {loc}\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    );
                    let _ = stream.write_all(head.as_bytes()).await;
                    let _ = stream.shutdown().await;
                    return;
                }
                if rel == "workers/modules/resources/export-on-load-script.py" {
                    let body = b"export const importedModules = ['export-on-load-script.js'];";
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/javascript\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Headers: Service-Worker\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(head.as_bytes()).await;
                    let _ = stream.write_all(body).await;
                    let _ = stream.shutdown().await;
                    return;
                }
                if rel == "workers/modules/resources/export-credentials.py"
                    || rel == "workers/modules/resources/postmessage-credentials.py"
                {
                    let cookie_hdr = get_req_hdr("cookie")
                        .map(|b| String::from_utf8_lossy(&b).into_owned())
                        .unwrap_or_default();
                    let cookie_val = cookie_hdr
                        .split(';')
                        .filter_map(|p| p.trim().split_once('='))
                        .find(|(k, _)| k.trim() == "COOKIE_NAME")
                        .map(|(_, v)| v.trim().to_string())
                        .unwrap_or_default();
                    let origin_hdr = get_req_hdr("origin")
                        .map(|b| String::from_utf8_lossy(&b).into_owned());
                    let acao = origin_hdr
                        .map(|o| format!("Access-Control-Allow-Origin: {o}\r\n"))
                        .unwrap_or_else(|| "Access-Control-Allow-Origin: *\r\n".to_string());
                    let body = if rel.ends_with("export-credentials.py") {
                        format!("export const cookie = '{cookie_val}';").into_bytes()
                    } else {
                        format!(
                            "if ('DedicatedWorkerGlobalScope' in self && self instanceof DedicatedWorkerGlobalScope) {{ postMessage('{cookie_val}'); }} else if ('SharedWorkerGlobalScope' in self && self instanceof SharedWorkerGlobalScope) {{ onconnect = e => e.ports[0].postMessage('{cookie_val}'); }}"
                        )
                        .into_bytes()
                    };
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/javascript\r\n{acao}Access-Control-Allow-Credentials: true\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(head.as_bytes()).await;
                    let _ = stream.write_all(&body).await;
                    let _ = stream.shutdown().await;
                    return;
                }
                if rel == "workers/modules/resources/export-referrer-checker.py"
                    || rel == "workers/modules/resources/postmessage-referrer-checker.py"
                {
                    let referrer = get_req_hdr("referer")
                        .map(|b| String::from_utf8_lossy(&b).into_owned())
                        .unwrap_or_default();
                    let body = if rel.ends_with("export-referrer-checker.py") {
                        format!("export const referrer = '{referrer}';").into_bytes()
                    } else {
                        format!(
                            "if ('DedicatedWorkerGlobalScope' in self && self instanceof DedicatedWorkerGlobalScope) {{ postMessage('{referrer}'); }} else if ('SharedWorkerGlobalScope' in self && self instanceof SharedWorkerGlobalScope) {{ onconnect = e => e.ports[0].postMessage('{referrer}'); }}"
                        )
                        .into_bytes()
                    };
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/javascript\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(head.as_bytes()).await;
                    let _ = stream.write_all(&body).await;
                    let _ = stream.shutdown().await;
                    return;
                }
                if rel == "workers/semantics/encodings/003-1.py" {
                    let body: &[u8] = if query_first("x").as_deref() == Some("å") {
                        b"PASS"
                    } else {
                        b"FAIL"
                    };
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(head.as_bytes()).await;
                    let _ = stream.write_all(body).await;
                    let _ = stream.shutdown().await;
                    return;
                }
                if rel == "workers/support/imported_script.py" {
                    let mime = query_first("mime").unwrap_or_else(|| "text/javascript".into());
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    );
                    let _ = stream.write_all(head.as_bytes()).await;
                    let _ = stream.shutdown().await;
                    return;
                }
                if rel == "workers/support/nosiniff-error-worker.py" {
                    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nX-Content-Type-Options: nosniff\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                    let _ = stream.write_all(head.as_bytes()).await;
                    let _ = stream.shutdown().await;
                    return;
                }

                // WPT dynamic handler: `fetch/api/resources/status.py`
                if rel == "fetch/api/resources/status.py" {
                    let code = query_first("code")
                        .and_then(|s| s.parse::<u16>().ok())
                        .unwrap_or(200);
                    let text = query_first("text").unwrap_or_else(|| "OMG".into());
                    let ct = query_first("type").unwrap_or_else(|| "text/plain".into());
                    let content = query_first("content").unwrap_or_default().into_bytes();
                    let head = format!(
                        "HTTP/1.1 {code} {text}\r\nContent-Type: {ct}\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        content.len()
                    );
                    let _ = stream.write_all(head.as_bytes()).await;
                    let _ = stream.write_all(&content).await;
                    let _ = stream.shutdown().await;
                    return;
                }

                let mut file_path = if rel == "resources/WebIDLParser.js" {
                    root.join("resources/webidl2/lib/webidl2.js")
                } else {
                    root.join(rel)
                };
                if file_path.is_dir() {
                    file_path = file_path.join("index.html");
                }
                let Ok(mut body) = tokio::fs::read(&file_path).await else {
                    // Upstream WPT serves synthesized HTML wrappers for `*.any.html` / `*.window.html`
                    // from their corresponding `*.any.js` / `*.window.js` source files.
                    let synth_js = rel
                        .strip_suffix(".any.html")
                        .map(|p| root.join(format!("{p}.any.js")))
                        .or_else(|| {
                            rel.strip_suffix(".window.html")
                                .map(|p| root.join(format!("{p}.window.js")))
                        });
                    if let Some(js_path) = synth_js {
                        if let Ok(js_src) = tokio::fs::read_to_string(&js_path).await {
                            if let Some(html) = synthesize_wpt_js_html(&js_path, &js_src) {
                                let head = format!(
                                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                    html.len()
                                );
                                let _ = stream.write_all(head.as_bytes()).await;
                                let _ = stream.write_all(html.as_bytes()).await;
                                let _ = stream.shutdown().await;
                                return;
                            }
                        }
                    }
                    let _ = stream
                        .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        .await;
                    return;
                };
                let fname = file_path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("");
                if fname.contains(".sub.") {
                    if let Ok(txt) = String::from_utf8(body.clone()) {
                        let mut sub = substitute_wpt_templates(&txt, port);
                        // Substitute WPT `{{GET[key]}}` template placeholders from query string.
                        let mut get_params: HashMap<String, String> = HashMap::new();
                        for pair in query_only.split('&') {
                            if let Some((k, v)) = pair.split_once('=') {
                                get_params.insert(percent_decode_path(k), percent_decode_path(v));
                            }
                        }
                        while let Some(pos) = sub.find("{{GET[") {
                            let rest = &sub[pos + 6..];
                            let Some(end) = rest.find("]}}") else {
                                break;
                            };
                            let key = &rest[..end];
                            let repl = get_params.get(key).map(String::as_str).unwrap_or("");
                            sub.replace_range(pos..pos + 6 + end + 3, repl);
                        }
                        body = sub.into_bytes();
                    }
                }
                // Honor WPT `pipe=trickle(d<seconds>)` delay parameter in query string.
                for pair in query_only.split('&') {
                    if let Some(pipe_val) = pair.strip_prefix("pipe=") {
                        let dec = percent_decode_path(pipe_val);
                        if let Some(pos) = dec.find("trickle(d") {
                            let rest = &dec[pos + 9..];
                            if let Some(end) = rest.find(')') {
                                if let Ok(secs) = rest[..end].trim().parse::<f64>() {
                                    let ms = (secs.clamp(0.0, 0.06) * 1000.0) as u64;
                                    if ms > 0 {
                                        tokio::time::sleep(Duration::from_millis(ms)).await;
                                    }
                                }
                            }
                        }
                    }
                }
                let mut content_type = wpt_mime_type(&file_path).to_string();
                let mut extra_headers = Vec::new();
                let headers_sidecar = PathBuf::from(format!("{}.headers", file_path.display()));
                if let Ok(hdr_txt) = tokio::fs::read_to_string(&headers_sidecar).await {
                    for line in hdr_txt.lines() {
                        let line = line.trim();
                        if let Some((k, v)) = line.split_once(':') {
                            if k.trim().eq_ignore_ascii_case("content-type") {
                                content_type = v.trim().to_string();
                            } else {
                                extra_headers.push(format!("{}: {}", k.trim(), v.trim()));
                            }
                        }
                    }
                }
                let mut resp_head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: {}\r\nConnection: close\r\n",
                    body.len()
                );
                for h in extra_headers {
                    resp_head.push_str(&h);
                    resp_head.push_str("\r\n");
                }
                resp_head.push_str("\r\n");
                let _ = stream.write_all(resp_head.as_bytes()).await;
                let _ = stream.write_all(&body).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    let client = HttpClient::new(None).context("init WPT loopback HttpClient")?;
    let fetch_engine = Arc::new(FetchEngine::new(client));
    Ok(LocalWptServer {
        rt,
        port,
        fetch_engine,
    })
}

fn substitute_wpt_templates(input: &str, port: u16) -> String {
    let port_s = port.to_string();
    let host_port = format!("localhost:{port_s}");
    let out = input
        .replace("{{location[scheme]}}", "http")
        .replace("{{location[port]}}", &port_s)
        .replace("{{location[host]}}", &host_port)
        .replace("{{host}}", "localhost")
        .replace("{{domains[]}}", "localhost")
        .replace("{{domains[www]}}", "127.0.0.1")
        .replace("{{domains[www1]}}", "127.0.0.1")
        .replace("{{domains[www2]}}", "127.0.0.1")
        .replace("{{domains[天気の良い日]}}", "127.0.0.1")
        .replace("{{hosts[][]}}", "localhost")
        .replace("{{hosts[][www]}}", "127.0.0.1")
        .replace("{{hosts[alt][www]}}", "127.0.0.1")
        .replace("{{hosts[alt][]}}", "127.0.0.1")
        .replace("{{ports[http][0]}}", &port_s)
        .replace("{{ports[http][1]}}", &(port.saturating_add(1)).to_string())
        .replace("{{ports[https][0]}}", &port_s)
        .replace("{{ports[ws][0]}}", &port_s)
        .replace("{{ports[wss][0]}}", &port_s);
    // LocalWptServer serves loopback HTTP on `port`, so map local `https://127.0.0.1:<port>`
    // and `https://localhost:<port>` test URLs to loopback HTTP while preserving distinct origins.
    out.replace(
        &format!("https://127.0.0.1:{port_s}"),
        &format!("http://127.0.0.1:{port_s}"),
    )
    .replace(
        &format!("https://localhost:{port_s}"),
        &format!("http://localhost:{port_s}"),
    )
}

fn run_wpt_testharness_file(
    wpt_root: &Path,
    path: &Path,
    html: &str,
    server: &LocalWptServer,
) -> ScriptRunOutcome {
    let _enter = server.rt.enter();
    let font_db = wpt_font_db(wpt_root);

    let sub_html;
    let html = if html.contains("{{") {
        sub_html = substitute_wpt_templates(html, server.port);
        sub_html.as_str()
    } else {
        html
    };

    let parsed = parse_html(html);
    let viewport = Size::new(800.0, 600.0);
    let media = MediaContext {
        viewport_width: viewport.width,
        viewport_height: viewport.height,
        ..MediaContext::default()
    };
    let ext_sheets = collect_local_stylesheets(wpt_root, path, &parsed, &media);

    let mut ext_scripts: HashMap<String, String> = HashMap::new();
    let mut module_scan_queue: Vec<(PathBuf, String)> = Vec::new();
    for node in parsed.descendants(parsed.root()) {
        let Some(NodeData::Element(elem)) = parsed.get(node) else {
            continue;
        };
        if &*elem.name.local == "script" {
            let is_module = elem
                .get_attribute("type")
                .is_some_and(|t| t.trim().eq_ignore_ascii_case("module"));
            if let Some(src) = elem.get_attribute("src") {
                if src.contains("testharnessreport.js") {
                    ext_scripts.insert(src.to_string(), String::new());
                } else if src.contains("testharness.js") {
                    ext_scripts.insert(src.to_string(), WPT_TESTHARNESS_SHIM.to_string());
                } else {
                    let js_path = resolve_wpt_path(wpt_root, path, src);
                    if let Ok(mut code) = fs::read_to_string(&js_path) {
                        if js_path
                            .file_name()
                            .and_then(|s| s.to_str())
                            .is_some_and(|n| n.contains(".sub."))
                        {
                            code = substitute_wpt_templates(&code, server.port);
                        }
                        if is_module {
                            module_scan_queue.push((js_path, code.clone()));
                        }
                        ext_scripts.insert(src.to_string(), code);
                    }
                }
            } else if is_module {
                let inline_code = parsed.inner_text(node);
                if !inline_code.trim().is_empty() {
                    module_scan_queue.push((path.to_path_buf(), inline_code));
                }
            }
        }
    }
    let origin_base = format!("http://localhost:{}", server.port);
    // Recursively preload static ES module imports (`import ... from "..."`, `export ... from "..."`)
    // from the local WPT tree so `<script type="module">` tests work offline cleanly.
    let mut visited_modules: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    while let Some((cur_file, code)) = module_scan_queue.pop() {
        for spec in extract_es_module_specifiers(&code) {
            if spec.starts_with("http://") || spec.starts_with("https://") {
                continue;
            }
            let dep_path = resolve_wpt_path(wpt_root, &cur_file, &spec);
            let canonical = fs::canonicalize(&dep_path).unwrap_or_else(|_| dep_path.clone());
            if !visited_modules.insert(canonical.clone()) {
                continue;
            }
            if let Ok(dep_code) = fs::read_to_string(&dep_path) {
                if let Ok(rel_dep) = canonical.strip_prefix(wpt_root) {
                    let abs_url = format!(
                        "{origin_base}/{}",
                        rel_dep.to_string_lossy().replace('\\', "/")
                    );
                    ext_scripts.insert(abs_url, dep_code.clone());
                } else if let Ok(rel_dep) = dep_path.strip_prefix(wpt_root) {
                    let abs_url = format!(
                        "{origin_base}/{}",
                        rel_dep.to_string_lossy().replace('\\', "/")
                    );
                    ext_scripts.insert(abs_url, dep_code.clone());
                }
                ext_scripts.insert(spec, dep_code.clone());
                module_scan_queue.push((dep_path, dep_code));
            }
        }
    }

    let rel = path.strip_prefix(wpt_root).unwrap_or(path);
    let doc_url = format!("{origin_base}/{}", rel.to_string_lossy().replace('\\', "/"));
    let doc = Rc::new(RefCell::new(Document::new()));
    let per_file_engine = Arc::new(FetchEngine::new(server.fetch_engine.client().clone()));
    let mut rt =
        ContentRuntime::with_fetch_engine(doc, viewport, font_db, &doc_url, per_file_engine);
    rt.vm.max_loop_iterations = Some(2_500_000);
    rt.event_loop.borrow_mut().enable_virtual_clock();
    // Apply WPT `.headers` sidecar CSP headers (e.g. `Content-Security-Policy` / `-Report-Only`).
    let headers_sidecar = PathBuf::from(format!("{}.headers", path.display()));
    if let Ok(hdr_txt) = fs::read_to_string(&headers_sidecar) {
        for line in hdr_txt.lines() {
            if let Some((k, v)) = line.trim().split_once(':') {
                rt.apply_response_csp_header(k.trim(), v.trim());
            }
        }
    }
    rt.host.borrow_mut().set_external_stylesheets(ext_sheets);
    preload_local_wpt_images(
        wpt_root,
        path,
        &parsed,
        &mut rt.host.borrow_mut().layout_engine.assets,
    );

    let script_errors = rt.load_document_with_external_scripts(html, &ext_scripts);
    let has_pending = |rt: &mut ContentRuntime| -> bool {
        rt.eval_script("typeof __rix_wpt_pending === 'function' ? __rix_wpt_pending() : 0")
            .ok()
            .and_then(|v| v.as_number())
            .is_some_and(|n| n > 0.0)
    };
    // Drive async network/worker events, timers, microtasks, and VSync/layout observers
    // until all async/promise subtests settle (exiting immediately when pending == 0).
    let mut async_waits = 0u32;
    let mut virtual_timer_jumps = 0u32;
    let mut media_playback_ticks = 0u32;
    for tick in 0..1000u32 {
        rt.advance_and_drain(Duration::from_millis(if tick == 0 { 0 } else { 10 }));
        if !has_pending(&mut rt) {
            break;
        }
        let had_raf = rt.event_loop.borrow().has_pending_raf();
        let now_ms = rt.event_loop.borrow().now_ms();
        rt.fire_vsync(now_ms);
        if !has_pending(&mut rt) {
            break;
        }
        // When rAF animations or active HTML5 media playback are running, keep advancing VSync
        // smoothly without burning wall-clock async waits.
        if had_raf || rt.event_loop.borrow().has_pending_raf() {
            continue;
        }
        if rt.media_elements.borrow().has_active_playback()
            && media_playback_ticks < 200
            && !rt.net.borrow().has_pending_io()
            && !rt.workers.borrow().has_active_workers()
        {
            media_playback_ticks += 1;
            rt.event_loop
                .borrow_mut()
                .advance_clock(Duration::from_millis(50));
            continue;
        }
        // When JS timers (`setTimeout` / `delay(500)`) are pending, fast-forward virtual time
        // directly to the next timer deadline — UNLESS OS worker threads are actively running
        // and the timer is a long timeout guard (>= 100 ms), in which case yield to async I/O first
        // so worker threads finish before timeout guards fire!
        let has_workers = rt.workers.borrow().has_active_workers();
        let next_timer = rt.event_loop.borrow_mut().time_until_next_timer();
        if let Some(dt) = next_timer {
            let allow_jump = !has_workers || dt < Duration::from_millis(100) || async_waits >= 20;
            if allow_jump && dt <= Duration::from_secs(5) && virtual_timer_jumps < 400 {
                virtual_timer_jumps += 1;
                if !dt.is_zero() {
                    rt.event_loop.borrow_mut().advance_clock(dt);
                }
                continue;
            }
        }
        if async_waits >= 160 {
            break;
        }
        async_waits += 1;
        let wait_started = std::time::Instant::now();
        let settled =
            server
                .rt
                .handle()
                .block_on(rt.drive_async_until(Duration::from_millis(40), |rt| {
                    !rt.eval_script(
                        "typeof __rix_wpt_pending === 'function' ? __rix_wpt_pending() : 0",
                    )
                    .ok()
                    .and_then(|v| v.as_number())
                    .is_some_and(|n| n > 0.0)
                }));
        rt.event_loop
            .borrow_mut()
            .advance_clock(wait_started.elapsed());
        if settled {
            break;
        }
    }

    let summary_json = match rt
        .eval_script("typeof __rix_wpt_summary === 'function' ? __rix_wpt_summary() : ''")
    {
        Ok(val) => rt.vm.value_to_rust_string(val),
        Err(e) => {
            return ScriptRunOutcome {
                passed_file: false,
                subtests_passed: 0,
                subtests_total: 0,
                reason: Some(format!("summary eval failed: {e}")),
                failed_subtests: Vec::new(),
            };
        }
    };

    let Ok(summary) = serde_json::from_str::<JsWptSummary>(&summary_json) else {
        let first_err = script_errors
            .first()
            .cloned()
            .unwrap_or_else(|| "testharness summary missing".to_string());
        return ScriptRunOutcome {
            passed_file: false,
            subtests_passed: 0,
            subtests_total: 0,
            reason: Some(first_err),
            failed_subtests: Vec::new(),
        };
    };

    let passed_file = summary.total > 0
        && summary.failed == 0
        && summary.pending == 0
        && summary.status == 0
        && script_errors.is_empty();

    let reason = if passed_file {
        None
    } else if !script_errors.is_empty() {
        Some(script_errors[0].clone())
    } else if !summary.first_error.is_empty() {
        Some(summary.first_error)
    } else if summary.pending > 0 {
        Some(format!(
            "{} async subtest(s) still pending",
            summary.pending
        ))
    } else if summary.total == 0 {
        Some("0 subtests executed".to_string())
    } else {
        Some(format!(
            "{}/{} subtests failed",
            summary.failed, summary.total
        ))
    };

    ScriptRunOutcome {
        passed_file,
        subtests_passed: summary.passed,
        subtests_total: summary.total,
        reason,
        failed_subtests: summary.failed_subtests,
    }
}

/// Runs an official upstream WPT directory (both `testharness.js` and visual reftests).
pub fn run_official_wpt_suite(opts: &OfficialWptOptions) -> Result<OfficialWptReport> {
    let suite_dir = opts.wpt_dir.join(&opts.suite);
    if !suite_dir.exists() {
        bail!(
            "official WPT suite directory not found: {}",
            suite_dir.display()
        );
    }

    let mut files = Vec::new();
    collect_wpt_html_files(&suite_dir, &mut files)?;
    files.sort();

    let mut testharness_jobs: Vec<(PathBuf, String)> = Vec::new();
    let mut reftest_jobs: Vec<(PathBuf, String, bool, PathBuf)> = Vec::new();

    for path in files {
        let Ok(raw) = fs::read_to_string(&path) else {
            continue;
        };
        let is_js_test = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.ends_with(".any.js") || n.ends_with(".window.js"));
        if is_js_test {
            if matches!(
                opts.kind,
                OfficialWptKind::All | OfficialWptKind::Testharness
            ) {
                if let Some(synth_html) = synthesize_wpt_js_html(&path, &raw) {
                    testharness_jobs.push((path, synth_html));
                }
            }
        } else {
            match classify_wpt_file(&opts.wpt_dir, &path, &raw) {
                ClassifiedTest::Reftest { is_match, ref_path }
                    if matches!(opts.kind, OfficialWptKind::All | OfficialWptKind::Reftest) =>
                {
                    reftest_jobs.push((path, raw, is_match, ref_path));
                }
                ClassifiedTest::Testharness
                    if matches!(
                        opts.kind,
                        OfficialWptKind::All | OfficialWptKind::Testharness
                    ) =>
                {
                    testharness_jobs.push((path, raw));
                }
                _ => {}
            }
        }
        if let Some(lim) = opts.limit {
            if testharness_jobs.len() + reftest_jobs.len() >= lim {
                break;
            }
        }
    }

    let mut report = OfficialWptReport {
        suite: opts.suite.clone(),
        ..OfficialWptReport::default()
    };
    let mut reasons: HashMap<String, usize> = HashMap::new();

    // Execute script tests in parallel with 8 MiB stacks, local loopback WPT server, and panic isolation.
    let wpt_root = opts.wpt_dir.clone();
    let local_server = start_local_wpt_server(wpt_root.clone())?;
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(4)
        .stack_size(8 * 1024 * 1024)
        .build()
        .context("build wpt worker pool")?;
    let script_results: Vec<(PathBuf, std::result::Result<ScriptRunOutcome, String>)> = pool
        .install(|| {
            testharness_jobs
                .par_iter()
                .map(|(path, html)| {
                    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        run_wpt_testharness_file(&wpt_root, path, html, &local_server)
                    }))
                    .map_err(|payload| {
                        let msg = payload
                            .downcast_ref::<&str>()
                            .map(|s| (*s).to_string())
                            .or_else(|| payload.downcast_ref::<String>().cloned())
                            .unwrap_or_else(|| "unknown panic".to_string());
                        format!("PANIC: {msg}")
                    });
                    (path.clone(), res)
                })
                .collect()
        });

    struct FailedWptForFirefox {
        rel: String,
        rix_total: usize,
        rix_failed: Vec<(String, String)>,
        rix_reason: String,
    }
    let mut failed_for_firefox: Vec<FailedWptForFirefox> = Vec::new();
    for (path, outcome) in script_results {
        let rel = path
            .strip_prefix(&opts.wpt_dir)
            .unwrap_or(&path)
            .display()
            .to_string();
        report.files_total += 1;
        report.testharness_files_total += 1;
        match outcome {
            Ok(out) => {
                report.subtests_total += out.subtests_total;
                report.subtests_passed += out.subtests_passed;
                if out.passed_file {
                    report.files_passed += 1;
                    report.testharness_files_passed += 1;
                } else {
                    report.files_failed += 1;
                    let r = out.reason.unwrap_or_else(|| "subtest failure".into());
                    let bucket: String = r.chars().take(72).collect();
                    *reasons.entry(bucket).or_insert(0) += 1;
                    if opts.compare_firefox {
                        failed_for_firefox.push(FailedWptForFirefox {
                            rel: rel.clone(),
                            rix_total: out.subtests_total,
                            rix_failed: out.failed_subtests,
                            rix_reason: r.clone(),
                        });
                    }
                    if report.sample_failures.len() < 300 {
                        report.sample_failures.push(OfficialWptFailure {
                            file: format!("{rel} [{}/{}]", out.subtests_passed, out.subtests_total),
                            kind: "testharness".into(),
                            reason: r,
                        });
                    }
                }
            }
            Err(panic_msg) => {
                report.files_failed += 1;
                report.files_panicked += 1;
                let bucket: String = panic_msg.chars().take(72).collect();
                *reasons.entry(bucket).or_insert(0) += 1;
                if opts.compare_firefox {
                    failed_for_firefox.push(FailedWptForFirefox {
                        rel: rel.clone(),
                        rix_total: 0,
                        rix_failed: Vec::new(),
                        rix_reason: panic_msg.clone(),
                    });
                }
                if report.sample_failures.len() < 40 {
                    report.sample_failures.push(OfficialWptFailure {
                        file: rel,
                        kind: "testharness".into(),
                        reason: panic_msg,
                    });
                }
            }
        }
    }

    // Optional Firefox-reference WPT differential pass: execute every Rix-failing testharness
    // file in a single reusable headless Firefox BiDi session to identify the exact
    // Firefox-superset gap (`firefox_pass_rix_fail == 0` target).
    if opts.compare_firefox && !failed_for_firefox.is_empty() {
        let mut ff = crate::firefox_bidi::PersistentFirefoxBidi::launch(&opts.firefox_bin)
            .with_context(|| {
                format!(
                    "launch Firefox WPT differential reference at {}",
                    opts.firefox_bin.display()
                )
            })?;
        for item in failed_for_firefox {
            let FailedWptForFirefox {
                rel,
                rix_total,
                rix_failed,
                rix_reason,
            } = item;
            let web_rel = if let Some(stem) = rel.strip_suffix(".any.js") {
                format!("{stem}.any.html")
            } else if let Some(stem) = rel.strip_suffix(".window.js") {
                format!("{stem}.window.html")
            } else {
                rel.clone()
            };
            let url = format!("http://127.0.0.1:{}/{}", local_server.port, web_rel);
            let Ok(ff_res) = ff.run_wpt_testharness(&url, Duration::from_secs(12)) else {
                continue;
            };
            report.firefox_checked_files += 1;
            let rix_failed_map: HashMap<&str, &str> = rix_failed
                .iter()
                .map(|(n, m)| (n.as_str(), m.as_str()))
                .collect();
            let mut file_has_gap = false;
            for st in &ff_res.subtests {
                if !st.pass {
                    continue;
                }
                let rix_missed = rix_total == 0 || rix_failed_map.contains_key(st.name.as_str());
                if rix_missed {
                    file_has_gap = true;
                    report.firefox_pass_rix_fail_subtests += 1;
                    if report.firefox_only_gaps.len() < 300 {
                        let err = rix_failed_map
                            .get(st.name.as_str())
                            .copied()
                            .unwrap_or(&rix_reason)
                            .to_string();
                        report.firefox_only_gaps.push(FirefoxWptGap {
                            file: rel.clone(),
                            subtest: st.name.clone(),
                            rix_error: err,
                        });
                    }
                }
            }
            if file_has_gap {
                report.firefox_pass_rix_fail_files += 1;
            }
        }
    }

    // Execute GPU visual reftests sequentially to reuse one GPU device cleanly.
    let mut renderer = VelloRenderer::new();
    for (path, html, is_match, ref_path) in reftest_jobs {
        let rel = path
            .strip_prefix(&opts.wpt_dir)
            .unwrap_or(&path)
            .display()
            .to_string();
        report.files_total += 1;
        report.reftests_total += 1;
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<bool> {
            let ref_html = fs::read_to_string(&ref_path)
                .with_context(|| format!("read ref {}", ref_path.display()))?;
            let actual = render_wpt_reftest_rgba(&mut renderer, &opts.wpt_dir, &path, &html)?;
            let expected =
                render_wpt_reftest_rgba(&mut renderer, &opts.wpt_dir, &ref_path, &ref_html)?;
            let diff = compare_rgba(&actual, &expected, 4);
            Ok(if is_match {
                diff.diff_pixels == 0
            } else {
                diff.diff_pixels > 0
            })
        }));

        match res {
            Ok(Ok(true)) => {
                report.files_passed += 1;
                report.reftests_passed += 1;
            }
            Ok(Ok(false)) => {
                report.files_failed += 1;
                let reason = if is_match {
                    "reftest pixel mismatch".to_string()
                } else {
                    "mismatch reftest rendered identically".to_string()
                };
                *reasons.entry(reason.clone()).or_insert(0) += 1;
                if report.sample_failures.len() < 40 {
                    report.sample_failures.push(OfficialWptFailure {
                        file: rel,
                        kind: "reftest".into(),
                        reason,
                    });
                }
            }
            Ok(Err(err)) => {
                report.files_failed += 1;
                let reason = format!("render error: {err}");
                *reasons
                    .entry(reason.chars().take(72).collect())
                    .or_insert(0) += 1;
                if report.sample_failures.len() < 40 {
                    report.sample_failures.push(OfficialWptFailure {
                        file: rel,
                        kind: "reftest".into(),
                        reason,
                    });
                }
            }
            Err(_) => {
                report.files_failed += 1;
                report.files_panicked += 1;
                *reasons.entry("PANIC in reftest".into()).or_insert(0) += 1;
                if report.sample_failures.len() < 40 {
                    report.sample_failures.push(OfficialWptFailure {
                        file: rel,
                        kind: "reftest".into(),
                        reason: "PANIC in reftest".into(),
                    });
                }
            }
        }
    }

    report.file_pass_rate_pct = if report.files_total == 0 {
        100.0
    } else {
        (report.files_passed as f64 / report.files_total as f64) * 100.0
    };
    report.subtest_pass_rate_pct = if report.subtests_total == 0 {
        report.file_pass_rate_pct
    } else {
        (report.subtests_passed as f64 / report.subtests_total as f64) * 100.0
    };

    let mut ranked: Vec<(String, usize)> = reasons.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ranked.truncate(15);
    report.top_failure_reasons = ranked;

    if let Some(ref json_path) = opts.report_json {
        if let Some(parent) = json_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(json_path, serde_json::to_string_pretty(&report)? + "\n")?;
    }

    Ok(report)
}

pub fn print_report(report: &OfficialWptReport) {
    println!("=== Official W3C WPT Conformance ({}) ===", report.suite);
    println!(
        "  Files: {} / {} ({:.1}%) | Reftests: {}/{} | Testharness files: {}/{} | Subtests: {} / {} ({:.1}%) | Panics: {}",
        report.files_passed,
        report.files_total,
        report.file_pass_rate_pct,
        report.reftests_passed,
        report.reftests_total,
        report.testharness_files_passed,
        report.testharness_files_total,
        report.subtests_passed,
        report.subtests_total,
        report.subtest_pass_rate_pct,
        report.files_panicked,
    );
    if report.firefox_checked_files > 0 {
        println!(
            "  Firefox-Superset Diff: checked {} Rix-failing files | FF-PASS/Rix-FAIL: {} files, {} subtests",
            report.firefox_checked_files,
            report.firefox_pass_rix_fail_files,
            report.firefox_pass_rix_fail_subtests,
        );
        for gap in report.firefox_only_gaps.iter().take(10) {
            println!(
                "    [FF-ONLY] {} :: {} => {}",
                gap.file, gap.subtest, gap.rix_error
            );
        }
    }
    if !report.top_failure_reasons.is_empty() {
        println!("  Top failure reasons:");
        for (reason, cnt) in report.top_failure_reasons.iter().take(10) {
            println!("    [{:>4}] {}", cnt, reason);
        }
    }
    let _ = BTreeMap::<String, usize>::new();
}

/// Incoming RFC 6455 message decoded on the local WPT WebSocket test server.
enum WptWsClientMsg {
    Text(String),
    Binary(Vec<u8>),
    /// Raw Close payload bytes (empty if client sent no status code).
    Close(Vec<u8>),
    Eof,
}

async fn read_wpt_ws_client_msg(
    stream: &mut tokio::net::TcpStream,
    buf: &mut Vec<u8>,
) -> WptWsClientMsg {
    let mut frag_opcode: Option<u8> = None;
    let mut frag_payload: Vec<u8> = Vec::new();
    let mut tmp = [0u8; 4096];

    loop {
        while buf.len() >= 2 {
            let b0 = buf[0];
            let b1 = buf[1];
            let fin = (b0 & 0x80) != 0;
            let opcode = b0 & 0x0F;
            let masked = (b1 & 0x80) != 0;
            let len7 = (b1 & 0x7F) as usize;
            let mut offset = 2usize;
            let payload_len = if len7 <= 125 {
                len7
            } else if len7 == 126 {
                if buf.len() < 4 {
                    break;
                }
                offset = 4;
                u16::from_be_bytes([buf[2], buf[3]]) as usize
            } else {
                if buf.len() < 10 {
                    break;
                }
                offset = 10;
                u64::from_be_bytes([
                    buf[2], buf[3], buf[4], buf[5], buf[6], buf[7], buf[8], buf[9],
                ]) as usize
            };
            let mask_len = if masked { 4 } else { 0 };
            let total = offset + mask_len + payload_len;
            if buf.len() < total {
                break;
            }
            let mask_key = if masked {
                [
                    buf[offset],
                    buf[offset + 1],
                    buf[offset + 2],
                    buf[offset + 3],
                ]
            } else {
                [0u8; 4]
            };
            let mut payload = buf[offset + mask_len..total].to_vec();
            buf.drain(..total);
            if masked {
                for (i, byte) in payload.iter_mut().enumerate() {
                    *byte ^= mask_key[i % 4];
                }
            }

            match opcode {
                0x8 => return WptWsClientMsg::Close(payload),
                0x9 => {
                    let pong = rix_net::encode_server_frame(0xA, &payload);
                    let _ = stream.write_all(&pong).await;
                }
                0xA => {}
                0x1 | 0x2 => {
                    if fin {
                        if opcode == 0x1 {
                            return WptWsClientMsg::Text(
                                String::from_utf8_lossy(&payload).into_owned(),
                            );
                        }
                        return WptWsClientMsg::Binary(payload);
                    }
                    frag_opcode = Some(opcode);
                    frag_payload = payload;
                }
                0x0 => {
                    frag_payload.extend_from_slice(&payload);
                    if fin {
                        let op = frag_opcode.take().unwrap_or(0x2);
                        let full = std::mem::take(&mut frag_payload);
                        if op == 0x1 {
                            return WptWsClientMsg::Text(
                                String::from_utf8_lossy(&full).into_owned(),
                            );
                        }
                        return WptWsClientMsg::Binary(full);
                    }
                }
                _ => return WptWsClientMsg::Eof,
            }
        }
        match stream.read(&mut tmp).await {
            Ok(0) | Err(_) => return WptWsClientMsg::Eof,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
        }
    }
}

fn encode_ws_close_payload(code: Option<u16>, reason: &str) -> Vec<u8> {
    match code {
        Some(c) => {
            let mut out = Vec::with_capacity(2 + reason.len());
            out.extend_from_slice(&c.to_be_bytes());
            out.extend_from_slice(reason.as_bytes());
            out
        }
        None => Vec::new(),
    }
}

async fn send_ws_101(
    stream: &mut tokio::net::TcpStream,
    accept: &str,
    protocol: Option<&str>,
    extra_headers: &[(&str, String)],
) -> bool {
    let mut resp = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n"
    );
    if let Some(p) = protocol {
        resp.push_str(&format!("Sec-WebSocket-Protocol: {p}\r\n"));
    }
    for (k, v) in extra_headers {
        resp.push_str(&format!("{k}: {v}\r\n"));
    }
    resp.push_str("\r\n");
    stream.write_all(resp.as_bytes()).await.is_ok()
}

/// Official WPT `websockets/handlers/*_wsh.py` local server endpoints.
#[allow(clippy::too_many_arguments)]
async fn handle_wpt_websocket_connection(
    mut stream: tokio::net::TcpStream,
    mut buf: Vec<u8>,
    rel: &str,
    query_only: &str,
    query_params: &[(String, String)],
    sec_key: &str,
    req_proto: Option<&str>,
    req_origin: &str,
    req_referer: Option<&str>,
    req_cookie: Option<&str>,
    req_auth: Option<&str>,
) {
    let route = rel
        .trim_matches('/')
        .strip_prefix("websockets/handlers/")
        .unwrap_or(rel.trim_matches('/'))
        .strip_suffix("_wsh.py")
        .unwrap_or(rel.trim_matches('/'));
    let accept = rix_net::compute_ws_accept_key(sec_key);

    let q_first = |k: &str| -> Option<String> {
        query_params
            .iter()
            .find(|(key, _)| key == k)
            .map(|(_, v)| v.clone())
    };
    let q_has = |k: &str| -> bool { query_params.iter().any(|(key, _)| key == k) };

    match route {
        "wrong_accept_key" => {
            let msg = format!(
                "HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: WebSocket\r\nSec-WebSocket-Origin: {req_origin}\r\nSec-WebSocket-Accept: thisisawrongacceptkey\r\n\r\n"
            );
            let _ = stream.write_all(msg.as_bytes()).await;
            let _ = stream.shutdown().await;
        }
        "invalid" => {
            let _ = stream.write_all(b"FOO BAR BAZ\r\n\r\n").await;
            let _ = stream.shutdown().await;
        }
        "basic_auth" => {
            if req_auth == Some("Basic Zm9vOmJhcg==") {
                if send_ws_101(&mut stream, &accept, None, &[]).await {
                    let _ = stream
                        .write_all(&rix_net::encode_server_frame(0x8, &[]))
                        .await;
                    let _ = read_wpt_ws_client_msg(&mut stream, &mut buf).await;
                }
            } else {
                let _ = stream
                    .write_all(
                        b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nWWW-Authenticate: Basic realm=\"camelot\"\r\n\r\n",
                    )
                    .await;
            }
            let _ = stream.shutdown().await;
        }
        r if r.ends_with("redirect_response.py") => {
            let _ = stream
                .write_all(
                    b"HTTP/1.1 301 Moved Permanently\r\nLocation: /echo\r\nContent-Length: 0\r\n\r\n",
                )
                .await;
            let _ = stream.shutdown().await;
        }
        "simple_handshake" => {
            let mut packet = format!(
                "HTTP/1.1 101 Switching Protocols:\r\nConnection: Upgrade\r\nUpgrade: WebSocket\r\nSet-Cookie: ws_test=test\r\nSec-WebSocket-Origin: {req_origin}\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
            )
            .into_bytes();
            let close_body = encode_ws_close_payload(Some(1001), "PASS");
            packet.extend_from_slice(&rix_net::encode_server_frame(0x8, &close_body));
            let _ = stream.write_all(&packet).await;
            let _ = read_wpt_ws_client_msg(&mut stream, &mut buf).await;
            let _ = stream.shutdown().await;
        }
        "handshake_no_protocol" => {
            if send_ws_101(&mut stream, &accept, None, &[]).await {
                let _ = stream
                    .write_all(&rix_net::encode_server_frame(0x8, &[]))
                    .await;
                let _ = read_wpt_ws_client_msg(&mut stream, &mut buf).await;
            }
            let _ = stream.shutdown().await;
        }
        "handshake_protocol" => {
            if send_ws_101(&mut stream, &accept, Some("foobar"), &[]).await {
                let _ = stream
                    .write_all(&rix_net::encode_server_frame(0x8, &[]))
                    .await;
                let _ = read_wpt_ws_client_msg(&mut stream, &mut buf).await;
            }
            let _ = stream.shutdown().await;
        }
        "handshake_no_extensions" => {
            if send_ws_101(&mut stream, &accept, None, &[]).await {
                let _ = stream
                    .write_all(&rix_net::encode_server_frame(0x8, &[]))
                    .await;
                let _ = read_wpt_ws_client_msg(&mut stream, &mut buf).await;
            }
            let _ = stream.shutdown().await;
        }
        "handshake_sleep_2" => {
            tokio::time::sleep(Duration::from_millis(2000)).await;
            if send_ws_101(&mut stream, &accept, None, &[]).await {
                let _ = read_wpt_ws_client_msg(&mut stream, &mut buf).await;
                let _ = stream
                    .write_all(&rix_net::encode_server_frame(0x8, &[]))
                    .await;
            }
            let _ = stream.shutdown().await;
        }
        "sleep_10_v13" => {
            tokio::time::sleep(Duration::from_millis(2500)).await;
            if send_ws_101(&mut stream, &accept, None, &[]).await {
                loop {
                    match read_wpt_ws_client_msg(&mut stream, &mut buf).await {
                        WptWsClientMsg::Text(t) => {
                            if t == "Goodbye" {
                                let _ = stream
                                    .write_all(&rix_net::encode_server_frame(0x8, &[]))
                                    .await;
                                break;
                            }
                            let _ = stream
                                .write_all(&rix_net::encode_server_frame(0x1, t.as_bytes()))
                                .await;
                        }
                        WptWsClientMsg::Close(p) => {
                            let _ = stream
                                .write_all(&rix_net::encode_server_frame(0x8, &p))
                                .await;
                            break;
                        }
                        _ => break,
                    }
                }
            }
            let _ = stream.shutdown().await;
        }
        "protocol" => {
            if send_ws_101(&mut stream, &accept, req_proto, &[]).await {
                let msg = req_proto.unwrap_or("");
                let _ = stream
                    .write_all(&rix_net::encode_server_frame(0x1, msg.as_bytes()))
                    .await;
                let _ = stream
                    .write_all(&rix_net::encode_server_frame(0x8, &[]))
                    .await;
                let _ = read_wpt_ws_client_msg(&mut stream, &mut buf).await;
            }
            let _ = stream.shutdown().await;
        }
        "protocol_array" => {
            let first = req_proto
                .and_then(|s| s.split(',').next())
                .map(str::trim)
                .filter(|s| !s.is_empty());
            if send_ws_101(&mut stream, &accept, first, &[]).await {
                let msg = first.unwrap_or("");
                let _ = stream
                    .write_all(&rix_net::encode_server_frame(0x1, msg.as_bytes()))
                    .await;
                let _ = stream
                    .write_all(&rix_net::encode_server_frame(0x8, &[]))
                    .await;
                let _ = read_wpt_ws_client_msg(&mut stream, &mut buf).await;
            }
            let _ = stream.shutdown().await;
        }
        "origin" => {
            if send_ws_101(&mut stream, &accept, None, &[]).await {
                let _ = stream
                    .write_all(&rix_net::encode_server_frame(0x1, req_origin.as_bytes()))
                    .await;
                let _ = stream
                    .write_all(&rix_net::encode_server_frame(0x8, &[]))
                    .await;
                let _ = read_wpt_ws_client_msg(&mut stream, &mut buf).await;
            }
            let _ = stream.shutdown().await;
        }
        "referrer" => {
            if send_ws_101(&mut stream, &accept, None, &[]).await {
                let msg = req_referer.unwrap_or("MISSING AS PER FETCH");
                let _ = stream
                    .write_all(&rix_net::encode_server_frame(0x1, msg.as_bytes()))
                    .await;
                let _ = stream
                    .write_all(&rix_net::encode_server_frame(0x8, &[]))
                    .await;
                let _ = read_wpt_ws_client_msg(&mut stream, &mut buf).await;
            }
            let _ = stream.shutdown().await;
        }
        "echo-query" | "echo-query_v13" => {
            if send_ws_101(&mut stream, &accept, None, &[]).await {
                let _ = stream
                    .write_all(&rix_net::encode_server_frame(0x1, query_only.as_bytes()))
                    .await;
                let _ = stream
                    .write_all(&rix_net::encode_server_frame(0x8, &[]))
                    .await;
                let _ = read_wpt_ws_client_msg(&mut stream, &mut buf).await;
            }
            let _ = stream.shutdown().await;
        }
        "echo-cookie" => {
            if send_ws_101(&mut stream, &accept, None, &[]).await {
                let msg = req_cookie.unwrap_or("(none)");
                let _ = stream
                    .write_all(&rix_net::encode_server_frame(0x1, msg.as_bytes()))
                    .await;
                let _ = stream
                    .write_all(&rix_net::encode_server_frame(0x8, &[]))
                    .await;
                let _ = read_wpt_ws_client_msg(&mut stream, &mut buf).await;
            }
            let _ = stream.shutdown().await;
        }
        "set-cookie" | "set-cookie-secure" | "set-cookie_http" => {
            let cookie_val = match route {
                "set-cookie-secure" => format!("ws_test_{query_only}=test; Secure; Path=/"),
                "set-cookie_http" => format!("ws_test_{query_only}=test; Path=/; HttpOnly"),
                _ => format!("ws_test_{query_only}=test; Path=/"),
            };
            if send_ws_101(&mut stream, &accept, None, &[("Set-Cookie", cookie_val)]).await {
                if let WptWsClientMsg::Close(p) =
                    read_wpt_ws_client_msg(&mut stream, &mut buf).await
                {
                    let _ = stream
                        .write_all(&rix_net::encode_server_frame(0x8, &p))
                        .await;
                }
            }
            let _ = stream.shutdown().await;
        }
        "empty-message" => {
            if send_ws_101(&mut stream, &accept, None, &[]).await {
                let resp = match read_wpt_ws_client_msg(&mut stream, &mut buf).await {
                    WptWsClientMsg::Text(t) if t.is_empty() => "pass",
                    _ => "fail",
                };
                let _ = stream
                    .write_all(&rix_net::encode_server_frame(0x1, resp.as_bytes()))
                    .await;
                let _ = stream
                    .write_all(&rix_net::encode_server_frame(0x8, &[]))
                    .await;
                let _ = read_wpt_ws_client_msg(&mut stream, &mut buf).await;
            }
            let _ = stream.shutdown().await;
        }
        "remote-close" => {
            if send_ws_101(&mut stream, &accept, None, &[]).await {
                if q_has("abrupt") {
                    let _ = stream
                        .write_all(&rix_net::encode_server_frame(0x9, b"ping"))
                        .await;
                    let _ = stream.flush().await;
                    let _ = stream.shutdown().await;
                    return;
                }
                let code = q_first("code").and_then(|s| s.parse::<u16>().ok());
                let reason = q_first("reason").unwrap_or_default();
                let payload = encode_ws_close_payload(code, &reason);
                let _ = stream
                    .write_all(&rix_net::encode_server_frame(0x8, &payload))
                    .await;
                let _ = read_wpt_ws_client_msg(&mut stream, &mut buf).await;
            }
            let _ = stream.shutdown().await;
        }
        "passive-close-abort" => {
            if send_ws_101(&mut stream, &accept, None, &[]).await {
                loop {
                    match read_wpt_ws_client_msg(&mut stream, &mut buf).await {
                        WptWsClientMsg::Close(_) | WptWsClientMsg::Eof => break,
                        _ => {}
                    }
                }
            }
            // Intentionally drop TCP socket without sending a Close frame back.
            let _ = stream.shutdown().await;
        }
        "delayed-passive-close" => {
            if send_ws_101(&mut stream, &accept, None, &[]).await {
                let close_payload = loop {
                    match read_wpt_ws_client_msg(&mut stream, &mut buf).await {
                        WptWsClientMsg::Close(p) => break p,
                        WptWsClientMsg::Eof => return,
                        _ => {}
                    }
                };
                tokio::time::sleep(Duration::from_millis(1000)).await;
                let _ = stream
                    .write_all(&rix_net::encode_server_frame(0x8, &close_payload))
                    .await;
            }
            let _ = stream.shutdown().await;
        }
        "echo_raw" => {
            if send_ws_101(&mut stream, &accept, None, &[]).await {
                loop {
                    match read_wpt_ws_client_msg(&mut stream, &mut buf).await {
                        WptWsClientMsg::Binary(b) => {
                            if b == b"exit" {
                                let _ = stream
                                    .write_all(&rix_net::encode_server_frame(0x8, &[]))
                                    .await;
                                break;
                            }
                            if stream.write_all(&b).await.is_err() {
                                break;
                            }
                        }
                        WptWsClientMsg::Text(t) => {
                            if t == "exit" {
                                let _ = stream
                                    .write_all(&rix_net::encode_server_frame(0x8, &[]))
                                    .await;
                                break;
                            }
                            if stream.write_all(t.as_bytes()).await.is_err() {
                                break;
                            }
                        }
                        WptWsClientMsg::Close(p) => {
                            let _ = stream
                                .write_all(&rix_net::encode_server_frame(0x8, &p))
                                .await;
                            break;
                        }
                        WptWsClientMsg::Eof => break,
                    }
                }
            }
            let _ = stream.shutdown().await;
        }
        "echo_exit" | "echo_close_data" => {
            if send_ws_101(&mut stream, &accept, None, &[]).await {
                loop {
                    match read_wpt_ws_client_msg(&mut stream, &mut buf).await {
                        WptWsClientMsg::Text(t) if t == "Goodbye" => {
                            let _ = stream
                                .write_all(&rix_net::encode_server_frame(0x8, &[]))
                                .await;
                            let _ = read_wpt_ws_client_msg(&mut stream, &mut buf).await;
                            break;
                        }
                        WptWsClientMsg::Close(p) => {
                            let _ = stream
                                .write_all(&rix_net::encode_server_frame(0x8, &p))
                                .await;
                            break;
                        }
                        WptWsClientMsg::Eof => break,
                        _ => {}
                    }
                }
            }
            let _ = stream.shutdown().await;
        }
        "receive-many-with-backpressure" => {
            if send_ws_101(&mut stream, &accept, None, &[]).await {
                loop {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    match read_wpt_ws_client_msg(&mut stream, &mut buf).await {
                        WptWsClientMsg::Text(t) => {
                            let len_s = t.len().to_string();
                            let _ = stream
                                .write_all(&rix_net::encode_server_frame(0x1, len_s.as_bytes()))
                                .await;
                        }
                        WptWsClientMsg::Binary(b) => {
                            let len_s = b.len().to_string();
                            let _ = stream
                                .write_all(&rix_net::encode_server_frame(0x1, len_s.as_bytes()))
                                .await;
                        }
                        WptWsClientMsg::Close(p) => {
                            let _ = stream
                                .write_all(&rix_net::encode_server_frame(0x8, &p))
                                .await;
                            break;
                        }
                        WptWsClientMsg::Eof => break,
                    }
                }
            }
            let _ = stream.shutdown().await;
        }
        "echo" | "" => {
            // Default `/echo` and `/` root echo handler.
            let chosen_proto =
                req_proto.and_then(|s| s.split(',').map(str::trim).find(|p| *p == "echo"));
            if send_ws_101(&mut stream, &accept, chosen_proto, &[]).await {
                loop {
                    match read_wpt_ws_client_msg(&mut stream, &mut buf).await {
                        WptWsClientMsg::Text(t) => {
                            if stream
                                .write_all(&rix_net::encode_server_frame(0x1, t.as_bytes()))
                                .await
                                .is_err()
                            {
                                break;
                            }
                            if t == "Goodbye" {
                                let _ = stream
                                    .write_all(&rix_net::encode_server_frame(0x8, &[]))
                                    .await;
                                let _ = read_wpt_ws_client_msg(&mut stream, &mut buf).await;
                                break;
                            }
                        }
                        WptWsClientMsg::Binary(b) => {
                            if stream
                                .write_all(&rix_net::encode_server_frame(0x2, &b))
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                        WptWsClientMsg::Close(p) => {
                            let _ = stream
                                .write_all(&rix_net::encode_server_frame(0x8, &p))
                                .await;
                            break;
                        }
                        WptWsClientMsg::Eof => break,
                    }
                }
            }
            let _ = stream.shutdown().await;
        }
        _ => {
            let _ = stream
                .write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await;
            let _ = stream.shutdown().await;
        }
    }
}
