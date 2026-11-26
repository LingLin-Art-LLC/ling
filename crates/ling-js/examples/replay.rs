// Replay a page's scripts captured by an embedder: cargo run --release --example replay -- DIR
// DIR/index.txt has one "file<TAB>url" line per script in the order the engine got them:
// script-*.js run as classic scripts, module-*.js as modules (eval_module with that URL), and
// import-*.js answer HostReq::Import for their URL. Page objects are not offered: reads answer
// "not mine", writes become expandos, host calls throw. Prints the console and uncaught errors.
use ling_js::{Engine, HostReq, Status, Value};
use std::collections::HashMap;

fn drive(e: &mut Engine, mut st: Status, imports: &HashMap<String, String>) -> Status {
    loop {
        st = match st {
            Status::Host(HostReq::Import { url }) => {
                let u = ling_js::strs::to_rust(&url);
                match imports.get(&u) {
                    Some(src) => e.resume(Value::str(src)),
                    None => {
                        let ex = e.type_error(&format!("not captured: {}", u));
                        e.resume_throw(ex)
                    },
                }
            },
            Status::Host(HostReq::Get { .. }) => e.resume(Value::Empty),
            Status::Host(HostReq::Set { .. }) => e.resume(Value::Bool(false)),
            Status::Host(HostReq::Call { name, .. }) => {
                let ex = e.type_error(&format!("{} is not available", ling_js::strs::to_rust(&name)));
                e.resume_throw(ex)
            },
            other => return other,
        };
    }
}

fn main() {
    let dir = std::env::args().nth(1).expect("usage: replay DIR");
    let index = std::fs::read_to_string(format!("{}/index.txt", dir)).expect("no index.txt");
    let entries: Vec<(String, String)> = index
        .lines()
        .filter_map(|l| {
            let mut p = l.splitn(2, '\t');
            Some((p.next()?.trim().to_string(), p.next().unwrap_or("").trim().to_string()))
        })
        .collect();
    let mut imports = HashMap::new();
    for (f, url) in &entries {
        if f.starts_with("import-") {
            if let Ok(src) = std::fs::read_to_string(format!("{}/{}", dir, f)) {
                imports.insert(url.clone(), src);
            }
        }
    }
    let mut e = Engine::new();
    e.budget = 100_000_000;
    e.trace_throws = std::env::var("TRACE_THROWS").is_ok();
    let g = e.global();
    for name in ["window", "self", "top", "parent"] {
        e.define(g, name, Value::Obj(g), 7);
    }
    // an optional page stand-in (REPLAY_STUB=file.js), run first
    if let Ok(p) = std::env::var("REPLAY_STUB") {
        if let Ok(src) = std::fs::read_to_string(&p) {
            let st = e.eval(&src);
            if let Status::Error(v) = st {
                println!("stub failed: {}", e.display(&v));
            }
        }
    }
    for (f, url) in &entries {
        if f.starts_with("import-") {
            continue;
        }
        let src = match std::fs::read_to_string(format!("{}/{}", dir, f)) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let t = std::time::Instant::now();
        let st = if f.starts_with("module-") {
            e.base_url = url.clone();
            e.eval_module(&src, url)
        } else {
            e.eval(&src)
        };
        let st = drive(&mut e, st, &imports);
        let st = match st {
            Status::Done(v) => {
                let j = e.run_jobs();
                match drive(&mut e, j, &imports) {
                    Status::Done(_) => Status::Done(v),
                    other => other,
                }
            },
            other => other,
        };
        let out = match st {
            Status::Done(_) => "ok".to_string(),
            Status::Error(v) => {
                let m = e.display(&v);
                let stack = match &v {
                    Value::Obj(_) => match e.get_str(&v, "stack") {
                        Ok(Value::Str(s)) => ling_js::strs::to_rust(&s).lines().skip(1).take(3).map(|l| l.trim().to_string()).collect::<Vec<_>>().join(" < "),
                        _ => String::new(),
                    },
                    _ => String::new(),
                };
                format!("UNCAUGHT {} ({})", m, stack)
            },
            Status::Host(r) => format!("host request: {:?}", r),
        };
        println!("{} ({} ms): {}", f, t.elapsed().as_millis(), out);
        for l in e.log.drain(..) {
            println!("  console: {}", l.chars().take(300).collect::<String>());
        }
    }
}
