// (included into g3d.rs)
//
// Post effects the PC engine runs over the finished 3-D frame, ported so a game looks the same on
// LingOS: FXAA-lite (`set_fxaa`, the host's gfx/toon.rs apply_fxaa), the colour-grading LUT
// (`set_color_grade` / `set_color_grade_mix` / `set_color_cycle`, the host's gfx/grade.rs) and
// depth of field (`depth_blur` / `dof`, the host's gfx/raster.rs depth_of_field). The host runs
// FXAA then the grade at the end of its post chain (flush_post / present) over the whole world
// buffer; here they run over the window right after the frame's post pass put the 3-D on it,
// so a half-resolution 3-D layer is smoothed after its upscale. Depth of field is the game's own
// call after flush_post, also over the window, with the 3-D depth buffer for the blur amount.
//
// Everything is spread over the cores in row bands (crate::par) with buffers kept between frames:
// no allocation per frame (the grade's table is built when the look changes).

/// The post effects' state (outside `State`, so they stay one place).
struct Post {
    fxaa: bool,
    /// The grade as built for its look (64^3 packed RGB), and the table in use (the hue cycle
    /// turns `base` into `cur` a few times a second).
    base: Vec<u32>,
    cur: Vec<u32>,
    graded: bool,
    cycle_amount: f32,
    cycle_period: [f32; 3],
    cycle_built_us: u64,
    /// FXAA per-band rows: 4 colour rows (prev, cur, the band's two boundary rows) + 3 luma rows.
    rows: Vec<u32>,
    lrows: Vec<u8>,
    /// Depth of field: two half-resolution planes (packed RGB).
    half_a: Vec<u32>,
    half_b: Vec<u32>,
}

static mut POST: Option<Post> = None;
/// A `flush_post` call is flushing (its post pass runs FXAA + grade at once, before the HUD).
static mut POST_FLUSH_CALL: bool = false;
/// FXAA + grade already ran this frame (at a `flush_post`); else `end_frame` runs them.
static mut POST_RAN_FRAME: bool = false;

fn post_st() -> &'static mut Post {
    let p = unsafe { &mut *(&raw mut POST) };
    p.get_or_insert_with(|| Post {
        fxaa: false,
        base: Vec::new(),
        cur: Vec::new(),
        graded: false,
        cycle_amount: 0.0,
        cycle_period: [61.0, 37.0, 23.0],
        cycle_built_us: 0,
        rows: Vec::new(),
        lrows: Vec::new(),
        half_a: Vec::new(),
        half_b: Vec::new(),
    })
}

const GRADE_N: usize = 64;

/// The builtins handled here (before g3d's own list, which only accepted them).
fn post_dispatch(s: &mut State, name: &[u8], args: &[u64]) -> Option<u64> {
    Some(match name {
        b"set_fxaa" => {
            post_st().fxaa = n(args, 0, 1.0) as i64 != 0;
            UNIT
        },
        b"flush_post" | b"post_now" => {
            unsafe { POST_FLUSH_CALL = true };
            s.flush();
            unsafe { POST_FLUSH_CALL = false };
            UNIT
        },
        b"set_color_grade" => {
            let look = arg_str(args, 0, "off");
            crate::lingrt::boolean(grade_set(&look, &look, 0.0, f(args, 1, 1.0)))
        },
        b"set_color_grade_mix" => {
            let (a, b) = (arg_str(args, 0, "off"), arg_str(args, 1, "off"));
            crate::lingrt::boolean(grade_set(&a, &b, f(args, 2, 0.0), f(args, 3, 1.0)))
        },
        b"set_color_cycle" => {
            let p = post_st();
            p.cycle_amount = f(args, 0, 0.0).clamp(0.0, 180.0) * (core::f32::consts::PI / 180.0);
            p.cycle_period = [f(args, 1, 61.0).max(1.0), f(args, 2, 37.0).max(1.0), f(args, 3, 23.0).max(1.0)];
            p.cycle_built_us = 0;
            if p.cycle_amount <= 0.0 && p.graded {
                let (base, cur) = (&p.base, &mut p.cur);
                cur.copy_from_slice(base);
            }
            UNIT
        },
        b"depth_blur" | b"dof" | b"depth_of_field" => {
            let (focus, range, rad, oil) = (f(args, 0, 30.0), f(args, 1, 60.0), f(args, 2, 3.0).max(0.0) as usize, f(args, 3, 0.0));
            s.sync();
            let t0 = crate::arch::timer::now_us();
            depth_of_field(s, focus, range, rad, oil);
            crate::prof::add(crate::prof::POST, crate::arch::timer::now_us().saturating_sub(t0));
            UNIT
        },
        _ => return None,
    })
}

fn arg_str(args: &[u64], i: usize, d: &str) -> String {
    match args.get(i).map(|&v| str_of(v)) {
        Some(s) if !s.is_empty() => String::from(s),
        _ => String::from(d),
    }
}

// ── the colour grade (host gfx/grade.rs) ─────────────────────────────────────

fn smooth01(x: f32) -> f32 {
    let x = x.clamp(0.0, 1.0);
    x * x * (3.0 - 2.0 * x)
}

/// One colour through a built-in look (0..1 in and out), as the host's `preset_color`.
fn preset_color(name: &str, r: f32, g: f32, b: f32) -> [f32; 3] {
    let lum = 0.2126 * r + 0.7152 * g + 0.0722 * b;
    let sat = |c: [f32; 3], s: f32| {
        let l = 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
        [l + (c[0] - l) * s, l + (c[1] - l) * s, l + (c[2] - l) * s]
    };
    let curve = |x: f32, k: f32| x + (smooth01(x) - x) * k;
    let c = match name {
        "heavy" => {
            let s = sat([r, g, b], 1.38);
            let mut c = [curve(s[0], 0.85), curve(s[1], 0.85), curve(s[2], 0.85)];
            let sh = (1.0 - lum) * (1.0 - lum);
            let hi = lum * lum;
            c[0] += -0.05 * sh + 0.09 * hi;
            c[1] += 0.03 * sh + 0.035 * hi;
            c[2] += 0.08 * sh - 0.07 * hi;
            [c[0] * 0.96 + 0.012, c[1] * 0.96 + 0.016, c[2] * 0.96 + 0.026]
        },
        "warm" => {
            let s = sat([r, g, b], 1.12);
            [curve(s[0], 0.35) * 1.06 + 0.02, curve(s[1], 0.35) * 1.01 + 0.008, curve(s[2], 0.35) * 0.88]
        },
        "cool" => {
            let s = sat([r, g, b], 1.05);
            [curve(s[0], 0.4) * 0.9, curve(s[1], 0.4) * 1.0 + 0.01, curve(s[2], 0.4) * 1.08 + 0.03]
        },
        "vivid" => {
            let s = sat([r, g, b], 1.55);
            [curve(s[0], 0.6), curve(s[1], 0.6), curve(s[2], 0.6)]
        },
        "biolum" => {
            let mx = r.max(g).max(b);
            let mn = r.min(g).min(b);
            let chroma = mx - mn;
            let base = [r * 0.20 + 0.010, g * 0.25 + 0.022, b * 0.40 + 0.065];
            let (glow, gain) = if g >= r && g >= b {
                ([0.10, 1.0, 0.70], 1.0)
            } else if b >= r {
                ([0.18, 0.75, 1.0], 0.85)
            } else {
                ([0.62, 0.16, 0.78], 0.35)
            };
            let k = smooth01((chroma - 0.12) * 2.6) * smooth01((lum - 0.18) * 2.4) * 0.75 * gain;
            let lift = 0.35 + lum * 0.6;
            [base[0] + glow[0] * k * lift, base[1] + glow[1] * k * lift, base[2] + glow[2] * k * lift]
        },
        "noir" => {
            let l = curve(lum, 1.0);
            [l * 1.02, l, l * 0.97]
        },
        _ => [r, g, b],
    };
    [c[0].clamp(0.0, 1.0), c[1].clamp(0.0, 1.0), c[2].clamp(0.0, 1.0)]
}

fn grade_known(n: &str) -> bool {
    matches!(n, "heavy" | "warm" | "cool" | "vivid" | "noir" | "biolum" | "off" | "none" | "")
}

/// Build the table for look `a` blended toward `b` by `t`, at `strength` (the host's set_preset /
/// set_mix). "off" / strength 0 clears it. False for an unknown look (grading then off, as the host).
fn grade_set(a: &str, b: &str, t: f32, strength: f32) -> bool {
    let (a, b) = (a.trim().to_ascii_lowercase(), b.trim().to_ascii_lowercase());
    let p = post_st();
    let off = |n: &str| matches!(n, "off" | "none" | "");
    if !grade_known(&a) || !grade_known(&b) {
        p.graded = false;
        return false;
    }
    // nothing to do: no strength, or "off" alone (a mix toward a look still builds; "off" = identity)
    if strength <= 0.0 || (off(&a) && (off(&b) || t <= 0.0)) {
        p.graded = false;
        return true;
    }
    let (t, s) = (t.clamp(0.0, 1.0), strength.clamp(0.0, 1.0));
    if p.base.len() != GRADE_N * GRADE_N * GRADE_N {
        p.base = vec![0u32; GRADE_N * GRADE_N * GRADE_N];
        p.cur = vec![0u32; GRADE_N * GRADE_N * GRADE_N];
    }
    let m = (GRADE_N - 1) as f32;
    let q = |v: f32| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u32;
    // the 64 red slices over the cores (pure arithmetic: no allocation in the jobs)
    let bp = crate::par::Shared(p.base.as_mut_ptr());
    let (aa, bb) = (a.as_str(), b.as_str());
    crate::par::for_each(GRADE_N, &|ri: usize| {
        for gi in 0..GRADE_N {
            for bi in 0..GRADE_N {
                let (r, g, bl) = (ri as f32 / m, gi as f32 / m, bi as f32 / m);
                let ca = preset_color(aa, r, g, bl);
                let o = if t > 0.0 && aa != bb {
                    let cb = preset_color(bb, r, g, bl);
                    [ca[0] + (cb[0] - ca[0]) * t, ca[1] + (cb[1] - ca[1]) * t, ca[2] + (cb[2] - ca[2]) * t]
                } else {
                    ca
                };
                let v = (q(r + (o[0] - r) * s) << 16) | (q(g + (o[1] - g) * s) << 8) | q(bl + (o[2] - bl) * s);
                unsafe { *bp.get().add((ri << 12) | (gi << 6) | bi) = v };
            }
        }
    });
    let (base, cur) = (&p.base, &mut p.cur);
    cur.copy_from_slice(base);
    p.graded = true;
    p.cycle_built_us = 0;
    true
}

/// The host's hue cycle: the table's outputs turned around the grey axis, each brightness band by an
/// angle that oscillates on its own period. Rebuilt at most every 160 ms.
fn grade_cycle(p: &mut Post) {
    if !p.graded || p.cycle_amount <= 0.0 {
        return;
    }
    let now = crate::arch::timer::now_us();
    if p.cycle_built_us != 0 && now.saturating_sub(p.cycle_built_us) < 160_000 {
        return;
    }
    p.cycle_built_us = now.max(1);
    let secs = now as f32 / 1_000_000.0;
    let tau = core::f32::consts::TAU;
    let ang = [0usize, 1, 2].map(|i| p.cycle_amount * libm::sinf(secs * tau / p.cycle_period[i] + i as f32 * 2.1));
    let src = crate::par::Shared(p.base.as_ptr() as *mut u32);
    let dst = crate::par::Shared(p.cur.as_mut_ptr());
    crate::par::for_each(GRADE_N, &|ri: usize| {
        for k in ri * GRADE_N * GRADE_N..(ri + 1) * GRADE_N * GRADE_N {
            let c = unsafe { *src.get().add(k) };
            let (r, g, b) = (((c >> 16) & 255) as f32, ((c >> 8) & 255) as f32, (c & 255) as f32);
            let l = (0.2126 * r + 0.7152 * g + 0.0722 * b) / 255.0;
            let ws = (1.0 - l * 2.0).clamp(0.0, 1.0);
            let wh = (l * 2.0 - 1.0).clamp(0.0, 1.0);
            let a = ang[0] * ws + ang[1] * (1.0 - ws - wh) + ang[2] * wh;
            let (sn, co) = (libm::sinf(a), libm::cosf(a));
            let k1 = (1.0 - co) / 3.0;
            let k2 = sn * 0.577_350_3;
            let (h0, h1, h2) = (co + k1, k1 - k2, k1 + k2);
            let q = |v: f32| v.clamp(0.0, 255.0) as u32;
            let v = (q(r * h0 + g * h1 + b * h2) << 16) | (q(r * h2 + g * h0 + b * h1) << 8) | q(r * h1 + g * h2 + b * h0);
            unsafe { *dst.get().add(k) = v };
        }
    });
}

#[inline]
fn grade_px(lut: &[u32], p: u32) -> u32 {
    // emissive pixels (near white, or a channel at full drive) keep their colour, as the host
    let (r, g, b) = ((p >> 16) & 255, (p >> 8) & 255, p & 255);
    if (r >= 220 && g >= 220 && b >= 220) || r.max(g).max(b) >= 250 {
        return p;
    }
    let i = (((p >> 18) & 63) << 12) | (((p >> 10) & 63) << 6) | ((p >> 2) & 63);
    (p & 0xFF00_0000) | lut[i as usize]
}

// ── FXAA-lite (host gfx/toon.rs apply_fxaa) ──────────────────────────────────

#[inline]
fn luma8(p: u32) -> u8 {
    ((((p >> 16) & 0xFF) * 77 + ((p >> 8) & 0xFF) * 150 + (p & 0xFF) * 29) >> 8) as u8
}

/// FXAA over rows y0..y1 of `buf` (w x h): every row reads the ORIGINAL rows around it, as the host's
/// snapshot does, from per-band copies (a band's outer neighbours are copied before any band runs).
fn fxaa_rows(p: &mut Post, buf: &mut [u32], w: usize, h: usize, x0: usize, x1: usize, y0: usize, y1: usize) {
    let (y0, y1) = (y0.max(1), y1.min(h - 1));
    if w < 3 || y0 >= y1 {
        return;
    }
    let (x0, x1) = (x0.max(1), x1.min(w - 1));
    let (chunks, per) = crate::par::row_chunks(y1 - y0, 16);
    if p.rows.len() < chunks * 4 * w {
        p.rows = vec![0u32; chunks * 4 * w];
        p.lrows = vec![0u8; chunks * 3 * w];
    }
    // the rows just outside each band, as they are now (other bands write them meanwhile)
    for c in 0..chunks {
        let (b0, b1) = (y0 + c * per, (y0 + (c + 1) * per).min(y1));
        let r = &mut p.rows[c * 4 * w..(c * 4 + 4) * w];
        r[2 * w..3 * w].copy_from_slice(&buf[(b0 - 1) * w..b0 * w]);
        r[3 * w..4 * w].copy_from_slice(&buf[b1 * w..(b1 + 1) * w]);
    }
    let bp = crate::par::Shared(buf.as_mut_ptr());
    let rp = crate::par::Shared(p.rows.as_mut_ptr());
    let lp = crate::par::Shared(p.lrows.as_mut_ptr());
    crate::par::for_each(chunks, &|c: usize| {
        let (b0, b1) = (y0 + c * per, (y0 + (c + 1) * per).min(y1));
        if b0 >= b1 {
            return;
        }
        let rows = unsafe { core::slice::from_raw_parts_mut(rp.get().add(c * 4 * w), 4 * w) };
        let lum = unsafe { core::slice::from_raw_parts_mut(lp.get().add(c * 3 * w), 3 * w) };
        let row = |y: usize| unsafe { core::slice::from_raw_parts_mut(bp.get().add(y * w), w) };
        // rows[0..w] = original y-1, rows[w..2w] = original y; lum: 3 rotating rows (y-1, y, y+1)
        let (prev, rest) = rows.split_at_mut(w);
        let (cur, rest) = rest.split_at_mut(w);
        let (above, below) = rest.split_at_mut(w);
        prev.copy_from_slice(above);
        cur.copy_from_slice(row(b0));
        let (lp0, lrest) = lum.split_at_mut(w);
        let (lc0, ln0) = lrest.split_at_mut(w);
        let (mut lpv, mut lcv, mut lnv) = (lp0, lc0, ln0);
        for x in 0..w {
            lpv[x] = luma8(prev[x]);
            lcv[x] = luma8(cur[x]);
        }
        for y in b0..b1 {
            // the row below, as it was: the next row of this band is untouched yet; the band's last
            // row reads the copy taken before the bands ran
            let next: &[u32] = if y + 1 < b1 { row(y + 1) } else { &*below };
            for x in 0..w {
                lnv[x] = luma8(next[x]);
            }
            let out = row(y);
            for x in x0..x1 {
                let l = lcv[x] as i32;
                let d = (l - lpv[x] as i32).abs().max((l - lnv[x] as i32).abs()).max((l - lcv[x - 1] as i32).abs()).max((l - lcv[x + 1] as i32).abs());
                if d < 24 {
                    continue;
                }
                let k = (0.25 + ((d - 24) as f32 / 90.0).clamp(0.0, 1.0) * 0.20).min(0.45);
                let ki = (k * 256.0) as u32;
                let inv = 256 - ki;
                let (c, pn, ps, pw, pe) = (cur[x], prev[x], next[x], cur[x - 1], cur[x + 1]);
                let avg = |sh: u32| (((pn >> sh) & 0xFF) + ((ps >> sh) & 0xFF) + ((pw >> sh) & 0xFF) + ((pe >> sh) & 0xFF)) / 4;
                let r = (((c >> 16) & 0xFF) * inv + avg(16) * ki) >> 8;
                let g = (((c >> 8) & 0xFF) * inv + avg(8) * ki) >> 8;
                let b = ((c & 0xFF) * inv + avg(0) * ki) >> 8;
                out[x] = (c & 0xFF00_0000) | (r << 16) | (g << 8) | b;
            }
            // slide the window down one row
            if y + 1 < b1 {
                prev.copy_from_slice(cur);
                cur.copy_from_slice(next);
                let t = lpv;
                lpv = lcv;
                lcv = lnv;
                lnv = t;
            }
        }
    });
}

/// After the frame's post pass put the 3-D on the window: FXAA, then the grade (the host's order),
/// over the window or the clip rect a portrait well uses.
fn window_post(clip: Option<[i32; 4]>) {
    let p = post_st();
    if !p.fxaa && !p.graded {
        return;
    }
    let t0 = crate::arch::timer::now_us();
    let Some((win, w, h)) = crate::lingapp::surface() else { return };
    let (x0, y0, x1, y1) = match clip {
        Some([x, y, cw, ch]) => (x.max(0) as usize, y.max(0) as usize, ((x + cw).max(0) as usize).min(w), ((y + ch).max(0) as usize).min(h)),
        None => (0, 0, w, h),
    };
    if x0 >= x1 || y0 >= y1 || win.len() < w * h {
        return;
    }
    if p.fxaa {
        fxaa_rows(p, win, w, h, x0, x1, y0, y1);
    }
    if p.graded {
        grade_cycle(p);
        let (chunks, per) = crate::par::row_chunks(y1 - y0, 16);
        let bp = crate::par::Shared(win.as_mut_ptr());
        let lut = &p.cur;
        crate::par::for_each(chunks, &|c: usize| {
            for y in y0 + c * per..(y0 + (c + 1) * per).min(y1) {
                let row = unsafe { core::slice::from_raw_parts_mut(bp.get().add(y * w + x0), x1 - x0) };
                for px in row.iter_mut() {
                    *px = grade_px(lut, *px);
                }
            }
        });
    }
    crate::prof::add(crate::prof::POST, crate::arch::timer::now_us().saturating_sub(t0));
}

// ── depth of field (host gfx/raster.rs depth_of_field) ──────────────────────

#[inline]
fn unpack3(p: u32) -> [u32; 3] {
    [(p >> 16) & 255, (p >> 8) & 255, p & 255]
}

/// `depth_blur(focus, range, radius, oil)` over the window: sharp at camera depth `focus`, blurred up
/// to `radius` px as the depth departs by `range`; where nothing 3-D was drawn, fully blurred (the
/// host's background). The blur is built at half resolution (box filter, separable), as the host's.
fn depth_of_field(s: &mut State, focus: f32, range: f32, rad: usize, oil: f32) {
    let Some((win, w, h)) = crate::lingapp::surface() else { return };
    let (zw, zh) = (s.zw, s.zh);
    if w < 4 || h < 4 || rad == 0 || zw == 0 || zh == 0 || s.zbuf.len() < zw * zh || win.len() < w * h {
        return;
    }
    let range = range.max(1e-3);
    let oil = oil.clamp(0.0, 1.0);
    let zd = s.cam.zdist;
    let (hw, hh) = (w.div_ceil(2), h.div_ceil(2));
    let p = post_st();
    if p.half_a.len() != hw * hh {
        p.half_a = vec![0u32; hw * hh];
        p.half_b = vec![0u32; hw * hh];
    }
    let r_h = rad.div_ceil(2).max(1) as i32;
    let win_n = (2 * r_h + 1) as u32;
    let src = crate::par::Shared(win.as_mut_ptr());
    let pa = crate::par::Shared(p.half_a.as_mut_ptr());
    let pb = crate::par::Shared(p.half_b.as_mut_ptr());
    let (chunks, per) = crate::par::row_chunks(hh, 8);
    // 1. 2x2 average down
    crate::par::for_each(chunks, &|c: usize| {
        for hy in c * per..((c + 1) * per).min(hh) {
            let (y0, y1) = ((hy * 2).min(h - 1), (hy * 2 + 1).min(h - 1));
            for hx in 0..hw {
                let (x0, x1) = ((hx * 2).min(w - 1), (hx * 2 + 1).min(w - 1));
                let mut acc = [0u32; 3];
                for i in [y0 * w + x0, y0 * w + x1, y1 * w + x0, y1 * w + x1] {
                    let v = unpack3(unsafe { *src.get().add(i) });
                    acc[0] += v[0];
                    acc[1] += v[1];
                    acc[2] += v[2];
                }
                unsafe { *pa.get().add(hy * hw + hx) = ((acc[0] / 4) << 16) | ((acc[1] / 4) << 8) | (acc[2] / 4) };
            }
        }
    });
    // 2. horizontal box (a -> b), 3. vertical box (b -> a)
    let hwi = hw as i32;
    crate::par::for_each(chunks, &|c: usize| {
        for y in c * per..((c + 1) * per).min(hh) {
            let row = unsafe { core::slice::from_raw_parts(pa.get().add(y * hw), hw) };
            let out = unsafe { core::slice::from_raw_parts_mut(pb.get().add(y * hw), hw) };
            let mut acc = [0u32; 3];
            for k in -r_h..=r_h {
                let v = unpack3(row[k.clamp(0, hwi - 1) as usize]);
                acc[0] += v[0];
                acc[1] += v[1];
                acc[2] += v[2];
            }
            for x in 0..hw {
                out[x] = ((acc[0] / win_n) << 16) | ((acc[1] / win_n) << 8) | (acc[2] / win_n);
                let lo = unpack3(row[(x as i32 - r_h).clamp(0, hwi - 1) as usize]);
                let hi = unpack3(row[(x as i32 + r_h + 1).clamp(0, hwi - 1) as usize]);
                for ch in 0..3 {
                    acc[ch] = acc[ch] + hi[ch] - lo[ch];
                }
            }
        }
    });
    crate::par::for_each(chunks, &|c: usize| {
        for y in c * per..((c + 1) * per).min(hh) {
            let out = unsafe { core::slice::from_raw_parts_mut(pa.get().add(y * hw), hw) };
            for x in 0..hw {
                let mut acc = [0u32; 3];
                for k in -r_h..=r_h {
                    let yy = (y as i32 + k).clamp(0, hh as i32 - 1) as usize;
                    let v = unpack3(unsafe { *pb.get().add(yy * hw + x) });
                    acc[0] += v[0];
                    acc[1] += v[1];
                    acc[2] += v[2];
                }
                out[x] = ((acc[0] / win_n) << 16) | ((acc[1] / win_n) << 8) | (acc[2] / win_n);
            }
        }
    });
    // 4. composite: sharp -> blurred by the circle of confusion from the 3-D depth
    let zp = crate::par::Shared(s.zbuf.as_ptr() as *mut f32);
    let (wchunks, wper) = crate::par::row_chunks(h, 16);
    crate::par::for_each(wchunks, &|c: usize| {
        let samp = |ch: u32, fx: f32, fy: f32| -> f32 {
            let gx = (fx * 0.5).min(hw as f32 - 1.0).max(0.0);
            let gy = (fy * 0.5).min(hh as f32 - 1.0).max(0.0);
            let (x0, y0) = (gx as usize, gy as usize);
            let (x1, y1) = ((x0 + 1).min(hw - 1), (y0 + 1).min(hh - 1));
            let (tx, ty) = (gx - x0 as f32, gy - y0 as f32);
            let at = |x: usize, y: usize| ((unsafe { *pa.get().add(y * hw + x) } >> ch) & 255) as f32;
            let a = at(x0, y0) * (1.0 - tx) + at(x1, y0) * tx;
            let b = at(x0, y1) * (1.0 - tx) + at(x1, y1) * tx;
            a * (1.0 - ty) + b * ty
        };
        for y in c * wper..((c + 1) * wper).min(h) {
            let zy = (y * zh / h).min(zh - 1);
            let fy = y as f32;
            for x in 0..w {
                let zx = (x * zw / w).min(zw - 1);
                let iw = unsafe { *zp.get().add(zy * zw + zx) };
                // the kernel keeps 1/(z + zdist) (0 = nothing drawn); the host's depth is z itself
                let coc = if iw > 1e-9 { ((1.0 / iw - zd - focus).abs() / range).clamp(0.0, 1.0) } else { 1.0 };
                if coc <= 0.003 {
                    continue;
                }
                let px = unsafe { &mut *src.get().add(y * w + x) };
                let pv = *px;
                let sh = 1.0 - coc;
                let (pr, pg, pbv) = (((pv >> 16) & 255) as f32, ((pv >> 8) & 255) as f32, (pv & 255) as f32);
                let fx = x as f32;
                let (mut r, mut g, mut b);
                if oil > 0.0 {
                    let off = coc * oil * 3.0;
                    r = pr * sh + samp(16, fx - off, fy) * coc;
                    g = pg * sh + samp(8, fx, fy) * coc;
                    b = pbv * sh + samp(0, fx + off, fy) * coc;
                    let k = oil * coc * 0.22;
                    let (r0, g0, b0) = (r, g, b);
                    r = r0 * (1.0 - k) + b0 * k;
                    g = g0 * (1.0 - k) + r0 * k;
                    b = b0 * (1.0 - k) + g0 * k;
                } else {
                    r = pr * sh + samp(16, fx, fy) * coc;
                    g = pg * sh + samp(8, fx, fy) * coc;
                    b = pbv * sh + samp(0, fx, fy) * coc;
                }
                let q = |v: f32| (v + 0.5).clamp(0.0, 255.0) as u32;
                *px = (pv & 0xFF00_0000) | (q(r) << 16) | (q(g) << 8) | q(b);
            }
        }
    });
}
