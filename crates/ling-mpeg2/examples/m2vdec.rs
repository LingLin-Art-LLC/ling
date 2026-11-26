//! Decode an MPEG-1/2 elementary stream (.m2v / .m1v / .mpv), print the speed, optionally
//! write the decoded frames as raw yuv420p (display order).
//!
//! usage: m2vdec <in.m2v> [out.yuv] [--rgb] [--deint] [--ppm <frame> <out.ppm>] [--repeat N]
//!               [--group N]
//!
//! --rgb / --deint also time `frame_to_rgba` (with / without deinterlacing);
//! --ppm writes one frame as a (deinterlaced if --deint) PPM;
//! --group N passes N picture units per decode() call (e.g. both fields of a frame).

use ling_mpeg2::{frame_to_rgba, Decoder, Frame};
use std::io::Write;
use std::time::Instant;

/// Split an elementary stream into picture units: a unit starts at a sequence header, GOP
/// header or picture start code that follows a picture already in the current unit.
fn split(es: &[u8]) -> Vec<&[u8]> {
    let mut units = Vec::new();
    let mut start = 0;
    let mut have_pic = false;
    let mut i = 0;
    while i + 3 < es.len() {
        if es[i] == 0 && es[i + 1] == 0 && es[i + 2] == 1 {
            let c = es[i + 3];
            if matches!(c, 0x00 | 0xB3 | 0xB8) && have_pic {
                units.push(&es[start..i]);
                start = i;
                have_pic = false;
            }
            if c == 0x00 {
                have_pic = true;
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    units.push(&es[start..]);
    units
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: m2vdec <in.m2v> [out.yuv] [--rgb] [--deint] [--ppm N out.ppm] [--repeat N] [--group N]");
        std::process::exit(2);
    }
    let es = std::fs::read(&args[1]).expect("read input");
    let mut out_path = None;
    let mut rgb = false;
    let mut deint = false;
    let mut ppm: Option<(usize, String)> = None;
    let mut repeat = 1;
    let mut group = 1usize;
    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "--rgb" => rgb = true,
            "--deint" => {
                rgb = true;
                deint = true
            }
            "--ppm" => {
                ppm = Some((args[i + 1].parse().unwrap(), args[i + 2].clone()));
                i += 2;
            }
            "--group" => {
                group = args[i + 1].parse::<usize>().unwrap().max(1);
                i += 1;
            }
            "--repeat" => {
                repeat = args[i + 1].parse().unwrap();
                i += 1;
            }
            p => out_path = Some(p.to_string()),
        }
        i += 1;
    }
    let pics = split(&es);
    let units: Vec<&[u8]> = pics
        .chunks(group)
        .map(|c| {
            let s = c[0].as_ptr() as usize - es.as_ptr() as usize;
            let l = c.iter().map(|u| u.len()).sum::<usize>();
            &es[s..s + l]
        })
        .collect();
    let mut out = out_path.map(|p| std::io::BufWriter::new(std::fs::File::create(p).unwrap()));
    let mut rgbbuf: Vec<u32> = Vec::new();
    let mut rgb_time = 0f64;
    let mut nframes = 0usize;
    let mut keys = 0usize;
    let mut first: Option<Frame> = None;
    let t0 = Instant::now();
    for rep in 0..repeat {
        let mut dec = Decoder::new();
        let mut handle = |f: Frame, nframes: &mut usize| {
            if rep == 0 {
                if let Some(o) = out.as_mut() {
                    o.write_all(&f.y).unwrap();
                    o.write_all(&f.u).unwrap();
                    o.write_all(&f.v).unwrap();
                }
                if let Some((n, path)) = &ppm {
                    if *n == *nframes {
                        let mut px = vec![0u32; (f.width * f.height) as usize];
                        frame_to_rgba(&f, &mut px, deint);
                        let mut b = format!("P6\n{} {}\n255\n", f.width, f.height).into_bytes();
                        for p in px {
                            b.extend_from_slice(&[(p >> 16) as u8, (p >> 8) as u8, p as u8]);
                        }
                        std::fs::write(path, b).unwrap();
                    }
                }
                if first.is_none() {
                    first = Some(f.clone());
                }
            }
            if rgb {
                rgbbuf.resize((f.width * f.height) as usize, 0);
                let t = Instant::now();
                frame_to_rgba(&f, &mut rgbbuf, deint);
                rgb_time += t.elapsed().as_secs_f64();
            }
            if f.key {
                keys += 1;
            }
            *nframes += 1;
        };
        for (k, u) in units.iter().enumerate() {
            if let Err(e) = dec.decode(u, k as i64) {
                eprintln!("unit {k}: {e:?}");
            }
            while let Some(f) = dec.next_frame() {
                handle(f, &mut nframes);
            }
        }
        dec.flush();
        while let Some(f) = dec.next_frame() {
            handle(f, &mut nframes);
        }
    }
    let dt = t0.elapsed().as_secs_f64() - rgb_time;
    if let Some(f) = &first {
        println!(
            "{}x{} progressive={} tff={} dar={}:{}",
            f.width, f.height, f.progressive, f.top_field_first, f.dar.0, f.dar.1
        );
    }
    println!(
        "{} frames ({} key) in {:.3} s decode = {:.1} fps",
        nframes,
        keys,
        dt,
        nframes as f64 / dt
    );
    if rgb {
        println!(
            "frame_to_rgba(deinterlace={}): {:.3} ms/frame = {:.0} fps",
            deint,
            rgb_time * 1000.0 / nframes as f64,
            nframes as f64 / rgb_time
        );
    }
}
