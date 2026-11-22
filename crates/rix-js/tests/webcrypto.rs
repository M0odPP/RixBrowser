//! Integration tests & NIST/RFC test vectors for W3C WebCrypto (`crypto.getRandomValues`,
//! `crypto.randomUUID`, `crypto.subtle`) in `rix-js`.

use rix_js::JsVm;

fn eval_async(vm: &mut JsVm, code: &str) -> serde_json::Value {
    vm.eval_script(
        r#"
        globalThis.__done = false;
        globalThis.__err = "";
        globalThis.__res = null;
        function toHex(buf) {
            return Array.from(new Uint8Array(buf))
                .map((b) => b.toString(16).padStart(2, "0"))
                .join("");
        }
        function fromHex(hex) {
            const out = new Uint8Array(hex.length / 2);
            for (let i = 0; i < out.length; i++) {
                out[i] = parseInt(hex.slice(i * 2, i * 2 + 2), 16);
            }
            return out;
        }
        function utf8(s) {
            const out = new Uint8Array(s.length);
            for (let i = 0; i < s.length; i++) out[i] = s.charCodeAt(i);
            return out;
        }
        "#,
    )
    .expect("prelude");

    let wrapped = format!(
        "(async () => {{ {code} }})().then((v) => {{ globalThis.__res = JSON.stringify(v); globalThis.__done = true; }}).catch((e) => {{ globalThis.__err = String(e && e.name ? e.name + ': ' + e.message : e); globalThis.__done = true; }});"
    );
    vm.eval_script(&wrapped).expect("eval async");
    vm.drain_microtasks();

    let err_v = vm.eval_script("globalThis.__err").expect("err");
    let err_s = vm.value_to_rust_string(err_v);
    assert!(err_s.is_empty(), "WebCrypto JS test failed: {err_s}");
    assert_eq!(
        vm.eval_script("globalThis.__done").unwrap().as_bool(),
        Some(true),
        "async WebCrypto test did not settle"
    );
    let res_v = vm.eval_script("globalThis.__res").expect("res");
    serde_json::from_str(&vm.value_to_rust_string(res_v)).expect("valid json")
}

#[test]
fn crypto_random_values_uuid_and_nist_digests() {
    let mut vm = JsVm::new();
    let v = eval_async(
        &mut vm,
        r#"
        const buf = new Uint8Array(32);
        const same = crypto.getRandomValues(buf);
        const nonZero = Array.from(buf).some((b) => b !== 0);

        let floatErr = "";
        try { crypto.getRandomValues(new Float32Array(4)); } catch (e) { floatErr = e.name; }
        let quotaErr = "";
        try { crypto.getRandomValues(new Uint8Array(65537)); } catch (e) { quotaErr = e.name; }

        const u1 = crypto.randomUUID();
        const u2 = crypto.randomUUID();
        const uuidRe = new RegExp("^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$");

        const sha1 = toHex(await crypto.subtle.digest("SHA-1", utf8("abc")));
        const sha256 = toHex(await crypto.subtle.digest("SHA-256", utf8("abc")));
        const sha384 = toHex(await crypto.subtle.digest({ name: "SHA-384" }, utf8("abc")));
        const sha512 = toHex(await crypto.subtle.digest("SHA-512", utf8("abc")));

        return {
            sameRef: same === buf,
            nonZero,
            floatErr,
            quotaErr,
            u1Valid: uuidRe.test(u1),
            u2Valid: uuidRe.test(u2),
            uuidsDistinct: u1 !== u2,
            sha1,
            sha256,
            sha384,
            sha512,
        };
        "#,
    );
    assert_eq!(v["sameRef"], true);
    assert_eq!(v["nonZero"], true);
    assert_eq!(v["floatErr"], "TypeMismatchError");
    assert_eq!(v["quotaErr"], "QuotaExceededError");
    assert_eq!(v["u1Valid"], true);
    assert_eq!(v["u2Valid"], true);
    assert_eq!(v["uuidsDistinct"], true);
    // Exact NIST FIPS 180-4 vectors for "abc"
    assert_eq!(v["sha1"], "a9993e364706816aba3e25717850c26c9cd0d89d");
    assert_eq!(
        v["sha256"],
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(
        v["sha384"],
        "cb00753f45a35e8bb5a03d699ac65007272c32ab0eded1631a8b605a43ff5bed8086072ba1e7cc2358baeca134c825a7"
    );
    assert_eq!(
        v["sha512"],
        "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"
    );
}

#[test]
fn webcrypto_hmac_aes_gcm_cbc_ctr_hkdf_pbkdf2_and_wrap() {
    let mut vm = JsVm::new();
    let v = eval_async(
        &mut vm,
        r#"
        // RFC 4231 Test Case 2: HMAC-SHA-256("Jefe", "what do ya want for nothing?")
        const hmacKey = await crypto.subtle.importKey(
            "raw",
            utf8("Jefe"),
            { name: "HMAC", hash: "SHA-256" },
            true,
            ["sign", "verify"]
        );
        const mac = await crypto.subtle.sign("HMAC", hmacKey, utf8("what do ya want for nothing?"));
        const macOk = await crypto.subtle.verify("HMAC", hmacKey, mac, utf8("what do ya want for nothing?"));
        const macBad = await crypto.subtle.verify("HMAC", hmacKey, mac, utf8("tampered"));
        const hmacJwk = await crypto.subtle.exportKey("jwk", hmacKey);

        // AES-GCM 256 with AAD + tamper rejection + non-extractable check
        const gcmKey = await crypto.subtle.generateKey({ name: "AES-GCM", length: 256 }, false, ["encrypt", "decrypt"]);
        let nonExtErr = "";
        try { await crypto.subtle.exportKey("raw", gcmKey); } catch (e) { nonExtErr = e.name; }
        const iv12 = crypto.getRandomValues(new Uint8Array(12));
        const aad = utf8("header-aad");
        const ct = await crypto.subtle.encrypt({ name: "AES-GCM", iv: iv12, additionalData: aad }, gcmKey, utf8("secret-payload"));
        const pt = await crypto.subtle.decrypt({ name: "AES-GCM", iv: iv12, additionalData: aad }, gcmKey, ct);
        let aadTamperErr = "";
        try {
            await crypto.subtle.decrypt({ name: "AES-GCM", iv: iv12, additionalData: utf8("wrong-aad") }, gcmKey, ct);
        } catch (e) { aadTamperErr = e.name; }

        // AES-CBC 128 encrypt/decrypt
        const cbcKey = await crypto.subtle.generateKey({ name: "AES-CBC", length: 128 }, true, ["encrypt", "decrypt"]);
        const iv16 = crypto.getRandomValues(new Uint8Array(16));
        const cbcCt = await crypto.subtle.encrypt({ name: "AES-CBC", iv: iv16 }, cbcKey, utf8("hello-cbc-world!"));
        const cbcPt = await crypto.subtle.decrypt({ name: "AES-CBC", iv: iv16 }, cbcKey, cbcCt);

        // AES-CTR 192 encrypt/decrypt
        const ctrKey = await crypto.subtle.generateKey({ name: "AES-CTR", length: 192 }, true, ["encrypt", "decrypt"]);
        const counter = crypto.getRandomValues(new Uint8Array(16));
        const ctrCt = await crypto.subtle.encrypt({ name: "AES-CTR", counter, length: 64 }, ctrKey, utf8("ctr-stream-data"));
        const ctrPt = await crypto.subtle.decrypt({ name: "AES-CTR", counter, length: 64 }, ctrKey, ctrCt);

        // RFC 5869 HKDF-SHA-256 Test Case 1
        const hkdfBase = await crypto.subtle.importKey(
            "raw",
            fromHex("0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b"),
            "HKDF",
            false,
            ["deriveBits", "deriveKey"]
        );
        const okm = await crypto.subtle.deriveBits(
            {
                name: "HKDF",
                hash: "SHA-256",
                salt: fromHex("000102030405060708090a0b0c"),
                info: fromHex("f0f1f2f3f4f5f6f7f8f9"),
            },
            hkdfBase,
            42 * 8
        );

        // PBKDF2-SHA-256 deriveKey -> AES-GCM + wrapKey/unwrapKey
        const pwKey = await crypto.subtle.importKey("raw", utf8("password"), "PBKDF2", false, ["deriveBits", "deriveKey"]);
        const pbkBits = await crypto.subtle.deriveBits(
            { name: "PBKDF2", hash: "SHA-256", salt: utf8("salt"), iterations: 1 },
            pwKey,
            256
        );
        const wrapKek = await crypto.subtle.deriveKey(
            { name: "PBKDF2", hash: "SHA-256", salt: utf8("salt"), iterations: 10 },
            pwKey,
            { name: "AES-GCM", length: 256 },
            false,
            ["wrapKey", "unwrapKey"]
        );
        const wrapIv = crypto.getRandomValues(new Uint8Array(12));
        const wrapped = await crypto.subtle.wrapKey("jwk", cbcKey, wrapKek, { name: "AES-GCM", iv: wrapIv });
        const unwrapped = await crypto.subtle.unwrapKey(
            "jwk",
            wrapped,
            wrapKek,
            { name: "AES-GCM", iv: wrapIv },
            { name: "AES-CBC", length: 128 },
            true,
            ["decrypt"]
        );
        const unwrappedPt = await crypto.subtle.decrypt({ name: "AES-CBC", iv: iv16 }, unwrapped, cbcCt);

        return {
            macHex: toHex(mac),
            macOk,
            macBad,
            hmacJwkAlg: hmacJwk.alg,
            nonExtErr,
            gcmRoundtrip: toHex(pt) === toHex(utf8("secret-payload")),
            aadTamperErr,
            cbcRoundtrip: toHex(cbcPt) === toHex(utf8("hello-cbc-world!")),
            ctrRoundtrip: toHex(ctrPt) === toHex(utf8("ctr-stream-data")),
            hkdfOkm: toHex(okm),
            pbkdf2Vec: toHex(pbkBits),
            wrapRoundtrip: toHex(unwrappedPt) === toHex(utf8("hello-cbc-world!")),
        };
        "#,
    );
    assert_eq!(
        v["macHex"],
        "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
    );
    assert_eq!(v["macOk"], true);
    assert_eq!(v["macBad"], false);
    assert_eq!(v["hmacJwkAlg"], "HS256");
    assert_eq!(v["nonExtErr"], "InvalidAccessError");
    assert_eq!(v["gcmRoundtrip"], true);
    assert_eq!(v["aadTamperErr"], "OperationError");
    assert_eq!(v["cbcRoundtrip"], true);
    assert_eq!(v["ctrRoundtrip"], true);
    assert_eq!(
        v["hkdfOkm"],
        "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865"
    );
    // RFC 7914 / PBKDF2-HMAC-SHA256("password", "salt", 1, 32)
    assert_eq!(
        v["pbkdf2Vec"],
        "120fb6cffcf8b32c43e7225256c4f837a86548c92ccc35480805987cb70be17b"
    );
    assert_eq!(v["wrapRoundtrip"], true);
}

#[test]
fn webcrypto_ed25519_ecdsa_ecdh_and_rsa_oaep_pss_pkcs1() {
    let mut vm = JsVm::new();
    let v = eval_async(
        &mut vm,
        r#"
        // Ed25519 sign/verify + pkcs8/spki/jwk round-trip
        const ed = await crypto.subtle.generateKey("Ed25519", true, ["sign", "verify"]);
        const edSig = await crypto.subtle.sign("Ed25519", ed.privateKey, utf8("ed25519-msg"));
        const edSpki = await crypto.subtle.exportKey("spki", ed.publicKey);
        const edPub2 = await crypto.subtle.importKey("spki", edSpki, "Ed25519", true, ["verify"]);
        const edOk = await crypto.subtle.verify("Ed25519", edPub2, edSig, utf8("ed25519-msg"));
        const edJwk = await crypto.subtle.exportKey("jwk", ed.privateKey);
        const edPriv2 = await crypto.subtle.importKey("jwk", edJwk, "Ed25519", true, ["sign"]);
        const edSig2 = await crypto.subtle.sign("Ed25519", edPriv2, utf8("ed25519-msg"));
        const edOk2 = await crypto.subtle.verify("Ed25519", ed.publicKey, edSig2, utf8("ed25519-msg"));

        // ECDSA P-256 & P-384 (W3C IEEE P1363 64-byte / 96-byte signatures) + JWK round-trip
        const ec256 = await crypto.subtle.generateKey({ name: "ECDSA", namedCurve: "P-256" }, true, ["sign", "verify"]);
        const ec256Sig = await crypto.subtle.sign({ name: "ECDSA", hash: "SHA-256" }, ec256.privateKey, utf8("ecdsa-256"));
        const ec256Jwk = await crypto.subtle.exportKey("jwk", ec256.publicKey);
        const ec256Pub2 = await crypto.subtle.importKey("jwk", ec256Jwk, { name: "ECDSA", namedCurve: "P-256" }, true, ["verify"]);
        const ec256Ok = await crypto.subtle.verify({ name: "ECDSA", hash: "SHA-256" }, ec256Pub2, ec256Sig, utf8("ecdsa-256"));

        const ec384 = await crypto.subtle.generateKey({ name: "ECDSA", namedCurve: "P-384" }, true, ["sign", "verify"]);
        const ec384Sig = await crypto.subtle.sign({ name: "ECDSA", hash: "SHA-384" }, ec384.privateKey, utf8("ecdsa-384"));
        const ec384Ok = await crypto.subtle.verify({ name: "ECDSA", hash: "SHA-384" }, ec384.publicKey, ec384Sig, utf8("ecdsa-384"));

        // ECDH P-256 key agreement -> shared AES-GCM key
        const alice = await crypto.subtle.generateKey({ name: "ECDH", namedCurve: "P-256" }, true, ["deriveKey", "deriveBits"]);
        const bob = await crypto.subtle.generateKey({ name: "ECDH", namedCurve: "P-256" }, true, ["deriveKey", "deriveBits"]);
        const aBits = toHex(await crypto.subtle.deriveBits({ name: "ECDH", public: bob.publicKey }, alice.privateKey, 256));
        const bBits = toHex(await crypto.subtle.deriveBits({ name: "ECDH", public: alice.publicKey }, bob.privateKey, 256));

        // RSA-OAEP encrypt/decrypt with label + PKCS#8/SPKI round-trip (1024-bit for fast CI test)
        const rsaEnc = await crypto.subtle.generateKey(
            { name: "RSA-OAEP", modulusLength: 1024, publicExponent: new Uint8Array([1, 0, 1]), hash: "SHA-256" },
            true,
            ["encrypt", "decrypt"]
        );
        const rsaCt = await crypto.subtle.encrypt({ name: "RSA-OAEP", label: utf8("lbl") }, rsaEnc.publicKey, utf8("rsa-oaep-secret"));
        const rsaPkcs8 = await crypto.subtle.exportKey("pkcs8", rsaEnc.privateKey);
        const rsaPriv2 = await crypto.subtle.importKey("pkcs8", rsaPkcs8, { name: "RSA-OAEP", hash: "SHA-256" }, true, ["decrypt"]);
        const rsaPt = await crypto.subtle.decrypt({ name: "RSA-OAEP", label: utf8("lbl") }, rsaPriv2, rsaCt);

        // RSA-PSS sign/verify
        const rsaPss = await crypto.subtle.generateKey(
            { name: "RSA-PSS", modulusLength: 1024, publicExponent: new Uint8Array([1, 0, 1]), hash: "SHA-256" },
            true,
            ["sign", "verify"]
        );
        const pssSig = await crypto.subtle.sign({ name: "RSA-PSS", saltLength: 32 }, rsaPss.privateKey, utf8("pss-data"));
        const pssOk = await crypto.subtle.verify({ name: "RSA-PSS", saltLength: 32 }, rsaPss.publicKey, pssSig, utf8("pss-data"));

        // RSASSA-PKCS1-v1_5 sign/verify + JWK round-trip
        const rsaPkcs1 = await crypto.subtle.generateKey(
            { name: "RSASSA-PKCS1-v1_5", modulusLength: 1024, publicExponent: new Uint8Array([1, 0, 1]), hash: "SHA-256" },
            true,
            ["sign", "verify"]
        );
        const p1Sig = await crypto.subtle.sign("RSASSA-PKCS1-v1_5", rsaPkcs1.privateKey, utf8("pkcs1-data"));
        const p1Jwk = await crypto.subtle.exportKey("jwk", rsaPkcs1.publicKey);
        const p1Pub2 = await crypto.subtle.importKey("jwk", p1Jwk, { name: "RSASSA-PKCS1-v1_5", hash: "SHA-256" }, true, ["verify"]);
        const p1Ok = await crypto.subtle.verify("RSASSA-PKCS1-v1_5", p1Pub2, p1Sig, utf8("pkcs1-data"));

        return {
            edSigLen: edSig.byteLength,
            edOk,
            edOk2,
            ec256SigLen: ec256Sig.byteLength,
            ec256Ok,
            ec384SigLen: ec384Sig.byteLength,
            ec384Ok,
            ecdhMatch: aBits.length === 64 && aBits === bBits,
            rsaOaepOk: toHex(rsaPt) === toHex(utf8("rsa-oaep-secret")),
            pssOk,
            p1Ok,
        };
        "#,
    );
    assert_eq!(v["edSigLen"], 64);
    assert_eq!(v["edOk"], true);
    assert_eq!(v["edOk2"], true);
    assert_eq!(v["ec256SigLen"], 64);
    assert_eq!(v["ec256Ok"], true);
    assert_eq!(v["ec384SigLen"], 96);
    assert_eq!(v["ec384Ok"], true);
    assert_eq!(v["ecdhMatch"], true);
    assert_eq!(v["rsaOaepOk"], true);
    assert_eq!(v["pssOk"], true);
    assert_eq!(v["p1Ok"], true);
}
