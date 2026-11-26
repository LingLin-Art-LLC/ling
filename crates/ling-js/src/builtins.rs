//! The standard library's native half. Whatever would have to call back into script (a
//! callback, a getter, a user toString) answers `Fault::NeedJs`, and the prelude's JavaScript
//! version of that builtin takes over.

use crate::atoms::k;
use crate::object::*;
use crate::regex::Regex;
use crate::strs::{self, js, JsStr};
use crate::value::Value;
use crate::vm::{Engine, Fault, Key, NOut, NResult, NativeFn};
use crate::num::F64Ext;
use alloc::boxed::Box;
use alloc::format;
use alloc::rc::Rc;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

pub(crate) type R = NResult<NOut>;

pub(crate) fn ok(v: Value) -> R {
    Ok(NOut::Val(v))
}
pub(crate) fn num(n: f64) -> R {
    Ok(NOut::Val(Value::Num(n)))
}
pub(crate) fn st(s: JsStr) -> R {
    Ok(NOut::Val(Value::Str(s)))
}
pub(crate) fn arg(a: &[Value], i: usize) -> Value {
    a.get(i).cloned().unwrap_or(Value::Undefined)
}

pub fn reg(e: &mut Engine, obj: ObjRef, name: &str, f: NativeFn, len: u32) -> ObjRef {
    let idx = e.natives.len() as u16;
    e.natives.push((leak(name), f));
    let fo = e.new_native(name, idx, len, Value::Undefined);
    e.define(obj, name, Value::Obj(fo), HIDDEN);
    fo
}

pub(crate) fn leak(s: &str) -> &'static str {
    // names of builtins: a fixed, small set
    Box::leak(String::from(s).into_boxed_str())
}

pub(crate) fn getter(e: &mut Engine, obj: ObjRef, name: &str, f: NativeFn) {
    let idx = e.natives.len() as u16;
    e.natives.push((leak(name), f));
    let fo = e.new_native(&format!("get {}", name), idx, 0, Value::Undefined);
    let a = e.atom(name);
    e.heap.get_mut(obj).props.insert(a, Slot::Accessor(Some(fo), None, CONFIGURABLE));
}

/// a constructor: `name` on the global object, with its prototype object
pub(crate) fn ctor(e: &mut Engine, name: &str, f: NativeFn, len: u32, proto: ObjRef) -> ObjRef {
    let g = e.realm.global;
    let c = reg(e, g, name, f, len);
    e.define_atom(c, k::prototype, Value::Obj(proto), 0);
    e.define_atom(proto, k::constructor, Value::Obj(c), HIDDEN);
    c
}

pub(crate) fn this_obj(e: &mut Engine, this: &Value) -> NResult<ObjRef> {
    e.to_object(this)
}

fn this_str(e: &mut Engine, this: &Value) -> NResult<JsStr> {
    if this.is_nullish() {
        return e.throw_type("String.prototype method called on null or undefined");
    }
    e.to_str(this)
}

pub(crate) fn int_arg(e: &mut Engine, a: &[Value], i: usize, d: f64) -> NResult<f64> {
    match a.get(i) {
        None | Some(Value::Undefined) => Ok(d),
        Some(v) => {
            let n = e.to_number(v)?;
            Ok(if n.is_nan() { 0.0 } else { libm::trunc(n) })
        },
    }
}

/// a relative index (negative from the end), clamped to 0..=len
pub(crate) fn rel(n: f64, len: usize) -> usize {
    let l = len as f64;
    if n < 0.0 {
        (l + n).max(0.0) as usize
    } else {
        n.min(l) as usize
    }
}

pub(crate) fn elements_of(e: &Engine, v: &Value) -> Option<Vec<Value>> {
    match v {
        Value::Obj(o) => match &e.heap.get(*o).kind {
            Kind::Array(v) => Some(v.clone()),
            _ => None,
        },
        _ => None,
    }
}

pub(crate) fn is_array(e: &Engine, v: &Value) -> bool {
    matches!(v, Value::Obj(o) if matches!(e.heap.get(*o).kind, Kind::Array(_)))
}

/// the length of an array-like (plain reads only)
pub(crate) fn length_of(e: &mut Engine, o: &Value) -> NResult<usize> {
    if let Value::Obj(r) = o {
        if let Some(el) = e.heap.get(*r).elements() {
            if matches!(e.heap.get(*r).kind, Kind::Array(_)) {
                return Ok(el.len());
            }
        }
    }
    let l = e.get(o, Key::Atom(k::length))?;
    let n = e.to_number(&l)?;
    Ok(if n.is_nan() || n <= 0.0 { 0 } else { n.min(9007199254740991.0) as usize })
}

pub fn setup(e: &mut Engine) {
    let oc = e.atom("Object");
    let fc = e.atom("Function");
    let object_proto = e.heap.alloc(Obj::new(None, Kind::Ordinary, oc));
    // Function.prototype is itself callable (it returns undefined)
    let idx = e.natives.len() as u16;
    e.natives.push(("", |_, _, _, _, _| ok(Value::Undefined)));
    let function_proto = e.heap.alloc(Obj::new(Some(object_proto), Kind::Native(idx, Value::Undefined), fc));
    e.realm.object_proto = object_proto;
    e.realm.function_proto = function_proto;
    let mk = |e: &mut Engine, class: &str, kind: Kind| {
        let c = e.atom(class);
        e.heap.alloc(Obj::new(Some(object_proto), kind, c))
    };
    e.realm.array_proto = mk(e, "Array", Kind::Array(vec![]));
    e.realm.string_proto = mk(e, "String", Kind::StrBox(js("")));
    e.realm.number_proto = mk(e, "Number", Kind::NumBox(0.0));
    e.realm.boolean_proto = mk(e, "Boolean", Kind::BoolBox(false));
    e.realm.error_proto = mk(e, "Error", Kind::Ordinary);
    e.realm.regexp_proto = mk(e, "Object", Kind::Ordinary);
    e.realm.date_proto = mk(e, "Object", Kind::Ordinary);
    let global = mk(e, "global", Kind::Ordinary);
    e.realm.global = global;
    for name in ["TypeError", "RangeError", "SyntaxError", "ReferenceError", "EvalError", "URIError"] {
        let c = e.atom("Error");
        let p = e.heap.alloc(Obj::new(Some(e.realm.error_proto), Kind::Ordinary, c));
        match name {
            "TypeError" => e.realm.type_error_proto = p,
            "RangeError" => e.realm.range_error_proto = p,
            "SyntaxError" => e.realm.syntax_error_proto = p,
            "ReferenceError" => e.realm.reference_error_proto = p,
            "EvalError" => e.realm.eval_error_proto = p,
            _ => e.realm.uri_error_proto = p,
        }
    }
    e.define(global, "globalThis", Value::Obj(global), HIDDEN);
    e.define(global, "NaN", Value::Num(f64::NAN), 0);
    e.define(global, "Infinity", Value::Num(f64::INFINITY), 0);
    e.define(global, "undefined", Value::Undefined, 0);

    setup_object(e);
    setup_function(e);
    setup_array(e);
    setup_string(e);
    setup_number(e);
    setup_math(e);
    setup_json(e);
    setup_errors(e);
    setup_regexp(e);
    setup_date(e);
    setup_global_fns(e);
    crate::builtins2::setup(e);
}

// ── Object ──

fn setup_object(e: &mut Engine) {
    let p = e.realm.object_proto;
    let c = ctor(e, "Object", |e, _, a, _, _| {
        let v = arg(a, 0);
        if v.is_nullish() {
            return ok(Value::Obj(e.new_object()));
        }
        ok(Value::Obj(e.to_object(&v)?))
    }, 1, p);
    reg(e, c, "keys", |e, _, a, _, _| {
        let o = e.to_object(&arg(a, 0))?;
        if matches!(e.heap.get(o).kind, Kind::Host(..) | Kind::Proxy(_)) {
            return Err(Fault::NeedJs);
        }
        let ks = e.own_keys(o, true);
        ok(Value::Obj(e.new_array(ks)))
    }, 1);
    reg(e, c, "values", |e, _, a, _, _| {
        let ov = arg(a, 0);
        let o = e.to_object(&ov)?;
        let ks = e.own_keys(o, true);
        let mut out = Vec::new();
        for kv in ks {
            let key = e.to_key(&kv)?;
            out.push(e.get(&Value::Obj(o), key)?);
        }
        ok(Value::Obj(e.new_array(out)))
    }, 1);
    reg(e, c, "entries", |e, _, a, _, _| {
        let o = e.to_object(&arg(a, 0))?;
        let ks = e.own_keys(o, true);
        let mut out = Vec::new();
        for kv in ks {
            let key = e.to_key(&kv)?;
            let v = e.get(&Value::Obj(o), key)?;
            let pair = e.new_array(vec![kv, v]);
            out.push(Value::Obj(pair));
        }
        ok(Value::Obj(e.new_array(out)))
    }, 1);
    reg(e, c, "assign", |e, _, a, _, _| {
        let t = e.to_object(&arg(a, 0))?;
        for src in a.iter().skip(1) {
            if src.is_nullish() {
                continue;
            }
            let s = e.to_object(src)?;
            let mut ks = e.own_keys(s, true);
            ks.extend(e.own_symbol_keys(s, true));
            for kv in ks {
                let key = e.to_key(&kv)?;
                let v = e.get(&Value::Obj(s), key)?;
                e.put(&Value::Obj(t), key, v)?;
            }
        }
        ok(Value::Obj(t))
    }, 2);
    reg(e, c, "create", |e, _, a, _, _| {
        let proto = match arg(a, 0) {
            Value::Obj(p) => Some(p),
            Value::Null => None,
            _ => return e.throw_type("Object prototype may only be an Object or null"),
        };
        let o = e.new_object_proto(proto);
        if let Value::Obj(props) = arg(a, 1) {
            define_properties(e, o, props)?;
        }
        ok(Value::Obj(o))
    }, 2);
    reg(e, c, "defineProperty", |e, _, a, _, _| {
        let o = match arg(a, 0) {
            Value::Obj(o) => o,
            _ => return e.throw_type("Object.defineProperty called on non-object"),
        };
        if matches!(e.heap.get(o).kind, Kind::Host(..)) {
            // a host object gets an engine-side property (the host is not asked)
        }
        let key = e.to_key(&arg(a, 1))?;
        let d = match arg(a, 2) {
            Value::Obj(d) => d,
            _ => return e.throw_type("Property description must be an object"),
        };
        define_property(e, o, key, d)?;
        ok(Value::Obj(o))
    }, 3);
    reg(e, c, "defineProperties", |e, _, a, _, _| {
        let o = match arg(a, 0) {
            Value::Obj(o) => o,
            _ => return e.throw_type("Object.defineProperties called on non-object"),
        };
        let props = e.to_object(&arg(a, 1))?;
        define_properties(e, o, props)?;
        ok(Value::Obj(o))
    }, 2);
    reg(e, c, "getOwnPropertyNames", |e, _, a, _, _| {
        let o = e.to_object(&arg(a, 0))?;
        let ks = e.own_keys(o, false);
        ok(Value::Obj(e.new_array(ks)))
    }, 1);
    reg(e, c, "getOwnPropertySymbols", |e, _, _, _, _| ok(Value::Obj(e.new_array(vec![]))), 1);
    reg(e, c, "getOwnPropertyDescriptor", |e, _, a, _, _| {
        let o = e.to_object(&arg(a, 0))?;
        let key = e.to_key(&arg(a, 1))?;
        match e.get_own(o, key) {
            None => ok(Value::Undefined),
            Some(s) => ok(Value::Obj(descriptor(e, s))),
        }
    }, 2);
    reg(e, c, "getOwnPropertyDescriptors", |e, _, a, _, _| {
        let o = e.to_object(&arg(a, 0))?;
        let ks = e.own_keys(o, false);
        let out = e.new_object();
        for kv in ks {
            let key = e.to_key(&kv)?;
            if let Some(s) = e.get_own(o, key) {
                let d = descriptor(e, s);
                e.define_own(out, key, Slot::Data(Value::Obj(d), ALL));
            }
        }
        ok(Value::Obj(out))
    }, 1);
    reg(e, c, "getPrototypeOf", |e, _, a, _, _| {
        let v = arg(a, 0);
        let o = e.to_object(&v)?;
        ok(e.heap.get(o).proto.map(Value::Obj).unwrap_or(Value::Null))
    }, 1);
    reg(e, c, "setPrototypeOf", |e, _, a, _, _| {
        if let Value::Obj(o) = arg(a, 0) {
            match arg(a, 1) {
                Value::Obj(p) => e.heap.get_mut(o).proto = Some(p),
                Value::Null => e.heap.get_mut(o).proto = None,
                _ => return e.throw_type("Object prototype may only be an Object or null"),
            }
        }
        ok(arg(a, 0))
    }, 2);
    fn set_level(e: &mut Engine, v: &Value, freeze: bool) {
        if let Value::Obj(o) = v {
            let o = *o;
            e.heap.get_mut(o).extensible = false;
            let obj = e.heap.get_mut(o);
            for s in obj.props.slots.iter_mut() {
                match s {
                    Slot::Data(_, f) => {
                        *f &= !CONFIGURABLE;
                        if freeze {
                            *f &= !WRITABLE;
                        }
                    },
                    Slot::Accessor(_, _, f) => *f &= !CONFIGURABLE,
                }
            }
            if freeze {
                // frozen arrays: their elements become read-only properties
                if let Some(el) = obj.elements_mut() {
                    let items = core::mem::take(el);
                    for (i, x) in items.into_iter().enumerate() {
                        if !matches!(x, Value::Empty) {
                            let a = e.index_atom(i as u32);
                            e.heap.get_mut(o).props.insert(a, Slot::Data(x, ENUMERABLE));
                        }
                    }
                }
            }
        }
    }
    reg(e, c, "freeze", |e, _, a, _, _| {
        let v = arg(a, 0);
        if let Value::Obj(o) = &v {
            if let Kind::Array(el) = &e.heap.get(*o).kind {
                // keep arrays dense: remember frozenness by a non-extensible flag only
                let _ = el;
                e.heap.get_mut(*o).extensible = false;
                return ok(v);
            }
        }
        set_level(e, &v, true);
        ok(v)
    }, 1);
    reg(e, c, "seal", |e, _, a, _, _| {
        let v = arg(a, 0);
        set_level(e, &v, false);
        ok(v)
    }, 1);
    reg(e, c, "preventExtensions", |e, _, a, _, _| {
        if let Value::Obj(o) = arg(a, 0) {
            e.heap.get_mut(o).extensible = false;
        }
        ok(arg(a, 0))
    }, 1);
    reg(e, c, "isExtensible", |e, _, a, _, _| ok(Value::Bool(matches!(arg(a, 0), Value::Obj(o) if e.heap.get(o).extensible))), 1);
    reg(e, c, "isFrozen", |e, _, a, _, _| {
        ok(Value::Bool(match arg(a, 0) {
            Value::Obj(o) => {
                let obj = e.heap.get(o);
                !obj.extensible && obj.props.slots.iter().all(|s| s.flags() & (CONFIGURABLE | WRITABLE) == 0 || matches!(s, Slot::Accessor(..)) && s.flags() & CONFIGURABLE == 0)
            },
            _ => true,
        }))
    }, 1);
    reg(e, c, "isSealed", |e, _, a, _, _| {
        ok(Value::Bool(match arg(a, 0) {
            Value::Obj(o) => {
                let obj = e.heap.get(o);
                !obj.extensible && obj.props.slots.iter().all(|s| s.flags() & CONFIGURABLE == 0)
            },
            _ => true,
        }))
    }, 1);
    reg(e, c, "is", |_, _, a, _, _| ok(Value::Bool(arg(a, 0).same_value(&arg(a, 1)))), 2);
    reg(e, c, "hasOwn", |e, _, a, _, _| {
        let o = e.to_object(&arg(a, 0))?;
        let key = e.to_key(&arg(a, 1))?;
        ok(Value::Bool(e.get_own(o, key).is_some()))
    }, 2);

    reg(e, p, "hasOwnProperty", |e, this, a, _, _| {
        let key = e.to_key(&arg(a, 0))?;
        let o = this_obj(e, this)?;
        if matches!(e.heap.get(o).kind, Kind::Host(..)) {
            return Err(Fault::NeedJs);
        }
        ok(Value::Bool(e.get_own(o, key).is_some()))
    }, 1);
    reg(e, p, "isPrototypeOf", |e, this, a, _, _| {
        let (Value::Obj(me), Value::Obj(v)) = (this.clone(), arg(a, 0)) else { return ok(Value::Bool(false)) };
        let mut cur = e.heap.get(v).proto;
        while let Some(c) = cur {
            if c == me {
                return ok(Value::Bool(true));
            }
            cur = e.heap.get(c).proto;
        }
        ok(Value::Bool(false))
    }, 1);
    reg(e, p, "propertyIsEnumerable", |e, this, a, _, _| {
        let key = e.to_key(&arg(a, 0))?;
        let o = this_obj(e, this)?;
        ok(Value::Bool(e.get_own(o, key).is_some_and(|s| s.flags() & ENUMERABLE != 0)))
    }, 1);
    reg(e, p, "toString", |e, this, _, _, _| {
        let tag = match this {
            Value::Undefined => "Undefined".to_string(),
            Value::Null => "Null".to_string(),
            Value::Num(_) => "Number".into(),
            Value::Str(_) => "String".into(),
            Value::Bool(_) => "Boolean".into(),
            Value::Obj(o) => {
                let o = *o;
                let obj = e.heap.get(o);
                let base = match &obj.kind {
                    Kind::Array(_) => "Array".to_string(),
                    Kind::Arguments(_) => "Arguments".into(),
                    Kind::Closure(_) | Kind::Native(..) | Kind::Bound(_) | Kind::HostFn(_) => "Function".into(),
                    Kind::Error => "Error".into(),
                    Kind::BoolBox(_) => "Boolean".into(),
                    Kind::NumBox(_) => "Number".into(),
                    Kind::StrBox(_) => "String".into(),
                    Kind::Date(_) => "Date".into(),
                    Kind::RegExp(_) => "RegExp".into(),
                    Kind::Host(c, _) => strs::to_rust(e.atoms.name(*c)),
                    Kind::Symbol(_) => "Symbol".into(),
                    Kind::BigInt(_) => "BigInt".into(),
                    _ => "Object".into(),
                };
                // a Symbol.toStringTag stand-in: a string own / inherited "@@toStringTag"
                let tag_atom = crate::atoms::sym::toStringTag;
                match e.find_slot(o, Key::Atom(tag_atom)) {
                    Some(Slot::Data(Value::Str(s), _)) => strs::to_rust(&s),
                    Some(Slot::Accessor(Some(g), _, _)) => match e.call_native_direct(Value::Obj(g), this.clone(), vec![], 0) {
                        Ok(Value::Str(s)) => strs::to_rust(&s),
                        Ok(_) => base,
                        Err(_) => base,
                    },
                    _ => base,
                }
            },
            Value::Empty => "Undefined".into(),
        };
        st(js(&format!("[object {}]", tag)))
    }, 0);
    reg(e, p, "toLocaleString", |e, this, _, _, _| {
        let f = e.get_str(this, "toString")?;
        Ok(NOut::TailCall(f, this.clone(), vec![]))
    }, 0);
    // toLocaleString calls this.toString(): done by the prelude (it needs a call)
    reg(e, p, "valueOf", |e, this, _, _, _| ok(Value::Obj(this_obj(e, this)?)), 0);
    // __proto__ accessor
    let idx_g = e.natives.len() as u16;
    e.natives.push(("__proto__", |e, this, _, _, _| {
        let o = e.to_object(this)?;
        ok(e.heap.get(o).proto.map(Value::Obj).unwrap_or(Value::Null))
    }));
    let g = e.new_native("get __proto__", idx_g, 0, Value::Undefined);
    let idx_s = e.natives.len() as u16;
    e.natives.push(("__proto__", |e, this, a, _, _| {
        if let Value::Obj(o) = this {
            match arg(a, 0) {
                Value::Obj(p) => e.heap.get_mut(*o).proto = Some(p),
                Value::Null => e.heap.get_mut(*o).proto = None,
                _ => {},
            }
        }
        ok(Value::Undefined)
    }));
    let s = e.new_native("set __proto__", idx_s, 1, Value::Undefined);
    let pa = e.atom("__proto__");
    e.heap.get_mut(p).props.insert(pa, Slot::Accessor(Some(g), Some(s), CONFIGURABLE));
}

pub(crate) fn descriptor(e: &mut Engine, s: Slot) -> ObjRef {
    let d = e.new_object();
    match s {
        Slot::Data(v, f) => {
            e.define_atom(d, k::value, v, ALL);
            e.define_atom(d, k::writable, Value::Bool(f & WRITABLE != 0), ALL);
            e.define_atom(d, k::enumerable, Value::Bool(f & ENUMERABLE != 0), ALL);
            e.define_atom(d, k::configurable, Value::Bool(f & CONFIGURABLE != 0), ALL);
        },
        Slot::Accessor(g, s, f) => {
            e.define_atom(d, k::get, g.map(Value::Obj).unwrap_or(Value::Undefined), ALL);
            e.define_atom(d, k::set, s.map(Value::Obj).unwrap_or(Value::Undefined), ALL);
            e.define_atom(d, k::enumerable, Value::Bool(f & ENUMERABLE != 0), ALL);
            e.define_atom(d, k::configurable, Value::Bool(f & CONFIGURABLE != 0), ALL);
        },
    }
    d
}

fn define_properties(e: &mut Engine, o: ObjRef, props: ObjRef) -> NResult<()> {
    let ks = e.own_keys(props, true);
    for kv in ks {
        let key = e.to_key(&kv)?;
        let d = match e.get(&Value::Obj(props), key)? {
            Value::Obj(d) => d,
            _ => return e.throw_type("Property description must be an object"),
        };
        define_property(e, o, key, d)?;
    }
    Ok(())
}

pub(crate) fn define_property(e: &mut Engine, o: ObjRef, key: Key, d: ObjRef) -> NResult<()> {
    let dv = Value::Obj(d);
    let field = |e: &mut Engine, a| -> NResult<Option<Value>> {
        if e.find_slot(d, Key::Atom(a)).is_some() {
            Ok(Some(e.get(&dv, Key::Atom(a))?))
        } else {
            Ok(None)
        }
    };
    let get = field(e, k::get)?;
    let set = field(e, k::set)?;
    let value = field(e, k::value)?;
    let writable = field(e, k::writable)?;
    let enumerable = field(e, k::enumerable)?;
    let configurable = field(e, k::configurable)?;
    let old = e.get_own(o, key);
    if let Some(old) = &old {
        if old.flags() & CONFIGURABLE == 0 {
            // a non-configurable property may only have its value changed (when writable)
            if let (Slot::Data(_, f), Some(v)) = (old, &value) {
                if f & WRITABLE != 0 && get.is_none() && set.is_none() {
                    let f = *f;
                    let wr = match &writable {
                        Some(w) => e.truthy(w),
                        None => true,
                    };
                    let nf = if wr { f } else { f & !WRITABLE };
                    e.define_own(o, key, Slot::Data(v.clone(), nf));
                    return Ok(());
                }
            }
            if value.is_none() && get.is_none() && set.is_none() {
                return Ok(());
            }
            let n = e.key_name(key);
            return e.throw_type(&format!("Cannot redefine property: {}", strs::to_rust(&n)));
        }
    }
    let flag = |e: &Engine, v: &Option<Value>, bit: u8, oldf: u8| -> u8 {
        match v {
            Some(x) => {
                if e.truthy(x) {
                    bit
                } else {
                    0
                }
            },
            None => oldf & bit,
        }
    };
    let oldf = old.as_ref().map(|s| s.flags()).unwrap_or(0);
    if get.is_some() || set.is_some() {
        let as_fn = |v: Option<Value>| match v {
            Some(Value::Obj(f)) => Some(f),
            _ => None,
        };
        let (og, os) = match &old {
            Some(Slot::Accessor(g, s, _)) => (*g, *s),
            _ => (None, None),
        };
        let g = if get.is_some() { as_fn(get) } else { og };
        let s = if set.is_some() { as_fn(set) } else { os };
        let f = flag(e, &enumerable, ENUMERABLE, oldf) | flag(e, &configurable, CONFIGURABLE, oldf);
        e.define_own(o, key, Slot::Accessor(g, s, f));
    } else {
        let v = match (value, &old) {
            (Some(v), _) => v,
            (None, Some(Slot::Data(v, _))) => v.clone(),
            _ => Value::Undefined,
        };
        let f = flag(e, &writable, WRITABLE, oldf) | flag(e, &enumerable, ENUMERABLE, oldf) | flag(e, &configurable, CONFIGURABLE, oldf);
        // array length / elements
        if let (Kind::Array(_), Key::Atom(a)) = (&e.heap.get(o).kind, key) {
            if a == k::length {
                let n = e.to_number(&v)?;
                if let Some(el) = e.heap.get_mut(o).elements_mut() {
                    el.resize(n as usize, Value::Empty);
                }
                return Ok(());
            }
        }
        e.define_own(o, key, Slot::Data(v, f));
    }
    Ok(())
}

// ── Function ──

fn setup_function(e: &mut Engine) {
    let p = e.realm.function_proto;
    ctor(e, "Function", |e, _, a, _, _| {
        // Function(p1, p2, ..., body): compiled now, in the global scope
        let mut params = Vec::new();
        for x in a.iter().take(a.len().saturating_sub(1)) {
            params.push(strs::to_rust(&e.to_str(x)?));
        }
        let body = match a.last() {
            Some(b) => strs::to_rust(&e.to_str(b)?),
            None => String::new(),
        };
        let src = format!("(function anonymous({}
) {{
{}
}})", params.join(","), body);
        let p = e.compile_runtime(&src)?;
        Ok(NOut::RunScript(p))
    }, 1, p);
    let g = e.realm.global;
    reg(e, g, "eval", |e, _, a, _, _| {
        // indirect eval semantics (the global scope), for direct calls too
        let src = match arg(a, 0) {
            Value::Str(s) => strs::to_rust(&s),
            other => return ok(other),
        };
        let p = e.compile_runtime(&src)?;
        Ok(NOut::RunScript(p))
    }, 1);
    reg(e, p, "call", |_, this, a, _, _| Ok(NOut::TailCall(this.clone(), arg(a, 0), a.iter().skip(1).cloned().collect())), 1);
    reg(e, p, "apply", |e, this, a, _, _| {
        let args = match arg(a, 1) {
            Value::Undefined | Value::Null => vec![],
            v => {
                if let Some(el) = elements_of(e, &v) {
                    el.into_iter().map(|x| if let Value::Empty = x { Value::Undefined } else { x }).collect()
                } else if let Value::Obj(o) = &v {
                    if let Kind::Arguments(el) = &e.heap.get(*o).kind {
                        el.clone()
                    } else {
                        let n = length_of(e, &v)?;
                        let mut out = Vec::with_capacity(n);
                        for i in 0..n {
                            out.push(e.get(&v, Key::Index(i as u32))?);
                        }
                        out
                    }
                } else {
                    return e.throw_type("CreateListFromArrayLike called on non-object");
                }
            },
        };
        Ok(NOut::TailCall(this.clone(), arg(a, 0), args))
    }, 2);
    reg(e, p, "bind", |e, this, a, _, _| {
        let t = match this {
            Value::Obj(o) if e.heap.get(*o).is_callable() => *o,
            _ => return e.throw_type("Bind must be called on a function"),
        };
        let c = e.atom("Function");
        let b = Bound { target: t, this: arg(a, 0), args: a.iter().skip(1).cloned().collect() };
        let o = e.heap.alloc(Obj::new(Some(e.realm.function_proto), Kind::Bound(Box::new(b)), c));
        ok(Value::Obj(o))
    }, 1);
    reg(e, p, "toString", |e, this, _, _, _| {
        let o = match this {
            Value::Obj(o) => *o,
            _ => return e.throw_type("Function.prototype.toString requires that 'this' be a Function"),
        };
        let text = match &e.heap.get(o).kind {
            Kind::Closure(c) => match &c.func.src {
                Some((src, a, b)) => src.chars().skip(*a).take(b - a).collect::<String>(),
                None => "function () { [native code] }".into(),
            },
            Kind::Native(..) | Kind::Bound(_) | Kind::HostFn(_) => {
                let n = match e.get_own(o, Key::Atom(k::name)) {
                    Some(Slot::Data(Value::Str(s), _)) => strs::to_rust(&s),
                    _ => String::new(),
                };
                format!("function {}() {{ [native code] }}", n)
            },
            Kind::Proxy(p) if p.callable => "function () { [native code] }".into(),
            // a page object standing in for a constructor (HTMLElement ...): lenient
            Kind::Host(c, _) => format!("function {}() {{ [native code] }}", strs::to_rust(e.atoms.name(*c))),
            _ => return e.throw_type("Function.prototype.toString requires that 'this' be a Function"),
        };
        st(js(&text))
    }, 0);
}

// ── Array ──

fn setup_array(e: &mut Engine) {
    let p = e.realm.array_proto;
    let c = ctor(e, "Array", |e, _, a, _, _| {
        if a.len() == 1 {
            if let Value::Num(n) = a[0] {
                if n < 0.0 || n.fract() != 0.0 || n > 4294967295.0 {
                    return Err(Fault::Throw(e.range_error("Invalid array length")));
                }
                return ok(Value::Obj(e.new_array(vec![Value::Empty; n as usize])));
            }
        }
        ok(Value::Obj(e.new_array(a.to_vec())))
    }, 1, p);
    reg(e, c, "isArray", |e, _, a, _, _| ok(Value::Bool(is_array(e, &arg(a, 0)))), 1);
    reg(e, c, "of", |e, _, a, _, _| ok(Value::Obj(e.new_array(a.to_vec()))), 0);

    fn arr_mut<'a>(e: &'a mut Engine, this: &Value) -> NResult<&'a mut Vec<Value>> {
        match this {
            Value::Obj(o) if matches!(e.heap.get(*o).kind, Kind::Array(_)) && e.heap.get(*o).extensible => Ok(e.heap.get_mut(*o).elements_mut().unwrap()),
            _ => Err(Fault::NeedJs),
        }
    }
    fn arr(e: &Engine, this: &Value) -> NResult<Vec<Value>> {
        match this {
            Value::Obj(o) => match &e.heap.get(*o).kind {
                Kind::Array(v) => Ok(v.clone()),
                _ => Err(Fault::NeedJs),
            },
            _ => Err(Fault::NeedJs),
        }
    }
    fn hole(v: Value) -> Value {
        if let Value::Empty = v {
            Value::Undefined
        } else {
            v
        }
    }
    reg(e, p, "push", |e, this, a, _, _| {
        let el = arr_mut(e, this)?;
        el.extend_from_slice(a);
        num(el.len() as f64)
    }, 1);
    reg(e, p, "pop", |e, this, _, _, _| {
        let el = arr_mut(e, this)?;
        ok(hole(el.pop().unwrap_or(Value::Undefined)))
    }, 0);
    reg(e, p, "shift", |e, this, _, _, _| {
        let el = arr_mut(e, this)?;
        if el.is_empty() {
            return ok(Value::Undefined);
        }
        ok(hole(el.remove(0)))
    }, 0);
    reg(e, p, "unshift", |e, this, a, _, _| {
        let el = arr_mut(e, this)?;
        let mut v = a.to_vec();
        v.append(el);
        *el = v;
        num(el.len() as f64)
    }, 1);
    reg(e, p, "slice", |e, this, a, _, _| {
        let el = arr(e, this)?;
        let s = int_arg(e, a, 0, 0.0)?;
        let en = int_arg(e, a, 1, el.len() as f64)?;
        let (s, en) = (rel(s, el.len()), rel(en, el.len()));
        let out = if s < en { el[s..en].to_vec() } else { vec![] };
        ok(Value::Obj(e.new_array(out)))
    }, 2);
    reg(e, p, "splice", |e, this, a, _, _| {
        let len = arr(e, this)?.len();
        let s = rel(int_arg(e, a, 0, 0.0)?, len);
        let dc = if a.len() < 2 { len - s } else { (int_arg(e, a, 1, 0.0)?.max(0.0) as usize).min(len - s) };
        let items: Vec<Value> = if a.len() > 2 { a[2..].to_vec() } else { vec![] };
        let el = arr_mut(e, this)?;
        let removed: Vec<Value> = if a.is_empty() { vec![] } else { el.splice(s..s + dc, items).collect() };
        ok(Value::Obj(e.new_array(removed)))
    }, 2);
    reg(e, p, "concat", |e, this, a, _, _| {
        let mut out = arr(e, this)?;
        for x in a {
            match elements_of(e, x) {
                Some(v) => out.extend(v),
                None => out.push(x.clone()),
            }
        }
        ok(Value::Obj(e.new_array(out)))
    }, 1);
    reg(e, p, "join", |e, this, a, _, _| {
        let el = arr(e, this)?;
        let sep = match arg(a, 0) {
            Value::Undefined => js(","),
            v => e.to_str(&v)?,
        };
        let mut out: Vec<u16> = Vec::new();
        for (i, x) in el.iter().enumerate() {
            if i > 0 {
                out.extend_from_slice(&sep);
            }
            match x {
                Value::Undefined | Value::Null | Value::Empty => {},
                v => {
                    if let Value::Obj(o) = v {
                        if let Kind::Array(_) = e.heap.get(*o).kind {
                            // nested arrays join with "," (plain reads)
                        } else {
                            // objects may have a user toString
                        }
                    }
                    let s = e.to_str(v)?;
                    out.extend_from_slice(&s);
                },
            }
        }
        st(strs::from_units(out))
    }, 1);
    reg(e, p, "toString", |e, this, _, _, _| {
        let f = e.get_str(this, "join")?;
        Ok(NOut::TailCall(f, this.clone(), vec![]))
    }, 0);
    reg(e, p, "reverse", |e, this, _, _, _| {
        let el = arr_mut(e, this)?;
        el.reverse();
        ok(this.clone())
    }, 0);
    reg(e, p, "indexOf", |e, this, a, _, _| {
        let el = arr(e, this)?;
        let x = arg(a, 0);
        let from = rel(int_arg(e, a, 1, 0.0)?, el.len());
        for (i, v) in el.iter().enumerate().skip(from) {
            if v.strict_eq(&x) && !matches!(v, Value::Empty) {
                return num(i as f64);
            }
        }
        num(-1.0)
    }, 1);
    reg(e, p, "lastIndexOf", |e, this, a, _, _| {
        let el = arr(e, this)?;
        let x = arg(a, 0);
        let from = if a.len() > 1 { int_arg(e, a, 1, 0.0)? } else { el.len() as f64 - 1.0 };
        let from = if from < 0.0 { el.len() as f64 + from } else { from.min(el.len() as f64 - 1.0) };
        let mut i = from as isize;
        while i >= 0 {
            if el[i as usize].strict_eq(&x) {
                return num(i as f64);
            }
            i -= 1;
        }
        num(-1.0)
    }, 1);
    reg(e, p, "includes", |e, this, a, _, _| {
        let el = arr(e, this)?;
        let x = arg(a, 0);
        let from = rel(int_arg(e, a, 1, 0.0)?, el.len());
        ok(Value::Bool(el.iter().skip(from).any(|v| hole(v.clone()).same_value_zero(&x))))
    }, 1);
    reg(e, p, "fill", |e, this, a, _, _| {
        let len = arr(e, this)?.len();
        let v = arg(a, 0);
        let s = rel(int_arg(e, a, 1, 0.0)?, len);
        let en = rel(int_arg(e, a, 2, len as f64)?, len);
        let el = arr_mut(e, this)?;
        for x in el.iter_mut().take(en).skip(s) {
            *x = v.clone();
        }
        ok(this.clone())
    }, 1);
    reg(e, p, "at", |e, this, a, _, _| {
        let el = arr(e, this)?;
        let i = int_arg(e, a, 0, 0.0)?;
        let i = if i < 0.0 { el.len() as f64 + i } else { i };
        ok(if i >= 0.0 && (i as usize) < el.len() { hole(el[i as usize].clone()) } else { Value::Undefined })
    }, 1);
}

// ── String ──

fn setup_string(e: &mut Engine) {
    let p = e.realm.string_proto;
    let c = ctor(e, "String", |e, _, a, _, construct| {
        let s = if a.is_empty() {
            js("")
        } else if let (Some(sa), false) = (e.sym_of(&a[0]), construct) {
            js(&format!("Symbol({})", strs::to_rust(e.atoms.name(sa))))
        } else {
            e.to_str(&a[0])?
        };
        if construct {
            let c = e.atom("String");
            let o = e.heap.alloc(Obj::new(Some(e.realm.string_proto), Kind::StrBox(s), c));
            return ok(Value::Obj(o));
        }
        st(s)
    }, 1, p);
    reg(e, c, "fromCharCode", |e, _, a, _, _| {
        let mut out = Vec::with_capacity(a.len());
        for x in a {
            out.push(crate::num::to_uint32(e.to_number(x)?) as u16);
        }
        st(strs::from_units(out))
    }, 1);
    reg(e, c, "fromCodePoint", |e, _, a, _, _| {
        let mut out = Vec::new();
        for x in a {
            let n = e.to_number(x)?;
            if n < 0.0 || n > 1114111.0 || n.fract() != 0.0 {
                return Err(Fault::Throw(e.range_error("Invalid code point")));
            }
            crate::lexer::push_cp(&mut out, n as u32);
        }
        st(strs::from_units(out))
    }, 1);
    fn sv(e: &mut Engine, this: &Value) -> NResult<JsStr> {
        match this {
            Value::Str(s) => Ok(s.clone()),
            Value::Obj(o) => match &e.heap.get(*o).kind {
                Kind::StrBox(s) => Ok(s.clone()),
                _ => this_str(e, this),
            },
            _ => this_str(e, this),
        }
    }
    reg(e, p, "toString", |e, this, _, _, _| match this {
        Value::Str(s) => st(s.clone()),
        Value::Obj(o) => match &e.heap.get(*o).kind {
            Kind::StrBox(s) => st(s.clone()),
            _ => e.throw_type("String.prototype.toString requires that 'this' be a String"),
        },
        _ => e.throw_type("String.prototype.toString requires that 'this' be a String"),
    }, 0);
    reg(e, p, "valueOf", |e, this, _, _, _| st(sv(e, this)?), 0);
    reg(e, p, "charAt", |e, this, a, _, _| {
        let s = sv(e, this)?;
        let i = int_arg(e, a, 0, 0.0)?;
        st(if i >= 0.0 && (i as usize) < s.len() { Rc::from(vec![s[i as usize]]) } else { js("") })
    }, 1);
    reg(e, p, "charCodeAt", |e, this, a, _, _| {
        let s = sv(e, this)?;
        let i = int_arg(e, a, 0, 0.0)?;
        num(if i >= 0.0 && (i as usize) < s.len() { s[i as usize] as f64 } else { f64::NAN })
    }, 1);
    reg(e, p, "codePointAt", |e, this, a, _, _| {
        let s = sv(e, this)?;
        let i = int_arg(e, a, 0, 0.0)?;
        if i < 0.0 || (i as usize) >= s.len() {
            return ok(Value::Undefined);
        }
        let i = i as usize;
        let c = s[i] as u32;
        if (0xD800..0xDC00).contains(&c) && i + 1 < s.len() && (0xDC00..0xE000).contains(&(s[i + 1] as u32)) {
            return num((0x10000 + ((c - 0xD800) << 10) + (s[i + 1] as u32 - 0xDC00)) as f64);
        }
        num(c as f64)
    }, 1);
    reg(e, p, "at", |e, this, a, _, _| {
        let s = sv(e, this)?;
        let i = int_arg(e, a, 0, 0.0)?;
        let i = if i < 0.0 { s.len() as f64 + i } else { i };
        ok(if i >= 0.0 && (i as usize) < s.len() { Value::Str(Rc::from(vec![s[i as usize]])) } else { Value::Undefined })
    }, 1);
    reg(e, p, "indexOf", |e, this, a, _, _| {
        let s = sv(e, this)?;
        let n = e.to_str(&arg(a, 0))?;
        let from = (int_arg(e, a, 1, 0.0)?.max(0.0) as usize).min(s.len());
        num(strs::index_of(&s, &n, from).map(|i| i as f64).unwrap_or(-1.0))
    }, 1);
    reg(e, p, "lastIndexOf", |e, this, a, _, _| {
        let s = sv(e, this)?;
        let n = e.to_str(&arg(a, 0))?;
        let from = match a.get(1) {
            Some(v) => {
                let x = e.to_number(v)?;
                if x.is_nan() { s.len() } else { x.max(0.0) as usize }
            },
            None => s.len(),
        };
        num(strs::last_index_of(&s, &n, from).map(|i| i as f64).unwrap_or(-1.0))
    }, 1);
    reg(e, p, "includes", |e, this, a, _, _| {
        let s = sv(e, this)?;
        if let Value::Obj(o) = arg(a, 0) {
            if let Kind::RegExp(_) = e.heap.get(o).kind {
                return e.throw_type("First argument to String.prototype.includes must not be a regular expression");
            }
        }
        let n = e.to_str(&arg(a, 0))?;
        let from = (int_arg(e, a, 1, 0.0)?.max(0.0) as usize).min(s.len());
        ok(Value::Bool(strs::index_of(&s, &n, from).is_some()))
    }, 1);
    reg(e, p, "startsWith", |e, this, a, _, _| {
        let s = sv(e, this)?;
        let n = e.to_str(&arg(a, 0))?;
        let at = (int_arg(e, a, 1, 0.0)?.max(0.0) as usize).min(s.len());
        ok(Value::Bool(s[at..].starts_with(&n)))
    }, 1);
    reg(e, p, "endsWith", |e, this, a, _, _| {
        let s = sv(e, this)?;
        let n = e.to_str(&arg(a, 0))?;
        let end = (int_arg(e, a, 1, s.len() as f64)?.max(0.0) as usize).min(s.len());
        ok(Value::Bool(s[..end].ends_with(&n)))
    }, 1);
    reg(e, p, "slice", |e, this, a, _, _| {
        let s = sv(e, this)?;
        let b = rel(int_arg(e, a, 0, 0.0)?, s.len());
        let en = rel(int_arg(e, a, 1, s.len() as f64)?, s.len());
        st(if b < en { Rc::from(&s[b..en]) } else { js("") })
    }, 2);
    reg(e, p, "substring", |e, this, a, _, _| {
        let s = sv(e, this)?;
        let x = (int_arg(e, a, 0, 0.0)?.max(0.0) as usize).min(s.len());
        let y = (int_arg(e, a, 1, s.len() as f64)?.max(0.0) as usize).min(s.len());
        let (b, en) = if x < y { (x, y) } else { (y, x) };
        st(Rc::from(&s[b..en]))
    }, 2);
    reg(e, p, "substr", |e, this, a, _, _| {
        let s = sv(e, this)?;
        let b = rel(int_arg(e, a, 0, 0.0)?, s.len());
        let n = int_arg(e, a, 1, (s.len() - b) as f64)?.max(0.0) as usize;
        let en = (b + n).min(s.len());
        st(Rc::from(&s[b..en]))
    }, 2);
    reg(e, p, "toLowerCase", |e, this, _, _, _| st(strs::lower(&sv(e, this)?)), 0);
    reg(e, p, "toUpperCase", |e, this, _, _, _| st(strs::upper(&sv(e, this)?)), 0);
    reg(e, p, "toLocaleLowerCase", |e, this, _, _, _| st(strs::lower(&sv(e, this)?)), 0);
    reg(e, p, "toLocaleUpperCase", |e, this, _, _, _| st(strs::upper(&sv(e, this)?)), 0);
    reg(e, p, "trim", |e, this, _, _, _| st(Rc::from(strs::trim(&sv(e, this)?, true, true))), 0);
    reg(e, p, "trimStart", |e, this, _, _, _| st(Rc::from(strs::trim(&sv(e, this)?, true, false))), 0);
    reg(e, p, "trimEnd", |e, this, _, _, _| st(Rc::from(strs::trim(&sv(e, this)?, false, true))), 0);
    reg(e, p, "trimLeft", |e, this, _, _, _| st(Rc::from(strs::trim(&sv(e, this)?, true, false))), 0);
    reg(e, p, "trimRight", |e, this, _, _, _| st(Rc::from(strs::trim(&sv(e, this)?, false, true))), 0);
    fn pad(e: &mut Engine, this: &Value, a: &[Value], start: bool) -> R {
        let s = sv(e, this)?;
        let n = int_arg(e, a, 0, 0.0)?.max(0.0) as usize;
        let fill = match arg(a, 1) {
            Value::Undefined => js(" "),
            v => e.to_str(&v)?,
        };
        if n <= s.len() || fill.is_empty() {
            return st(s);
        }
        let need = n - s.len();
        let mut f: Vec<u16> = Vec::with_capacity(need);
        while f.len() < need {
            f.extend_from_slice(&fill);
        }
        f.truncate(need);
        st(if start { strs::concat(&f, &s) } else { strs::concat(&s, &f) })
    }
    reg(e, p, "padStart", |e, this, a, _, _| pad(e, this, a, true), 2);
    reg(e, p, "padEnd", |e, this, a, _, _| pad(e, this, a, false), 2);
    reg(e, p, "repeat", |e, this, a, _, _| {
        let s = sv(e, this)?;
        let n = int_arg(e, a, 0, 0.0)?;
        if n < 0.0 || n.is_infinite() || (n * s.len() as f64) > 1e9 {
            return Err(Fault::Throw(e.range_error("Invalid count value")));
        }
        st(Rc::from(s.repeat(n as usize)))
    }, 1);
    reg(e, p, "concat", |e, this, a, _, _| {
        let mut s = sv(e, this)?.to_vec();
        for x in a {
            s.extend_from_slice(&e.to_str(x)?);
        }
        st(strs::from_units(s))
    }, 1);
    reg(e, p, "localeCompare", |e, this, a, _, _| {
        let s = sv(e, this)?;
        let o = e.to_str(&arg(a, 0))?;
        let (x, y) = (strs::to_rust(&s), strs::to_rust(&o));
        let (lx, ly) = (x.to_lowercase(), y.to_lowercase());
        num(match lx.cmp(&ly).then(x.cmp(&y)) {
            core::cmp::Ordering::Less => -1.0,
            core::cmp::Ordering::Equal => 0.0,
            core::cmp::Ordering::Greater => 1.0,
        })
    }, 1);
    reg(e, p, "normalize", |e, this, _, _, _| st(sv(e, this)?), 0);
    reg(e, p, "split", |e, this, a, _, _| {
        let s = sv(e, this)?;
        let lim = match arg(a, 1) {
            Value::Undefined => u32::MAX as usize,
            v => crate::num::to_uint32(e.to_number(&v)?) as usize,
        };
        let sep = arg(a, 0);
        let mut out: Vec<Value> = Vec::new();
        if let Value::Obj(o) = &sep {
            if let Kind::RegExp(rd) = &e.heap.get(*o).kind {
                let re = rd.re.clone();
                if s.is_empty() {
                    let m = re.exec(&s, 0).map_err(|m| Fault::Throw(e.syntax_error(&m)))?;
                    if m.is_none() {
                        out.push(Value::Str(s.clone()));
                    }
                    return ok(Value::Obj(e.new_array(out)));
                }
                let mut p0 = 0;
                let mut q = 0;
                while q < s.len() && out.len() < lim {
                    let m = re.exec(&s, q).map_err(|m| Fault::Throw(e.syntax_error(&m)))?;
                    let caps = match m {
                        None => break,
                        Some(c) => c,
                    };
                    let (ms, me) = caps[0].unwrap();
                    if me == ms && ms >= s.len() {
                        break;
                    }
                    if me == p0 || (ms == me && ms == q && me == p0) {
                        q = ms.max(q) + 1;
                        continue;
                    }
                    if ms >= s.len() {
                        break;
                    }
                    out.push(Value::Str(Rc::from(&s[p0..ms])));
                    for c in caps.iter().skip(1) {
                        if out.len() >= lim {
                            break;
                        }
                        out.push(match c {
                            Some((a, b)) => Value::Str(Rc::from(&s[*a..*b])),
                            None => Value::Undefined,
                        });
                    }
                    p0 = me;
                    q = if me == ms { me + 1 } else { me };
                }
                if out.len() < lim {
                    out.push(Value::Str(Rc::from(&s[p0.min(s.len())..])));
                }
                return ok(Value::Obj(e.new_array(out)));
            }
        }
        if sep.is_undefined() {
            out.push(Value::Str(s));
            return ok(Value::Obj(e.new_array(out)));
        }
        let sep = e.to_str(&sep)?;
        if sep.is_empty() {
            for c in s.iter().take(lim) {
                out.push(Value::Str(Rc::from(vec![*c])));
            }
            return ok(Value::Obj(e.new_array(out)));
        }
        let mut start = 0;
        while out.len() < lim {
            match strs::index_of(&s, &sep, start) {
                Some(i) => {
                    out.push(Value::Str(Rc::from(&s[start..i])));
                    start = i + sep.len();
                },
                None => {
                    out.push(Value::Str(Rc::from(&s[start..])));
                    break;
                },
            }
        }
        ok(Value::Obj(e.new_array(out)))
    }, 2);
    reg(e, p, "replace", |e, this, a, _, _| replace(e, this, a, false), 2);
    reg(e, p, "replaceAll", |e, this, a, _, _| replace(e, this, a, true), 2);
    reg(e, p, "match", |e, this, a, _, _| {
        let s = sv(e, this)?;
        let re = match arg(a, 0) {
            Value::Obj(o) if matches!(e.heap.get(o).kind, Kind::RegExp(_)) => Value::Obj(o),
            Value::Undefined => new_regexp(e, &js(""), &js(""))?,
            v => {
                let src = e.to_str(&v)?;
                new_regexp(e, &src, &js(""))?
            },
        };
        let ro = re.as_obj().unwrap();
        let global = match &e.heap.get(ro).kind {
            Kind::RegExp(r) => r.re.flags.global,
            _ => false,
        };
        if !global {
            return regexp_exec(e, &re, &s).map(NOut::Val);
        }
        let mut out = Vec::new();
        let mut pos = 0;
        while pos <= s.len() {
            let m = run_regex(e, ro, &s, pos)?;
            match m {
                None => break,
                Some(c) => {
                    let (ms, me) = c[0].unwrap();
                    out.push(Value::Str(Rc::from(&s[ms..me])));
                    pos = if me == ms { me + 1 } else { me };
                },
            }
        }
        set_last_index(e, ro, 0.0);
        if out.is_empty() {
            return ok(Value::Null);
        }
        ok(Value::Obj(e.new_array(out)))
    }, 1);
    reg(e, p, "search", |e, this, a, _, _| {
        let s = sv(e, this)?;
        let re = match arg(a, 0) {
            Value::Obj(o) if matches!(e.heap.get(o).kind, Kind::RegExp(_)) => o,
            v => {
                let src = if v.is_undefined() { js("") } else { e.to_str(&v)? };
                new_regexp(e, &src, &js(""))?.as_obj().unwrap()
            },
        };
        let m = run_regex(e, re, &s, 0)?;
        num(m.map(|c| c[0].unwrap().0 as f64).unwrap_or(-1.0))
    }, 1);
}

/// String.prototype.replace / replaceAll with a string or regexp and a string replacement
/// (a function replacement goes to the prelude)
fn replace(e: &mut Engine, this: &Value, a: &[Value], all: bool) -> R {
    let s = this_str(e, this)?;
    let rep = arg(a, 1);
    if e.is_callable(&rep) {
        return Err(Fault::NeedJs);
    }
    let rep = e.to_str(&rep)?;
    let pat = arg(a, 0);
    let mut out: Vec<u16> = Vec::new();
    if let Value::Obj(o) = &pat {
        if let Kind::RegExp(rd) = &e.heap.get(*o).kind {
            let global = rd.re.flags.global;
            if all && !global {
                return e.throw_type("replaceAll must be called with a global RegExp");
            }
            let names = rd.re.names.clone();
            let mut pos = if global { 0 } else { last_index_start(e, *o) };
            let mut last = 0;
            loop {
                let m = run_regex(e, *o, &s, pos)?;
                let caps = match m {
                    None => break,
                    Some(c) => c,
                };
                let (ms, me) = caps[0].unwrap();
                out.extend_from_slice(&s[last..ms]);
                expand_replacement(&mut out, &rep, &s, &caps, &names);
                last = me;
                if !global {
                    break;
                }
                pos = if me == ms { me + 1 } else { me };
                if pos > s.len() {
                    break;
                }
            }
            if global {
                set_last_index(e, *o, 0.0);
            }
            out.extend_from_slice(&s[last.min(s.len())..]);
            return st(strs::from_units(out));
        }
    }
    let p = e.to_str(&pat)?;
    let mut start = 0;
    let mut last = 0;
    loop {
        let i = match strs::index_of(&s, &p, start) {
            Some(i) if i <= s.len() => i,
            _ => break,
        };
        out.extend_from_slice(&s[last..i]);
        let caps = vec![Some((i, i + p.len()))];
        expand_replacement(&mut out, &rep, &s, &caps, &[]);
        last = i + p.len();
        if !all {
            break;
        }
        start = if p.is_empty() { i + 1 } else { last };
        if start > s.len() {
            break;
        }
    }
    out.extend_from_slice(&s[last.min(s.len())..]);
    st(strs::from_units(out))
}

fn expand_replacement(out: &mut Vec<u16>, rep: &[u16], s: &[u16], caps: &[Option<(usize, usize)>], names: &[(String, usize)]) {
    let (ms, me) = caps[0].unwrap();
    let mut i = 0;
    while i < rep.len() {
        let c = rep[i];
        if c == b'$' as u16 && i + 1 < rep.len() {
            let d = rep[i + 1];
            match d {
                36 => {
                    out.push(36);
                    i += 2;
                    continue;
                },
                38 => {
                    out.extend_from_slice(&s[ms..me]);
                    i += 2;
                    continue;
                },
                96 => {
                    out.extend_from_slice(&s[..ms]);
                    i += 2;
                    continue;
                },
                39 => {
                    out.extend_from_slice(&s[me..]);
                    i += 2;
                    continue;
                },
                48..=57 => {
                    let mut n = (d - 48) as usize;
                    let mut used = 2;
                    if i + 2 < rep.len() && (48..=57).contains(&rep[i + 2]) {
                        let n2 = n * 10 + (rep[i + 2] - 48) as usize;
                        if n2 < caps.len() {
                            n = n2;
                            used = 3;
                        }
                    }
                    if n >= 1 && n < caps.len() {
                        if let Some((a, b)) = caps[n] {
                            out.extend_from_slice(&s[a..b]);
                        }
                        i += used;
                        continue;
                    }
                },
                60 if !names.is_empty() => {
                    if let Some(endp) = rep[i + 2..].iter().position(|&x| x == 62) {
                        let name = strs::to_rust(&rep[i + 2..i + 2 + endp]);
                        if let Some((_, gi)) = names.iter().find(|(n, _)| *n == name) {
                            if let Some(Some((a, b))) = caps.get(*gi) {
                                out.extend_from_slice(&s[*a..*b]);
                            }
                        }
                        i += 3 + endp;
                        continue;
                    }
                },
                _ => {},
            }
        }
        out.push(c);
        i += 1;
    }
}

// ── Number / Boolean ──

fn setup_number(e: &mut Engine) {
    let p = e.realm.number_proto;
    let c = ctor(e, "Number", |e, _, a, _, construct| {
        let n = if a.is_empty() {
            0.0
        } else if let Some(b) = e.bigint_of(&a[0]) {
            b.to_f64()
        } else {
            e.to_number(&a[0])?
        };
        if construct {
            let c = e.atom("Number");
            let o = e.heap.alloc(Obj::new(Some(e.realm.number_proto), Kind::NumBox(n), c));
            return ok(Value::Obj(o));
        }
        num(n)
    }, 1, p);
    e.define(c, "MAX_SAFE_INTEGER", Value::Num(9007199254740991.0), 0);
    e.define(c, "MIN_SAFE_INTEGER", Value::Num(-9007199254740991.0), 0);
    e.define(c, "EPSILON", Value::Num(f64::EPSILON), 0);
    e.define(c, "MAX_VALUE", Value::Num(f64::MAX), 0);
    e.define(c, "MIN_VALUE", Value::Num(5e-324), 0);
    e.define(c, "NaN", Value::Num(f64::NAN), 0);
    e.define(c, "POSITIVE_INFINITY", Value::Num(f64::INFINITY), 0);
    e.define(c, "NEGATIVE_INFINITY", Value::Num(f64::NEG_INFINITY), 0);
    reg(e, c, "isInteger", |_, _, a, _, _| ok(Value::Bool(matches!(arg(a, 0), Value::Num(n) if n.is_finite() && n.fract() == 0.0))), 1);
    reg(e, c, "isSafeInteger", |_, _, a, _, _| ok(Value::Bool(matches!(arg(a, 0), Value::Num(n) if n.is_finite() && n.fract() == 0.0 && n.abs() <= 9007199254740991.0))), 1);
    reg(e, c, "isFinite", |_, _, a, _, _| ok(Value::Bool(matches!(arg(a, 0), Value::Num(n) if n.is_finite()))), 1);
    reg(e, c, "isNaN", |_, _, a, _, _| ok(Value::Bool(matches!(arg(a, 0), Value::Num(n) if n.is_nan()))), 1);
    fn nv(e: &mut Engine, this: &Value) -> NResult<f64> {
        match this {
            Value::Num(n) => Ok(*n),
            Value::Obj(o) => match &e.heap.get(*o).kind {
                Kind::NumBox(n) => Ok(*n),
                _ => e.throw_type("Number.prototype method called on an incompatible receiver"),
            },
            _ => e.throw_type("Number.prototype method called on an incompatible receiver"),
        }
    }
    reg(e, p, "toString", |e, this, a, _, _| {
        let n = nv(e, this)?;
        let r = int_arg(e, a, 0, 10.0)?;
        if !(2.0..=36.0).contains(&r) {
            return Err(Fault::Throw(e.range_error("toString() radix must be between 2 and 36")));
        }
        st(js(&crate::num::radix_string(n, r as u32)))
    }, 1);
    reg(e, p, "toLocaleString", |e, this, _, _, _| {
        let n = nv(e, this)?;
        // en-US grouping
        let s = crate::num::to_string_rust(n);
        if !n.is_finite() || s.contains('e') {
            return st(js(&s));
        }
        let (ip, fp) = match s.split_once('.') {
            Some((a, b)) => (a.to_string(), Some(b.chars().take(3).collect::<String>())),
            None => (s.clone(), None),
        };
        let (neg, digits) = if let Some(d) = ip.strip_prefix('-') { (true, d.to_string()) } else { (false, ip) };
        let mut g = String::new();
        for (i, ch) in digits.chars().enumerate() {
            if i > 0 && (digits.len() - i) % 3 == 0 {
                g.push(',');
            }
            g.push(ch);
        }
        let mut out = if neg { format!("-{}", g) } else { g };
        if let Some(f) = fp {
            let f = f.trim_end_matches('0');
            if !f.is_empty() {
                out = format!("{}.{}", out, f);
            }
        }
        st(js(&out))
    }, 0);
    reg(e, p, "valueOf", |e, this, _, _, _| num(nv(e, this)?), 0);
    reg(e, p, "toFixed", |e, this, a, _, _| {
        let n = nv(e, this)?;
        let f = int_arg(e, a, 0, 0.0)?;
        if !(0.0..=100.0).contains(&f) {
            return Err(Fault::Throw(e.range_error("toFixed() digits argument must be between 0 and 100")));
        }
        st(js(&crate::num::to_fixed(n, f as usize)))
    }, 1);
    reg(e, p, "toPrecision", |e, this, a, _, _| {
        let n = nv(e, this)?;
        if arg(a, 0).is_undefined() {
            return st(js(&crate::num::to_string_rust(n)));
        }
        let pr = int_arg(e, a, 0, 0.0)?;
        if !(1.0..=100.0).contains(&pr) {
            return Err(Fault::Throw(e.range_error("toPrecision() argument must be between 1 and 100")));
        }
        st(js(&crate::num::to_precision(n, pr as usize)))
    }, 1);
    reg(e, p, "toExponential", |e, this, a, _, _| {
        let n = nv(e, this)?;
        let f = if arg(a, 0).is_undefined() { None } else { Some(int_arg(e, a, 0, 0.0)? as usize) };
        st(js(&crate::num::to_exponential(n, f)))
    }, 1);

    let bp = e.realm.boolean_proto;
    ctor(e, "Boolean", |e, _, a, _, construct| {
        let b = e.truthy(&arg(a, 0));
        if construct {
            let c = e.atom("Boolean");
            let o = e.heap.alloc(Obj::new(Some(e.realm.boolean_proto), Kind::BoolBox(b), c));
            return ok(Value::Obj(o));
        }
        ok(Value::Bool(b))
    }, 1, bp);
    fn bv(e: &mut Engine, this: &Value) -> NResult<bool> {
        match this {
            Value::Bool(b) => Ok(*b),
            Value::Obj(o) => match &e.heap.get(*o).kind {
                Kind::BoolBox(b) => Ok(*b),
                _ => e.throw_type("Boolean.prototype method called on an incompatible receiver"),
            },
            _ => e.throw_type("Boolean.prototype method called on an incompatible receiver"),
        }
    }
    reg(e, bp, "toString", |e, this, _, _, _| st(js(if bv(e, this)? { "true" } else { "false" })), 0);
    reg(e, bp, "valueOf", |e, this, _, _, _| ok(Value::Bool(bv(e, this)?)), 0);
}

// ── Math ──

fn setup_math(e: &mut Engine) {
    let g = e.realm.global;
    let m = e.new_object();
    e.define(g, "Math", Value::Obj(m), HIDDEN);
    let tag = crate::atoms::sym::toStringTag;
    e.define_atom(m, tag, Value::str("Math"), CONFIGURABLE);
    for (n, v) in [
        ("PI", core::f64::consts::PI),
        ("E", core::f64::consts::E),
        ("LN2", core::f64::consts::LN_2),
        ("LN10", core::f64::consts::LN_10),
        ("LOG2E", core::f64::consts::LOG2_E),
        ("LOG10E", core::f64::consts::LOG10_E),
        ("SQRT2", core::f64::consts::SQRT_2),
        ("SQRT1_2", core::f64::consts::FRAC_1_SQRT_2),
    ] {
        e.define(m, n, Value::Num(v), 0);
    }
    macro_rules! m1 {
        ($name:expr, $f:expr) => {
            reg(e, m, $name, |e, _, a, _, _| {
                let x = e.to_number(&arg(a, 0))?;
                let f: fn(f64) -> f64 = $f;
                num(f(x))
            }, 1);
        };
    }
    m1!("abs", |x| x.abs());
    m1!("floor", libm::floor);
    m1!("ceil", libm::ceil);
    m1!("trunc", libm::trunc);
    m1!("sign", |x| if x.is_nan() { f64::NAN } else if x > 0.0 { 1.0 } else if x < 0.0 { -1.0 } else { x });
    m1!("sqrt", libm::sqrt);
    m1!("cbrt", libm::cbrt);
    m1!("exp", libm::exp);
    m1!("expm1", libm::expm1);
    m1!("log", libm::log);
    m1!("log2", libm::log2);
    m1!("log10", libm::log10);
    m1!("log1p", libm::log1p);
    m1!("sin", libm::sin);
    m1!("cos", libm::cos);
    m1!("tan", libm::tan);
    m1!("asin", libm::asin);
    m1!("acos", libm::acos);
    m1!("atan", libm::atan);
    m1!("sinh", libm::sinh);
    m1!("cosh", libm::cosh);
    m1!("tanh", libm::tanh);
    m1!("asinh", libm::asinh);
    m1!("acosh", libm::acosh);
    m1!("atanh", libm::atanh);
    m1!("fround", |x| x as f32 as f64);
    m1!("clz32", |x| crate::num::to_uint32(x).leading_zeros() as f64);
    m1!("round", |x| {
        if !x.is_finite() || x == 0.0 {
            return x;
        }
        if x > 0.0 && x < 0.5 {
            return 0.0;
        }
        if x < 0.0 && x >= -0.5 {
            return -0.0;
        }
        libm::floor(x + 0.5)
    });
    reg(e, m, "atan2", |e, _, a, _, _| {
        let y = e.to_number(&arg(a, 0))?;
        let x = e.to_number(&arg(a, 1))?;
        num(libm::atan2(y, x))
    }, 2);
    reg(e, m, "pow", |e, _, a, _, _| {
        let x = e.to_number(&arg(a, 0))?;
        let y = e.to_number(&arg(a, 1))?;
        num(if y.is_nan() || ((x == 1.0 || x == -1.0) && y.is_infinite()) { f64::NAN } else { libm::pow(x, y) })
    }, 2);
    reg(e, m, "imul", |e, _, a, _, _| {
        let x = crate::num::to_int32(e.to_number(&arg(a, 0))?);
        let y = crate::num::to_int32(e.to_number(&arg(a, 1))?);
        num(x.wrapping_mul(y) as f64)
    }, 2);
    reg(e, m, "hypot", |e, _, a, _, _| {
        let mut s = 0.0;
        for x in a {
            let v = e.to_number(x)?;
            if v.is_infinite() {
                return num(f64::INFINITY);
            }
            s += v * v;
        }
        num(libm::sqrt(s))
    }, 2);
    reg(e, m, "max", |e, _, a, _, _| {
        let mut r = f64::NEG_INFINITY;
        for x in a {
            let v = e.to_number(x)?;
            if v.is_nan() {
                return num(f64::NAN);
            }
            if v > r || (v == 0.0 && r == 0.0 && !v.is_sign_negative()) {
                r = v;
            }
        }
        num(r)
    }, 2);
    reg(e, m, "min", |e, _, a, _, _| {
        let mut r = f64::INFINITY;
        for x in a {
            let v = e.to_number(x)?;
            if v.is_nan() {
                return num(f64::NAN);
            }
            if v < r || (v == 0.0 && r == 0.0 && v.is_sign_negative()) {
                r = v;
            }
        }
        num(r)
    }, 2);
    reg(e, m, "random", |e, _, _, _, _| {
        // xorshift64*, seeded by the embedder (Engine::seed_random)
        let mut x = e.random_state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        e.random_state = x;
        let r = x.wrapping_mul(0x2545F4914F6CDD1D) >> 11;
        num(r as f64 / (1u64 << 53) as f64)
    }, 0);
}

// ── JSON ──

fn setup_json(e: &mut Engine) {
    let g = e.realm.global;
    let j = e.new_object();
    e.define(g, "JSON", Value::Obj(j), HIDDEN);
    let tag = crate::atoms::sym::toStringTag;
    e.define_atom(j, tag, Value::str("JSON"), CONFIGURABLE);
    reg(e, j, "parse", |e, _, a, _, _| {
        if e.is_callable(&arg(a, 1)) {
            return Err(Fault::NeedJs);
        }
        let s = e.to_str(&arg(a, 0))?;
        let mut p = JsonP { s: &s, i: 0 };
        p.ws();
        let v = match p.value(e) {
            Ok(v) => v,
            Err(m) => return Err(Fault::Throw(e.syntax_error(&m))),
        };
        p.ws();
        if p.i < s.len() {
            let m = format!("Unexpected non-whitespace character after JSON at position {}", p.i);
            return Err(Fault::Throw(e.syntax_error(&m)));
        }
        ok(v)
    }, 2);
    reg(e, j, "stringify", |e, _, a, _, _| {
        let replacer = arg(a, 1);
        let mut allow: Option<Vec<JsStr>> = None;
        if e.is_callable(&replacer) {
            return Err(Fault::NeedJs);
        }
        if let Some(list) = elements_of(e, &replacer) {
            let mut v = Vec::new();
            for x in list {
                match x {
                    Value::Str(s) => v.push(s),
                    Value::Num(_) => v.push(e.to_str(&x)?),
                    _ => {},
                }
            }
            allow = Some(v);
        }
        let indent: Vec<u16> = match arg(a, 2) {
            Value::Num(n) => vec![32; (n.clamp(0.0, 10.0)) as usize],
            Value::Str(s) => s.iter().take(10).copied().collect(),
            _ => vec![],
        };
        let mut out = Vec::new();
        let mut stack = Vec::new();
        let ok_ = stringify(e, &arg(a, 0), &mut out, &indent, &mut String::new(), &mut stack, &allow)?;
        if !ok_ {
            return ok(Value::Undefined);
        }
        st(strs::from_units(out))
    }, 3);
}

struct JsonP<'a> {
    s: &'a [u16],
    i: usize,
}

impl JsonP<'_> {
    fn ws(&mut self) {
        while self.i < self.s.len() && matches!(self.s[self.i], 32 | 9 | 10 | 13) {
            self.i += 1;
        }
    }
    fn err<T>(&self) -> Result<T, String> {
        if self.i >= self.s.len() {
            Err("Unexpected end of JSON input".into())
        } else {
            Err(format!("Unexpected token '{}' in JSON at position {}", char::from_u32(self.s[self.i] as u32).unwrap_or('?'), self.i))
        }
    }
    fn value(&mut self, e: &mut Engine) -> Result<Value, String> {
        self.ws();
        let c = match self.s.get(self.i) {
            Some(c) => *c,
            None => return self.err(),
        };
        match c {
            123 => {
                self.i += 1;
                let o = e.new_object();
                self.ws();
                if self.s.get(self.i) == Some(&125) {
                    self.i += 1;
                    return Ok(Value::Obj(o));
                }
                loop {
                    self.ws();
                    if self.s.get(self.i) != Some(&34) {
                        return self.err();
                    }
                    let k = self.string()?;
                    self.ws();
                    if self.s.get(self.i) != Some(&58) {
                        return self.err();
                    }
                    self.i += 1;
                    let v = self.value(e)?;
                    let key = e.key_of_str(&k);
                    if strs::eq_str(&k, "__proto__") {
                        let a = e.atom("__proto__");
                        e.heap.get_mut(o).props.insert(a, Slot::Data(v, ALL));
                    } else {
                        e.define_own(o, key, Slot::Data(v, ALL));
                    }
                    self.ws();
                    match self.s.get(self.i) {
                        Some(44) => self.i += 1,
                        Some(125) => {
                            self.i += 1;
                            return Ok(Value::Obj(o));
                        },
                        _ => return self.err(),
                    }
                }
            },
            91 => {
                self.i += 1;
                let mut items = Vec::new();
                self.ws();
                if self.s.get(self.i) == Some(&93) {
                    self.i += 1;
                    return Ok(Value::Obj(e.new_array(items)));
                }
                loop {
                    items.push(self.value(e)?);
                    self.ws();
                    match self.s.get(self.i) {
                        Some(44) => self.i += 1,
                        Some(93) => {
                            self.i += 1;
                            return Ok(Value::Obj(e.new_array(items)));
                        },
                        _ => return self.err(),
                    }
                }
            },
            34 => Ok(Value::Str(self.string()?)),
            116 => self.word("true", Value::Bool(true)),
            102 => self.word("false", Value::Bool(false)),
            110 => self.word("null", Value::Null),
            45 | 48..=57 => {
                let st = self.i;
                if c == 45 {
                    self.i += 1;
                }
                let digits = |p: &mut Self| {
                    let s0 = p.i;
                    while p.i < p.s.len() && (48..=57).contains(&p.s[p.i]) {
                        p.i += 1;
                    }
                    p.i - s0
                };
                if self.s.get(self.i) == Some(&48) {
                    self.i += 1;
                } else if digits(self) == 0 {
                    return self.err();
                }
                if self.s.get(self.i) == Some(&46) {
                    self.i += 1;
                    if digits(self) == 0 {
                        return self.err();
                    }
                }
                if matches!(self.s.get(self.i), Some(101) | Some(69)) {
                    self.i += 1;
                    if matches!(self.s.get(self.i), Some(43) | Some(45)) {
                        self.i += 1;
                    }
                    if digits(self) == 0 {
                        return self.err();
                    }
                }
                let t = strs::to_rust(&self.s[st..self.i]);
                Ok(Value::Num(t.parse().unwrap_or(f64::NAN)))
            },
            _ => self.err(),
        }
    }
    fn word(&mut self, w: &str, v: Value) -> Result<Value, String> {
        for (k, ch) in w.bytes().enumerate() {
            if self.s.get(self.i + k) != Some(&(ch as u16)) {
                self.i += k;
                return self.err();
            }
        }
        self.i += w.len();
        Ok(v)
    }
    fn string(&mut self) -> Result<JsStr, String> {
        self.i += 1;
        let mut out = Vec::new();
        loop {
            let c = match self.s.get(self.i) {
                Some(c) => *c,
                None => return Err("Unterminated string in JSON".into()),
            };
            self.i += 1;
            match c {
                34 => break,
                92 => {
                    let d = match self.s.get(self.i) {
                        Some(d) => *d,
                        None => return self.err(),
                    };
                    self.i += 1;
                    match d {
                        34 | 92 | 47 => out.push(d),
                        98 => out.push(8),
                        102 => out.push(12),
                        110 => out.push(10),
                        114 => out.push(13),
                        116 => out.push(9),
                        117 => {
                            let mut v = 0u32;
                            for _ in 0..4 {
                                let h = self.s.get(self.i).and_then(|c| char::from_u32(*c as u32)).and_then(|c| c.to_digit(16));
                                match h {
                                    Some(h) => v = v * 16 + h,
                                    None => return self.err(),
                                }
                                self.i += 1;
                            }
                            out.push(v as u16);
                        },
                        _ => {
                            self.i -= 1;
                            return self.err();
                        },
                    }
                },
                0..=31 => {
                    self.i -= 1;
                    return Err(format!("Bad control character in string literal in JSON at position {}", self.i));
                },
                _ => out.push(c),
            }
        }
        Ok(strs::from_units(out))
    }
}

fn quote(out: &mut Vec<u16>, s: &[u16]) {
    out.push(34);
    for (i, &c) in s.iter().enumerate() {
        match c {
            34 => out.extend_from_slice(&[92, 34]),
            92 => out.extend_from_slice(&[92, 92]),
            8 => out.extend_from_slice(&[92, 98]),
            12 => out.extend_from_slice(&[92, 102]),
            10 => out.extend_from_slice(&[92, 110]),
            13 => out.extend_from_slice(&[92, 114]),
            9 => out.extend_from_slice(&[92, 116]),
            0..=31 => out.extend(format!("\\u{:04x}", c).encode_utf16()),
            0xD800..=0xDBFF if !(s.get(i + 1).is_some_and(|n| (0xDC00..0xE000).contains(n))) => out.extend(format!("\\u{:04x}", c).encode_utf16()),
            0xDC00..=0xDFFF if !(i > 0 && (0xD800..0xDC00).contains(&s[i - 1])) => out.extend(format!("\\u{:04x}", c).encode_utf16()),
            _ => out.push(c),
        }
    }
    out.push(34);
}

/// false when the value is not serialisable (undefined, a function)
fn stringify(e: &mut Engine, v: &Value, out: &mut Vec<u16>, indent: &[u16], cur: &mut String, stack: &mut Vec<ObjRef>, allow: &Option<Vec<JsStr>>) -> NResult<bool> {
    let mut v = v.clone();
    if e.sym_of(&v).is_some() {
        return Ok(false);
    }
    if e.bigint_of(&v).is_some() {
        return e.throw_type("Do not know how to serialize a BigInt");
    }
    if let Value::Obj(o) = &v {
        // toJSON (Date's is native; a user one goes to the prelude)
        if let Some(Slot::Data(f, _)) = e.find_slot(*o, Key::Atom(k::toJSON)) {
            if let Value::Obj(fo) = &f {
                match e.heap.get(*fo).kind {
                    Kind::Native(..) => {
                        if let Kind::Date(t) = e.heap.get(*o).kind {
                            v = if t.is_finite() { Value::Str(js(&iso_string(t))) } else { Value::Null };
                        }
                    },
                    Kind::Closure(_) | Kind::Bound(_) => return Err(Fault::NeedJs),
                    _ => {},
                }
            }
        }
        if let Value::Obj(o) = &v {
            match &e.heap.get(*o).kind {
                Kind::NumBox(n) => v = Value::Num(*n),
                Kind::StrBox(s) => v = Value::Str(s.clone()),
                Kind::BoolBox(b) => v = Value::Bool(*b),
                Kind::Host(..) => return Err(Fault::NeedJs),
                _ => {},
            }
        }
    }
    match &v {
        Value::Null => out.extend("null".encode_utf16()),
        Value::Bool(b) => out.extend(if *b { "true" } else { "false" }.encode_utf16()),
        Value::Num(n) => {
            if n.is_finite() {
                out.extend(crate::num::to_string_rust(*n).encode_utf16());
            } else {
                out.extend("null".encode_utf16());
            }
        },
        Value::Str(s) => quote(out, s),
        Value::Undefined | Value::Empty => return Ok(false),
        Value::Obj(o) => {
            let o = *o;
            if e.heap.get(o).is_callable() {
                return Ok(false);
            }
            if stack.contains(&o) {
                return e.throw_type("Converting circular structure to JSON");
            }
            stack.push(o);
            let ind = strs::to_rust(indent);
            let inner = format!("{}{}", cur, ind);
            let is_arr = matches!(e.heap.get(o).kind, Kind::Array(_));
            if is_arr {
                let items = e.heap.get(o).elements().cloned().unwrap_or_default();
                out.push(91);
                for (i, x) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(44);
                    }
                    if !indent.is_empty() {
                        out.push(10);
                        out.extend(inner.encode_utf16());
                    }
                    let mut c2 = inner.clone();
                    if !stringify(e, x, out, indent, &mut c2, stack, allow)? {
                        out.extend("null".encode_utf16());
                    }
                }
                if !indent.is_empty() && !items.is_empty() {
                    out.push(10);
                    out.extend(cur.encode_utf16());
                }
                out.push(93);
            } else {
                let keys = e.own_keys(o, true);
                out.push(123);
                let mut first = true;
                for kv in keys {
                    let ks = match &kv {
                        Value::Str(s) => s.clone(),
                        _ => continue,
                    };
                    if let Some(al) = allow {
                        if !al.iter().any(|x| **x == *ks) {
                            continue;
                        }
                    }
                    let key = e.key_of_str(&ks);
                    let val = match e.get_own(o, key) {
                        Some(Slot::Data(x, _)) => x,
                        Some(Slot::Accessor(..)) => return Err(Fault::NeedJs),
                        None => continue,
                    };
                    let mark = out.len();
                    if !first {
                        out.push(44);
                    }
                    if !indent.is_empty() {
                        out.push(10);
                        out.extend(inner.encode_utf16());
                    }
                    quote(out, &ks);
                    out.push(58);
                    if !indent.is_empty() {
                        out.push(32);
                    }
                    let mut c2 = inner.clone();
                    if !stringify(e, &val, out, indent, &mut c2, stack, allow)? {
                        out.truncate(mark);
                        continue;
                    }
                    first = false;
                }
                if !indent.is_empty() && !first {
                    out.push(10);
                    out.extend(cur.encode_utf16());
                }
                out.push(125);
            }
            stack.pop();
        },
    }
    Ok(true)
}

// ── Errors ──

fn setup_errors(e: &mut Engine) {
    fn make(e: &mut Engine, data: &Value, a: &[Value]) -> R {
        let proto = match data {
            Value::Obj(p) => *p,
            _ => e.realm.error_proto,
        };
        let c = e.atom("Error");
        let o = e.heap.alloc(Obj::new(Some(proto), Kind::Error, c));
        if let Some(m) = a.first() {
            if !m.is_undefined() {
                let s = e.to_str(m)?;
                e.define_atom(o, k::message, Value::Str(s), HIDDEN);
            }
        }
        if let Value::Obj(opts) = arg(a, 1) {
            if e.find_slot(opts, Key::Atom(k::cause)).is_some() {
                let cv = e.get(&Value::Obj(opts), Key::Atom(k::cause))?;
                e.define_atom(o, k::cause, cv, HIDDEN);
            }
        }
        let stack = e.stack_trace();
        let msg = match e.get(&Value::Obj(o), Key::Atom(k::message)) {
            Ok(Value::Str(s)) => strs::to_rust(&s),
            _ => String::new(),
        };
        let name = match e.get(&Value::Obj(o), Key::Atom(k::name)) {
            Ok(Value::Str(s)) => strs::to_rust(&s),
            _ => "Error".into(),
        };
        e.define_atom(o, k::stack, Value::str(&format!("{}: {}\n{}", name, msg, stack)), HIDDEN);
        ok(Value::Obj(o))
    }
    let names = ["Error", "TypeError", "RangeError", "SyntaxError", "ReferenceError", "EvalError", "URIError"];
    let mut base_ctor = None;
    for n in names {
        let proto = match n {
            "Error" => e.realm.error_proto,
            "TypeError" => e.realm.type_error_proto,
            "RangeError" => e.realm.range_error_proto,
            "SyntaxError" => e.realm.syntax_error_proto,
            "ReferenceError" => e.realm.reference_error_proto,
            "EvalError" => e.realm.eval_error_proto,
            _ => e.realm.uri_error_proto,
        };
        let idx = e.natives.len() as u16;
        e.natives.push((leak(n), |e, _, a, data, _| make(e, data, a)));
        let c = e.new_native(n, idx, 1, Value::Obj(proto));
        let g = e.realm.global;
        e.define(g, n, Value::Obj(c), HIDDEN);
        e.define_atom(c, k::prototype, Value::Obj(proto), 0);
        e.define_atom(proto, k::constructor, Value::Obj(c), HIDDEN);
        e.define_atom(proto, k::name, Value::str(n), HIDDEN);
        e.define_atom(proto, k::message, Value::str(""), HIDDEN);
        match base_ctor {
            None => base_ctor = Some(c),
            Some(b) => e.heap.get_mut(c).proto = Some(b),
        }
    }
    let ep = e.realm.error_proto;
    reg(e, ep, "toString", |e, this, _, _, _| {
        let name = match e.get(this, Key::Atom(k::name))? {
            Value::Undefined => js("Error"),
            v => e.to_str(&v)?,
        };
        let msg = match e.get(this, Key::Atom(k::message))? {
            Value::Undefined => js(""),
            v => e.to_str(&v)?,
        };
        if msg.is_empty() {
            return st(name);
        }
        if name.is_empty() {
            return st(msg);
        }
        st(js(&format!("{}: {}", strs::to_rust(&name), strs::to_rust(&msg))))
    }, 0);
    if let Some(b) = base_ctor {
        reg(e, b, "captureStackTrace", |e, _, a, _, _| {
            if let Value::Obj(o) = arg(a, 0) {
                let s = e.stack_trace();
                e.define_atom(o, k::stack, Value::str(&s), HIDDEN);
            }
            ok(Value::Undefined)
        }, 1);
        e.define(b, "stackTraceLimit", Value::Num(10.0), ALL);
    }
}

// ── RegExp ──

pub fn new_regexp(e: &mut Engine, src: &JsStr, flags: &JsStr) -> NResult<Value> {
    let fl = strs::to_rust(flags);
    let re = match Regex::new(src, &fl) {
        Ok(r) => r,
        Err(m) => {
            let msg = format!("Invalid regular expression: /{}/{}: {}", strs::to_rust(src), fl, m);
            return Err(Fault::Throw(e.syntax_error(&msg)));
        },
    };
    let c = e.atom("RegExp");
    let o = e.heap.alloc(Obj::new(Some(e.realm.regexp_proto), Kind::RegExp(Box::new(RegExpData { source: src.clone(), flags: flags.clone(), re: Rc::new(re) })), c));
    e.define_atom(o, k::lastIndex, Value::Num(0.0), WRITABLE);
    Ok(Value::Obj(o))
}

fn last_index_start(e: &mut Engine, o: ObjRef) -> usize {
    let (g, y) = match &e.heap.get(o).kind {
        Kind::RegExp(r) => (r.re.flags.global, r.re.flags.sticky),
        _ => (false, false),
    };
    if !g && !y {
        return 0;
    }
    match e.heap.get(o).props.get(k::lastIndex) {
        Some(Slot::Data(Value::Num(n), _)) => n.max(0.0) as usize,
        _ => 0,
    }
}

fn set_last_index(e: &mut Engine, o: ObjRef, n: f64) {
    if let Some(Slot::Data(v, _)) = e.heap.get_mut(o).props.get_mut(k::lastIndex) {
        *v = Value::Num(n);
    }
}

fn run_regex(e: &mut Engine, o: ObjRef, s: &[u16], from: usize) -> NResult<Option<Vec<Option<(usize, usize)>>>> {
    let re = match &e.heap.get(o).kind {
        Kind::RegExp(r) => r.re.clone(),
        _ => return e.throw_type("not a RegExp"),
    };
    if from > s.len() {
        return Ok(None);
    }
    re.exec(s, from).map_err(|m| Fault::Throw(e.range_error(&m)))
}

/// RegExp.prototype.exec semantics (lastIndex for g / y)
fn regexp_exec(e: &mut Engine, rv: &Value, s: &JsStr) -> NResult<Value> {
    let o = match rv {
        Value::Obj(o) if matches!(e.heap.get(*o).kind, Kind::RegExp(_)) => *o,
        _ => return e.throw_type("RegExp.prototype.exec called on incompatible receiver"),
    };
    let (global, sticky, names) = match &e.heap.get(o).kind {
        Kind::RegExp(r) => (r.re.flags.global, r.re.flags.sticky, r.re.names.clone()),
        _ => unreachable!(),
    };
    let start = last_index_start(e, o);
    if start > s.len() {
        set_last_index(e, o, 0.0);
        return Ok(Value::Null);
    }
    let m = run_regex(e, o, s, start)?;
    let caps = match m {
        None => {
            if global || sticky {
                set_last_index(e, o, 0.0);
            }
            return Ok(Value::Null);
        },
        Some(c) => c,
    };
    let (ms, me) = caps[0].unwrap();
    if global || sticky {
        set_last_index(e, o, me as f64);
    }
    let items: Vec<Value> = caps.iter().map(|c| match c {
        Some((a, b)) => Value::Str(Rc::from(&s[*a..*b])),
        None => Value::Undefined,
    }).collect();
    let arr = e.new_array(items.clone());
    e.define_atom(arr, k::index, Value::Num(ms as f64), ALL);
    e.define_atom(arr, k::input, Value::Str(s.clone()), ALL);
    if names.is_empty() {
        e.define_atom(arr, k::groups, Value::Undefined, ALL);
    } else {
        let g = e.new_object_proto(None);
        for (n, i) in names {
            e.define(g, &n, items[i].clone(), ALL);
        }
        e.define_atom(arr, k::groups, Value::Obj(g), ALL);
    }
    Ok(Value::Obj(arr))
}

fn setup_regexp(e: &mut Engine) {
    let p = e.realm.regexp_proto;
    ctor(e, "RegExp", |e, _, a, _, _| {
        let (src, mut flags) = match arg(a, 0) {
            Value::Obj(o) => match &e.heap.get(o).kind {
                Kind::RegExp(r) => (r.source.clone(), r.flags.clone()),
                _ => (e.to_str(&Value::Obj(o))?, js("")),
            },
            Value::Undefined => (js("(?:)"), js("")),
            v => (e.to_str(&v)?, js("")),
        };
        if !arg(a, 1).is_undefined() {
            flags = e.to_str(&arg(a, 1))?;
        }
        ok(new_regexp(e, &src, &flags)?)
    }, 2, p);
    reg(e, p, "exec", |e, this, a, _, _| {
        let s = e.to_str(&arg(a, 0))?;
        ok(regexp_exec(e, this, &s)?)
    }, 1);
    reg(e, p, "test", |e, this, a, _, _| {
        let s = e.to_str(&arg(a, 0))?;
        let r = regexp_exec(e, this, &s)?;
        ok(Value::Bool(!matches!(r, Value::Null)))
    }, 1);
    reg(e, p, "toString", |e, this, _, _, _| {
        match this {
            Value::Obj(o) => match &e.heap.get(*o).kind {
                Kind::RegExp(r) => {
                    let s = format!("/{}/{}", strs::to_rust(&r.source), strs::to_rust(&r.flags));
                    st(js(&s))
                },
                _ => st(js("/(?:)/")),
            },
            _ => e.throw_type("RegExp.prototype.toString called on incompatible receiver"),
        }
    }, 0);
    fn flag_getter(e: &mut Engine, this: &Value, f: fn(&crate::regex::Flags) -> bool) -> R {
        match this {
            Value::Obj(o) => match &e.heap.get(*o).kind {
                Kind::RegExp(r) => ok(Value::Bool(f(&r.re.flags))),
                _ => ok(Value::Undefined),
            },
            _ => ok(Value::Undefined),
        }
    }
    getter(e, p, "global", |e, this, _, _, _| flag_getter(e, this, |f| f.global));
    getter(e, p, "ignoreCase", |e, this, _, _, _| flag_getter(e, this, |f| f.ignore_case));
    getter(e, p, "multiline", |e, this, _, _, _| flag_getter(e, this, |f| f.multiline));
    getter(e, p, "sticky", |e, this, _, _, _| flag_getter(e, this, |f| f.sticky));
    getter(e, p, "unicode", |e, this, _, _, _| flag_getter(e, this, |f| f.unicode));
    getter(e, p, "dotAll", |e, this, _, _, _| flag_getter(e, this, |f| f.dot_all));
    getter(e, p, "hasIndices", |e, this, _, _, _| flag_getter(e, this, |f| f.has_indices));
    getter(e, p, "source", |e, this, _, _, _| match this {
        Value::Obj(o) => match &e.heap.get(*o).kind {
            Kind::RegExp(r) => st(r.source.clone()),
            _ => st(js("(?:)")),
        },
        _ => ok(Value::Undefined),
    });
    getter(e, p, "flags", |e, this, _, _, _| match this {
        Value::Obj(o) => match &e.heap.get(*o).kind {
            Kind::RegExp(r) => {
                let mut f: Vec<char> = strs::to_rust(&r.flags).chars().collect();
                f.sort_by_key(|c| "dgimsuvy".find(*c).unwrap_or(9));
                st(js(&f.into_iter().collect::<String>()))
            },
            _ => st(js("")),
        },
        _ => ok(Value::Undefined),
    });
}

// ── Date ──

fn days_from_civil(y: f64, m: f64, d: f64) -> f64 {
    // m 0-based; handles month overflow
    let y = y + libm::floor(m / 12.0);
    let m = m - 12.0 * libm::floor(m / 12.0) + 1.0;
    let y = if m <= 2.0 { y - 1.0 } else { y };
    let era = libm::floor(y / 400.0);
    let yoe = y - era * 400.0;
    let mp = (m + 9.0) % 12.0;
    let doy = libm::floor((153.0 * mp + 2.0) / 5.0) + d - 1.0;
    let doe = yoe * 365.0 + libm::floor(yoe / 4.0) - libm::floor(yoe / 100.0) + doy;
    era * 146097.0 + doe - 719468.0
}

/// (year, month 0-11, day 1-31, weekday 0-6, h, m, s, ms) of a time value
fn civil(t: f64) -> (f64, f64, f64, f64, f64, f64, f64, f64) {
    let days = libm::floor(t / 86400000.0);
    let ms_day = t - days * 86400000.0;
    let z = days + 719468.0;
    let era = libm::floor(z / 146097.0);
    let doe = z - era * 146097.0;
    let yoe = libm::floor((doe - libm::floor(doe / 1460.0) + libm::floor(doe / 36524.0) - libm::floor(doe / 146096.0)) / 365.0);
    let y = yoe + era * 400.0;
    let doy = doe - (365.0 * yoe + libm::floor(yoe / 4.0) - libm::floor(yoe / 100.0));
    let mp = libm::floor((5.0 * doy + 2.0) / 153.0);
    let d = doy - libm::floor((153.0 * mp + 2.0) / 5.0) + 1.0;
    let m = if mp < 10.0 { mp + 3.0 } else { mp - 9.0 };
    let y = if m <= 2.0 { y + 1.0 } else { y };
    let wd = (days + 4.0).rem_euclid(7.0);
    let h = libm::floor(ms_day / 3600000.0);
    let mi = libm::floor((ms_day % 3600000.0) / 60000.0);
    let s = libm::floor((ms_day % 60000.0) / 1000.0);
    let ms = ms_day % 1000.0;
    (y, m - 1.0, d, wd, h, mi, s, ms)
}

fn make_time(y: f64, mo: f64, d: f64, h: f64, mi: f64, s: f64, ms: f64) -> f64 {
    if ![y, mo, d, h, mi, s, ms].iter().all(|x| x.is_finite()) {
        return f64::NAN;
    }
    let t = days_from_civil(libm::trunc(y), libm::trunc(mo), libm::trunc(d)) * 86400000.0 + libm::trunc(h) * 3600000.0 + libm::trunc(mi) * 60000.0 + libm::trunc(s) * 1000.0 + libm::trunc(ms);
    time_clip(t)
}

fn time_clip(t: f64) -> f64 {
    if !t.is_finite() || t.abs() > 8.64e15 {
        f64::NAN
    } else {
        libm::trunc(t) + 0.0
    }
}

pub fn iso_string(t: f64) -> String {
    let (y, m, d, _, h, mi, s, ms) = civil(t);
    let ys = if (0.0..=9999.0).contains(&y) { format!("{:04}", y as i64) } else if y < 0.0 { format!("-{:06}", (-y) as i64) } else { format!("+{:06}", y as i64) };
    format!("{}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z", ys, m as i64 + 1, d as i64, h as i64, mi as i64, s as i64, ms as i64)
}

const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

pub fn date_to_string(e: &Engine, t: f64) -> String {
    if t.is_nan() {
        return "Invalid Date".into();
    }
    let off = -e.tz_offset_min;
    let (y, m, d, wd, h, mi, s, _) = civil(t + off * 60000.0);
    let sign = if off >= 0.0 { '+' } else { '-' };
    let oa = off.abs() as i64;
    format!("{} {} {:02} {:04} {:02}:{:02}:{:02} GMT{}{:02}{:02}", DAYS[wd as usize], MONTHS[m as usize], d as i64, y as i64, h as i64, mi as i64, s as i64, sign, oa / 60, oa % 60)
}

/// Date.parse: ISO forms, and the common "Mon DD YYYY HH:MM:SS GMT" / RFC 1123 forms
fn parse_date(s: &str, tz_min: f64) -> f64 {
    let s = s.trim();
    // ISO: YYYY[-MM[-DD]][THH:MM[:SS[.sss]]][Z|±HH:MM]
    let b = s.as_bytes();
    if b.len() >= 4 && (b[0].is_ascii_digit() || b[0] == b'+' || b[0] == b'-') {
        let (sign, rest) = match b[0] {
            b'+' => (1.0, &s[1..]),
            b'-' => (-1.0, &s[1..]),
            _ => (1.0, s),
        };
        let mut it = rest.splitn(2, 'T');
        let date = it.next().unwrap_or("");
        let time = it.next();
        let dp: Vec<&str> = date.split('-').collect();
        if dp.iter().all(|x| !x.is_empty() && x.bytes().all(|c| c.is_ascii_digit())) && !dp.is_empty() && dp.len() <= 3 {
            let y: f64 = dp[0].parse::<f64>().unwrap_or(f64::NAN) * sign;
            let mo: f64 = dp.get(1).map(|x| x.parse::<f64>().unwrap_or(f64::NAN) - 1.0).unwrap_or(0.0);
            let d: f64 = dp.get(2).map(|x| x.parse::<f64>().unwrap_or(f64::NAN)).unwrap_or(1.0);
            let (mut h, mut mi, mut sec, mut ms) = (0.0, 0.0, 0.0, 0.0);
            let mut offset: Option<f64> = if time.is_none() { Some(0.0) } else { None };
            if let Some(t) = time {
                let (clock, zone) = if let Some(i) = t.find(['Z', '+', '-']) { (&t[..i], Some(&t[i..])) } else { (t, None) };
                let cp: Vec<&str> = clock.split(':').collect();
                h = cp.first().and_then(|x| x.parse().ok()).unwrap_or(f64::NAN);
                mi = cp.get(1).and_then(|x| x.parse().ok()).unwrap_or(0.0);
                if let Some(sx) = cp.get(2) {
                    let (a, f) = sx.split_once('.').unwrap_or((sx, ""));
                    sec = a.parse().unwrap_or(f64::NAN);
                    if !f.is_empty() {
                        let f3: String = f.chars().chain("000".chars()).take(3).collect();
                        ms = f3.parse().unwrap_or(0.0);
                    }
                }
                offset = match zone {
                    Some("Z") => Some(0.0),
                    Some(z) => {
                        let sg = if z.starts_with('-') { -1.0 } else { 1.0 };
                        let zz: String = z[1..].chars().filter(|c| *c != ':').collect();
                        let hh: f64 = zz.get(0..2).and_then(|x| x.parse().ok()).unwrap_or(0.0);
                        let mm: f64 = zz.get(2..4).and_then(|x| x.parse().ok()).unwrap_or(0.0);
                        Some(sg * (hh * 60.0 + mm))
                    },
                    None => None,
                };
            }
            let t = make_time(y, mo, d, h, mi, sec, ms);
            return match offset {
                Some(o) => t - o * 60000.0,
                None => t + tz_min * 60000.0,
            };
        }
    }
    // "Tue, 12 Nov 2026 10:00:00 GMT", "Nov 12 2026", "November 12, 2026 10:00:00"
    let mut y = f64::NAN;
    let mut mo = f64::NAN;
    let mut d = f64::NAN;
    let (mut h, mut mi, mut sec) = (0.0, 0.0, 0.0);
    let mut off: Option<f64> = None;
    for tok in s.split(|c: char| c == ' ' || c == ',').filter(|t| !t.is_empty()) {
        let low = tok.to_ascii_lowercase();
        if let Some(i) = MONTHS.iter().position(|m| low.starts_with(&m.to_ascii_lowercase())) {
            mo = i as f64;
        } else if tok.contains(':') {
            let p: Vec<f64> = tok.split(':').map(|x| x.parse().unwrap_or(0.0)).collect();
            h = p.first().copied().unwrap_or(0.0);
            mi = p.get(1).copied().unwrap_or(0.0);
            sec = p.get(2).copied().unwrap_or(0.0);
        } else if low == "gmt" || low == "utc" || low == "z" {
            off = Some(0.0);
        } else if (low.starts_with("gmt") || low.starts_with('+') || low.starts_with('-')) && tok.len() >= 5 {
            let z = low.trim_start_matches("gmt");
            let sg = if z.starts_with('-') { -1.0 } else { 1.0 };
            let digits: String = z.chars().filter(|c| c.is_ascii_digit()).collect();
            let hh: f64 = digits.get(0..2).and_then(|x| x.parse().ok()).unwrap_or(0.0);
            let mm: f64 = digits.get(2..4).and_then(|x| x.parse().ok()).unwrap_or(0.0);
            off = Some(sg * (hh * 60.0 + mm));
        } else if let Ok(n) = tok.parse::<f64>() {
            if n > 31.0 || !d.is_nan() {
                y = n;
            } else {
                d = n;
            }
        } else if low.ends_with("am") || low.ends_with("pm") {
        }
    }
    if y.is_nan() || mo.is_nan() || d.is_nan() {
        return f64::NAN;
    }
    let t = make_time(y, mo, d, h, mi, sec, 0.0);
    match off {
        Some(o) => t - o * 60000.0,
        None => t + tz_min * 60000.0,
    }
}

fn setup_date(e: &mut Engine) {
    let p = e.realm.date_proto;
    let c = ctor(e, "Date", |e, _, a, _, construct| {
        if !construct {
            let s = date_to_string(e, e.now_ms);
            return st(js(&s));
        }
        let tz = e.tz_offset_min;
        let t = match a.len() {
            0 => e.now_ms,
            1 => match &a[0] {
                Value::Str(s) => parse_date(&strs::to_rust(s), tz),
                Value::Obj(o) if matches!(e.heap.get(*o).kind, Kind::Date(_)) => match e.heap.get(*o).kind {
                    Kind::Date(t) => t,
                    _ => f64::NAN,
                },
                v => {
                    let p = e.to_primitive(v, true)?;
                    match p {
                        Value::Str(s) => parse_date(&strs::to_rust(&s), tz),
                        other => time_clip(e.to_number(&other)?),
                    }
                },
            },
            _ => {
                let mut f = [0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0];
                for (i, x) in a.iter().take(7).enumerate() {
                    f[i] = e.to_number(x)?;
                }
                if (0.0..=99.0).contains(&f[0]) && f[0].fract() == 0.0 {
                    f[0] += 1900.0;
                }
                make_time(f[0], f[1], f[2], f[3], f[4], f[5], f[6]) + tz * 60000.0
            },
        };
        let c = e.atom("Date");
        let o = e.heap.alloc(Obj::new(Some(e.realm.date_proto), Kind::Date(time_clip(t)), c));
        ok(Value::Obj(o))
    }, 7, p);
    reg(e, c, "now", |e, _, _, _, _| num(libm::floor(e.now_ms)), 0);
    reg(e, c, "parse", |e, _, a, _, _| {
        let s = e.to_str(&arg(a, 0))?;
        let tz = e.tz_offset_min;
        num(parse_date(&strs::to_rust(&s), tz))
    }, 1);
    reg(e, c, "UTC", |e, _, a, _, _| {
        let mut f = [f64::NAN, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0];
        for (i, x) in a.iter().take(7).enumerate() {
            f[i] = e.to_number(x)?;
        }
        if (0.0..=99.0).contains(&f[0]) && f[0].fract() == 0.0 {
            f[0] += 1900.0;
        }
        num(make_time(f[0], f[1], f[2], f[3], f[4], f[5], f[6]))
    }, 7);
    fn tv(e: &mut Engine, this: &Value) -> NResult<f64> {
        match this {
            Value::Obj(o) => match e.heap.get(*o).kind {
                Kind::Date(t) => Ok(t),
                _ => e.throw_type("this is not a Date object."),
            },
            _ => e.throw_type("this is not a Date object."),
        }
    }
    fn set_tv(e: &mut Engine, this: &Value, t: f64) -> R {
        if let Value::Obj(o) = this {
            e.heap.get_mut(*o).kind = Kind::Date(time_clip(t));
        }
        num(time_clip(t))
    }
    reg(e, p, "getTime", |e, this, _, _, _| num(tv(e, this)?), 0);
    reg(e, p, "valueOf", |e, this, _, _, _| num(tv(e, this)?), 0);
    reg(e, p, "getTimezoneOffset", |e, this, _, _, _| {
        tv(e, this)?;
        num(e.tz_offset_min)
    }, 0);
    macro_rules! part {
        ($name:expr, $idx:tt, $local:expr) => {
            reg(e, p, $name, |e, this, _, _, _| {
                let t = tv(e, this)?;
                if t.is_nan() {
                    return num(f64::NAN);
                }
                let t = if $local { t - e.tz_offset_min * 60000.0 } else { t };
                let c = civil(t);
                num(c.$idx)
            }, 0);
        };
    }
    part!("getFullYear", 0, true);
    part!("getMonth", 1, true);
    part!("getDate", 2, true);
    part!("getDay", 3, true);
    part!("getHours", 4, true);
    part!("getMinutes", 5, true);
    part!("getSeconds", 6, true);
    part!("getMilliseconds", 7, true);
    part!("getUTCFullYear", 0, false);
    part!("getUTCMonth", 1, false);
    part!("getUTCDate", 2, false);
    part!("getUTCDay", 3, false);
    part!("getUTCHours", 4, false);
    part!("getUTCMinutes", 5, false);
    part!("getUTCSeconds", 6, false);
    part!("getUTCMilliseconds", 7, false);
    reg(e, p, "getYear", |e, this, _, _, _| {
        let t = tv(e, this)?;
        num(civil(t - e.tz_offset_min * 60000.0).0 - 1900.0)
    }, 0);
    reg(e, p, "setTime", |e, this, a, _, _| {
        tv(e, this)?;
        let t = e.to_number(&arg(a, 0))?;
        set_tv(e, this, t)
    }, 1);
    // setters: (field index in [y, mo, d, h, mi, s, ms], how many args it takes, local?)
    fn set_fields(e: &mut Engine, this: &Value, a: &[Value], first: usize, max: usize, local: bool) -> R {
        let t = tv(e, this)?;
        let off = if local { e.tz_offset_min * 60000.0 } else { 0.0 };
        let base = if t.is_nan() && first == 0 { 0.0 } else { t };
        if base.is_nan() {
            return num(f64::NAN);
        }
        let c = civil(base - off);
        let mut f = [c.0, c.1, c.2, c.4, c.5, c.6, c.7];
        for i in 0..max.min(a.len().max(1)) {
            f[first + i] = e.to_number(&arg(a, i))?;
        }
        let nt = make_time(f[0], f[1], f[2], f[3], f[4], f[5], f[6]) + off;
        set_tv(e, this, nt)
    }
    macro_rules! setter {
        ($name:expr, $first:expr, $max:expr, $local:expr) => {
            reg(e, p, $name, |e, this, a, _, _| set_fields(e, this, a, $first, $max, $local), $max as u32);
        };
    }
    setter!("setFullYear", 0, 3, true);
    setter!("setMonth", 1, 2, true);
    setter!("setDate", 2, 1, true);
    setter!("setHours", 3, 4, true);
    setter!("setMinutes", 4, 3, true);
    setter!("setSeconds", 5, 2, true);
    setter!("setMilliseconds", 6, 1, true);
    setter!("setUTCFullYear", 0, 3, false);
    setter!("setUTCMonth", 1, 2, false);
    setter!("setUTCDate", 2, 1, false);
    setter!("setUTCHours", 3, 4, false);
    setter!("setUTCMinutes", 4, 3, false);
    setter!("setUTCSeconds", 5, 2, false);
    setter!("setUTCMilliseconds", 6, 1, false);
    reg(e, p, "toISOString", |e, this, _, _, _| {
        let t = tv(e, this)?;
        if t.is_nan() {
            return Err(Fault::Throw(e.range_error("Invalid time value")));
        }
        st(js(&iso_string(t)))
    }, 0);
    reg(e, p, "toJSON", |e, this, _, _, _| {
        let t = tv(e, this)?;
        if t.is_nan() {
            return ok(Value::Null);
        }
        st(js(&iso_string(t)))
    }, 1);
    reg(e, p, "toString", |e, this, _, _, _| {
        let t = tv(e, this)?;
        st(js(&date_to_string(e, t)))
    }, 0);
    reg(e, p, "toDateString", |e, this, _, _, _| {
        let t = tv(e, this)?;
        if t.is_nan() {
            return st(js("Invalid Date"));
        }
        let (y, m, d, wd, ..) = civil(t - e.tz_offset_min * 60000.0);
        st(js(&format!("{} {} {:02} {:04}", DAYS[wd as usize], MONTHS[m as usize], d as i64, y as i64)))
    }, 0);
    reg(e, p, "toTimeString", |e, this, _, _, _| {
        let t = tv(e, this)?;
        let s = date_to_string(e, t);
        st(js(s.split_once(' ').map(|x| x.1).and_then(|x| x.splitn(4, ' ').nth(3)).unwrap_or(&s)))
    }, 0);
    reg(e, p, "toUTCString", |e, this, _, _, _| {
        let t = tv(e, this)?;
        if t.is_nan() {
            return st(js("Invalid Date"));
        }
        let (y, m, d, wd, h, mi, s, _) = civil(t);
        st(js(&format!("{}, {:02} {} {:04} {:02}:{:02}:{:02} GMT", DAYS[wd as usize], d as i64, MONTHS[m as usize], y as i64, h as i64, mi as i64, s as i64)))
    }, 0);
    reg(e, p, "toGMTString", |e, this, _, _, _| {
        let f = e.get_str(this, "toUTCString")?;
        Ok(NOut::TailCall(f, this.clone(), vec![]))
    }, 0);
    reg(e, p, "toLocaleDateString", |e, this, _, _, _| {
        let t = tv(e, this)?;
        let (y, m, d, ..) = civil(t - e.tz_offset_min * 60000.0);
        st(js(&format!("{}/{}/{}", m as i64 + 1, d as i64, y as i64)))
    }, 0);
    reg(e, p, "toLocaleTimeString", |e, this, _, _, _| {
        let t = tv(e, this)?;
        let (_, _, _, _, h, mi, s, _) = civil(t - e.tz_offset_min * 60000.0);
        let (h12, ap) = if h == 0.0 { (12.0, "AM") } else if h < 12.0 { (h, "AM") } else if h == 12.0 { (12.0, "PM") } else { (h - 12.0, "PM") };
        st(js(&format!("{}:{:02}:{:02} {}", h12 as i64, mi as i64, s as i64, ap)))
    }, 0);
    reg(e, p, "toLocaleString", |e, this, _, _, _| {
        let t = tv(e, this)?;
        let (y, m, d, _, h, mi, s, _) = civil(t - e.tz_offset_min * 60000.0);
        let (h12, ap) = if h == 0.0 { (12.0, "AM") } else if h < 12.0 { (h, "AM") } else if h == 12.0 { (12.0, "PM") } else { (h - 12.0, "PM") };
        st(js(&format!("{}/{}/{}, {}:{:02}:{:02} {}", m as i64 + 1, d as i64, y as i64, h12 as i64, mi as i64, s as i64, ap)))
    }, 0);
}

// ── global functions ──

fn uri_encode(e: &mut Engine, s: &[u16], keep: &str) -> NResult<JsStr> {
    let mut out: Vec<u16> = Vec::new();
    let mut i = 0;
    while i < s.len() {
        let c = s[i] as u32;
        if c < 128 && ((c as u8).is_ascii_alphanumeric() || keep.as_bytes().contains(&(c as u8))) {
            out.push(c as u16);
            i += 1;
            continue;
        }
        let cp = if (0xD800..0xDC00).contains(&c) {
            match s.get(i + 1) {
                Some(&lo) if (0xDC00..0xE000).contains(&(lo as u32)) => {
                    i += 1;
                    0x10000 + ((c - 0xD800) << 10) + (lo as u32 - 0xDC00)
                },
                _ => return Err(Fault::Throw(e.make_error(e.realm.uri_error_proto, "URI malformed"))),
            }
        } else if (0xDC00..0xE000).contains(&c) {
            return Err(Fault::Throw(e.make_error(e.realm.uri_error_proto, "URI malformed")));
        } else {
            c
        };
        let ch = char::from_u32(cp).unwrap_or('\u{FFFD}');
        let mut buf = [0u8; 4];
        for b in ch.encode_utf8(&mut buf).bytes() {
            out.extend(format!("%{:02X}", b).encode_utf16());
        }
        i += 1;
    }
    Ok(strs::from_units(out))
}

fn uri_decode(e: &mut Engine, s: &[u16], reserved: &str) -> NResult<JsStr> {
    let mut out: Vec<u16> = Vec::new();
    let mut i = 0;
    let bad = |e: &mut Engine| Fault::Throw(e.make_error(e.realm.uri_error_proto, "URI malformed"));
    let hexv = |c: u16| char::from_u32(c as u32).and_then(|c| c.to_digit(16));
    while i < s.len() {
        if s[i] != 37 {
            out.push(s[i]);
            i += 1;
            continue;
        }
        let byte = |i: usize| -> Option<u8> { Some((hexv(*s.get(i + 1)?)? * 16 + hexv(*s.get(i + 2)?)?) as u8) };
        let b0 = byte(i).ok_or_else(|| bad(e))?;
        if b0 < 128 {
            if reserved.as_bytes().contains(&b0) {
                out.extend_from_slice(&s[i..i + 3]);
            } else {
                out.push(b0 as u16);
            }
            i += 3;
            continue;
        }
        let n = if b0 & 0xE0 == 0xC0 { 2 } else if b0 & 0xF0 == 0xE0 { 3 } else if b0 & 0xF8 == 0xF0 { 4 } else { return Err(bad(e)) };
        let mut bytes = vec![b0];
        for k in 1..n {
            let at = i + k * 3;
            if s.get(at) != Some(&37) {
                return Err(bad(e));
            }
            bytes.push(byte(at).ok_or_else(|| bad(e))?);
        }
        let txt = core::str::from_utf8(&bytes).map_err(|_| bad(e))?;
        out.extend(txt.encode_utf16());
        i += n * 3;
    }
    Ok(strs::from_units(out))
}

fn setup_global_fns(e: &mut Engine) {
    let g = e.realm.global;
    reg(e, g, "parseInt", |e, _, a, _, _| {
        let s = e.to_str(&arg(a, 0))?;
        let r = crate::num::to_int32(e.to_number(&arg(a, 1))?);
        num(crate::num::parse_int(&s, r))
    }, 2);
    reg(e, g, "parseFloat", |e, _, a, _, _| {
        let s = e.to_str(&arg(a, 0))?;
        num(crate::num::parse_float(&s))
    }, 1);
    reg(e, g, "isNaN", |e, _, a, _, _| ok(Value::Bool(e.to_number(&arg(a, 0))?.is_nan())), 1);
    reg(e, g, "isFinite", |e, _, a, _, _| ok(Value::Bool(e.to_number(&arg(a, 0))?.is_finite())), 1);
    reg(e, g, "encodeURIComponent", |e, _, a, _, _| {
        let s = e.to_str(&arg(a, 0))?;
        st(uri_encode(e, &s, "-_.!~*'()")?)
    }, 1);
    reg(e, g, "encodeURI", |e, _, a, _, _| {
        let s = e.to_str(&arg(a, 0))?;
        st(uri_encode(e, &s, "-_.!~*'();/?:@&=+$,#")?)
    }, 1);
    reg(e, g, "decodeURIComponent", |e, _, a, _, _| {
        let s = e.to_str(&arg(a, 0))?;
        st(uri_decode(e, &s, "")?)
    }, 1);
    reg(e, g, "decodeURI", |e, _, a, _, _| {
        let s = e.to_str(&arg(a, 0))?;
        st(uri_decode(e, &s, ";/?:@&=+$,#")?)
    }, 1);
    reg(e, g, "escape", |e, _, a, _, _| {
        let s = e.to_str(&arg(a, 0))?;
        let mut out = String::new();
        for &c in s.iter() {
            if c < 128 && ((c as u8).is_ascii_alphanumeric() || b"@*_+-./".contains(&(c as u8))) {
                out.push(c as u8 as char);
            } else if c < 256 {
                out.push_str(&format!("%{:02X}", c));
            } else {
                out.push_str(&format!("%u{:04X}", c));
            }
        }
        st(js(&out))
    }, 1);
    reg(e, g, "unescape", |e, _, a, _, _| {
        let s = e.to_str(&arg(a, 0))?;
        let mut out = Vec::new();
        let mut i = 0;
        let hx = |c: u16| char::from_u32(c as u32).and_then(|c| c.to_digit(16));
        while i < s.len() {
            if s[i] == 37 {
                if s.get(i + 1) == Some(&117) {
                    let v: Option<u32> = (2..6).try_fold(0u32, |acc, k| Some(acc * 16 + hx(*s.get(i + k)?)?));
                    if let Some(v) = v {
                        out.push(v as u16);
                        i += 6;
                        continue;
                    }
                } else {
                    let v: Option<u32> = (1..3).try_fold(0u32, |acc, k| Some(acc * 16 + hx(*s.get(i + k)?)?));
                    if let Some(v) = v {
                        out.push(v as u16);
                        i += 3;
                        continue;
                    }
                }
            }
            out.push(s[i]);
            i += 1;
        }
        st(strs::from_units(out))
    }, 1);
    // console: to the engine's log (the embedder reads it)
    let con = e.new_object();
    e.define(g, "console", Value::Obj(con), HIDDEN);
    for (name, level) in [("log", ""), ("info", ""), ("debug", "debug: "), ("warn", "warning: "), ("error", "error: "), ("trace", "trace: "), ("table", "")] {
        let idx = e.natives.len() as u16;
        e.natives.push((leak(name), |e, _, a, data, _| {
            let mut parts = Vec::new();
            for x in a {
                parts.push(e.display(x));
            }
            let prefix = match data {
                Value::Str(s) => strs::to_rust(s),
                _ => String::new(),
            };
            let line = format!("{}{}", prefix, parts.join(" "));
            if e.log.len() < 2000 {
                e.log.push(line);
            }
            ok(Value::Undefined)
        }));
        let f = e.new_native(name, idx, 0, Value::str(level));
        e.define(con, name, Value::Obj(f), HIDDEN);
    }
    reg(e, g, "queueMicrotask", |e, _, a, _, _| {
        let f = arg(a, 0);
        if !e.is_callable(&f) {
            return e.throw_type("queueMicrotask: the argument is not a function");
        }
        e.enqueue_job(f, Value::Undefined, vec![]);
        ok(Value::Undefined)
    }, 1);
    // the prelude's hooks (removed again when it has run)
    reg(e, g, "__slow", |e, _, a, _, _| {
        if let Value::Obj(n) = arg(a, 0) {
            if let Kind::Native(idx, _) = e.heap.get(n).kind {
                e.slow.insert(idx, arg(a, 1));
            }
        }
        ok(Value::Undefined)
    }, 2);
    reg(e, g, "__intrinsic", |e, _, a, _, _| {
        let i = e.to_number(&arg(a, 0))? as usize;
        while e.realm.intrinsics.len() <= i {
            e.realm.intrinsics.push(Value::Undefined);
        }
        e.realm.intrinsics[i] = arg(a, 1);
        ok(Value::Undefined)
    }, 2);
    reg(e, g, "__hide", |e, _, a, _, _| {
        // make a prelude-defined property non-enumerable
        if let Value::Obj(o) = arg(a, 0) {
            let key = e.to_key(&arg(a, 1))?;
            if let Some(Slot::Data(v, _)) = e.get_own(o, key) {
                e.define_own(o, key, Slot::Data(v, HIDDEN));
            }
        }
        ok(Value::Undefined)
    }, 2);
    reg(e, g, "__isArrayLike", |e, _, a, _, _| {
        ok(Value::Bool(match arg(a, 0) {
            Value::Obj(o) => matches!(e.heap.get(o).kind, Kind::Array(_) | Kind::Arguments(_)),
            _ => false,
        }))
    }, 1);
    let _ = (getter as fn(&mut Engine, ObjRef, &str, NativeFn), this_obj as fn(&mut Engine, &Value) -> NResult<ObjRef>);
}

#[allow(dead_code)]
fn _unused() -> String {
    ToString::to_string("")
}
