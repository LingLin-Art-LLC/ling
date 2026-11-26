//! Every LingStationX effect, run here (the no_std copy the LingOS kernel uses) and in ling-daw (Windows) on the
//! same input with the same knobs: the two must sound the same. Run with `-- --nocapture` for the table.

const SR: f32 = 48000.0;

/// 1.5 s of test music: a saw chord, a kick every half second, a noise hat, interleaved stereo.
fn input() -> Vec<f32> {
    let n = (SR * 1.5) as usize;
    let mut out = Vec::with_capacity(n * 2);
    let mut seed = 0x1234_5678u32;
    for i in 0..n {
        let t = i as f32 / SR;
        let mut s = 0.0;
        for f in [220.0f32, 277.18, 329.63] {
            s += ((t * f).fract() * 2.0 - 1.0) * 0.12;
        }
        let kt = t % 0.5;
        s += (std::f32::consts::TAU * (50.0 + 120.0 * (-kt * 30.0).exp()) * kt).sin() * (-kt * 9.0).exp() * 0.6;
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        let ht = (t + 0.25) % 0.5;
        let nz = (seed as f32 / u32::MAX as f32) * 2.0 - 1.0;
        s += nz * (-ht * 60.0).exp() * 0.15;
        out.push(s);
        out.push(s * 0.8 + nz * 0.01);
    }
    out
}

fn knobs(name: &str, salt: u32) -> Vec<(usize, f32)> {
    // the defaults moved a little, the same way for both
    let mut r = salt.wrapping_mul(2654435761) | 1;
    ling_fxcore::defaults(name)
        .iter()
        .enumerate()
        .map(|(i, (_, d))| {
            r ^= r << 13;
            r ^= r >> 17;
            r ^= r << 5;
            let x = (r & 0xffff) as f32 / 65535.0;
            (i, (d + (x - d) * 0.3).clamp(0.0, 1.0))
        })
        .collect()
}

fn run_daw(name: &str, salt: u32, inp: &[f32]) -> Vec<f32> {
    let mut s = ling_daw::model::EffectSlot::new(name, SR).expect(name);
    for (i, v) in knobs(name, salt) {
        s.set(i, v);
    }
    let mut out = Vec::with_capacity(inp.len());
    let mut beat = 0.0f64;
    for (k, blk) in inp.chunks(1024).enumerate() {
        let mut b = blk.to_vec();
        s.fx.set_transport(120.0, beat);
        s.fx.set_keys(&[57, 61, 64], beat, beat + 1.0);
        s.fx.set_triggers(if k % 23 == 0 { &[(3, 1.0)] } else { &[] });
        s.process(&mut b);
        beat += (blk.len() / 2) as f64 / SR as f64 * 2.0;
        out.extend(b);
    }
    out
}

fn run_os(name: &str, salt: u32, inp: &[f32]) -> Vec<f32> {
    let mut s = ling_fxcore::Slot::new(name, SR).expect(name);
    for (i, v) in knobs(name, salt) {
        s.set(i, v);
    }
    let mut out = Vec::with_capacity(inp.len());
    let mut beat = 0.0f64;
    for (k, blk) in inp.chunks(1024).enumerate() {
        let mut b = blk.to_vec();
        s.fx.set_transport(120.0, beat);
        s.fx.set_keys(&[57, 61, 64], beat, beat + 1.0);
        s.fx.set_triggers(if k % 23 == 0 { &[(3, 1.0)] } else { &[] });
        s.process(&mut b);
        beat += (blk.len() / 2) as f64 / SR as f64 * 2.0;
        out.extend(b);
    }
    out
}

fn rms(x: &[f32]) -> f64 {
    (x.iter().map(|v| (*v as f64) * (*v as f64)).sum::<f64>() / x.len().max(1) as f64).sqrt()
}

#[test]
fn every_effect_sounds_the_same_as_on_windows() {
    assert_eq!(ling_fxcore::EFFECT_NAMES, ling_daw::dsp::fx::EFFECT_NAMES, "the same effect list");
    let inp = input();
    let mut bad = vec![];
    println!("{:<20} {:>10} {:>10} {:>10} {:>9}", "effect", "rms in", "rms out", "rms diff", "rel");
    for name in ling_fxcore::EFFECT_NAMES {
        assert_eq!(ling_fxcore::defaults(name), ling_daw::dsp::fx::defaults(name), "{name}: knobs + defaults");
        assert_eq!(ling_fxcore::presets(name).len(), ling_daw::dsp::fx::presets(name).len(), "{name}: presets");
        for salt in [1u32, 7] {
            let a = run_daw(name, salt, &inp);
            let b = run_os(name, salt, &inp);
            assert_eq!(a.len(), b.len());
            let diff: Vec<f32> = a.iter().zip(&b).map(|(x, y)| x - y).collect();
            let (ra, rd) = (rms(&a), rms(&diff));
            let rel = rd / ra.max(1e-9);
            if salt == 1 {
                println!("{:<20} {:>10.5} {:>10.5} {:>10.2e} {:>9.2e}", name, rms(&inp), ra, rd, rel);
            }
            // libm and the platform maths differ in the last bits; feedback effects can carry that a little further
            if rel > 2e-3 {
                bad.push(format!("{name} (salt {salt}): rel diff {rel:.2e}"));
            }
        }
    }
    assert!(bad.is_empty(), "differ from ling-daw: {bad:#?}");
}

#[test]
fn bypass_wet_and_knobs_change_the_sound() {
    let inp = input();
    for name in ["EQ", "Compressor", "Drive", "Delay", "Reverb", "Chorus", "Phaser", "Flanger", "Limiter", "Digitize"] {
        let mut s = ling_fxcore::Slot::new(name, SR).unwrap();
        if name == "EQ" {
            s.set(0, 0.9); // flat at its defaults
        }
        let mut b = inp.clone();
        s.process(&mut b);
        let d: f64 = rms(&b.iter().zip(&inp).map(|(x, y)| x - y).collect::<Vec<_>>());
        assert!(d > 1e-4, "{name} changes the input ({d})");
        // wet 0 = the input untouched
        let mut s = ling_fxcore::Slot::new(name, SR).unwrap();
        s.wet = 0.0;
        let mut b = inp.clone();
        s.process(&mut b);
        assert!(b.iter().zip(&inp).all(|(x, y)| (x - y).abs() < 1e-6), "{name} at wet 0 is dry");
        // a knob at both ends sounds different
        let ends: Vec<Vec<f32>> = [0.05f32, 0.95]
            .iter()
            .map(|v| {
                let mut s = ling_fxcore::Slot::new(name, SR).unwrap();
                s.set(0, *v);
                let mut b = inp.clone();
                s.process(&mut b);
                b
            })
            .collect();
        let d = rms(&ends[0].iter().zip(&ends[1]).map(|(x, y)| x - y).collect::<Vec<_>>());
        assert!(d > 1e-4, "{name}: its first knob matters ({d})");
    }
}
