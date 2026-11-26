//! The newer half of the standard library: Symbol, generators, Map / Set / WeakMap / WeakSet,
//! ArrayBuffer and typed arrays, DataView, TextEncoder / TextDecoder, Reflect, Proxy, BigInt,
//! and the hooks the prelude's Promise uses (the job queue, rejection tracking).

use crate::atoms::{k, sym, Atom};
use crate::builtins::{arg, ctor, leak, ok, reg, st, R};
use crate::object::*;
use crate::strs::{self, js};
use crate::value::Value;
use crate::vm::{Engine, Fault, Key, NOut, NResult, NativeFn};
use alloc::boxed::Box;
use alloc::format;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

/// a builtin under a symbol key (`[Symbol.iterator]() {}`)
pub(crate) fn reg_sym(e: &mut Engine, obj: ObjRef, key: Atom, name: &str, f: NativeFn, len: u32) -> ObjRef {
    let idx = e.natives.len() as u16;
    e.natives.push((leak(name), f));
    let fo = e.new_native(name, idx, len, Value::Undefined);
    e.heap.get_mut(obj).props.insert(key, Slot::Data(Value::Obj(fo), HIDDEN));
    fo
}

/// a getter (accessor without a setter), not enumerable
pub(crate) fn reg_getter(e: &mut Engine, obj: ObjRef, key: Atom, name: &str, f: NativeFn) {
    let idx = e.natives.len() as u16;
    e.natives.push((leak(name), f));
    let fo = e.new_native(&format!("get {}", name), idx, 0, Value::Undefined);
    e.heap.get_mut(obj).props.insert(key, Slot::Accessor(Some(fo), None, CONFIGURABLE));
}

fn new_proto(e: &mut Engine, name: &'static str, parent: Option<ObjRef>, tag: Option<&str>) -> ObjRef {
    let c = e.atom("Object");
    let p = e.heap.alloc(Obj::new(parent, Kind::Ordinary, c));
    if let Some(t) = tag {
        e.define_atom(p, sym::toStringTag, Value::str(t), CONFIGURABLE);
    }
    e.realm.protos.insert(name, p);
    p
}

pub fn setup(e: &mut Engine) {
    let op = e.realm.object_proto;
    new_proto(e, "Symbol", Some(op), Some("Symbol"));
    setup_symbol(e);
    setup_iterators(e);
    setup_collections(e);
    setup_typed(e);
    setup_reflect(e);
    setup_bigint(e);
    setup_hooks(e);
}

// ── Symbol ──

fn this_sym(e: &mut Engine, this: &Value) -> NResult<Atom> {
    match e.sym_of(this) {
        Some(a) => Ok(a),
        None => e.throw_type("Symbol.prototype method called on a non-symbol"),
    }
}

fn setup_symbol(e: &mut Engine) {
    let p = e.proto("Symbol");
    let c = ctor(e, "Symbol", |e, _, a, _, construct| {
        if construct {
            return e.throw_type("Symbol is not a constructor");
        }
        let desc = match arg(a, 0) {
            Value::Undefined => {
                let at = e.atoms.new_symbol_undescribed();
                return ok(e.symbol_value(at));
            },
            v => e.to_str(&v)?,
        };
        ok(e.new_symbol(&desc, false))
    }, 0, p);
    for (i, n) in sym::NAMES.iter().enumerate() {
        let a = Atom(sym::iterator.0 + i as u32);
        let v = e.symbol_value(a);
        e.define(c, n, v, 0);
    }
    reg(e, c, "for", |e, _, a, _, _| {
        let s = e.to_str(&arg(a, 0))?;
        if let Some(o) = e.sym_registry.get(&s.to_vec()) {
            return ok(Value::Obj(*o));
        }
        let v = e.new_symbol(&s, false);
        e.sym_registry.insert(s.to_vec(), v.as_obj().unwrap());
        ok(v)
    }, 1);
    reg(e, c, "keyFor", |e, _, a, _, _| {
        let v = arg(a, 0);
        let Some(at) = e.sym_of(&v) else { return e.throw_type("Symbol.keyFor: not a symbol") };
        let o = v.as_obj().unwrap();
        if e.sym_registry.values().any(|x| *x == o) {
            return st(e.atoms.name(at).clone());
        }
        ok(Value::Undefined)
    }, 1);
    reg(e, p, "toString", |e, this, _, _, _| {
        let a = this_sym(e, this)?;
        st(js(&format!("Symbol({})", strs::to_rust(e.atoms.name(a)))))
    }, 0);
    reg(e, p, "valueOf", |e, this, _, _, _| {
        this_sym(e, this)?;
        ok(this.clone())
    }, 0);
    reg_sym(e, p, sym::toPrimitive, "[Symbol.toPrimitive]", |e, this, _, _, _| {
        this_sym(e, this)?;
        ok(this.clone())
    }, 1);
    reg_getter(e, p, k::description, "description", |e, this, _, _, _| {
        let a = this_sym(e, this)?;
        if e.atoms.is_undescribed(a) {
            return ok(Value::Undefined);
        }
        st(e.atoms.name(a).clone())
    });
    let oc = e.global_obj("Object");
    reg(e, oc, "getOwnPropertySymbols", |e, _, a, _, _| {
        let o = e.to_object(&arg(a, 0))?;
        let ks = e.own_symbol_keys(o, false);
        ok(Value::Obj(e.new_array(ks)))
    }, 1);
}

// ── iterators and generators ──

fn this_gen(e: &mut Engine, this: &Value, flavor: u8) -> NResult<ObjRef> {
    if let Value::Obj(o) = this {
        if let Kind::Generator(g) = &e.heap.get(*o).kind {
            if g.flavor == flavor {
                return Ok(*o);
            }
        }
    }
    e.throw_type("next method called on an incompatible receiver")
}

fn setup_iterators(e: &mut Engine) {
    let op = e.realm.object_proto;
    let ip = new_proto(e, "Iterator", Some(op), None);
    reg_sym(e, ip, sym::iterator, "[Symbol.iterator]", |_, this, _, _, _| ok(this.clone()), 0);
    let aip = new_proto(e, "AsyncIterator", Some(op), None);
    reg_sym(e, aip, sym::asyncIterator, "[Symbol.asyncIterator]", |_, this, _, _, _| ok(this.clone()), 0);
    let gp = new_proto(e, "Generator", Some(ip), Some("Generator"));
    reg(e, gp, "next", |e, this, a, _, _| Ok(NOut::GenResume(this_gen(e, this, 0)?, 0, arg(a, 0))), 1);
    reg(e, gp, "throw", |e, this, a, _, _| Ok(NOut::GenResume(this_gen(e, this, 0)?, 1, arg(a, 0))), 1);
    reg(e, gp, "return", |e, this, a, _, _| Ok(NOut::GenResume(this_gen(e, this, 0)?, 2, arg(a, 0))), 1);
    // AsyncGenerator.prototype's next / return / throw come from the prelude
    new_proto(e, "AsyncGenerator", Some(aip), Some("AsyncGenerator"));
    // the function constructors' prototypes (GeneratorFunction etc. are not globals)
    let fp = e.realm.function_proto;
    let gfp = new_proto(e, "GeneratorFunction", Some(fp), Some("GeneratorFunction"));
    e.define_atom(gfp, k::prototype, Value::Obj(gp), CONFIGURABLE);
    new_proto(e, "AsyncFunction", Some(fp), Some("AsyncFunction"));
}

// ── the prelude's hooks (deleted when the prelude has run) ──

fn setup_hooks(e: &mut Engine) {
    let g = e.realm.global;
    reg(e, g, "__privateSymbol", |e, _, a, _, _| {
        let s = e.to_str(&arg(a, 0))?;
        ok(e.new_symbol(&s, true))
    }, 1);
    reg(e, g, "__proto", |e, _, a, _, _| {
        let s = e.to_str(&arg(a, 0))?;
        let n = strs::to_rust(&s);
        match e.realm.protos.iter().find(|(k, _)| **k == n.as_str()) {
            Some((_, p)) => ok(Value::Obj(*p)),
            None => ok(Value::Undefined),
        }
    }, 1);
    reg(e, g, "__enqueueJob", |e, _, a, _, _| {
        e.enqueue_job(arg(a, 0), Value::Undefined, a.iter().skip(1).cloned().collect());
        ok(Value::Undefined)
    }, 3);
    reg(e, g, "__trackRejection", |e, _, a, _, _| {
        if let Value::Obj(p) = arg(a, 0) {
            if e.to_number(&arg(a, 1))? == 0.0 {
                e.unhandled.push((p, arg(a, 2)));
            } else {
                e.unhandled.retain(|(q, _)| *q != p);
            }
        }
        ok(Value::Undefined)
    }, 3);
    reg(e, g, "__genResume", |e, _, a, _, _| {
        let gen = match arg(a, 0) {
            Value::Obj(o) if matches!(e.heap.get(o).kind, Kind::Generator(_)) => o,
            _ => return e.throw_type("not a generator"),
        };
        let mode = e.to_number(&arg(a, 1))? as u8;
        Ok(NOut::GenResume(gen, mode, arg(a, 2)))
    }, 3);
    reg(e, g, "__genState", |e, _, a, _, _| {
        let n = match arg(a, 0) {
            Value::Obj(o) => match &e.heap.get(o).kind {
                Kind::Generator(g) => match g.state {
                    GenState::Start => 0.0,
                    GenState::Suspended => {
                        if g.yield_kind == 2 {
                            2.0
                        } else {
                            1.0
                        }
                    },
                    GenState::Done => 3.0,
                    GenState::Running => 4.0,
                },
                _ => -1.0,
            },
            _ => -1.0,
        };
        ok(Value::Num(n))
    }, 1);
    reg(e, g, "__isProxy", |e, _, a, _, _| ok(Value::Bool(matches!(arg(a, 0), Value::Obj(o) if matches!(e.heap.get(o).kind, Kind::Proxy(_))))), 1);
    reg(e, g, "__compileModule", |e, _, a, _, _| {
        let s = e.to_str(&arg(a, 0))?;
        let p = e.compile_module_src(&strs::to_rust(&s))?;
        Ok(NOut::RunScript(p))
    }, 1);
    reg(e, g, "__baseUrl", |e, _, _, _, _| ok(Value::str(&e.base_url.clone())), 0);
    let fm = e.host_function("%import", 1);
    e.define(g, "__fetchModule", Value::Obj(fm), HIDDEN);
    reg(e, g, "__isGenerator", |e, _, a, _, _| {
        ok(Value::Bool(matches!(arg(a, 0), Value::Obj(o) if matches!(e.heap.get(o).kind, Kind::Generator(_)))))
    }, 1);
    let _ = (Rc::new(0), String::new(), Box::new(0), Fault::NeedJs, Key::Index(0), vec![0]);
}

#[allow(dead_code)]
fn _r() -> R {
    ok(Value::Undefined)
}

// ── Map, Set, WeakMap, WeakSet, WeakRef, FinalizationRegistry ──

fn this_coll(e: &mut Engine, this: &Value, set: bool) -> NResult<ObjRef> {
    if let Value::Obj(o) = this {
        match e.heap.get(*o).kind {
            Kind::Map(_) if !set => return Ok(*o),
            Kind::Set(_) if set => return Ok(*o),
            _ => {},
        }
    }
    e.throw_type(if set { "Set method called on an incompatible receiver" } else { "Map method called on an incompatible receiver" })
}

fn coll(e: &mut Engine, o: ObjRef) -> &mut MapData {
    match &mut e.heap.get_mut(o).kind {
        Kind::Map(m) | Kind::Set(m) => m,
        _ => unreachable!(),
    }
}

fn new_coll(e: &mut Engine, set: bool) -> ObjRef {
    let name = if set { "Set" } else { "Map" };
    let p = e.proto(name);
    let c = e.atom(name);
    let kind = if set { Kind::Set(Box::default()) } else { Kind::Map(Box::default()) };
    e.heap.alloc(Obj::new(Some(p), kind, c))
}

fn coll_iter(e: &mut Engine, o: ObjRef, kind: u8, set: bool) -> Value {
    let p = e.proto(if set { "SetIterator" } else { "MapIterator" });
    let c = e.atom("Object");
    let it = e.heap.alloc(Obj::new(Some(p), Kind::CollIter(Some(o), 0, kind), c));
    coll(e, o).iters.push(it);
    Value::Obj(it)
}

fn coll_next(e: &mut Engine, this: &Value) -> R {
    let it = match this {
        Value::Obj(o) if matches!(e.heap.get(*o).kind, Kind::CollIter(..)) => *o,
        _ => return e.throw_type("next called on an incompatible receiver"),
    };
    let (c, mut pos, kind) = match e.heap.get(it).kind {
        Kind::CollIter(c, p, k) => (c, p, k),
        _ => unreachable!(),
    };
    let Some(c) = c else { return ok(e.iter_result(Value::Undefined, true)) };
    let is_set = matches!(e.heap.get(c).kind, Kind::Set(_));
    loop {
        let m = coll(e, c);
        if pos >= m.entries.len() {
            m.iters.retain(|x| *x != it);
            if let Kind::CollIter(cc, _, _) = &mut e.heap.get_mut(it).kind {
                *cc = None;
            }
            return ok(e.iter_result(Value::Undefined, true));
        }
        let (k, v) = m.entries[pos].clone();
        pos += 1;
        if let Value::Empty = k {
            continue;
        }
        if let Kind::CollIter(_, p, _) = &mut e.heap.get_mut(it).kind {
            *p = pos;
        }
        let out = match kind {
            0 => k,
            1 => {
                if is_set {
                    k
                } else {
                    v
                }
            },
            _ => {
                let v2 = if is_set { k.clone() } else { v };
                Value::Obj(e.new_array(vec![k, v2]))
            },
        };
        return ok(e.iter_result(out, false));
    }
}

/// fill a new Map / Set from a plain array (of [k, v] arrays for a Map); NeedJs for other iterables
fn fill_from(e: &mut Engine, o: ObjRef, src: &Value, set: bool) -> NResult<()> {
    if src.is_nullish() {
        return Ok(());
    }
    let s = match src {
        Value::Obj(s) if matches!(e.heap.get(*s).kind, Kind::Array(_)) => *s,
        _ => return Err(Fault::NeedJs),
    };
    let ap = e.realm.array_proto;
    if e.heap.get(s).proto != Some(ap) || e.heap.get(s).props.contains(sym::iterator) || e.heap.get(ap).props.get(sym::iterator).is_none() {
        return Err(Fault::NeedJs);
    }
    let items = match &e.heap.get(s).kind {
        Kind::Array(v) => v.clone(),
        _ => unreachable!(),
    };
    for it in items {
        let it = if let Value::Empty = it { Value::Undefined } else { it };
        if set {
            coll(e, o).set(it, Value::Undefined);
        } else {
            let pair = match &it {
                Value::Obj(p) => match &e.heap.get(*p).kind {
                    Kind::Array(v) => (v.first().cloned().unwrap_or(Value::Undefined), v.get(1).cloned().unwrap_or(Value::Undefined)),
                    _ => return Err(Fault::NeedJs),
                },
                _ => return e.throw_type("Iterator value is not an entry object"),
            };
            let k = if let Value::Empty = pair.0 { Value::Undefined } else { pair.0 };
            let v = if let Value::Empty = pair.1 { Value::Undefined } else { pair.1 };
            coll(e, o).set(k, v);
        }
    }
    Ok(())
}

fn weak_key(e: &mut Engine, this: &Value, set: bool) -> NResult<Atom> {
    if let Value::Obj(o) = this {
        match e.heap.get(*o).kind {
            Kind::WeakMap(a) if !set => return Ok(a),
            Kind::WeakSet(a) if set => return Ok(a),
            _ => {},
        }
    }
    e.throw_type("WeakMap / WeakSet method called on an incompatible receiver")
}

fn can_be_weak(e: &Engine, v: &Value) -> Option<ObjRef> {
    match v {
        Value::Obj(o) if e.is_object(v) || matches!(e.heap.get(*o).kind, Kind::Symbol(_)) => Some(*o),
        _ => None,
    }
}

fn map_ctor(e: &mut Engine, a: &[Value], construct: bool, set: bool) -> R {
    if !construct {
        return e.throw_type(if set { "Constructor Set requires 'new'" } else { "Constructor Map requires 'new'" });
    }
    let o = new_coll(e, set);
    fill_from(e, o, &arg(a, 0), set)?;
    ok(Value::Obj(o))
}

fn weak_ctor(e: &mut Engine, construct: bool, set: bool) -> R {
    let name = if set { "WeakSet" } else { "WeakMap" };
    if !construct {
        return e.throw_type(&format!("Constructor {} requires 'new'", name));
    }
    let a = e.atoms.new_symbol(&js(name), true);
    let (p, c) = (e.proto(name), e.atom(name));
    let kind = if set { Kind::WeakSet(a) } else { Kind::WeakMap(a) };
    ok(Value::Obj(e.heap.alloc(Obj::new(Some(p), kind, c))))
}

fn weak_has(e: &mut Engine, this: &Value, a: &[Value], set: bool) -> R {
    let at = weak_key(e, this, set)?;
    ok(Value::Bool(can_be_weak(e, &arg(a, 0)).is_some_and(|k| e.heap.get(k).props.contains(at))))
}

fn weak_delete(e: &mut Engine, this: &Value, a: &[Value], set: bool) -> R {
    let at = weak_key(e, this, set)?;
    ok(Value::Bool(can_be_weak(e, &arg(a, 0)).is_some_and(|k| e.heap.get_mut(k).props.remove(at))))
}

pub(crate) fn setup_collections(e: &mut Engine) {
    let op = e.realm.object_proto;
    let ip = e.proto("Iterator");
    let size_a = e.atom("size");
    // Map
    let p = new_proto(e, "Map", Some(op), Some("Map"));
    let c = ctor(e, "Map", |e, _, a, _, construct| map_ctor(e, a, construct, false), 0, p);
    reg_getter(e, c, sym::species, "[Symbol.species]", |_, this, _, _, _| ok(this.clone()));
    let itp = new_proto(e, "MapIterator", Some(ip), Some("Map Iterator"));
    reg(e, itp, "next", |e, this, _, _, _| coll_next(e, this), 0);
    reg_getter(e, p, size_a, "size", |e, this, _, _, _| {
        let o = this_coll(e, this, false)?;
        ok(Value::Num(coll(e, o).live as f64))
    });
    reg(e, p, "get", |e, this, a, _, _| {
        let o = this_coll(e, this, false)?;
        ok(coll(e, o).get(&arg(a, 0)).cloned().unwrap_or(Value::Undefined))
    }, 1);
    reg(e, p, "set", |e, this, a, _, _| {
        let o = this_coll(e, this, false)?;
        coll(e, o).set(arg(a, 0), arg(a, 1));
        ok(this.clone())
    }, 2);
    reg(e, p, "has", |e, this, a, _, _| {
        let o = this_coll(e, this, false)?;
        ok(Value::Bool(coll(e, o).find(&arg(a, 0)).is_some()))
    }, 1);
    reg(e, p, "delete", |e, this, a, _, _| {
        let o = this_coll(e, this, false)?;
        ok(Value::Bool(coll(e, o).remove(&arg(a, 0))))
    }, 1);
    reg(e, p, "clear", |e, this, _, _, _| {
        let o = this_coll(e, this, false)?;
        coll(e, o).clear();
        ok(Value::Undefined)
    }, 0);
    reg(e, p, "keys", |e, this, _, _, _| {
        let o = this_coll(e, this, false)?;
        ok(coll_iter(e, o, 0, false))
    }, 0);
    reg(e, p, "values", |e, this, _, _, _| {
        let o = this_coll(e, this, false)?;
        ok(coll_iter(e, o, 1, false))
    }, 0);
    let ef = reg(e, p, "entries", |e, this, _, _, _| {
        let o = this_coll(e, this, false)?;
        ok(coll_iter(e, o, 2, false))
    }, 0);
    e.heap.get_mut(p).props.insert(sym::iterator, Slot::Data(Value::Obj(ef), HIDDEN));
    // Set
    let p = new_proto(e, "Set", Some(op), Some("Set"));
    let c = ctor(e, "Set", |e, _, a, _, construct| map_ctor(e, a, construct, true), 0, p);
    reg_getter(e, c, sym::species, "[Symbol.species]", |_, this, _, _, _| ok(this.clone()));
    let itp = new_proto(e, "SetIterator", Some(ip), Some("Set Iterator"));
    reg(e, itp, "next", |e, this, _, _, _| coll_next(e, this), 0);
    reg_getter(e, p, size_a, "size", |e, this, _, _, _| {
        let o = this_coll(e, this, true)?;
        ok(Value::Num(coll(e, o).live as f64))
    });
    reg(e, p, "add", |e, this, a, _, _| {
        let o = this_coll(e, this, true)?;
        coll(e, o).set(arg(a, 0), Value::Undefined);
        ok(this.clone())
    }, 1);
    reg(e, p, "has", |e, this, a, _, _| {
        let o = this_coll(e, this, true)?;
        ok(Value::Bool(coll(e, o).find(&arg(a, 0)).is_some()))
    }, 1);
    reg(e, p, "delete", |e, this, a, _, _| {
        let o = this_coll(e, this, true)?;
        ok(Value::Bool(coll(e, o).remove(&arg(a, 0))))
    }, 1);
    reg(e, p, "clear", |e, this, _, _, _| {
        let o = this_coll(e, this, true)?;
        coll(e, o).clear();
        ok(Value::Undefined)
    }, 0);
    let vf = reg(e, p, "values", |e, this, _, _, _| {
        let o = this_coll(e, this, true)?;
        ok(coll_iter(e, o, 1, true))
    }, 0);
    e.define(p, "keys", Value::Obj(vf), HIDDEN);
    e.heap.get_mut(p).props.insert(sym::iterator, Slot::Data(Value::Obj(vf), HIDDEN));
    reg(e, p, "entries", |e, this, _, _, _| {
        let o = this_coll(e, this, true)?;
        ok(coll_iter(e, o, 2, true))
    }, 0);
    // a for-of that stops early lets the iterator go (so the table can compact again)
    for name in ["MapIterator", "SetIterator"] {
        let itp = e.proto(name);
        reg(e, itp, "return", |e, this, a, _, _| {
            if let Value::Obj(it) = this {
                let it = *it;
                if let Kind::CollIter(Some(c), _, _) = e.heap.get(it).kind {
                    coll(e, c).iters.retain(|x| *x != it);
                    if let Kind::CollIter(cc, _, _) = &mut e.heap.get_mut(it).kind {
                        *cc = None;
                    }
                }
            }
            ok(e.iter_result(arg(a, 0), true))
        }, 0);
    }
    // WeakMap / WeakSet: entries live on the keys under the collection's private name
    let p = new_proto(e, "WeakMap", Some(op), Some("WeakMap"));
    ctor(e, "WeakMap", |e, _, _, _, construct| weak_ctor(e, construct, false), 0, p);
    reg(e, p, "set", |e, this, a, _, _| {
        let at = weak_key(e, this, false)?;
        let Some(k) = can_be_weak(e, &arg(a, 0)) else { return e.throw_type("Invalid value used as weak map key") };
        e.heap.get_mut(k).props.insert(at, Slot::Data(arg(a, 1), 0));
        ok(this.clone())
    }, 2);
    reg(e, p, "get", |e, this, a, _, _| {
        let at = weak_key(e, this, false)?;
        let Some(k) = can_be_weak(e, &arg(a, 0)) else { return ok(Value::Undefined) };
        ok(match e.heap.get(k).props.get(at) {
            Some(Slot::Data(v, _)) => v.clone(),
            _ => Value::Undefined,
        })
    }, 1);
    reg(e, p, "has", |e, this, a, _, _| weak_has(e, this, a, false), 1);
    reg(e, p, "delete", |e, this, a, _, _| weak_delete(e, this, a, false), 1);
    let p = new_proto(e, "WeakSet", Some(op), Some("WeakSet"));
    ctor(e, "WeakSet", |e, _, _, _, construct| weak_ctor(e, construct, true), 0, p);
    reg(e, p, "add", |e, this, a, _, _| {
        let at = weak_key(e, this, true)?;
        let Some(k) = can_be_weak(e, &arg(a, 0)) else { return e.throw_type("Invalid value used in weak set") };
        e.heap.get_mut(k).props.insert(at, Slot::Data(Value::Bool(true), 0));
        ok(this.clone())
    }, 1);
    reg(e, p, "has", |e, this, a, _, _| weak_has(e, this, a, true), 1);
    reg(e, p, "delete", |e, this, a, _, _| weak_delete(e, this, a, true), 1);
    // WeakRef holds its target strongly (no weak collection here); FinalizationRegistry never fires
    let slot = e.new_symbol(&js("WeakRefTarget"), true);
    e.realm.protos.insert("%WeakRefSlot", slot.as_obj().unwrap());
    let wp = new_proto(e, "WeakRef", Some(op), Some("WeakRef"));
    ctor(e, "WeakRef", |e, _, a, _, construct| {
        if !construct {
            return e.throw_type("Constructor WeakRef requires 'new'");
        }
        if can_be_weak(e, &arg(a, 0)).is_none() {
            return e.throw_type("WeakRef: invalid target");
        }
        let p = e.proto("WeakRef");
        let o = e.new_object_proto(Some(p));
        let ta = weakref_atom(e);
        e.heap.get_mut(o).props.insert(ta, Slot::Data(arg(a, 0), 0));
        ok(Value::Obj(o))
    }, 1, wp);
    reg(e, wp, "deref", |e, this, _, _, _| {
        let ta = weakref_atom(e);
        ok(match this.as_obj().and_then(|o| e.heap.get(o).props.get(ta).cloned()) {
            Some(Slot::Data(v, _)) => v,
            _ => Value::Undefined,
        })
    }, 0);
    let fp = new_proto(e, "FinalizationRegistry", Some(op), Some("FinalizationRegistry"));
    ctor(e, "FinalizationRegistry", |e, _, _, _, construct| {
        if !construct {
            return e.throw_type("Constructor FinalizationRegistry requires 'new'");
        }
        let p = e.proto("FinalizationRegistry");
        ok(Value::Obj(e.new_object_proto(Some(p))))
    }, 1, fp);
    reg(e, fp, "register", |_, _, _, _, _| ok(Value::Undefined), 2);
    reg(e, fp, "unregister", |_, _, _, _, _| ok(Value::Bool(false)), 1);
}

fn weakref_atom(e: &mut Engine) -> Atom {
    let o = e.proto("%WeakRefSlot");
    match e.heap.get(o).kind {
        Kind::Symbol(a) => a,
        _ => k::EMPTY,
    }
}

// ── ArrayBuffer, typed arrays, DataView ──

pub(crate) const TA_NAMES: [&str; 11] = [
    "Int8Array", "Uint8Array", "Uint8ClampedArray", "Int16Array", "Uint16Array", "Int32Array", "Uint32Array", "Float32Array", "Float64Array", "BigInt64Array", "BigUint64Array",
];

fn ta_size(ty: u8) -> usize {
    TypedArrayData { buf: ObjRef(0), offset: 0, len: 0, ty }.elem_size()
}

pub(crate) fn new_buffer(e: &mut Engine, bytes: Vec<u8>) -> ObjRef {
    let p = e.proto("ArrayBuffer");
    let c = e.atom("ArrayBuffer");
    e.heap.alloc(Obj::new(Some(p), Kind::ArrayBuffer(bytes), c))
}

pub(crate) fn new_typed(e: &mut Engine, ty: u8, buf: ObjRef, offset: usize, len: usize) -> ObjRef {
    let name = TA_NAMES[ty as usize];
    let p = e.proto(name);
    let c = e.atom(name);
    e.heap.alloc(Obj::new(Some(p), Kind::TypedArray(TypedArrayData { buf, offset, len, ty }), c))
}

pub(crate) fn buf_bytes(e: &Engine, buf: ObjRef) -> &[u8] {
    match &e.heap.get(buf).kind {
        Kind::ArrayBuffer(v) => v,
        _ => &[],
    }
}

fn buf_bytes_mut(e: &mut Engine, buf: ObjRef) -> &mut Vec<u8> {
    match &mut e.heap.get_mut(buf).kind {
        Kind::ArrayBuffer(v) => v,
        _ => unreachable!(),
    }
}

/// a raw element of `ty` at byte `at` (little-endian unless `big`)
fn read_raw(bytes: &[u8], at: usize, ty: u8, big: bool) -> Option<RawNum> {
    let n = ta_size(ty);
    let s = bytes.get(at..at + n)?;
    let mut b = [0u8; 8];
    b[..n].copy_from_slice(s);
    if big {
        b[..n].reverse();
    }
    Some(match ty {
        0 => RawNum::F(b[0] as i8 as f64),
        1 | 2 => RawNum::F(b[0] as f64),
        3 => RawNum::F(i16::from_le_bytes([b[0], b[1]]) as f64),
        4 => RawNum::F(u16::from_le_bytes([b[0], b[1]]) as f64),
        5 => RawNum::F(i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f64),
        6 => RawNum::F(u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f64),
        7 => RawNum::F(f32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f64),
        8 => RawNum::F(f64::from_le_bytes(b)),
        9 => RawNum::I(i64::from_le_bytes(b) as i128),
        _ => RawNum::I(u64::from_le_bytes(b) as i128),
    })
}

enum RawNum {
    F(f64),
    I(i128),
}

fn write_raw(bytes: &mut [u8], at: usize, ty: u8, x: f64, bi: u64, big: bool) {
    let n = ta_size(ty);
    let mut b: [u8; 8] = match ty {
        0 | 1 => [crate::num::to_int32(x) as u8, 0, 0, 0, 0, 0, 0, 0],
        2 => {
            let c = if x.is_nan() { 0.0 } else { x.clamp(0.0, 255.0) };
            // round half to even
            let r = libm::rint(c);
            [r as u8, 0, 0, 0, 0, 0, 0, 0]
        },
        3 | 4 => {
            let v = (crate::num::to_int32(x) as u32 as u16).to_le_bytes();
            [v[0], v[1], 0, 0, 0, 0, 0, 0]
        },
        5 | 6 => {
            let v = crate::num::to_uint32(x).to_le_bytes();
            [v[0], v[1], v[2], v[3], 0, 0, 0, 0]
        },
        7 => {
            let v = (x as f32).to_le_bytes();
            [v[0], v[1], v[2], v[3], 0, 0, 0, 0]
        },
        8 => x.to_le_bytes(),
        _ => bi.to_le_bytes(),
    };
    if big {
        b[..n].reverse();
    }
    if let Some(d) = bytes.get_mut(at..at + n) {
        d.copy_from_slice(&b[..n]);
    }
}

fn raw_value(e: &mut Engine, r: RawNum) -> Value {
    match r {
        RawNum::F(f) => Value::Num(f),
        RawNum::I(i) => e.new_bigint(crate::bigint::BigInt::from_i64(0).add(&if i < 0 { crate::bigint::BigInt::from_i64(i as i64) } else { crate::bigint::BigInt::from_u64(i as u64) })),
    }
}

/// element i of a typed array (None past its end)
pub(crate) fn ta_get(e: &mut Engine, t: &TypedArrayData, i: usize) -> Option<Value> {
    if i >= t.len {
        return None;
    }
    let at = t.offset + i * t.elem_size();
    let r = read_raw(buf_bytes(e, t.buf), at, t.ty, false)?;
    Some(raw_value(e, r))
}

/// the number (or BigInt bits) a typed array element is written from
fn elem_input(e: &mut Engine, ty: u8, v: &Value) -> NResult<(f64, u64)> {
    if ty >= 9 {
        match e.bigint_of(v) {
            Some(b) => Ok((0.0, b.to_u64_wrapping())),
            None => e.throw_type("Cannot convert a non-BigInt to a BigInt element"),
        }
    } else {
        Ok((e.to_number(v)?, 0))
    }
}

pub(crate) fn ta_set(e: &mut Engine, t: &TypedArrayData, i: usize, v: &Value) -> NResult<()> {
    let (x, bi) = elem_input(e, t.ty, v)?;
    if i >= t.len {
        return Ok(());
    }
    let at = t.offset + i * t.elem_size();
    let ty = t.ty;
    write_raw(buf_bytes_mut(e, t.buf), at, ty, x, bi, false);
    Ok(())
}

fn this_ta(e: &mut Engine, this: &Value) -> NResult<TypedArrayData> {
    if let Value::Obj(o) = this {
        if let Kind::TypedArray(t) = e.heap.get(*o).kind {
            return Ok(t);
        }
    }
    e.throw_type("this is not a typed array")
}

fn this_buf(e: &mut Engine, this: &Value) -> NResult<ObjRef> {
    if let Value::Obj(o) = this {
        if let Kind::ArrayBuffer(_) = e.heap.get(*o).kind {
            return Ok(*o);
        }
    }
    e.throw_type("this is not an ArrayBuffer")
}

fn this_view(e: &mut Engine, this: &Value) -> NResult<TypedArrayData> {
    if let Value::Obj(o) = this {
        if let Kind::DataView(t) = e.heap.get(*o).kind {
            return Ok(t);
        }
    }
    e.throw_type("this is not a DataView")
}

fn index_arg(e: &mut Engine, a: &[Value], i: usize) -> NResult<usize> {
    let n = e.to_number(&arg(a, i))?;
    if n.is_nan() {
        return Ok(0);
    }
    if n < 0.0 || n > 9007199254740991.0 {
        return Err(Fault::Throw(e.range_error("Invalid offset or length")));
    }
    Ok(libm::trunc(n) as usize)
}

fn rel_arg(e: &mut Engine, a: &[Value], i: usize, len: usize, d: usize) -> NResult<usize> {
    match a.get(i) {
        None | Some(Value::Undefined) => Ok(d),
        Some(v) => {
            let n = e.to_number(v)?;
            let n = if n.is_nan() { 0.0 } else { libm::trunc(n) };
            Ok(crate::builtins::rel(n, len))
        },
    }
}

/// new XArray(...): a length, an ArrayBuffer (+ offset, length), a typed array, or an array
fn make_typed(e: &mut Engine, ty: u8, a: &[Value]) -> NResult<ObjRef> {
    let size = ta_size(ty);
    let first = arg(a, 0);
    if let Value::Obj(o) = &first {
        let o = *o;
        let kind = match &e.heap.get(o).kind {
            Kind::ArrayBuffer(b) => Some((0, b.len())),
            Kind::TypedArray(_) => Some((1, 0)),
            Kind::Array(_) => Some((2, 0)),
            _ if e.is_heap_prim(&first) => None,
            _ => Some((3, 0)),
        };
        match kind {
            Some((0, blen)) => {
                let off = if a.len() > 1 { index_arg(e, a, 1)? } else { 0 };
                if off % size != 0 {
                    return Err(Fault::Throw(e.range_error("start offset of the typed array should be a multiple of its element size")));
                }
                let len = match a.get(2) {
                    None | Some(Value::Undefined) => {
                        if blen % size != 0 || off > blen {
                            return Err(Fault::Throw(e.range_error("byte length of the typed array should be a multiple of its element size")));
                        }
                        (blen - off) / size
                    },
                    Some(_) => index_arg(e, a, 2)?,
                };
                if off + len * size > blen {
                    return Err(Fault::Throw(e.range_error("Invalid typed array length")));
                }
                return Ok(new_typed(e, ty, o, off, len));
            },
            Some((1, _)) => {
                let src = match e.heap.get(o).kind {
                    Kind::TypedArray(t) => t,
                    _ => unreachable!(),
                };
                let buf = new_buffer(e, vec![0; src.len * size]);
                let t = new_typed(e, ty, buf, 0, src.len);
                let td = TypedArrayData { buf, offset: 0, len: src.len, ty };
                for i in 0..src.len {
                    let v = ta_get(e, &src, i).unwrap_or(Value::Undefined);
                    if (src.ty >= 9) != (ty >= 9) {
                        return e.throw_type("Cannot mix BigInt and other types");
                    }
                    ta_set(e, &td, i, &v)?;
                }
                return Ok(t);
            },
            Some((2, _)) => {
                let items = match &e.heap.get(o).kind {
                    Kind::Array(v) => v.clone(),
                    _ => unreachable!(),
                };
                let buf = new_buffer(e, vec![0; items.len() * size]);
                let t = new_typed(e, ty, buf, 0, items.len());
                let td = TypedArrayData { buf, offset: 0, len: items.len(), ty };
                for (i, v) in items.iter().enumerate() {
                    let v = if let Value::Empty = v { Value::Undefined } else { v.clone() };
                    ta_set(e, &td, i, &v)?;
                }
                return Ok(t);
            },
            _ => return Err(Fault::NeedJs),
        }
    }
    let n = if first.is_undefined() { 0 } else { index_arg(e, a, 0)? };
    if n > 1 << 31 {
        return Err(Fault::Throw(e.range_error("Invalid typed array length")));
    }
    let buf = new_buffer(e, vec![0; n * size]);
    Ok(new_typed(e, ty, buf, 0, n))
}

fn view_get(e: &mut Engine, this: &Value, a: &[Value], ty: u8) -> R {
    let v = this_view(e, this)?;
    let off = index_arg(e, a, 0)?;
    let little = e.truthy(&arg(a, 1));
    if off + ta_size(ty) > v.len {
        return Err(Fault::Throw(e.range_error("Offset is outside the bounds of the DataView")));
    }
    let r = read_raw(buf_bytes(e, v.buf), v.offset + off, ty, !little).unwrap();
    ok(raw_value(e, r))
}

fn view_set(e: &mut Engine, this: &Value, a: &[Value], ty: u8) -> R {
    let v = this_view(e, this)?;
    let off = index_arg(e, a, 0)?;
    let (x, bi) = elem_input(e, ty, &arg(a, 1))?;
    let little = e.truthy(&arg(a, 2));
    if off + ta_size(ty) > v.len {
        return Err(Fault::Throw(e.range_error("Offset is outside the bounds of the DataView")));
    }
    write_raw(buf_bytes_mut(e, v.buf), v.offset + off, ty, x, bi, !little);
    ok(Value::Undefined)
}

pub(crate) fn setup_typed(e: &mut Engine) {
    let op = e.realm.object_proto;
    // ArrayBuffer
    let bp = new_proto(e, "ArrayBuffer", Some(op), Some("ArrayBuffer"));
    let bc = ctor(e, "ArrayBuffer", |e, _, a, _, construct| {
        if !construct {
            return e.throw_type("Constructor ArrayBuffer requires 'new'");
        }
        let n = index_arg(e, a, 0)?;
        if n > 1 << 31 {
            return Err(Fault::Throw(e.range_error("Array buffer allocation failed")));
        }
        ok(Value::Obj(new_buffer(e, vec![0; n])))
    }, 1, bp);
    reg(e, bc, "isView", |e, _, a, _, _| ok(Value::Bool(matches!(arg(a, 0), Value::Obj(o) if matches!(e.heap.get(o).kind, Kind::TypedArray(_) | Kind::DataView(_))))), 1);
    reg_getter(e, bc, sym::species, "[Symbol.species]", |_, this, _, _, _| ok(this.clone()));
    let bl = e.atom("byteLength");
    reg_getter(e, bp, bl, "byteLength", |e, this, _, _, _| {
        let b = this_buf(e, this)?;
        ok(Value::Num(buf_bytes(e, b).len() as f64))
    });
    reg(e, bp, "slice", |e, this, a, _, _| {
        let b = this_buf(e, this)?;
        let len = buf_bytes(e, b).len();
        let s = rel_arg(e, a, 0, len, 0)?;
        let t = rel_arg(e, a, 1, len, len)?;
        let bytes = if s < t { buf_bytes(e, b)[s..t].to_vec() } else { vec![] };
        ok(Value::Obj(new_buffer(e, bytes)))
    }, 2);
    // %TypedArray% and the eleven element types
    let fp = e.realm.function_proto;
    let tap = new_proto(e, "TypedArray", Some(op), None);
    let tac = {
        let idx = e.natives.len() as u16;
        e.natives.push(("TypedArray", |e, _, _, _, _| e.throw_type("Abstract class TypedArray not directly constructable")));
        let c = e.new_native("TypedArray", idx, 0, Value::Undefined);
        e.define_atom(c, k::prototype, Value::Obj(tap), 0);
        e.define_atom(tap, k::constructor, Value::Obj(c), HIDDEN);
        e.realm.protos.insert("%TypedArray", c);
        c
    };
    let _ = fp;
    reg_getter(e, tac, sym::species, "[Symbol.species]", |_, this, _, _, _| ok(this.clone()));
    let len_a = k::length;
    reg_getter(e, tap, len_a, "length", |e, this, _, _, _| ok(Value::Num(this_ta(e, this)?.len as f64)));
    reg_getter(e, tap, bl, "byteLength", |e, this, _, _, _| {
        let t = this_ta(e, this)?;
        ok(Value::Num((t.len * t.elem_size()) as f64))
    });
    let bo = e.atom("byteOffset");
    reg_getter(e, tap, bo, "byteOffset", |e, this, _, _, _| ok(Value::Num(this_ta(e, this)?.offset as f64)));
    let ba = e.atom("buffer");
    reg_getter(e, tap, ba, "buffer", |e, this, _, _, _| ok(Value::Obj(this_ta(e, this)?.buf)));
    // a getter for @@toStringTag: the type's name (undefined for other objects)
    reg_getter(e, tap, sym::toStringTag, "[Symbol.toStringTag]", |e, this, _, _, _| match this {
        Value::Obj(o) => match e.heap.get(*o).kind {
            Kind::TypedArray(t) => ok(Value::str(TA_NAMES[t.ty as usize])),
            _ => ok(Value::Undefined),
        },
        _ => ok(Value::Undefined),
    });
    reg(e, tap, "set", |e, this, a, _, _| {
        let t = this_ta(e, this)?;
        let off = if a.len() > 1 { index_arg(e, a, 1)? } else { 0 };
        let src = arg(a, 0);
        let items: Vec<Value> = match &src {
            Value::Obj(o) => match e.heap.get(*o).kind.clone_ta() {
                Some(s) => {
                    let mut v = Vec::with_capacity(s.len);
                    for i in 0..s.len {
                        v.push(ta_get(e, &s, i).unwrap());
                    }
                    v
                },
                None => match &e.heap.get(*o).kind {
                    Kind::Array(v) => v.iter().map(|x| if let Value::Empty = x { Value::Undefined } else { x.clone() }).collect(),
                    _ => return Err(Fault::NeedJs),
                },
            },
            _ => return e.throw_type("invalid argument to TypedArray.prototype.set"),
        };
        if off + items.len() > t.len {
            return Err(Fault::Throw(e.range_error("offset is out of bounds")));
        }
        for (i, v) in items.iter().enumerate() {
            ta_set(e, &t, off + i, v)?;
        }
        ok(Value::Undefined)
    }, 1);
    reg(e, tap, "subarray", |e, this, a, _, _| {
        let t = this_ta(e, this)?;
        let s = rel_arg(e, a, 0, t.len, 0)?;
        let end = rel_arg(e, a, 1, t.len, t.len)?;
        let n = end.saturating_sub(s);
        ok(Value::Obj(new_typed(e, t.ty, t.buf, t.offset + s * t.elem_size(), n)))
    }, 2);
    reg(e, tap, "slice", |e, this, a, _, _| {
        let t = this_ta(e, this)?;
        let s = rel_arg(e, a, 0, t.len, 0)?;
        let end = rel_arg(e, a, 1, t.len, t.len)?;
        let n = end.saturating_sub(s);
        let es = t.elem_size();
        let bytes = buf_bytes(e, t.buf)[t.offset + s * es..t.offset + (s + n) * es].to_vec();
        let buf = new_buffer(e, bytes);
        ok(Value::Obj(new_typed(e, t.ty, buf, 0, n)))
    }, 2);
    reg(e, tap, "fill", |e, this, a, _, _| {
        let t = this_ta(e, this)?;
        let v = arg(a, 0);
        let s = rel_arg(e, a, 1, t.len, 0)?;
        let end = rel_arg(e, a, 2, t.len, t.len)?;
        for i in s..end {
            ta_set(e, &t, i, &v)?;
        }
        ok(this.clone())
    }, 1);
    reg(e, tap, "reverse", |e, this, _, _, _| {
        let t = this_ta(e, this)?;
        let es = t.elem_size();
        let bytes = buf_bytes_mut(e, t.buf);
        let (mut i, mut j) = (0usize, t.len);
        while i + 1 < j {
            j -= 1;
            for k2 in 0..es {
                bytes.swap(t.offset + i * es + k2, t.offset + j * es + k2);
            }
            i += 1;
        }
        ok(this.clone())
    }, 0);
    reg(e, tap, "copyWithin", |e, this, a, _, _| {
        let t = this_ta(e, this)?;
        let to = rel_arg(e, a, 0, t.len, 0)?;
        let from = rel_arg(e, a, 1, t.len, 0)?;
        let end = rel_arg(e, a, 2, t.len, t.len)?;
        let n = end.saturating_sub(from).min(t.len - to);
        let es = t.elem_size();
        let bytes = buf_bytes_mut(e, t.buf);
        bytes.copy_within(t.offset + from * es..t.offset + (from + n) * es, t.offset + to * es);
        ok(this.clone())
    }, 2);
    reg(e, tap, "at", |e, this, a, _, _| {
        let t = this_ta(e, this)?;
        let n = e.to_number(&arg(a, 0))?;
        let n = if n.is_nan() { 0.0 } else { libm::trunc(n) };
        let i = if n < 0.0 { t.len as f64 + n } else { n };
        if i < 0.0 || i >= t.len as f64 {
            return ok(Value::Undefined);
        }
        ok(ta_get(e, &t, i as usize).unwrap_or(Value::Undefined))
    }, 1);
    for (ty, name) in TA_NAMES.iter().enumerate() {
        let p = new_proto(e, name, Some(tap), None);
        let idx = e.natives.len() as u16;
        e.natives.push((leak(name), |e, _, a, data, construct| {
            if !construct {
                return e.throw_type("Constructor requires 'new'");
            }
            let ty = match data {
                Value::Num(n) => *n as u8,
                _ => 0,
            };
            ok(Value::Obj(make_typed(e, ty, a)?))
        }));
        let c = e.new_native(name, idx, 3, Value::Num(ty as f64));
        let g = e.realm.global;
        e.define(g, name, Value::Obj(c), HIDDEN);
        e.define_atom(c, k::prototype, Value::Obj(p), 0);
        e.define_atom(p, k::constructor, Value::Obj(c), HIDDEN);
        e.heap.get_mut(c).proto = Some(tac);
        let bpe = Value::Num(ta_size(ty as u8) as f64);
        e.define(c, "BYTES_PER_ELEMENT", bpe.clone(), 0);
        e.define(p, "BYTES_PER_ELEMENT", bpe, 0);
    }
    // DataView
    let dp = new_proto(e, "DataView", Some(op), Some("DataView"));
    ctor(e, "DataView", |e, _, a, _, construct| {
        if !construct {
            return e.throw_type("Constructor DataView requires 'new'");
        }
        let b = match arg(a, 0) {
            Value::Obj(o) if matches!(e.heap.get(o).kind, Kind::ArrayBuffer(_)) => o,
            _ => return e.throw_type("First argument to DataView constructor must be an ArrayBuffer"),
        };
        let blen = buf_bytes(e, b).len();
        let off = if a.len() > 1 { index_arg(e, a, 1)? } else { 0 };
        let len = match a.get(2) {
            None | Some(Value::Undefined) => blen.saturating_sub(off),
            Some(_) => index_arg(e, a, 2)?,
        };
        if off + len > blen {
            return Err(Fault::Throw(e.range_error("Invalid DataView length")));
        }
        let p = e.proto("DataView");
        let c = e.atom("DataView");
        ok(Value::Obj(e.heap.alloc(Obj::new(Some(p), Kind::DataView(TypedArrayData { buf: b, offset: off, len, ty: 1 }), c))))
    }, 1, dp);
    reg_getter(e, dp, bl, "byteLength", |e, this, _, _, _| ok(Value::Num(this_view(e, this)?.len as f64)));
    reg_getter(e, dp, bo, "byteOffset", |e, this, _, _, _| ok(Value::Num(this_view(e, this)?.offset as f64)));
    reg_getter(e, dp, ba, "buffer", |e, this, _, _, _| ok(Value::Obj(this_view(e, this)?.buf)));
    macro_rules! dv {
        ($get:expr, $set:expr, $ty:expr) => {
            reg(e, dp, $get, |e, this, a, _, _| view_get(e, this, a, $ty), 1);
            reg(e, dp, $set, |e, this, a, _, _| view_set(e, this, a, $ty), 2);
        };
    }
    dv!("getInt8", "setInt8", 0);
    dv!("getUint8", "setUint8", 1);
    dv!("getInt16", "setInt16", 3);
    dv!("getUint16", "setUint16", 4);
    dv!("getInt32", "setInt32", 5);
    dv!("getUint32", "setUint32", 6);
    dv!("getFloat32", "setFloat32", 7);
    dv!("getFloat64", "setFloat64", 8);
    dv!("getBigInt64", "setBigInt64", 9);
    dv!("getBigUint64", "setBigUint64", 10);
    setup_text(e);
}

trait CloneTa {
    fn clone_ta(&self) -> Option<TypedArrayData>;
}
impl CloneTa for Kind {
    fn clone_ta(&self) -> Option<TypedArrayData> {
        match self {
            Kind::TypedArray(t) => Some(*t),
            _ => None,
        }
    }
}

// ── TextEncoder / TextDecoder, atob / btoa ──

/// the bytes a typed array / DataView / ArrayBuffer covers
fn bytes_of(e: &mut Engine, v: &Value) -> NResult<Vec<u8>> {
    if let Value::Obj(o) = v {
        match &e.heap.get(*o).kind {
            Kind::ArrayBuffer(b) => return Ok(b.clone()),
            Kind::TypedArray(t) | Kind::DataView(t) => {
                let t = *t;
                let n = if matches!(e.heap.get(*o).kind, Kind::DataView(_)) { t.len } else { t.len * t.elem_size() };
                return Ok(buf_bytes(e, t.buf)[t.offset..t.offset + n].to_vec());
            },
            _ => {},
        }
    }
    if v.is_undefined() {
        return Ok(vec![]);
    }
    e.throw_type("The provided value is not of type '(ArrayBuffer or ArrayBufferView)'")
}

fn setup_text(e: &mut Engine) {
    let op = e.realm.object_proto;
    let ep = new_proto(e, "TextEncoder", Some(op), Some("TextEncoder"));
    ctor(e, "TextEncoder", |e, _, _, _, _| {
        let p = e.proto("TextEncoder");
        ok(Value::Obj(e.new_object_proto(Some(p))))
    }, 0, ep);
    e.define(ep, "encoding", Value::str("utf-8"), CONFIGURABLE);
    reg(e, ep, "encode", |e, _, a, _, _| {
        let s = match arg(a, 0) {
            Value::Undefined => js(""),
            v => e.to_str(&v)?,
        };
        let bytes = strs::to_rust(&s).into_bytes();
        let n = bytes.len();
        let b = new_buffer(e, bytes);
        ok(Value::Obj(new_typed(e, 1, b, 0, n)))
    }, 1);
    reg(e, ep, "encodeInto", |e, _, a, _, _| {
        let s = e.to_str(&arg(a, 0))?;
        let t = match arg(a, 1) {
            Value::Obj(o) => match e.heap.get(o).kind {
                Kind::TypedArray(t) if t.ty == 1 => t,
                _ => return e.throw_type("encodeInto needs a Uint8Array"),
            },
            _ => return e.throw_type("encodeInto needs a Uint8Array"),
        };
        let text = strs::to_rust(&s);
        let mut written = 0;
        let mut read = 0;
        let mut out = Vec::new();
        for ch in text.chars() {
            let mut buf = [0u8; 4];
            let enc = ch.encode_utf8(&mut buf);
            if written + enc.len() > t.len {
                break;
            }
            out.extend_from_slice(enc.as_bytes());
            written += enc.len();
            read += ch.len_utf16();
        }
        let bytes = buf_bytes_mut(e, t.buf);
        bytes[t.offset..t.offset + out.len()].copy_from_slice(&out);
        let r = e.new_object();
        e.define(r, "read", Value::Num(read as f64), ALL);
        e.define(r, "written", Value::Num(written as f64), ALL);
        ok(Value::Obj(r))
    }, 2);
    let dp = new_proto(e, "TextDecoder", Some(op), Some("TextDecoder"));
    ctor(e, "TextDecoder", |e, _, a, _, _| {
        let label = match arg(a, 0) {
            Value::Undefined => String::from("utf-8"),
            v => strs::to_rust(&e.to_str(&v)?).to_ascii_lowercase(),
        };
        let enc = match label.trim() {
            "utf-8" | "utf8" | "unicode-1-1-utf-8" => "utf-8",
            "utf-16le" | "utf-16" => "utf-16le",
            "latin1" | "iso-8859-1" | "ascii" | "us-ascii" | "windows-1252" => "windows-1252",
            _ => return Err(Fault::Throw(e.range_error(&format!("The encoding label provided ('{}') is invalid.", label)))),
        };
        let p = e.proto("TextDecoder");
        let o = e.new_object_proto(Some(p));
        e.define(o, "encoding", Value::str(enc), CONFIGURABLE);
        let fatal = match arg(a, 1) {
            Value::Obj(opts) => {
                let f = e.get_str(&Value::Obj(opts), "fatal")?;
                e.truthy(&f)
            },
            _ => false,
        };
        e.define(o, "fatal", Value::Bool(fatal), CONFIGURABLE);
        ok(Value::Obj(o))
    }, 0, dp);
    reg(e, dp, "decode", |e, this, a, _, _| {
        let bytes = bytes_of(e, &arg(a, 0))?;
        let enc = match e.get_str(this, "encoding")? {
            Value::Str(s) => strs::to_rust(&s),
            _ => String::from("utf-8"),
        };
        let fatal = {
            let f = e.get_str(this, "fatal")?;
            e.truthy(&f)
        };
        let units: Vec<u16> = match enc.as_str() {
            "utf-16le" => bytes.chunks(2).map(|c| u16::from_le_bytes([c[0], *c.get(1).unwrap_or(&0)])).collect(),
            "windows-1252" => bytes.iter().map(|b| *b as u16).collect(),
            _ => {
                let start = if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) { 3 } else { 0 };
                match core::str::from_utf8(&bytes[start..]) {
                    Ok(s) => s.encode_utf16().collect(),
                    Err(_) if fatal => return e.throw_type("The encoded data was not valid."),
                    Err(_) => String::from_utf8_lossy(&bytes[start..]).encode_utf16().collect(),
                }
            },
        };
        st(strs::from_units(units))
    }, 1);
    let g = e.realm.global;
    reg(e, g, "btoa", |e, _, a, _, _| {
        let s = e.to_str(&arg(a, 0))?;
        let mut bytes = Vec::with_capacity(s.len());
        for &c in s.iter() {
            if c > 255 {
                let p = e.realm.error_proto;
                let err = e.make_error(p, "The string to be encoded contains characters outside of the Latin1 range.");
                if let Value::Obj(o) = &err {
                    e.define_atom(*o, k::name, Value::str("InvalidCharacterError"), HIDDEN);
                }
                return Err(Fault::Throw(err));
            }
            bytes.push(c as u8);
        }
        const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::with_capacity(bytes.len() * 4 / 3 + 4);
        for ch in bytes.chunks(3) {
            let b = [ch[0], *ch.get(1).unwrap_or(&0), *ch.get(2).unwrap_or(&0)];
            let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
            out.push(T[(n >> 18) as usize & 63] as char);
            out.push(T[(n >> 12) as usize & 63] as char);
            out.push(if ch.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
            out.push(if ch.len() > 2 { T[n as usize & 63] as char } else { '=' });
        }
        st(js(&out))
    }, 1);
    reg(e, g, "atob", |e, _, a, _, _| {
        let s = e.to_str(&arg(a, 0))?;
        let mut vals = Vec::new();
        for &c in s.iter() {
            let v = match c {
                0x41..=0x5A => c - 0x41,
                0x61..=0x7A => c - 0x61 + 26,
                0x30..=0x39 => c - 0x30 + 52,
                0x2B | 0x2D => 62,
                0x2F | 0x5F => 63,
                0x3D | 0x20 | 0x09 | 0x0A | 0x0C | 0x0D => continue,
                _ => {
                    let p = e.realm.error_proto;
                    let err = e.make_error(p, "The string to be decoded is not correctly encoded.");
                    if let Value::Obj(o) = &err {
                        e.define_atom(*o, k::name, Value::str("InvalidCharacterError"), HIDDEN);
                    }
                    return Err(Fault::Throw(err));
                },
            };
            vals.push(v as u32);
        }
        let mut out = Vec::with_capacity(vals.len() * 3 / 4);
        for ch in vals.chunks(4) {
            let mut n = 0u32;
            for (i, v) in ch.iter().enumerate() {
                n |= v << (18 - 6 * i);
            }
            out.push((n >> 16) as u16 & 0xFF);
            if ch.len() > 2 {
                out.push((n >> 8) as u16 & 0xFF);
            }
            if ch.len() > 3 {
                out.push(n as u16 & 0xFF);
            }
        }
        st(strs::from_units(out))
    }, 1);
}

// ── Proxy, Reflect ──

fn make_proxy(e: &mut Engine, a: &[Value]) -> NResult<ObjRef> {
    let (t, h) = match (arg(a, 0), arg(a, 1)) {
        (Value::Obj(t), Value::Obj(h)) if !e.is_heap_prim(&Value::Obj(t)) && !e.is_heap_prim(&Value::Obj(h)) => (t, h),
        _ => return e.throw_type("Cannot create proxy with a non-object as target or handler"),
    };
    let callable = e.heap.get(t).is_callable();
    let proto = e.heap.get(t).proto;
    let class = e.heap.get(t).class;
    let pd = ProxyData { target: Some(t), handler: Some(h), callable };
    Ok(e.heap.alloc(Obj::new(proto, Kind::Proxy(Box::new(pd)), class)))
}

fn target_obj(e: &mut Engine, v: &Value, what: &str) -> NResult<ObjRef> {
    match v {
        Value::Obj(o) if !e.is_heap_prim(v) => Ok(*o),
        _ => e.throw_type(&format!("Reflect.{} called on non-object", what)),
    }
}

/// a proxy's trap for a Reflect call: Some((trap, handler, target))
fn trap_of(e: &mut Engine, o: ObjRef, name: &str) -> NResult<Option<(Value, ObjRef, ObjRef)>> {
    if let Some((t, h)) = e.proxy_parts(o)? {
        if let Some(f) = e.proxy_trap(h, name)? {
            return Ok(Some((f, h, t)));
        }
    }
    Ok(None)
}

fn list_from(e: &mut Engine, v: &Value) -> NResult<Vec<Value>> {
    match v {
        Value::Obj(o) => match &e.heap.get(*o).kind {
            Kind::Array(v) | Kind::Arguments(v) => Ok(v.iter().map(|x| if let Value::Empty = x { Value::Undefined } else { x.clone() }).collect()),
            _ => {
                let n = crate::builtins::length_of(e, v)?;
                let mut out = Vec::with_capacity(n);
                for i in 0..n {
                    out.push(e.get(v, Key::Index(i as u32))?);
                }
                Ok(out)
            },
        },
        _ => e.throw_type("CreateListFromArrayLike called on non-object"),
    }
}

pub(crate) fn setup_reflect(e: &mut Engine) {
    let op = e.realm.object_proto;
    let pc = {
        let idx = e.natives.len() as u16;
        e.natives.push(("Proxy", |e, _, a, _, construct| {
            if !construct {
                return e.throw_type("Constructor Proxy requires 'new'");
            }
            ok(Value::Obj(make_proxy(e, a)?))
        }));
        let c = e.new_native("Proxy", idx, 2, Value::Undefined);
        e.define_atom(c, k::prototype, Value::Undefined, 0);
        // a constructor without a prototype property
        let g = e.realm.global;
        e.define(g, "Proxy", Value::Obj(c), HIDDEN);
        c
    };
    e.realm.protos.insert("%Proxy", pc);
    reg(e, pc, "revocable", |e, _, a, _, _| {
        let p = make_proxy(e, a)?;
        let idx = e.natives.len() as u16 - 1;
        let _ = idx;
        let rv = e.global_native("__revoke");
        let revoke = {
            let (i, _) = match &e.heap.get(rv).kind {
                Kind::Native(i, d) => (*i, d.clone()),
                _ => unreachable!(),
            };
            e.new_native("", i, 0, Value::Obj(p))
        };
        let r = e.new_object();
        e.define(r, "proxy", Value::Obj(p), ALL);
        e.define(r, "revoke", Value::Obj(revoke), ALL);
        ok(Value::Obj(r))
    }, 2);
    let idx = e.natives.len() as u16;
    e.natives.push(("revoke", |e, _, _, data, _| {
        if let Value::Obj(p) = data {
            if let Kind::Proxy(pd) = &mut e.heap.get_mut(*p).kind {
                pd.target = None;
                pd.handler = None;
            }
        }
        ok(Value::Undefined)
    }));
    let rv = e.new_native("revoke", idx, 0, Value::Undefined);
    e.realm.protos.insert("%revoke", rv);

    let r = e.new_object();
    let g = e.realm.global;
    e.define(g, "Reflect", Value::Obj(r), HIDDEN);
    e.define_atom(r, sym::toStringTag, Value::str("Reflect"), CONFIGURABLE);
    let _ = op;
    reg(e, r, "apply", |e, _, a, _, _| {
        let args = list_from(e, &arg(a, 2))?;
        Ok(NOut::TailCall(arg(a, 0), arg(a, 1), args))
    }, 3);
    reg(e, r, "construct", |e, _, a, _, _| {
        let f = arg(a, 0);
        if !e.is_constructor(&f) {
            return e.throw_type("Reflect.construct: target is not a constructor");
        }
        let args = list_from(e, &arg(a, 1))?;
        let nt = if a.len() > 2 { arg(a, 2) } else { f.clone() };
        if !e.is_constructor(&nt) {
            return e.throw_type("Reflect.construct: newTarget is not a constructor");
        }
        Ok(NOut::TailConstruct(f, args, nt))
    }, 2);
    reg(e, r, "ownKeys", |e, _, a, _, _| {
        let o = target_obj(e, &arg(a, 0), "ownKeys")?;
        if let Some((f, h, t)) = trap_of(e, o, "ownKeys")? {
            return Ok(NOut::TailCall(f, Value::Obj(h), vec![Value::Obj(t)]));
        }
        let mut ks = e.own_keys(o, false);
        ks.extend(e.own_symbol_keys(o, false));
        ok(Value::Obj(e.new_array(ks)))
    }, 1);
    reg(e, r, "getOwnPropertyDescriptor", |e, _, a, _, _| {
        let o = target_obj(e, &arg(a, 0), "getOwnPropertyDescriptor")?;
        if let Some((f, h, t)) = trap_of(e, o, "getOwnPropertyDescriptor")? {
            let kv = arg(a, 1);
            return Ok(NOut::TailCall(f, Value::Obj(h), vec![Value::Obj(t), kv]));
        }
        let key = e.to_key(&arg(a, 1))?;
        match e.get_own(o, key) {
            None => ok(Value::Undefined),
            Some(s) => ok(Value::Obj(crate::builtins::descriptor(e, s))),
        }
    }, 2);
    reg(e, r, "defineProperty", |e, _, a, _, _| {
        let o = target_obj(e, &arg(a, 0), "defineProperty")?;
        if let Some((f, h, t)) = trap_of(e, o, "defineProperty")? {
            return Ok(NOut::TailCall(f, Value::Obj(h), vec![Value::Obj(t), arg(a, 1), arg(a, 2)]));
        }
        let key = e.to_key(&arg(a, 1))?;
        let d = match arg(a, 2) {
            Value::Obj(d) => d,
            _ => return e.throw_type("Property description must be an object"),
        };
        match crate::builtins::define_property(e, o, key, d) {
            Ok(()) => ok(Value::Bool(true)),
            Err(Fault::Throw(_)) => ok(Value::Bool(false)),
            Err(f) => Err(f),
        }
    }, 3);
    reg(e, r, "getPrototypeOf", |e, _, a, _, _| {
        let o = target_obj(e, &arg(a, 0), "getPrototypeOf")?;
        if let Some((f, h, t)) = trap_of(e, o, "getPrototypeOf")? {
            return Ok(NOut::TailCall(f, Value::Obj(h), vec![Value::Obj(t)]));
        }
        ok(e.heap.get(o).proto.map(Value::Obj).unwrap_or(Value::Null))
    }, 1);
    reg(e, r, "setPrototypeOf", |e, _, a, _, _| {
        let o = target_obj(e, &arg(a, 0), "setPrototypeOf")?;
        if let Some((f, h, t)) = trap_of(e, o, "setPrototypeOf")? {
            return Ok(NOut::TailCall(f, Value::Obj(h), vec![Value::Obj(t), arg(a, 1)]));
        }
        let p = match arg(a, 1) {
            Value::Obj(p) => Some(p),
            Value::Null => None,
            _ => return e.throw_type("Object prototype may only be an Object or null"),
        };
        let mut cur = p;
        while let Some(c) = cur {
            if c == o {
                return ok(Value::Bool(false));
            }
            cur = e.heap.get(c).proto;
        }
        if !e.heap.get(o).extensible && e.heap.get(o).proto != p {
            return ok(Value::Bool(false));
        }
        e.heap.get_mut(o).proto = p;
        ok(Value::Bool(true))
    }, 2);
    reg(e, r, "isExtensible", |e, _, a, _, _| {
        let o = target_obj(e, &arg(a, 0), "isExtensible")?;
        if let Some((f, h, t)) = trap_of(e, o, "isExtensible")? {
            return Ok(NOut::TailCall(f, Value::Obj(h), vec![Value::Obj(t)]));
        }
        let o = match e.heap.get(o).kind {
            Kind::Proxy(ref p) => p.target.unwrap_or(o),
            _ => o,
        };
        ok(Value::Bool(e.heap.get(o).extensible))
    }, 1);
    reg(e, r, "preventExtensions", |e, _, a, _, _| {
        let o = target_obj(e, &arg(a, 0), "preventExtensions")?;
        if let Some((f, h, t)) = trap_of(e, o, "preventExtensions")? {
            return Ok(NOut::TailCall(f, Value::Obj(h), vec![Value::Obj(t)]));
        }
        e.heap.get_mut(o).extensible = false;
        ok(Value::Bool(true))
    }, 1);
    // get / set / has / deleteProperty go through the VM (getters, setters, traps): the prelude
}

impl Engine {
    pub(crate) fn global_native(&mut self, name: &'static str) -> ObjRef {
        let _ = name;
        self.proto("%revoke")
    }
}

// ── BigInt ──

fn to_bigint(e: &mut Engine, v: &Value) -> NResult<crate::bigint::BigInt> {
    use crate::bigint::BigInt;
    if let Some(b) = e.bigint_of(v) {
        return Ok((*b).clone());
    }
    match v {
        Value::Num(n) => match BigInt::from_f64(*n) {
            Some(b) => Ok(b),
            None => Err(Fault::Throw(e.range_error(&format!("The number {} cannot be converted to a BigInt because it is not an integer", crate::num::to_string_rust(*n))))),
        },
        Value::Bool(b) => Ok(BigInt::from_i64(*b as i64)),
        Value::Str(s) => match BigInt::parse(&strs::to_rust(s)) {
            Some(b) => Ok(b),
            None => Err(Fault::Throw(e.syntax_error(&format!("Cannot convert {} to a BigInt", strs::to_rust(s))))),
        },
        Value::Obj(_) => {
            let p = e.to_primitive(v, true)?;
            to_bigint(e, &p)
        },
        _ => e.throw_type("Cannot convert undefined or null to a BigInt"),
    }
}

fn this_big(e: &mut Engine, this: &Value) -> NResult<Rc<crate::bigint::BigInt>> {
    match e.bigint_of(this) {
        Some(b) => Ok(b),
        None => e.throw_type("BigInt.prototype method called on a non-BigInt"),
    }
}

pub(crate) fn setup_bigint(e: &mut Engine) {
    let op = e.realm.object_proto;
    let p = new_proto(e, "BigInt", Some(op), Some("BigInt"));
    let c = ctor(e, "BigInt", |e, _, a, _, construct| {
        if construct {
            return e.throw_type("BigInt is not a constructor");
        }
        let b = to_bigint(e, &arg(a, 0))?;
        ok(e.new_bigint(b))
    }, 1, p);
    reg(e, c, "asUintN", |e, _, a, _, _| {
        let bits = e.to_number(&arg(a, 0))? as u64;
        let b = to_bigint(e, &arg(a, 1))?;
        ok(e.new_bigint(b.as_uint_n(bits)))
    }, 2);
    reg(e, c, "asIntN", |e, _, a, _, _| {
        let bits = e.to_number(&arg(a, 0))? as u64;
        let b = to_bigint(e, &arg(a, 1))?;
        ok(e.new_bigint(b.as_int_n(bits)))
    }, 2);
    reg(e, p, "toString", |e, this, a, _, _| {
        let b = this_big(e, this)?;
        let r = match arg(a, 0) {
            Value::Undefined => 10,
            v => e.to_number(&v)? as u32,
        };
        if !(2..=36).contains(&r) {
            return Err(Fault::Throw(e.range_error("toString() radix must be between 2 and 36")));
        }
        st(js(&b.to_string_radix(r)))
    }, 0);
    reg(e, p, "toLocaleString", |e, this, _, _, _| {
        let b = this_big(e, this)?;
        st(js(&b.to_string_radix(10)))
    }, 0);
    reg(e, p, "valueOf", |e, this, _, _, _| {
        this_big(e, this)?;
        ok(this.clone())
    }, 0);
}
