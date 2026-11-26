//! The virtual machine: runs bytecode on an explicit frame stack (no Rust recursion per JS
//! call), so a run can stop anywhere and pick up later. That is how host objects work: touching
//! one rewinds the current instruction and returns [`Status::Host`]; [`Engine::resume`] stores
//! the answer and re-runs the instruction, which takes it.

use crate::atoms::{k, Atom, Atoms};
use crate::compiler::{Compiler, FuncProto, Op};
use crate::object::*;
use crate::strs::{self, js, JsStr};
use crate::value::Value;
use crate::num::F64Ext;
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::rc::Rc;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

/// What a run ended with.
#[derive(Debug)]
pub enum Status {
    /// the script / call finished with this value
    Done(Value),
    /// a script threw this (uncaught)
    Error(Value),
    /// the page must answer: call [`Engine::resume`] (or [`Engine::resume_throw`])
    Host(HostReq),
}

#[derive(Debug, Clone)]
pub enum HostReq {
    /// read `key` of a host object: answer the value, or `Value::Empty` for "not mine" (the
    /// engine then looks the property up as usual: expandos, prototype methods)
    Get { obj: ObjRef, class: JsStr, id: f64, key: JsStr },
    /// write `key`: answer `true` when the host took it, `false` to store it as an expando
    Set { obj: ObjRef, class: JsStr, id: f64, key: JsStr, value: Value },
    /// call a host method (`new_target` set for `new`): answer the result
    Call { func: ObjRef, name: JsStr, this: Value, args: Vec<Value>, construct: bool },
    /// fetch a module's source (`url` is resolved and absolute): answer `resume(Value::Str(source))`,
    /// or `resume_throw(error)` when it cannot be had
    Import { url: JsStr },
}

/// a failure inside a builtin: a JS exception, or "this needs JavaScript" (the prelude's
/// slow version of the builtin takes over)
pub enum Fault {
    Throw(Value),
    NeedJs,
}

pub type NResult<T> = Result<T, Fault>;

pub enum NOut {
    Val(Value),
    TailCall(Value, Value, Vec<Value>),
    TailNew(Value, Vec<Value>),
    /// resume a generator: (generator, 0 next / 1 throw / 2 return, the value)
    GenResume(ObjRef, u8, Value),
    /// run a compiled script (eval): its completion value is the result
    RunScript(Rc<FuncProto>),
    /// `new f(...args)` with this new.target
    TailConstruct(Value, Vec<Value>, Value),
}

pub type NativeFn = fn(&mut Engine, &Value, &[Value], &Value, bool) -> NResult<NOut>;

#[derive(Clone, Debug)]
pub enum Cont {
    Push,
    Discard,
    Construct(ObjRef),
    /// ToPrimitive: the result replaces stack[slot] and the caller re-runs its instruction
    Prim { slot: usize, obj: ObjRef, number_first: bool, stage: u8 },
    /// back to the embedder
    Host,
    /// back to the embedder from `new` (an object result, else the new object)
    HostConstruct(ObjRef),
    /// a construct result: a non-object becomes the second (when set); an object gets the first
    /// as its prototype (a builtin made for a subclass); then the inner continuation
    FixProto(Option<ObjRef>, Option<ObjRef>, Box<Cont>),
}

struct Frame {
    func: Rc<FuncProto>,
    callee: Option<ObjRef>,
    ip: usize,
    base: usize,
    /// the caller's stack height to go back to
    ret_sp: usize,
    this: Value,
    env: Option<ObjRef>,
    handlers: Vec<Handler>,
    args: Option<Vec<Value>>,
    new_target: Value,
    cont: Cont,
    /// a generator's (or async function's) frame: its generator object
    gen: Option<ObjRef>,
}

#[derive(Clone, Copy)]
pub enum Key {
    Atom(Atom),
    Index(u32),
}

pub struct Realm {
    pub global: ObjRef,
    pub object_proto: ObjRef,
    pub function_proto: ObjRef,
    pub array_proto: ObjRef,
    pub string_proto: ObjRef,
    pub number_proto: ObjRef,
    pub boolean_proto: ObjRef,
    pub error_proto: ObjRef,
    pub type_error_proto: ObjRef,
    pub range_error_proto: ObjRef,
    pub syntax_error_proto: ObjRef,
    pub reference_error_proto: ObjRef,
    pub eval_error_proto: ObjRef,
    pub uri_error_proto: ObjRef,
    pub regexp_proto: ObjRef,
    pub date_proto: ObjRef,
    /// prototypes of the newer builtins (Symbol, generators, Map, Set, typed arrays ...), by name
    pub protos: BTreeMap<&'static str, ObjRef>,
    pub intrinsics: Vec<Value>,
}

pub struct Engine {
    pub atoms: Atoms,
    pub heap: Heap,
    pub realm: Realm,
    pub(crate) natives: Vec<(&'static str, NativeFn)>,
    /// slow (JavaScript) versions of natives, by native index
    pub(crate) slow: BTreeMap<u16, Value>,
    stack: Vec<Value>,
    frames: Vec<Frame>,
    /// the host's answer to the request the current instruction made
    answer: Option<Value>,
    pub budget: u64,
    pub steps: u64,
    pub budget_hit: bool,
    host_objs: BTreeMap<(Atom, u64), ObjRef>,
    /// the scripts' shared top-level let / const / class bindings
    pub(crate) global_lex: ObjRef,
    native_objs: BTreeMap<u16, ObjRef>,
    pinned: BTreeMap<u32, u32>,
    pub(crate) int_atoms: Vec<Atom>,
    pub now_ms: f64,
    pub tz_offset_min: f64,
    /// the page's URL: what a classic script's import() resolves against
    pub base_url: String,
    pub log: Vec<String>,
    pub max_frames: usize,
    /// debugging: log every exception as it is thrown (caught or not)
    pub trace_throws: bool,
    pub gc_threshold: usize,
    /// a native that faulted with NeedJs: its slow version runs instead
    pub(crate) random_state: u64,
    /// the microtask queue: (function, this, arguments)
    pub(crate) jobs: alloc::collections::VecDeque<(Value, Value, Vec<Value>)>,
    /// frame heights where a run_jobs drain waits for its current job to finish
    drain: Vec<usize>,
    /// rejected promises nothing has handled yet (reported when the queue drains)
    pub(crate) unhandled: Vec<(ObjRef, Value)>,
    /// the object of each symbol atom; Symbol.for's registry
    pub(crate) sym_objs: BTreeMap<u32, ObjRef>,
    pub(crate) sym_registry: BTreeMap<Vec<u16>, ObjRef>,
    line_tables: core::cell::RefCell<BTreeMap<usize, Rc<Vec<u32>>>>,
}

const MAX_FRAMES: usize = 400;

fn err_slot() -> Value {
    Value::Undefined
}

impl Engine {
    pub fn new() -> Engine {
        let mut atoms = Atoms::new();
        let mut heap = Heap::new();
        let object_class = atoms.intern_str("Object");
        let placeholder = Realm {
            global: ObjRef(0),
            object_proto: ObjRef(0),
            function_proto: ObjRef(0),
            array_proto: ObjRef(0),
            string_proto: ObjRef(0),
            number_proto: ObjRef(0),
            boolean_proto: ObjRef(0),
            error_proto: ObjRef(0),
            type_error_proto: ObjRef(0),
            range_error_proto: ObjRef(0),
            syntax_error_proto: ObjRef(0),
            reference_error_proto: ObjRef(0),
            eval_error_proto: ObjRef(0),
            uri_error_proto: ObjRef(0),
            regexp_proto: ObjRef(0),
            date_proto: ObjRef(0),
            protos: BTreeMap::new(),
            intrinsics: Vec::new(),
        };
        let _ = heap.alloc(Obj::new(None, Kind::Ordinary, object_class));
        let mut int_atoms = Vec::with_capacity(1024);
        for i in 0..1024u32 {
            int_atoms.push(atoms.intern_str(&format!("{}", i)));
        }
        let mut e = Engine {
            atoms,
            heap,
            realm: placeholder,
            natives: Vec::new(),
            slow: BTreeMap::new(),
            stack: Vec::with_capacity(4096),
            frames: Vec::new(),
            answer: None,
            budget: 50_000_000,
            steps: 0,
            budget_hit: false,
            host_objs: BTreeMap::new(),
            global_lex: ObjRef(0),
            native_objs: BTreeMap::new(),
            pinned: BTreeMap::new(),
            int_atoms,
            now_ms: 0.0,
            tz_offset_min: 0.0,
            base_url: String::new(),
            log: Vec::new(),
            max_frames: MAX_FRAMES,
            trace_throws: false,
            gc_threshold: 20000,
            random_state: 0x9E3779B97F4A7C15,
            jobs: alloc::collections::VecDeque::new(),
            drain: Vec::new(),
            unhandled: Vec::new(),
            sym_objs: BTreeMap::new(),
            sym_registry: BTreeMap::new(),
            line_tables: core::cell::RefCell::new(BTreeMap::new()),
        };
        crate::builtins::setup(&mut e);
        e.global_lex = e.new_object_proto(None);
        crate::prelude::load(&mut e);
        e
    }

    // ── small helpers for builtins and the embedder ──

    pub fn atom(&mut self, s: &str) -> Atom {
        self.atoms.intern_str(s)
    }

    pub fn atom_of(&mut self, s: &[u16]) -> Atom {
        self.atoms.intern(s)
    }

    pub fn index_atom(&mut self, i: u32) -> Atom {
        if (i as usize) < self.int_atoms.len() {
            self.int_atoms[i as usize]
        } else {
            self.atoms.intern_str(&format!("{}", i))
        }
    }

    pub fn key_atom(&mut self, k: Key) -> Atom {
        match k {
            Key::Atom(a) => a,
            Key::Index(i) => self.index_atom(i),
        }
    }

    pub fn key_of_str(&mut self, s: &[u16]) -> Key {
        match strs::array_index(s) {
            Some(i) => Key::Index(i),
            None => Key::Atom(self.atoms.intern(s)),
        }
    }

    pub fn key_of_atom(&self, a: Atom) -> Key {
        if self.atoms.is_symbol(a) {
            return Key::Atom(a);
        }
        match strs::array_index(self.atoms.name(a)) {
            Some(i) => Key::Index(i),
            None => Key::Atom(a),
        }
    }

    pub fn key_name(&mut self, k: Key) -> JsStr {
        match k {
            Key::Atom(a) => self.atoms.name(a).clone(),
            Key::Index(i) => js(&format!("{}", i)),
        }
    }

    pub fn new_object(&mut self) -> ObjRef {
        let c = self.atom("Object");
        self.heap.alloc(Obj::new(Some(self.realm.object_proto), Kind::Ordinary, c))
    }

    pub fn new_object_proto(&mut self, proto: Option<ObjRef>) -> ObjRef {
        let c = self.atom("Object");
        self.heap.alloc(Obj::new(proto, Kind::Ordinary, c))
    }

    pub fn new_array(&mut self, items: Vec<Value>) -> ObjRef {
        let c = self.atom("Array");
        self.heap.alloc(Obj::new(Some(self.realm.array_proto), Kind::Array(items), c))
    }

    pub fn new_native(&mut self, name: &str, idx: u16, length: u32, data: Value) -> ObjRef {
        let c = self.atom("Function");
        let o = self.heap.alloc(Obj::new(Some(self.realm.function_proto), Kind::Native(idx, data), c));
        let n = self.atom("name");
        let l = self.atom("length");
        self.heap.get_mut(o).props.insert(n, Slot::Data(Value::str(name), CONFIGURABLE));
        self.heap.get_mut(o).props.insert(l, Slot::Data(Value::Num(length as f64), CONFIGURABLE));
        o
    }

    /// a data property, as a builtin defines it
    pub fn define(&mut self, o: ObjRef, name: &str, v: Value, flags: u8) {
        let a = self.atom(name);
        self.heap.get_mut(o).props.insert(a, Slot::Data(v, flags));
    }

    pub fn define_atom(&mut self, o: ObjRef, a: Atom, v: Value, flags: u8) {
        self.heap.get_mut(o).props.insert(a, Slot::Data(v, flags));
    }

    /// a global object's own data property, as an object (the builtins' constructors)
    pub fn global_obj(&mut self, name: &str) -> ObjRef {
        let a = self.atom(name);
        match self.heap.get(self.realm.global).props.get(a) {
            Some(Slot::Data(Value::Obj(o), _)) => *o,
            _ => self.realm.object_proto,
        }
    }

    /// a builtin prototype by name ("Symbol", "Map", "Generator" ...)
    pub fn proto(&self, name: &str) -> ObjRef {
        self.realm.protos.get(name).copied().unwrap_or(self.realm.object_proto)
    }

    /// the value of a symbol atom (its object, made once)
    pub fn symbol_value(&mut self, a: Atom) -> Value {
        if let Some(o) = self.sym_objs.get(&a.0) {
            return Value::Obj(*o);
        }
        let c = self.atom("Symbol");
        let p = self.proto("Symbol");
        let mut ob = Obj::new(Some(p), Kind::Symbol(a), c);
        ob.extensible = false;
        let o = self.heap.alloc(ob);
        self.sym_objs.insert(a.0, o);
        Value::Obj(o)
    }

    /// a new symbol (`private`: a name nothing enumerates, for the engine's own hidden slots)
    pub fn new_symbol(&mut self, desc: &[u16], private: bool) -> Value {
        let a = self.atoms.new_symbol(desc, private);
        self.symbol_value(a)
    }

    /// the atom of a symbol value
    pub fn sym_of(&self, v: &Value) -> Option<Atom> {
        match v {
            Value::Obj(o) => match self.heap.get(*o).kind {
                Kind::Symbol(a) => Some(a),
                _ => None,
            },
            _ => None,
        }
    }

    pub fn bigint_of(&self, v: &Value) -> Option<Rc<crate::bigint::BigInt>> {
        match v {
            Value::Obj(o) => match &self.heap.get(*o).kind {
                Kind::BigInt(b) => Some(b.clone()),
                _ => None,
            },
            _ => None,
        }
    }

    pub fn new_bigint(&mut self, b: crate::bigint::BigInt) -> Value {
        let c = self.atom("BigInt");
        let p = self.proto("BigInt");
        let mut ob = Obj::new(Some(p), Kind::BigInt(Rc::new(b)), c);
        ob.extensible = false;
        Value::Obj(self.heap.alloc(ob))
    }

    /// a primitive that lives on the heap (a symbol or a BigInt)?
    #[inline]
    pub fn is_heap_prim(&self, v: &Value) -> bool {
        match v {
            Value::Obj(o) => matches!(self.heap.get(*o).kind, Kind::Symbol(_) | Kind::BigInt(_)),
            _ => false,
        }
    }

    /// "is an object" in the language sense (symbols and BigInts are not)
    #[inline]
    pub fn is_object(&self, v: &Value) -> bool {
        matches!(v, Value::Obj(_)) && !self.is_heap_prim(v)
    }

    pub fn str_val(&self, v: &Value) -> Option<JsStr> {
        match v {
            Value::Str(s) => Some(s.clone()),
            _ => None,
        }
    }

    pub fn type_error(&mut self, msg: &str) -> Value {
        let p = self.realm.type_error_proto;
        self.make_error(p, msg)
    }
    pub fn range_error(&mut self, msg: &str) -> Value {
        let p = self.realm.range_error_proto;
        self.make_error(p, msg)
    }
    pub fn reference_error(&mut self, msg: &str) -> Value {
        let p = self.realm.reference_error_proto;
        self.make_error(p, msg)
    }
    pub fn syntax_error(&mut self, msg: &str) -> Value {
        let p = self.realm.syntax_error_proto;
        self.make_error(p, msg)
    }

    pub fn make_error(&mut self, proto: ObjRef, msg: &str) -> Value {
        let c = self.atom("Error");
        let o = self.heap.alloc(Obj::new(Some(proto), Kind::Error, c));
        self.define_atom(o, k::message, Value::str(msg), HIDDEN);
        let stack = self.stack_trace();
        let name = self.error_name(proto);
        self.define_atom(o, k::stack, Value::str(&format!("{}: {}\n{}", name, msg, stack)), HIDDEN);
        Value::Obj(o)
    }

    fn error_name(&mut self, proto: ObjRef) -> String {
        match self.heap.get(proto).props.get(k::name) {
            Some(Slot::Data(Value::Str(s), _)) => strs::to_rust(s),
            _ => "Error".into(),
        }
    }

    /// (line, column) of a char offset in a source (1-based), from a cached table of line starts
    fn line_col(&self, src: &Rc<str>, at: usize) -> (usize, usize) {
        let key = Rc::as_ptr(src) as *const u8 as usize ^ src.len();
        let table = {
            let mut cache = self.line_tables.borrow_mut();
            cache
                .entry(key)
                .or_insert_with(|| {
                    let mut v = vec![0u32];
                    for (i, c) in src.chars().enumerate() {
                        if c == '\n' {
                            v.push(i as u32 + 1);
                        }
                    }
                    Rc::new(v)
                })
                .clone()
        };
        let line = match table.binary_search(&(at as u32)) {
            Ok(i) => i,
            Err(i) => i - 1,
        };
        (line + 1, at - table[line] as usize + 1)
    }

    pub fn stack_trace(&self) -> String {
        let mut out = String::new();
        for f in self.frames.iter().rev().take(10) {
            let n = strs::to_rust(&f.func.name);
            let at = match &f.func.src {
                Some((src, a, _)) => {
                    let (l, c) = self.line_col(src, *a);
                    format!("line {}:{}", l, c)
                },
                None => format!("line {}", line_of(&f.func, f.ip)),
            };
            out.push_str(&format!("    at {} ({})\n", if n.is_empty() { "<anonymous>" } else { &n }, at));
        }
        out
    }

    pub fn throw_type<T>(&mut self, msg: &str) -> NResult<T> {
        Err(Fault::Throw(self.type_error(msg)))
    }

    /// the class atom of an object ("Array", "Function" ...)
    pub fn class_of(&self, o: ObjRef) -> Atom {
        self.heap.get(o).class
    }

    pub fn is_callable(&self, v: &Value) -> bool {
        match v {
            Value::Obj(o) => self.heap.get(*o).is_callable(),
            _ => false,
        }
    }

    pub fn is_constructor(&self, v: &Value) -> bool {
        match v {
            Value::Obj(o) => match &self.heap.get(*o).kind {
                Kind::Closure(c) => {
                    matches!(c.func.kind, crate::ast::FuncKind::ClassConstructor | crate::ast::FuncKind::DerivedConstructor)
                        || (!c.func.arrow && !c.func.method && !c.func.generator && !c.func.is_async)
                },
                Kind::Proxy(p) => p.callable && p.target.is_some_and(|t| self.is_constructor(&Value::Obj(t))),
                Kind::Native(..) => self.heap.get(*o).props.contains(k::prototype),
                Kind::Bound(b) => self.is_constructor(&Value::Obj(b.target)),
                Kind::HostFn(_) => true,
                _ => false,
            },
            _ => false,
        }
    }

    // ── embedder API ──

    pub fn global(&self) -> ObjRef {
        self.realm.global
    }

    /// the engine object for a host object (the same one every time for the same class and id)
    pub fn host_object(&mut self, class: &str, id: f64, proto: Option<ObjRef>) -> ObjRef {
        let c = self.atom(class);
        if let Some(o) = self.host_objs.get(&(c, id.to_bits())) {
            return *o;
        }
        let proto = proto.or(Some(self.realm.object_proto));
        let o = self.heap.alloc(Obj::new(proto, Kind::Host(c, id), c));
        self.host_objs.insert((c, id.to_bits()), o);
        o
    }

    /// a function value that, called, asks the host (`name` tells the host which)
    pub fn host_function(&mut self, name: &str, length: u32) -> ObjRef {
        let a = self.atom(name);
        let c = self.atom("Function");
        let o = self.heap.alloc(Obj::new(Some(self.realm.function_proto), Kind::HostFn(a), c));
        self.heap.get_mut(o).props.insert(k::name, Slot::Data(Value::str(name), CONFIGURABLE));
        self.heap.get_mut(o).props.insert(k::length, Slot::Data(Value::Num(length as f64), CONFIGURABLE));
        o
    }

    /// keep an object alive while the host holds it (counted)
    pub fn pin(&mut self, o: ObjRef) {
        *self.pinned.entry(o.0).or_insert(0) += 1;
    }
    pub fn unpin(&mut self, o: ObjRef) {
        if let Some(n) = self.pinned.get_mut(&o.0) {
            *n -= 1;
            if *n == 0 {
                self.pinned.remove(&o.0);
            }
        }
    }

    /// compile and start running a script
    pub fn eval(&mut self, src: &str) -> Status {
        let prog = match crate::parser::parse_program(src) {
            Ok(p) => p,
            Err(e) => {
                let v = self.syntax_error(&format!("{} (line {})", e.msg, e.line));
                return Status::Error(v);
            },
        };
        let comp = Compiler::new(&mut self.atoms, src);
        let proto = match comp.compile_script(&prog) {
            Ok(p) => p,
            Err(e) => {
                let v = self.syntax_error(&e.0);
                return Status::Error(v);
            },
        };
        self.run_script(proto)
    }

    /// does the source parse? (for a quick check without running)
    /// load, link and evaluate a module (`url` is its absolute URL, what its imports resolve
    /// against). Imports ask the host with [`HostReq::Import`]. A module is evaluated once per URL.
    /// Done(undefined) when it ran to the end, Done(a promise) when it awaits (top-level await or
    /// an awaiting dependency): run the jobs to finish it.
    pub fn eval_module(&mut self, src: &str, url: &str) -> Status {
        if self.frames.is_empty() {
            self.steps = 0;
            self.budget_hit = false;
        }
        let f = self.realm.intrinsics[crate::compiler::intr::MODULE_EVAL as usize].clone();
        self.call_inner(f, Value::Undefined, vec![Value::str(src), Value::str(url)])
    }

    /// compile a module (the prelude's loader calls this)
    pub(crate) fn compile_module_src(&mut self, src: &str) -> NResult<Rc<FuncProto>> {
        match crate::compiler::compile_module(&mut self.atoms, src) {
            Ok(p) => Ok(p),
            Err(e) => {
                let v = self.syntax_error(&e.0);
                Err(Fault::Throw(v))
            },
        }
    }

    pub fn check_syntax(src: &str) -> Result<(), String> {
        crate::parser::parse_program(src).map(|_| ()).map_err(|e| format!("{} (line {})", e.msg, e.line))
    }

    fn run_script(&mut self, proto: Rc<FuncProto>) -> Status {
        if self.frames.is_empty() {
            self.steps = 0;
            self.budget_hit = false;
        }
        self.push_script(proto, Cont::Host);
        self.run()
    }

    fn push_script(&mut self, proto: Rc<FuncProto>, cont: Cont) {
        let base = self.stack.len();
        for _ in 0..proto.nlocals {
            self.stack.push(Value::Undefined);
        }
        let g = Value::Obj(self.realm.global);
        self.frames.push(Frame { func: proto, callee: None, ip: 0, base, ret_sp: base, this: g, env: None, handlers: Vec::new(), args: None, new_target: Value::Undefined, cont, gen: None });
    }

    /// compile code made at run time (eval, Function): it runs in the global scope; its top-level
    /// let / const / class stay its own
    pub(crate) fn compile_runtime(&mut self, src: &str) -> NResult<Rc<FuncProto>> {
        let mut prog = match crate::parser::parse_program(src) {
            Ok(p) => p,
            Err(e) => {
                let v = self.syntax_error(&format!("{} (line {})", e.msg, e.line));
                return Err(Fault::Throw(v));
            },
        };
        let lexical = prog.body.iter().any(|s| matches!(s, crate::ast::Stmt::Var(k, _) if *k != crate::ast::VarKind::Var) || matches!(s, crate::ast::Stmt::Class(_)));
        if lexical {
            let body = core::mem::take(&mut prog.body);
            prog.body = vec![crate::ast::Stmt::Block(body)];
        }
        let comp = Compiler::new(&mut self.atoms, src);
        match comp.compile_script(&prog) {
            Ok(p) => Ok(p),
            Err(e) => {
                let v = self.syntax_error(&e.0);
                Err(Fault::Throw(v))
            },
        }
    }

    /// call a function (an event handler, a timer) and run until it returns or asks the host
    pub fn call(&mut self, f: Value, this: Value, args: Vec<Value>) -> Status {
        if self.frames.is_empty() {
            self.steps = 0;
            self.budget_hit = false;
        }
        self.call_inner(f, this, args)
    }

    fn call_inner(&mut self, f: Value, this: Value, args: Vec<Value>) -> Status {
        let depth = self.frames.len();
        match self.call_value(f, this, args, Cont::Host, false) {
            Ok(Some(v)) => Status::Done(v),
            Ok(None) => self.run(),
            Err(e) => {
                // a frame pushed for this call (a resumed generator) unwinds first
                if self.frames.len() > depth {
                    match self.throw(e) {
                        Ok(()) => self.run(),
                        Err(e) => Status::Error(e),
                    }
                } else {
                    Status::Error(e)
                }
            },
        }
    }

    pub fn construct(&mut self, f: Value, args: Vec<Value>) -> Status {
        match self.construct_value(f, args, Cont::Host) {
            Ok(Some(v)) => Status::Done(v),
            Ok(None) => self.run(),
            Err(e) => Status::Error(e),
        }
    }

    /// the host's answer to the last [`Status::Host`]
    pub fn resume(&mut self, answer: Value) -> Status {
        self.answer = Some(answer);
        let st = self.run();
        self.after_run(st)
    }

    /// the host failed the request: throw this into the script
    pub fn resume_throw(&mut self, exc: Value) -> Status {
        let st = match self.throw(exc) {
            Ok(()) => self.run(),
            Err(e) => Status::Error(e),
        };
        self.after_run(st)
    }

    /// seed Math.random (a page gets its own sequence)
    pub fn seed_random(&mut self, seed: u64) {
        self.random_state = if seed == 0 { 0x9E3779B97F4A7C15 } else { seed };
    }

    // ── the microtask queue ──

    /// are there queued jobs (promise reactions, queueMicrotask callbacks)?
    pub fn has_jobs(&self) -> bool {
        !self.jobs.is_empty()
    }

    /// queue a job: `f.call(this, ...args)` runs when the queue drains
    pub fn enqueue_job(&mut self, f: Value, this: Value, args: Vec<Value>) {
        self.jobs.push_back((f, this, args));
    }

    /// run queued jobs until the queue is empty: `Done(undefined)` then. A job that touches
    /// the page returns `Status::Host`; [`Engine::resume`] then finishes it and keeps draining.
    /// A job that throws (or a rejection nobody handled) is logged ("uncaught ...") and the
    /// queue goes on.
    pub fn run_jobs(&mut self) -> Status {
        if self.frames.is_empty() {
            self.steps = 0;
            self.budget_hit = false;
        }
        self.drain.push(self.frames.len());
        self.drain_jobs()
    }

    fn drain_jobs(&mut self) -> Status {
        loop {
            if self.budget_hit {
                if !self.jobs.is_empty() {
                    self.jobs.clear();
                    self.log.push("microtasks stopped: script ran too long".into());
                }
            }
            let (f, this, args) = match self.jobs.pop_front() {
                Some(j) => j,
                None => {
                    self.drain.pop();
                    self.report_rejections();
                    return Status::Done(Value::Undefined);
                },
            };
            let st = self.call_inner(f, this, args);
            match st {
                Status::Host(r) => return Status::Host(r),
                Status::Error(v) => self.log_uncaught("uncaught", &v),
                Status::Done(_) => {},
            }
        }
    }

    /// a run finished (or stopped for the host): when it was a job of a drain, the drain goes on
    fn after_run(&mut self, st: Status) -> Status {
        if let Some(&base) = self.drain.last() {
            if self.frames.len() == base && !matches!(st, Status::Host(_)) {
                if let Status::Error(v) = &st {
                    let v = v.clone();
                    self.log_uncaught("uncaught", &v);
                }
                return self.drain_jobs();
            }
        }
        st
    }

    fn log_uncaught(&mut self, what: &str, v: &Value) {
        let m = self.display(v);
        if self.log.len() < 2000 {
            self.log.push(format!("{} {}", what, m));
        }
    }

    fn report_rejections(&mut self) {
        let list = core::mem::take(&mut self.unhandled);
        for (_, reason) in list {
            self.log_uncaught("uncaught (in promise)", &reason);
        }
    }

    /// is a run suspended, waiting for the host?
    pub fn suspended(&self) -> bool {
        !self.frames.is_empty()
    }

    /// give up on a suspended run (the page went away)
    pub fn abort(&mut self) {
        while let Some(f) = self.frames.pop() {
            self.stack.truncate(f.ret_sp);
            if let Some(g) = f.gen {
                self.gen_done(g);
            }
            if let Cont::Host | Cont::HostConstruct(_) = f.cont {
                break;
            }
        }
        self.answer = None;
        let n = self.frames.len();
        self.drain.retain(|&b| b < n);
    }

    // ── garbage collection ──

    pub fn gc(&mut self) -> usize {
        let mut roots: Vec<ObjRef> = Vec::new();
        let r = &self.realm;
        roots.extend_from_slice(&[
            r.global,
            r.object_proto,
            r.function_proto,
            r.array_proto,
            r.string_proto,
            r.number_proto,
            r.boolean_proto,
            r.error_proto,
            r.type_error_proto,
            r.range_error_proto,
            r.syntax_error_proto,
            r.reference_error_proto,
            r.eval_error_proto,
            r.uri_error_proto,
            r.regexp_proto,
            r.date_proto,
            self.global_lex,
        ]);
        roots.extend(self.host_objs.values().copied());
        roots.extend(self.realm.protos.values().copied());
        roots.extend(self.sym_objs.values().copied());
        roots.extend(self.native_objs.values().copied());
        roots.extend(self.pinned.keys().map(|k| ObjRef(*k)));
        for f in &self.frames {
            if let Some(c) = f.callee {
                roots.push(c);
            }
            if let Some(g) = f.gen {
                roots.push(g);
            }
            if let Some(e) = f.env {
                roots.push(e);
            }
            for h in &f.handlers {
                if let Some(e) = h.env {
                    roots.push(e);
                }
            }
            let mut c = &f.cont;
            while let Cont::FixProto(p, d, inner) = c {
                roots.extend(p.iter().copied());
                roots.extend(d.iter().copied());
                c = inner;
            }
            match c {
                Cont::Construct(o) | Cont::HostConstruct(o) => roots.push(*o),
                Cont::Prim { obj, .. } => roots.push(*obj),
                _ => {},
            }
        }
        let stack = &self.stack;
        let frames = &self.frames;
        let intr = &self.realm.intrinsics;
        let slow = &self.slow;
        let answer = &self.answer;
        let jobs = &self.jobs;
        let unhandled = &self.unhandled;
        let mut extra = |push: &mut dyn FnMut(&Value)| {
            for v in stack {
                push(v);
            }
            for f in frames {
                push(&f.this);
                push(&f.new_target);
                if let Some(a) = &f.args {
                    for v in a {
                        push(v);
                    }
                }
            }
            for v in intr {
                push(v);
            }
            for v in slow.values() {
                push(v);
            }
            if let Some(a) = answer {
                push(a);
            }
            for (f, t, a) in jobs {
                push(f);
                push(t);
                for v in a {
                    push(v);
                }
            }
            for (p, r) in unhandled {
                push(&Value::Obj(*p));
                push(r);
            }
        };
        let freed = self.heap.collect(&roots, &mut extra);
        self.gc_threshold = (self.heap.live * 2).max(20000);
        freed
    }

    // ── conversions (no JavaScript calls: NeedJs when one would be needed) ──

    pub fn truthy(&self, v: &Value) -> bool {
        match v {
            Value::Undefined | Value::Null | Value::Empty => false,
            Value::Bool(b) => *b,
            Value::Num(n) => !(*n == 0.0 || n.is_nan()),
            Value::Str(s) => !s.is_empty(),
            Value::Obj(o) => match &self.heap.get(*o).kind {
                Kind::BigInt(b) => !b.is_zero(),
                _ => true,
            },
        }
    }

    pub fn typeof_str(&self, v: &Value) -> &'static str {
        match v {
            Value::Undefined | Value::Empty => "undefined",
            Value::Null => "object",
            Value::Bool(_) => "boolean",
            Value::Num(_) => "number",
            Value::Str(_) => "string",
            Value::Obj(o) => {
                let ob = self.heap.get(*o);
                match ob.kind {
                    Kind::Symbol(_) => "symbol",
                    Kind::BigInt(_) => "bigint",
                    _ if ob.is_callable() => "function",
                    _ => "object",
                }
            },
        }
    }

    /// ToPrimitive without running script: Err(NeedJs) when a user method would run
    pub fn to_primitive(&mut self, v: &Value, number_first: bool) -> NResult<Value> {
        let o = match v {
            Value::Obj(o) => *o,
            other => return Ok(other.clone()),
        };
        // the builtin conversions, done here
        match &self.heap.get(o).kind {
            Kind::Symbol(_) | Kind::BigInt(_) => return Ok(v.clone()),
            Kind::Date(t) if !number_first || true => {
                let t = *t;
                let has_own = self.user_method(o, k::valueOf).is_some() || self.user_method(o, k::toString).is_some();
                if !has_own {
                    if number_first {
                        return Ok(Value::Num(t));
                    }
                    return Ok(Value::Str(js(&crate::builtins::date_to_string(self, t))));
                }
            },
            _ => {},
        }
        if let Some(f) = self.lookup_method(o, crate::atoms::sym::toPrimitive)? {
            if !f.is_nullish() {
                return Err(Fault::NeedJs);
            }
        }
        let order = if number_first { [k::valueOf, k::toString] } else { [k::toString, k::valueOf] };
        for m in order {
            let f = self.lookup_method(o, m)?;
            if let Some(f) = f {
                if let Value::Obj(fo) = &f {
                    match &self.heap.get(*fo).kind {
                        Kind::Native(..) => {
                            let x = self.call_native_direct(f.clone(), v.clone(), vec![], 0)?;
                            if x.as_obj().is_none() {
                                return Ok(x);
                            }
                        },
                        _ if self.heap.get(*fo).is_callable() => return Err(Fault::NeedJs),
                        _ => {},
                    }
                }
            }
        }
        self.throw_type("Cannot convert object to primitive value")
    }

    /// run a native (and the natives it tail-calls) right here; NeedJs when script would run
    pub fn call_native_direct(&mut self, f: Value, this: Value, args: Vec<Value>, depth: u32) -> NResult<Value> {
        if depth > 8 {
            return Err(Fault::NeedJs);
        }
        let fo = match &f {
            Value::Obj(o) => *o,
            _ => return Err(Fault::NeedJs),
        };
        let (idx, data) = match &self.heap.get(fo).kind {
            Kind::Native(idx, data) => (*idx, data.clone()),
            Kind::Bound(b) => {
                let (t, bt, mut ba) = (b.target, b.this.clone(), b.args.clone());
                ba.extend(args);
                return self.call_native_direct(Value::Obj(t), bt, ba, depth + 1);
            },
            _ => return Err(Fault::NeedJs),
        };
        let func = self.natives[idx as usize].1;
        match func(self, &this, &args, &data, false)? {
            NOut::Val(x) => Ok(x),
            NOut::TailCall(f2, t2, a2) => self.call_native_direct(f2, t2, a2, depth + 1),
            NOut::TailNew(..) | NOut::GenResume(..) | NOut::RunScript(_) | NOut::TailConstruct(..) => Err(Fault::NeedJs),
        }
    }

    /// a method found by a plain walk (data properties only; NeedJs for a getter or a host object)
    fn lookup_method(&mut self, o: ObjRef, a: Atom) -> NResult<Option<Value>> {
        let mut cur = Some(o);
        while let Some(c) = cur {
            let obj = self.heap.get(c);
            if let Kind::Host(..) = obj.kind {
                // host objects: their methods come from engine prototypes; a host property with
                // this name is not asked for here
            }
            if let Some(s) = obj.props.get(a) {
                return match s {
                    Slot::Data(v, _) => Ok(Some(v.clone())),
                    Slot::Accessor(..) => Err(Fault::NeedJs),
                };
            }
            cur = obj.proto;
        }
        Ok(None)
    }

    /// a closure (user code) as the object's method?
    fn user_method(&mut self, o: ObjRef, a: Atom) -> Option<ObjRef> {
        match self.lookup_method(o, a) {
            Ok(Some(Value::Obj(f))) => match self.heap.get(f).kind {
                Kind::Closure(_) => Some(f),
                _ => None,
            },
            _ => None,
        }
    }

    pub fn to_number(&mut self, v: &Value) -> NResult<f64> {
        Ok(match v {
            Value::Undefined | Value::Empty => f64::NAN,
            Value::Null => 0.0,
            Value::Bool(b) => *b as u8 as f64,
            Value::Num(n) => *n,
            Value::Str(s) => crate::num::parse_number(s),
            Value::Obj(o) => {
                match self.heap.get(*o).kind {
                    Kind::Symbol(_) => return self.throw_type("Cannot convert a Symbol value to a number"),
                    Kind::BigInt(_) => return self.throw_type("Cannot convert a BigInt value to a number"),
                    _ => {},
                }
                let p = self.to_primitive(v, true)?;
                return self.to_number(&p);
            },
        })
    }

    pub fn to_str(&mut self, v: &Value) -> NResult<JsStr> {
        Ok(match v {
            Value::Undefined | Value::Empty => js("undefined"),
            Value::Null => js("null"),
            Value::Bool(true) => js("true"),
            Value::Bool(false) => js("false"),
            Value::Num(n) => js(&crate::num::to_string_rust(*n)),
            Value::Str(s) => s.clone(),
            Value::Obj(o) => {
                match &self.heap.get(*o).kind {
                    Kind::Symbol(_) => return self.throw_type("Cannot convert a Symbol value to a string"),
                    Kind::BigInt(b) => return Ok(js(&b.to_string_radix(10))),
                    _ => {},
                }
                let p = self.to_primitive(v, false)?;
                return self.to_str(&p);
            },
        })
    }

    pub fn to_object(&mut self, v: &Value) -> NResult<ObjRef> {
        match v {
            Value::Obj(o) => Ok(*o),
            Value::Undefined | Value::Null | Value::Empty => self.throw_type("Cannot convert undefined or null to object"),
            Value::Bool(b) => {
                let c = self.atom("Boolean");
                Ok(self.heap.alloc(Obj::new(Some(self.realm.boolean_proto), Kind::BoolBox(*b), c)))
            },
            Value::Num(n) => {
                let c = self.atom("Number");
                Ok(self.heap.alloc(Obj::new(Some(self.realm.number_proto), Kind::NumBox(*n), c)))
            },
            Value::Str(s) => {
                let c = self.atom("String");
                Ok(self.heap.alloc(Obj::new(Some(self.realm.string_proto), Kind::StrBox(s.clone()), c)))
            },
        }
    }

    pub fn to_key(&mut self, v: &Value) -> NResult<Key> {
        Ok(match v {
            Value::Num(n) => {
                let n = *n;
                if n >= 0.0 && n < 4294967295.0 && n.fract() == 0.0 {
                    Key::Index(n as u32)
                } else {
                    let s = js(&crate::num::to_string_rust(n));
                    Key::Atom(self.atoms.intern(&s))
                }
            },
            Value::Str(s) => self.key_of_str(s),
            other => {
                if let Some(a) = self.sym_of(other) {
                    return Ok(Key::Atom(a));
                }
                let p = self.to_primitive(other, false)?;
                if let Some(a) = self.sym_of(&p) {
                    return Ok(Key::Atom(a));
                }
                let s = self.to_str(&p)?;
                self.key_of_str(&s)
            },
        })
    }

    pub fn key_value(&mut self, key: Key) -> Value {
        match key {
            Key::Index(i) => Value::Str(js(&format!("{}", i))),
            Key::Atom(a) if self.atoms.is_symbol(a) => self.symbol_value(a),
            Key::Atom(a) => Value::Str(self.atoms.name(a).clone()),
        }
    }

    // ── properties ──

    /// an own property: Some(slot) (elements and virtual properties included)
    pub fn get_own(&mut self, o: ObjRef, key: Key) -> Option<Slot> {
        if let Kind::Proxy(p) = &self.heap.get(o).kind {
            return p.target.and_then(|t| self.get_own(t, key));
        }
        let obj = self.heap.get(o);
        match (&obj.kind, key) {
            (Kind::Array(v) | Kind::Arguments(v), Key::Index(i)) => {
                if let Some(x) = v.get(i as usize) {
                    if !matches!(x, Value::Empty) {
                        return Some(Slot::Data(x.clone(), ALL));
                    }
                }
                if v.len() as u64 > i as u64 {
                    return None;
                }
            },
            (Kind::Array(v), Key::Atom(a)) if a == k::length => {
                return Some(Slot::Data(Value::Num(v.len() as f64), WRITABLE));
            },
            (Kind::Arguments(v), Key::Atom(a)) if a == k::length && !obj.props.contains(a) => {
                return Some(Slot::Data(Value::Num(v.len() as f64), HIDDEN));
            },
            (Kind::TypedArray(t), Key::Index(i)) => {
                let t = *t;
                return crate::builtins2::ta_get(self, &t, i as usize).map(|v| Slot::Data(v, ALL));
            },
            (Kind::StrBox(s), Key::Index(i)) => {
                if let Some(c) = s.get(i as usize) {
                    return Some(Slot::Data(Value::Str(Rc::from(vec![*c])), ENUMERABLE));
                }
            },
            (Kind::StrBox(s), Key::Atom(a)) if a == k::length => {
                return Some(Slot::Data(Value::Num(s.len() as f64), 0));
            },
            (Kind::Closure(c), Key::Atom(a)) if !obj.props.contains(a) => {
                if a == k::name {
                    return Some(Slot::Data(Value::Str(c.func.name.clone()), CONFIGURABLE));
                }
                if a == k::length {
                    return Some(Slot::Data(Value::Num(c.func.length as f64), CONFIGURABLE));
                }
                if a == k::prototype && c.func.generator {
                    let gp = self.proto(if c.func.is_async { "AsyncGenerator" } else { "Generator" });
                    let p = self.new_object_proto(Some(gp));
                    self.heap.get_mut(o).props.insert(k::prototype, Slot::Data(Value::Obj(p), WRITABLE));
                    return Some(Slot::Data(Value::Obj(p), WRITABLE));
                }
                if a == k::prototype && !c.func.arrow && !c.func.method && !c.func.is_async {
                    // made the first time it is asked for
                    let p = self.new_object();
                    self.define_atom(p, k::constructor, Value::Obj(o), HIDDEN);
                    self.heap.get_mut(o).props.insert(k::prototype, Slot::Data(Value::Obj(p), WRITABLE));
                    return Some(Slot::Data(Value::Obj(p), WRITABLE));
                }
            },
            (Kind::Bound(b), Key::Atom(a)) if !obj.props.contains(a) && (a == k::name || a == k::length) => {
                let t = b.target;
                let nargs = b.args.len();
                if a == k::name {
                    let n = match self.get_own(t, Key::Atom(k::name)) {
                        Some(Slot::Data(Value::Str(s), _)) => s,
                        _ => js(""),
                    };
                    return Some(Slot::Data(Value::Str(strs::concat(&js("bound "), &n)), CONFIGURABLE));
                }
                let l = match self.get_own(t, Key::Atom(k::length)) {
                    Some(Slot::Data(Value::Num(n), _)) => (n - nargs as f64).max(0.0),
                    _ => 0.0,
                };
                return Some(Slot::Data(Value::Num(l), CONFIGURABLE));
            },
            _ => {},
        }
        let a = self.key_atom(key);
        self.heap.get(o).props.get(a).cloned()
    }

    /// Ok(Ok(value)), or Ok(Err(getter)) when a getter must run, or a host request
    fn get_from(&mut self, start: ObjRef, key: Key) -> Result<Lookup, Fault> {
        let mut cur = Some(start);
        let mut first = true;
        while let Some(o) = cur {
            if let Some((t, h)) = self.proxy_parts(o)? {
                if let Some(f) = self.proxy_trap(h, "get")? {
                    let kv = self.key_value(key);
                    return Ok(Lookup::Call(f, Value::Obj(h), vec![Value::Obj(t), kv, Value::Obj(start)]));
                }
                cur = Some(t);
                first = false;
                continue;
            }
            if let Kind::Host(c, id) = self.heap.get(o).kind {
                if first && !matches!(key, Key::Atom(a) if self.atoms.is_symbol(a)) {
                    match self.answer.take() {
                        Some(Value::Empty) => {},
                        Some(v) => return Ok(Lookup::Val(v)),
                        None => {
                            let class = self.atoms.name(c).clone();
                            let k2 = self.key_name(key);
                            return Ok(Lookup::Host(HostReq::Get { obj: o, class, id, key: k2 }));
                        },
                    }
                }
            }
            first = false;
            if let Some(s) = self.get_own(o, key) {
                return Ok(match s {
                    Slot::Data(v, _) => Lookup::Val(v),
                    Slot::Accessor(Some(g), _, _) => Lookup::Getter(g),
                    Slot::Accessor(None, _, _) => Lookup::Val(Value::Undefined),
                });
            }
            cur = self.heap.get(o).proto;
        }
        Ok(Lookup::Val(Value::Undefined))
    }

    fn proto_of_primitive(&self, v: &Value) -> Option<ObjRef> {
        match v {
            Value::Str(_) => Some(self.realm.string_proto),
            Value::Num(_) => Some(self.realm.number_proto),
            Value::Bool(_) => Some(self.realm.boolean_proto),
            _ => None,
        }
    }

    fn get_value(&mut self, base: &Value, key: Key) -> Result<Lookup, Fault> {
        match base {
            Value::Obj(o) => self.get_from(*o, key),
            Value::Str(s) => {
                match key {
                    Key::Index(i) => {
                        if let Some(c) = s.get(i as usize) {
                            return Ok(Lookup::Val(Value::Str(Rc::from(vec![*c]))));
                        }
                    },
                    Key::Atom(a) if a == k::length => return Ok(Lookup::Val(Value::Num(s.len() as f64))),
                    _ => {},
                }
                let p = self.realm.string_proto;
                self.get_from(p, key)
            },
            Value::Undefined | Value::Null | Value::Empty => {
                let kn = self.key_name(key);
                let what = if base.is_undefined() || matches!(base, Value::Empty) { "undefined" } else { "null" };
                self.throw_type(&format!("Cannot read properties of {} (reading '{}')", what, strs::to_rust(&kn)))
            },
            other => {
                let p = self.proto_of_primitive(other).unwrap();
                self.get_from(p, key)
            },
        }
    }

    /// a plain read for builtins: NeedJs for getters and host objects
    pub fn get(&mut self, base: &Value, key: Key) -> NResult<Value> {
        if let Value::Obj(o) = base {
            if let Kind::Host(..) = self.heap.get(*o).kind {
                return Err(Fault::NeedJs);
            }
        }
        match self.get_value(base, key)? {
            Lookup::Val(v) => Ok(v),
            _ => Err(Fault::NeedJs),
        }
    }

    pub fn get_str(&mut self, base: &Value, name: &str) -> NResult<Value> {
        let a = self.atom(name);
        let key = self.key_of_atom(a);
        self.get(base, key)
    }

    /// set: Ok(None) done, Ok(Some(setter)) when a setter must run, or a host request
    fn set_value(&mut self, base: &Value, key: Key, v: Value, strict: bool) -> Result<SetOut, Fault> {
        let o = match base {
            Value::Obj(o) => *o,
            Value::Undefined | Value::Null | Value::Empty => {
                let kn = self.key_name(key);
                return self.throw_type(&format!("Cannot set properties of {} (setting '{}')", if base.is_undefined() { "undefined" } else { "null" }, strs::to_rust(&kn)));
            },
            _ => {
                // a primitive: setters on its prototype run; otherwise nothing happens
                let p = self.proto_of_primitive(base).unwrap();
                if let Some(Slot::Accessor(_, Some(s), _)) = self.find_slot(p, key) {
                    return Ok(SetOut::Setter(s));
                }
                if strict {
                    return self.throw_type("Cannot create a property on a primitive");
                }
                return Ok(SetOut::Done);
            },
        };
        if let Some((t, h)) = self.proxy_parts(o)? {
            if let Some(f) = self.proxy_trap(h, "set")? {
                let kv = self.key_value(key);
                return Ok(SetOut::Call(f, Value::Obj(h), vec![Value::Obj(t), kv, v, base.clone()]));
            }
            return self.set_value(&Value::Obj(t), key, v, strict);
        }
        if let Kind::Host(c, id) = self.heap.get(o).kind {
            match self.answer.take() {
                Some(Value::Bool(true)) => return Ok(SetOut::Done),
                Some(_) => {},
                None => {
                    let class = self.atoms.name(c).clone();
                    let k2 = self.key_name(key);
                    return Ok(SetOut::Host(HostReq::Set { obj: o, class, id, key: k2, value: v }));
                },
            }
        }
        if let (Kind::TypedArray(t), Key::Index(i)) = (&self.heap.get(o).kind, key) {
            let t = *t;
            crate::builtins2::ta_set(self, &t, i as usize, &v)?;
            return Ok(SetOut::Done);
        }
        // arrays: elements and length
        if let Kind::Array(_) | Kind::Arguments(_) = self.heap.get(o).kind {
            match key {
                Key::Index(i) => {
                    let i = i as usize;
                    let len = self.heap.get(o).elements().unwrap().len();
                    if i < len || (i < len + 1_000_000 && self.heap.get(o).extensible && self.find_setter_in_protos(o, key).is_none()) {
                        let el = self.heap.get_mut(o).elements_mut().unwrap();
                        if i >= el.len() {
                            el.resize(i + 1, Value::Empty);
                        }
                        el[i] = v;
                        return Ok(SetOut::Done);
                    }
                },
                Key::Atom(a) if a == k::length && matches!(self.heap.get(o).kind, Kind::Array(_)) => {
                    let n = self.to_number(&v)?;
                    if n < 0.0 || n.fract() != 0.0 || n > 4294967295.0 {
                        return Err(Fault::Throw(self.range_error("Invalid array length")));
                    }
                    let el = self.heap.get_mut(o).elements_mut().unwrap();
                    el.resize(n as usize, Value::Empty);
                    return Ok(SetOut::Done);
                },
                _ => {},
            }
        }
        if let Kind::StrBox(_) = self.heap.get(o).kind {
            if matches!(key, Key::Index(_)) || matches!(key, Key::Atom(a) if a == k::length) {
                if strict {
                    return self.throw_type("Cannot assign to read only property of a string");
                }
                return Ok(SetOut::Done);
            }
        }
        let a = self.key_atom(key);
        // an own data property
        if let Some(slot) = self.heap.get_mut(o).props.get_mut(a) {
            match slot {
                Slot::Data(old, f) => {
                    if *f & WRITABLE != 0 {
                        *old = v;
                        return Ok(SetOut::Done);
                    }
                    if strict {
                        let n = strs::to_rust(self.atoms.name(a));
                        return self.throw_type(&format!("Cannot assign to read only property '{}'", n));
                    }
                    return Ok(SetOut::Done);
                },
                Slot::Accessor(_, Some(s), _) => return Ok(SetOut::Setter(*s)),
                Slot::Accessor(_, None, _) => {
                    if strict {
                        return self.throw_type("Cannot set a property that has only a getter");
                    }
                    return Ok(SetOut::Done);
                },
            }
        }
        // virtual own properties (function name / length / prototype) become real
        if let Kind::Closure(_) | Kind::Bound(_) = self.heap.get(o).kind {
            if a == k::prototype {
                if let Some(Slot::Data(_, f)) = self.get_own(o, key) {
                    if f & WRITABLE != 0 {
                        self.heap.get_mut(o).props.insert(a, Slot::Data(v, f));
                    }
                    return Ok(SetOut::Done);
                }
            }
            if a == k::name || a == k::length {
                if strict {
                    return self.throw_type("Cannot assign to read only property of a function");
                }
                return Ok(SetOut::Done);
            }
        }
        // inherited: a setter, or a read-only property, stops it
        let proto = self.heap.get(o).proto;
        if let Some(p) = proto {
            match self.find_slot(p, key) {
                Some(Slot::Accessor(_, Some(s), _)) => return Ok(SetOut::Setter(s)),
                Some(Slot::Accessor(_, None, _)) => {
                    if strict {
                        return self.throw_type("Cannot set a property that has only a getter");
                    }
                    return Ok(SetOut::Done);
                },
                Some(Slot::Data(_, f)) if f & WRITABLE == 0 => {
                    if strict {
                        let n = strs::to_rust(self.atoms.name(a));
                        return self.throw_type(&format!("Cannot assign to read only property '{}'", n));
                    }
                    return Ok(SetOut::Done);
                },
                _ => {},
            }
        }
        if !self.heap.get(o).extensible {
            if strict {
                return self.throw_type("Cannot add a property, the object is not extensible");
            }
            return Ok(SetOut::Done);
        }
        self.heap.get_mut(o).props.insert(a, Slot::Data(v, ALL));
        Ok(SetOut::Done)
    }

    fn find_setter_in_protos(&mut self, o: ObjRef, key: Key) -> Option<ObjRef> {
        let p = self.heap.get(o).proto?;
        match self.find_slot(p, key) {
            Some(Slot::Accessor(_, Some(s), _)) => Some(s),
            _ => None,
        }
    }

    /// the slot for a key on an object or its prototypes
    pub fn find_slot(&mut self, start: ObjRef, key: Key) -> Option<Slot> {
        let mut cur = Some(start);
        while let Some(o) = cur {
            if let Some(s) = self.get_own(o, key) {
                return Some(s);
            }
            cur = self.heap.get(o).proto;
        }
        None
    }

    /// a plain write for builtins: NeedJs for setters and host objects
    pub fn put(&mut self, base: &Value, key: Key, v: Value) -> NResult<()> {
        if let Value::Obj(o) = base {
            if let Kind::Host(..) = self.heap.get(*o).kind {
                return Err(Fault::NeedJs);
            }
        }
        match self.set_value(base, key, v, true)? {
            SetOut::Done => Ok(()),
            _ => Err(Fault::NeedJs),
        }
    }

    pub fn put_str(&mut self, base: &Value, name: &str, v: Value) -> NResult<()> {
        let a = self.atom(name);
        let key = self.key_of_atom(a);
        self.put(base, key, v)
    }

    pub fn has_property(&mut self, o: ObjRef, key: Key) -> bool {
        self.find_slot(o, key).is_some()
    }

    pub fn delete(&mut self, o: ObjRef, key: Key, strict: bool) -> NResult<bool> {
        if let Kind::Proxy(p) = &self.heap.get(o).kind {
            return match p.target {
                Some(t) => self.delete(t, key, strict),
                None => self.throw_type("Cannot perform 'deleteProperty' on a proxy that has been revoked"),
            };
        }
        if let Kind::Array(_) | Kind::Arguments(_) = self.heap.get(o).kind {
            if let Key::Index(i) = key {
                let el = self.heap.get_mut(o).elements_mut().unwrap();
                let i = i as usize;
                if i < el.len() {
                    if i + 1 == el.len() && false {
                        el.pop();
                    } else {
                        el[i] = Value::Empty;
                    }
                }
                return Ok(true);
            }
            if let Key::Atom(a) = key {
                if a == k::length && matches!(self.heap.get(o).kind, Kind::Array(_)) {
                    return Ok(false);
                }
            }
        }
        let a = self.key_atom(key);
        let flags = self.heap.get(o).props.get(a).map(|s| s.flags());
        match flags {
            Some(f) if f & CONFIGURABLE == 0 => {
                if strict {
                    let n = strs::to_rust(self.atoms.name(a));
                    return self.throw_type(&format!("Cannot delete property '{}'", n));
                }
                Ok(false)
            },
            Some(_) => {
                self.heap.get_mut(o).props.remove(a);
                Ok(true)
            },
            None => Ok(true),
        }
    }

    /// own enumerable string keys in order: integer keys ascending, then the rest as added
    pub fn own_keys(&mut self, o: ObjRef, only_enumerable: bool) -> Vec<Value> {
        if let Kind::Proxy(p) = &self.heap.get(o).kind {
            return match p.target {
                Some(t) => self.own_keys(t, only_enumerable),
                None => vec![],
            };
        }
        let mut ints: Vec<u32> = Vec::new();
        let mut names: Vec<Value> = Vec::new();
        let obj = self.heap.get(o);
        match &obj.kind {
            Kind::Array(v) | Kind::Arguments(v) => {
                for (i, x) in v.iter().enumerate() {
                    if !matches!(x, Value::Empty) {
                        ints.push(i as u32);
                    }
                }
            },
            Kind::TypedArray(t) => {
                for i in 0..t.len {
                    ints.push(i as u32);
                }
            },
            Kind::StrBox(s) => {
                for i in 0..s.len() {
                    ints.push(i as u32);
                }
            },
            _ => {},
        }
        let obj = self.heap.get(o);
        let mut extra_ints = Vec::new();
        for (key, slot) in obj.props.keys.iter().zip(obj.props.slots.iter()) {
            if only_enumerable && slot.flags() & ENUMERABLE == 0 {
                continue;
            }
            if self.atoms.is_symbol(*key) {
                continue;
            }
            let n = self.atoms.name(*key);
            match strs::array_index(n) {
                Some(i) => extra_ints.push(i),
                None => names.push(Value::Str(n.clone())),
            }
        }
        if !only_enumerable {
            if let Kind::Array(_) = self.heap.get(o).kind {
                names.insert(0, Value::str("length"));
            }
        }
        ints.extend(extra_ints);
        ints.sort_unstable();
        ints.dedup();
        let mut out: Vec<Value> = ints.into_iter().map(|i| Value::Str(js(&format!("{}", i)))).collect();
        out.extend(names);
        out
    }

    /// own symbol keys in order (private names left out)
    pub fn own_symbol_keys(&mut self, o: ObjRef, only_enumerable: bool) -> Vec<Value> {
        if let Kind::Proxy(p) = &self.heap.get(o).kind {
            return match p.target {
                Some(t) => self.own_symbol_keys(t, only_enumerable),
                None => vec![],
            };
        }
        let mut atoms = Vec::new();
        let obj = self.heap.get(o);
        for (key, slot) in obj.props.keys.iter().zip(obj.props.slots.iter()) {
            if only_enumerable && slot.flags() & ENUMERABLE == 0 {
                continue;
            }
            if self.atoms.is_symbol(*key) && !self.atoms.is_private(*key) {
                atoms.push(*key);
            }
        }
        atoms.into_iter().map(|a| self.symbol_value(a)).collect()
    }

    pub fn define_own(&mut self, o: ObjRef, key: Key, slot: Slot) {
        if let Kind::Proxy(p) = &self.heap.get(o).kind {
            if let Some(t) = p.target {
                return self.define_own(t, key, slot);
            }
            return;
        }
        if let Kind::Array(_) | Kind::Arguments(_) = self.heap.get(o).kind {
            if let (Key::Index(i), Slot::Data(v, ALL)) = (key, &slot) {
                let el = self.heap.get_mut(o).elements_mut().unwrap();
                let i = i as usize;
                if i < el.len() + 1_000_000 {
                    if i >= el.len() {
                        el.resize(i + 1, Value::Empty);
                    }
                    el[i] = v.clone();
                    return;
                }
            }
            if let Key::Index(i) = key {
                // a non-default element: keep it as a property; the dense slot becomes a hole
                let el = self.heap.get_mut(o).elements_mut().unwrap();
                if (i as usize) < el.len() {
                    el[i as usize] = Value::Empty;
                }
            }
        }
        let a = self.key_atom(key);
        self.heap.get_mut(o).props.insert(a, slot);
    }

    pub fn instance_of(&mut self, v: &Value, ctor: &Value) -> NResult<bool> {
        let c = match ctor {
            Value::Obj(c) if self.heap.get(*c).is_callable() => *c,
            _ => return self.throw_type("Right-hand side of 'instanceof' is not callable"),
        };
        if let Ok(Some(Value::Obj(h))) = self.lookup_method(c, crate::atoms::sym::hasInstance) {
            if matches!(self.heap.get(h).kind, Kind::Closure(_)) {
                return Err(Fault::NeedJs);
            }
        }
        if let Kind::Bound(b) = &self.heap.get(c).kind {
            let t = Value::Obj(b.target);
            return self.instance_of(v, &t);
        }
        let o = match v {
            Value::Obj(o) => *o,
            _ => return Ok(false),
        };
        if self.is_heap_prim(v) {
            return Ok(false);
        }
        let proto = match self.get(&Value::Obj(c), Key::Atom(k::prototype))? {
            Value::Obj(p) => p,
            _ => return self.throw_type("Function has non-object prototype in instanceof check"),
        };
        let mut cur = self.heap.get(o).proto;
        while let Some(p) = cur {
            if p == proto {
                return Ok(true);
            }
            cur = self.heap.get(p).proto;
        }
        Ok(false)
    }

    // ── calls ──

    /// start a call. Ok(Some(v)) when it finished at once (a native), Ok(None) when a frame
    /// was pushed (or the host was asked); the result goes to `cont`.
    pub(crate) fn call_value(&mut self, f: Value, this: Value, args: Vec<Value>, cont: Cont, _construct: bool) -> Result<Option<Value>, Value> {
        let fo = match &f {
            Value::Obj(o) if self.heap.get(*o).is_callable() => *o,
            _ => {
                let what = self.call_site_name().unwrap_or_else(|| self.describe(&f));
                return Err(self.type_error(&format!("{} is not a function", what)));
            },
        };
        match &self.heap.get(fo).kind {
            Kind::Closure(c) => {
                if matches!(c.func.kind, crate::ast::FuncKind::ClassConstructor | crate::ast::FuncKind::DerivedConstructor) {
                    return Err(self.type_error("Class constructor cannot be invoked without 'new'"));
                }
                let base = self.stack.len();
                let n = args.len();
                for a in args {
                    self.stack.push(a);
                }
                self.enter_closure(fo, this, base, n, base, cont, Value::Undefined)?;
                Ok(None)
            },
            Kind::Native(idx, data) => {
                let (idx, data) = (*idx, data.clone());
                self.run_native(idx, data, this, args, cont, false)
            },
            Kind::Bound(b) => {
                let (t, bt, mut ba) = (b.target, b.this.clone(), b.args.clone());
                ba.extend(args);
                self.call_value(Value::Obj(t), bt, ba, cont, false)
            },
            Kind::Proxy(_) => {
                let Some((t, h)) = self.proxy_parts(fo).map_err(|f| match f {
                    Fault::Throw(e) => e,
                    Fault::NeedJs => Value::Undefined,
                })?
                else {
                    unreachable!()
                };
                match self.proxy_trap(h, "apply") {
                    Ok(Some(trap)) => {
                        let arr = self.new_array(args);
                        self.call_value(trap, Value::Obj(h), vec![Value::Obj(t), this, Value::Obj(arr)], cont, false)
                    },
                    Ok(None) => self.call_value(Value::Obj(t), this, args, cont, false),
                    Err(Fault::Throw(e)) => Err(e),
                    Err(Fault::NeedJs) => Err(self.type_error("proxy apply trap behind a getter")),
                }
            },
            Kind::HostFn(_) => Err(self.type_error("internal: host functions are called through the VM")),
            _ => unreachable!(),
        }
    }

    fn run_native(&mut self, idx: u16, data: Value, this: Value, args: Vec<Value>, cont: Cont, construct: bool) -> Result<Option<Value>, Value> {
        let func = self.natives[idx as usize].1;
        match func(self, &this, &args, &data, construct) {
            Ok(NOut::Val(v)) => self.deliver_now(v, cont),
            Ok(NOut::TailCall(f, t, a)) => self.call_value(f, t, a, cont, false),
            Ok(NOut::TailNew(f, a)) => self.construct_value(f, a, cont),
            Ok(NOut::GenResume(g, mode, v)) => self.gen_resume(g, mode, v, cont),
            Ok(NOut::TailConstruct(f, a, nt)) => self.construct_nt(f, a, nt, cont),
            Ok(NOut::RunScript(p)) => {
                if self.frames.len() >= self.max_frames {
                    return Err(self.range_error("Maximum call stack size exceeded"));
                }
                self.push_script(p, cont);
                Ok(None)
            },
            Err(Fault::Throw(e)) => Err(e),
            Err(Fault::NeedJs) => match self.slow.get(&idx).cloned() {
                Some(slow) => {
                    // the slow version takes (this, ...args)
                    let mut a2 = vec![this];
                    a2.extend(args);
                    self.call_value(slow, Value::Undefined, a2, cont, false)
                },
                None => {
                    let has_obj = args.iter().any(|a| matches!(a, Value::Obj(_)));
                    let retry = self.realm.intrinsics.get(crate::compiler::intr::RETRY_PRIM as usize).cloned();
                    match retry {
                        Some(r) if has_obj && !matches!(r, Value::Undefined) => {
                            let fnv = Value::Obj(self.native_fn_obj(idx));
                            let arr = self.new_array(args);
                            self.call_value(r, Value::Undefined, vec![fnv, this, Value::Obj(arr)], cont, false)
                        },
                        _ => {
                            let name = self.natives[idx as usize].0;
                            Err(self.type_error(&format!("{} cannot be used with this kind of object yet", name)))
                        },
                    }
                },
            },
        }
    }

    /// the function object of a native (made once per native)
    fn native_fn_obj(&mut self, idx: u16) -> ObjRef {
        if let Some(o) = self.native_objs.get(&idx) {
            return *o;
        }
        let name = self.natives[idx as usize].0;
        let o = self.new_native(name, idx, 0, Value::Undefined);
        self.native_objs.insert(idx, o);
        o
    }

    /// a value for a continuation, outside the run loop (natives): Some(v) when it goes back
    /// to the caller as is
    fn deliver_now(&mut self, v: Value, cont: Cont) -> Result<Option<Value>, Value> {
        let (v, cont) = self.unwrap_fix(v, cont);
        match cont {
            Cont::Host => Ok(Some(v)),
            other => {
                self.deliver(v, other)?;
                Ok(None)
            },
        }
    }

    pub(crate) fn construct_value(&mut self, f: Value, args: Vec<Value>, cont: Cont) -> Result<Option<Value>, Value> {
        let nt = f.clone();
        self.construct_nt(f, args, nt, cont)
    }

    /// `new` with a new.target (`super(...)` passes the derived class)
    pub(crate) fn construct_nt(&mut self, f: Value, args: Vec<Value>, nt: Value, cont: Cont) -> Result<Option<Value>, Value> {
        if !self.is_constructor(&f) {
            let what = self.call_site_name().unwrap_or_else(|| self.describe(&f));
            return Err(self.type_error(&format!("{} is not a constructor", what)));
        }
        let fo = f.as_obj().unwrap();
        match &self.heap.get(fo).kind {
            Kind::Closure(c) => {
                let derived = c.func.kind == crate::ast::FuncKind::DerivedConstructor;
                let base = self.stack.len();
                let n = args.len();
                if derived {
                    for a in args {
                        self.stack.push(a);
                    }
                    let c = match cont {
                        Cont::HostConstruct(_) => Cont::Host,
                        c => c,
                    };
                    self.enter_closure(fo, Value::Empty, base, n, base, c, nt)?;
                    return Ok(None);
                }
                let proto = match self.get(&nt, Key::Atom(k::prototype)) {
                    Ok(Value::Obj(p)) => p,
                    _ => self.realm.object_proto,
                };
                let this = self.new_object_proto(Some(proto));
                for a in args {
                    self.stack.push(a);
                }
                let c = match cont {
                    Cont::Host => Cont::HostConstruct(this),
                    Cont::Push => Cont::Construct(this),
                    other => Cont::FixProto(None, Some(this), Box::new(other)),
                };
                self.enter_closure(fo, Value::Obj(this), base, n, base, c, nt)?;
                Ok(None)
            },
            Kind::Native(idx, data) => {
                let (idx, data) = (*idx, data.clone());
                let cont = if !nt.strict_eq(&f) {
                    // a subclass of a builtin: the builtin's object gets the subclass's prototype
                    match self.get(&nt, Key::Atom(k::prototype)) {
                        Ok(Value::Obj(p)) => Cont::FixProto(Some(p), None, Box::new(cont)),
                        _ => cont,
                    }
                } else {
                    cont
                };
                self.run_native(idx, data, Value::Undefined, args, cont, true)
            },
            Kind::Bound(b) => {
                let (t, mut ba) = (b.target, b.args.clone());
                ba.extend(args);
                let nt = if nt.strict_eq(&f) { Value::Obj(t) } else { nt };
                self.construct_nt(Value::Obj(t), ba, nt, cont)
            },
            Kind::Proxy(_) => self.proxy_construct(fo, args, nt, cont),
            _ => Err(self.type_error("not a constructor")),
        }
    }

    /// push a frame for a closure whose `n` arguments sit on the stack from `base`
    fn enter_closure(&mut self, fo: ObjRef, this: Value, base: usize, n: usize, ret_sp: usize, cont: Cont, new_target: Value) -> Result<(), Value> {
        if self.frames.len() >= self.max_frames {
            self.stack.truncate(ret_sp);
            return Err(self.range_error("Maximum call stack size exceeded"));
        }
        let (func, env, this_val, arrow_nt) = match &self.heap.get(fo).kind {
            Kind::Closure(c) => (c.func.clone(), c.env, c.this_val.clone(), c.fields),
            _ => unreachable!(),
        };
        let new_target = if func.arrow { arrow_nt.map(Value::Obj).unwrap_or(Value::Undefined) } else { new_target };
        let this = if func.arrow {
            this_val.unwrap_or(Value::Undefined)
        } else if !func.strict && this.is_nullish() {
            Value::Obj(self.realm.global)
        } else if !func.strict && !matches!(this, Value::Obj(_)) {
            match self.to_object(&this) {
                Ok(o) => Value::Obj(o),
                Err(_) => this,
            }
        } else {
            this
        };
        let args = if func.keep_args { Some(self.stack[base..base + n].to_vec()) } else { None };
        let np = func.nparams as usize;
        if n > np {
            self.stack.truncate(base + np);
        } else {
            for _ in n..np {
                self.stack.push(Value::Undefined);
            }
        }
        for _ in np..func.nlocals as usize {
            self.stack.push(Value::Undefined);
        }
        self.frames.push(Frame { func, callee: Some(fo), ip: 0, base, ret_sp, this, env, handlers: Vec::new(), args, new_target, cont, gen: None });
        Ok(())
    }

    /// hand a finished call's value to its continuation (inside the run loop)
    fn unwrap_fix(&mut self, mut v: Value, mut cont: Cont) -> (Value, Cont) {
        while let Cont::FixProto(p, d, inner) = cont {
            if let (Some(d), false) = (d, matches!(v, Value::Obj(_))) {
                v = Value::Obj(d);
            }
            if let (Some(p), Value::Obj(o)) = (p, &v) {
                self.heap.get_mut(*o).proto = Some(p);
            }
            cont = *inner;
        }
        (v, cont)
    }

    fn deliver(&mut self, v: Value, cont: Cont) -> Result<(), Value> {
        let (v, cont) = self.unwrap_fix(v, cont);
        match cont {
            Cont::Push => self.stack.push(v),
            Cont::Discard => {},
            Cont::Construct(o) => {
                if let Value::Obj(_) = v {
                    self.stack.push(v);
                } else {
                    self.stack.push(Value::Obj(o));
                }
            },
            Cont::Prim { slot, obj, number_first, stage } => {
                if v.as_obj().is_none() {
                    self.stack[slot] = v;
                } else if stage == 0 {
                    let m = if number_first { k::toString } else { k::valueOf };
                    let f = match self.lookup_method(obj, m) {
                        Ok(Some(f)) if self.is_callable(&f) => f,
                        _ => return Err(self.type_error("Cannot convert object to primitive value")),
                    };
                    let cont = Cont::Prim { slot, obj, number_first, stage: 1 };
                    if let Some(x) = self.call_value(f, Value::Obj(obj), vec![], cont.clone(), false)? {
                        self.deliver(x, cont)?;
                    }
                } else {
                    return Err(self.type_error("Cannot convert object to primitive value"));
                }
            },
            Cont::Host | Cont::HostConstruct(_) | Cont::FixProto(..) => self.stack.push(v),
        }
        Ok(())
    }

    /// ToPrimitive on stack[slot] inside the run loop: Ok(true) when it is now primitive,
    /// Ok(false) when a method frame was started (the instruction re-runs when it returns)
    fn prim_slot(&mut self, slot: usize, number_first: bool) -> Result<bool, Value> {
        let v = self.stack[slot].clone();
        let o = match v {
            Value::Obj(o) => o,
            _ => return Ok(true),
        };
        match self.to_primitive(&v, number_first) {
            Ok(p) => {
                self.stack[slot] = p;
                Ok(true)
            },
            Err(Fault::Throw(e)) => Err(e),
            Err(Fault::NeedJs) => {
                if let Ok(Some(f)) = self.lookup_method(o, crate::atoms::sym::toPrimitive) {
                    if self.is_callable(&f) {
                        let hint = Value::str(if number_first { "default" } else { "string" });
                        let cont = Cont::Prim { slot, obj: o, number_first, stage: 2 };
                        self.call_value(f, Value::Obj(o), vec![hint], cont, false)?;
                        return Ok(false);
                    }
                }
                let order = if number_first { [k::valueOf, k::toString] } else { [k::toString, k::valueOf] };
                for (stage, m) in order.iter().enumerate() {
                    let f = match self.lookup_method(o, *m) {
                        Ok(Some(f)) if self.is_callable(&f) => f,
                        Ok(_) => continue,
                        Err(_) => return Err(self.type_error("Cannot convert object to primitive value (a getter)")),
                    };
                    let cont = Cont::Prim { slot, obj: o, number_first, stage: stage as u8 };
                    // a native finishes at once (the slot is filled); a closure pushes a frame.
                    // Either way the instruction runs again.
                    self.call_value(f, Value::Obj(o), vec![], cont, false)?;
                    return Ok(false);
                }
                Err(self.type_error("Cannot convert object to primitive value"))
            },
        }
    }

    /// the call site the running frame is at ("a.b.c"), when the compiler noted one
    fn call_site_name(&self) -> Option<String> {
        let f = self.frames.last()?;
        let ip = f.ip.saturating_sub(1) as u32;
        f.func.call_names.iter().find(|(at, _)| *at == ip).map(|(_, n)| strs::to_rust(n))
    }

    pub fn describe(&mut self, v: &Value) -> String {
        match v {
            Value::Str(s) => format!("\"{}\"", strs::to_rust(s)),
            Value::Obj(o) => match &self.heap.get(*o).kind {
                Kind::Array(_) => "an array".into(),
                _ => "an object".into(),
            },
            other => match self.to_str(other) {
                Ok(s) => strs::to_rust(&s),
                Err(_) => "value".into(),
            },
        }
    }

    // ── generators ──

    pub fn iter_result(&mut self, v: Value, done: bool) -> Value {
        let o = self.new_object();
        let obj = self.heap.get_mut(o);
        obj.props.insert(k::value, Slot::Data(v, ALL));
        obj.props.insert(k::done, Slot::Data(Value::Bool(done), ALL));
        Value::Obj(o)
    }

    fn gen_done(&mut self, g: ObjRef) {
        if let Kind::Generator(gd) = &mut self.heap.get_mut(g).kind {
            gd.state = GenState::Done;
            gd.frame = None;
        }
    }

    fn gen_flavor(&self, g: ObjRef) -> u8 {
        match &self.heap.get(g).kind {
            Kind::Generator(gd) => gd.flavor,
            _ => 0,
        }
    }

    /// a finished frame's value to its continuation: Some(status) when it goes to the embedder
    fn finish(&mut self, v: Value, cont: Cont) -> Result<Option<Status>, Value> {
        let (v, cont) = self.unwrap_fix(v, cont);
        match cont {
            Cont::Host => Ok(Some(Status::Done(v))),
            Cont::HostConstruct(o) => Ok(Some(Status::Done(if let Value::Obj(_) = v { v } else { Value::Obj(o) }))),
            c => {
                self.deliver(v, c)?;
                Ok(None)
            },
        }
    }

    /// take the running (top) frame off the stack into a saved frame; returns it and its continuation
    fn save_top_frame(&mut self) -> (SavedFrame, Cont) {
        let f = self.frames.pop().unwrap();
        let stack = self.stack.split_off(f.base);
        self.stack.truncate(f.ret_sp);
        let base = f.base;
        let handlers = f
            .handlers
            .into_iter()
            .map(|mut h| {
                h.sp -= base;
                h
            })
            .collect();
        let saved = SavedFrame { func: f.func, callee: f.callee, ip: f.ip, this: f.this, env: f.env, handlers, args: f.args, new_target: f.new_target, stack };
        (saved, f.cont)
    }

    /// next() / throw() / return() on a generator (mode 0 / 1 / 2)
    pub(crate) fn gen_resume(&mut self, g: ObjRef, mode: u8, v: Value, cont: Cont) -> Result<Option<Value>, Value> {
        let (state, flavor, kind) = match &self.heap.get(g).kind {
            Kind::Generator(gd) => (gd.state, gd.flavor, gd.yield_kind),
            _ => return Err(self.type_error("not a generator")),
        };
        match state {
            GenState::Running => return Err(self.type_error("Generator is already running")),
            GenState::Done => {
                return match mode {
                    1 => Err(v),
                    _ => {
                        let rv = if mode == 2 { v } else { Value::Undefined };
                        let r = if flavor == 0 { self.iter_result(rv, true) } else { rv };
                        self.deliver_now(r, cont)
                    },
                };
            },
            GenState::Start if mode != 0 => {
                self.gen_done(g);
                if mode == 1 {
                    return Err(v);
                }
                let r = if flavor == 0 { self.iter_result(v, true) } else { v };
                return self.deliver_now(r, cont);
            },
            _ => {},
        }
        if self.frames.len() >= self.max_frames {
            return Err(self.range_error("Maximum call stack size exceeded"));
        }
        let saved = match &mut self.heap.get_mut(g).kind {
            Kind::Generator(gd) => {
                gd.state = GenState::Running;
                gd.frame.take().unwrap()
            },
            _ => unreachable!(),
        };
        let base = self.stack.len();
        self.stack.extend(saved.stack);
        let handlers = saved
            .handlers
            .into_iter()
            .map(|mut h| {
                h.sp += base;
                h
            })
            .collect();
        self.frames.push(Frame { func: saved.func, callee: saved.callee, ip: saved.ip, base, ret_sp: base, this: saved.this, env: saved.env, handlers, args: saved.args, new_target: saved.new_target, cont, gen: Some(g) });
        if state == GenState::Start {
            return Ok(None);
        }
        if kind == 1 {
            self.stack.push(v);
            self.stack.push(Value::Num(mode as f64));
            return Ok(None);
        }
        match mode {
            0 => {
                self.stack.push(v);
                Ok(None)
            },
            1 => Err(v),
            _ => {
                if let Kind::Generator(gd) = &mut self.heap.get_mut(g).kind {
                    gd.ret_val = Some(v);
                }
                Err(Value::Empty)
            },
        }
    }

    // ── exceptions ──

    /// unwind to the nearest handler; Err(v) when it reached the run's boundary uncaught
    fn throw(&mut self, v: Value) -> Result<(), Value> {
        if self.trace_throws && self.log.len() < 2000 {
            let m = self.display(&v);
            let st = match &v {
                Value::Obj(_) => match self.get_str(&v, "stack") {
                    Ok(Value::Str(s)) => strs::to_rust(&s).lines().nth(1).unwrap_or("").to_string(),
                    _ => String::new(),
                },
                _ => String::new(),
            };
            self.log.push(format!("thrown: {} {}", m, st.trim()));
        }
        let mut v = v;
        loop {
            let f = match self.frames.last_mut() {
                Some(f) => f,
                None => return Err(v),
            };
            if let Value::Empty = v {
                // a generator's return(): through its finally blocks only, then a return
                while let Some(h) = f.handlers.pop() {
                    if h.finally {
                        f.ip = h.catch_ip;
                        f.env = h.env;
                        self.stack.truncate(h.sp);
                        self.stack.push(Value::Empty);
                        return Ok(());
                    }
                }
                if let Some(g) = f.gen {
                    f.ip = f.func.code.len() - 1; // every function ends with Return
                    let rv = match &mut self.heap.get_mut(g).kind {
                        Kind::Generator(gd) => gd.ret_val.take().unwrap_or(Value::Undefined),
                        _ => Value::Undefined,
                    };
                    self.stack.push(rv);
                    return Ok(());
                }
                v = Value::Undefined;
                continue;
            }
            if let Some(h) = f.handlers.pop() {
                f.ip = h.catch_ip;
                f.env = h.env;
                self.stack.truncate(h.sp);
                self.stack.push(v);
                return Ok(());
            }
            let f = self.frames.pop().unwrap();
            self.stack.truncate(f.ret_sp);
            if let Some(g) = f.gen {
                self.gen_done(g);
            }
            if matches!(f.cont, Cont::Host | Cont::HostConstruct(_)) {
                return Err(v);
            }
        }
    }

    // ── the loop ──

    fn run(&mut self) -> Status {
        loop {
            match self.step_loop() {
                Ok(Some(st)) => return st,
                Ok(None) => continue,
                Err(e) => match self.throw(e) {
                    Ok(()) => continue,
                    Err(e) => return Status::Error(e),
                },
            }
        }
    }

    /// runs instructions; Ok(Some(status)) to stop, Err(exception) to unwind
    fn step_loop(&mut self) -> Result<Option<Status>, Value> {
        loop {
            if self.heap.since_gc > self.gc_threshold {
                self.gc();
            }
            let fi = self.frames.len() - 1;
            let (op, ip) = {
                let f = &mut self.frames[fi];
                let ip = f.ip;
                f.ip = ip + 1;
                (f.func.code[ip], ip)
            };
            self.steps += 1;
            if self.steps > self.budget {
                // every instruction from here on throws: a catch cannot keep a runaway script going
                self.budget_hit = true;
                return Err(self.range_error("script ran too long (Horizon stopped it)"));
            }
            macro_rules! pop {
                () => {
                    self.stack.pop().unwrap()
                };
            }
            macro_rules! top {
                () => {
                    self.stack.last().unwrap()
                };
            }
            macro_rules! rewind {
                () => {
                    self.frames[fi].ip = ip
                };
            }
            macro_rules! nres {
                ($e:expr) => {
                    match $e {
                        Ok(v) => v,
                        Err(Fault::Throw(e)) => return Err(e),
                        Err(Fault::NeedJs) => return Err(self.type_error("internal: unexpected NeedJs")),
                    }
                };
            }
            match op {
                Op::Const(i) => {
                    let v = self.frames[fi].func.consts[i as usize].clone();
                    self.stack.push(v);
                },
                Op::Int(n) => self.stack.push(Value::Num(n as f64)),
                Op::Undef => self.stack.push(Value::Undefined),
                Op::Null => self.stack.push(Value::Null),
                Op::True => self.stack.push(Value::Bool(true)),
                Op::False => self.stack.push(Value::Bool(false)),
                Op::EmptyVal => self.stack.push(Value::Empty),
                Op::Pop => {
                    self.stack.pop();
                },
                Op::Dup => {
                    let v = top!().clone();
                    self.stack.push(v);
                },
                Op::Dup2 => {
                    let n = self.stack.len();
                    let a = self.stack[n - 2].clone();
                    let b = self.stack[n - 1].clone();
                    self.stack.push(a);
                    self.stack.push(b);
                },
                Op::Swap => {
                    let n = self.stack.len();
                    self.stack.swap(n - 1, n - 2);
                },
                Op::Rot3 => {
                    let c = pop!();
                    let n = self.stack.len();
                    self.stack.insert(n - 2, c);
                },
                Op::GetLocal(s) => {
                    let v = self.stack[self.frames[fi].base + s as usize].clone();
                    self.stack.push(v);
                },
                Op::SetLocal(s) => {
                    let v = top!().clone();
                    let b = self.frames[fi].base;
                    self.stack[b + s as usize] = v;
                },
                Op::GetLocalChk(s, a) => {
                    let v = self.stack[self.frames[fi].base + s as usize].clone();
                    if let Value::Empty = v {
                        let n = strs::to_rust(self.atoms.name(a));
                        return Err(self.reference_error(&format!("Cannot access '{}' before initialization", n)));
                    }
                    self.stack.push(v);
                },
                Op::SetLocalChk(s, a) => {
                    let b = self.frames[fi].base;
                    if let Value::Empty = self.stack[b + s as usize] {
                        let n = strs::to_rust(self.atoms.name(a));
                        return Err(self.reference_error(&format!("Cannot access '{}' before initialization", n)));
                    }
                    let v = top!().clone();
                    self.stack[b + s as usize] = v;
                },
                Op::ClearLocal(s) => {
                    let b = self.frames[fi].base;
                    self.stack[b + s as usize] = Value::Empty;
                },
                Op::GetEnv(d, s) => {
                    let e = self.env_at(fi, d);
                    let v = match &self.heap.get(e).kind {
                        Kind::Env(v, _) => v[s as usize].clone(),
                        _ => unreachable!(),
                    };
                    self.stack.push(v);
                },
                Op::GetEnvChk(d, s, a) => {
                    let e = self.env_at(fi, d);
                    let v = match &self.heap.get(e).kind {
                        Kind::Env(v, _) => v[s as usize].clone(),
                        _ => unreachable!(),
                    };
                    if let Value::Empty = v {
                        let n = strs::to_rust(self.atoms.name(a));
                        return Err(self.reference_error(&format!("Cannot access '{}' before initialization", n)));
                    }
                    self.stack.push(v);
                },
                Op::SetEnv(d, s) | Op::SetEnvChk(d, s, _) => {
                    let e = self.env_at(fi, d);
                    let v = top!().clone();
                    if let Op::SetEnvChk(_, _, a) = op {
                        if let Kind::Env(vals, _) = &self.heap.get(e).kind {
                            if let Value::Empty = vals[s as usize] {
                                let n = strs::to_rust(self.atoms.name(a));
                                return Err(self.reference_error(&format!("Cannot access '{}' before initialization", n)));
                            }
                        }
                    }
                    if let Kind::Env(vals, _) = &mut self.heap.get_mut(e).kind {
                        vals[s as usize] = v;
                    }
                },
                Op::PushEnv(n, tdz_from) => {
                    let mut vals = vec![Value::Undefined; n as usize];
                    for v in vals.iter_mut().skip(tdz_from as usize) {
                        *v = Value::Empty;
                    }
                    let parent = self.frames[fi].env;
                    let c = self.atom("Env");
                    let e = self.heap.alloc(Obj::new(None, Kind::Env(vals, parent), c));
                    self.frames[fi].env = Some(e);
                },
                Op::PopEnv => {
                    let e = self.frames[fi].env.unwrap();
                    let p = match &self.heap.get(e).kind {
                        Kind::Env(_, p) => *p,
                        _ => None,
                    };
                    self.frames[fi].env = p;
                },
                Op::RefreshEnv => {
                    let e = self.frames[fi].env.unwrap();
                    let (vals, p) = match &self.heap.get(e).kind {
                        Kind::Env(v, p) => (v.clone(), *p),
                        _ => unreachable!(),
                    };
                    let c = self.atom("Env");
                    let ne = self.heap.alloc(Obj::new(None, Kind::Env(vals, p), c));
                    self.frames[fi].env = Some(ne);
                },
                Op::GetGlobal(a) => {
                    if let Some(Slot::Data(x, _)) = self.heap.get(self.global_lex).props.get(a) {
                        if let Value::Empty = x {
                            let n = strs::to_rust(self.atoms.name(a));
                            return Err(self.reference_error(&format!("Cannot access '{}' before initialization", n)));
                        }
                        let x = x.clone();
                        self.stack.push(x);
                        continue;
                    }
                    let g = self.realm.global;
                    let key = self.key_of_atom(a);
                    match self.find_slot_or_host(g, key)? {
                        Some(Lookup::Val(v)) => self.stack.push(v),
                        Some(Lookup::Getter(f)) => {
                            let gv = Value::Obj(g);
                            if let Some(v) = self.call_value(Value::Obj(f), gv, vec![], Cont::Push, false)? {
                                self.stack.push(v);
                            }
                        },
                        Some(Lookup::Host(req)) => {
                            rewind!();
                            return Ok(Some(Status::Host(req)));
                        },
                        Some(Lookup::Call(..)) => self.stack.push(Value::Undefined),
                        None => {
                            let n = strs::to_rust(self.atoms.name(a));
                            return Err(self.reference_error(&format!("{} is not defined", n)));
                        },
                    }
                },
                Op::TypeofGlobal(a) => {
                    if let Some(Slot::Data(x, _)) = self.heap.get(self.global_lex).props.get(a) {
                        if let Value::Empty = x {
                            let n = strs::to_rust(self.atoms.name(a));
                            return Err(self.reference_error(&format!("Cannot access '{}' before initialization", n)));
                        }
                        let t = self.typeof_str(x);
                        self.stack.push(Value::str(t));
                        continue;
                    }
                    let g = self.realm.global;
                    let key = self.key_of_atom(a);
                    match self.find_slot_or_host(g, key)? {
                        Some(Lookup::Val(v)) => {
                            let t = self.typeof_str(&v);
                            self.stack.push(Value::str(t));
                        },
                        Some(Lookup::Getter(f)) => {
                            // typeof of a global getter: run it, then typeof
                            let gv = Value::Obj(g);
                            if let Some(v) = self.call_value(Value::Obj(f), gv, vec![], Cont::Push, false)? {
                                let t = self.typeof_str(&v);
                                self.stack.push(Value::str(t));
                            } else {
                                return Err(self.type_error("typeof on a global getter is not supported yet"));
                            }
                        },
                        Some(Lookup::Host(req)) => {
                            rewind!();
                            return Ok(Some(Status::Host(req)));
                        },
                        Some(Lookup::Call(..)) => self.stack.push(Value::Undefined),
                        None => self.stack.push(Value::str("undefined")),
                    }
                },
                Op::SetGlobal(a) => {
                    let lex = self.global_lex;
                    if let Some(Slot::Data(x, f)) = self.heap.get(lex).props.get(a) {
                        let n = strs::to_rust(self.atoms.name(a));
                        if let Value::Empty = x {
                            return Err(self.reference_error(&format!("Cannot access '{}' before initialization", n)));
                        }
                        if f & WRITABLE == 0 {
                            return Err(self.type_error(&format!("Assignment to constant variable '{}'.", n)));
                        }
                        let nv = top!().clone();
                        if let Some(Slot::Data(x, _)) = self.heap.get_mut(lex).props.get_mut(a) {
                            *x = nv;
                        }
                        continue;
                    }
                    let g = self.realm.global;
                    let v = top!().clone();
                    let strict = self.frames[fi].func.strict;
                    let key = self.key_of_atom(a);
                    if strict && !self.has_property(g, key) && !matches!(self.heap.get(g).kind, Kind::Host(..)) {
                        let n = strs::to_rust(self.atoms.name(a));
                        return Err(self.reference_error(&format!("{} is not defined", n)));
                    }
                    match nres!(self.set_value(&Value::Obj(g), key, v.clone(), strict)) {
                        SetOut::Done => {},
                        SetOut::Call(f, t, args) => {
                            self.call_value(f, t, args, Cont::Discard, false)?;
                        },
                        SetOut::Setter(s) => {
                            self.call_value(Value::Obj(s), Value::Obj(g), vec![v], Cont::Discard, false)?;
                        },
                        SetOut::Host(req) => {
                            rewind!();
                            return Ok(Some(Status::Host(req)));
                        },
                    }
                },
                Op::DeclareLex(a, is_const) => {
                    let lex = self.global_lex;
                    if self.heap.get(lex).props.contains(a) {
                        let n = strs::to_rust(self.atoms.name(a));
                        return Err(self.syntax_error(&format!("Identifier '{}' has already been declared", n)));
                    }
                    let f = if is_const { 0 } else { WRITABLE };
                    self.heap.get_mut(lex).props.insert(a, Slot::Data(Value::Empty, f));
                },
                Op::InitLex(a) => {
                    let lex = self.global_lex;
                    let nv = top!().clone();
                    if let Some(Slot::Data(x, _)) = self.heap.get_mut(lex).props.get_mut(a) {
                        *x = nv;
                    }
                },
                Op::DeclareGlobal(a) => {
                    let g = self.realm.global;
                    if !self.heap.get(g).props.contains(a) {
                        self.heap.get_mut(g).props.insert(a, Slot::Data(Value::Undefined, WRITABLE | ENUMERABLE));
                    }
                },
                Op::DeleteGlobal(a) => {
                    let g = self.realm.global;
                    let key = self.key_of_atom(a);
                    let r = nres!(self.delete(g, key, false));
                    self.stack.push(Value::Bool(r));
                },
                Op::ThrowConst(a) => {
                    let n = strs::to_rust(self.atoms.name(a));
                    return Err(self.type_error(&format!("Assignment to constant variable '{}'.", n)));
                },
                Op::GetProp(a) | Op::GetPropKeep(a) => {
                    let base = top!().clone();
                    let key = self.key_of_atom(a);
                    let keep = matches!(op, Op::GetPropKeep(_));
                    match self.get_value(&base, key) {
                        Ok(Lookup::Val(v)) => {
                            if !keep {
                                self.stack.pop();
                            }
                            self.stack.push(v);
                        },
                        Ok(Lookup::Getter(g)) => {
                            if !keep {
                                self.stack.pop();
                            }
                            if let Some(v) = self.call_value(Value::Obj(g), base, vec![], Cont::Push, false)? {
                                self.stack.push(v);
                            }
                        },
                        Ok(Lookup::Call(f, t, args)) => {
                            if !keep {
                                self.stack.pop();
                            }
                            if let Some(v) = self.call_value(f, t, args, Cont::Push, false)? {
                                self.stack.push(v);
                            }
                        },
                        Ok(Lookup::Host(req)) => {
                            rewind!();
                            return Ok(Some(Status::Host(req)));
                        },
                        Err(Fault::Throw(e)) => return Err(e),
                        Err(Fault::NeedJs) => return Err(self.type_error("internal")),
                    }
                },
                Op::GetElem | Op::GetElemKeep => {
                    let n = self.stack.len();
                    if let Value::Obj(_) = self.stack[n - 1] {
                        if !self.prim_slot(n - 1, false)? {
                            rewind!();
                            continue;
                        }
                    }
                    let kv = self.stack[n - 1].clone();
                    let base = self.stack[n - 2].clone();
                    let key = nres!(self.to_key(&kv));
                    match self.get_value(&base, key) {
                        Ok(Lookup::Val(v)) => {
                            self.stack.pop();
                            if !matches!(op, Op::GetElemKeep) {
                                self.stack.pop();
                            }
                            self.stack.push(v);
                        },
                        Ok(Lookup::Getter(g)) => {
                            self.stack.pop();
                            if !matches!(op, Op::GetElemKeep) {
                                self.stack.pop();
                            }
                            if let Some(v) = self.call_value(Value::Obj(g), base, vec![], Cont::Push, false)? {
                                self.stack.push(v);
                            }
                        },
                        Ok(Lookup::Call(f, t, args)) => {
                            self.stack.pop();
                            if !matches!(op, Op::GetElemKeep) {
                                self.stack.pop();
                            }
                            if let Some(v) = self.call_value(f, t, args, Cont::Push, false)? {
                                self.stack.push(v);
                            }
                        },
                        Ok(Lookup::Host(req)) => {
                            rewind!();
                            return Ok(Some(Status::Host(req)));
                        },
                        Err(Fault::Throw(e)) => return Err(e),
                        Err(Fault::NeedJs) => return Err(self.type_error("internal")),
                    }
                },
                Op::SetProp(a) => {
                    let n = self.stack.len();
                    let v = self.stack[n - 1].clone();
                    let base = self.stack[n - 2].clone();
                    let key = self.key_of_atom(a);
                    let strict = self.frames[fi].func.strict;
                    if a == self.proto_atom() {
                        self.set_proto_of(&base, &v)?;
                        self.stack.truncate(n - 2);
                        self.stack.push(v);
                        continue;
                    }
                    match self.set_value(&base, key, v.clone(), strict) {
                        Ok(SetOut::Done) => {
                            self.stack.truncate(n - 2);
                            self.stack.push(v);
                        },
                        Ok(SetOut::Setter(s)) => {
                            self.stack.truncate(n - 2);
                            self.stack.push(v.clone());
                            self.call_value(Value::Obj(s), base, vec![v], Cont::Discard, false)?;
                        },
                        Ok(SetOut::Call(f, t, args)) => {
                            self.stack.truncate(n - 2);
                            self.stack.push(v);
                            self.call_value(f, t, args, Cont::Discard, false)?;
                        },
                        Ok(SetOut::Host(req)) => {
                            rewind!();
                            return Ok(Some(Status::Host(req)));
                        },
                        Err(Fault::Throw(e)) => return Err(e),
                        Err(Fault::NeedJs) => return Err(self.type_error("internal")),
                    }
                },
                Op::SetElem => {
                    let n = self.stack.len();
                    if let Value::Obj(_) = self.stack[n - 2] {
                        if !self.prim_slot(n - 2, false)? {
                            rewind!();
                            continue;
                        }
                    }
                    let v = self.stack[n - 1].clone();
                    let kv = self.stack[n - 2].clone();
                    let base = self.stack[n - 3].clone();
                    let key = nres!(self.to_key(&kv));
                    let strict = self.frames[fi].func.strict;
                    if let Key::Atom(a) = key {
                        if a == self.proto_atom() {
                            self.set_proto_of(&base, &v)?;
                            self.stack.truncate(n - 3);
                            self.stack.push(v);
                            continue;
                        }
                    }
                    match self.set_value(&base, key, v.clone(), strict) {
                        Ok(SetOut::Done) => {
                            self.stack.truncate(n - 3);
                            self.stack.push(v);
                        },
                        Ok(SetOut::Setter(s)) => {
                            self.stack.truncate(n - 3);
                            self.stack.push(v.clone());
                            self.call_value(Value::Obj(s), base, vec![v], Cont::Discard, false)?;
                        },
                        Ok(SetOut::Call(f, t, args)) => {
                            self.stack.truncate(n - 3);
                            self.stack.push(v);
                            self.call_value(f, t, args, Cont::Discard, false)?;
                        },
                        Ok(SetOut::Host(req)) => {
                            rewind!();
                            return Ok(Some(Status::Host(req)));
                        },
                        Err(Fault::Throw(e)) => return Err(e),
                        Err(Fault::NeedJs) => return Err(self.type_error("internal")),
                    }
                },
                Op::DelProp(a) => {
                    let base = pop!();
                    let strict = self.frames[fi].func.strict;
                    let o = nres!(self.to_object(&base));
                    let key = self.key_of_atom(a);
                    if let Some((t, h)) = nres!(self.proxy_parts(o)) {
                        if let Some(f) = nres!(self.proxy_trap(h, "deleteProperty")) {
                            let kv2 = self.key_value(key);
                            if let Some(v) = self.call_value(f, Value::Obj(h), vec![Value::Obj(t), kv2], Cont::Push, false)? {
                                self.stack.push(v);
                            }
                            continue;
                        }
                    }
                    let r = nres!(self.delete(o, key, strict));
                    self.stack.push(Value::Bool(r));
                },
                Op::DelElem => {
                    let kv = pop!();
                    let base = pop!();
                    let strict = self.frames[fi].func.strict;
                    let o = nres!(self.to_object(&base));
                    let key = nres!(self.to_key(&kv));
                    if let Some((t, h)) = nres!(self.proxy_parts(o)) {
                        if let Some(f) = nres!(self.proxy_trap(h, "deleteProperty")) {
                            let kv2 = self.key_value(key);
                            if let Some(v) = self.call_value(f, Value::Obj(h), vec![Value::Obj(t), kv2], Cont::Push, false)? {
                                self.stack.push(v);
                            }
                            continue;
                        }
                    }
                    let r = nres!(self.delete(o, key, strict));
                    self.stack.push(Value::Bool(r));
                },
                Op::InitProp(a) => {
                    let v = pop!();
                    let o = top!().as_obj().unwrap();
                    let key = self.key_of_atom(a);
                    self.define_own(o, key, Slot::Data(v, ALL));
                },
                Op::InitElem => {
                    let v = pop!();
                    let kv = pop!();
                    let o = top!().as_obj().unwrap();
                    let key = nres!(self.to_key(&kv));
                    self.define_own(o, key, Slot::Data(v, ALL));
                },
                Op::InitGetter(_) | Op::InitSetter(_) | Op::InitGetterElem | Op::InitSetterElem => {
                    let f = pop!().as_obj().unwrap();
                    let key = match op {
                        Op::InitGetter(a) | Op::InitSetter(a) => self.key_of_atom(a),
                        _ => {
                            let kv = pop!();
                            nres!(self.to_key(&kv))
                        },
                    };
                    let o = top!().as_obj().unwrap();
                    let is_get = matches!(op, Op::InitGetter(_) | Op::InitGetterElem);
                    let a = self.key_atom(key);
                    let old = self.heap.get(o).props.get(a).cloned();
                    let (mut g, mut s) = match old {
                        Some(Slot::Accessor(g, s, _)) => (g, s),
                        _ => (None, None),
                    };
                    if is_get {
                        g = Some(f);
                    } else {
                        s = Some(f);
                    }
                    self.heap.get_mut(o).props.insert(a, Slot::Accessor(g, s, ENUMERABLE | CONFIGURABLE));
                },
                Op::Call(argc) | Op::CallMethod(argc) | Op::New(argc) => {
                    let argc = argc as usize;
                    let n = self.stack.len();
                    let fpos = n - argc - 1;
                    let f = self.stack[fpos].clone();
                    let (this, ret_sp) = match op {
                        Op::CallMethod(_) => (self.stack[fpos - 1].clone(), fpos - 1),
                        _ => (Value::Undefined, fpos),
                    };
                    let construct = matches!(op, Op::New(_));
                    // host functions ask the host
                    if let Value::Obj(fo) = &f {
                        if let Kind::HostFn(name) = self.heap.get(*fo).kind {
                            match self.answer.take() {
                                Some(v) => {
                                    self.stack.truncate(ret_sp);
                                    self.stack.push(v);
                                },
                                None => {
                                    rewind!();
                                    let args = self.stack[fpos + 1..].to_vec();
                                    let nm = self.atoms.name(name).clone();
                                    if strs::eq_str(&nm, "%import") {
                                        let url = match args.first() {
                                            Some(Value::Str(s)) => s.clone(),
                                            _ => js(""),
                                        };
                                        return Ok(Some(Status::Host(HostReq::Import { url })));
                                    }
                                    return Ok(Some(Status::Host(HostReq::Call { func: *fo, name: nm, this, args, construct })));
                                },
                            }
                            continue;
                        }
                        // a closure: the arguments are already in place
                        if let Kind::Closure(c) = &self.heap.get(*fo).kind {
                            let fo = *fo;
                            if construct {
                                let derived = c.func.kind == crate::ast::FuncKind::DerivedConstructor;
                                if !self.is_constructor(&f) {
                                    let what = self.call_site_name().unwrap_or_else(|| self.describe(&f));
                                    return Err(self.type_error(&format!("{} is not a constructor", what)));
                                }
                                if derived {
                                    self.stack.remove(fpos);
                                    self.enter_closure(fo, Value::Empty, fpos, argc, fpos, Cont::Push, f.clone())?;
                                    continue;
                                }
                                let proto = match nres!(self.get(&f, Key::Atom(k::prototype))) {
                                    Value::Obj(p) => p,
                                    _ => self.realm.object_proto,
                                };
                                let obj = self.new_object_proto(Some(proto));
                                // drop the callee slot: args move down one
                                self.stack.remove(fpos);
                                self.enter_closure(fo, Value::Obj(obj), fpos, argc, fpos, Cont::Construct(obj), f.clone())?;
                                continue;
                            }
                            if matches!(c.func.kind, crate::ast::FuncKind::ClassConstructor | crate::ast::FuncKind::DerivedConstructor) {
                                return Err(self.type_error("Class constructor cannot be invoked without 'new'"));
                            }
                            // args start at fpos + 1; the frame returns to ret_sp
                            self.enter_closure(fo, this, fpos + 1, argc, ret_sp, Cont::Push, Value::Undefined)?;
                            continue;
                        }
                    }
                    let args = self.stack[fpos + 1..].to_vec();
                    self.stack.truncate(ret_sp);
                    let r = if construct { self.construct_value(f, args, Cont::Push)? } else { self.call_value(f, this, args, Cont::Push, false)? };
                    if let Some(v) = r {
                        self.stack.push(v);
                    }
                },
                Op::CallSpread | Op::CallMethodSpread | Op::NewSpread => {
                    let n = self.stack.len();
                    let method = matches!(op, Op::CallMethodSpread);
                    let arr = self.stack[n - 1].clone();
                    let f = self.stack[n - 2].clone();
                    let this = if method { self.stack[n - 3].clone() } else { Value::Undefined };
                    let args = match arr.as_obj().map(|o| self.heap.get(o).elements().cloned()) {
                        Some(Some(v)) => v.into_iter().map(|x| if let Value::Empty = x { Value::Undefined } else { x }).collect::<Vec<_>>(),
                        _ => vec![],
                    };
                    let base = n - 2 - method as usize;
                    if let Value::Obj(fo) = &f {
                        if let Kind::HostFn(name) = self.heap.get(*fo).kind {
                            match self.answer.take() {
                                Some(v) => {
                                    self.stack.truncate(base);
                                    self.stack.push(v);
                                },
                                None => {
                                    rewind!();
                                    let nm = self.atoms.name(name).clone();
                                    return Ok(Some(Status::Host(HostReq::Call { func: *fo, name: nm, this, args, construct: matches!(op, Op::NewSpread) })));
                                },
                            }
                            continue;
                        }
                    }
                    self.stack.truncate(base);
                    let r = if matches!(op, Op::NewSpread) { self.construct_value(f, args, Cont::Push)? } else { self.call_value(f, this, args, Cont::Push, false)? };
                    if let Some(v) = r {
                        self.stack.push(v);
                    }
                },
                Op::Return => {
                    let mut v = pop!();
                    let f = self.frames.pop().unwrap();
                    self.stack.truncate(f.ret_sp);
                    if let Some(g) = f.gen {
                        self.gen_done(g);
                        if self.gen_flavor(g) == 0 {
                            v = self.iter_result(v, true);
                        }
                    }
                    if let Some(st) = self.finish(v, f.cont)? {
                        return Ok(Some(st));
                    }
                },
                Op::Throw => {
                    let v = pop!();
                    return Err(v);
                },
                Op::MakeClass(i, heritage) => {
                    let (proto_parent, ctor_parent) = if heritage {
                        let parent = pop!();
                        match parent {
                            Value::Null => (None, self.realm.function_proto),
                            Value::Obj(po) if self.is_constructor(&parent) => {
                                let pp = match self.get_value(&parent, Key::Atom(k::prototype)) {
                                    Ok(Lookup::Val(v)) => v,
                                    Ok(_) => Value::Undefined,
                                    Err(Fault::Throw(e)) => return Err(e),
                                    Err(Fault::NeedJs) => Value::Undefined,
                                };
                                match pp {
                                    Value::Obj(p) => (Some(p), po),
                                    Value::Null => (None, po),
                                    _ => return Err(self.type_error("Class extends value does not have valid prototype property")),
                                }
                            },
                            other => {
                                let d = self.describe(&other);
                                return Err(self.type_error(&format!("Class extends value {} is not a constructor or null", d)));
                            },
                        }
                    } else {
                        (Some(self.realm.object_proto), self.realm.function_proto)
                    };
                    let proto = self.new_object_proto(proto_parent);
                    let fproto = self.frames[fi].func.funcs[i as usize].clone();
                    let env = self.frames[fi].env;
                    let c = self.atom("Function");
                    let cl = Closure { func: fproto, env, home: Some(proto), this_val: None, fields: None };
                    let fo = self.heap.alloc(Obj::new(Some(ctor_parent), Kind::Closure(cl), c));
                    self.define_atom(fo, k::prototype, Value::Obj(proto), 0);
                    self.define_atom(proto, k::constructor, Value::Obj(fo), HIDDEN);
                    self.stack.push(Value::Obj(fo));
                    self.stack.push(Value::Obj(proto));
                },
                Op::DefineMethod(_, kind) | Op::DefineMethodElem(kind) => {
                    let f = pop!();
                    let key = match op {
                        Op::DefineMethod(a, _) => self.key_of_atom(a),
                        _ => {
                            let kv = pop!();
                            nres!(self.to_key(&kv))
                        },
                    };
                    let o = top!().as_obj().unwrap();
                    let fo = f.as_obj().unwrap();
                    if let Kind::Closure(c) = &mut self.heap.get_mut(fo).kind {
                        c.home = Some(o);
                    }
                    if kind == 0 {
                        self.define_own(o, key, Slot::Data(f, HIDDEN));
                    } else {
                        let a = self.key_atom(key);
                        let (mut g, mut s) = match self.heap.get(o).props.get(a) {
                            Some(Slot::Accessor(g, s, _)) => (*g, *s),
                            _ => (None, None),
                        };
                        if kind == 1 {
                            g = Some(fo);
                        } else {
                            s = Some(fo);
                        }
                        self.heap.get_mut(o).props.insert(a, Slot::Accessor(g, s, CONFIGURABLE));
                    }
                },
                Op::SetHome => {
                    let n = self.stack.len();
                    if let (Value::Obj(o), Value::Obj(f)) = (self.stack[n - 2].clone(), self.stack[n - 1].clone()) {
                        if let Kind::Closure(c) = &mut self.heap.get_mut(f).kind {
                            c.home = Some(o);
                        }
                    }
                },
                Op::SetFields | Op::SetFieldKeys => {
                    let v = pop!();
                    if let (Some(t), Value::Obj(x)) = (top!().as_obj(), v) {
                        if let Kind::Closure(c) = &mut self.heap.get_mut(t).kind {
                            c.fields = Some(x);
                        }
                    }
                },
                Op::FieldKey(n) => {
                    let keys = match self.frames[fi].callee.map(|c| &self.heap.get(c).kind) {
                        Some(Kind::Closure(c)) => c.fields,
                        _ => None,
                    };
                    let v = match keys.map(|k| &self.heap.get(k).kind) {
                        Some(Kind::Array(items)) => items.get(n as usize).cloned().unwrap_or(Value::Undefined),
                        _ => Value::Undefined,
                    };
                    self.stack.push(v);
                },
                Op::DefineField(_) | Op::DefineFieldElem => {
                    let v = pop!();
                    let key = match op {
                        Op::DefineField(a) => self.key_of_atom(a),
                        _ => {
                            let kv = pop!();
                            nres!(self.to_key(&kv))
                        },
                    };
                    let this = pop!();
                    if let Value::Obj(o) = this {
                        match key {
                            Key::Atom(a) if self.atoms.is_private(a) => {
                                self.heap.get_mut(o).props.insert(a, Slot::Data(v, WRITABLE));
                            },
                            _ => self.define_own(o, key, Slot::Data(v, ALL)),
                        }
                    }
                    self.stack.push(Value::Undefined);
                },
                Op::InitFields => {
                    let ctor = pop!();
                    let this = top!().clone();
                    let init = match ctor.as_obj().map(|c| &self.heap.get(c).kind) {
                        Some(Kind::Closure(c)) => c.fields,
                        _ => None,
                    };
                    if let Some(init) = init {
                        self.call_value(Value::Obj(init), this, vec![], Cont::Discard, false)?;
                    }
                },
                Op::SuperCall(_) | Op::SuperCallSpread => {
                    let args = match op {
                        Op::SuperCall(n) => {
                            let at = self.stack.len() - n as usize;
                            self.stack.split_off(at)
                        },
                        _ => {
                            let arr = pop!();
                            match arr.as_obj().map(|o| self.heap.get(o).elements().cloned()) {
                                Some(Some(v)) => v.into_iter().map(|x| if let Value::Empty = x { Value::Undefined } else { x }).collect(),
                                _ => vec![],
                            }
                        },
                    };
                    let nt = pop!();
                    let parent = pop!();
                    if !self.is_constructor(&parent) {
                        return Err(self.type_error("Super constructor is not a constructor"));
                    }
                    if let Some(v) = self.construct_nt(parent, args, nt, Cont::Push)? {
                        self.stack.push(v);
                    }
                },
                Op::HasPrivate(a) => {
                    let v = pop!();
                    let r = match v {
                        Value::Obj(o) => self.has_property(o, Key::Atom(a)),
                        _ => return Err(self.type_error("Cannot use 'in' operator to search for a private field in a non-object")),
                    };
                    self.stack.push(Value::Bool(r));
                },
                Op::GetProtoOf => {
                    let v = pop!();
                    let p = match v {
                        Value::Obj(o) => self.heap.get(o).proto.map(Value::Obj).unwrap_or(Value::Null),
                        _ => Value::Null,
                    };
                    self.stack.push(p);
                },
                Op::SuperGet(_, keep) | Op::SuperGetElem(keep) => {
                    let key = match op {
                        Op::SuperGet(a, _) => self.key_of_atom(a),
                        _ => {
                            let kv = pop!();
                            nres!(self.to_key(&kv))
                        },
                    };
                    let this = if keep { top!().clone() } else { pop!() };
                    let home = match self.frames[fi].callee.map(|c| &self.heap.get(c).kind) {
                        Some(Kind::Closure(c)) => c.home,
                        _ => None,
                    };
                    let start = home.and_then(|h| self.heap.get(h).proto);
                    let r = match start {
                        Some(s) => match self.get_from(s, key) {
                            Ok(l) => l,
                            Err(Fault::Throw(e)) => return Err(e),
                            Err(Fault::NeedJs) => Lookup::Val(Value::Undefined),
                        },
                        None => Lookup::Val(Value::Undefined),
                    };
                    match r {
                        Lookup::Val(v) => self.stack.push(v),
                        Lookup::Getter(g) => {
                            if let Some(v) = self.call_value(Value::Obj(g), this, vec![], Cont::Push, false)? {
                                self.stack.push(v);
                            }
                        },
                        _ => self.stack.push(Value::Undefined),
                    }
                },
                Op::BigInt(i) => {
                    let s = match &self.frames[fi].func.consts[i as usize] {
                        Value::Str(s) => strs::to_rust(s),
                        _ => String::new(),
                    };
                    let b = crate::bigint::BigInt::parse(&s).unwrap_or_else(crate::bigint::BigInt::zero);
                    let v = self.new_bigint(b);
                    self.stack.push(v);
                },
                Op::GenStart => {
                    let func = self.frames[fi].func.clone();
                    let callee = self.frames[fi].callee;
                    let flavor = if func.is_async { if func.generator { 2 } else { 1 } } else { 0 };
                    let proto = if flavor == 1 {
                        None
                    } else {
                        let p = match callee {
                            Some(c) => match self.get(&Value::Obj(c), Key::Atom(k::prototype)) {
                                Ok(Value::Obj(p)) => Some(p),
                                _ => None,
                            },
                            None => None,
                        };
                        Some(p.unwrap_or_else(|| self.proto(if flavor == 2 { "AsyncGenerator" } else { "Generator" })))
                    };
                    let (saved, cont) = self.save_top_frame();
                    let c = self.atom("Generator");
                    let gd = GenData { state: GenState::Start, frame: Some(saved), flavor, yield_kind: 0, ret_val: None };
                    let g = self.heap.alloc(Obj::new(proto, Kind::Generator(Box::new(gd)), c));
                    if flavor == 0 {
                        if let Some(st) = self.finish(Value::Obj(g), cont)? {
                            return Ok(Some(st));
                        }
                    } else {
                        let i = if flavor == 1 { crate::compiler::intr::ASYNC_RUN } else { crate::compiler::intr::ASYNC_GEN };
                        let drv = self.realm.intrinsics[i as usize].clone();
                        if let Some(v) = self.call_value(drv, Value::Undefined, vec![Value::Obj(g)], cont, false)? {
                            return Ok(Some(Status::Done(v)));
                        }
                    }
                },
                Op::Yield(kind) => {
                    let v = pop!();
                    let Some(g) = self.frames[fi].gen else { return Err(self.syntax_error("yield or await outside a generator or async function")) };
                    let (saved, cont) = self.save_top_frame();
                    let flavor = self.gen_flavor(g);
                    if let Kind::Generator(gd) = &mut self.heap.get_mut(g).kind {
                        gd.state = GenState::Suspended;
                        gd.yield_kind = kind;
                        gd.frame = Some(saved);
                    }
                    let out = if flavor == 0 && kind == 0 { self.iter_result(v, false) } else { v };
                    if let Some(st) = self.finish(out, cont)? {
                        return Ok(Some(st));
                    }
                },
                Op::This => {
                    let t = self.frames[fi].this.clone();
                    self.stack.push(t);
                },
                Op::NewTarget => {
                    let t = self.frames[fi].new_target.clone();
                    self.stack.push(t);
                },
                Op::Jump(t) => self.frames[fi].ip = t as usize,
                Op::JumpIfFalse(t) => {
                    let v = pop!();
                    if !self.truthy(&v) {
                        self.frames[fi].ip = t as usize;
                    }
                },
                Op::JumpIfTrue(t) => {
                    let v = pop!();
                    if self.truthy(&v) {
                        self.frames[fi].ip = t as usize;
                    }
                },
                Op::JumpIfFalseKeep(t) => {
                    if !self.truthy(top!()) {
                        self.frames[fi].ip = t as usize;
                    } else {
                        self.stack.pop();
                    }
                },
                Op::JumpIfTrueKeep(t) => {
                    if self.truthy(top!()) {
                        self.frames[fi].ip = t as usize;
                    } else {
                        self.stack.pop();
                    }
                },
                Op::JumpIfNotNullishKeep(t) => {
                    if !top!().is_nullish() {
                        self.frames[fi].ip = t as usize;
                    } else {
                        self.stack.pop();
                    }
                },
                Op::JumpIfNotUndefined(t) => {
                    if !top!().is_undefined() {
                        self.frames[fi].ip = t as usize;
                    }
                },
                Op::OptCheck(t) => {
                    if top!().is_nullish() {
                        self.stack.pop();
                        self.stack.push(Value::Undefined);
                        self.frames[fi].ip = t as usize;
                    }
                },
                Op::OptCheckCall(t) => {
                    if top!().is_nullish() {
                        self.stack.pop();
                        self.stack.pop();
                        self.stack.push(Value::Undefined);
                        self.frames[fi].ip = t as usize;
                    }
                },
                Op::Not => {
                    let v = pop!();
                    let b = self.truthy(&v);
                    self.stack.push(Value::Bool(!b));
                },
                Op::Typeof => {
                    let v = pop!();
                    let t = self.typeof_str(&v);
                    self.stack.push(Value::str(t));
                },
                Op::Neg | Op::Plus | Op::BitNot | Op::Inc | Op::Dec | Op::ToNumber => {
                    let n = self.stack.len();
                    if let Value::Obj(_) = self.stack[n - 1] {
                        if !self.prim_slot(n - 1, true)? {
                            rewind!();
                            continue;
                        }
                    }
                    let v = pop!();
                    if let Some(b) = self.bigint_of(&v) {
                        let one = crate::bigint::BigInt::from_i64(1);
                        let r = match op {
                            Op::Neg => b.neg(),
                            Op::BitNot => b.not(),
                            Op::Inc => b.add(&one),
                            Op::Dec => b.sub(&one),
                            Op::ToNumber => (*b).clone(),
                            _ => return Err(self.type_error("Cannot convert a BigInt value to a number")),
                        };
                        let r = self.new_bigint(r);
                        self.stack.push(r);
                        continue;
                    }
                    let x = nres!(self.to_number(&v));
                    let r = match op {
                        Op::Neg => -x,
                        Op::Plus | Op::ToNumber => x,
                        Op::BitNot => !crate::num::to_int32(x) as f64,
                        Op::Inc => x + 1.0,
                        _ => x - 1.0,
                    };
                    self.stack.push(Value::Num(r));
                },
                Op::ToStr => {
                    let n = self.stack.len();
                    if let Value::Obj(_) = self.stack[n - 1] {
                        if !self.prim_slot(n - 1, false)? {
                            rewind!();
                            continue;
                        }
                    }
                    let v = pop!();
                    let s = nres!(self.to_str(&v));
                    self.stack.push(Value::Str(s));
                },
                Op::ToKey => {
                    let n = self.stack.len();
                    if let Value::Obj(_) = self.stack[n - 1] {
                        if !self.prim_slot(n - 1, false)? {
                            rewind!();
                            continue;
                        }
                    }
                    // numbers stay numbers (array indexes); everything else becomes a string
                    let v = pop!();
                    let r = match v {
                        Value::Num(_) | Value::Str(_) => v,
                        ref s if self.sym_of(s).is_some() => v,
                        other => Value::Str(nres!(self.to_str(&other))),
                    };
                    self.stack.push(r);
                },
                Op::Add => {
                    let n = self.stack.len();
                    match (&self.stack[n - 2], &self.stack[n - 1]) {
                        (Value::Num(a), Value::Num(b)) => {
                            let r = a + b;
                            self.stack.truncate(n - 2);
                            self.stack.push(Value::Num(r));
                            continue;
                        },
                        (Value::Str(a), Value::Str(b)) => {
                            let r = strs::concat(a, b);
                            self.stack.truncate(n - 2);
                            self.stack.push(Value::Str(r));
                            continue;
                        },
                        _ => {},
                    }
                    if !self.prim_slot(n - 2, true)? || !self.prim_slot(n - 1, true)? {
                        rewind!();
                        continue;
                    }
                    let b = pop!();
                    let a = pop!();
                    if !matches!(a, Value::Str(_)) && !matches!(b, Value::Str(_)) && (self.bigint_of(&a).is_some() || self.bigint_of(&b).is_some()) {
                        let r = self.bigint_binop(Op::Add, &a, &b)?;
                        self.stack.push(r);
                        continue;
                    }
                    let r = if matches!(a, Value::Str(_)) || matches!(b, Value::Str(_)) {
                        let x = nres!(self.to_str(&a));
                        let y = nres!(self.to_str(&b));
                        Value::Str(strs::concat(&x, &y))
                    } else {
                        Value::Num(nres!(self.to_number(&a)) + nres!(self.to_number(&b)))
                    };
                    self.stack.push(r);
                },
                Op::Sub | Op::Mul | Op::Div | Op::Mod | Op::Exp | Op::Shl | Op::Shr | Op::UShr | Op::BitAnd | Op::BitOr | Op::BitXor => {
                    let n = self.stack.len();
                    let (x, y) = match (&self.stack[n - 2], &self.stack[n - 1]) {
                        (Value::Num(a), Value::Num(b)) => (*a, *b),
                        _ => {
                            if !self.prim_slot(n - 2, true)? || !self.prim_slot(n - 1, true)? {
                                rewind!();
                                continue;
                            }
                            let a = self.stack[n - 2].clone();
                            let b = self.stack[n - 1].clone();
                            if self.bigint_of(&a).is_some() || self.bigint_of(&b).is_some() {
                                let r = self.bigint_binop(op, &a, &b)?;
                                self.stack.truncate(n - 2);
                                self.stack.push(r);
                                continue;
                            }
                            (nres!(self.to_number(&a)), nres!(self.to_number(&b)))
                        },
                    };
                    self.stack.truncate(n - 2);
                    let r = arith(op, x, y);
                    self.stack.push(Value::Num(r));
                },
                Op::Lt | Op::Le | Op::Gt | Op::Ge => {
                    let n = self.stack.len();
                    if !matches!((&self.stack[n - 2], &self.stack[n - 1]), (Value::Num(_), Value::Num(_))) {
                        if !self.prim_slot(n - 2, true)? || !self.prim_slot(n - 1, true)? {
                            rewind!();
                            continue;
                        }
                    }
                    if self.bigint_of(&self.stack[n - 1]).is_some() || self.bigint_of(&self.stack[n - 2]).is_some() {
                        let b = pop!();
                        let a = pop!();
                        let r = self.bigint_compare(op, &a, &b)?;
                        self.stack.push(Value::Bool(r));
                        continue;
                    }
                    let b = pop!();
                    let a = pop!();
                    let r = match (&a, &b) {
                        (Value::Str(x), Value::Str(y)) => match op {
                            Op::Lt => x < y,
                            Op::Le => x <= y,
                            Op::Gt => x > y,
                            _ => x >= y,
                        },
                        _ => {
                            let x = nres!(self.to_number(&a));
                            let y = nres!(self.to_number(&b));
                            match op {
                                Op::Lt => x < y,
                                Op::Le => x <= y,
                                Op::Gt => x > y,
                                _ => x >= y,
                            }
                        },
                    };
                    self.stack.push(Value::Bool(r));
                },
                Op::StrictEq | Op::StrictNe => {
                    let b = pop!();
                    let a = pop!();
                    let r = self.strict_equals(&a, &b);
                    self.stack.push(Value::Bool(if matches!(op, Op::StrictEq) { r } else { !r }));
                },
                Op::Eq | Op::Ne => {
                    let n = self.stack.len();
                    // an object against a primitive converts the object
                    let (a_obj, b_obj) = (matches!(self.stack[n - 2], Value::Obj(_)), matches!(self.stack[n - 1], Value::Obj(_)));
                    if a_obj != b_obj {
                        let other = if a_obj { &self.stack[n - 1] } else { &self.stack[n - 2] };
                        if !other.is_nullish() {
                            let slot = if a_obj { n - 2 } else { n - 1 };
                            if !self.prim_slot(slot, true)? {
                                rewind!();
                                continue;
                            }
                        }
                    }
                    let b = pop!();
                    let a = pop!();
                    let r = nres!(self.loose_eq(&a, &b));
                    self.stack.push(Value::Bool(if matches!(op, Op::Eq) { r } else { !r }));
                },
                Op::In => {
                    let o = pop!();
                    let kv = pop!();
                    let o = match o {
                        Value::Obj(o) => o,
                        _ => return Err(self.type_error("Cannot use 'in' operator to search for a key in a non-object")),
                    };
                    if let Some((t, h)) = nres!(self.proxy_parts(o)) {
                        if let Some(f) = nres!(self.proxy_trap(h, "has")) {
                            let key = nres!(self.to_key(&kv));
                            let kv2 = self.key_value(key);
                            if let Some(v) = self.call_value(f, Value::Obj(h), vec![Value::Obj(t), kv2], Cont::Push, false)? {
                                self.stack.push(v);
                            }
                            continue;
                        }
                    }
                    if let Kind::Host(..) = self.heap.get(o).kind {
                        // ask the host whether it has the key: a read that is not "not mine"
                        let key = nres!(self.to_key(&kv));
                        match self.answer.take() {
                            Some(v) => {
                                let has = !matches!(v, Value::Empty) || self.has_property(o, key);
                                self.stack.push(Value::Bool(has));
                            },
                            None => {
                                self.stack.push(kv);
                                self.stack.push(Value::Obj(o));
                                rewind!();
                                let class = self.atoms.name(self.heap.get(o).class).clone();
                                let id = match self.heap.get(o).kind {
                                    Kind::Host(_, id) => id,
                                    _ => 0.0,
                                };
                                let k2 = self.key_name(key);
                                return Ok(Some(Status::Host(HostReq::Get { obj: o, class, id, key: k2 })));
                            },
                        }
                        continue;
                    }
                    let key = nres!(self.to_key(&kv));
                    let r = self.has_property(o, key);
                    self.stack.push(Value::Bool(r));
                },
                Op::InstanceOf => {
                    let c = pop!();
                    let v = pop!();
                    let r = match self.instance_of(&v, &c) {
                        Ok(r) => r,
                        Err(Fault::Throw(e)) => return Err(e),
                        Err(Fault::NeedJs) => {
                            let h = match c.as_obj() {
                                Some(co) => self.lookup_method(co, crate::atoms::sym::hasInstance).ok().flatten(),
                                None => None,
                            };
                            match h {
                                Some(h) if self.is_callable(&h) => {
                                    if let Some(x) = self.call_value(h, c, vec![v], Cont::Push, false)? {
                                        let b = self.truthy(&x);
                                        self.stack.push(Value::Bool(b));
                                    }
                                    continue;
                                },
                                _ => false,
                            }
                        },
                    };
                    self.stack.push(Value::Bool(r));
                },
                Op::Closure(i) => {
                    let proto = self.frames[fi].func.funcs[i as usize].clone();
                    let env = self.frames[fi].env;
                    let this_val = if proto.arrow { Some(self.frames[fi].this.clone()) } else { None };
                    let c = self.atom("Function");
                    let home = if proto.arrow {
                        match self.frames[fi].callee.map(|c| &self.heap.get(c).kind) {
                            Some(Kind::Closure(cc)) => cc.home,
                            _ => None,
                        }
                    } else {
                        None
                    };
                    // an arrow keeps new.target too (in the slot only constructors use)
                    let arrow_nt = if proto.arrow { self.frames[fi].new_target.as_obj() } else { None };
                    let o = self.heap.alloc(Obj::new(Some(self.realm.function_proto), Kind::Closure(Closure { func: proto, env, home, this_val, fields: arrow_nt }), c));
                    self.stack.push(Value::Obj(o));
                },
                Op::NewObject => {
                    let o = self.new_object();
                    self.stack.push(Value::Obj(o));
                },
                Op::NewArray(n) => {
                    let n = n as usize;
                    let at = self.stack.len() - n;
                    let items = self.stack.split_off(at);
                    let a = self.new_array(items);
                    self.stack.push(Value::Obj(a));
                },
                Op::ArrayPush | Op::ArrayHole => {
                    let v = if matches!(op, Op::ArrayHole) { Value::Empty } else { pop!() };
                    let a = top!().as_obj().unwrap();
                    if let Some(el) = self.heap.get_mut(a).elements_mut() {
                        el.push(v);
                    }
                },
                Op::ArrayAppend => {
                    let src = pop!();
                    let a = top!().as_obj().unwrap();
                    let items = match src.as_obj().and_then(|o| self.heap.get(o).elements().cloned()) {
                        Some(v) => v,
                        None => vec![],
                    };
                    if let Some(el) = self.heap.get_mut(a).elements_mut() {
                        el.extend(items.into_iter().map(|x| if let Value::Empty = x { Value::Undefined } else { x }));
                    }
                },
                Op::RegExp(i) => {
                    let (src, flags) = self.frames[fi].func.regexes[i as usize].clone();
                    let r = nres!(crate::builtins::new_regexp(self, &src, &flags));
                    self.stack.push(r);
                },
                Op::Intrinsic(i) => {
                    if i == u32::MAX {
                        let c = self.frames[fi].callee.map(Value::Obj).unwrap_or(Value::Undefined);
                        self.stack.push(c);
                    } else {
                        let v = self.realm.intrinsics[i as usize].clone();
                        self.stack.push(v);
                    }
                },
                Op::Arguments => {
                    let args = self.frames[fi].args.clone().unwrap_or_default();
                    let c = self.atom("Arguments");
                    let o = self.heap.alloc(Obj::new(Some(self.realm.object_proto), Kind::Arguments(args), c));
                    if let Some(callee) = self.frames[fi].callee {
                        if !self.frames[fi].func.strict {
                            self.define_atom(o, k::callee, Value::Obj(callee), HIDDEN);
                        }
                    }
                    self.stack.push(Value::Obj(o));
                },
                Op::Rest(from) => {
                    let args = self.frames[fi].args.clone().unwrap_or_default();
                    let rest: Vec<Value> = args.into_iter().skip(from as usize).collect();
                    let a = self.new_array(rest);
                    self.stack.push(Value::Obj(a));
                },
                Op::ForInStart => {
                    let v = pop!();
                    let keys = match &v {
                        Value::Obj(o) => self.for_in_keys(*o),
                        Value::Str(s) => (0..s.len()).map(|i| Value::Str(js(&format!("{}", i)))).collect(),
                        _ => vec![],
                    };
                    let target = match v {
                        Value::Obj(o) => o,
                        _ => self.new_object(),
                    };
                    let c = self.atom("ForIn");
                    let it = self.heap.alloc(Obj::new(None, Kind::ForIn(keys, 0, target), c));
                    self.stack.push(Value::Obj(it));
                },
                Op::ForInNext(t) => {
                    let it = top!().as_obj().unwrap();
                    loop {
                        let (key, target) = match &mut self.heap.get_mut(it).kind {
                            Kind::ForIn(keys, pos, target) => {
                                if *pos >= keys.len() {
                                    (None, *target)
                                } else {
                                    *pos += 1;
                                    (Some(keys[*pos - 1].clone()), *target)
                                }
                            },
                            _ => unreachable!(),
                        };
                        match key {
                            None => {
                                self.frames[fi].ip = t as usize;
                                break;
                            },
                            Some(kv) => {
                                // skip keys deleted during the walk
                                let s = match &kv {
                                    Value::Str(s) => s.clone(),
                                    _ => continue,
                                };
                                let key = self.key_of_str(&s);
                                if matches!(self.heap.get(target).kind, Kind::Host(..)) || self.has_property(target, key) {
                                    self.stack.push(kv);
                                    break;
                                }
                            },
                        }
                    }
                },
                Op::EnterTry(t) | Op::EnterFinally(t) => {
                    let sp = self.stack.len();
                    let env = self.frames[fi].env;
                    self.frames[fi].handlers.push(Handler { catch_ip: t as usize, sp, env, finally: matches!(op, Op::EnterFinally(_)) });
                },
                Op::LeaveTry => {
                    self.frames[fi].handlers.pop();
                },
                Op::CheckObjCoercible => {
                    if top!().is_nullish() {
                        return Err(self.type_error("Cannot destructure undefined or null"));
                    }
                },
                Op::Debugger => {},
                Op::ThrowMsg(i, kind) => {
                    let m = match &self.frames[fi].func.consts[i as usize] {
                        Value::Str(s) => strs::to_rust(s),
                        _ => String::new(),
                    };
                    return Err(if kind == 0 { self.type_error(&m) } else { self.reference_error(&m) });
                },
            }
        }
    }

    fn env_at(&self, fi: usize, depth: u16) -> ObjRef {
        let mut e = self.frames[fi].env.expect("no environment");
        for _ in 0..depth {
            e = match &self.heap.get(e).kind {
                Kind::Env(_, Some(p)) => *p,
                _ => panic!("environment chain too short"),
            };
        }
        e
    }

    fn proto_atom(&mut self) -> Atom {
        self.atom("__proto__")
    }

    fn set_proto_of(&mut self, base: &Value, v: &Value) -> Result<(), Value> {
        if let Value::Obj(o) = base {
            match v {
                Value::Obj(p) => {
                    // no cycles
                    let mut cur = Some(*p);
                    while let Some(c) = cur {
                        if c == *o {
                            return Err(self.type_error("Cyclic __proto__ value"));
                        }
                        cur = self.heap.get(c).proto;
                    }
                    self.heap.get_mut(*o).proto = Some(*p);
                },
                Value::Null => self.heap.get_mut(*o).proto = None,
                _ => {},
            }
        }
        Ok(())
    }

    fn find_slot_or_host(&mut self, g: ObjRef, key: Key) -> Result<Option<Lookup>, Value> {
        if let Kind::Host(..) = self.heap.get(g).kind {
            if let Some(s) = self.heap.get(g).props.get(self.key_atom_ro(key)).cloned() {
                return Ok(Some(match s {
                    Slot::Data(v, _) => Lookup::Val(v),
                    Slot::Accessor(Some(f), _, _) => Lookup::Getter(f),
                    Slot::Accessor(None, _, _) => Lookup::Val(Value::Undefined),
                }));
            }
            match self.get_from(g, key) {
                Ok(Lookup::Val(Value::Undefined)) => {
                    // undefined from the host and nowhere else: is it there at all?
                    if self.has_property(g, key) {
                        return Ok(Some(Lookup::Val(Value::Undefined)));
                    }
                    return Ok(None);
                },
                Ok(l) => return Ok(Some(l)),
                Err(Fault::Throw(e)) => return Err(e),
                Err(Fault::NeedJs) => return Ok(None),
            }
        }
        Ok(match self.find_slot(g, key) {
            None => None,
            Some(Slot::Data(v, _)) => Some(Lookup::Val(v)),
            Some(Slot::Accessor(Some(f), _, _)) => Some(Lookup::Getter(f)),
            Some(Slot::Accessor(None, _, _)) => Some(Lookup::Val(Value::Undefined)),
        })
    }

    fn key_atom_ro(&self, key: Key) -> Atom {
        match key {
            Key::Atom(a) => a,
            Key::Index(i) => {
                if (i as usize) < self.int_atoms.len() {
                    self.int_atoms[i as usize]
                } else {
                    Atom(u32::MAX)
                }
            },
        }
    }

    fn for_in_keys(&mut self, o: ObjRef) -> Vec<Value> {
        let mut out: Vec<Value> = Vec::new();
        let mut seen: alloc::collections::BTreeSet<Vec<u16>> = alloc::collections::BTreeSet::new();
        let mut cur = Some(o);
        while let Some(c) = cur {
            let all = self.own_keys(c, false);
            let en = self.own_keys(c, true);
            for kv in all {
                if let Value::Str(s) = &kv {
                    if seen.insert(s.to_vec()) && en.iter().any(|e| e.strict_eq(&kv)) {
                        out.push(kv);
                    }
                }
            }
            cur = self.heap.get(c).proto;
        }
        out
    }

    /// === (BigInts by value)
    pub fn strict_equals(&self, a: &Value, b: &Value) -> bool {
        if a.strict_eq(b) {
            return true;
        }
        if let (Value::Obj(_), Value::Obj(_)) = (a, b) {
            if let (Some(x), Some(y)) = (self.bigint_of(a), self.bigint_of(b)) {
                return x == y;
            }
        }
        false
    }

    pub fn loose_eq(&mut self, a: &Value, b: &Value) -> NResult<bool> {
        if self.bigint_of(a).is_some() || self.bigint_of(b).is_some() {
            let (x, y) = (self.bigint_of(a), self.bigint_of(b));
            return Ok(match (x, y) {
                (Some(x), Some(y)) => x == y,
                (Some(x), None) | (None, Some(x)) => {
                    let other = if self.bigint_of(a).is_some() { b } else { a };
                    match other {
                        Value::Num(n) => x.cmp_f64(*n) == Some(core::cmp::Ordering::Equal),
                        Value::Str(s) => crate::bigint::BigInt::parse(&strs::to_rust(s)).is_some_and(|y| *x == y),
                        Value::Bool(t) => *x == crate::bigint::BigInt::from_i64(*t as i64),
                        _ => false,
                    }
                },
                _ => false,
            });
        }
        Ok(match (a, b) {
            (Value::Undefined | Value::Null | Value::Empty, Value::Undefined | Value::Null | Value::Empty) => true,
            (Value::Undefined | Value::Null | Value::Empty, _) | (_, Value::Undefined | Value::Null | Value::Empty) => false,
            (Value::Num(x), Value::Num(y)) => x == y,
            (Value::Str(x), Value::Str(y)) => x == y,
            (Value::Bool(x), Value::Bool(y)) => x == y,
            (Value::Obj(x), Value::Obj(y)) => x == y,
            (Value::Num(x), Value::Str(s)) | (Value::Str(s), Value::Num(x)) => *x == crate::num::parse_number(s),
            (Value::Bool(x), other) | (other, Value::Bool(x)) => {
                let n = Value::Num(*x as u8 as f64);
                return self.loose_eq(&n, other);
            },
            (Value::Obj(_), other) | (other, Value::Obj(_)) => {
                let o = if let Value::Obj(_) = a { a } else { b };
                let p = self.to_primitive(o, true)?;
                return self.loose_eq(&p, other);
            },
        })
    }

    // ── display (console.log): never runs script ──

    pub fn display(&mut self, v: &Value) -> String {
        let mut seen = Vec::new();
        self.display_inner(v, 0, &mut seen)
    }

    fn display_inner(&mut self, v: &Value, depth: usize, seen: &mut Vec<ObjRef>) -> String {
        match v {
            Value::Str(s) => strs::to_rust(s),
            Value::Obj(o) => {
                let o = *o;
                if seen.contains(&o) {
                    return "[Circular]".into();
                }
                let kind_name = match &self.heap.get(o).kind {
                    Kind::Array(_) => "array",
                    Kind::Error => "error",
                    Kind::Closure(_) | Kind::Native(..) | Kind::Bound(_) | Kind::HostFn(_) => "function",
                    Kind::Host(..) => "host",
                    Kind::Date(_) => "date",
                    Kind::RegExp(_) => "regexp",
                    Kind::Symbol(_) => "symbol",
                    Kind::BigInt(_) => "bigint",
                    Kind::Map(_) => "map",
                    Kind::Set(_) => "set",
                    Kind::Generator(_) => "generator",
                    Kind::Proxy(_) => "proxy",
                    _ => "object",
                };
                match kind_name {
                    "error" => {
                        let name = match self.get(&Value::Obj(o), Key::Atom(k::name)) {
                            Ok(Value::Str(s)) => strs::to_rust(&s),
                            _ => "Error".into(),
                        };
                        let msg = match self.get(&Value::Obj(o), Key::Atom(k::message)) {
                            Ok(Value::Str(s)) => strs::to_rust(&s),
                            _ => String::new(),
                        };
                        if msg.is_empty() { name } else { format!("{}: {}", name, msg) }
                    },
                    "function" => {
                        let n = match self.get_own(o, Key::Atom(k::name)) {
                            Some(Slot::Data(Value::Str(s), _)) => strs::to_rust(&s),
                            _ => String::new(),
                        };
                        format!("[Function: {}]", if n.is_empty() { "(anonymous)" } else { &n })
                    },
                    "host" => {
                        let c = strs::to_rust(self.atoms.name(self.heap.get(o).class));
                        format!("[object {}]", c)
                    },
                    "symbol" => {
                        let a = self.sym_of(v).unwrap();
                        format!("Symbol({})", strs::to_rust(self.atoms.name(a)))
                    },
                    "bigint" => format!("{}n", self.bigint_of(v).unwrap().to_string_radix(10)),
                    "generator" => "Object [Generator] {}".into(),
                    "proxy" => "[object Proxy]".into(),
                    "map" | "set" => {
                        let entries: Vec<(Value, Value)> = match &self.heap.get(o).kind {
                            Kind::Map(m) | Kind::Set(m) => m.entries.iter().filter(|e| !matches!(e.0, Value::Empty)).take(50).cloned().collect(),
                            _ => vec![],
                        };
                        if depth > 2 {
                            return format!("[{}]", if kind_name == "map" { "Map" } else { "Set" });
                        }
                        seen.push(o);
                        let mut parts = Vec::new();
                        for (k2, v2) in entries {
                            let ks = self.display_inner(&k2, depth + 1, seen);
                            if kind_name == "map" {
                                let vs = self.display_inner(&v2, depth + 1, seen);
                                parts.push(format!("{} => {}", ks, vs));
                            } else {
                                parts.push(ks);
                            }
                        }
                        seen.pop();
                        format!("{}({}) {{{}}}", if kind_name == "map" { "Map" } else { "Set" }, parts.len(), parts.join(", "))
                    },
                    "date" | "regexp" => match self.to_str(v) {
                        Ok(s) => strs::to_rust(&s),
                        Err(_) => "[object]".into(),
                    },
                    _ => {
                        if depth > 2 {
                            return if kind_name == "array" { "[Array]".into() } else { "[Object]".into() };
                        }
                        seen.push(o);
                        let keys = self.own_keys(o, true);
                        let mut parts = Vec::new();
                        for kv in keys.iter().take(50) {
                            let ks = match kv {
                                Value::Str(s) => s.clone(),
                                _ => continue,
                            };
                            let key = self.key_of_str(&ks);
                            let val = match self.get_own(o, key) {
                                Some(Slot::Data(x, _)) => x,
                                Some(_) => Value::str("[Getter/Setter]"),
                                None => Value::Undefined,
                            };
                            let shown = match &val {
                                Value::Str(s) if depth >= 0 => format!("'{}'", strs::to_rust(s)),
                                other => self.display_inner(other, depth + 1, seen),
                            };
                            if kind_name == "array" {
                                parts.push(shown);
                            } else {
                                parts.push(format!("{}: {}", strs::to_rust(&ks), shown));
                            }
                        }
                        seen.pop();
                        if kind_name == "array" {
                            format!("[{}]", parts.join(", "))
                        } else {
                            format!("{{{}}}", parts.join(", "))
                        }
                    },
                }
            },
            other => match self.to_str(other) {
                Ok(s) => strs::to_rust(&s),
                Err(_) => String::new(),
            },
        }
    }
}

enum Lookup {
    Val(Value),
    Getter(ObjRef),
    Host(HostReq),
    /// call this (a proxy trap): f, this, arguments
    Call(Value, Value, Vec<Value>),
}

enum SetOut {
    Done,
    Setter(ObjRef),
    Host(HostReq),
    Call(Value, Value, Vec<Value>),
}

fn arith(op: Op, x: f64, y: f64) -> f64 {
    use crate::num::{to_int32, to_uint32};
    match op {
        Op::Sub => x - y,
        Op::Mul => x * y,
        Op::Div => x / y,
        Op::Mod => {
            if y == 0.0 || x.is_infinite() || x.is_nan() || y.is_nan() {
                f64::NAN
            } else if y.is_infinite() {
                x
            } else {
                libm::fmod(x, y)
            }
        },
        Op::Exp => {
            if y.is_nan() || ((x == 1.0 || x == -1.0) && y.is_infinite()) {
                f64::NAN
            } else {
                libm::pow(x, y)
            }
        },
        Op::Shl => (to_int32(x).wrapping_shl(to_uint32(y) & 31)) as f64,
        Op::Shr => (to_int32(x) >> (to_uint32(y) & 31)) as f64,
        Op::UShr => (to_uint32(x) >> (to_uint32(y) & 31)) as f64,
        Op::BitAnd => (to_int32(x) & to_int32(y)) as f64,
        Op::BitOr => (to_int32(x) | to_int32(y)) as f64,
        Op::BitXor => (to_int32(x) ^ to_int32(y)) as f64,
        _ => f64::NAN,
    }
}

fn line_of(f: &FuncProto, ip: usize) -> u32 {
    let mut line = 0;
    for (at, l) in &f.lines {
        if *at as usize > ip {
            break;
        }
        line = *l;
    }
    line
}

#[allow(dead_code)]
fn _unused() -> Value {
    let _ = err_slot();
    let _: Option<Box<u8>> = None;
    let _ = ToString::to_string("");
    Value::Undefined
}

impl Engine {
    /// `new` on a proxy: its construct trap, or the target
    pub(crate) fn proxy_construct(&mut self, p: ObjRef, args: Vec<Value>, nt: Value, cont: Cont) -> Result<Option<Value>, Value> {
        let (target, handler) = match &self.heap.get(p).kind {
            Kind::Proxy(d) => (d.target, d.handler),
            _ => (None, None),
        };
        let (Some(t), Some(h)) = (target, handler) else { return Err(self.type_error("Cannot perform 'construct' on a proxy that has been revoked")) };
        let nt = if nt.strict_eq(&Value::Obj(p)) { Value::Obj(t) } else { nt };
        let ca = self.atom("construct");
        let trap = match self.get(&Value::Obj(h), Key::Atom(ca)) {
            Ok(v) => v,
            Err(Fault::Throw(e)) => return Err(e),
            Err(Fault::NeedJs) => Value::Undefined,
        };
        if trap.is_nullish() {
            return self.construct_nt(Value::Obj(t), args, nt, cont);
        }
        let arr = self.new_array(args);
        self.call_value(trap, Value::Obj(h), vec![Value::Obj(t), Value::Obj(arr), nt], cont, false)
    }
}

impl Engine {
    /// a proxy's (target, handler); Err for a revoked one, Ok(None) for anything else
    pub(crate) fn proxy_parts(&mut self, o: ObjRef) -> NResult<Option<(ObjRef, ObjRef)>> {
        match &self.heap.get(o).kind {
            Kind::Proxy(p) => match (p.target, p.handler) {
                (Some(t), Some(h)) => Ok(Some((t, h))),
                _ => self.throw_type("Cannot perform an operation on a proxy that has been revoked"),
            },
            _ => Ok(None),
        }
    }

    /// the handler's trap (None when it has none)
    pub(crate) fn proxy_trap(&mut self, h: ObjRef, name: &str) -> NResult<Option<Value>> {
        let a = self.atom(name);
        let f = match self.get_value(&Value::Obj(h), Key::Atom(a))? {
            Lookup::Val(v) => v,
            _ => return Err(Fault::NeedJs),
        };
        if f.is_nullish() {
            return Ok(None);
        }
        if !self.is_callable(&f) {
            return self.throw_type(&format!("proxy trap '{}' is not a function", name));
        }
        Ok(Some(f))
    }
}

impl Engine {
    /// a binary operator with a BigInt operand (both must be BigInts)
    fn bigint_binop(&mut self, op: Op, a: &Value, b: &Value) -> Result<Value, Value> {
        use crate::bigint::BigInt;
        let (Some(x), Some(y)) = (self.bigint_of(a), self.bigint_of(b)) else {
            return Err(self.type_error("Cannot mix BigInt and other types, use explicit conversions"));
        };
        let small = |e: &mut Self, v: &BigInt| -> Result<i64, Value> {
            let f = v.to_f64();
            if f.abs() > 1e9 {
                return Err(e.range_error("Maximum BigInt size exceeded"));
            }
            Ok(f as i64)
        };
        let r = match op {
            Op::Add => x.add(&y),
            Op::Sub => x.sub(&y),
            Op::Mul => x.mul(&y),
            Op::Div | Op::Mod => match x.divrem(&y) {
                Some((q, r)) => {
                    if matches!(op, Op::Div) {
                        q
                    } else {
                        r
                    }
                },
                None => return Err(self.range_error("Division by zero")),
            },
            Op::Exp => {
                if y.neg {
                    return Err(self.range_error("Exponent must be non-negative"));
                }
                let n = small(self, &y)?;
                if n > 100_000 {
                    return Err(self.range_error("Maximum BigInt size exceeded"));
                }
                x.pow(n as u64)
            },
            Op::BitAnd => x.bitop(&y, 0),
            Op::BitOr => x.bitop(&y, 1),
            Op::BitXor => x.bitop(&y, 2),
            Op::Shl => {
                let n = small(self, &y)?;
                x.shl(n)
            },
            Op::Shr => {
                let n = small(self, &y)?;
                x.shr(n)
            },
            _ => return Err(self.type_error("BigInts have no unsigned right shift, use >> instead")),
        };
        Ok(self.new_bigint(r))
    }

    /// < <= > >= with a BigInt on either side
    fn bigint_compare(&mut self, op: Op, a: &Value, b: &Value) -> Result<bool, Value> {
        use core::cmp::Ordering;
        let to_big = |e: &mut Self, v: &Value| -> Option<crate::bigint::BigInt> {
            match v {
                Value::Str(s) => crate::bigint::BigInt::parse(&strs::to_rust(s)),
                _ => e.bigint_of(v).map(|b| (*b).clone()),
            }
        };
        let ord: Option<Ordering> = match (self.bigint_of(a), self.bigint_of(b)) {
            (Some(x), Some(y)) => Some(x.cmp(&y)),
            (Some(x), None) => match b {
                Value::Str(_) => to_big(self, b).map(|y| x.cmp(&y)),
                _ => {
                    let n = self.to_number(b).map_err(|f| match f {
                        Fault::Throw(e) => e,
                        Fault::NeedJs => Value::Undefined,
                    })?;
                    x.cmp_f64(n)
                },
            },
            (None, Some(y)) => match a {
                Value::Str(_) => to_big(self, a).map(|x| x.cmp(&y)),
                _ => {
                    let n = self.to_number(a).map_err(|f| match f {
                        Fault::Throw(e) => e,
                        Fault::NeedJs => Value::Undefined,
                    })?;
                    y.cmp_f64(n).map(|o| o.reverse())
                },
            },
            _ => None,
        };
        Ok(match ord {
            None => false,
            Some(o) => match op {
                Op::Lt => o == Ordering::Less,
                Op::Le => o != Ordering::Greater,
                Op::Gt => o == Ordering::Greater,
                _ => o != Ordering::Less,
            },
        })
    }
}
