//! X.509 certificate checking for the TLS client: just enough DER to verify a
//! server's chain up to a bundled root, its validity dates and its host name, and
//! the signature on the TLS 1.3 CertificateVerify. Pure: no kernel calls (the
//! caller passes the time), so the same file is unit-tested on the host against a
//! real chain (scratchpad claudetest).
//!
//! Scope, stated plainly: RSA (PKCS#1 v1.5 with SHA-256/384/512) and ECDSA (P-256,
//! P-384) certificate signatures; RSA-PSS, ECDSA and Ed25519 CertificateVerify
//! signatures; subjectAltName dNSName matching (one leading `*.` label); basic
//! constraints (an issuer must be a CA). Not checked: revocation (OCSP / CRL),
//! name constraints, key usage / extended key usage, policy, critical-extension
//! refusal. That is the usual minimum of a small client; it stops an active
//! man-in-the-middle, which the old "no verification at all" did not.

use alloc::string::String;
use alloc::vec::Vec;

// ── DER ──

/// One tag-length-value: (tag, content, the whole TLV, rest after it).
fn tlv(d: &[u8]) -> Option<(u8, &[u8], &[u8], &[u8])> {
    let tag = *d.first()?;
    let l0 = *d.get(1)? as usize;
    let (len, hl) = if l0 < 0x80 {
        (l0, 2)
    } else {
        let n = l0 & 0x7F;
        if n == 0 || n > 4 || d.len() < 2 + n {
            return None;
        }
        let mut v = 0usize;
        for &b in &d[2..2 + n] {
            v = (v << 8) | b as usize;
        }
        (v, 2 + n)
    };
    let end = hl.checked_add(len)?;
    if d.len() < end {
        return None;
    }
    Some((tag, &d[hl..end], &d[..end], &d[end..]))
}

/// Items of a constructed value, in order.
fn items(mut d: &[u8]) -> Option<Vec<(u8, &[u8], &[u8])>> {
    let mut out = Vec::new();
    while !d.is_empty() {
        let (t, c, whole, rest) = tlv(d)?;
        out.push((t, c, whole));
        d = rest;
    }
    Some(out)
}

const OID_EC_PUBKEY: &[u8] = &[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x02, 0x01];
const OID_P256: &[u8] = &[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x03, 0x01, 0x07];
const OID_P384: &[u8] = &[0x2B, 0x81, 0x04, 0x00, 0x22];
const OID_RSA: &[u8] = &[0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x01, 0x01];
const OID_ED25519: &[u8] = &[0x2B, 0x65, 0x70];
const OID_ECDSA_SHA256: &[u8] = &[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x04, 0x03, 0x02];
const OID_ECDSA_SHA384: &[u8] = &[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x04, 0x03, 0x03];
const OID_ECDSA_SHA512: &[u8] = &[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x04, 0x03, 0x04];
const OID_RSA_SHA256: &[u8] = &[0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x01, 0x0B];
const OID_RSA_SHA384: &[u8] = &[0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x01, 0x0C];
const OID_RSA_SHA512: &[u8] = &[0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x01, 0x0D];
const OID_SAN: &[u8] = &[0x55, 0x1D, 0x11];
const OID_BASIC: &[u8] = &[0x55, 0x1D, 0x13];

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum KeyKind {
    P256,
    P384,
    Rsa,
    Ed25519,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Hash {
    Sha256,
    Sha384,
    Sha512,
}

/// What a certificate holds that the checks need (borrowed from the DER).
pub struct Cert<'a> {
    pub der: &'a [u8],
    pub tbs: &'a [u8],
    pub sig_alg: (bool, Hash), // (ecdsa?, hash): true = ECDSA, false = RSA PKCS#1 v1.5
    pub sig: &'a [u8],
    pub issuer: &'a [u8],
    pub subject: &'a [u8],
    pub not_before: i64,
    pub not_after: i64,
    pub key_kind: KeyKind,
    /// EC: the SEC1 point; RSA: the PKCS#1 RSAPublicKey DER; Ed25519: 32 bytes.
    pub key: &'a [u8],
    pub is_ca: bool,
    pub dns_names: Vec<&'a [u8]>,
}

/// UTCTime (YYMMDDHHMMSSZ) / GeneralizedTime (YYYYMMDDHHMMSSZ) -> unix seconds.
fn der_time(tag: u8, s: &[u8]) -> Option<i64> {
    let d = |i: usize| -> Option<i64> {
        let a = *s.get(i)?;
        let b = *s.get(i + 1)?;
        if !a.is_ascii_digit() || !b.is_ascii_digit() {
            return None;
        }
        Some(((a - b'0') * 10 + (b - b'0')) as i64)
    };
    let (year, at) = match tag {
        0x17 => {
            let y = d(0)?;
            (if y >= 50 { 1900 + y } else { 2000 + y }, 2)
        },
        0x18 => (d(0)? * 100 + d(2)?, 4),
        _ => return None,
    };
    let (mo, day, h, mi, se) = (d(at)?, d(at + 2)?, d(at + 4)?, d(at + 6)?, d(at + 8)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&day) {
        return None;
    }
    // days from civil (Howard Hinnant)
    let y = if mo <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + h * 3600 + mi * 60 + se)
}

fn sig_alg(alg_seq: &[u8]) -> Option<(bool, Hash)> {
    let (t, oid, _, _) = tlv(alg_seq)?;
    if t != 0x06 {
        return None;
    }
    Some(match oid {
        x if x == OID_ECDSA_SHA256 => (true, Hash::Sha256),
        x if x == OID_ECDSA_SHA384 => (true, Hash::Sha384),
        x if x == OID_ECDSA_SHA512 => (true, Hash::Sha512),
        x if x == OID_RSA_SHA256 => (false, Hash::Sha256),
        x if x == OID_RSA_SHA384 => (false, Hash::Sha384),
        x if x == OID_RSA_SHA512 => (false, Hash::Sha512),
        _ => return None,
    })
}

/// A BIT STRING's bytes (the unused-bits count must be 0).
fn bits(c: &[u8]) -> Option<&[u8]> {
    if c.first() != Some(&0) {
        return None;
    }
    Some(&c[1..])
}

impl<'a> Cert<'a> {
    pub fn parse(der: &'a [u8]) -> Option<Cert<'a>> {
        let (t, body, whole, _) = tlv(der)?;
        if t != 0x30 {
            return None;
        }
        let top = items(body)?;
        if top.len() != 3 {
            return None;
        }
        let tbs = top[0].2;
        let (sa_t, sa_c, _) = top[1];
        if sa_t != 0x30 {
            return None;
        }
        let alg = sig_alg(sa_c)?;
        if top[2].0 != 0x03 {
            return None;
        }
        let sig = bits(top[2].1)?;
        let f = items(top[0].1)?;
        // [0] version is optional
        let mut i = if f.first()?.0 == 0xA0 { 1 } else { 0 };
        i += 1; // serial
        i += 1; // signature algorithm (must match the outer one; not used)
        let issuer = f.get(i)?.2;
        i += 1;
        let validity = items(f.get(i)?.1)?;
        let not_before = der_time(validity.first()?.0, validity.first()?.1)?;
        let not_after = der_time(validity.get(1)?.0, validity.get(1)?.1)?;
        i += 1;
        let subject = f.get(i)?.2;
        i += 1;
        let spki = items(f.get(i)?.1)?;
        let alg_items = items(spki.first()?.1)?;
        let key_oid = alg_items.first()?.1;
        let key_bits = bits(spki.get(1)?.1)?;
        let key_kind = if key_oid == OID_EC_PUBKEY {
            match alg_items.get(1)?.1 {
                c if c == OID_P256 => KeyKind::P256,
                c if c == OID_P384 => KeyKind::P384,
                _ => return None,
            }
        } else if key_oid == OID_RSA {
            KeyKind::Rsa
        } else if key_oid == OID_ED25519 {
            KeyKind::Ed25519
        } else {
            return None;
        };
        i += 1;
        let mut is_ca = false;
        let mut dns_names = Vec::new();
        // [1] issuerUniqueID, [2] subjectUniqueID, [3] extensions
        for (tag, c, _) in f.iter().skip(i) {
            if *tag != 0xA3 {
                continue;
            }
            let (_, exts, _, _) = tlv(c)?;
            for (_, ext, _) in items(exts)? {
                let parts = items(ext)?;
                let oid = parts.first()?.1;
                let val = parts.last()?.1; // OCTET STRING (critical BOOLEAN may sit between)
                if oid == OID_SAN {
                    let (_, names, _, _) = tlv(val)?;
                    for (nt, nc, _) in items(names)? {
                        if nt == 0x82 {
                            dns_names.push(nc);
                        }
                    }
                } else if oid == OID_BASIC {
                    let (_, bc, _, _) = tlv(val)?;
                    if let Some((0x01, b, _)) = items(bc)?.first().copied() {
                        is_ca = b.first().copied().unwrap_or(0) != 0;
                    }
                }
            }
        }
        Some(Cert { der: whole, tbs, sig_alg: alg, sig, issuer, subject, not_before, not_after, key_kind, key: key_bits, is_ca, dns_names })
    }
}

// ── signatures ──

fn digest(h: Hash, msg: &[u8]) -> Vec<u8> {
    use sha2::Digest;
    match h {
        Hash::Sha256 => sha2::Sha256::digest(msg).to_vec(),
        Hash::Sha384 => sha2::Sha384::digest(msg).to_vec(),
        Hash::Sha512 => sha2::Sha512::digest(msg).to_vec(),
    }
}

/// ECDSA over a prehashed message (a P-256 key may sign a SHA-384 digest and so on), DER signature.
fn ecdsa_verify(kind: KeyKind, key: &[u8], prehash: &[u8], sig_der: &[u8]) -> bool {
    use p256::ecdsa::signature::hazmat::PrehashVerifier;
    match kind {
        KeyKind::P256 => {
            let (Ok(vk), Ok(sig)) = (p256::ecdsa::VerifyingKey::from_sec1_bytes(key), p256::ecdsa::Signature::from_der(sig_der)) else { return false };
            vk.verify_prehash(prehash, &sig).is_ok()
        },
        KeyKind::P384 => {
            let (Ok(vk), Ok(sig)) = (p384::ecdsa::VerifyingKey::from_sec1_bytes(key), p384::ecdsa::Signature::from_der(sig_der)) else { return false };
            vk.verify_prehash(prehash, &sig).is_ok()
        },
        _ => false,
    }
}

/// DigestInfo prefixes (RFC 8017 §9.2 note 1) for PKCS#1 v1.5.
fn pkcs1_prefix(h: Hash) -> &'static [u8] {
    match h {
        Hash::Sha256 => &[0x30, 0x31, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01, 0x05, 0x00, 0x04, 0x20],
        Hash::Sha384 => &[0x30, 0x41, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x02, 0x05, 0x00, 0x04, 0x30],
        Hash::Sha512 => &[0x30, 0x51, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x03, 0x05, 0x00, 0x04, 0x40],
    }
}

fn rsa_key(pkcs1_der: &[u8]) -> Option<rsa::RsaPublicKey> {
    use rsa::pkcs1::DecodeRsaPublicKey;
    rsa::RsaPublicKey::from_pkcs1_der(pkcs1_der).ok()
}

fn rsa_pkcs1_verify(key: &[u8], h: Hash, hashed: &[u8], sig: &[u8]) -> bool {
    let Some(k) = rsa_key(key) else { return false };
    let scheme = rsa::Pkcs1v15Sign { hash_len: Some(hashed.len()), prefix: pkcs1_prefix(h).into() };
    k.verify(scheme, hashed, sig).is_ok()
}

/// Is `child` signed by the key in `issuer`?
pub fn signed_by(child: &Cert, issuer: &Cert) -> bool {
    let (ecdsa, h) = child.sig_alg;
    let hashed = digest(h, child.tbs);
    if ecdsa {
        ecdsa_verify(issuer.key_kind, issuer.key, &hashed, child.sig)
    } else {
        issuer.key_kind == KeyKind::Rsa && rsa_pkcs1_verify(issuer.key, h, &hashed, child.sig)
    }
}

/// The TLS 1.3 CertificateVerify (RFC 8446 §4.4.3): `scheme` signed over
/// 64 spaces + "TLS 1.3, server CertificateVerify" + 0 + the transcript hash.
pub fn verify_certificate_verify(leaf: &Cert, scheme: u16, transcript_hash: &[u8], sig: &[u8]) -> bool {
    let mut msg = Vec::with_capacity(64 + 34 + transcript_hash.len());
    msg.extend_from_slice(&[0x20; 64]);
    msg.extend_from_slice(b"TLS 1.3, server CertificateVerify\0");
    msg.extend_from_slice(transcript_hash);
    match scheme {
        0x0403 if leaf.key_kind == KeyKind::P256 => ecdsa_verify(KeyKind::P256, leaf.key, &digest(Hash::Sha256, &msg), sig),
        0x0503 if leaf.key_kind == KeyKind::P384 => ecdsa_verify(KeyKind::P384, leaf.key, &digest(Hash::Sha384, &msg), sig),
        0x0804 | 0x0805 | 0x0806 if leaf.key_kind == KeyKind::Rsa => {
            let Some(k) = rsa_key(leaf.key) else { return false };
            match scheme {
                0x0804 => k.verify(rsa::Pss::new::<sha2::Sha256>(), &digest(Hash::Sha256, &msg), sig).is_ok(),
                0x0805 => k.verify(rsa::Pss::new::<sha2::Sha384>(), &digest(Hash::Sha384, &msg), sig).is_ok(),
                _ => k.verify(rsa::Pss::new::<sha2::Sha512>(), &digest(Hash::Sha512, &msg), sig).is_ok(),
            }
        },
        0x0807 if leaf.key_kind == KeyKind::Ed25519 => {
            let (Ok(kb), Ok(sb)) = (<[u8; 32]>::try_from(leaf.key), <[u8; 64]>::try_from(sig)) else { return false };
            let Ok(vk) = ed25519_dalek::VerifyingKey::from_bytes(&kb) else { return false };
            vk.verify_strict(&msg, &ed25519_dalek::Signature::from_bytes(&sb)).is_ok()
        },
        _ => false,
    }
}

// ── host names ──

fn ascii_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.to_ascii_lowercase() == y.to_ascii_lowercase())
}

/// RFC 6125: exact, or one leading `*.` label that matches exactly one label.
pub fn host_matches(pattern: &[u8], host: &str) -> bool {
    let h = host.as_bytes();
    if let Some(rest) = pattern.strip_prefix(b"*.") {
        let Some(dot) = h.iter().position(|&c| c == b'.') else { return false };
        dot > 0 && ascii_eq(rest, &h[dot + 1..])
    } else {
        ascii_eq(pattern, h)
    }
}

// ── the chain ──

/// The bundled trust anchors (DER), from the system store of the build machine
/// (Mozilla's set): Google Trust Services R1-R4 (api.anthropic.com, Google), ISRG
/// X1/X2 (Let's Encrypt), DigiCert Global G2/G3, Amazon Root CA 1.
pub const ROOTS: &[(&str, &[u8])] = &[
    ("GTS Root R1", include_bytes!("tls_roots/gts-root-r1.der")),
    ("GTS Root R2", include_bytes!("tls_roots/gts-root-r2.der")),
    ("GTS Root R3", include_bytes!("tls_roots/gts-root-r3.der")),
    ("GTS Root R4", include_bytes!("tls_roots/gts-root-r4.der")),
    ("ISRG Root X1", include_bytes!("tls_roots/isrg-root-x1.der")),
    ("ISRG Root X2", include_bytes!("tls_roots/isrg-root-x2.der")),
    ("DigiCert Global Root G2", include_bytes!("tls_roots/digicert-global-root-g2.der")),
    ("DigiCert Global Root G3", include_bytes!("tls_roots/digicert-global-root-g3.der")),
    ("Amazon Root CA 1", include_bytes!("tls_roots/amazon-root-ca-1.der")),
];

/// Verify a server's certificate list (leaf first, as TLS sends it) for `host`
/// at unix time `now`: Ok(the root's name), or Err(why). Every certificate on
/// the path must be inside its validity dates; every issuer below the root must
/// be a CA; the leaf must name `host`.
pub fn verify_chain(chain: &[&[u8]], host: &str, now: i64) -> Result<&'static str, String> {
    let certs: Vec<Cert> = chain.iter().filter_map(|d| Cert::parse(d)).collect();
    if certs.len() != chain.len() || certs.is_empty() {
        return Err(String::from("a certificate in the server's chain could not be read"));
    }
    let leaf = &certs[0];
    if !leaf.dns_names.iter().any(|n| host_matches(n, host)) {
        return Err(alloc::format!("the certificate is not for {host}"));
    }
    let roots: Vec<(&'static str, Cert)> = ROOTS.iter().filter_map(|(n, d)| Cert::parse(d).map(|c| (*n, c))).collect();
    let mut cur = 0usize;
    for _depth in 0..6 {
        let c = &certs[cur];
        if now < c.not_before || now > c.not_after {
            return Err(String::from("a certificate in the chain is expired or not yet valid (check the clock: `date`)"));
        }
        // the server sent a trusted root itself
        if let Some((n, _)) = roots.iter().find(|(_, r)| r.der == c.der) {
            return Ok(n);
        }
        // issued by a trusted root
        for (n, r) in &roots {
            if r.subject == c.issuer && signed_by(c, r) {
                return Ok(n);
            }
        }
        // else the next certificate up the server's chain
        let next = certs.iter().enumerate().find(|(j, p)| *j != cur && p.subject == c.issuer && p.is_ca && signed_by(c, p));
        match next {
            Some((j, _)) => cur = j,
            None => return Err(String::from("the certificate chain does not lead to a trusted root")),
        }
    }
    Err(String::from("the certificate chain is too long"))
}
