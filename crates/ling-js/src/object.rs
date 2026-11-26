//! The heap: objects in an arena addressed by [`ObjRef`], property maps, object kinds.
//! Collection (mark-sweep) is driven by the VM, which knows the roots.

use crate::atoms::Atom;
use crate::compiler::FuncProto;
use crate::regex::Regex;
use crate::strs::JsStr;
use crate::value::Value;
use alloc::boxed::Box;
use alloc::rc::Rc;
use alloc::vec;
use alloc::vec::Vec;

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub struct ObjRef(pub u32);

pub const WRITABLE: u8 = 1;
pub const ENUMERABLE: u8 = 2;
pub const CONFIGURABLE: u8 = 4;
pub const ALL: u8 = 7;
/// a builtin's method: writable and configurable, not enumerable
pub const HIDDEN: u8 = WRITABLE | CONFIGURABLE;

#[derive(Clone, Debug)]
pub enum Slot {
    Data(Value, u8),
    Accessor(Option<ObjRef>, Option<ObjRef>, u8),
}

impl Slot {
    pub fn flags(&self) -> u8 {
        match self {
            Slot::Data(_, f) | Slot::Accessor(_, _, f) => *f,
        }
    }
}

/// An insertion-ordered map from atom to slot; a hash index once it grows past a few keys.
#[derive(Clone, Default, Debug)]
pub struct PropMap {
    pub keys: Vec<Atom>,
    pub slots: Vec<Slot>,
    index: Vec<u32>,
}

const LINEAR: usize = 8;

fn h(a: Atom) -> usize {
    (a.0.wrapping_mul(0x9E37_79B1)) as usize
}

impl PropMap {
    pub fn len(&self) -> usize {
        self.keys.len()
    }
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    fn find(&self, a: Atom) -> Option<usize> {
        if self.index.is_empty() {
            return self.keys.iter().position(|k| *k == a);
        }
        let mask = self.index.len() - 1;
        let mut i = h(a) & mask;
        loop {
            let e = self.index[i];
            if e == 0 {
                return None;
            }
            if self.keys[(e - 1) as usize] == a {
                return Some((e - 1) as usize);
            }
            i = (i + 1) & mask;
        }
    }

    fn reindex(&mut self) {
        if self.keys.len() <= LINEAR {
            self.index.clear();
            return;
        }
        let n = (self.keys.len() * 2).next_power_of_two().max(16);
        self.index = vec![0; n];
        let mask = n - 1;
        for (pos, k) in self.keys.iter().enumerate() {
            let mut i = h(*k) & mask;
            while self.index[i] != 0 {
                i = (i + 1) & mask;
            }
            self.index[i] = pos as u32 + 1;
        }
    }

    pub fn get(&self, a: Atom) -> Option<&Slot> {
        self.find(a).map(|i| &self.slots[i])
    }
    pub fn get_mut(&mut self, a: Atom) -> Option<&mut Slot> {
        match self.find(a) {
            Some(i) => Some(&mut self.slots[i]),
            None => None,
        }
    }
    pub fn contains(&self, a: Atom) -> bool {
        self.find(a).is_some()
    }

    pub fn insert(&mut self, a: Atom, s: Slot) {
        if let Some(i) = self.find(a) {
            self.slots[i] = s;
            return;
        }
        self.keys.push(a);
        self.slots.push(s);
        let n = self.keys.len();
        if n > LINEAR {
            if self.index.is_empty() || n * 2 > self.index.len() {
                self.reindex();
            } else {
                let mask = self.index.len() - 1;
                let mut i = h(a) & mask;
                while self.index[i] != 0 {
                    i = (i + 1) & mask;
                }
                self.index[i] = n as u32;
            }
        }
    }

    pub fn remove(&mut self, a: Atom) -> bool {
        match self.find(a) {
            Some(i) => {
                self.keys.remove(i);
                self.slots.remove(i);
                self.reindex();
                true
            },
            None => false,
        }
    }
}

pub struct Closure {
    pub func: Rc<FuncProto>,
    pub env: Option<ObjRef>,
    /// for methods: the object `super` looks up from
    pub home: Option<ObjRef>,
    /// an arrow's `this`, captured when it was made
    pub this_val: Option<Value>,
    /// class constructors: the instance fields' initialiser; a field initialiser: its computed keys
    pub fields: Option<ObjRef>,
}

pub struct Bound {
    pub target: ObjRef,
    pub this: Value,
    pub args: Vec<Value>,
}

pub struct RegExpData {
    pub source: JsStr,
    pub flags: JsStr,
    pub re: Rc<Regex>,
}

pub enum Kind {
    Ordinary,
    /// dense elements (`Value::Empty` = a hole)
    Array(Vec<Value>),
    Arguments(Vec<Value>),
    Closure(Closure),
    /// a builtin: its index in the native table, and a value it carries
    Native(u16, Value),
    Bound(Box<Bound>),
    /// a method of a host object: calling it suspends the VM
    HostFn(Atom),
    Error,
    BoolBox(bool),
    NumBox(f64),
    StrBox(JsStr),
    Date(f64),
    RegExp(Box<RegExpData>),
    /// a scope's captured variables, and the scope around it
    Env(Vec<Value>, Option<ObjRef>),
    /// a for-in walk: the keys still to visit and the object they came from
    ForIn(Vec<Value>, usize, ObjRef),
    /// an object the page owns
    Host(Atom, f64),
    /// a symbol (a primitive in the language; an engine object only to keep its identity)
    Symbol(Atom),
    /// a generator object (also the engine side of an async function's run)
    Generator(Box<GenData>),
    Map(Box<MapData>),
    Set(Box<MapData>),
    /// a WeakMap / WeakSet: its entries live on the keys, under this private name
    WeakMap(Atom),
    WeakSet(Atom),
    /// a Map / Set iterator: the collection (None when finished), the position, keys/values/entries
    CollIter(Option<ObjRef>, usize, u8),
    ArrayBuffer(Vec<u8>),
    TypedArray(TypedArrayData),
    DataView(TypedArrayData),
    Proxy(Box<ProxyData>),
    /// a BigInt (a primitive in the language, compared by value)
    BigInt(Rc<crate::bigint::BigInt>),
}

/// a suspended generator's frame: where it stopped, its locals and operand stack
pub struct SavedFrame {
    pub func: Rc<FuncProto>,
    pub callee: Option<ObjRef>,
    pub ip: usize,
    pub this: Value,
    pub env: Option<ObjRef>,
    /// try handlers (stack heights relative to the frame's base)
    pub handlers: Vec<Handler>,
    pub args: Option<Vec<Value>>,
    pub new_target: Value,
    pub stack: Vec<Value>,
}

#[derive(Clone)]
pub struct Handler {
    pub catch_ip: usize,
    pub sp: usize,
    pub env: Option<ObjRef>,
    /// a finally block's handler (a generator's return() runs these, not catch blocks)
    pub finally: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GenState {
    /// made, not started
    Start,
    /// stopped at a yield / await
    Suspended,
    Running,
    Done,
}

pub struct GenData {
    pub state: GenState,
    pub frame: Option<SavedFrame>,
    /// 0 an ordinary generator, 1 an async function, 2 an async generator
    pub flavor: u8,
    /// what it stopped at: 0 yield, 1 a yield* step, 2 await
    pub yield_kind: u8,
    /// a return() on its way out through finally blocks
    pub ret_val: Option<Value>,
}

#[derive(Clone, Copy)]
pub struct TypedArrayData {
    pub buf: ObjRef,
    pub offset: usize,
    /// elements (bytes for a DataView)
    pub len: usize,
    /// 0 Int8 1 Uint8 2 Uint8Clamped 3 Int16 4 Uint16 5 Int32 6 Uint32 7 Float32 8 Float64 9 BigInt64 10 BigUint64
    pub ty: u8,
}

impl TypedArrayData {
    pub fn elem_size(&self) -> usize {
        match self.ty {
            0..=2 => 1,
            3 | 4 => 2,
            5..=7 => 4,
            _ => 8,
        }
    }
}

pub struct ProxyData {
    /// None once revoked
    pub target: Option<ObjRef>,
    pub handler: Option<ObjRef>,
    pub callable: bool,
}

/// an insertion-ordered hash table under SameValueZero (Map / Set)
#[derive(Default)]
pub struct MapData {
    /// key `Value::Empty` marks a deleted entry
    pub entries: Vec<(Value, Value)>,
    pub live: usize,
    /// open addressing: entry + 1; 0 empty; TOMB a removed slot
    index: Vec<u32>,
    used: usize,
    /// iterators walking the entries (no compaction while there are any)
    pub iters: Vec<ObjRef>,
}

const TOMB: u32 = u32::MAX;

fn mix(mut x: u64) -> u64 {
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51afd7ed558ccd);
    x ^= x >> 33;
    x
}

pub fn value_hash(v: &Value) -> u64 {
    match v {
        Value::Undefined | Value::Empty => 1,
        Value::Null => 2,
        Value::Bool(b) => 3 + *b as u64,
        Value::Num(n) => {
            if *n == 0.0 {
                mix(0)
            } else if n.is_nan() {
                7
            } else {
                mix(n.to_bits())
            }
        },
        Value::Str(s) => mix(crate::strs::hash(s) as u64 ^ 0x5555),
        Value::Obj(o) => mix(o.0 as u64 ^ 0xABCD_0000_0000),
    }
}

impl MapData {
    pub fn find(&self, k: &Value) -> Option<usize> {
        if self.index.is_empty() {
            return None;
        }
        let mask = self.index.len() - 1;
        let mut i = value_hash(k) as usize & mask;
        loop {
            let e = self.index[i];
            if e == 0 {
                return None;
            }
            if e != TOMB {
                let ent = &self.entries[(e - 1) as usize];
                if ent.0.same_value_zero(k) {
                    return Some((e - 1) as usize);
                }
            }
            i = (i + 1) & mask;
        }
    }

    pub fn get(&self, k: &Value) -> Option<&Value> {
        self.find(k).map(|i| &self.entries[i].1)
    }

    fn rehash(&mut self) {
        let n = ((self.live + 1) * 2).next_power_of_two().max(8);
        self.index = alloc::vec![0; n];
        self.used = 0;
        let mask = n - 1;
        for (pos, (k, _)) in self.entries.iter().enumerate() {
            if let Value::Empty = k {
                continue;
            }
            let mut i = value_hash(k) as usize & mask;
            while self.index[i] != 0 {
                i = (i + 1) & mask;
            }
            self.index[i] = pos as u32 + 1;
            self.used += 1;
        }
    }

    pub fn set(&mut self, k: Value, v: Value) {
        let k = match k {
            Value::Num(n) if n == 0.0 => Value::Num(0.0),
            other => other,
        };
        if let Some(i) = self.find(&k) {
            self.entries[i].1 = v;
            return;
        }
        if self.iters.is_empty() && self.entries.len() > 16 && self.entries.len() > self.live * 2 {
            self.entries.retain(|(k, _)| !matches!(k, Value::Empty));
            self.rehash();
        }
        if (self.used + 1) * 2 > self.index.len() {
            self.entries.push((k, v));
            self.live += 1;
            self.rehash();
            return;
        }
        let mask = self.index.len() - 1;
        let mut i = value_hash(&k) as usize & mask;
        while self.index[i] != 0 && self.index[i] != TOMB {
            i = (i + 1) & mask;
        }
        if self.index[i] == 0 {
            self.used += 1;
        }
        self.entries.push((k, v));
        self.live += 1;
        self.index[i] = self.entries.len() as u32;
    }

    pub fn remove(&mut self, k: &Value) -> bool {
        let Some(pos) = self.find(k) else { return false };
        let mask = self.index.len() - 1;
        let mut i = value_hash(k) as usize & mask;
        while self.index[i] != pos as u32 + 1 {
            i = (i + 1) & mask;
        }
        self.index[i] = TOMB;
        self.entries[pos] = (Value::Empty, Value::Undefined);
        self.live -= 1;
        true
    }

    pub fn clear(&mut self) {
        if self.iters.is_empty() {
            self.entries.clear();
        } else {
            for e in self.entries.iter_mut() {
                *e = (Value::Empty, Value::Undefined);
            }
        }
        self.live = 0;
        self.rehash();
    }
}

pub struct Obj {
    pub proto: Option<ObjRef>,
    pub props: PropMap,
    pub kind: Kind,
    pub extensible: bool,
    /// Object.prototype.toString's tag ("Object", "Array", "Function" ...): an atom
    pub class: Atom,
}

impl Obj {
    pub fn new(proto: Option<ObjRef>, kind: Kind, class: Atom) -> Obj {
        Obj { proto, props: PropMap::default(), kind, extensible: true, class }
    }

    pub fn is_callable(&self) -> bool {
        match &self.kind {
            Kind::Closure(_) | Kind::Native(..) | Kind::Bound(_) | Kind::HostFn(_) => true,
            Kind::Proxy(p) => p.callable,
            _ => false,
        }
    }

    pub fn elements(&self) -> Option<&Vec<Value>> {
        match &self.kind {
            Kind::Array(v) | Kind::Arguments(v) => Some(v),
            _ => None,
        }
    }
    pub fn elements_mut(&mut self) -> Option<&mut Vec<Value>> {
        match &mut self.kind {
            Kind::Array(v) | Kind::Arguments(v) => Some(v),
            _ => None,
        }
    }
}

pub struct Heap {
    pub objs: Vec<Option<Obj>>,
    pub free: Vec<u32>,
    pub live: usize,
    pub since_gc: usize,
    pub marks: Vec<bool>,
}

impl Heap {
    pub fn new() -> Heap {
        Heap { objs: Vec::new(), free: Vec::new(), live: 0, since_gc: 0, marks: Vec::new() }
    }

    pub fn alloc(&mut self, o: Obj) -> ObjRef {
        self.live += 1;
        self.since_gc += 1;
        if let Some(i) = self.free.pop() {
            self.objs[i as usize] = Some(o);
            return ObjRef(i);
        }
        self.objs.push(Some(o));
        ObjRef(self.objs.len() as u32 - 1)
    }

    #[inline]
    pub fn get(&self, r: ObjRef) -> &Obj {
        self.objs[r.0 as usize].as_ref().expect("a freed object was used")
    }
    #[inline]
    pub fn get_mut(&mut self, r: ObjRef) -> &mut Obj {
        self.objs[r.0 as usize].as_mut().expect("a freed object was used")
    }

    /// marks everything reachable from `roots`, frees the rest; returns how many were freed
    pub fn collect(&mut self, roots: &[ObjRef], extra: &mut dyn FnMut(&mut dyn FnMut(&Value))) -> usize {
        let n = self.objs.len();
        self.marks.clear();
        self.marks.resize(n, false);
        let mut work: Vec<ObjRef> = roots.to_vec();
        {
            let mut push = |v: &Value| {
                if let Value::Obj(o) = v {
                    work.push(*o);
                }
            };
            extra(&mut push);
        }
        while let Some(r) = work.pop() {
            let i = r.0 as usize;
            if i >= n || self.marks[i] {
                continue;
            }
            self.marks[i] = true;
            let o = match &self.objs[i] {
                Some(o) => o,
                None => continue,
            };
            if let Some(p) = o.proto {
                work.push(p);
            }
            for s in &o.props.slots {
                match s {
                    Slot::Data(Value::Obj(x), _) => work.push(*x),
                    Slot::Accessor(g, s, _) => {
                        if let Some(g) = g {
                            work.push(*g);
                        }
                        if let Some(s) = s {
                            work.push(*s);
                        }
                    },
                    _ => {},
                }
            }
            let mut val = |v: &Value, w: &mut Vec<ObjRef>| {
                if let Value::Obj(x) = v {
                    w.push(*x);
                }
            };
            match &o.kind {
                Kind::Array(v) | Kind::Arguments(v) => {
                    for x in v {
                        val(x, &mut work);
                    }
                },
                Kind::Closure(c) => {
                    if let Some(e) = c.env {
                        work.push(e);
                    }
                    if let Some(hm) = c.home {
                        work.push(hm);
                    }
                    if let Some(f) = c.fields {
                        work.push(f);
                    }
                    if let Some(t) = &c.this_val {
                        val(t, &mut work);
                    }
                },
                Kind::Native(_, d) => val(d, &mut work),
                Kind::Bound(b) => {
                    work.push(b.target);
                    val(&b.this, &mut work);
                    for x in &b.args {
                        val(x, &mut work);
                    }
                },
                Kind::Env(v, p) => {
                    for x in v {
                        val(x, &mut work);
                    }
                    if let Some(p) = p {
                        work.push(*p);
                    }
                },
                Kind::Generator(g) => {
                    if let Some(f) = &g.frame {
                        if let Some(c) = f.callee {
                            work.push(c);
                        }
                        if let Some(e) = f.env {
                            work.push(e);
                        }
                        for h in &f.handlers {
                            if let Some(e) = h.env {
                                work.push(e);
                            }
                        }
                        val(&f.this, &mut work);
                        val(&f.new_target, &mut work);
                        if let Some(a) = &f.args {
                            for x in a {
                                val(x, &mut work);
                            }
                        }
                        for x in &f.stack {
                            val(x, &mut work);
                        }
                    }
                    if let Some(r) = &g.ret_val {
                        val(r, &mut work);
                    }
                },
                Kind::Map(m) | Kind::Set(m) => {
                    for (k, v) in &m.entries {
                        val(k, &mut work);
                        val(v, &mut work);
                    }
                    work.extend_from_slice(&m.iters);
                },
                Kind::CollIter(Some(c), _, _) => work.push(*c),
                Kind::TypedArray(t) | Kind::DataView(t) => work.push(t.buf),
                Kind::Proxy(p) => {
                    if let Some(t) = p.target {
                        work.push(t);
                    }
                    if let Some(h) = p.handler {
                        work.push(h);
                    }
                },
                Kind::ForIn(v, _, o) => {
                    for x in v {
                        val(x, &mut work);
                    }
                    work.push(*o);
                },
                _ => {},
            }
        }
        let mut freed = 0;
        for i in 0..n {
            if !self.marks[i] && self.objs[i].is_some() {
                self.objs[i] = None;
                self.free.push(i as u32);
                freed += 1;
            }
        }
        self.live -= freed;
        self.since_gc = 0;
        freed
    }
}
