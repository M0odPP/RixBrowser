//! W3C Web Cryptography API (`crypto.getRandomValues`, `crypto.randomUUID`,
//! `crypto.subtle` / `SubtleCrypto`, `CryptoKey`, `Crypto`) implemented in pure
//! Rust over audited RustCrypto crates (`sha1`, `sha2`, `hmac`, `hkdf`, `pbkdf2`,
//! `aes`, `aes-gcm`, `cbc`, `ctr`, `ed25519-dalek`, `p256`, `p384`, `rsa`, `getrandom`).
//!
//! Reference: Spec 06 §3, Issue #25.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use aes::cipher::block_padding::Pkcs7;
use aes::cipher::{BlockModeDecrypt, BlockModeEncrypt, KeyIvInit};
use aes::{Aes128, Aes192, Aes256};
use aes_gcm::aead::consts::U12;
use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes128Gcm, Aes256Gcm, AesGcm};
use ed25519_dalek::{
    Signature as Ed25519Sig, Signer as EdSigner, SigningKey as EdSigningKey,
    Verifier as EdVerifier, VerifyingKey as EdVerifyingKey,
};
use getrandom::SysRng;
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use p256::ecdsa::signature::hazmat::{PrehashSigner, PrehashVerifier};
use p256::elliptic_curve::Generate;
use p256::pkcs8::{DecodePrivateKey, DecodePublicKey, EncodePrivateKey, EncodePublicKey};
use pbkdf2::pbkdf2_hmac;
use rsa::oaep::{DecryptingKey as OaepDecryptor, EncryptingKey as OaepEncryptor};
use rsa::pss::{SigningKey as PssSigner, VerifyingKey as PssVerifier};
use rsa::signature::{RandomizedSigner, SignatureEncoding};
use rsa::traits::{Decryptor, PublicKeyParts, RandomizedEncryptor};
use rsa::{BoxedUint, PrivateKey as RsaPrivateKey, PublicKey as RsaPublicKey};
use sha1::Sha1;
use sha2::{Digest, Sha256, Sha384, Sha512};

use crate::builtins_collections::alloc_array_buffer;
use crate::heap::{JsObject, ObjectKind, TypedArrayKind};
use crate::promise::{alloc_promise, reject_promise, resolve_promise};
use crate::shape::PropertyAttrs;
use crate::value::{HeapHandle, JsValue};
use crate::vm::{JsException, JsVm};

type Aes192Gcm = AesGcm<Aes192, U12>;

// ---------------------------------------------------------------------------
// Core WebCrypto domain types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HashAlg {
    Sha1,
    Sha256,
    Sha384,
    Sha512,
}

impl HashAlg {
    fn parse(name: &str) -> Option<Self> {
        match name.to_ascii_uppercase().as_str() {
            "SHA-1" => Some(Self::Sha1),
            "SHA-256" => Some(Self::Sha256),
            "SHA-384" => Some(Self::Sha384),
            "SHA-512" => Some(Self::Sha512),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Sha1 => "SHA-1",
            Self::Sha256 => "SHA-256",
            Self::Sha384 => "SHA-384",
            Self::Sha512 => "SHA-512",
        }
    }

    fn digest_bytes(self, data: &[u8]) -> Vec<u8> {
        match self {
            Self::Sha1 => Sha1::digest(data).to_vec(),
            Self::Sha256 => Sha256::digest(data).to_vec(),
            Self::Sha384 => Sha384::digest(data).to_vec(),
            Self::Sha512 => Sha512::digest(data).to_vec(),
        }
    }

    fn block_size_bytes(self) -> usize {
        match self {
            Self::Sha1 | Self::Sha256 => 64,
            Self::Sha384 | Self::Sha512 => 128,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NamedCurve {
    P256,
    P384,
}

impl NamedCurve {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "P-256" => Some(Self::P256),
            "P-384" => Some(Self::P384),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::P256 => "P-256",
            Self::P384 => "P-384",
        }
    }

    fn coord_len(self) -> usize {
        match self {
            Self::P256 => 32,
            Self::P384 => 48,
        }
    }
}

#[derive(Clone)]
enum KeyMaterial {
    Aes {
        alg: String,
        bytes: Vec<u8>,
    },
    Hmac {
        hash: HashAlg,
        bytes: Vec<u8>,
    },
    RawKdf {
        alg: String,
        bytes: Vec<u8>,
    },
    Ed25519Private(EdSigningKey),
    Ed25519Public(EdVerifyingKey),
    EcP256Private {
        alg: String,
        key: p256::SecretKey,
    },
    EcP256Public {
        alg: String,
        key: p256::PublicKey,
    },
    EcP384Private {
        alg: String,
        key: p384::SecretKey,
    },
    EcP384Public {
        alg: String,
        key: p384::PublicKey,
    },
    RsaPrivate {
        alg: String,
        hash: HashAlg,
        key: RsaPrivateKey,
    },
    RsaPublic {
        alg: String,
        hash: HashAlg,
        key: RsaPublicKey,
    },
}

#[derive(Clone)]
struct CryptoKeyRecord {
    key_type: &'static str, // "secret" | "public" | "private"
    extractable: bool,
    usages: Vec<String>,
    material: KeyMaterial,
}

impl CryptoKeyRecord {
    fn alg_name(&self) -> &str {
        match &self.material {
            KeyMaterial::Aes { alg, .. }
            | KeyMaterial::RawKdf { alg, .. }
            | KeyMaterial::EcP256Private { alg, .. }
            | KeyMaterial::EcP256Public { alg, .. }
            | KeyMaterial::EcP384Private { alg, .. }
            | KeyMaterial::EcP384Public { alg, .. }
            | KeyMaterial::RsaPrivate { alg, .. }
            | KeyMaterial::RsaPublic { alg, .. } => alg.as_str(),
            KeyMaterial::Hmac { .. } => "HMAC",
            KeyMaterial::Ed25519Private(_) | KeyMaterial::Ed25519Public(_) => "Ed25519",
        }
    }

    fn has_usage(&self, u: &str) -> bool {
        self.usages.iter().any(|x| x == u)
    }
}

#[derive(Default)]
struct CryptoState {
    keys: HashMap<HeapHandle, CryptoKeyRecord>,
    crypto_key_proto: Option<HeapHandle>,
}

type SharedCrypto = Rc<RefCell<CryptoState>>;

#[derive(Debug)]
struct CryptoErr {
    name: &'static str,
    message: String,
}

impl CryptoErr {
    fn new(name: &'static str, msg: impl Into<String>) -> Self {
        Self {
            name,
            message: msg.into(),
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers: DOMException-style errors, base64url, BufferSource extraction
// ---------------------------------------------------------------------------

fn arg(args: &[JsValue], i: usize) -> JsValue {
    args.get(i).copied().unwrap_or(JsValue::UNDEFINED)
}

fn str_val(vm: &mut JsVm, s: &str) -> JsValue {
    JsValue::from_string(vm.heap.alloc_string(s))
}

fn def_data(vm: &mut JsVm, obj: HeapHandle, name: &str, val: JsValue, attrs: PropertyAttrs) {
    let k = vm.heap.intern_key(name);
    vm.heap
        .object_define_data(obj, k, val, attrs, &mut vm.shapes);
}

fn def_host_method(
    vm: &mut JsVm,
    obj: HeapHandle,
    name: &str,
    f: impl Fn(&mut JsVm, JsValue, &[JsValue]) -> Result<JsValue, JsException> + 'static,
) {
    let fh = vm.alloc_host_fn(name, f);
    def_data(
        vm,
        obj,
        name,
        JsValue::from_object(fh),
        PropertyAttrs::METHOD,
    );
}

fn def_host_getter(
    vm: &mut JsVm,
    obj: HeapHandle,
    name: &str,
    f: impl Fn(&mut JsVm, JsValue, &[JsValue]) -> Result<JsValue, JsException> + 'static,
) {
    let k = vm.heap.intern_key(name);
    let fh = vm.alloc_host_fn(name, f);
    vm.heap.object_define_accessor(
        obj,
        k,
        Some(fh),
        None,
        PropertyAttrs::CONFIGURABLE,
        &mut vm.shapes,
    );
}

fn bind_ctor_proto(vm: &mut JsVm, ctor: HeapHandle, proto: HeapHandle) {
    def_data(
        vm,
        ctor,
        "prototype",
        JsValue::from_object(proto),
        PropertyAttrs::READONLY,
    );
    def_data(
        vm,
        proto,
        "constructor",
        JsValue::from_object(ctor),
        PropertyAttrs::METHOD,
    );
}

fn make_crypto_error_val(vm: &mut JsVm, err: &CryptoErr) -> JsValue {
    let obj = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(vm.error_proto),
    ));
    let nv = str_val(vm, err.name);
    let mv = str_val(vm, &err.message);
    def_data(vm, obj, "name", nv, PropertyAttrs::DEFAULT);
    def_data(vm, obj, "message", mv, PropertyAttrs::DEFAULT);
    JsValue::from_object(obj)
}

fn make_crypto_exception(vm: &mut JsVm, name: &'static str, msg: impl Into<String>) -> JsException {
    let err = CryptoErr::new(name, msg);
    let value = make_crypto_error_val(vm, &err);
    JsException {
        value,
        message: format!("{}: {}", err.name, err.message),
    }
}

fn settle_promise(vm: &mut JsVm, res: Result<JsValue, CryptoErr>) -> JsValue {
    let p = alloc_promise(vm);
    match res {
        Ok(v) => resolve_promise(vm, p, v),
        Err(e) => {
            let ev = make_crypto_error_val(vm, &e);
            reject_promise(vm, p, ev);
        }
    }
    JsValue::from_object(p)
}

fn alloc_array_of(vm: &mut JsVm, items: &[JsValue]) -> HeapHandle {
    let arr = vm.alloc_array();
    for &v in items {
        vm.heap.object_push_element(arr, v);
    }
    arr
}

fn read_buffer_source(vm: &JsVm, val: JsValue) -> Result<Vec<u8>, CryptoErr> {
    let Some(h) = val.as_object() else {
        return Err(CryptoErr::new(
            "TypeError",
            "Expected BufferSource (ArrayBuffer, TypedArray, or DataView)",
        ));
    };
    match vm.heap.get_object(h).map(|o| &o.kind) {
        Some(ObjectKind::ArrayBuffer(buf)) => Ok(buf.as_ref().clone()),
        Some(ObjectKind::TypedArray(ta)) => {
            let start = ta.byte_offset as usize;
            let len = (ta.length as usize).saturating_mul(ta.kind.bytes_per_element());
            match vm.heap.get_object(ta.buffer).map(|o| &o.kind) {
                Some(ObjectKind::ArrayBuffer(buf)) => {
                    let s = start.min(buf.len());
                    let e = s.saturating_add(len).min(buf.len());
                    Ok(buf[s..e].to_vec())
                }
                _ => Err(CryptoErr::new("TypeError", "Invalid TypedArray buffer")),
            }
        }
        Some(ObjectKind::DataView(dv)) => {
            let start = dv.byte_offset as usize;
            let len = dv.byte_length as usize;
            match vm.heap.get_object(dv.buffer).map(|o| &o.kind) {
                Some(ObjectKind::ArrayBuffer(buf)) => {
                    let s = start.min(buf.len());
                    let e = s.saturating_add(len).min(buf.len());
                    Ok(buf[s..e].to_vec())
                }
                _ => Err(CryptoErr::new("TypeError", "Invalid DataView buffer")),
            }
        }
        _ => Err(CryptoErr::new(
            "TypeError",
            "Expected BufferSource (ArrayBuffer, TypedArray, or DataView)",
        )),
    }
}

fn get_prop(vm: &mut JsVm, val: JsValue, name: &str) -> Result<JsValue, CryptoErr> {
    let Some(h) = val.as_object() else {
        return Ok(JsValue::UNDEFINED);
    };
    let k = vm.heap.intern_key(name);
    vm.get_property_by_key(h, k)
        .map_err(|e| CryptoErr::new("TypeError", e.message))
}

fn get_prop_bytes(vm: &mut JsVm, val: JsValue, name: &str) -> Result<Vec<u8>, CryptoErr> {
    let v = get_prop(vm, val, name)?;
    read_buffer_source(vm, v)
}

fn get_prop_str(vm: &mut JsVm, val: JsValue, name: &str) -> Result<String, CryptoErr> {
    let v = get_prop(vm, val, name)?;
    Ok(vm.value_to_rust_string(v))
}

fn parse_algorithm_name(vm: &mut JsVm, alg: JsValue) -> Result<String, CryptoErr> {
    if alg.is_string() {
        return Ok(vm.value_to_rust_string(alg));
    }
    let name_v = get_prop(vm, alg, "name")?;
    if name_v.is_undefined() || name_v.is_null() {
        return Err(CryptoErr::new("TypeError", "Algorithm must specify a name"));
    }
    Ok(vm.value_to_rust_string(name_v))
}

fn parse_hash_from_alg(vm: &mut JsVm, alg: JsValue) -> Result<HashAlg, CryptoErr> {
    let hash_v = get_prop(vm, alg, "hash")?;
    let name = parse_algorithm_name(vm, hash_v)?;
    HashAlg::parse(&name)
        .ok_or_else(|| CryptoErr::new("NotSupportedError", format!("Unsupported hash: {name}")))
}

fn parse_usages(vm: &mut JsVm, val: JsValue) -> Result<Vec<String>, CryptoErr> {
    let Some(h) = val.as_object() else {
        return Err(CryptoErr::new("TypeError", "keyUsages must be an array"));
    };
    let len = match vm.heap.get_object(h) {
        Some(o) if matches!(o.kind, ObjectKind::Array) => o.elements.len() as usize,
        _ => {
            let lk = vm.heap.intern_key("length");
            let lv = vm
                .get_property_by_key(h, lk)
                .map_err(|e| CryptoErr::new("TypeError", e.message))?;
            vm.to_number(lv).unwrap_or(0.0).max(0.0) as usize
        }
    };
    let mut out = Vec::new();
    for i in 0..len {
        let elem = if let Ok(idx) = u32::try_from(i) {
            vm.heap
                .get_object(h)
                .and_then(|o| o.elements.get(idx))
                .filter(|v| !v.is_empty())
        } else {
            None
        };
        let v = match elem {
            Some(v) => v,
            None => {
                let k = vm.heap.intern_key(&i.to_string());
                vm.get_property_by_key(h, k)
                    .map_err(|e| CryptoErr::new("TypeError", e.message))?
            }
        };
        let s = vm.value_to_rust_string(v);
        if !out.contains(&s) {
            out.push(s);
        }
    }
    Ok(out)
}

fn require_key(
    state: &SharedCrypto,
    val: JsValue,
    required_usage: Option<&str>,
) -> Result<CryptoKeyRecord, CryptoErr> {
    let Some(h) = val.as_object() else {
        return Err(CryptoErr::new("TypeError", "Expected a CryptoKey"));
    };
    let rec = state
        .borrow()
        .keys
        .get(&h)
        .cloned()
        .ok_or_else(|| CryptoErr::new("TypeError", "Expected a CryptoKey"))?;
    if let Some(u) = required_usage {
        if !rec.has_usage(u) {
            return Err(CryptoErr::new(
                "InvalidAccessError",
                format!("CryptoKey does not permit usage '{u}'"),
            ));
        }
    }
    Ok(rec)
}

fn base64url_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity((bytes.len() * 4).div_ceil(3));
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        out.push(TABLE[(b0 >> 2) as usize] as char);
        out.push(TABLE[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(TABLE[(b2 & 0x3f) as usize] as char);
        }
    }
    out
}

fn base64url_decode(input: &str) -> Result<Vec<u8>, CryptoErr> {
    let mut vals = Vec::with_capacity(input.len());
    for b in input.bytes() {
        if b == b'=' {
            continue;
        }
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            _ => return Err(CryptoErr::new("DataError", "Invalid base64url character")),
        };
        vals.push(v);
    }
    let mut out = Vec::with_capacity(vals.len() * 3 / 4);
    for chunk in vals.chunks(4) {
        if chunk.len() < 2 {
            return Err(CryptoErr::new("DataError", "Invalid base64url length"));
        }
        out.push((chunk[0] << 2) | (chunk[1] >> 4));
        if chunk.len() > 2 {
            out.push((chunk[1] << 4) | (chunk[2] >> 2));
        }
        if chunk.len() > 3 {
            out.push((chunk[2] << 6) | chunk[3]);
        }
    }
    Ok(out)
}

fn fill_random(buf: &mut [u8]) -> Result<(), CryptoErr> {
    getrandom::fill(buf).map_err(|e| CryptoErr::new("OperationError", format!("RNG error: {e}")))
}

fn alloc_crypto_key(vm: &mut JsVm, state: &SharedCrypto, rec: CryptoKeyRecord) -> HeapHandle {
    let proto = state.borrow().crypto_key_proto.unwrap_or(vm.object_proto);
    let h = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
    state.borrow_mut().keys.insert(h, rec);
    h
}

fn build_key_algorithm_js(vm: &mut JsVm, rec: &CryptoKeyRecord) -> HeapHandle {
    let obj = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(vm.object_proto),
    ));
    let name_v = str_val(vm, rec.alg_name());
    def_data(vm, obj, "name", name_v, PropertyAttrs::DEFAULT);
    match &rec.material {
        KeyMaterial::Aes { bytes, .. } => {
            def_data(
                vm,
                obj,
                "length",
                JsValue::from_number((bytes.len() * 8) as f64),
                PropertyAttrs::DEFAULT,
            );
        }
        KeyMaterial::Hmac { hash, bytes } => {
            let hobj = vm.heap.alloc_object(JsObject::with_kind(
                ObjectKind::Ordinary,
                Some(vm.object_proto),
            ));
            let hv = str_val(vm, hash.as_str());
            def_data(vm, hobj, "name", hv, PropertyAttrs::DEFAULT);
            def_data(
                vm,
                obj,
                "hash",
                JsValue::from_object(hobj),
                PropertyAttrs::DEFAULT,
            );
            def_data(
                vm,
                obj,
                "length",
                JsValue::from_number((bytes.len() * 8) as f64),
                PropertyAttrs::DEFAULT,
            );
        }
        KeyMaterial::EcP256Private { .. } | KeyMaterial::EcP256Public { .. } => {
            let cv = str_val(vm, "P-256");
            def_data(vm, obj, "namedCurve", cv, PropertyAttrs::DEFAULT);
        }
        KeyMaterial::EcP384Private { .. } | KeyMaterial::EcP384Public { .. } => {
            let cv = str_val(vm, "P-384");
            def_data(vm, obj, "namedCurve", cv, PropertyAttrs::DEFAULT);
        }
        KeyMaterial::RsaPrivate { hash, key, .. } => {
            let pub_k = key.to_public_key();
            populate_rsa_alg_obj(vm, obj, *hash, &pub_k);
        }
        KeyMaterial::RsaPublic { hash, key, .. } => {
            populate_rsa_alg_obj(vm, obj, *hash, key);
        }
        KeyMaterial::RawKdf { .. }
        | KeyMaterial::Ed25519Private(_)
        | KeyMaterial::Ed25519Public(_) => {}
    }
    obj
}

fn populate_rsa_alg_obj(vm: &mut JsVm, obj: HeapHandle, hash: HashAlg, pub_k: &RsaPublicKey) {
    let hobj = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(vm.object_proto),
    ));
    let hv = str_val(vm, hash.as_str());
    def_data(vm, hobj, "name", hv, PropertyAttrs::DEFAULT);
    def_data(
        vm,
        obj,
        "hash",
        JsValue::from_object(hobj),
        PropertyAttrs::DEFAULT,
    );
    def_data(
        vm,
        obj,
        "modulusLength",
        JsValue::from_number((pub_k.size() * 8) as f64),
        PropertyAttrs::DEFAULT,
    );
    let exp_bytes = trim_leading_zeros(&pub_k.e().to_be_bytes());
    let ab = alloc_array_buffer(vm, exp_bytes.clone());
    let u8_arr = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::TypedArray(crate::heap::TypedArrayObject {
            kind: TypedArrayKind::Uint8,
            buffer: ab,
            byte_offset: 0,
            length: exp_bytes.len() as u32,
        }),
        Some(vm.object_proto),
    ));
    def_data(
        vm,
        obj,
        "publicExponent",
        JsValue::from_object(u8_arr),
        PropertyAttrs::DEFAULT,
    );
}

fn trim_leading_zeros(bytes: &[u8]) -> Vec<u8> {
    let first = bytes
        .iter()
        .position(|&b| b != 0)
        .unwrap_or(bytes.len().saturating_sub(1));
    bytes[first..].to_vec()
}

fn left_pad(bytes: &[u8], len: usize) -> Vec<u8> {
    if bytes.len() >= len {
        return bytes[bytes.len() - len..].to_vec();
    }
    let mut out = vec![0u8; len - bytes.len()];
    out.extend_from_slice(bytes);
    out
}

// ---------------------------------------------------------------------------
// Cryptographic primitives (AES-GCM, AES-CBC, AES-CTR, HMAC, HKDF, PBKDF2,
// Ed25519, ECDSA, ECDH, RSA-OAEP, RSA-PSS, RSASSA-PKCS1-v1_5)
// ---------------------------------------------------------------------------

fn aes_gcm_encrypt(
    key: &[u8],
    iv: &[u8],
    aad: &[u8],
    plaintext: &[u8],
    tag_len_bits: usize,
) -> Result<Vec<u8>, CryptoErr> {
    if iv.len() != 12 {
        return Err(CryptoErr::new(
            "OperationError",
            "AES-GCM requires a 12-byte (96-bit) IV",
        ));
    }
    if !matches!(tag_len_bits, 96 | 104 | 112 | 120 | 128) {
        return Err(CryptoErr::new(
            "OperationError",
            "Invalid AES-GCM tagLength",
        ));
    }
    let nonce = aes_gcm::Nonce::try_from(iv)
        .map_err(|_| CryptoErr::new("OperationError", "Invalid AES-GCM nonce"))?;
    let payload = Payload {
        msg: plaintext,
        aad,
    };
    let mut ct = match key.len() {
        16 => Aes128Gcm::new_from_slice(key)
            .map_err(|_| CryptoErr::new("OperationError", "Invalid AES-128 key"))?
            .encrypt(&nonce, payload),
        24 => Aes192Gcm::new_from_slice(key)
            .map_err(|_| CryptoErr::new("OperationError", "Invalid AES-192 key"))?
            .encrypt(&nonce, payload),
        32 => Aes256Gcm::new_from_slice(key)
            .map_err(|_| CryptoErr::new("OperationError", "Invalid AES-256 key"))?
            .encrypt(&nonce, payload),
        _ => {
            return Err(CryptoErr::new(
                "DataError",
                "AES key must be 128, 192, or 256 bits",
            ))
        }
    }
    .map_err(|_| CryptoErr::new("OperationError", "AES-GCM encryption failed"))?;

    let tag_bytes = tag_len_bits / 8;
    if tag_bytes < 16 {
        let drop_bytes = 16 - tag_bytes;
        ct.truncate(ct.len().saturating_sub(drop_bytes));
    }
    Ok(ct)
}

fn aes_gcm_decrypt(
    key: &[u8],
    iv: &[u8],
    aad: &[u8],
    ciphertext: &[u8],
    tag_len_bits: usize,
) -> Result<Vec<u8>, CryptoErr> {
    if iv.len() != 12 {
        return Err(CryptoErr::new(
            "OperationError",
            "AES-GCM requires a 12-byte (96-bit) IV",
        ));
    }
    if tag_len_bits != 128 {
        return Err(CryptoErr::new(
            "OperationError",
            "AES-GCM decryption requires 128-bit authentication tag",
        ));
    }
    if ciphertext.len() < 16 {
        return Err(CryptoErr::new(
            "OperationError",
            "AES-GCM ciphertext too short",
        ));
    }
    let nonce = aes_gcm::Nonce::try_from(iv)
        .map_err(|_| CryptoErr::new("OperationError", "Invalid AES-GCM nonce"))?;
    let payload = Payload {
        msg: ciphertext,
        aad,
    };
    match key.len() {
        16 => Aes128Gcm::new_from_slice(key)
            .map_err(|_| CryptoErr::new("OperationError", "Invalid AES-128 key"))?
            .decrypt(&nonce, payload),
        24 => Aes192Gcm::new_from_slice(key)
            .map_err(|_| CryptoErr::new("OperationError", "Invalid AES-192 key"))?
            .decrypt(&nonce, payload),
        32 => Aes256Gcm::new_from_slice(key)
            .map_err(|_| CryptoErr::new("OperationError", "Invalid AES-256 key"))?
            .decrypt(&nonce, payload),
        _ => {
            return Err(CryptoErr::new(
                "DataError",
                "AES key must be 128, 192, or 256 bits",
            ))
        }
    }
    .map_err(|_| CryptoErr::new("OperationError", "AES-GCM authentication failed"))
}

fn aes_cbc_encrypt(key: &[u8], iv: &[u8], plaintext: &[u8]) -> Result<Vec<u8>, CryptoErr> {
    if iv.len() != 16 {
        return Err(CryptoErr::new(
            "OperationError",
            "AES-CBC requires a 16-byte IV",
        ));
    }
    let res = match key.len() {
        16 => cbc::Encryptor::<Aes128>::new_from_slices(key, iv)
            .map_err(|_| CryptoErr::new("OperationError", "Invalid AES-128-CBC key/iv"))?
            .encrypt_padded_vec::<Pkcs7>(plaintext),
        24 => cbc::Encryptor::<Aes192>::new_from_slices(key, iv)
            .map_err(|_| CryptoErr::new("OperationError", "Invalid AES-192-CBC key/iv"))?
            .encrypt_padded_vec::<Pkcs7>(plaintext),
        32 => cbc::Encryptor::<Aes256>::new_from_slices(key, iv)
            .map_err(|_| CryptoErr::new("OperationError", "Invalid AES-256-CBC key/iv"))?
            .encrypt_padded_vec::<Pkcs7>(plaintext),
        _ => {
            return Err(CryptoErr::new(
                "DataError",
                "AES key must be 128, 192, or 256 bits",
            ))
        }
    };
    Ok(res)
}

fn aes_cbc_decrypt(key: &[u8], iv: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>, CryptoErr> {
    if iv.len() != 16 {
        return Err(CryptoErr::new(
            "OperationError",
            "AES-CBC requires a 16-byte IV",
        ));
    }
    if ciphertext.is_empty() || !ciphertext.len().is_multiple_of(16) {
        return Err(CryptoErr::new(
            "OperationError",
            "AES-CBC ciphertext must be a non-empty multiple of 16 bytes",
        ));
    }
    let res = match key.len() {
        16 => cbc::Decryptor::<Aes128>::new_from_slices(key, iv)
            .map_err(|_| CryptoErr::new("OperationError", "Invalid AES-128-CBC key/iv"))?
            .decrypt_padded_vec::<Pkcs7>(ciphertext),
        24 => cbc::Decryptor::<Aes192>::new_from_slices(key, iv)
            .map_err(|_| CryptoErr::new("OperationError", "Invalid AES-192-CBC key/iv"))?
            .decrypt_padded_vec::<Pkcs7>(ciphertext),
        32 => cbc::Decryptor::<Aes256>::new_from_slices(key, iv)
            .map_err(|_| CryptoErr::new("OperationError", "Invalid AES-256-CBC key/iv"))?
            .decrypt_padded_vec::<Pkcs7>(ciphertext),
        _ => {
            return Err(CryptoErr::new(
                "DataError",
                "AES key must be 128, 192, or 256 bits",
            ))
        }
    };
    res.map_err(|_| CryptoErr::new("OperationError", "AES-CBC decryption/unpadding failed"))
}

/// W3C WebCrypto AES-CTR supporting exact right-aligned `counter_bits` (1..=128) wrapping.
fn aes_ctr_xor(
    key: &[u8],
    counter_init: &[u8],
    counter_bits: u32,
    input: &[u8],
) -> Result<Vec<u8>, CryptoErr> {
    use aes::cipher::BlockCipherEncrypt;
    if counter_init.len() != 16 {
        return Err(CryptoErr::new(
            "OperationError",
            "AES-CTR counter must be 16 bytes",
        ));
    }
    if counter_bits == 0 || counter_bits > 128 {
        return Err(CryptoErr::new(
            "OperationError",
            "AES-CTR length must be between 1 and 128 bits",
        ));
    }
    let mut counter = u128::from_be_bytes(
        counter_init
            .try_into()
            .map_err(|_| CryptoErr::new("OperationError", "Invalid counter"))?,
    );
    let mask = if counter_bits == 128 {
        u128::MAX
    } else {
        (1u128 << counter_bits) - 1
    };
    let prefix = counter & !mask;

    let mut out = input.to_vec();
    for chunk in out.chunks_mut(16) {
        let mut block = aes::Block::from(counter.to_be_bytes());
        match key.len() {
            16 => Aes128::new_from_slice(key)
                .map_err(|_| CryptoErr::new("OperationError", "Invalid AES-128 key"))?
                .encrypt_block(&mut block),
            24 => Aes192::new_from_slice(key)
                .map_err(|_| CryptoErr::new("OperationError", "Invalid AES-192 key"))?
                .encrypt_block(&mut block),
            32 => Aes256::new_from_slice(key)
                .map_err(|_| CryptoErr::new("OperationError", "Invalid AES-256 key"))?
                .encrypt_block(&mut block),
            _ => {
                return Err(CryptoErr::new(
                    "DataError",
                    "AES key must be 128, 192, or 256 bits",
                ))
            }
        }
        for (o, &k) in chunk.iter_mut().zip(block.iter()) {
            *o ^= k;
        }
        let next_low = (counter & mask).wrapping_add(1) & mask;
        counter = prefix | next_low;
    }
    Ok(out)
}

fn hmac_sign(hash: HashAlg, key: &[u8], data: &[u8]) -> Result<Vec<u8>, CryptoErr> {
    macro_rules! do_mac {
        ($H:ty) => {{
            let mut mac = <Hmac<$H> as KeyInit>::new_from_slice(key)
                .map_err(|_| CryptoErr::new("OperationError", "HMAC init failed"))?;
            mac.update(data);
            Ok(mac.finalize().into_bytes().to_vec())
        }};
    }
    match hash {
        HashAlg::Sha1 => do_mac!(Sha1),
        HashAlg::Sha256 => do_mac!(Sha256),
        HashAlg::Sha384 => do_mac!(Sha384),
        HashAlg::Sha512 => do_mac!(Sha512),
    }
}

fn hmac_verify(hash: HashAlg, key: &[u8], sig: &[u8], data: &[u8]) -> Result<bool, CryptoErr> {
    macro_rules! do_verify {
        ($H:ty) => {{
            let mut mac = <Hmac<$H> as KeyInit>::new_from_slice(key)
                .map_err(|_| CryptoErr::new("OperationError", "HMAC init failed"))?;
            mac.update(data);
            Ok(mac.verify_slice(sig).is_ok())
        }};
    }
    match hash {
        HashAlg::Sha1 => do_verify!(Sha1),
        HashAlg::Sha256 => do_verify!(Sha256),
        HashAlg::Sha384 => do_verify!(Sha384),
        HashAlg::Sha512 => do_verify!(Sha512),
    }
}

fn hkdf_derive(
    hash: HashAlg,
    ikm: &[u8],
    salt: &[u8],
    info: &[u8],
    out_bytes: usize,
) -> Result<Vec<u8>, CryptoErr> {
    let mut okm = vec![0u8; out_bytes];
    macro_rules! do_hkdf {
        ($H:ty) => {{
            let hk = Hkdf::<$H>::new(Some(salt), ikm);
            hk.expand(info, &mut okm)
                .map_err(|_| CryptoErr::new("OperationError", "HKDF output length exceeded"))?;
        }};
    }
    match hash {
        HashAlg::Sha1 => do_hkdf!(Sha1),
        HashAlg::Sha256 => do_hkdf!(Sha256),
        HashAlg::Sha384 => do_hkdf!(Sha384),
        HashAlg::Sha512 => do_hkdf!(Sha512),
    }
    Ok(okm)
}

fn pbkdf2_derive(
    hash: HashAlg,
    password: &[u8],
    salt: &[u8],
    iterations: u32,
    out_bytes: usize,
) -> Result<Vec<u8>, CryptoErr> {
    if iterations == 0 {
        return Err(CryptoErr::new(
            "OperationError",
            "PBKDF2 iterations must be > 0",
        ));
    }
    let mut out = vec![0u8; out_bytes];
    match hash {
        HashAlg::Sha1 => pbkdf2_hmac::<Sha1>(password, salt, iterations, &mut out),
        HashAlg::Sha256 => pbkdf2_hmac::<Sha256>(password, salt, iterations, &mut out),
        HashAlg::Sha384 => pbkdf2_hmac::<Sha384>(password, salt, iterations, &mut out),
        HashAlg::Sha512 => pbkdf2_hmac::<Sha512>(password, salt, iterations, &mut out),
    }
    Ok(out)
}

fn rsa_oaep_encrypt(
    key: &RsaPublicKey,
    hash: HashAlg,
    label: Option<&[u8]>,
    data: &[u8],
) -> Result<Vec<u8>, CryptoErr> {
    let mut rng = SysRng;
    macro_rules! do_enc {
        ($H:ty) => {{
            let enc = match label {
                Some(l) if !l.is_empty() => OaepEncryptor::<$H>::new_with_label(key.clone(), l),
                _ => OaepEncryptor::<$H>::new(key.clone()),
            };
            enc.encrypt_with_rng(&mut rng, data)
                .map_err(|e| CryptoErr::new("OperationError", format!("RSA-OAEP encrypt: {e}")))
        }};
    }
    match hash {
        HashAlg::Sha1 => do_enc!(Sha1),
        HashAlg::Sha256 => do_enc!(Sha256),
        HashAlg::Sha384 => do_enc!(Sha384),
        HashAlg::Sha512 => do_enc!(Sha512),
    }
}

fn rsa_oaep_decrypt(
    key: &RsaPrivateKey,
    hash: HashAlg,
    label: Option<&[u8]>,
    data: &[u8],
) -> Result<Vec<u8>, CryptoErr> {
    macro_rules! do_dec {
        ($H:ty) => {{
            let dec = match label {
                Some(l) if !l.is_empty() => OaepDecryptor::<$H>::new_with_label(key.clone(), l),
                _ => OaepDecryptor::<$H>::new(key.clone()),
            };
            dec.decrypt(data)
                .map_err(|e| CryptoErr::new("OperationError", format!("RSA-OAEP decrypt: {e}")))
        }};
    }
    match hash {
        HashAlg::Sha1 => do_dec!(Sha1),
        HashAlg::Sha256 => do_dec!(Sha256),
        HashAlg::Sha384 => do_dec!(Sha384),
        HashAlg::Sha512 => do_dec!(Sha512),
    }
}

fn rsa_pss_sign(
    key: &RsaPrivateKey,
    hash: HashAlg,
    salt_len: usize,
    data: &[u8],
) -> Result<Vec<u8>, CryptoErr> {
    let mut rng = getrandom::rand_core::UnwrapErr(SysRng);
    macro_rules! do_sign {
        ($H:ty) => {{
            let signer = PssSigner::<$H>::new_with_salt_len(key.clone(), salt_len);
            let sig = signer.sign_with_rng(&mut rng, data);
            Ok(sig.to_vec())
        }};
    }
    match hash {
        HashAlg::Sha1 => do_sign!(Sha1),
        HashAlg::Sha256 => do_sign!(Sha256),
        HashAlg::Sha384 => do_sign!(Sha384),
        HashAlg::Sha512 => do_sign!(Sha512),
    }
}

fn rsa_pss_verify(
    key: &RsaPublicKey,
    hash: HashAlg,
    salt_len: usize,
    sig_bytes: &[u8],
    data: &[u8],
) -> Result<bool, CryptoErr> {
    use rsa::signature::Verifier;
    let Ok(sig) = rsa::pss::Signature::try_from(sig_bytes) else {
        return Ok(false);
    };
    macro_rules! do_ver {
        ($H:ty) => {{
            let verifier = PssVerifier::<$H>::new_with_salt_len(key.clone(), salt_len);
            Ok(verifier.verify(data, &sig).is_ok())
        }};
    }
    match hash {
        HashAlg::Sha1 => do_ver!(Sha1),
        HashAlg::Sha256 => do_ver!(Sha256),
        HashAlg::Sha384 => do_ver!(Sha384),
        HashAlg::Sha512 => do_ver!(Sha512),
    }
}

// ---------------------------------------------------------------------------
// SubtleCrypto high-level operations
// ---------------------------------------------------------------------------

fn op_digest(vm: &mut JsVm, args: &[JsValue]) -> Result<JsValue, CryptoErr> {
    let name = parse_algorithm_name(vm, arg(args, 0))?;
    let hash = HashAlg::parse(&name).ok_or_else(|| {
        CryptoErr::new("NotSupportedError", format!("Unsupported digest: {name}"))
    })?;
    let data = read_buffer_source(vm, arg(args, 1))?;
    let out = hash.digest_bytes(&data);
    Ok(JsValue::from_object(alloc_array_buffer(vm, out)))
}

fn op_generate_key(
    vm: &mut JsVm,
    state: &SharedCrypto,
    args: &[JsValue],
) -> Result<JsValue, CryptoErr> {
    let alg_val = arg(args, 0);
    let alg_name = parse_algorithm_name(vm, alg_val)?;
    let extractable = vm.to_boolean(arg(args, 1));
    let usages = parse_usages(vm, arg(args, 2))?;

    match alg_name.to_ascii_uppercase().as_str() {
        "AES-GCM" | "AES-CBC" | "AES-CTR" => {
            let canonical = alg_name.to_ascii_uppercase();
            let len_bits = get_prop(vm, alg_val, "length")?.as_number().unwrap_or(0.0) as usize;
            if !matches!(len_bits, 128 | 192 | 256) {
                return Err(CryptoErr::new(
                    "OperationError",
                    "AES key length must be 128, 192, or 256",
                ));
            }
            let mut bytes = vec![0u8; len_bits / 8];
            fill_random(&mut bytes)?;
            let kh = alloc_crypto_key(
                vm,
                state,
                CryptoKeyRecord {
                    key_type: "secret",
                    extractable,
                    usages,
                    material: KeyMaterial::Aes {
                        alg: canonical,
                        bytes,
                    },
                },
            );
            Ok(JsValue::from_object(kh))
        }
        "HMAC" => {
            let hash = parse_hash_from_alg(vm, alg_val)?;
            let len_v = get_prop(vm, alg_val, "length")?;
            let byte_len = if len_v.is_undefined() {
                hash.block_size_bytes()
            } else {
                let bits = len_v.as_number().unwrap_or(0.0) as usize;
                if bits == 0 || !bits.is_multiple_of(8) {
                    return Err(CryptoErr::new("OperationError", "Invalid HMAC key length"));
                }
                bits / 8
            };
            let mut bytes = vec![0u8; byte_len];
            fill_random(&mut bytes)?;
            let kh = alloc_crypto_key(
                vm,
                state,
                CryptoKeyRecord {
                    key_type: "secret",
                    extractable,
                    usages,
                    material: KeyMaterial::Hmac { hash, bytes },
                },
            );
            Ok(JsValue::from_object(kh))
        }
        "ED25519" => {
            let mut seed = [0u8; 32];
            fill_random(&mut seed)?;
            let sk = EdSigningKey::from_bytes(&seed);
            let vk = sk.verifying_key();
            let pub_usages: Vec<String> = usages
                .iter()
                .filter(|u| u.as_str() == "verify")
                .cloned()
                .collect();
            let priv_usages: Vec<String> = usages
                .iter()
                .filter(|u| u.as_str() == "sign")
                .cloned()
                .collect();
            let pub_h = alloc_crypto_key(
                vm,
                state,
                CryptoKeyRecord {
                    key_type: "public",
                    extractable: true,
                    usages: pub_usages,
                    material: KeyMaterial::Ed25519Public(vk),
                },
            );
            let priv_h = alloc_crypto_key(
                vm,
                state,
                CryptoKeyRecord {
                    key_type: "private",
                    extractable,
                    usages: priv_usages,
                    material: KeyMaterial::Ed25519Private(sk),
                },
            );
            Ok(alloc_key_pair(vm, pub_h, priv_h))
        }
        "ECDSA" | "ECDH" => {
            let canonical = alg_name.to_ascii_uppercase();
            let curve_v = get_prop(vm, alg_val, "namedCurve")?;
            let curve_s = vm.value_to_rust_string(curve_v);
            let curve = NamedCurve::parse(&curve_s).ok_or_else(|| {
                CryptoErr::new("NotSupportedError", format!("Unsupported curve: {curve_s}"))
            })?;
            let (pub_allowed, priv_allowed): (&[&str], &[&str]) = if canonical == "ECDSA" {
                (&["verify"], &["sign"])
            } else {
                (&[], &["deriveBits", "deriveKey"])
            };
            let pub_usages: Vec<String> = usages
                .iter()
                .filter(|u| pub_allowed.contains(&u.as_str()))
                .cloned()
                .collect();
            let priv_usages: Vec<String> = usages
                .iter()
                .filter(|u| priv_allowed.contains(&u.as_str()))
                .cloned()
                .collect();
            let (pub_mat, priv_mat) = match curve {
                NamedCurve::P256 => {
                    let sk = p256::SecretKey::generate();
                    let pk = sk.public_key();
                    (
                        KeyMaterial::EcP256Public {
                            alg: canonical.clone(),
                            key: pk,
                        },
                        KeyMaterial::EcP256Private {
                            alg: canonical,
                            key: sk,
                        },
                    )
                }
                NamedCurve::P384 => {
                    let sk = p384::SecretKey::generate();
                    let pk = sk.public_key();
                    (
                        KeyMaterial::EcP384Public {
                            alg: canonical.clone(),
                            key: pk,
                        },
                        KeyMaterial::EcP384Private {
                            alg: canonical,
                            key: sk,
                        },
                    )
                }
            };
            let pub_h = alloc_crypto_key(
                vm,
                state,
                CryptoKeyRecord {
                    key_type: "public",
                    extractable: true,
                    usages: pub_usages,
                    material: pub_mat,
                },
            );
            let priv_h = alloc_crypto_key(
                vm,
                state,
                CryptoKeyRecord {
                    key_type: "private",
                    extractable,
                    usages: priv_usages,
                    material: priv_mat,
                },
            );
            Ok(alloc_key_pair(vm, pub_h, priv_h))
        }
        "RSA-OAEP" | "RSA-PSS" | "RSASSA-PKCS1-V1_5" => {
            let canonical = match alg_name.to_ascii_uppercase().as_str() {
                "RSA-OAEP" => "RSA-OAEP".to_string(),
                "RSA-PSS" => "RSA-PSS".to_string(),
                _ => "RSASSA-PKCS1-v1_5".to_string(),
            };
            let bits = get_prop(vm, alg_val, "modulusLength")?
                .as_number()
                .unwrap_or(2048.0) as usize;
            if !(1024..=4096).contains(&bits) || !bits.is_multiple_of(8) {
                return Err(CryptoErr::new(
                    "OperationError",
                    "RSA modulusLength must be between 1024 and 4096 bits",
                ));
            }
            let hash = parse_hash_from_alg(vm, alg_val)?;
            let mut rng = SysRng;
            let sk = RsaPrivateKey::new(&mut rng, bits)
                .map_err(|e| CryptoErr::new("OperationError", format!("RSA keygen failed: {e}")))?;
            let pk = sk.to_public_key();
            let (pub_allowed, priv_allowed): (&[&str], &[&str]) = if canonical == "RSA-OAEP" {
                (&["encrypt", "wrapKey"], &["decrypt", "unwrapKey"])
            } else {
                (&["verify"], &["sign"])
            };
            let pub_usages: Vec<String> = usages
                .iter()
                .filter(|u| pub_allowed.contains(&u.as_str()))
                .cloned()
                .collect();
            let priv_usages: Vec<String> = usages
                .iter()
                .filter(|u| priv_allowed.contains(&u.as_str()))
                .cloned()
                .collect();
            let pub_h = alloc_crypto_key(
                vm,
                state,
                CryptoKeyRecord {
                    key_type: "public",
                    extractable: true,
                    usages: pub_usages,
                    material: KeyMaterial::RsaPublic {
                        alg: canonical.clone(),
                        hash,
                        key: pk,
                    },
                },
            );
            let priv_h = alloc_crypto_key(
                vm,
                state,
                CryptoKeyRecord {
                    key_type: "private",
                    extractable,
                    usages: priv_usages,
                    material: KeyMaterial::RsaPrivate {
                        alg: canonical,
                        hash,
                        key: sk,
                    },
                },
            );
            Ok(alloc_key_pair(vm, pub_h, priv_h))
        }
        other => Err(CryptoErr::new(
            "NotSupportedError",
            format!("Unsupported generateKey algorithm: {other}"),
        )),
    }
}

fn alloc_key_pair(vm: &mut JsVm, pub_h: HeapHandle, priv_h: HeapHandle) -> JsValue {
    let pair = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(vm.object_proto),
    ));
    def_data(
        vm,
        pair,
        "publicKey",
        JsValue::from_object(pub_h),
        PropertyAttrs::DEFAULT,
    );
    def_data(
        vm,
        pair,
        "privateKey",
        JsValue::from_object(priv_h),
        PropertyAttrs::DEFAULT,
    );
    JsValue::from_object(pair)
}

fn op_import_key(
    vm: &mut JsVm,
    state: &SharedCrypto,
    args: &[JsValue],
) -> Result<JsValue, CryptoErr> {
    let format = vm.value_to_rust_string(arg(args, 0));
    let key_data = arg(args, 1);
    let alg_val = arg(args, 2);
    let extractable = vm.to_boolean(arg(args, 3));
    let usages = parse_usages(vm, arg(args, 4))?;
    let rec = import_key_record(vm, &format, key_data, alg_val, extractable, usages)?;
    let kh = alloc_crypto_key(vm, state, rec);
    Ok(JsValue::from_object(kh))
}

fn import_key_record(
    vm: &mut JsVm,
    format: &str,
    key_data: JsValue,
    alg_val: JsValue,
    extractable: bool,
    usages: Vec<String>,
) -> Result<CryptoKeyRecord, CryptoErr> {
    let alg_name = parse_algorithm_name(vm, alg_val)?;
    let upper = alg_name.to_ascii_uppercase();

    match format {
        "raw" => {
            let bytes = read_buffer_source(vm, key_data)?;
            match upper.as_str() {
                "AES-GCM" | "AES-CBC" | "AES-CTR" => {
                    if !matches!(bytes.len(), 16 | 24 | 32) {
                        return Err(CryptoErr::new(
                            "DataError",
                            "AES key length must be 128, 192, or 256 bits",
                        ));
                    }
                    Ok(CryptoKeyRecord {
                        key_type: "secret",
                        extractable,
                        usages,
                        material: KeyMaterial::Aes { alg: upper, bytes },
                    })
                }
                "HMAC" => {
                    if bytes.is_empty() {
                        return Err(CryptoErr::new("DataError", "HMAC key must be non-empty"));
                    }
                    let hash = parse_hash_from_alg(vm, alg_val)?;
                    Ok(CryptoKeyRecord {
                        key_type: "secret",
                        extractable,
                        usages,
                        material: KeyMaterial::Hmac { hash, bytes },
                    })
                }
                "HKDF" | "PBKDF2" => {
                    if extractable {
                        return Err(CryptoErr::new(
                            "SyntaxError",
                            "HKDF/PBKDF2 keys cannot be extractable",
                        ));
                    }
                    Ok(CryptoKeyRecord {
                        key_type: "secret",
                        extractable: false,
                        usages,
                        material: KeyMaterial::RawKdf { alg: upper, bytes },
                    })
                }
                "ED25519" => {
                    let arr: [u8; 32] = bytes.try_into().map_err(|_| {
                        CryptoErr::new("DataError", "Ed25519 raw public key must be 32 bytes")
                    })?;
                    let vk = EdVerifyingKey::from_bytes(&arr)
                        .map_err(|_| CryptoErr::new("DataError", "Invalid Ed25519 public key"))?;
                    Ok(CryptoKeyRecord {
                        key_type: "public",
                        extractable,
                        usages,
                        material: KeyMaterial::Ed25519Public(vk),
                    })
                }
                "ECDSA" | "ECDH" => {
                    let curve_s = get_prop_str(vm, alg_val, "namedCurve")?;
                    let curve = NamedCurve::parse(&curve_s).ok_or_else(|| {
                        CryptoErr::new("NotSupportedError", format!("Unsupported curve: {curve_s}"))
                    })?;
                    let mat = match curve {
                        NamedCurve::P256 => KeyMaterial::EcP256Public {
                            alg: upper,
                            key: p256::PublicKey::from_sec1_bytes(&bytes).map_err(|_| {
                                CryptoErr::new("DataError", "Invalid P-256 public key")
                            })?,
                        },
                        NamedCurve::P384 => KeyMaterial::EcP384Public {
                            alg: upper,
                            key: p384::PublicKey::from_sec1_bytes(&bytes).map_err(|_| {
                                CryptoErr::new("DataError", "Invalid P-384 public key")
                            })?,
                        },
                    };
                    Ok(CryptoKeyRecord {
                        key_type: "public",
                        extractable,
                        usages,
                        material: mat,
                    })
                }
                _ => Err(CryptoErr::new(
                    "NotSupportedError",
                    format!("Unsupported raw import for {alg_name}"),
                )),
            }
        }
        "pkcs8" => {
            let der = read_buffer_source(vm, key_data)?;
            match upper.as_str() {
                "ED25519" => {
                    let sk = EdSigningKey::from_pkcs8_der(&der)
                        .map_err(|_| CryptoErr::new("DataError", "Invalid Ed25519 PKCS#8 key"))?;
                    Ok(CryptoKeyRecord {
                        key_type: "private",
                        extractable,
                        usages,
                        material: KeyMaterial::Ed25519Private(sk),
                    })
                }
                "ECDSA" | "ECDH" => {
                    let curve_s = get_prop_str(vm, alg_val, "namedCurve")?;
                    let curve = NamedCurve::parse(&curve_s).ok_or_else(|| {
                        CryptoErr::new("NotSupportedError", format!("Unsupported curve: {curve_s}"))
                    })?;
                    let mat = match curve {
                        NamedCurve::P256 => KeyMaterial::EcP256Private {
                            alg: upper,
                            key: p256::SecretKey::from_pkcs8_der(&der).map_err(|_| {
                                CryptoErr::new("DataError", "Invalid P-256 PKCS#8 key")
                            })?,
                        },
                        NamedCurve::P384 => KeyMaterial::EcP384Private {
                            alg: upper,
                            key: p384::SecretKey::from_pkcs8_der(&der).map_err(|_| {
                                CryptoErr::new("DataError", "Invalid P-384 PKCS#8 key")
                            })?,
                        },
                    };
                    Ok(CryptoKeyRecord {
                        key_type: "private",
                        extractable,
                        usages,
                        material: mat,
                    })
                }
                "RSA-OAEP" | "RSA-PSS" | "RSASSA-PKCS1-V1_5" => {
                    let canonical = if upper == "RSASSA-PKCS1-V1_5" {
                        "RSASSA-PKCS1-v1_5".to_string()
                    } else {
                        upper
                    };
                    let hash = parse_hash_from_alg(vm, alg_val)?;
                    let sk = RsaPrivateKey::from_pkcs8_der(&der)
                        .map_err(|_| CryptoErr::new("DataError", "Invalid RSA PKCS#8 key"))?;
                    Ok(CryptoKeyRecord {
                        key_type: "private",
                        extractable,
                        usages,
                        material: KeyMaterial::RsaPrivate {
                            alg: canonical,
                            hash,
                            key: sk,
                        },
                    })
                }
                _ => Err(CryptoErr::new(
                    "NotSupportedError",
                    format!("Unsupported pkcs8 import for {alg_name}"),
                )),
            }
        }
        "spki" => {
            let der = read_buffer_source(vm, key_data)?;
            match upper.as_str() {
                "ED25519" => {
                    let vk = EdVerifyingKey::from_public_key_der(&der)
                        .map_err(|_| CryptoErr::new("DataError", "Invalid Ed25519 SPKI key"))?;
                    Ok(CryptoKeyRecord {
                        key_type: "public",
                        extractable,
                        usages,
                        material: KeyMaterial::Ed25519Public(vk),
                    })
                }
                "ECDSA" | "ECDH" => {
                    let curve_s = get_prop_str(vm, alg_val, "namedCurve")?;
                    let curve = NamedCurve::parse(&curve_s).ok_or_else(|| {
                        CryptoErr::new("NotSupportedError", format!("Unsupported curve: {curve_s}"))
                    })?;
                    let mat = match curve {
                        NamedCurve::P256 => KeyMaterial::EcP256Public {
                            alg: upper,
                            key: p256::PublicKey::from_public_key_der(&der).map_err(|_| {
                                CryptoErr::new("DataError", "Invalid P-256 SPKI key")
                            })?,
                        },
                        NamedCurve::P384 => KeyMaterial::EcP384Public {
                            alg: upper,
                            key: p384::PublicKey::from_public_key_der(&der).map_err(|_| {
                                CryptoErr::new("DataError", "Invalid P-384 SPKI key")
                            })?,
                        },
                    };
                    Ok(CryptoKeyRecord {
                        key_type: "public",
                        extractable,
                        usages,
                        material: mat,
                    })
                }
                "RSA-OAEP" | "RSA-PSS" | "RSASSA-PKCS1-V1_5" => {
                    let canonical = if upper == "RSASSA-PKCS1-V1_5" {
                        "RSASSA-PKCS1-v1_5".to_string()
                    } else {
                        upper
                    };
                    let hash = parse_hash_from_alg(vm, alg_val)?;
                    let pk = RsaPublicKey::from_public_key_der(&der)
                        .map_err(|_| CryptoErr::new("DataError", "Invalid RSA SPKI key"))?;
                    Ok(CryptoKeyRecord {
                        key_type: "public",
                        extractable,
                        usages,
                        material: KeyMaterial::RsaPublic {
                            alg: canonical,
                            hash,
                            key: pk,
                        },
                    })
                }
                _ => Err(CryptoErr::new(
                    "NotSupportedError",
                    format!("Unsupported spki import for {alg_name}"),
                )),
            }
        }
        "jwk" => import_jwk(vm, key_data, &upper, alg_val, extractable, usages),
        other => Err(CryptoErr::new(
            "NotSupportedError",
            format!("Unsupported key format: {other}"),
        )),
    }
}

fn jwk_str(vm: &mut JsVm, jwk: JsValue, prop: &str) -> Result<Option<String>, CryptoErr> {
    let v = get_prop(vm, jwk, prop)?;
    if v.is_undefined() || v.is_null() {
        return Ok(None);
    }
    Ok(Some(vm.value_to_rust_string(v)))
}

fn import_jwk(
    vm: &mut JsVm,
    jwk: JsValue,
    upper_alg: &str,
    alg_val: JsValue,
    extractable: bool,
    usages: Vec<String>,
) -> Result<CryptoKeyRecord, CryptoErr> {
    let kty =
        jwk_str(vm, jwk, "kty")?.ok_or_else(|| CryptoErr::new("DataError", "JWK missing 'kty'"))?;
    match (upper_alg, kty.as_str()) {
        ("AES-GCM" | "AES-CBC" | "AES-CTR", "oct") => {
            let k = jwk_str(vm, jwk, "k")?
                .ok_or_else(|| CryptoErr::new("DataError", "JWK missing 'k'"))?;
            let bytes = base64url_decode(&k)?;
            if !matches!(bytes.len(), 16 | 24 | 32) {
                return Err(CryptoErr::new("DataError", "Invalid AES JWK key length"));
            }
            Ok(CryptoKeyRecord {
                key_type: "secret",
                extractable,
                usages,
                material: KeyMaterial::Aes {
                    alg: upper_alg.to_string(),
                    bytes,
                },
            })
        }
        ("HMAC", "oct") => {
            let k = jwk_str(vm, jwk, "k")?
                .ok_or_else(|| CryptoErr::new("DataError", "JWK missing 'k'"))?;
            let bytes = base64url_decode(&k)?;
            let hash = parse_hash_from_alg(vm, alg_val)?;
            Ok(CryptoKeyRecord {
                key_type: "secret",
                extractable,
                usages,
                material: KeyMaterial::Hmac { hash, bytes },
            })
        }
        ("ED25519", "OKP") => {
            if let Some(d_str) = jwk_str(vm, jwk, "d")? {
                let d: [u8; 32] = base64url_decode(&d_str)?
                    .try_into()
                    .map_err(|_| CryptoErr::new("DataError", "Invalid Ed25519 JWK 'd'"))?;
                Ok(CryptoKeyRecord {
                    key_type: "private",
                    extractable,
                    usages,
                    material: KeyMaterial::Ed25519Private(EdSigningKey::from_bytes(&d)),
                })
            } else {
                let x_str = jwk_str(vm, jwk, "x")?
                    .ok_or_else(|| CryptoErr::new("DataError", "Ed25519 JWK missing 'x'"))?;
                let x: [u8; 32] = base64url_decode(&x_str)?
                    .try_into()
                    .map_err(|_| CryptoErr::new("DataError", "Invalid Ed25519 JWK 'x'"))?;
                let vk = EdVerifyingKey::from_bytes(&x)
                    .map_err(|_| CryptoErr::new("DataError", "Invalid Ed25519 public key"))?;
                Ok(CryptoKeyRecord {
                    key_type: "public",
                    extractable,
                    usages,
                    material: KeyMaterial::Ed25519Public(vk),
                })
            }
        }
        ("ECDSA" | "ECDH", "EC") => {
            let crv = jwk_str(vm, jwk, "crv")?
                .ok_or_else(|| CryptoErr::new("DataError", "EC JWK missing 'crv'"))?;
            let curve = NamedCurve::parse(&crv).ok_or_else(|| {
                CryptoErr::new("NotSupportedError", format!("Unsupported curve: {crv}"))
            })?;
            if let Some(d_str) = jwk_str(vm, jwk, "d")? {
                let d_bytes = left_pad(&base64url_decode(&d_str)?, curve.coord_len());
                let mat = match curve {
                    NamedCurve::P256 => KeyMaterial::EcP256Private {
                        alg: upper_alg.to_string(),
                        key: p256::SecretKey::from_slice(&d_bytes)
                            .map_err(|_| CryptoErr::new("DataError", "Invalid P-256 JWK 'd'"))?,
                    },
                    NamedCurve::P384 => KeyMaterial::EcP384Private {
                        alg: upper_alg.to_string(),
                        key: p384::SecretKey::from_slice(&d_bytes)
                            .map_err(|_| CryptoErr::new("DataError", "Invalid P-384 JWK 'd'"))?,
                    },
                };
                Ok(CryptoKeyRecord {
                    key_type: "private",
                    extractable,
                    usages,
                    material: mat,
                })
            } else {
                let x = left_pad(
                    &base64url_decode(
                        &jwk_str(vm, jwk, "x")?
                            .ok_or_else(|| CryptoErr::new("DataError", "EC JWK missing 'x'"))?,
                    )?,
                    curve.coord_len(),
                );
                let y = left_pad(
                    &base64url_decode(
                        &jwk_str(vm, jwk, "y")?
                            .ok_or_else(|| CryptoErr::new("DataError", "EC JWK missing 'y'"))?,
                    )?,
                    curve.coord_len(),
                );
                let mut sec1 = Vec::with_capacity(1 + 2 * curve.coord_len());
                sec1.push(0x04);
                sec1.extend_from_slice(&x);
                sec1.extend_from_slice(&y);
                let mat = match curve {
                    NamedCurve::P256 => KeyMaterial::EcP256Public {
                        alg: upper_alg.to_string(),
                        key: p256::PublicKey::from_sec1_bytes(&sec1)
                            .map_err(|_| CryptoErr::new("DataError", "Invalid P-256 JWK point"))?,
                    },
                    NamedCurve::P384 => KeyMaterial::EcP384Public {
                        alg: upper_alg.to_string(),
                        key: p384::PublicKey::from_sec1_bytes(&sec1)
                            .map_err(|_| CryptoErr::new("DataError", "Invalid P-384 JWK point"))?,
                    },
                };
                Ok(CryptoKeyRecord {
                    key_type: "public",
                    extractable,
                    usages,
                    material: mat,
                })
            }
        }
        ("RSA-OAEP" | "RSA-PSS" | "RSASSA-PKCS1-V1_5", "RSA") => {
            let canonical = if upper_alg == "RSASSA-PKCS1-V1_5" {
                "RSASSA-PKCS1-v1_5".to_string()
            } else {
                upper_alg.to_string()
            };
            let hash = parse_hash_from_alg(vm, alg_val)?;
            let n_bytes = base64url_decode(
                &jwk_str(vm, jwk, "n")?
                    .ok_or_else(|| CryptoErr::new("DataError", "RSA JWK missing 'n'"))?,
            )?;
            let e_bytes = base64url_decode(
                &jwk_str(vm, jwk, "e")?
                    .ok_or_else(|| CryptoErr::new("DataError", "RSA JWK missing 'e'"))?,
            )?;
            let n = BoxedUint::from_be_slice(&n_bytes, (n_bytes.len() * 8) as u32)
                .map_err(|_| CryptoErr::new("DataError", "Invalid RSA JWK 'n'"))?;
            let e = BoxedUint::from_be_slice(&e_bytes, (e_bytes.len() * 8) as u32)
                .map_err(|_| CryptoErr::new("DataError", "Invalid RSA JWK 'e'"))?;
            if let (Some(d_s), Some(p_s), Some(q_s)) = (
                jwk_str(vm, jwk, "d")?,
                jwk_str(vm, jwk, "p")?,
                jwk_str(vm, jwk, "q")?,
            ) {
                let d_b = base64url_decode(&d_s)?;
                let p_b = base64url_decode(&p_s)?;
                let q_b = base64url_decode(&q_s)?;
                let d = BoxedUint::from_be_slice(&d_b, (d_b.len() * 8) as u32)
                    .map_err(|_| CryptoErr::new("DataError", "Invalid RSA JWK 'd'"))?;
                let p = BoxedUint::from_be_slice(&p_b, (p_b.len() * 8) as u32)
                    .map_err(|_| CryptoErr::new("DataError", "Invalid RSA JWK 'p'"))?;
                let q = BoxedUint::from_be_slice(&q_b, (q_b.len() * 8) as u32)
                    .map_err(|_| CryptoErr::new("DataError", "Invalid RSA JWK 'q'"))?;
                let sk = RsaPrivateKey::from_components(n, e, d, vec![p, q]).map_err(|_| {
                    CryptoErr::new("DataError", "Invalid RSA private key components")
                })?;
                Ok(CryptoKeyRecord {
                    key_type: "private",
                    extractable,
                    usages,
                    material: KeyMaterial::RsaPrivate {
                        alg: canonical,
                        hash,
                        key: sk,
                    },
                })
            } else {
                let pk = RsaPublicKey::new(n, e)
                    .map_err(|_| CryptoErr::new("DataError", "Invalid RSA public key"))?;
                Ok(CryptoKeyRecord {
                    key_type: "public",
                    extractable,
                    usages,
                    material: KeyMaterial::RsaPublic {
                        alg: canonical,
                        hash,
                        key: pk,
                    },
                })
            }
        }
        _ => Err(CryptoErr::new(
            "DataError",
            format!("Unsupported JWK kty '{kty}' for algorithm '{upper_alg}'"),
        )),
    }
}

fn op_export_key(
    vm: &mut JsVm,
    state: &SharedCrypto,
    args: &[JsValue],
) -> Result<JsValue, CryptoErr> {
    let format = vm.value_to_rust_string(arg(args, 0));
    let key = require_key(state, arg(args, 1), None)?;
    if !key.extractable {
        return Err(CryptoErr::new(
            "InvalidAccessError",
            "CryptoKey is not extractable",
        ));
    }
    export_key_record(vm, &format, &key)
}

fn export_key_record(
    vm: &mut JsVm,
    format: &str,
    key: &CryptoKeyRecord,
) -> Result<JsValue, CryptoErr> {
    match format {
        "raw" => {
            let bytes = match &key.material {
                KeyMaterial::Aes { bytes, .. } | KeyMaterial::Hmac { bytes, .. } => bytes.clone(),
                KeyMaterial::Ed25519Public(vk) => vk.to_bytes().to_vec(),
                KeyMaterial::EcP256Public { key: pk, .. } => {
                    p256::Sec1Point::from(*pk).as_bytes().to_vec()
                }
                KeyMaterial::EcP384Public { key: pk, .. } => {
                    p384::Sec1Point::from(*pk).as_bytes().to_vec()
                }
                _ => {
                    return Err(CryptoErr::new(
                        "InvalidAccessError",
                        "raw export is not supported for this key type",
                    ));
                }
            };
            Ok(JsValue::from_object(alloc_array_buffer(vm, bytes)))
        }
        "pkcs8" => {
            let der = match &key.material {
                KeyMaterial::Ed25519Private(sk) => sk
                    .to_pkcs8_der()
                    .map_err(|_| CryptoErr::new("OperationError", "PKCS#8 encode failed"))?
                    .as_bytes()
                    .to_vec(),
                KeyMaterial::EcP256Private { key: sk, .. } => sk
                    .to_pkcs8_der()
                    .map_err(|_| CryptoErr::new("OperationError", "PKCS#8 encode failed"))?
                    .as_bytes()
                    .to_vec(),
                KeyMaterial::EcP384Private { key: sk, .. } => sk
                    .to_pkcs8_der()
                    .map_err(|_| CryptoErr::new("OperationError", "PKCS#8 encode failed"))?
                    .as_bytes()
                    .to_vec(),
                KeyMaterial::RsaPrivate { key: sk, .. } => sk
                    .to_pkcs8_der()
                    .map_err(|_| CryptoErr::new("OperationError", "PKCS#8 encode failed"))?
                    .as_bytes()
                    .to_vec(),
                _ => {
                    return Err(CryptoErr::new(
                        "InvalidAccessError",
                        "pkcs8 export is only valid for private keys",
                    ));
                }
            };
            Ok(JsValue::from_object(alloc_array_buffer(vm, der)))
        }
        "spki" => {
            let der = match &key.material {
                KeyMaterial::Ed25519Public(vk) => vk
                    .to_public_key_der()
                    .map_err(|_| CryptoErr::new("OperationError", "SPKI encode failed"))?
                    .as_bytes()
                    .to_vec(),
                KeyMaterial::EcP256Public { key: pk, .. } => pk
                    .to_public_key_der()
                    .map_err(|_| CryptoErr::new("OperationError", "SPKI encode failed"))?
                    .as_bytes()
                    .to_vec(),
                KeyMaterial::EcP384Public { key: pk, .. } => pk
                    .to_public_key_der()
                    .map_err(|_| CryptoErr::new("OperationError", "SPKI encode failed"))?
                    .as_bytes()
                    .to_vec(),
                KeyMaterial::RsaPublic { key: pk, .. } => pk
                    .to_public_key_der()
                    .map_err(|_| CryptoErr::new("OperationError", "SPKI encode failed"))?
                    .as_bytes()
                    .to_vec(),
                _ => {
                    return Err(CryptoErr::new(
                        "InvalidAccessError",
                        "spki export is only valid for public keys",
                    ));
                }
            };
            Ok(JsValue::from_object(alloc_array_buffer(vm, der)))
        }
        "jwk" => export_jwk(vm, key),
        other => Err(CryptoErr::new(
            "NotSupportedError",
            format!("Unsupported export format: {other}"),
        )),
    }
}

fn export_jwk(vm: &mut JsVm, key: &CryptoKeyRecord) -> Result<JsValue, CryptoErr> {
    let obj = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(vm.object_proto),
    ));
    let set_s = |vm: &mut JsVm, k: &str, v: &str| {
        let sv = str_val(vm, v);
        def_data(vm, obj, k, sv, PropertyAttrs::DEFAULT);
    };
    def_data(
        vm,
        obj,
        "ext",
        JsValue::from_bool(key.extractable),
        PropertyAttrs::DEFAULT,
    );
    let ops: Vec<JsValue> = key.usages.iter().map(|u| str_val(vm, u)).collect();
    let ops_arr = alloc_array_of(vm, &ops);
    def_data(
        vm,
        obj,
        "key_ops",
        JsValue::from_object(ops_arr),
        PropertyAttrs::DEFAULT,
    );

    match &key.material {
        KeyMaterial::Aes { alg, bytes } => {
            set_s(vm, "kty", "oct");
            set_s(vm, "k", &base64url_encode(bytes));
            let jwk_alg = match (alg.as_str(), bytes.len() * 8) {
                ("AES-GCM", 128) => "A128GCM",
                ("AES-GCM", 192) => "A192GCM",
                ("AES-GCM", 256) => "A256GCM",
                ("AES-CBC", 128) => "A128CBC",
                ("AES-CBC", 192) => "A192CBC",
                ("AES-CBC", 256) => "A256CBC",
                ("AES-CTR", 128) => "A128CTR",
                ("AES-CTR", 192) => "A192CTR",
                _ => "A256CTR",
            };
            set_s(vm, "alg", jwk_alg);
        }
        KeyMaterial::Hmac { hash, bytes } => {
            set_s(vm, "kty", "oct");
            set_s(vm, "k", &base64url_encode(bytes));
            let jwk_alg = match hash {
                HashAlg::Sha1 => "HS1",
                HashAlg::Sha256 => "HS256",
                HashAlg::Sha384 => "HS384",
                HashAlg::Sha512 => "HS512",
            };
            set_s(vm, "alg", jwk_alg);
        }
        KeyMaterial::Ed25519Private(sk) => {
            set_s(vm, "kty", "OKP");
            set_s(vm, "crv", "Ed25519");
            set_s(vm, "x", &base64url_encode(&sk.verifying_key().to_bytes()));
            set_s(vm, "d", &base64url_encode(sk.as_bytes()));
        }
        KeyMaterial::Ed25519Public(vk) => {
            set_s(vm, "kty", "OKP");
            set_s(vm, "crv", "Ed25519");
            set_s(vm, "x", &base64url_encode(&vk.to_bytes()));
        }
        KeyMaterial::EcP256Private { key: sk, .. } => {
            let pt = p256::Sec1Point::from(sk.public_key());
            let raw = pt.as_bytes();
            set_s(vm, "kty", "EC");
            set_s(vm, "crv", NamedCurve::P256.as_str());
            set_s(vm, "x", &base64url_encode(&raw[1..33]));
            set_s(vm, "y", &base64url_encode(&raw[33..65]));
            set_s(vm, "d", &base64url_encode(&sk.to_bytes()));
        }
        KeyMaterial::EcP256Public { key: pk, .. } => {
            let pt = p256::Sec1Point::from(*pk);
            let raw = pt.as_bytes();
            set_s(vm, "kty", "EC");
            set_s(vm, "crv", NamedCurve::P256.as_str());
            set_s(vm, "x", &base64url_encode(&raw[1..33]));
            set_s(vm, "y", &base64url_encode(&raw[33..65]));
        }
        KeyMaterial::EcP384Private { key: sk, .. } => {
            let pt = p384::Sec1Point::from(sk.public_key());
            let raw = pt.as_bytes();
            set_s(vm, "kty", "EC");
            set_s(vm, "crv", NamedCurve::P384.as_str());
            set_s(vm, "x", &base64url_encode(&raw[1..49]));
            set_s(vm, "y", &base64url_encode(&raw[49..97]));
            set_s(vm, "d", &base64url_encode(&sk.to_bytes()));
        }
        KeyMaterial::EcP384Public { key: pk, .. } => {
            let pt = p384::Sec1Point::from(*pk);
            let raw = pt.as_bytes();
            set_s(vm, "kty", "EC");
            set_s(vm, "crv", NamedCurve::P384.as_str());
            set_s(vm, "x", &base64url_encode(&raw[1..49]));
            set_s(vm, "y", &base64url_encode(&raw[49..97]));
        }
        KeyMaterial::RsaPublic { key: pk, .. } => {
            set_s(vm, "kty", "RSA");
            set_s(
                vm,
                "n",
                &base64url_encode(&trim_leading_zeros(&pk.n().to_be_bytes())),
            );
            set_s(
                vm,
                "e",
                &base64url_encode(&trim_leading_zeros(&pk.e().to_be_bytes())),
            );
        }
        KeyMaterial::RsaPrivate { key: sk, .. } => {
            use rsa::traits::PrivateKeyParts;
            set_s(vm, "kty", "RSA");
            set_s(
                vm,
                "n",
                &base64url_encode(&trim_leading_zeros(&sk.n().to_be_bytes())),
            );
            set_s(
                vm,
                "e",
                &base64url_encode(&trim_leading_zeros(&sk.e().to_be_bytes())),
            );
            set_s(
                vm,
                "d",
                &base64url_encode(&trim_leading_zeros(&sk.d().to_be_bytes())),
            );
            let primes = sk.primes();
            if primes.len() >= 2 {
                set_s(
                    vm,
                    "p",
                    &base64url_encode(&trim_leading_zeros(&primes[0].to_be_bytes())),
                );
                set_s(
                    vm,
                    "q",
                    &base64url_encode(&trim_leading_zeros(&primes[1].to_be_bytes())),
                );
            }
        }
        KeyMaterial::RawKdf { .. } => {
            return Err(CryptoErr::new(
                "InvalidAccessError",
                "KDF keys cannot be exported as JWK",
            ));
        }
    }
    Ok(JsValue::from_object(obj))
}

fn encrypt_bytes(
    vm: &mut JsVm,
    alg_val: JsValue,
    key: &CryptoKeyRecord,
    data: &[u8],
) -> Result<Vec<u8>, CryptoErr> {
    let alg_name = parse_algorithm_name(vm, alg_val)?.to_ascii_uppercase();
    match (&key.material, alg_name.as_str()) {
        (KeyMaterial::Aes { alg, bytes }, "AES-GCM") if alg == "AES-GCM" => {
            let iv = get_prop_bytes(vm, alg_val, "iv")?;
            let aad_v = get_prop(vm, alg_val, "additionalData")?;
            let aad = if aad_v.is_undefined() {
                Vec::new()
            } else {
                read_buffer_source(vm, aad_v)?
            };
            let tag_v = get_prop(vm, alg_val, "tagLength")?;
            let tag_bits = if tag_v.is_undefined() {
                128
            } else {
                tag_v.as_number().unwrap_or(128.0) as usize
            };
            aes_gcm_encrypt(bytes, &iv, &aad, data, tag_bits)
        }
        (KeyMaterial::Aes { alg, bytes }, "AES-CBC") if alg == "AES-CBC" => {
            let iv = get_prop_bytes(vm, alg_val, "iv")?;
            aes_cbc_encrypt(bytes, &iv, data)
        }
        (KeyMaterial::Aes { alg, bytes }, "AES-CTR") if alg == "AES-CTR" => {
            let counter = get_prop_bytes(vm, alg_val, "counter")?;
            let length = get_prop(vm, alg_val, "length")?.as_number().unwrap_or(0.0) as u32;
            aes_ctr_xor(bytes, &counter, length, data)
        }
        (KeyMaterial::RsaPublic { alg, hash, key: pk }, "RSA-OAEP") if alg == "RSA-OAEP" => {
            let label_v = get_prop(vm, alg_val, "label")?;
            let label = if label_v.is_undefined() {
                None
            } else {
                Some(read_buffer_source(vm, label_v)?)
            };
            rsa_oaep_encrypt(pk, *hash, label.as_deref(), data)
        }
        _ => Err(CryptoErr::new(
            "InvalidAccessError",
            format!("Key algorithm does not match encrypt algorithm '{alg_name}'"),
        )),
    }
}

fn decrypt_bytes(
    vm: &mut JsVm,
    alg_val: JsValue,
    key: &CryptoKeyRecord,
    data: &[u8],
) -> Result<Vec<u8>, CryptoErr> {
    let alg_name = parse_algorithm_name(vm, alg_val)?.to_ascii_uppercase();
    match (&key.material, alg_name.as_str()) {
        (KeyMaterial::Aes { alg, bytes }, "AES-GCM") if alg == "AES-GCM" => {
            let iv = get_prop_bytes(vm, alg_val, "iv")?;
            let aad_v = get_prop(vm, alg_val, "additionalData")?;
            let aad = if aad_v.is_undefined() {
                Vec::new()
            } else {
                read_buffer_source(vm, aad_v)?
            };
            let tag_v = get_prop(vm, alg_val, "tagLength")?;
            let tag_bits = if tag_v.is_undefined() {
                128
            } else {
                tag_v.as_number().unwrap_or(128.0) as usize
            };
            aes_gcm_decrypt(bytes, &iv, &aad, data, tag_bits)
        }
        (KeyMaterial::Aes { alg, bytes }, "AES-CBC") if alg == "AES-CBC" => {
            let iv = get_prop_bytes(vm, alg_val, "iv")?;
            aes_cbc_decrypt(bytes, &iv, data)
        }
        (KeyMaterial::Aes { alg, bytes }, "AES-CTR") if alg == "AES-CTR" => {
            let counter = get_prop_bytes(vm, alg_val, "counter")?;
            let length = get_prop(vm, alg_val, "length")?.as_number().unwrap_or(0.0) as u32;
            aes_ctr_xor(bytes, &counter, length, data)
        }
        (KeyMaterial::RsaPrivate { alg, hash, key: sk }, "RSA-OAEP") if alg == "RSA-OAEP" => {
            let label_v = get_prop(vm, alg_val, "label")?;
            let label = if label_v.is_undefined() {
                None
            } else {
                Some(read_buffer_source(vm, label_v)?)
            };
            rsa_oaep_decrypt(sk, *hash, label.as_deref(), data)
        }
        _ => Err(CryptoErr::new(
            "InvalidAccessError",
            format!("Key algorithm does not match decrypt algorithm '{alg_name}'"),
        )),
    }
}

fn op_encrypt(vm: &mut JsVm, state: &SharedCrypto, args: &[JsValue]) -> Result<JsValue, CryptoErr> {
    let key = require_key(state, arg(args, 1), Some("encrypt"))?;
    let data = read_buffer_source(vm, arg(args, 2))?;
    let out = encrypt_bytes(vm, arg(args, 0), &key, &data)?;
    Ok(JsValue::from_object(alloc_array_buffer(vm, out)))
}

fn op_decrypt(vm: &mut JsVm, state: &SharedCrypto, args: &[JsValue]) -> Result<JsValue, CryptoErr> {
    let key = require_key(state, arg(args, 1), Some("decrypt"))?;
    let data = read_buffer_source(vm, arg(args, 2))?;
    let out = decrypt_bytes(vm, arg(args, 0), &key, &data)?;
    Ok(JsValue::from_object(alloc_array_buffer(vm, out)))
}

fn op_sign(vm: &mut JsVm, state: &SharedCrypto, args: &[JsValue]) -> Result<JsValue, CryptoErr> {
    let alg_val = arg(args, 0);
    let alg_name = parse_algorithm_name(vm, alg_val)?.to_ascii_uppercase();
    let key = require_key(state, arg(args, 1), Some("sign"))?;
    let data = read_buffer_source(vm, arg(args, 2))?;

    let sig = match (&key.material, alg_name.as_str()) {
        (KeyMaterial::Hmac { hash, bytes }, "HMAC") => hmac_sign(*hash, bytes, &data)?,
        (KeyMaterial::Ed25519Private(sk), "ED25519") => sk.sign(&data).to_bytes().to_vec(),
        (KeyMaterial::EcP256Private { alg, key: sk }, "ECDSA") if alg == "ECDSA" => {
            let hash = parse_hash_from_alg(vm, alg_val)?;
            let digest = hash.digest_bytes(&data);
            let signer = p256::ecdsa::SigningKey::from(sk.clone());
            let s: p256::ecdsa::Signature = signer
                .sign_prehash(&digest)
                .map_err(|_| CryptoErr::new("OperationError", "ECDSA P-256 sign failed"))?;
            s.to_bytes().to_vec()
        }
        (KeyMaterial::EcP384Private { alg, key: sk }, "ECDSA") if alg == "ECDSA" => {
            let hash = parse_hash_from_alg(vm, alg_val)?;
            let digest = hash.digest_bytes(&data);
            let signer = p384::ecdsa::SigningKey::from(sk.clone());
            let s: p384::ecdsa::Signature = signer
                .sign_prehash(&digest)
                .map_err(|_| CryptoErr::new("OperationError", "ECDSA P-384 sign failed"))?;
            s.to_bytes().to_vec()
        }
        (KeyMaterial::RsaPrivate { alg, hash, key: sk }, "RSA-PSS") if alg == "RSA-PSS" => {
            let salt_len = get_prop(vm, alg_val, "saltLength")?
                .as_number()
                .unwrap_or(32.0) as usize;
            rsa_pss_sign(sk, *hash, salt_len, &data)?
        }
        (KeyMaterial::RsaPrivate { alg, hash, key: sk }, "RSASSA-PKCS1-V1_5")
            if alg == "RSASSA-PKCS1-v1_5" =>
        {
            match hash {
                HashAlg::Sha1 => sk.pkcs1v15_sign::<Sha1>(&data),
                HashAlg::Sha256 => sk.pkcs1v15_sign::<Sha256>(&data),
                HashAlg::Sha384 => sk.pkcs1v15_sign::<Sha384>(&data),
                HashAlg::Sha512 => sk.pkcs1v15_sign::<Sha512>(&data),
            }
            .map_err(|e| CryptoErr::new("OperationError", format!("RSASSA-PKCS1-v1_5 sign: {e}")))?
        }
        _ => {
            return Err(CryptoErr::new(
                "InvalidAccessError",
                format!("Key does not match sign algorithm '{alg_name}'"),
            ));
        }
    };
    Ok(JsValue::from_object(alloc_array_buffer(vm, sig)))
}

fn op_verify(vm: &mut JsVm, state: &SharedCrypto, args: &[JsValue]) -> Result<JsValue, CryptoErr> {
    let alg_val = arg(args, 0);
    let alg_name = parse_algorithm_name(vm, alg_val)?.to_ascii_uppercase();
    let key = require_key(state, arg(args, 1), Some("verify"))?;
    let sig_bytes = read_buffer_source(vm, arg(args, 2))?;
    let data = read_buffer_source(vm, arg(args, 3))?;

    let valid = match (&key.material, alg_name.as_str()) {
        (KeyMaterial::Hmac { hash, bytes }, "HMAC") => {
            hmac_verify(*hash, bytes, &sig_bytes, &data)?
        }
        (KeyMaterial::Ed25519Public(vk), "ED25519") => {
            let Ok(sig) = Ed25519Sig::from_slice(&sig_bytes) else {
                return Ok(JsValue::from_bool(false));
            };
            vk.verify(&data, &sig).is_ok()
        }
        (KeyMaterial::EcP256Public { alg, key: pk }, "ECDSA") if alg == "ECDSA" => {
            let hash = parse_hash_from_alg(vm, alg_val)?;
            let digest = hash.digest_bytes(&data);
            let Ok(sig) = p256::ecdsa::Signature::from_slice(&sig_bytes) else {
                return Ok(JsValue::from_bool(false));
            };
            let verifier = p256::ecdsa::VerifyingKey::from(*pk);
            verifier.verify_prehash(&digest, &sig).is_ok()
        }
        (KeyMaterial::EcP384Public { alg, key: pk }, "ECDSA") if alg == "ECDSA" => {
            let hash = parse_hash_from_alg(vm, alg_val)?;
            let digest = hash.digest_bytes(&data);
            let Ok(sig) = p384::ecdsa::Signature::from_slice(&sig_bytes) else {
                return Ok(JsValue::from_bool(false));
            };
            let verifier = p384::ecdsa::VerifyingKey::from(*pk);
            verifier.verify_prehash(&digest, &sig).is_ok()
        }
        (KeyMaterial::RsaPublic { alg, hash, key: pk }, "RSA-PSS") if alg == "RSA-PSS" => {
            let salt_len = get_prop(vm, alg_val, "saltLength")?
                .as_number()
                .unwrap_or(32.0) as usize;
            rsa_pss_verify(pk, *hash, salt_len, &sig_bytes, &data)?
        }
        (KeyMaterial::RsaPublic { alg, hash, key: pk }, "RSASSA-PKCS1-V1_5")
            if alg == "RSASSA-PKCS1-v1_5" =>
        {
            match hash {
                HashAlg::Sha1 => pk.pkcs1v15_verify::<Sha1>(&data, &sig_bytes).is_ok(),
                HashAlg::Sha256 => pk.pkcs1v15_verify::<Sha256>(&data, &sig_bytes).is_ok(),
                HashAlg::Sha384 => pk.pkcs1v15_verify::<Sha384>(&data, &sig_bytes).is_ok(),
                HashAlg::Sha512 => pk.pkcs1v15_verify::<Sha512>(&data, &sig_bytes).is_ok(),
            }
        }
        _ => {
            return Err(CryptoErr::new(
                "InvalidAccessError",
                format!("Key does not match verify algorithm '{alg_name}'"),
            ));
        }
    };
    Ok(JsValue::from_bool(valid))
}

fn derive_raw_bits(
    vm: &mut JsVm,
    state: &SharedCrypto,
    alg_val: JsValue,
    base_key: &CryptoKeyRecord,
    length_bits: usize,
) -> Result<Vec<u8>, CryptoErr> {
    if !length_bits.is_multiple_of(8) {
        return Err(CryptoErr::new(
            "OperationError",
            "deriveBits length must be a multiple of 8 bits",
        ));
    }
    let out_bytes = length_bits / 8;
    let alg_name = parse_algorithm_name(vm, alg_val)?.to_ascii_uppercase();

    match (&base_key.material, alg_name.as_str()) {
        (KeyMaterial::RawKdf { alg, bytes }, "HKDF") if alg == "HKDF" => {
            let hash = parse_hash_from_alg(vm, alg_val)?;
            let salt = get_prop_bytes(vm, alg_val, "salt")?;
            let info = get_prop_bytes(vm, alg_val, "info")?;
            hkdf_derive(hash, bytes, &salt, &info, out_bytes)
        }
        (KeyMaterial::RawKdf { alg, bytes }, "PBKDF2") if alg == "PBKDF2" => {
            let hash = parse_hash_from_alg(vm, alg_val)?;
            let salt = get_prop_bytes(vm, alg_val, "salt")?;
            let iterations = get_prop(vm, alg_val, "iterations")?
                .as_number()
                .unwrap_or(0.0) as u32;
            pbkdf2_derive(hash, bytes, &salt, iterations, out_bytes)
        }
        (KeyMaterial::EcP256Private { alg, key: sk }, "ECDH") if alg == "ECDH" => {
            let pub_val = get_prop(vm, alg_val, "public")?;
            let peer = require_key(state, pub_val, None)?;
            let KeyMaterial::EcP256Public { key: pk, .. } = peer.material else {
                return Err(CryptoErr::new(
                    "InvalidAccessError",
                    "ECDH peer key must be a P-256 public key",
                ));
            };
            let shared = p256::ecdh::diffie_hellman(sk.to_nonzero_scalar(), pk.as_affine());
            let raw = shared.raw_secret_bytes();
            if out_bytes > raw.len() {
                return Err(CryptoErr::new(
                    "OperationError",
                    "Requested ECDH length exceeds curve size",
                ));
            }
            Ok(raw[..out_bytes].to_vec())
        }
        (KeyMaterial::EcP384Private { alg, key: sk }, "ECDH") if alg == "ECDH" => {
            let pub_val = get_prop(vm, alg_val, "public")?;
            let peer = require_key(state, pub_val, None)?;
            let KeyMaterial::EcP384Public { key: pk, .. } = peer.material else {
                return Err(CryptoErr::new(
                    "InvalidAccessError",
                    "ECDH peer key must be a P-384 public key",
                ));
            };
            let shared = p384::ecdh::diffie_hellman(sk.to_nonzero_scalar(), pk.as_affine());
            let raw = shared.raw_secret_bytes();
            if out_bytes > raw.len() {
                return Err(CryptoErr::new(
                    "OperationError",
                    "Requested ECDH length exceeds curve size",
                ));
            }
            Ok(raw[..out_bytes].to_vec())
        }
        _ => Err(CryptoErr::new(
            "InvalidAccessError",
            format!("Base key does not support derivation with '{alg_name}'"),
        )),
    }
}

fn op_derive_bits(
    vm: &mut JsVm,
    state: &SharedCrypto,
    args: &[JsValue],
) -> Result<JsValue, CryptoErr> {
    let base_key = require_key(state, arg(args, 1), Some("deriveBits"))?;
    let length_bits = arg(args, 2).as_number().unwrap_or(0.0) as usize;
    let out = derive_raw_bits(vm, state, arg(args, 0), &base_key, length_bits)?;
    Ok(JsValue::from_object(alloc_array_buffer(vm, out)))
}

fn op_derive_key(
    vm: &mut JsVm,
    state: &SharedCrypto,
    args: &[JsValue],
) -> Result<JsValue, CryptoErr> {
    let base_key = require_key(state, arg(args, 1), Some("deriveKey"))?;
    let derived_alg = arg(args, 2);
    let derived_name = parse_algorithm_name(vm, derived_alg)?.to_ascii_uppercase();
    let extractable = vm.to_boolean(arg(args, 3));
    let usages = parse_usages(vm, arg(args, 4))?;

    let length_bits = match derived_name.as_str() {
        "AES-GCM" | "AES-CBC" | "AES-CTR" => get_prop(vm, derived_alg, "length")?
            .as_number()
            .unwrap_or(256.0) as usize,
        "HMAC" => {
            let lv = get_prop(vm, derived_alg, "length")?;
            if lv.is_undefined() {
                parse_hash_from_alg(vm, derived_alg)?.block_size_bytes() * 8
            } else {
                lv.as_number().unwrap_or(256.0) as usize
            }
        }
        other => {
            return Err(CryptoErr::new(
                "NotSupportedError",
                format!("Unsupported derivedKeyAlgorithm: {other}"),
            ));
        }
    };

    let bytes = derive_raw_bits(vm, state, arg(args, 0), &base_key, length_bits)?;
    let ab = alloc_array_buffer(vm, bytes);
    let rec = import_key_record(
        vm,
        "raw",
        JsValue::from_object(ab),
        derived_alg,
        extractable,
        usages,
    )?;
    let kh = alloc_crypto_key(vm, state, rec);
    Ok(JsValue::from_object(kh))
}

fn op_wrap_key(
    vm: &mut JsVm,
    state: &SharedCrypto,
    args: &[JsValue],
) -> Result<JsValue, CryptoErr> {
    let format = vm.value_to_rust_string(arg(args, 0));
    let target = require_key(state, arg(args, 1), None)?;
    if !target.extractable {
        return Err(CryptoErr::new(
            "InvalidAccessError",
            "Target key is not extractable",
        ));
    }
    let wrapping = require_key(state, arg(args, 2), Some("wrapKey"))?;
    let exported = export_key_record(vm, &format, &target)?;
    let raw_bytes = if format == "jwk" {
        let json_k = vm.heap.intern_key("JSON");
        let str_k = vm.heap.intern_key("stringify");
        let json_obj = vm
            .get_property_by_key(vm.global, json_k)
            .map_err(|e| CryptoErr::new("OperationError", e.message))?;
        let sfn = vm
            .get_property_by_key(json_obj.as_object().unwrap_or(vm.global), str_k)
            .map_err(|e| CryptoErr::new("OperationError", e.message))?;
        let jstr = vm
            .call_value(sfn, json_obj, &[exported])
            .map_err(|e| CryptoErr::new("OperationError", e.message))?;
        vm.value_to_rust_string(jstr).into_bytes()
    } else {
        read_buffer_source(vm, exported)?
    };
    let wrapped = encrypt_bytes(vm, arg(args, 3), &wrapping, &raw_bytes)?;
    Ok(JsValue::from_object(alloc_array_buffer(vm, wrapped)))
}

fn op_unwrap_key(
    vm: &mut JsVm,
    state: &SharedCrypto,
    args: &[JsValue],
) -> Result<JsValue, CryptoErr> {
    let format = vm.value_to_rust_string(arg(args, 0));
    let wrapped_bytes = read_buffer_source(vm, arg(args, 1))?;
    let unwrapping = require_key(state, arg(args, 2), Some("unwrapKey"))?;
    let unwrapped_bytes = decrypt_bytes(vm, arg(args, 3), &unwrapping, &wrapped_bytes)?;
    let key_data = if format == "jwk" {
        let s = String::from_utf8(unwrapped_bytes)
            .map_err(|_| CryptoErr::new("DataError", "Unwrapped JWK is not valid UTF-8"))?;
        let json_k = vm.heap.intern_key("JSON");
        let parse_k = vm.heap.intern_key("parse");
        let json_obj = vm
            .get_property_by_key(vm.global, json_k)
            .map_err(|e| CryptoErr::new("OperationError", e.message))?;
        let pfn = vm
            .get_property_by_key(json_obj.as_object().unwrap_or(vm.global), parse_k)
            .map_err(|e| CryptoErr::new("OperationError", e.message))?;
        let sv = str_val(vm, &s);
        vm.call_value(pfn, json_obj, &[sv])
            .map_err(|e| CryptoErr::new("DataError", e.message))?
    } else {
        JsValue::from_object(alloc_array_buffer(vm, unwrapped_bytes))
    };
    let extractable = vm.to_boolean(arg(args, 5));
    let usages = parse_usages(vm, arg(args, 6))?;
    let rec = import_key_record(vm, &format, key_data, arg(args, 4), extractable, usages)?;
    let kh = alloc_crypto_key(vm, state, rec);
    Ok(JsValue::from_object(kh))
}

// ---------------------------------------------------------------------------
// Public entry point: install global `crypto`, `Crypto`, `SubtleCrypto`, `CryptoKey`
// ---------------------------------------------------------------------------

/// Installs `globalThis.crypto` (`getRandomValues`, `randomUUID`, `subtle`),
/// `Crypto`, `SubtleCrypto`, and `CryptoKey` onto `vm`.
pub fn install_webcrypto(vm: &mut JsVm) {
    let state: SharedCrypto = Rc::new(RefCell::new(CryptoState::default()));

    // CryptoKey prototype
    let ck_proto = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(vm.object_proto),
    ));
    state.borrow_mut().crypto_key_proto = Some(ck_proto);
    {
        let st = Rc::clone(&state);
        def_host_getter(vm, ck_proto, "type", move |vm, this_val, _| {
            let key = require_key(&st, this_val, None)
                .map_err(|e| make_crypto_exception(vm, e.name, e.message))?;
            Ok(str_val(vm, key.key_type))
        });
    }
    {
        let st = Rc::clone(&state);
        def_host_getter(vm, ck_proto, "extractable", move |vm, this_val, _| {
            let key = require_key(&st, this_val, None)
                .map_err(|e| make_crypto_exception(vm, e.name, e.message))?;
            Ok(JsValue::from_bool(key.extractable))
        });
    }
    {
        let st = Rc::clone(&state);
        def_host_getter(vm, ck_proto, "algorithm", move |vm, this_val, _| {
            let key = require_key(&st, this_val, None)
                .map_err(|e| make_crypto_exception(vm, e.name, e.message))?;
            Ok(JsValue::from_object(build_key_algorithm_js(vm, &key)))
        });
    }
    {
        let st = Rc::clone(&state);
        def_host_getter(vm, ck_proto, "usages", move |vm, this_val, _| {
            let key = require_key(&st, this_val, None)
                .map_err(|e| make_crypto_exception(vm, e.name, e.message))?;
            let vals: Vec<JsValue> = key.usages.iter().map(|u| str_val(vm, u)).collect();
            Ok(JsValue::from_object(alloc_array_of(vm, &vals)))
        });
    }
    let ck_ctor = vm.alloc_host_fn("CryptoKey", |vm, _, _| {
        Err(vm.type_error("Illegal constructor"))
    });
    bind_ctor_proto(vm, ck_ctor, ck_proto);
    let global = vm.global;
    def_data(
        vm,
        global,
        "CryptoKey",
        JsValue::from_object(ck_ctor),
        PropertyAttrs::METHOD,
    );

    // SubtleCrypto prototype
    let subtle_proto = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(vm.object_proto),
    ));
    def_host_method(vm, subtle_proto, "digest", |vm, _, args| {
        let r = op_digest(vm, args);
        Ok(settle_promise(vm, r))
    });
    {
        let st = Rc::clone(&state);
        def_host_method(vm, subtle_proto, "generateKey", move |vm, _, args| {
            let r = op_generate_key(vm, &st, args);
            Ok(settle_promise(vm, r))
        });
    }
    {
        let st = Rc::clone(&state);
        def_host_method(vm, subtle_proto, "importKey", move |vm, _, args| {
            let r = op_import_key(vm, &st, args);
            Ok(settle_promise(vm, r))
        });
    }
    {
        let st = Rc::clone(&state);
        def_host_method(vm, subtle_proto, "exportKey", move |vm, _, args| {
            let r = op_export_key(vm, &st, args);
            Ok(settle_promise(vm, r))
        });
    }
    {
        let st = Rc::clone(&state);
        def_host_method(vm, subtle_proto, "encrypt", move |vm, _, args| {
            let r = op_encrypt(vm, &st, args);
            Ok(settle_promise(vm, r))
        });
    }
    {
        let st = Rc::clone(&state);
        def_host_method(vm, subtle_proto, "decrypt", move |vm, _, args| {
            let r = op_decrypt(vm, &st, args);
            Ok(settle_promise(vm, r))
        });
    }
    {
        let st = Rc::clone(&state);
        def_host_method(vm, subtle_proto, "sign", move |vm, _, args| {
            let r = op_sign(vm, &st, args);
            Ok(settle_promise(vm, r))
        });
    }
    {
        let st = Rc::clone(&state);
        def_host_method(vm, subtle_proto, "verify", move |vm, _, args| {
            let r = op_verify(vm, &st, args);
            Ok(settle_promise(vm, r))
        });
    }
    {
        let st = Rc::clone(&state);
        def_host_method(vm, subtle_proto, "deriveBits", move |vm, _, args| {
            let r = op_derive_bits(vm, &st, args);
            Ok(settle_promise(vm, r))
        });
    }
    {
        let st = Rc::clone(&state);
        def_host_method(vm, subtle_proto, "deriveKey", move |vm, _, args| {
            let r = op_derive_key(vm, &st, args);
            Ok(settle_promise(vm, r))
        });
    }
    {
        let st = Rc::clone(&state);
        def_host_method(vm, subtle_proto, "wrapKey", move |vm, _, args| {
            let r = op_wrap_key(vm, &st, args);
            Ok(settle_promise(vm, r))
        });
    }
    {
        let st = Rc::clone(&state);
        def_host_method(vm, subtle_proto, "unwrapKey", move |vm, _, args| {
            let r = op_unwrap_key(vm, &st, args);
            Ok(settle_promise(vm, r))
        });
    }

    let subtle_ctor = vm.alloc_host_fn("SubtleCrypto", |vm, _, _| {
        Err(vm.type_error("Illegal constructor"))
    });
    bind_ctor_proto(vm, subtle_ctor, subtle_proto);
    def_data(
        vm,
        global,
        "SubtleCrypto",
        JsValue::from_object(subtle_ctor),
        PropertyAttrs::METHOD,
    );
    let subtle_obj = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(subtle_proto),
    ));

    // Crypto prototype (`getRandomValues`, `randomUUID`, `subtle`)
    let crypto_proto = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(vm.object_proto),
    ));
    def_host_method(vm, crypto_proto, "getRandomValues", |vm, _, args| {
        let ta_val = arg(args, 0);
        let Some(h) = ta_val.as_object() else {
            return Err(make_crypto_exception(
                vm,
                "TypeMismatchError",
                "getRandomValues requires an integer TypedArray",
            ));
        };
        let (buf_h, start, byte_len) = match vm.heap.get_object(h).map(|o| &o.kind) {
            Some(ObjectKind::TypedArray(ta))
                if !matches!(ta.kind, TypedArrayKind::Float32 | TypedArrayKind::Float64) =>
            {
                let start = ta.byte_offset as usize;
                let byte_len = (ta.length as usize).saturating_mul(ta.kind.bytes_per_element());
                (ta.buffer, start, byte_len)
            }
            _ => {
                return Err(make_crypto_exception(
                    vm,
                    "TypeMismatchError",
                    "getRandomValues requires an integer TypedArray",
                ));
            }
        };
        if byte_len > 65_536 {
            return Err(make_crypto_exception(
                vm,
                "QuotaExceededError",
                "getRandomValues byteLength exceeds 65536 bytes",
            ));
        }
        if let Some(ObjectKind::ArrayBuffer(buf)) =
            vm.heap.get_object_mut(buf_h).map(|o| &mut o.kind)
        {
            let s = start.min(buf.len());
            let e = s.saturating_add(byte_len).min(buf.len());
            getrandom::fill(&mut buf[s..e])
                .map_err(|err| make_crypto_exception(vm, "OperationError", err.to_string()))?;
        }
        Ok(ta_val)
    });

    def_host_method(vm, crypto_proto, "randomUUID", |vm, _, _| {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes)
            .map_err(|err| make_crypto_exception(vm, "OperationError", err.to_string()))?;
        bytes[6] = (bytes[6] & 0x0f) | 0x40;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        let uuid = format!(
            "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
            bytes[0], bytes[1], bytes[2], bytes[3],
            bytes[4], bytes[5],
            bytes[6], bytes[7],
            bytes[8], bytes[9],
            bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15]
        );
        Ok(str_val(vm, &uuid))
    });

    def_host_getter(vm, crypto_proto, "subtle", move |_, _, _| {
        Ok(JsValue::from_object(subtle_obj))
    });

    let crypto_ctor = vm.alloc_host_fn("Crypto", |vm, _, _| {
        Err(vm.type_error("Illegal constructor"))
    });
    bind_ctor_proto(vm, crypto_ctor, crypto_proto);
    def_data(
        vm,
        global,
        "Crypto",
        JsValue::from_object(crypto_ctor),
        PropertyAttrs::METHOD,
    );

    let crypto_obj = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(crypto_proto),
    ));
    def_data(
        vm,
        global,
        "crypto",
        JsValue::from_object(crypto_obj),
        PropertyAttrs::READONLY,
    );
}
