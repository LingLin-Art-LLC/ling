//! Keyed / signed crosshatch: deterministic per key, ML-DSA-87 record verifies, watermark reads back only with the right key.
use ling_paint::{crosshatch, Studio, Val};

fn n(v: f64) -> Val {
    Val::Num(v)
}
fn s(v: &str) -> Val {
    Val::Str(v.into())
}
fn call(st: &mut Studio, name: &str, a: &[Val]) -> Val {
    st.call(name, a).unwrap_or_else(|| panic!("unknown call {name}")).unwrap()
}
fn list(v: Val) -> Vec<Val> {
    match v {
        Val::List(l) => l,
        o => panic!("expected a list, got {o:?}"),
    }
}
fn nums(v: Val) -> Vec<f64> {
    list(v).into_iter().map(|x| if let Val::Num(f) = x { f } else { f64::NAN }).collect()
}
fn strs(v: Val) -> Vec<String> {
    list(v).into_iter().map(|x| if let Val::Str(s) = x { s } else { String::new() }).collect()
}
fn unhex(h: &str) -> Vec<u8> {
    (0..h.len()).step_by(2).map(|i| u8::from_str_radix(&h[i..i + 2], 16).unwrap()).collect()
}
fn params(seed: f64) -> Val {
    // spacing, angle, directions, weight, wobble, contrast, seed source, watermark, ink (on white)
    Val::List([6.0, 45.0, 3.0, 0.35, 0.3, 1.0, seed, 1.0, 1.0].iter().map(|&v| n(v)).collect())
}

/// a 600 x 400 page with a left-to-right grey ramp, hatched with `pass`; returns the png bytes of the result
fn hatched(pass: &str, seed: f64) -> (Studio, Vec<u8>) {
    let mut st = Studio::new();
    call(&mut st, "art_doc_new", &[n(600.0), n(400.0), n(300.0), s("Hatch"), n(16777215.0), n(1.0)]);
    call(&mut st, "art_layer_add", &[n(0.0), s("Tone")]);
    call(&mut st, "art_color", &[n(0.0)]);
    call(&mut st, "art_grad_preset", &[s("FG to BG")]);
    call(&mut st, "art_grad_apply", &[n(0.0), n(200.0), n(600.0), n(200.0), n(0.0), n(0.0), n(1.0), n(0.0)]);
    call(&mut st, "art_color", &[n(1181726.0)]); // 18,8,30 ink
    call(&mut st, "art_hatch_keys", &[s(pass), s("drawn by Ling")]);
    let ok = call(&mut st, "art_filter_apply", &[s("crosshatch"), params(seed), Val::List(vec![])]);
    assert!(matches!(ok, Val::Num(v) if v == 1.0), "apply failed: {:?}", call(&mut st, "art_error", &[]));
    let out = std::env::temp_dir().join(format!("xhatch_{seed}_{}.png", pass.len()));
    let p = out.to_string_lossy().to_string();
    call(&mut st, "art_export_png", &[s(&p), n(1.0), n(0.0)]);
    let bytes = std::fs::read(&out).unwrap_or_default();
    (st, bytes)
}

#[test]
fn crosshatch_is_keyed_signed_and_watermarked() {
    for seed in [0.0, 1.0] {
        let (mut st, a) = hatched("correct horse battery staple", seed);
        let (_, b) = hatched("correct horse battery staple", seed);
        let (_, c) = hatched("a different passphrase", seed);
        if !a.is_empty() {
            assert_eq!(a, b, "same key + settings must draw the same hatching");
            assert_ne!(a, c, "another key must draw different hatching");
        }

        // the watermark reads back with the right key ...
        let r = nums(call(&mut st, "art_hatch_verify", &[params(seed)]));
        println!("seed {seed}: right key {r:?}");
        assert!(r[1] >= 100.0, "too few bits read: {r:?}");
        assert!(r[0] / r[1] > 0.95, "watermark not found with the right key: {r:?}");
        // ... and not with a wrong one
        call(&mut st, "art_hatch_keys", &[s("a different passphrase"), s("drawn by Ling")]);
        let w = nums(call(&mut st, "art_hatch_verify", &[params(seed)]));
        println!("seed {seed}: wrong key {w:?}");
        assert!(w[1] < 1.0 || w[0] / w[1] < 0.75, "a wrong key matched: {w:?}");
        // ... nor with the right key but other watermark text
        call(&mut st, "art_hatch_keys", &[s("correct horse battery staple"), s("someone else")]);
        let t = nums(call(&mut st, "art_hatch_verify", &[params(seed)]));
        println!("seed {seed}: wrong text {t:?}");
        assert!(t[1] < 1.0 || t[0] / t[1] < 0.75, "other watermark text matched: {t:?}");

        // ML-DSA-87 record: the signature verifies for the message, and fails for a changed one
        call(&mut st, "art_hatch_keys", &[s("correct horse battery staple"), s("drawn by Ling")]);
        let info = strs(call(&mut st, "art_hatch_info", &[params(seed)]));
        assert_eq!(info[0].len(), 16);
        if seed == 1.0 {
            let (pk, sig, mut msg) = (unhex(&info[1]), unhex(&info[2]), unhex(&info[3]));
            assert_eq!(pk.len(), 2592);
            assert_eq!(sig.len(), 4627);
            assert!(crosshatch::verify_signature(&pk, &msg, &sig));
            msg[30] ^= 1;
            assert!(!crosshatch::verify_signature(&pk, &msg, &sig));
        } else {
            assert!(info[1].is_empty() && info[2].is_empty());
        }
    }
}
