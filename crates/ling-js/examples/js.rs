// Run scripts in one engine, in order (each file is one <script>): cargo run --example js -- a.js b.js
// A single argument that is not a file runs as code. A `.mjs` file runs as a module; its imports
// are read from disk (file:/// URLs). The microtasks a script queues run after it.
use ling_js::{Engine, HostReq, Status, Value};

fn file_url(path: &str) -> String {
    let abs = std::fs::canonicalize(path).map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| path.to_string());
    let abs = abs.trim_start_matches(r"\\?\").replace('\\', "/");
    if abs.starts_with('/') { format!("file://{}", abs) } else { format!("file:///{}", abs) }
}

fn url_path(url: &str) -> String {
    let p = url.trim_start_matches("file://");
    // file:///C:/x -> C:/x
    if p.len() > 3 && p.as_bytes()[0] == b'/' && p.as_bytes()[2] == b':' { p[1..].to_string() } else { p.to_string() }
}

/// answer the host requests a run makes (module fetches from disk; nothing else is offered)
fn drive(e: &mut Engine, mut st: Status) -> Status {
    loop {
        st = match st {
            Status::Host(HostReq::Import { url }) => {
                let u = ling_js::strs::to_rust(&url);
                match std::fs::read_to_string(url_path(&u)) {
                    Ok(src) => e.resume(Value::str(&src)),
                    Err(err) => {
                        let ex = e.type_error(&format!("cannot load {}: {}", u, err));
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
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut e = Engine::new();
    e.budget = 500_000_000;
    e.trace_throws = std::env::var("TRACE_THROWS").is_ok();
    for a in &args {
        let (name, src) = match std::fs::read_to_string(a) {
            Ok(s) => (std::path::Path::new(a).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(), s),
            Err(_) => ("<code>".to_string(), a.clone()),
        };
        let t = std::time::Instant::now();
        let st = if a.ends_with(".mjs") {
            let url = file_url(a);
            e.base_url = url.clone();
            e.eval_module(&src, &url)
        } else {
            e.eval(&src)
        };
        let st = drive(&mut e, st);
        let steps = e.steps;
        // then the microtasks the script queued (promise reactions)
        let st = match st {
            Status::Done(v) => {
                let j = e.run_jobs();
                match drive(&mut e, j) {
                    Status::Done(_) => Status::Done(v),
                    other => other,
                }
            },
            other => other,
        };
        let out = match st {
            Status::Done(v) => match &v {
                Value::Str(s) => format!("= {}", ling_js::strs::to_rust(s).chars().take(200).collect::<String>()),
                _ => format!("= {}", e.display(&v).chars().take(200).collect::<String>()),
            },
            Status::Error(v) => {
                let msg = e.display(&v);
                let stack = match &v {
                    Value::Obj(_) => match e.get_str(&v, "stack") {
                        Ok(Value::Str(s)) => ling_js::strs::to_rust(&s),
                        _ => String::new(),
                    },
                    _ => String::new(),
                };
                format!("UNCAUGHT {}\n{}", msg, stack.lines().skip(1).take(4).collect::<Vec<_>>().join("\n"))
            },
            Status::Host(r) => format!("host request: {:?}", r),
        };
        for l in e.log.drain(..) {
            println!("  console: {}", l.chars().take(200).collect::<String>());
        }
        println!("{} ({} ms, {} steps, {} live): {}", name, t.elapsed().as_millis(), steps, e.heap.live, out);
    }
}
