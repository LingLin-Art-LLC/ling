//! `lingjs_*`: LingJS (crates/ling-js), Ling's own JavaScript engine, for Ling programs — Horizon
//! runs a page's scripts with it.
//!
//! Nothing here calls back into Ling (that would deadlock under the JIT). A script that needs the
//! page calls the global `__host(op, ...args)`: the engine suspends and the builtin returns
//! `["host", op, [args]]`; the program does the work and calls `lingjs_resume(h, answer)`, and so
//! on until `["done", value]` or `["error", message]`.
//!
//!   lingjs_new()                     -> handle (a fresh engine: no network, no storage, no eval)
//!   lingjs_eval(h, src, steps)       -> status   (run a script with an instruction allowance)
//!   lingjs_call(h, name, args, steps)-> status   (call the global function `name`)
//!   lingjs_resume(h, answer)         -> status   (answer the pending `__host` call)
//!   lingjs_throw(h, message)         -> status   (fail it: throws an Error into the script)
//!   lingjs_jobs(h, steps)            -> status   (run queued promise jobs)
//!   lingjs_log(h)                    -> [lines]  (console output since the last call)
//!   lingjs_module(h, src, url, steps) -> status (run an ES module; its imports come back as
//!                                     ["import", url]: answer with lingjs_resume(h, source))
//!   lingjs_base(h, url)                 (the page address classic scripts' import() resolves against)
//!   lingjs_check(src)                -> ""  or the syntax error
//!   lingjs_abort(h) / lingjs_free(h)
//!
//! Values cross as Ling values: numbers, strings, booleans, lists (JS arrays, nested); undefined
//! and null become the unit value; any other object crosses as the string "[object]". Only the
//! data the program asked for leaves the engine.

use super::Value;
use ling_js::{Engine, HostReq, Status, Value as JV};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

pub const NAMES: &[&str] = &[
    "lingjs_new",
    "lingjs_eval",
    "lingjs_call",
    "lingjs_resume",
    "lingjs_throw",
    "lingjs_jobs",
    "lingjs_log",
    "lingjs_check",
    "lingjs_abort",
    "lingjs_free",
    "lingjs_steps",
    "lingjs_module",
    "lingjs_base",
];

pub fn handles(name: &str) -> bool {
    NAMES.contains(&name)
}

struct Ctx {
    e: Engine,
    /// Get / Set requests on host objects never reach the program: none are made by Horizon,
    /// and a stray one is answered "not mine"
    _host: ling_js::ObjRef,
}

thread_local! {
    static ENGINES: RefCell<HashMap<u64, Box<Ctx>>> = RefCell::new(HashMap::new());
    static NEXT: RefCell<u64> = const { RefCell::new(1) };
}

fn num(args: &[Value], i: usize) -> f64 {
    match args.get(i) {
        Some(Value::Number(n)) => *n,
        Some(Value::Str(s)) => s.trim().parse().unwrap_or(0.0),
        Some(Value::Bool(b)) => *b as u8 as f64,
        _ => 0.0,
    }
}

fn text(args: &[Value], i: usize) -> String {
    match args.get(i) {
        Some(Value::Str(s)) => s.clone(),
        Some(v) => v.to_string(),
        None => String::new(),
    }
}

fn list(items: Vec<Value>) -> Value {
    Value::List(Rc::new(items))
}

fn s(t: &str) -> Value {
    Value::Str(t.to_string())
}

/// Ling -> JS
fn to_js(e: &mut Engine, v: &Value, depth: u32) -> JV {
    match v {
        Value::Number(n) => JV::Num(*n),
        Value::Str(t) => JV::str(t),
        Value::Bool(b) => JV::Bool(*b),
        Value::List(l) if depth < 64 => {
            let items: Vec<JV> = l.iter().map(|x| to_js(e, x, depth + 1)).collect();
            JV::Obj(e.new_array(items))
        }
        _ => JV::Undefined,
    }
}

/// JS -> Ling
fn to_ling(e: &mut Engine, v: &JV, depth: u32) -> Value {
    match v {
        JV::Undefined | JV::Null | JV::Empty => Value::Unit,
        JV::Bool(b) => Value::Bool(*b),
        JV::Num(n) => Value::Number(*n),
        JV::Str(t) => Value::Str(ling_js::strs::to_rust(t)),
        JV::Obj(o) => {
            let items = match &e.heap.get(*o).kind {
                ling_js::object::Kind::Array(items) if depth < 64 => Some(items.clone()),
                _ => None,
            };
            match items {
                Some(items) => list(items.iter().map(|x| to_ling(e, x, depth + 1)).collect()),
                None => s("[object]"),
            }
        }
    }
}

fn error_text(e: &mut Engine, v: &JV) -> String {
    let msg = e.display(v);
    let stack = match v {
        JV::Obj(_) => match e.get_str(v, "stack") {
            Ok(JV::Str(st)) => ling_js::strs::to_rust(&st),
            _ => String::new(),
        },
        _ => String::new(),
    };
    let at: Vec<&str> = stack.lines().skip(1).take(3).map(|l| l.trim()).collect();
    if at.is_empty() { msg } else { format!("{} ({})", msg, at.join(" < ")) }
}

/// a run's end, for the program; host Get / Set requests are answered here (never the program's)
fn status_out(c: &mut Ctx, mut st: Status) -> Value {
    loop {
        match st {
            Status::Done(v) => {
                let out = to_ling(&mut c.e, &v, 0);
                return list(vec![s("done"), out]);
            }
            Status::Error(v) => {
                let msg = error_text(&mut c.e, &v);
                return list(vec![s("error"), Value::Str(msg)]);
            }
            Status::Host(HostReq::Call { name, args, .. }) => {
                let n = ling_js::strs::to_rust(&name);
                if n == "__host" {
                    let op = match args.first() {
                        Some(JV::Str(t)) => ling_js::strs::to_rust(t),
                        _ => String::new(),
                    };
                    let rest: Vec<Value> = args.iter().skip(1).map(|a| to_ling(&mut c.e, a, 0)).collect();
                    return list(vec![s("host"), Value::Str(op), list(rest)]);
                }
                let err = c.e.type_error(&format!("{} is not available", n));
                st = c.e.resume_throw(err);
            }
            // a module to load (already resolved): the program fetches it and answers its source
            Status::Host(HostReq::Import { url }) => {
                return list(vec![s("import"), Value::Str(ling_js::strs::to_rust(&url))]);
            }
            Status::Host(HostReq::Get { .. }) => st = c.e.resume(JV::Empty),
            Status::Host(HostReq::Set { .. }) => st = c.e.resume(JV::Bool(false)),
        }
    }
}

fn now_ms() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64() * 1000.0).unwrap_or(0.0)
}

fn with<F: FnOnce(&mut Ctx) -> Value>(h: f64, f: F) -> Value {
    ENGINES.with(|m| {
        let mut m = m.borrow_mut();
        match m.get_mut(&(h as u64)) {
            Some(c) => {
                c.e.now_ms = now_ms();
                f(c)
            }
            None => list(vec![s("error"), s("no such engine")]),
        }
    })
}

pub fn call(name: &str, args: &[Value]) -> Value {
    match name {
        "lingjs_new" => {
            let mut e = Engine::new();
            // Math.random / crypto.getRandomValues: seeded from the OS, different every page
            e.seed_random(rand::random::<u64>() | 1);
            e.now_ms = now_ms();
            let host = e.host_function("__host", 1);
            let g = e.global();
            e.define(g, "__host", JV::Obj(host), 0);
            e.pin(host);
            let id = NEXT.with(|n| {
                let mut n = n.borrow_mut();
                let id = *n;
                *n += 1;
                id
            });
            ENGINES.with(|m| m.borrow_mut().insert(id, Box::new(Ctx { e, _host: host })));
            Value::Number(id as f64)
        }
        "lingjs_eval" => {
            let src = text(args, 1);
            let steps = num(args, 2);
            with(num(args, 0), |c| {
                if steps > 0.0 {
                    c.e.budget = steps as u64;
                }
                let st = c.e.eval(&src);
                status_out(c, st)
            })
        }
        "lingjs_call" => {
            let fname = text(args, 1);
            let steps = num(args, 3);
            let a = args.get(2).cloned().unwrap_or(Value::Unit);
            with(num(args, 0), |c| {
                if steps > 0.0 {
                    c.e.budget = steps as u64;
                }
                let g = JV::Obj(c.e.global());
                let f = match c.e.get_str(&g, &fname) {
                    Ok(f) => f,
                    Err(_) => JV::Undefined,
                };
                if !c.e.is_callable(&f) {
                    return list(vec![s("error"), Value::Str(format!("{} is not a function", fname))]);
                }
                let jargs: Vec<JV> = match &a {
                    Value::List(l) => l.iter().map(|x| to_js(&mut c.e, x, 0)).collect(),
                    Value::Unit => Vec::new(),
                    other => vec![to_js(&mut c.e, other, 0)],
                };
                let st = c.e.call(f, JV::Undefined, jargs);
                status_out(c, st)
            })
        }
        "lingjs_resume" => {
            let a = args.get(1).cloned().unwrap_or(Value::Unit);
            with(num(args, 0), |c| {
                if !c.e.suspended() {
                    return list(vec![s("error"), s("nothing to resume")]);
                }
                let v = to_js(&mut c.e, &a, 0);
                let st = c.e.resume(v);
                status_out(c, st)
            })
        }
        "lingjs_throw" => {
            let msg = text(args, 1);
            with(num(args, 0), |c| {
                if !c.e.suspended() {
                    return list(vec![s("error"), s("nothing to resume")]);
                }
                let err = c.e.type_error(&msg);
                let st = c.e.resume_throw(err);
                status_out(c, st)
            })
        }
        "lingjs_jobs" => {
            let steps = num(args, 1);
            with(num(args, 0), |c| {
                if steps > 0.0 {
                    c.e.budget = steps as u64;
                }
                if !c.e.has_jobs() {
                    return list(vec![s("done"), Value::Unit]);
                }
                let st = c.e.run_jobs();
                status_out(c, st)
            })
        }
        "lingjs_module" => {
            let src = text(args, 1);
            let url = text(args, 2);
            let steps = num(args, 3);
            with(num(args, 0), |c| {
                if steps > 0.0 {
                    c.e.budget = steps as u64;
                }
                let st = c.e.eval_module(&src, &url);
                status_out(c, st)
            })
        }
        "lingjs_base" => {
            let u = text(args, 1);
            with(num(args, 0), |c| {
                c.e.base_url = u;
                Value::Bool(true)
            })
        }
        "lingjs_log" => with(num(args, 0), |c| list(c.e.log.drain(..).map(Value::Str).collect())),
        "lingjs_check" => match Engine::check_syntax(&text(args, 0)) {
            Ok(()) => s(""),
            Err(m) => Value::Str(m),
        },
        "lingjs_steps" => with(num(args, 0), |c| Value::Number(c.e.steps as f64)),
        "lingjs_abort" => with(num(args, 0), |c| {
            c.e.abort();
            Value::Bool(true)
        }),
        "lingjs_free" => {
            let h = num(args, 0) as u64;
            ENGINES.with(|m| m.borrow_mut().remove(&h));
            Value::Bool(true)
        }
        _ => Value::Unit,
    }
}
