//! The compiler: syntax tree -> stack bytecode.
//!
//! Variables are resolved at compile time. A binding nothing else captures lives in its frame's
//! local slots; one an inner function refers to lives in an *environment* (a heap object holding
//! the scope's captured slots, linked to the scope around it) and is addressed by (depth, slot).
//! Names that resolve nowhere are global object properties.

use crate::ast::*;
use crate::atoms::{Atom, Atoms};
use crate::strs::{js, JsStr};
use crate::value::Value;
use crate::num::F64Ext;
use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::format;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Op {
    Const(u32),
    Int(i32),
    Undef,
    Null,
    True,
    False,
    EmptyVal,
    Pop,
    Dup,
    /// a b -> a b a b
    Dup2,
    Swap,
    /// a b c -> c a b
    Rot3,

    GetLocal(u32),
    SetLocal(u32),
    /// a `let` read: throws while it is still uninitialised
    GetLocalChk(u32, Atom),
    /// assigning a let: throws while uninitialised
    SetLocalChk(u32, Atom),
    ClearLocal(u32),
    GetEnv(u16, u16),
    SetEnv(u16, u16),
    GetEnvChk(u16, u16, Atom),
    SetEnvChk(u16, u16, Atom),
    /// a new environment with n slots (slots >= the second operand start uninitialised)
    PushEnv(u16, u16),
    PopEnv,
    /// a loop's next iteration gets a copy of the scope (closures keep their own `let`)
    RefreshEnv,

    GetGlobal(Atom),
    SetGlobal(Atom),
    TypeofGlobal(Atom),
    DeclareGlobal(Atom),
    /// a script's top-level let / const / class (the bool: const), uninitialised
    DeclareLex(Atom, bool),
    /// its initialisation
    InitLex(Atom),
    DeleteGlobal(Atom),
    ThrowConst(Atom),

    GetProp(Atom),
    /// obj -> obj value (a method call keeps its `this`)
    GetPropKeep(Atom),
    SetProp(Atom),
    GetElem,
    GetElemKeep,
    SetElem,
    DelProp(Atom),
    DelElem,
    InitProp(Atom),
    InitElem,
    InitGetter(Atom),
    InitSetter(Atom),
    InitGetterElem,
    InitSetterElem,

    /// f a1..an -> result
    Call(u32),
    /// this f a1..an -> result
    CallMethod(u32),
    New(u32),
    /// f args-array -> result ; this f args-array -> result ; f args-array -> object
    CallSpread,
    CallMethodSpread,
    NewSpread,
    Return,
    Throw,
    This,
    NewTarget,

    Jump(u32),
    JumpIfFalse(u32),
    JumpIfTrue(u32),
    /// && / ||: keep the value and jump, or pop it and go on
    JumpIfFalseKeep(u32),
    JumpIfTrueKeep(u32),
    /// ??: keep a non-nullish value and jump
    JumpIfNotNullishKeep(u32),
    JumpIfNotUndefined(u32),
    /// optional chain: a nullish value becomes undefined and jumps to the chain's end
    OptCheck(u32),
    /// this f: a nullish f drops both and jumps with undefined
    OptCheckCall(u32),

    Neg,
    Plus,
    Not,
    BitNot,
    Typeof,
    Inc,
    Dec,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Exp,
    Shl,
    Shr,
    UShr,
    BitAnd,
    BitOr,
    BitXor,
    Eq,
    Ne,
    StrictEq,
    StrictNe,
    Lt,
    Le,
    Gt,
    Ge,
    In,
    InstanceOf,
    ToNumber,
    ToStr,
    ToKey,

    Closure(u32),
    NewObject,
    NewArray(u32),
    ArrayPush,
    ArrayHole,
    /// arr src -> arr (src an array: its items appended)
    ArrayAppend,
    RegExp(u32),
    /// the engine's own JavaScript helpers (prelude)
    Intrinsic(u32),
    Arguments,
    /// the arguments from the n-th on, as an array
    Rest(u32),
    ForInStart,
    /// iter -> iter key, or jump when done
    ForInNext(u32),
    EnterTry(u32),
    LeaveTry,
    CheckObjCoercible,
    Debugger,
    /// throw a TypeError / ReferenceError with this message
    ThrowMsg(u32, u8),
    /// a try whose handler is a finally block
    EnterFinally(u32),
    /// a generator / async function: stop here and hand back the generator object (or promise)
    GenStart,
    /// v -> (resumed value): 0 yield, 1 a yield* step (pushes value, mode), 2 await
    Yield(u8),
    /// [parent] -> F proto: a class constructor from funcs[i] (bool: there is a parent)
    MakeClass(u32, bool),
    /// obj fn -> obj: a class method (0) / getter (1) / setter (2), not enumerable; sets its home
    DefineMethod(Atom, u8),
    /// obj key fn -> obj
    DefineMethodElem(u8),
    /// obj fn -> obj fn: fn's home object (for super) is obj
    SetHome,
    /// o -> bool: `#x in o`
    HasPrivate(Atom),
    /// F init -> F: the instance field initialiser
    SetFields,
    /// init keys -> init: the computed field keys it reads
    SetFieldKeys,
    /// the running field initialiser's n-th computed key
    FieldKey(u32),
    /// this v -> undefined: define a field
    DefineField(Atom),
    /// this key v -> undefined
    DefineFieldElem,
    /// this ctor -> this: run ctor's field initialiser on this
    InitFields,
    /// parent newTarget a1..an -> object
    SuperCall(u32),
    /// parent newTarget args -> object
    SuperCallSpread,
    /// o -> its prototype
    GetProtoOf,
    /// this -> value (keep: this value): a property of the home object's prototype
    SuperGet(Atom, bool),
    /// this key -> value (keep: this value)
    SuperGetElem(bool),
    /// a BigInt literal (the const is its decimal digits)
    BigInt(u32),
}

#[derive(Debug)]
pub struct FuncProto {
    pub name: JsStr,
    pub length: u32,
    pub nparams: u32,
    pub nlocals: u32,
    pub code: Vec<Op>,
    pub consts: Vec<Value>,
    pub funcs: Vec<Rc<FuncProto>>,
    pub regexes: Vec<(JsStr, JsStr)>,
    pub lines: Vec<(u32, u32)>,
    /// what each call site calls ("a.b.c", "f"), for "... is not a function"
    pub call_names: Vec<(u32, JsStr)>,
    pub strict: bool,
    pub arrow: bool,
    pub method: bool,
    pub kind: FuncKind,
    /// keep every argument (for `arguments` / rest parameters)
    pub keep_args: bool,
    pub is_script: bool,
    pub src: Option<(Rc<str>, usize, usize)>,
    pub is_async: bool,
    pub generator: bool,
}

#[derive(Clone, Copy, Debug)]
enum Binding {
    Local(u32, VarKind),
    Env(u16, VarKind),
}

struct Scope {
    names: Vec<(Name, Binding)>,
    has_env: bool,
    env_size: u16,
    env_tdz_from: u16,
    function: bool,
}

#[derive(Default)]
struct Ctl {
    labels: Vec<Name>,
    is_loop: bool,
    is_switch: bool,
    breaks: Vec<usize>,
    continues: Vec<usize>,
    /// how many env scopes were open where this target is (break)
    env_depth: usize,
    /// ... and where a continue lands
    cont_depth: usize,
    /// how many scopes were open (a finally block is compiled against these)
    scope_len: usize,
    /// a try with a finally block (inlined on every exit through it)
    finally: Option<Vec<Stmt>>,
    /// a try / catch handler to leave on the way out
    handler: bool,
}

struct FnCtx {
    code: Vec<Op>,
    consts: Vec<Value>,
    const_map: BTreeMap<ConstKey, u32>,
    funcs: Vec<Rc<FuncProto>>,
    regexes: Vec<(JsStr, JsStr)>,
    lines: Vec<(u32, u32)>,
    call_names: Vec<(u32, JsStr)>,
    nlocals: u32,
    free_temps: Vec<u32>,
    scopes: Vec<Scope>,
    ctl: Vec<Ctl>,
    captured: BTreeSet<Name>,
    is_script: bool,
    arrow: bool,
    strict: bool,
    opt_jumps: Vec<Vec<usize>>,
    keep_args: bool,
    /// an async generator (its yields await first)
    async_gen: bool,
    /// a derived class constructor (`this` is a binding super() initialises)
    derived: bool,
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum ConstKey {
    Num(u64),
    Str(Vec<u16>),
}

pub struct Compiler<'a> {
    pub atoms: &'a mut Atoms,
    fns: Vec<FnCtx>,
    source: Rc<str>,
    free_cache: BTreeMap<usize, Rc<BTreeSet<Name>>>,
    /// labels waiting for the loop they name
    pending: Vec<Name>,
    /// the script's completion value slot (expression statements outside functions store there)
    completion: Option<u32>,
    /// the script's top-level lexical names
    script_lex: BTreeSet<Name>,
    pending_call_name: Option<String>,
    /// the private names (`#x`) of the classes being compiled, innermost last
    privates: Vec<BTreeMap<String, Atom>>,
    /// a module's imported names: (the namespace parameter, the export name; None = the namespace)
    pub module_imports: BTreeMap<String, (Name, Option<Name>)>,
}

#[derive(Debug)]
pub struct CompileError(pub String);

type CResult<T> = Result<T, CompileError>;

fn cerr<T>(m: &str) -> CResult<T> {
    Err(CompileError(m.into()))
}

/// intrinsic ids (prelude functions the compiled code calls)
pub mod intr {
    pub const ITER_TO_ARRAY: u32 = 0;
    pub const GET_ITERATOR: u32 = 1;
    pub const COPY_PROPS: u32 = 2;
    pub const OBJECT_REST: u32 = 3;
    pub const TEMPLATE_OBJECT: u32 = 4;
    pub const ITER_CLOSE: u32 = 5;
    pub const RETRY_PRIM: u32 = 6;
    /// drives an async function's generator; returns its promise
    pub const ASYNC_RUN: u32 = 7;
    /// wraps an async generator's generator object
    pub const ASYNC_GEN: u32 = 8;
    /// (iterator, mode, value) -> the inner result for a yield* step
    pub const YIELD_STEP: u32 = 9;
    pub const GET_ASYNC_ITERATOR: u32 = 10;
    /// import(): a rejected promise
    pub const DYN_IMPORT: u32 = 11;
    /// a for-await / async yield* result object check: (r) -> r
    pub const CHECK_RESULT: u32 = 12;
    /// (source, url): load, link and evaluate a module
    pub const MODULE_EVAL: u32 = 13;
    pub const COUNT: u32 = 14;
}

impl<'a> Compiler<'a> {
    pub fn new(atoms: &'a mut Atoms, source: &str) -> Self {
        Compiler { atoms, fns: Vec::new(), source: Rc::from(source), free_cache: BTreeMap::new(), pending: Vec::new(), completion: None, script_lex: BTreeSet::new(), pending_call_name: None, privates: Vec::new(), module_imports: BTreeMap::new() }
    }

    fn atom(&mut self, n: &str) -> Atom {
        if n.starts_with('#') {
            for m in self.privates.iter().rev() {
                if let Some(a) = m.get(n) {
                    return *a;
                }
            }
        }
        self.atoms.intern_str(n)
    }

    /// is `this` a binding here (inside a derived constructor, or an arrow in one)?
    fn this_is_binding(&self) -> bool {
        for f in self.fns.iter().rev() {
            if f.derived {
                return true;
            }
            if !f.arrow || f.is_script {
                return false;
            }
        }
        false
    }

    fn emit_this(&mut self) {
        if self.this_is_binding() {
            let n: Name = Rc::from("this");
            self.emit_get(&n);
        } else {
            self.emit(Op::This);
        }
    }

    fn f(&mut self) -> &mut FnCtx {
        self.fns.last_mut().unwrap()
    }

    fn emit(&mut self, op: Op) -> usize {
        let c = &mut self.f().code;
        c.push(op);
        c.len() - 1
    }

    fn here(&mut self) -> u32 {
        self.f().code.len() as u32
    }

    fn patch(&mut self, at: usize, to: u32) {
        let c = &mut self.f().code;
        c[at] = match c[at] {
            Op::Jump(_) => Op::Jump(to),
            Op::JumpIfFalse(_) => Op::JumpIfFalse(to),
            Op::JumpIfTrue(_) => Op::JumpIfTrue(to),
            Op::JumpIfFalseKeep(_) => Op::JumpIfFalseKeep(to),
            Op::JumpIfTrueKeep(_) => Op::JumpIfTrueKeep(to),
            Op::JumpIfNotNullishKeep(_) => Op::JumpIfNotNullishKeep(to),
            Op::JumpIfNotUndefined(_) => Op::JumpIfNotUndefined(to),
            Op::OptCheck(_) => Op::OptCheck(to),
            Op::OptCheckCall(_) => Op::OptCheckCall(to),
            Op::ForInNext(_) => Op::ForInNext(to),
            Op::EnterTry(_) => Op::EnterTry(to),
            Op::EnterFinally(_) => Op::EnterFinally(to),
            other => other,
        };
    }

    fn patch_here(&mut self, at: usize) {
        let h = self.here();
        self.patch(at, h);
    }

    fn konst(&mut self, v: Value) -> u32 {
        let key = match &v {
            Value::Num(n) => ConstKey::Num(n.to_bits()),
            Value::Str(s) => ConstKey::Str(s.to_vec()),
            _ => unreachable!(),
        };
        if let Some(i) = self.f().const_map.get(&key) {
            return *i;
        }
        let f = self.f();
        f.consts.push(v);
        let i = f.consts.len() as u32 - 1;
        f.const_map.insert(key, i);
        i
    }

    fn emit_num(&mut self, n: f64) {
        if n.fract() == 0.0 && n.abs() < 1e9 && !(n == 0.0 && n.is_sign_negative()) {
            self.emit(Op::Int(n as i32));
        } else {
            let k = self.konst(Value::Num(n));
            self.emit(Op::Const(k));
        }
    }

    fn emit_str(&mut self, s: JsStr) {
        let k = self.konst(Value::Str(s));
        self.emit(Op::Const(k));
    }

    fn temp(&mut self) -> u32 {
        let f = self.f();
        if let Some(t) = f.free_temps.pop() {
            return t;
        }
        f.nlocals += 1;
        f.nlocals - 1
    }
    fn free_temp(&mut self, t: u32) {
        self.f().free_temps.push(t);
    }
    fn new_local(&mut self) -> u32 {
        let f = self.f();
        f.nlocals += 1;
        f.nlocals - 1
    }

    fn env_depth(&self) -> usize {
        self.fns.last().unwrap().scopes.iter().filter(|s| s.has_env).count()
    }

    // ── free variables (what a function refers to from outside itself) ──

    fn free_vars(&mut self, f: &Rc<FuncNode>) -> Rc<BTreeSet<Name>> {
        let key = Rc::as_ptr(f) as usize;
        if let Some(s) = self.free_cache.get(&key) {
            return s.clone();
        }
        let mut w = FreeWalker { scopes: Vec::new(), free: BTreeSet::new(), comp: self };
        let mut top = BTreeSet::new();
        for p in &f.params {
            let mut ns = Vec::new();
            p.pat.names(&mut ns);
            top.extend(ns);
        }
        if let Some(n) = &f.name {
            // only a function expression binds its own name (not a method, getter, arrow or class constructor)
            if !f.decl && !f.method && !f.arrow {
                top.insert(n.clone());
            }
        }
        if !f.arrow {
            top.insert(Rc::from("arguments"));
            top.insert(Rc::from("this"));
            top.insert(Rc::from("%nt"));
            top.insert(Rc::from("%ctor"));
        }
        let mut vars = Vec::new();
        var_names(&f.body, &mut vars, true);
        top.extend(vars);
        w.scopes.push(top);
        for p in &f.params {
            if let Some(d) = &p.default {
                w.expr(d);
            }
            w.pattern_exprs(&p.pat);
        }
        w.block(&f.body);
        let s = Rc::new(w.free);
        self.free_cache.insert(key, s.clone());
        s
    }

    /// the names inner functions of this body capture
    fn captured_in(&mut self, body: &[Stmt], params: &[Param]) -> BTreeSet<Name> {
        let mut inner: Vec<Rc<FuncNode>> = Vec::new();
        let mut classes: Vec<Rc<ClassNode>> = Vec::new();
        for p in params {
            if let Some(d) = &p.default {
                collect_fns_expr(d, &mut inner, &mut classes);
            }
            collect_fns_pat(&p.pat, &mut inner, &mut classes);
        }
        for s in body {
            collect_fns_stmt(s, &mut inner, &mut classes);
        }
        let mut out = BTreeSet::new();
        for f in &inner {
            let fv = self.free_vars(f);
            out.extend(fv.iter().cloned());
        }
        for c in &classes {
            for f in class_fns(c) {
                let fv = self.free_vars(&f);
                out.extend(fv.iter().cloned());
            }
            if let Some(n) = &c.name {
                out.insert(n.clone());
            }
        }
        out
    }

    // ── scopes and names ──

    fn declare(&mut self, name: &Name, kind: VarKind) -> Binding {
        let captured = self.f().captured.contains(name);
        let is_fn_scope = self.f().scopes.last().unwrap().function;
        let _ = is_fn_scope;
        let b = if captured {
            let s = self.f().scopes.last_mut().unwrap();
            s.has_env = true;
            let slot = s.env_size;
            s.env_size += 1;
            Binding::Env(slot, kind)
        } else {
            Binding::Local(self.new_local(), kind)
        };
        self.f().scopes.last_mut().unwrap().names.push((name.clone(), b));
        b
    }

    fn lookup_here(&self, name: &str) -> Option<Binding> {
        let s = self.fns.last().unwrap().scopes.last().unwrap();
        s.names.iter().rev().find(|(n, _)| &**n == name).map(|(_, b)| *b)
    }

    /// where a name lives: Some((binding, env depth)) or None for a global
    fn resolve(&self, name: &str) -> Option<(Binding, u16)> {
        let mut depth: u16 = 0;
        for (fi, f) in self.fns.iter().enumerate().rev() {
            let innermost_fn = fi == self.fns.len() - 1;
            for s in f.scopes.iter().rev() {
                if let Some((_, b)) = s.names.iter().rev().find(|(n, _)| &**n == name) {
                    match b {
                        Binding::Local(..) if innermost_fn => return Some((*b, 0)),
                        Binding::Local(..) => return None, // (not captured: cannot happen)
                        Binding::Env(..) => return Some((*b, depth)),
                    }
                }
                if s.has_env {
                    depth += 1;
                }
            }
            if f.is_script {
                break;
            }
        }
        None
    }

    fn emit_get(&mut self, name: &Name) {
        if !self.module_imports.is_empty() && self.resolve(name).is_none() {
            if let Some((p, key)) = self.module_imports.get(&**name).cloned() {
                self.emit_get(&p);
                if let Some(k) = key {
                    let a = self.atom(&k);
                    self.emit(Op::GetProp(a));
                }
                return;
            }
        }
        match self.resolve(name) {
            Some((Binding::Local(slot, k), _)) => {
                if k == VarKind::Var {
                    self.emit(Op::GetLocal(slot));
                } else {
                    let a = self.atom(name);
                    self.emit(Op::GetLocalChk(slot, a));
                }
            },
            Some((Binding::Env(slot, k), d)) => {
                if k == VarKind::Var {
                    self.emit(Op::GetEnv(d, slot));
                } else {
                    let a = self.atom(name);
                    self.emit(Op::GetEnvChk(d, slot, a));
                }
            },
            None => {
                let a = self.atom(name);
                self.emit(Op::GetGlobal(a));
            },
        }
    }

    /// store the top of the stack (left there) into a name
    fn emit_set(&mut self, name: &Name, init: bool) {
        if !self.module_imports.is_empty() && self.resolve(name).is_none() && self.module_imports.contains_key(&**name) {
            let a = self.atom(name);
            self.emit(Op::ThrowConst(a));
            return;
        }
        match self.resolve(name) {
            Some((Binding::Local(slot, k), _)) => {
                if k == VarKind::Const && !init {
                    let a = self.atom(name);
                    self.emit(Op::ThrowConst(a));
                } else if k == VarKind::Var || init {
                    self.emit(Op::SetLocal(slot));
                } else {
                    let a = self.atom(name);
                    self.emit(Op::SetLocalChk(slot, a));
                }
            },
            Some((Binding::Env(slot, k), d)) => {
                if k == VarKind::Const && !init {
                    let a = self.atom(name);
                    self.emit(Op::ThrowConst(a));
                } else if k == VarKind::Var || init {
                    self.emit(Op::SetEnv(d, slot));
                } else {
                    let a = self.atom(name);
                    self.emit(Op::SetEnvChk(d, slot, a));
                }
            },
            None => {
                let a = self.atom(name);
                if init && self.fns.len() == 1 && self.script_lex.contains(name) && self.f().scopes.len() == 1 {
                    self.emit(Op::InitLex(a));
                } else {
                    self.emit(Op::SetGlobal(a));
                }
            },
        }
    }

    fn push_scope(&mut self, function: bool) {
        self.f().scopes.push(Scope { names: Vec::new(), has_env: false, env_size: 0, env_tdz_from: 0, function });
    }

    /// open a block scope for its lexical declarations; returns the PushEnv position to patch
    fn enter_block(&mut self, body: &[Stmt]) -> CResult<Option<usize>> {
        self.push_scope(false);
        let push_at = self.emit(Op::PushEnv(0, 0));
        let mut lex: Vec<(Name, VarKind)> = Vec::new();
        for s in body {
            lexical_names(s, &mut lex, false);
        }
        for (n, k) in &lex {
            if self.lookup_here(n).is_some() {
                return Err(CompileError(format!("'{}' has already been declared", n)));
            }
            let b = self.declare(n, *k);
            if let Binding::Local(slot, _) = b {
                self.emit(Op::ClearLocal(slot));
            }
        }
        let s = self.f().scopes.last_mut().unwrap();
        if s.has_env {
            let size = s.env_size;
            self.f().code[push_at] = Op::PushEnv(size, 0);
            // block functions (strict) are initialised now
            self.hoist_block_functions(body, true)?;
            return Ok(Some(push_at));
        }
        // no env: drop the placeholder by turning it into a no-op jump
        let next = push_at as u32 + 1;
        self.f().code[push_at] = Op::Jump(next);
        self.hoist_block_functions(body, true)?;
        Ok(None)
    }

    fn leave_block(&mut self) {
        let s = self.f().scopes.pop().unwrap();
        if s.has_env {
            self.emit(Op::PopEnv);
        }
    }

    /// function declarations directly in a block: made when the block is entered
    fn hoist_block_functions(&mut self, body: &[Stmt], block: bool) -> CResult<()> {
        for s in body {
            let f = match s {
                Stmt::Function(f) => f,
                Stmt::Labeled(_, inner) => match &**inner {
                    Stmt::Function(f) => f,
                    _ => continue,
                },
                _ => continue,
            };
            let name = f.name.clone().unwrap();
            self.function_value(f, None)?;
            if block && !self.f().strict {
                // sloppy mode: the block's binding and the function's var both get it
                self.emit(Op::Dup);
                let local = self.lookup_here(&name);
                if let Some(Binding::Local(slot, _)) = local {
                    self.emit(Op::SetLocal(slot));
                } else if let Some(Binding::Env(slot, _)) = local {
                    self.emit(Op::SetEnv(0, slot));
                }
                self.emit(Op::Pop);
                // and the var in the function scope (resolve skipping this block)
                let scope = self.f().scopes.pop().unwrap();
                let had_env = scope.has_env;
                self.emit_set_from_outer_block(&name, had_env);
                self.f().scopes.push(scope);
            } else {
                self.emit_set(&name, true);
            }
            self.emit(Op::Pop);
        }
        Ok(())
    }

    /// (sloppy block function) store into the name as seen from outside the current block
    fn emit_set_from_outer_block(&mut self, name: &Name, block_has_env: bool) {
        match self.resolve(name) {
            Some((Binding::Local(slot, _), _)) => {
                self.emit(Op::SetLocal(slot));
            },
            Some((Binding::Env(slot, _), d)) => {
                let d = if block_has_env { d + 1 } else { d };
                self.emit(Op::SetEnv(d, slot));
            },
            None => {
                let a = self.atom(name);
                self.emit(Op::SetGlobal(a));
            },
        }
    }

    // ── programs and functions ──

    pub fn compile_script(mut self, prog: &crate::parser::Program) -> CResult<Rc<FuncProto>> {
        let captured = self.captured_in(&prog.body, &[]);
        self.fns.push(FnCtx::new(true, false, prog.strict, captured));
        self.push_scope(true);
        // declarations become global object properties
        let mut vars = Vec::new();
        var_names(&prog.body, &mut vars, true);
        let mut lex = Vec::new();
        for s in &prog.body {
            lexical_names(s, &mut lex, true);
        }
        for n in vars.iter() {
            let a = self.atom(n);
            self.emit(Op::DeclareGlobal(a));
        }
        for (n, k) in lex.iter() {
            let a = self.atom(n);
            self.emit(Op::DeclareLex(a, *k == VarKind::Const));
            self.script_lex.insert(n.clone());
        }
        for s in &prog.body {
            if let Stmt::Function(f) = s {
                self.function_value(f, None)?;
                let a = self.atom(f.name.as_ref().unwrap());
                self.emit(Op::SetGlobal(a));
                self.emit(Op::Pop);
            }
        }
        // the completion value of the script (for an embedder's eval)
        let result = self.new_local();
        self.emit(Op::Undef);
        self.emit(Op::SetLocal(result));
        self.emit(Op::Pop);
        self.completion = Some(result);
        for s in &prog.body {
            self.stmt(s)?;
        }
        self.completion = None;
        self.emit(Op::GetLocal(result));
        self.emit(Op::Return);
        let f = self.fns.pop().unwrap();
        let src = Some((self.source.clone(), 0, self.source.chars().count()));
        Ok(Rc::new(f.finish(js(""), 0, 0, false, false, FuncKind::Normal, true, src, false, false)))
    }

    /// compile a nested function and push it as a closure
    fn function_value(&mut self, node: &Rc<FuncNode>, name_hint: Option<&str>) -> CResult<()> {
        let proto = self.compile_function(node, name_hint)?;
        let f = self.f();
        f.funcs.push(proto);
        let i = f.funcs.len() as u32 - 1;
        self.emit(Op::Closure(i));
        Ok(())
    }

    fn compile_function(&mut self, node: &Rc<FuncNode>, name_hint: Option<&str>) -> CResult<Rc<FuncProto>> {
        let mut captured = self.captured_in(&node.body, &node.params);
        if matches!(node.params.first(), Some(Param { pat: Pattern::Ident(n), .. }) if &**n == "%ns") {
            // a module function: inner functions reach its namespaces and import.meta
            for p in &node.params {
                if let Pattern::Ident(n) = &p.pat {
                    captured.insert(n.clone());
                }
            }
        }
        let strict = node.strict;
        let simple_params = node.params.iter().all(|p| matches!(p.pat, Pattern::Ident(_)) && p.default.is_none() && !p.rest);
        let mut uses_args = false;
        if !node.arrow {
            // `arguments` used here (or by an arrow inside)
            let fv = self.free_vars(node);
            let _ = fv;
            uses_args = refers_to(&node.body, &node.params, "arguments");
            if captured.contains("arguments") {
                uses_args = true;
            }
        }
        // a named function expression can call itself by its name
        if let Some(n) = &node.name {
            if !node.decl && node.kind == FuncKind::Normal && !node.method {
                let _ = n;
            }
        }
        self.fns.push(FnCtx::new(false, node.arrow, strict, BTreeSet::new()));
        core::mem::swap(&mut self.f().captured, &mut captured);
        self.f().async_gen = node.is_async && node.generator;
        self.f().derived = node.kind == FuncKind::DerivedConstructor;
        if node.is_async && !node.generator {
            // an async function runs as a generator its driver steps; it starts at once
            self.emit(Op::GenStart);
        }
        let nparams = node.params.len() as u32;
        self.f().nlocals = nparams;
        let has_rest = node.params.iter().any(|p| p.rest);
        self.f().keep_args = uses_args || has_rest;

        // the function's own name, in a scope just outside its body (function expressions)
        let self_name = match &node.name {
            Some(n) if !node.decl && !node.method && !node.arrow => Some(n.clone()),
            _ => None,
        };
        if let Some(n) = &self_name {
            self.push_scope(true);
            let b = self.declare(n, VarKind::Var);
            let s = self.f().scopes.last().unwrap();
            if s.has_env {
                let size = s.env_size;
                self.emit(Op::PushEnv(size, size));
            }
            // the running function itself: the VM keeps it as `callee`
            self.emit(Op::Intrinsic(u32::MAX));
            match b {
                Binding::Local(slot, _) => {
                    self.emit(Op::SetLocal(slot));
                },
                Binding::Env(slot, _) => {
                    self.emit(Op::SetEnv(0, slot));
                },
            }
            self.emit(Op::Pop);
        }

        self.push_scope(true);
        let push_at = self.emit(Op::PushEnv(0, 0));
        // parameters: locals 0..n hold the arguments as they came
        let mut param_bindings = Vec::new();
        for (i, p) in node.params.iter().enumerate() {
            if let Pattern::Ident(n) = &p.pat {
                if simple_params {
                    if self.f().captured.contains(n) {
                        let b = self.declare(n, VarKind::Var);
                        param_bindings.push((i as u32, b));
                    } else {
                        self.f().scopes.last_mut().unwrap().names.push((n.clone(), Binding::Local(i as u32, VarKind::Var)));
                    }
                    continue;
                }
            }
            let _ = i;
        }
        if !simple_params {
            // parameter names are declared up front; defaults and patterns run in order
            let mut names = Vec::new();
            for p in &node.params {
                p.pat.names(&mut names);
            }
            for n in &names {
                if self.lookup_here(n).is_none() {
                    self.declare(n, VarKind::Var);
                }
            }
        }
        // `arguments`
        if uses_args && self.lookup_here("arguments").is_none() {
            let n: Name = Rc::from("arguments");
            self.declare(&n, VarKind::Var);
        }
        // vars and functions (hoisted)
        let mut vars = Vec::new();
        var_names(&node.body, &mut vars, true);
        for n in &vars {
            if self.lookup_here(n).is_none() {
                self.declare(n, VarKind::Var);
            }
        }
        let derived = node.kind == FuncKind::DerivedConstructor;
        if derived {
            for n in ["%ctor", "%nt"] {
                let n: Name = Rc::from(n);
                self.declare(&n, VarKind::Var);
            }
        }
        let tdz_from = self.f().scopes.last().unwrap().env_size;
        // top-level lexical declarations share the function scope
        let mut lex = Vec::new();
        if derived {
            lex.push((Rc::from("this"), VarKind::Let));
        }
        for s in &node.body {
            lexical_names(s, &mut lex, true);
        }
        for (n, k) in &lex {
            if let Some(b) = self.lookup_here(n) {
                if let Binding::Local(_, VarKind::Var) | Binding::Env(_, VarKind::Var) = b {
                    if vars.contains(n) || node.params.iter().any(|p| matches!(&p.pat, Pattern::Ident(x) if x == n)) {
                        return Err(CompileError(format!("'{}' has already been declared", n)));
                    }
                }
            }
            let b = self.declare(n, *k);
            if let Binding::Local(slot, _) = b {
                self.emit(Op::ClearLocal(slot));
            }
        }
        let s = self.f().scopes.last().unwrap();
        if s.has_env {
            let (size, _) = (s.env_size, 0);
            self.f().code[push_at] = Op::PushEnv(size, tdz_from);
        } else {
            let next = push_at as u32 + 1;
            self.f().code[push_at] = Op::Jump(next);
        }
        if derived {
            self.emit(Op::Intrinsic(u32::MAX));
            self.emit_set(&Rc::from("%ctor"), true);
            self.emit(Op::Pop);
            self.emit(Op::NewTarget);
            self.emit_set(&Rc::from("%nt"), true);
            self.emit(Op::Pop);
        }
        if node.kind == FuncKind::ClassConstructor {
            // the instance fields, before the body
            self.emit(Op::This);
            self.emit(Op::Intrinsic(u32::MAX));
            self.emit(Op::InitFields);
            self.emit(Op::Pop);
        }
        // captured simple params: copy into the environment
        for (i, b) in &param_bindings {
            if let Binding::Env(slot, _) = b {
                self.emit(Op::GetLocal(*i));
                self.emit(Op::SetEnv(0, *slot));
                self.emit(Op::Pop);
            }
        }
        if uses_args {
            self.emit(Op::Arguments);
            let n: Name = Rc::from("arguments");
            self.emit_set(&n, true);
            self.emit(Op::Pop);
        }
        // patterns, defaults and rest
        if !simple_params {
            for (i, p) in node.params.iter().enumerate() {
                if p.rest {
                    self.emit(Op::Rest(i as u32));
                } else {
                    self.emit(Op::GetLocal(i as u32));
                }
                if let Some(d) = &p.default {
                    let j = self.emit(Op::JumpIfNotUndefined(0));
                    self.emit(Op::Pop);
                    self.expr_named(d, pattern_name(&p.pat))?;
                    self.patch_here(j);
                }
                self.bind_pattern(&p.pat, true)?;
            }
        }
        self.hoist_block_functions(&node.body, false)?;
        if node.generator {
            self.emit(Op::GenStart);
        }
        for s in &node.body {
            self.stmt(s)?;
        }
        if derived {
            self.emit_this();
        } else {
            self.emit(Op::Undef);
        }
        self.emit(Op::Return);
        let f = self.fns.pop().unwrap();
        let name = match (&node.name, name_hint) {
            (Some(n), _) => js(n),
            (None, Some(h)) => js(h),
            _ => js(""),
        };
        let name = match node.kind {
            FuncKind::Getter => js(&format!("get {}", crate::strs::to_rust(&name))),
            FuncKind::Setter => js(&format!("set {}", crate::strs::to_rust(&name))),
            _ => name,
        };
        let length = node.params.iter().take_while(|p| p.default.is_none() && !p.rest).count() as u32;
        let src = Some((self.source.clone(), node.src.0, node.src.1));
        Ok(Rc::new(f.finish(name, length, nparams, node.arrow, node.method, node.kind, false, src, node.is_async, node.generator)))
    }

    // ── statements ──

    fn stmts(&mut self, body: &[Stmt]) -> CResult<()> {
        for s in body {
            self.stmt(s)?;
        }
        Ok(())
    }

    fn block(&mut self, body: &[Stmt]) -> CResult<()> {
        self.enter_block(body)?;
        self.stmts(body)?;
        self.leave_block();
        Ok(())
    }

    fn stmt(&mut self, s: &Stmt) -> CResult<()> {
        match s {
            Stmt::Expr(e) => {
                self.expr(e)?;
                match self.completion {
                    Some(slot) if self.fns.len() == 1 => {
                        self.emit(Op::SetLocal(slot));
                    },
                    _ => {},
                }
                self.emit(Op::Pop);
            },
            Stmt::Var(kind, decls) => {
                for (pat, init) in decls {
                    match init {
                        Some(e) => {
                            self.expr_named(e, pattern_name(pat))?;
                            self.bind_pattern(pat, *kind != VarKind::Var || true)?;
                        },
                        None => {
                            if *kind != VarKind::Var {
                                // `let x;` initialises x to undefined
                                self.emit(Op::Undef);
                                self.bind_pattern(pat, true)?;
                            }
                        },
                    }
                }
            },
            Stmt::Function(_) => {}, // hoisted
            Stmt::Class(c) => {
                self.class(c)?;
                let n = c.name.clone().unwrap();
                self.emit_set(&n, true);
                self.emit(Op::Pop);
            },
            Stmt::Return(v) => {
                let derived = self.f().derived;
                match v {
                    Some(e) => {
                        self.expr(e)?;
                        if derived {
                            let j = self.emit(Op::JumpIfNotUndefined(0));
                            self.emit(Op::Pop);
                            self.emit_this();
                            self.patch_here(j);
                        }
                    },
                    None if derived => self.emit_this(),
                    None => {
                        self.emit(Op::Undef);
                    },
                }
                self.exit_to(None, true)?;
                self.emit(Op::Return);
            },
            Stmt::If(t, a, b) => {
                self.cond_jump(t)?;
                let j = self.emit(Op::JumpIfFalse(0));
                self.sub_stmt(a)?;
                if let Some(b) = b {
                    let j2 = self.emit(Op::Jump(0));
                    self.patch_here(j);
                    self.sub_stmt(b)?;
                    self.patch_here(j2);
                } else {
                    self.patch_here(j);
                }
            },
            Stmt::Block(b) => self.block(b)?,
            Stmt::Empty | Stmt::Debugger => {},
            Stmt::Throw(e) => {
                self.expr(e)?;
                self.emit(Op::Throw);
            },
            Stmt::While(t, body) => {
                let labels = self.take_labels();
                let top = self.here();
                self.cond_jump(t)?;
                let exit = self.emit(Op::JumpIfFalse(0));
                self.push_loop(labels);
                self.sub_stmt(body)?;
                let ctl = self.f().ctl.pop().unwrap();
                for c in ctl.continues {
                    self.patch(c, top);
                }
                self.emit(Op::Jump(top));
                self.patch_here(exit);
                for b in ctl.breaks {
                    self.patch_here(b);
                }
            },
            Stmt::DoWhile(body, t) => {
                let labels = self.take_labels();
                let top = self.here();
                self.push_loop(labels);
                self.sub_stmt(body)?;
                let ctl = self.f().ctl.pop().unwrap();
                let cont = self.here();
                for c in ctl.continues {
                    self.patch(c, cont);
                }
                self.cond_jump(t)?;
                self.emit(Op::JumpIfTrue(top));
                for b in ctl.breaks {
                    self.patch_here(b);
                }
            },
            Stmt::For(init, test, update, body) => self.for_stmt(init, test, update, body)?,
            Stmt::ForIn(head, obj, body) => self.for_in(head, obj, body)?,
            Stmt::ForOf(head, obj, body) => self.for_of(head, obj, body)?,
            Stmt::ForAwaitOf(head, obj, body) => self.for_await_of(head, obj, body)?,
            Stmt::Break(label) => {
                let target = self.find_ctl(label.as_deref(), false)?;
                self.exit_to(Some(target), false)?;
                let j = self.emit(Op::Jump(0));
                self.f().ctl[target].breaks.push(j);
            },
            Stmt::Continue(label) => {
                let target = self.find_ctl(label.as_deref(), true)?;
                self.exit_to_(Some(target), false, true)?;
                let j = self.emit(Op::Jump(0));
                self.f().ctl[target].continues.push(j);
            },
            Stmt::Labeled(name, body) => {
                match &**body {
                    Stmt::For(..) | Stmt::ForIn(..) | Stmt::ForOf(..) | Stmt::ForAwaitOf(..) | Stmt::While(..) | Stmt::DoWhile(..) | Stmt::Labeled(..) => {
                        // the loop picks the label up
                        self.pending_label(name.clone());
                        self.stmt(body)?;
                    },
                    _ => {
                        let depth = self.env_depth();
                        self.push_ctl(Ctl { labels: vec![name.clone()], ..Default::default() }, depth, depth);
                        self.stmt(body)?;
                        let ctl = self.f().ctl.pop().unwrap();
                        for b in ctl.breaks {
                            self.patch_here(b);
                        }
                    },
                }
            },
            Stmt::Try(block, catch, finally) => self.try_stmt(block, catch, finally)?,
            Stmt::Switch(d, cases) => self.switch(d, cases)?,
        }
        Ok(())
    }

    /// the body of if / loops: a lone declaration still gets its own scope
    fn sub_stmt(&mut self, s: &Stmt) -> CResult<()> {
        match s {
            Stmt::Function(f) => {
                // `if (x) function f(){}` (sloppy): as if in a block
                let b = vec![Stmt::Function(f.clone())];
                self.block(&b)
            },
            _ => self.stmt(s),
        }
    }

    fn take_labels(&mut self) -> Vec<Name> {
        core::mem::take(&mut self.pending)
    }

    fn pending_label(&mut self, n: Name) {
        self.pending.push(n);
    }

    fn push_loop(&mut self, labels: Vec<Name>) {
        let depth = self.env_depth();
        self.push_ctl(Ctl { labels, is_loop: true, ..Default::default() }, depth, depth);
    }

    fn push_ctl(&mut self, mut c: Ctl, env_depth: usize, cont_depth: usize) {
        c.env_depth = env_depth;
        c.cont_depth = cont_depth;
        c.scope_len = self.f().scopes.len();
        self.f().ctl.push(c);
    }

    fn find_ctl(&self, label: Option<&str>, cont: bool) -> CResult<usize> {
        let ctl = &self.fns.last().unwrap().ctl;
        for i in (0..ctl.len()).rev() {
            let c = &ctl[i];
            match label {
                Some(l) => {
                    if c.labels.iter().any(|x| &**x == l) {
                        if cont && !c.is_loop {
                            return cerr("continue to a label that is not a loop");
                        }
                        return Ok(i);
                    }
                },
                None => {
                    if c.is_loop || (!cont && c.is_switch) {
                        return Ok(i);
                    }
                },
            }
        }
        cerr(if label.is_some() { "undefined label" } else if cont { "continue outside a loop" } else { "break outside a loop or switch" })
    }

    /// leaving through try blocks: leave their handlers and run their finally blocks; close
    /// environments. `target` None = the whole function (return, the value on the stack).
    fn exit_to(&mut self, target: Option<usize>, ret: bool) -> CResult<()> {
        self.exit_to_(target, ret, false)
    }

    /// `cont`: a continue (lands at its loop's continue depth)
    fn exit_to_(&mut self, target: Option<usize>, ret: bool, cont: bool) -> CResult<()> {
        let n = self.fns.last().unwrap().ctl.len();
        let stop = target.map(|t| t + 1).unwrap_or(0);
        let mut cur_env = self.env_depth();
        let mut tmp = None;
        let mut i = n;
        while i > stop {
            i -= 1;
            let (handler, fin, depth, scope_len) = {
                let c = &self.fns.last().unwrap().ctl[i];
                (c.handler, c.finally.clone(), c.env_depth, c.scope_len)
            };
            if handler {
                self.emit(Op::LeaveTry);
            }
            if let Some(fin) = fin {
                if ret && tmp.is_none() {
                    let t = self.temp();
                    self.emit(Op::SetLocal(t));
                    self.emit(Op::Pop);
                    tmp = Some(t);
                }
                for _ in depth..cur_env {
                    self.emit(Op::PopEnv);
                }
                cur_env = cur_env.min(depth);
                let saved_ctl: Vec<Ctl> = self.f().ctl.drain(i..).collect();
                let saved_scopes = self.f().scopes.split_off(scope_len);
                self.block(&fin)?;
                self.f().scopes.extend(saved_scopes);
                self.f().ctl.extend(saved_ctl);
            }
        }
        if !ret {
            let want = match target {
                Some(t) => {
                    let c = &self.fns.last().unwrap().ctl[t];
                    if cont { c.cont_depth } else { c.env_depth }
                },
                None => 0,
            };
            for _ in want..cur_env {
                self.emit(Op::PopEnv);
            }
        }
        if let Some(t) = tmp {
            self.emit(Op::GetLocal(t));
            self.free_temp(t);
        }
        Ok(())
    }

    fn for_stmt(&mut self, init: &Option<ForInit>, test: &Option<Expr>, update: &Option<Expr>, body: &Stmt) -> CResult<()> {
        let labels = self.take_labels();
        // `for (let i ...)`: a scope around the loop, renewed each iteration when captured
        let mut lexical = false;
        if let Some(ForInit::Var(kind, decls)) = init {
            if *kind != VarKind::Var {
                lexical = true;
                let stmts = vec![Stmt::Var(*kind, decls.clone())];
                self.enter_block(&stmts)?;
            }
        }
        match init {
            Some(ForInit::Var(kind, decls)) => self.stmt(&Stmt::Var(*kind, decls.clone()))?,
            Some(ForInit::Expr(e)) => {
                self.expr(e)?;
                self.emit(Op::Pop);
            },
            None => {},
        }
        let renew = lexical && self.f().scopes.last().unwrap().has_env;
        let top = self.here();
        let exit = match test {
            Some(t) => {
                self.cond_jump(t)?;
                Some(self.emit(Op::JumpIfFalse(0)))
            },
            None => None,
        };
        self.push_loop(labels);
        self.sub_stmt(body)?;
        let ctl = self.f().ctl.pop().unwrap();
        let cont = self.here();
        for c in ctl.continues {
            self.patch(c, cont);
        }
        if renew {
            self.emit(Op::RefreshEnv);
        }
        if let Some(u) = update {
            self.expr(u)?;
            self.emit(Op::Pop);
        }
        self.emit(Op::Jump(top));
        if let Some(e) = exit {
            self.patch_here(e);
        }
        for b in ctl.breaks {
            self.patch_here(b);
        }
        if lexical {
            self.leave_block();
        }
        Ok(())
    }

    /// assign the value on top of the stack to a for-in / for-of head (consumes it)
    fn for_head_assign(&mut self, head: &ForHead) -> CResult<()> {
        match head {
            ForHead::Var(kind, pat) => self.bind_pattern(pat, *kind != VarKind::Var || true),
            ForHead::Target(pat) => self.assign_pattern(pat),
        }
    }

    fn for_head_scope(&mut self, head: &ForHead) -> CResult<bool> {
        if let ForHead::Var(kind, pat) = head {
            if *kind != VarKind::Var {
                let stmts = vec![Stmt::Var(*kind, vec![(pat.clone(), Some(Expr::Undefined))])];
                self.enter_block(&stmts)?;
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn for_in(&mut self, head: &ForHead, obj: &Expr, body: &Stmt) -> CResult<()> {
        let labels = self.take_labels();
        self.expr(obj)?;
        self.emit(Op::ForInStart);
        let it = self.new_local();
        self.emit(Op::SetLocal(it));
        self.emit(Op::Pop);
        let top = self.here();
        self.emit(Op::GetLocal(it));
        let next = self.emit(Op::ForInNext(0));
        // stack: iter key
        self.emit(Op::Swap);
        self.emit(Op::Pop);
        let outer = self.env_depth();
        let scoped = self.for_head_scope(head)?;
        self.for_head_assign(head)?;
        let inner = self.env_depth();
        self.push_ctl(Ctl { labels, is_loop: true, ..Default::default() }, outer, inner);
        self.sub_stmt(body)?;
        let ctl = self.f().ctl.pop().unwrap();
        let cont = self.here();
        for c in ctl.continues {
            self.patch(c, cont);
        }
        if scoped {
            self.leave_block();
        }
        self.emit(Op::Jump(top));
        self.patch_here(next);
        // ForInNext jumped here with the iterator still on the stack
        self.emit(Op::Pop);
        for b in ctl.breaks {
            self.patch_here(b);
        }
        Ok(())
    }

    fn for_of(&mut self, head: &ForHead, obj: &Expr, body: &Stmt) -> CResult<()> {
        let labels = self.take_labels();
        // it = %getIterator(obj)
        self.emit(Op::Intrinsic(intr::GET_ITERATOR));
        self.expr(obj)?;
        self.emit(Op::Call(1));
        let it = self.new_local();
        self.emit(Op::SetLocal(it));
        self.emit(Op::Pop);
        let top = self.here();
        // r = it.next()
        self.emit(Op::GetLocal(it));
        let next_a = self.atom("next");
        self.emit(Op::GetPropKeep(next_a));
        self.emit(Op::CallMethod(0));
        self.emit(Op::Dup);
        let done_a = self.atom("done");
        self.emit(Op::GetProp(done_a));
        let exit = self.emit(Op::JumpIfTrue(0));
        let value_a = self.atom("value");
        self.emit(Op::GetProp(value_a));
        let outer = self.env_depth();
        let scoped = self.for_head_scope(head)?;
        self.for_head_assign(head)?;
        let inner = self.env_depth();
        self.push_ctl(Ctl { labels, is_loop: true, ..Default::default() }, outer, inner);
        self.sub_stmt(body)?;
        let ctl = self.f().ctl.pop().unwrap();
        let cont = self.here();
        for c in ctl.continues {
            self.patch(c, cont);
        }
        if scoped {
            self.leave_block();
        }
        self.emit(Op::Jump(top));
        self.patch_here(exit);
        self.emit(Op::Pop); // the result object
        let skip = self.emit(Op::Jump(0));
        // a break closes the iterator (it.return())
        for b in ctl.breaks {
            self.patch_here(b);
        }
        self.emit(Op::Intrinsic(intr::ITER_CLOSE));
        self.emit(Op::GetLocal(it));
        self.emit(Op::Call(1));
        self.emit(Op::Pop);
        self.patch_here(skip);
        Ok(())
    }

    fn try_stmt(&mut self, block: &[Stmt], catch: &Option<(Option<Pattern>, Vec<Stmt>)>, finally: &Option<Vec<Stmt>>) -> CResult<()> {
        let depth = self.env_depth();
        let fin_try = if let Some(fin) = finally {
            let at = self.emit(Op::EnterFinally(0));
            self.push_ctl(Ctl { finally: Some(fin.clone()), handler: true, ..Default::default() }, depth, depth);
            Some(at)
        } else {
            None
        };
        if let Some((param, cbody)) = catch {
            let at = self.emit(Op::EnterTry(0));
            self.push_ctl(Ctl { handler: true, ..Default::default() }, depth, depth);
            self.block(block)?;
            self.f().ctl.pop();
            self.emit(Op::LeaveTry);
            let over = self.emit(Op::Jump(0));
            self.patch_here(at);
            // the exception is on the stack
            match param {
                Some(p) => {
                    let mut names = Vec::new();
                    p.names(&mut names);
                    let decl = vec![Stmt::Var(VarKind::Let, names.iter().map(|n| (Pattern::Ident(n.clone()), None)).collect())];
                    self.enter_block(&decl)?;
                    self.bind_pattern(p, true)?;
                    self.block(cbody)?;
                    self.leave_block();
                },
                None => {
                    self.emit(Op::Pop);
                    self.block(cbody)?;
                },
            }
            self.patch_here(over);
        } else {
            self.block(block)?;
        }
        if let (Some(at), Some(fin)) = (fin_try, finally) {
            self.f().ctl.pop();
            self.emit(Op::LeaveTry);
            self.block(fin)?;
            let over = self.emit(Op::Jump(0));
            self.patch_here(at);
            let t = self.temp();
            self.emit(Op::SetLocal(t));
            self.emit(Op::Pop);
            self.block(fin)?;
            self.emit(Op::GetLocal(t));
            self.emit(Op::Throw);
            self.free_temp(t);
            self.patch_here(over);
        }
        Ok(())
    }

    fn switch(&mut self, d: &Expr, cases: &[Case]) -> CResult<()> {
        let labels = self.take_labels();
        self.expr(d)?;
        let t = self.new_local();
        self.emit(Op::SetLocal(t));
        self.emit(Op::Pop);
        let all: Vec<Stmt> = cases.iter().flat_map(|c| c.body.iter().cloned()).collect();
        self.enter_block(&all)?;
        let mut jumps = Vec::new();
        let mut default_case = None;
        for (i, c) in cases.iter().enumerate() {
            match &c.test {
                Some(e) => {
                    self.emit(Op::GetLocal(t));
                    self.expr(e)?;
                    self.emit(Op::StrictEq);
                    jumps.push((i, self.emit(Op::JumpIfTrue(0))));
                },
                None => default_case = Some(i),
            }
        }
        let to_default = self.emit(Op::Jump(0));
        // a break leaves the switch's block scope too: count from outside it
        let has_env = self.f().scopes.last().unwrap().has_env;
        let depth = self.env_depth() - if has_env { 1 } else { 0 };
        self.push_ctl(Ctl { labels, is_switch: true, ..Default::default() }, depth, depth);
        let mut starts = Vec::new();
        for c in cases {
            starts.push(self.here());
            self.stmts(&c.body)?;
        }
        let ctl = self.f().ctl.pop().unwrap();
        for (i, j) in jumps {
            self.patch(j, starts[i]);
        }
        match default_case {
            Some(i) => self.patch(to_default, starts[i]),
            None => {
                let end_inside = self.here();
                self.patch(to_default, end_inside);
            },
        }
        self.leave_block();
        for b in ctl.breaks {
            self.patch_here(b);
        }
        Ok(())
    }

    // ── patterns ──

    /// bind a declaration (var / let / const / parameter) from the value on the stack (consumed)
    fn bind_pattern(&mut self, pat: &Pattern, init: bool) -> CResult<()> {
        match pat {
            Pattern::Ident(n) => {
                self.emit_set(n, init);
                self.emit(Op::Pop);
                Ok(())
            },
            _ => self.destructure(pat, init, true),
        }
    }

    /// an assignment target (`=` / for-of): the value on the stack is consumed
    fn assign_pattern(&mut self, pat: &Pattern) -> CResult<()> {
        match pat {
            Pattern::Ident(n) => {
                self.emit_set(n, false);
                self.emit(Op::Pop);
                Ok(())
            },
            Pattern::Expr(e) => {
                let t = self.temp();
                self.emit(Op::SetLocal(t));
                self.emit(Op::Pop);
                match &**e {
                    Expr::Member(o, name, _) => {
                        self.expr(o)?;
                        self.emit(Op::GetLocal(t));
                        let a = self.atom(name);
                        self.emit(Op::SetProp(a));
                    },
                    Expr::Index(o, k, _) => {
                        self.expr(o)?;
                        self.expr(k)?;
                        self.emit(Op::GetLocal(t));
                        self.emit(Op::SetElem);
                    },
                    _ => return cerr("invalid assignment target"),
                }
                self.emit(Op::Pop);
                self.free_temp(t);
                Ok(())
            },
            _ => self.destructure(pat, false, false),
        }
    }

    fn destructure(&mut self, pat: &Pattern, init: bool, decl: bool) -> CResult<()> {
        let store = |c: &mut Self, p: &Pattern| -> CResult<()> {
            if decl {
                c.bind_pattern(p, init)
            } else {
                c.assign_pattern(p)
            }
        };
        match pat {
            Pattern::Array(items, rest) => {
                // the value as an array (iterables through the prelude)
                let t = self.temp();
                self.emit(Op::SetLocal(t));
                self.emit(Op::Pop);
                self.emit(Op::Intrinsic(intr::ITER_TO_ARRAY));
                self.emit(Op::GetLocal(t));
                self.emit(Op::Call(1));
                self.emit(Op::SetLocal(t));
                self.emit(Op::Pop);
                for (i, it) in items.iter().enumerate() {
                    if let Some(pe) = it {
                        self.emit(Op::GetLocal(t));
                        self.emit(Op::Int(i as i32));
                        self.emit(Op::GetElem);
                        if let Some(d) = &pe.default {
                            let j = self.emit(Op::JumpIfNotUndefined(0));
                            self.emit(Op::Pop);
                            self.expr_named(d, pattern_name(&pe.target))?;
                            self.patch_here(j);
                        }
                        store(self, &pe.target)?;
                    }
                }
                if let Some(r) = rest {
                    self.emit(Op::GetLocal(t));
                    let slice = self.atom("slice");
                    self.emit(Op::GetPropKeep(slice));
                    self.emit(Op::Int(items.len() as i32));
                    self.emit(Op::CallMethod(1));
                    store(self, r)?;
                }
                self.free_temp(t);
                Ok(())
            },
            Pattern::Object(props, rest) => {
                self.emit(Op::CheckObjCoercible);
                let t = self.temp();
                self.emit(Op::SetLocal(t));
                self.emit(Op::Pop);
                let keys_t = if rest.is_some() {
                    let k = self.temp();
                    self.emit(Op::NewArray(0));
                    self.emit(Op::SetLocal(k));
                    self.emit(Op::Pop);
                    Some(k)
                } else {
                    None
                };
                for (key, pe) in props {
                    self.emit(Op::GetLocal(t));
                    match key {
                        PropKey::Named(n) => {
                            if let Some(k) = keys_t {
                                self.emit(Op::GetLocal(k));
                                self.emit_str(js(n));
                                self.emit(Op::ArrayPush);
                                self.emit(Op::Pop);
                            }
                            let a = self.atom(n);
                            self.emit(Op::GetProp(a));
                        },
                        PropKey::Computed(e) => {
                            self.expr(e)?;
                            self.emit(Op::ToKey);
                            if let Some(k) = keys_t {
                                self.emit(Op::Dup);
                                self.emit(Op::GetLocal(k));
                                self.emit(Op::Swap);
                                self.emit(Op::ArrayPush);
                                self.emit(Op::Pop);
                            }
                            self.emit(Op::GetElem);
                        },
                    }
                    if let Some(d) = &pe.default {
                        let j = self.emit(Op::JumpIfNotUndefined(0));
                        self.emit(Op::Pop);
                        self.expr_named(d, pattern_name(&pe.target))?;
                        self.patch_here(j);
                    }
                    store(self, &pe.target)?;
                }
                if let (Some(r), Some(k)) = (rest, keys_t) {
                    self.emit(Op::Intrinsic(intr::OBJECT_REST));
                    self.emit(Op::GetLocal(t));
                    self.emit(Op::GetLocal(k));
                    self.emit(Op::Call(2));
                    store(self, r)?;
                    self.free_temp(k);
                }
                self.free_temp(t);
                Ok(())
            },
            Pattern::Ident(_) | Pattern::Expr(_) => store(self, pat),
        }
    }

    // ── expressions ──

    /// a jump condition (just the value; the VM tests truthiness)
    fn cond_jump(&mut self, e: &Expr) -> CResult<()> {
        self.expr(e)
    }

    fn expr_named(&mut self, e: &Expr, name: Option<&str>) -> CResult<()> {
        match (e, name) {
            (Expr::Function(f), Some(n)) if f.name.is_none() => self.function_value(f, Some(n)),
            (Expr::Class(c), Some(n)) if c.name.is_none() => self.class_named(c, Some(n)),
            _ => self.expr(e),
        }
    }

    pub fn expr(&mut self, e: &Expr) -> CResult<()> {
        match e {
            Expr::Num(n) => self.emit_num(*n),
            Expr::Str(s) => self.emit_str(s.clone()),
            Expr::Bool(true) => {
                self.emit(Op::True);
            },
            Expr::Bool(false) => {
                self.emit(Op::False);
            },
            Expr::Null => {
                self.emit(Op::Null);
            },
            Expr::Undefined => {
                self.emit(Op::Undef);
            },
            Expr::This | Expr::Super => self.emit_this(),
            Expr::NewTarget => {
                if self.this_is_binding() {
                    self.emit_get(&Rc::from("%nt"));
                } else {
                    self.emit(Op::NewTarget);
                }
            },
            Expr::Ident(n) => {
                if &**n == "undefined" && self.resolve(n).is_none() {
                    self.emit(Op::Undef);
                } else {
                    self.emit_get(n);
                }
            },
            Expr::Regex(b, f) => {
                let fx = self.f();
                fx.regexes.push((js(b), js(f)));
                let i = fx.regexes.len() as u32 - 1;
                self.emit(Op::RegExp(i));
            },
            Expr::Template(parts, exprs) => {
                self.emit_str(parts[0].clone());
                for (i, x) in exprs.iter().enumerate() {
                    self.expr(x)?;
                    self.emit(Op::ToStr);
                    self.emit(Op::Add);
                    if !parts[i + 1].is_empty() {
                        self.emit_str(parts[i + 1].clone());
                        self.emit(Op::Add);
                    }
                }
            },
            Expr::Tagged(tag, parts, exprs) => {
                // tag(strings, ...values); strings has .raw
                let (this_kept, method) = self.callee(tag)?;
                let _ = this_kept;
                self.emit(Op::Intrinsic(intr::TEMPLATE_OBJECT));
                self.emit(Op::NewArray(0));
                for c in &parts.cooked {
                    match c {
                        Some(s) => self.emit_str(s.clone()),
                        None => {
                            self.emit(Op::Undef);
                        },
                    }
                    self.emit(Op::ArrayPush);
                }
                self.emit(Op::NewArray(0));
                for r in &parts.raw {
                    self.emit_str(r.clone());
                    self.emit(Op::ArrayPush);
                }
                self.emit(Op::Call(2));
                for x in exprs {
                    self.expr(x)?;
                }
                let n = exprs.len() as u32 + 1;
                self.emit(if method { Op::CallMethod(n) } else { Op::Call(n) });
            },
            Expr::Array(items) => {
                self.emit(Op::NewArray(0));
                for it in items {
                    match it {
                        None => {
                            self.emit(Op::ArrayHole);
                        },
                        Some(Expr::Spread(x)) => {
                            self.emit(Op::Intrinsic(intr::ITER_TO_ARRAY));
                            self.expr(x)?;
                            self.emit(Op::Call(1));
                            self.emit(Op::ArrayAppend);
                        },
                        Some(x) => {
                            self.expr(x)?;
                            self.emit(Op::ArrayPush);
                        },
                    }
                }
            },
            Expr::Object(props) => self.object(props)?,
            Expr::Function(f) => self.function_value(f, None)?,
            Expr::Class(c) => self.class(c)?,
            Expr::Unary(op, x) => self.unary(op, x)?,
            Expr::Update(op, prefix, target) => self.update(op, *prefix, target)?,
            Expr::Binary(op, a, b) => {
                if let (&"in", Expr::PrivateName(n)) = (op, &**a) {
                    self.expr(b)?;
                    let at = self.atom(n);
                    self.emit(Op::HasPrivate(at));
                    return Ok(());
                }
                self.expr(a)?;
                self.expr(b)?;
                self.emit(binop(op));
            },
            Expr::Logical(op, a, b) => {
                self.expr(a)?;
                let j = match *op {
                    "&&" => self.emit(Op::JumpIfFalseKeep(0)),
                    "||" => self.emit(Op::JumpIfTrueKeep(0)),
                    _ => self.emit(Op::JumpIfNotNullishKeep(0)),
                };
                self.expr(b)?;
                self.patch_here(j);
            },
            Expr::Cond(c, a, b) => {
                self.expr(c)?;
                let j = self.emit(Op::JumpIfFalse(0));
                self.expr(a)?;
                let j2 = self.emit(Op::Jump(0));
                self.patch_here(j);
                self.expr(b)?;
                self.patch_here(j2);
            },
            Expr::Assign(op, target, value) => self.assign(op, target, value)?,
            Expr::Seq(items) => {
                for (i, x) in items.iter().enumerate() {
                    self.expr(x)?;
                    if i + 1 < items.len() {
                        self.emit(Op::Pop);
                    }
                }
            },
            Expr::Call(callee, args, optional) => {
                if let Expr::Super = &**callee {
                    return self.super_call(args);
                }
                let (_, method) = self.callee(callee)?;
                self.pending_call_name = describe_callee(callee);
                if *optional {
                    if !method {
                        // f?.(): put an undefined `this` under f so OptCheckCall sees "this f"
                        self.emit(Op::Undef);
                        self.emit(Op::Swap);
                    }
                    let j = self.emit(Op::OptCheckCall(0));
                    self.opt_jump(j)?;
                    self.call_args(args, true)?;
                } else {
                    self.call_args(args, method)?;
                }
            },
            Expr::New(callee, args) => {
                self.expr(callee)?;
                let name = describe_callee(callee);
                if args.iter().any(|a| matches!(a, Expr::Spread(_))) {
                    self.spread_array(args)?;
                    self.emit(Op::NewSpread);
                } else {
                    for a in args {
                        self.expr(a)?;
                    }
                    let at = self.emit(Op::New(args.len() as u32));
                    self.note_call(at, name);
                }
            },
            Expr::Member(o, n, optional) => {
                if let Expr::Super = &**o {
                    self.emit_this();
                    let a = self.atom(n);
                    self.emit(Op::SuperGet(a, false));
                    return Ok(());
                }
                self.expr(o)?;
                if *optional {
                    let j = self.emit(Op::OptCheck(0));
                    self.opt_jump(j)?;
                }
                let a = self.atom(n);
                self.emit(Op::GetProp(a));
            },
            Expr::Index(o, k, optional) => {
                if let Expr::Super = &**o {
                    self.emit_this();
                    self.expr(k)?;
                    self.emit(Op::ToKey);
                    self.emit(Op::SuperGetElem(false));
                    return Ok(());
                }
                self.expr(o)?;
                if *optional {
                    let j = self.emit(Op::OptCheck(0));
                    self.opt_jump(j)?;
                }
                self.expr(k)?;
                self.emit(Op::GetElem);
            },
            Expr::OptChain(inner) => {
                self.f().opt_jumps.push(Vec::new());
                self.expr(inner)?;
                let js_ = self.f().opt_jumps.pop().unwrap();
                for j in js_ {
                    self.patch_here(j);
                }
            },
            Expr::Spread(_) => return cerr("unexpected spread"),
            Expr::Yield(arg, delegate) => self.yield_expr(arg.as_deref(), *delegate)?,
            Expr::Await(x) => {
                self.expr(x)?;
                self.emit(Op::Yield(2));
            },
            Expr::BigInt(s) => {
                let k = self.konst(Value::Str(js(s)));
                self.emit(Op::BigInt(k));
            },
            Expr::PrivateName(_) => return cerr("a private name must be followed by 'in'"),
            Expr::Import(x) => {
                self.emit(Op::Intrinsic(intr::DYN_IMPORT));
                self.expr(x)?;
                if self.resolve("%meta").is_some() {
                    self.emit_get(&Rc::from("%meta"));
                    let u = self.atom("url");
                    self.emit(Op::GetProp(u));
                } else {
                    self.emit(Op::Undef);
                }
                self.emit(Op::Call(2));
            },
            Expr::ClassField(key, init) => self.class_field(key, init.as_deref())?,
        }
        Ok(())
    }

    fn opt_jump(&mut self, j: usize) -> CResult<()> {
        match self.f().opt_jumps.last_mut() {
            Some(l) => {
                l.push(j);
                Ok(())
            },
            None => cerr("optional chain outside a chain"),
        }
    }

    /// pushes the callee: (has this, method call) — `this f` for a method, else `f`
    fn callee(&mut self, callee: &Expr) -> CResult<(bool, bool)> {
        match callee {
            Expr::Member(o, n, _) if matches!(&**o, Expr::Super) => {
                self.emit_this();
                let a = self.atom(n);
                self.emit(Op::SuperGet(a, true));
                Ok((true, true))
            },
            Expr::Index(o, k, _) if matches!(&**o, Expr::Super) => {
                self.emit_this();
                self.expr(k)?;
                self.emit(Op::ToKey);
                self.emit(Op::SuperGetElem(true));
                Ok((true, true))
            },
            Expr::Member(o, n, optional) => {
                self.expr(o)?;
                if *optional {
                    let j = self.emit(Op::OptCheck(0));
                    self.opt_jump(j)?;
                }
                let a = self.atom(n);
                self.emit(Op::GetPropKeep(a));
                Ok((true, true))
            },
            Expr::Index(o, k, optional) => {
                self.expr(o)?;
                if *optional {
                    let j = self.emit(Op::OptCheck(0));
                    self.opt_jump(j)?;
                }
                self.expr(k)?;
                self.emit(Op::GetElemKeep);
                Ok((true, true))
            },
            _ => {
                self.expr(callee)?;
                Ok((false, false))
            },
        }
    }

    fn call_args(&mut self, args: &[Expr], method: bool) -> CResult<()> {
        let name = self.pending_call_name.take();
        if args.iter().any(|a| matches!(a, Expr::Spread(_))) {
            self.spread_array(args)?;
            let at = self.emit(if method { Op::CallMethodSpread } else { Op::CallSpread });
            self.note_call(at, name);
            return Ok(());
        }
        for a in args {
            self.expr(a)?;
        }
        let n = args.len() as u32;
        let at = self.emit(if method { Op::CallMethod(n) } else { Op::Call(n) });
        self.note_call(at, name);
        Ok(())
    }

    fn note_call(&mut self, at: usize, name: Option<String>) {
        if let Some(n) = name {
            self.f().call_names.push((at as u32, js(&n)));
        }
    }

    fn spread_array(&mut self, items: &[Expr]) -> CResult<()> {
        self.emit(Op::NewArray(0));
        for it in items {
            match it {
                Expr::Spread(x) => {
                    self.emit(Op::Intrinsic(intr::ITER_TO_ARRAY));
                    self.expr(x)?;
                    self.emit(Op::Call(1));
                    self.emit(Op::ArrayAppend);
                },
                x => {
                    self.expr(x)?;
                    self.emit(Op::ArrayPush);
                },
            }
        }
        Ok(())
    }

    fn object(&mut self, props: &[ObjProp]) -> CResult<()> {
        self.emit(Op::NewObject);
        for p in props {
            match p {
                ObjProp::Spread(x) => {
                    // %copyProps(obj, src) returns obj
                    let t = self.temp();
                    self.emit(Op::SetLocal(t));
                    self.emit(Op::Pop);
                    self.emit(Op::Intrinsic(intr::COPY_PROPS));
                    self.emit(Op::GetLocal(t));
                    self.expr(x)?;
                    self.emit(Op::Call(2));
                    self.free_temp(t);
                },
                ObjProp::Prop(PropKey::Named(n), kind, v) => {
                    let a = self.atom(n);
                    if &**n == "__proto__" && *kind == PropKind::Init && !matches!(v, Expr::Function(_)) {
                        // { __proto__: p } sets the prototype
                        self.emit(Op::Dup);
                        self.expr(v)?;
                        let sp = self.atom("__proto__");
                        self.emit(Op::SetProp(sp));
                        self.emit(Op::Pop);
                        continue;
                    }
                    match kind {
                        PropKind::Init => {
                            self.expr_named(v, Some(n))?;
                            if matches!(v, Expr::Function(f) if f.method) {
                                self.emit(Op::SetHome);
                            }
                            self.emit(Op::InitProp(a));
                        },
                        PropKind::Get => {
                            self.expr(v)?;
                            self.emit(Op::SetHome);
                            self.emit(Op::InitGetter(a));
                        },
                        PropKind::Set => {
                            self.expr(v)?;
                            self.emit(Op::SetHome);
                            self.emit(Op::InitSetter(a));
                        },
                    }
                },
                ObjProp::Prop(PropKey::Computed(k), kind, v) => {
                    self.expr(k)?;
                    self.emit(Op::ToKey);
                    self.expr(v)?;
                    self.emit(match kind {
                        PropKind::Init => Op::InitElem,
                        PropKind::Get => Op::InitGetterElem,
                        PropKind::Set => Op::InitSetterElem,
                    });
                },
            }
        }
        Ok(())
    }

    fn class(&mut self, c: &Rc<ClassNode>) -> CResult<()> {
        self.class_named(c, None)
    }

    fn class_named(&mut self, c: &Rc<ClassNode>, name_hint: Option<&str>) -> CResult<()> {
        // the class's private names: one hidden atom each
        let mut privs = BTreeMap::new();
        for m in &c.members {
            if let PropKey::Named(n) = &m.key {
                if n.starts_with('#') && !privs.contains_key(&**n) {
                    let a = self.atoms.new_symbol(&js(n), true);
                    privs.insert(String::from(&**n), a);
                }
            }
        }
        self.privates.push(privs);
        // a named class sees its own name (a const) inside
        let scoped = if let Some(n) = &c.name {
            let decl = vec![Stmt::Var(VarKind::Const, vec![(Pattern::Ident(n.clone()), None)])];
            self.enter_block(&decl)?;
            true
        } else {
            false
        };
        if let Some(x) = &c.extends {
            self.expr(x)?;
        }
        let ctor = c.ctor.as_ref().unwrap();
        let cname = c.name.as_deref().or(name_hint).unwrap_or("");
        let mut proto = self.compile_function(ctor, Some(cname))?;
        if let Some(p) = Rc::get_mut(&mut proto) {
            // Function.prototype.toString of a class is the class's text
            if let Some((src, _, _)) = &p.src {
                p.src = Some((src.clone(), c.src.0, c.src.1));
            }
        }
        let fx = self.f();
        fx.funcs.push(proto);
        let idx = fx.funcs.len() as u32 - 1;
        self.emit(Op::MakeClass(idx, c.extends.is_some()));
        let proto_t = self.temp();
        let f_t = self.temp();
        let keys_t = self.temp();
        self.emit(Op::SetLocal(proto_t));
        self.emit(Op::Pop);
        self.emit(Op::SetLocal(f_t));
        self.emit(Op::Pop);
        self.emit(Op::NewArray(0));
        self.emit(Op::SetLocal(keys_t));
        self.emit(Op::Pop);
        for m in &c.members {
            match &m.kind {
                ClassMemberKind::Method(f, kind) => {
                    let k = match kind {
                        PropKind::Init => 0,
                        PropKind::Get => 1,
                        PropKind::Set => 2,
                    };
                    self.emit(Op::GetLocal(if m.is_static { f_t } else { proto_t }));
                    match &m.key {
                        PropKey::Named(n) => {
                            self.function_value(f, Some(n))?;
                            let a = self.atom(n);
                            self.emit(Op::DefineMethod(a, k));
                        },
                        PropKey::Computed(e) => {
                            self.expr(e)?;
                            self.emit(Op::ToKey);
                            self.function_value(f, None)?;
                            self.emit(Op::DefineMethodElem(k));
                        },
                    }
                    self.emit(Op::Pop);
                },
                ClassMemberKind::Field(_) => {
                    if let PropKey::Computed(e) = &m.key {
                        self.emit(Op::GetLocal(keys_t));
                        self.expr(e)?;
                        self.emit(Op::ToKey);
                        self.emit(Op::ArrayPush);
                        self.emit(Op::Pop);
                    }
                },
                ClassMemberKind::StaticBlock(_) => {},
            }
        }
        if let Some(fi) = &c.field_init {
            self.emit(Op::GetLocal(proto_t));
            self.function_value(fi, None)?;
            self.emit(Op::SetHome);
            self.emit(Op::GetLocal(keys_t));
            self.emit(Op::SetFieldKeys);
            self.emit(Op::Swap);
            self.emit(Op::Pop);
            self.emit(Op::GetLocal(f_t));
            self.emit(Op::Swap);
            self.emit(Op::SetFields);
            self.emit(Op::Pop);
        }
        if let Some(n) = &c.name {
            self.emit(Op::GetLocal(f_t));
            self.emit_set(n, true);
            self.emit(Op::Pop);
        }
        if let Some(si) = &c.static_init {
            self.emit(Op::GetLocal(f_t));
            self.emit(Op::GetLocal(f_t));
            self.function_value(si, None)?;
            self.emit(Op::SetHome);
            self.emit(Op::GetLocal(keys_t));
            self.emit(Op::SetFieldKeys);
            self.emit(Op::CallMethod(0));
            self.emit(Op::Pop);
            self.emit(Op::Pop);
        }
        self.emit(Op::GetLocal(f_t));
        self.free_temp(keys_t);
        self.free_temp(f_t);
        self.free_temp(proto_t);
        if scoped {
            self.leave_block();
        }
        self.privates.pop();
        Ok(())
    }

    /// `super(...)`: construct the parent with this new.target; the result is `this`
    fn super_call(&mut self, args: &[Expr]) -> CResult<()> {
        if !self.this_is_binding() {
            return cerr("'super' keyword unexpected here");
        }
        let ctor: Name = Rc::from("%ctor");
        let nt: Name = Rc::from("%nt");
        let this: Name = Rc::from("this");
        self.emit_get(&ctor);
        self.emit(Op::GetProtoOf);
        self.emit_get(&nt);
        if args.iter().any(|a| matches!(a, Expr::Spread(_))) {
            self.spread_array(args)?;
            self.emit(Op::SuperCallSpread);
        } else {
            for a in args {
                self.expr(a)?;
            }
            self.emit(Op::SuperCall(args.len() as u32));
        }
        self.emit_set(&this, true);
        self.emit_get(&ctor);
        self.emit(Op::InitFields);
        Ok(())
    }

    fn class_field(&mut self, key: &ClassFieldKey, init: Option<&Expr>) -> CResult<()> {
        self.emit(Op::This);
        let value = |c: &mut Self, name: Option<&str>| -> CResult<()> {
            match init {
                Some(e) => c.expr_named(e, name),
                None => {
                    c.emit(Op::Undef);
                    Ok(())
                },
            }
        };
        match key {
            ClassFieldKey::Named(n) | ClassFieldKey::Private(n) => {
                value(self, Some(n))?;
                let a = self.atom(n);
                self.emit(Op::DefineField(a));
            },
            ClassFieldKey::Computed(i) => {
                self.emit(Op::FieldKey(*i));
                value(self, None)?;
                self.emit(Op::DefineFieldElem);
            },
        }
        Ok(())
    }

    fn unary(&mut self, op: &str, x: &Expr) -> CResult<()> {
        match op {
            "typeof" => {
                if let Expr::Ident(n) = x {
                    if self.resolve(n).is_none() && !self.module_imports.contains_key(&**n) {
                        let a = self.atom(n);
                        self.emit(Op::TypeofGlobal(a));
                        return Ok(());
                    }
                }
                self.expr(x)?;
                self.emit(Op::Typeof);
            },
            "delete" => match x {
                Expr::Member(o, n, _) => {
                    self.expr(o)?;
                    let a = self.atom(n);
                    self.emit(Op::DelProp(a));
                },
                Expr::Index(o, k, _) => {
                    self.expr(o)?;
                    self.expr(k)?;
                    self.emit(Op::DelElem);
                },
                Expr::Ident(n) => {
                    if self.resolve(n).is_none() {
                        let a = self.atom(n);
                        self.emit(Op::DeleteGlobal(a));
                    } else {
                        self.emit(Op::False);
                    }
                },
                Expr::OptChain(inner) => {
                    self.f().opt_jumps.push(Vec::new());
                    self.unary("delete", inner)?;
                    let js_ = self.f().opt_jumps.pop().unwrap();
                    // a short-circuited delete is true
                    let over = self.emit(Op::Jump(0));
                    for j in js_ {
                        self.patch_here(j);
                    }
                    self.emit(Op::Pop);
                    self.emit(Op::True);
                    self.patch_here(over);
                },
                other => {
                    self.expr(other)?;
                    self.emit(Op::Pop);
                    self.emit(Op::True);
                },
            },
            "void" => {
                self.expr(x)?;
                self.emit(Op::Pop);
                self.emit(Op::Undef);
            },
            "!" => {
                self.expr(x)?;
                self.emit(Op::Not);
            },
            "-" => {
                if let Expr::Num(n) = x {
                    self.emit_num(-n);
                } else {
                    self.expr(x)?;
                    self.emit(Op::Neg);
                }
            },
            "+" => {
                self.expr(x)?;
                self.emit(Op::Plus);
            },
            "~" => {
                self.expr(x)?;
                self.emit(Op::BitNot);
            },
            _ => return cerr("unknown unary operator"),
        }
        Ok(())
    }

    fn update(&mut self, op: &str, prefix: bool, target: &Expr) -> CResult<()> {
        let step = if op == "++" { Op::Inc } else { Op::Dec };
        match target {
            Expr::Ident(n) => {
                self.emit_get(n);
                self.emit(Op::ToNumber);
                if prefix {
                    self.emit(step);
                    self.emit_set(n, false);
                } else {
                    self.emit(Op::Dup);
                    self.emit(step);
                    self.emit_set(n, false);
                    self.emit(Op::Pop);
                }
            },
            Expr::Member(o, n, _) => {
                let a = self.atom(n);
                self.expr(o)?;
                self.emit(Op::Dup);
                self.emit(Op::GetProp(a));
                self.emit(Op::ToNumber);
                if prefix {
                    self.emit(step);
                    self.emit(Op::SetProp(a));
                } else {
                    let t = self.temp();
                    self.emit(Op::SetLocal(t));
                    self.emit(step);
                    self.emit(Op::SetProp(a));
                    self.emit(Op::Pop);
                    self.emit(Op::GetLocal(t));
                    self.free_temp(t);
                }
            },
            Expr::Index(o, k, _) => {
                self.expr(o)?;
                self.expr(k)?;
                self.emit(Op::ToKey);
                self.emit(Op::Dup2);
                self.emit(Op::GetElem);
                self.emit(Op::ToNumber);
                if prefix {
                    self.emit(step);
                    self.emit(Op::SetElem);
                } else {
                    let t = self.temp();
                    self.emit(Op::SetLocal(t));
                    self.emit(step);
                    self.emit(Op::SetElem);
                    self.emit(Op::Pop);
                    self.emit(Op::GetLocal(t));
                    self.free_temp(t);
                }
            },
            _ => return cerr("invalid update target"),
        }
        Ok(())
    }

    fn assign(&mut self, op: &str, target: &Pattern, value: &Expr) -> CResult<()> {
        if op == "=" {
            match target {
                Pattern::Ident(n) => {
                    self.expr_named(value, Some(n))?;
                    self.emit_set(n, false);
                },
                Pattern::Expr(e) => match &**e {
                    Expr::Member(o, n, _) => {
                        self.expr(o)?;
                        self.expr(value)?;
                        let a = self.atom(n);
                        self.emit(Op::SetProp(a));
                    },
                    Expr::Index(o, k, _) => {
                        self.expr(o)?;
                        self.expr(k)?;
                        self.emit(Op::ToKey);
                        self.expr(value)?;
                        self.emit(Op::SetElem);
                    },
                    _ => return cerr("invalid assignment target"),
                },
                _ => {
                    self.expr(value)?;
                    self.emit(Op::Dup);
                    self.assign_pattern(target)?;
                },
            }
            return Ok(());
        }
        let logical = matches!(op, "&&=" | "||=" | "??=");
        let bin = &op[..op.len() - 1];
        match target {
            Pattern::Ident(n) => {
                self.emit_get(n);
                if logical {
                    let j = self.logical_jump(bin);
                    self.expr_named(value, Some(n))?;
                    self.emit_set(n, false);
                    self.patch_here(j);
                } else {
                    self.expr(value)?;
                    self.emit(binop(bin));
                    self.emit_set(n, false);
                }
            },
            Pattern::Expr(e) => match &**e {
                Expr::Member(o, n, _) => {
                    let a = self.atom(n);
                    self.expr(o)?;
                    self.emit(Op::Dup);
                    self.emit(Op::GetProp(a));
                    if logical {
                        // obj val
                        let j = self.logical_jump(bin);
                        self.expr(value)?;
                        self.emit(Op::SetProp(a));
                        let over = self.emit(Op::Jump(0));
                        self.patch_here(j);
                        // obj val -> val
                        self.emit(Op::Swap);
                        self.emit(Op::Pop);
                        self.patch_here(over);
                    } else {
                        self.expr(value)?;
                        self.emit(binop(bin));
                        self.emit(Op::SetProp(a));
                    }
                },
                Expr::Index(o, k, _) => {
                    self.expr(o)?;
                    self.expr(k)?;
                    self.emit(Op::ToKey);
                    self.emit(Op::Dup2);
                    self.emit(Op::GetElem);
                    if logical {
                        let j = self.logical_jump(bin);
                        self.expr(value)?;
                        self.emit(Op::SetElem);
                        let over = self.emit(Op::Jump(0));
                        self.patch_here(j);
                        // obj key val -> val
                        self.emit(Op::Rot3);
                        self.emit(Op::Pop);
                        self.emit(Op::Pop);
                        self.patch_here(over);
                    } else {
                        self.expr(value)?;
                        self.emit(binop(bin));
                        self.emit(Op::SetElem);
                    }
                },
                _ => return cerr("invalid assignment target"),
            },
            _ => return cerr("invalid compound assignment target"),
        }
        Ok(())
    }

    /// for a &&= ||= ??=: jump (keeping the value) when no assignment happens
    fn logical_jump(&mut self, bin: &str) -> usize {
        match bin {
            "&&" => self.emit(Op::JumpIfFalseKeep(0)),
            "||" => self.emit(Op::JumpIfTrueKeep(0)),
            _ => self.emit(Op::JumpIfNotNullishKeep(0)),
        }
    }
}

fn binop(op: &str) -> Op {
    match op {
        "+" => Op::Add,
        "-" => Op::Sub,
        "*" => Op::Mul,
        "/" => Op::Div,
        "%" => Op::Mod,
        "**" => Op::Exp,
        "<<" => Op::Shl,
        ">>" => Op::Shr,
        ">>>" => Op::UShr,
        "&" => Op::BitAnd,
        "|" => Op::BitOr,
        "^" => Op::BitXor,
        "==" => Op::Eq,
        "!=" => Op::Ne,
        "===" => Op::StrictEq,
        "!==" => Op::StrictNe,
        "<" => Op::Lt,
        "<=" => Op::Le,
        ">" => Op::Gt,
        ">=" => Op::Ge,
        "in" => Op::In,
        "instanceof" => Op::InstanceOf,
        _ => Op::Undef,
    }
}

/// "a.b.c" / "f" / "a[...]" for error messages
fn describe_callee(e: &Expr) -> Option<String> {
    fn go(e: &Expr, depth: u32) -> Option<String> {
        if depth > 4 {
            return Some("...".into());
        }
        Some(match e {
            Expr::Ident(n) => String::from(&**n),
            Expr::This => "this".into(),
            Expr::Member(o, n, _) => format!("{}.{}", go(o, depth + 1)?, n),
            Expr::Index(o, k, _) => match &**k {
                Expr::Str(s) => format!("{}[\"{}\"]", go(o, depth + 1)?, crate::strs::to_rust(s)),
                Expr::Num(n) => format!("{}[{}]", go(o, depth + 1)?, n),
                _ => format!("{}[...]", go(o, depth + 1)?),
            },
            Expr::Call(c, _, _) => format!("{}(...)", go(c, depth + 1)?),
            Expr::OptChain(x) => go(x, depth)?,
            _ => "(intermediate value)".into(),
        })
    }
    go(e, 0)
}

fn pattern_name(p: &Pattern) -> Option<&str> {
    match p {
        Pattern::Ident(n) => Some(n),
        _ => None,
    }
}

impl FnCtx {
    fn new(is_script: bool, arrow: bool, strict: bool, captured: BTreeSet<Name>) -> Self {
        FnCtx {
            code: Vec::new(),
            consts: Vec::new(),
            const_map: BTreeMap::new(),
            funcs: Vec::new(),
            regexes: Vec::new(),
            lines: Vec::new(),
            call_names: Vec::new(),
            nlocals: 0,
            free_temps: Vec::new(),
            scopes: Vec::new(),
            ctl: Vec::new(),
            captured,
            is_script,
            arrow,
            strict,
            opt_jumps: Vec::new(),
            keep_args: false,
            async_gen: false,
            derived: false,
        }
    }

    fn finish(self, name: JsStr, length: u32, nparams: u32, arrow: bool, method: bool, kind: FuncKind, is_script: bool, src: Option<(Rc<str>, usize, usize)>, is_async: bool, generator: bool) -> FuncProto {
        FuncProto {
            name,
            length,
            nparams,
            nlocals: self.nlocals.max(nparams),
            code: self.code,
            consts: self.consts,
            funcs: self.funcs,
            regexes: self.regexes,
            lines: self.lines,
            call_names: self.call_names,
            strict: self.strict,
            arrow,
            method,
            kind,
            keep_args: self.keep_args,
            is_script,
            src,
            is_async,
            generator,
        }
    }
}

// ── syntax tree walks ──

/// `var` names (and, at function level, function declarations), not entering nested functions
pub fn var_names(body: &[Stmt], out: &mut Vec<Name>, top: bool) {
    for s in body {
        var_names_stmt(s, out, top);
    }
}

fn var_names_stmt(s: &Stmt, out: &mut Vec<Name>, top: bool) {
    let mut push = |n: &Name, out: &mut Vec<Name>| {
        if !out.contains(n) {
            out.push(n.clone());
        }
    };
    match s {
        Stmt::Var(VarKind::Var, decls) => {
            for (p, _) in decls {
                let mut ns = Vec::new();
                p.names(&mut ns);
                for n in &ns {
                    push(n, out);
                }
            }
        },
        Stmt::Function(f) => {
            // function-level declarations, and (sloppy) block-level ones too
            let _ = top;
            if let Some(n) = &f.name {
                push(n, out);
            }
        },
        Stmt::If(_, a, b) => {
            var_names_stmt(a, out, false);
            if let Some(b) = b {
                var_names_stmt(b, out, false);
            }
        },
        Stmt::Block(b) => var_names(b, out, false),
        Stmt::For(init, _, _, body) => {
            if let Some(ForInit::Var(VarKind::Var, decls)) = init {
                for (p, _) in decls {
                    let mut ns = Vec::new();
                    p.names(&mut ns);
                    for n in &ns {
                        push(n, out);
                    }
                }
            }
            var_names_stmt(body, out, false);
        },
        Stmt::ForIn(h, _, body) | Stmt::ForOf(h, _, body) | Stmt::ForAwaitOf(h, _, body) => {
            if let ForHead::Var(VarKind::Var, p) = h {
                let mut ns = Vec::new();
                p.names(&mut ns);
                for n in &ns {
                    push(n, out);
                }
            }
            var_names_stmt(body, out, false);
        },
        Stmt::While(_, b) | Stmt::DoWhile(b, _) | Stmt::Labeled(_, b) => var_names_stmt(b, out, false),
        Stmt::Try(b, c, f) => {
            var_names(b, out, false);
            if let Some((_, cb)) = c {
                var_names(cb, out, false);
            }
            if let Some(fb) = f {
                var_names(fb, out, false);
            }
        },
        Stmt::Switch(_, cases) => {
            for c in cases {
                var_names(&c.body, out, false);
            }
        },
        _ => {},
    }
}

/// a block's own let / const / class names (and functions, in strict mode)
fn lexical_names(s: &Stmt, out: &mut Vec<(Name, VarKind)>, strict_functions: bool) {
    match s {
        Stmt::Var(k, decls) if *k != VarKind::Var => {
            for (p, _) in decls {
                let mut ns = Vec::new();
                p.names(&mut ns);
                for n in ns {
                    out.push((n, *k));
                }
            }
        },
        Stmt::Class(c) => {
            if let Some(n) = &c.name {
                out.push((n.clone(), VarKind::Let));
            }
        },
        Stmt::Function(f) if !strict_functions => {
            if let Some(n) = &f.name {
                if !out.iter().any(|(x, _)| x == n) {
                    out.push((n.clone(), VarKind::Var));
                }
            }
        },
        _ => {},
    }
}

fn collect_fns_stmt(s: &Stmt, fns: &mut Vec<Rc<FuncNode>>, cls: &mut Vec<Rc<ClassNode>>) {
    match s {
        Stmt::Expr(e) | Stmt::Throw(e) => collect_fns_expr(e, fns, cls),
        Stmt::Var(_, decls) => {
            for (p, i) in decls {
                collect_fns_pat(p, fns, cls);
                if let Some(e) = i {
                    collect_fns_expr(e, fns, cls);
                }
            }
        },
        Stmt::Function(f) => fns.push(f.clone()),
        Stmt::Class(c) => cls.push(c.clone()),
        Stmt::Return(Some(e)) => collect_fns_expr(e, fns, cls),
        Stmt::If(t, a, b) => {
            collect_fns_expr(t, fns, cls);
            collect_fns_stmt(a, fns, cls);
            if let Some(b) = b {
                collect_fns_stmt(b, fns, cls);
            }
        },
        Stmt::Block(b) => {
            for x in b {
                collect_fns_stmt(x, fns, cls);
            }
        },
        Stmt::For(init, t, u, body) => {
            match init {
                Some(ForInit::Var(_, decls)) => {
                    for (p, i) in decls {
                        collect_fns_pat(p, fns, cls);
                        if let Some(e) = i {
                            collect_fns_expr(e, fns, cls);
                        }
                    }
                },
                Some(ForInit::Expr(e)) => collect_fns_expr(e, fns, cls),
                None => {},
            }
            if let Some(t) = t {
                collect_fns_expr(t, fns, cls);
            }
            if let Some(u) = u {
                collect_fns_expr(u, fns, cls);
            }
            collect_fns_stmt(body, fns, cls);
        },
        Stmt::ForIn(h, e, b) | Stmt::ForOf(h, e, b) | Stmt::ForAwaitOf(h, e, b) => {
            match h {
                ForHead::Var(_, p) | ForHead::Target(p) => collect_fns_pat(p, fns, cls),
            }
            collect_fns_expr(e, fns, cls);
            collect_fns_stmt(b, fns, cls);
        },
        Stmt::While(e, b) | Stmt::DoWhile(b, e) => {
            collect_fns_expr(e, fns, cls);
            collect_fns_stmt(b, fns, cls);
        },
        Stmt::Labeled(_, b) => collect_fns_stmt(b, fns, cls),
        Stmt::Try(b, c, f) => {
            for x in b {
                collect_fns_stmt(x, fns, cls);
            }
            if let Some((p, cb)) = c {
                if let Some(p) = p {
                    collect_fns_pat(p, fns, cls);
                }
                for x in cb {
                    collect_fns_stmt(x, fns, cls);
                }
            }
            if let Some(fb) = f {
                for x in fb {
                    collect_fns_stmt(x, fns, cls);
                }
            }
        },
        Stmt::Switch(d, cases) => {
            collect_fns_expr(d, fns, cls);
            for c in cases {
                if let Some(t) = &c.test {
                    collect_fns_expr(t, fns, cls);
                }
                for x in &c.body {
                    collect_fns_stmt(x, fns, cls);
                }
            }
        },
        _ => {},
    }
}

fn collect_fns_pat(p: &Pattern, fns: &mut Vec<Rc<FuncNode>>, cls: &mut Vec<Rc<ClassNode>>) {
    match p {
        Pattern::Ident(_) => {},
        Pattern::Expr(e) => collect_fns_expr(e, fns, cls),
        Pattern::Array(items, rest) => {
            for pe in items.iter().flatten() {
                collect_fns_pat(&pe.target, fns, cls);
                if let Some(d) = &pe.default {
                    collect_fns_expr(d, fns, cls);
                }
            }
            if let Some(r) = rest {
                collect_fns_pat(r, fns, cls);
            }
        },
        Pattern::Object(props, rest) => {
            for (k, pe) in props {
                if let PropKey::Computed(e) = k {
                    collect_fns_expr(e, fns, cls);
                }
                collect_fns_pat(&pe.target, fns, cls);
                if let Some(d) = &pe.default {
                    collect_fns_expr(d, fns, cls);
                }
            }
            if let Some(r) = rest {
                collect_fns_pat(r, fns, cls);
            }
        },
    }
}

fn collect_fns_expr(e: &Expr, fns: &mut Vec<Rc<FuncNode>>, cls: &mut Vec<Rc<ClassNode>>) {
    let mut sub = |x: &Expr| collect_fns_expr(x, fns, cls);
    match e {
        Expr::ClassField(_, Some(x)) | Expr::Import(x) => sub(x),
        Expr::Function(f) => fns.push(f.clone()),
        Expr::Class(c) => cls.push(c.clone()),
        Expr::Template(_, xs) => xs.iter().for_each(&mut sub),
        Expr::Tagged(t, _, xs) => {
            sub(t);
            xs.iter().for_each(sub);
        },
        Expr::Array(items) => items.iter().flatten().for_each(sub),
        Expr::Object(props) => {
            for p in props {
                match p {
                    ObjProp::Spread(x) => collect_fns_expr(x, fns, cls),
                    ObjProp::Prop(k, _, v) => {
                        if let PropKey::Computed(x) = k {
                            collect_fns_expr(x, fns, cls);
                        }
                        collect_fns_expr(v, fns, cls);
                    },
                }
            }
        },
        Expr::Unary(_, x) | Expr::Update(_, _, x) | Expr::Spread(x) | Expr::Await(x) | Expr::OptChain(x) => sub(x),
        Expr::Yield(Some(x), _) => sub(x),
        Expr::Binary(_, a, b) | Expr::Logical(_, a, b) | Expr::Index(a, b, _) => {
            sub(a);
            sub(b);
        },
        Expr::Assign(_, p, v) => {
            collect_fns_pat(p, fns, cls);
            collect_fns_expr(v, fns, cls);
        },
        Expr::Cond(a, b, c) => {
            sub(a);
            sub(b);
            sub(c);
        },
        Expr::Call(c, args, _) | Expr::New(c, args) => {
            sub(c);
            args.iter().for_each(sub);
        },
        Expr::Member(o, _, _) => sub(o),
        Expr::Seq(xs) => xs.iter().for_each(sub),
        _ => {},
    }
}

fn class_fns(c: &ClassNode) -> Vec<Rc<FuncNode>> {
    let mut out = Vec::new();
    if let Some(f) = &c.ctor {
        out.push(f.clone());
    }
    for m in &c.members {
        match &m.kind {
            ClassMemberKind::Method(f, _) | ClassMemberKind::StaticBlock(f) => out.push(f.clone()),
            ClassMemberKind::Field(_) => {},
        }
    }
    out.extend(c.field_init.iter().cloned());
    out.extend(c.static_init.iter().cloned());
    out
}

/// does the body (not counting nested non-arrow functions) mention `name`?
fn refers_to(body: &[Stmt], params: &[Param], name: &str) -> bool {
    struct W<'n> {
        name: &'n str,
        hit: bool,
    }
    impl W<'_> {
        fn e(&mut self, e: &Expr) {
            if self.hit {
                return;
            }
            match e {
                Expr::Ident(n) if &**n == self.name => self.hit = true,
                Expr::Function(f) => {
                    if f.arrow {
                        for p in &f.params {
                            if let Some(d) = &p.default {
                                self.e(d);
                            }
                        }
                        for s in &f.body {
                            self.s(s);
                        }
                    }
                },
                _ => walk_expr_children(e, self),
            }
        }
        fn p(&mut self, p: &Pattern) {
            match p {
                Pattern::Ident(n) if &**n == self.name => self.hit = true,
                Pattern::Expr(e) => self.e(e),
                Pattern::Array(items, rest) => {
                    for pe in items.iter().flatten() {
                        self.p(&pe.target);
                        if let Some(d) = &pe.default {
                            self.e(d);
                        }
                    }
                    if let Some(r) = rest {
                        self.p(r);
                    }
                },
                Pattern::Object(props, rest) => {
                    for (k, pe) in props {
                        if let PropKey::Computed(x) = k {
                            self.e(x);
                        }
                        self.p(&pe.target);
                        if let Some(d) = &pe.default {
                            self.e(d);
                        }
                    }
                    if let Some(r) = rest {
                        self.p(r);
                    }
                },
                _ => {},
            }
        }
        fn s(&mut self, s: &Stmt) {
            walk_stmt_children(s, self);
            for_each_substmt(s, &mut |st| self.s(st));
        }
    }
    impl Visit for W<'_> {
        fn ex(&mut self, e: &Expr) {
            self.e(e)
        }
        fn decl_pat(&mut self, p: &Pattern) {
            self.p(p)
        }
        fn target_pat(&mut self, p: &Pattern) {
            self.p(p)
        }
    }
    let mut w = W { name, hit: false };
    for p in params {
        if let Some(d) = &p.default {
            w.e(d);
        }
        w.p(&p.pat);
    }
    for s in body {
        w.s(s);
    }
    w.hit
}

/// what a tree walk does with the expressions and patterns it meets
trait Visit {
    fn ex(&mut self, e: &Expr);
    /// a declaration's pattern (var / let / const / parameters)
    fn decl_pat(&mut self, p: &Pattern);
    /// an assignment target
    fn target_pat(&mut self, p: &Pattern);
}

/// the expressions and patterns directly in a statement (not its sub-statements)
fn walk_stmt_children(s: &Stmt, v: &mut dyn Visit) {
    walk_stmt_children_(s, v)
}

fn walk_stmt_children_(s: &Stmt, v: &mut dyn Visit) {
    macro_rules! fe {
        ($e:expr) => {
            v.ex($e)
        };
    }
    macro_rules! fp {
        ($p:expr) => {
            v.decl_pat($p)
        };
    }
    match s {
        Stmt::Expr(e) | Stmt::Throw(e) => fe!(e),
        Stmt::Return(Some(e)) => fe!(e),
        Stmt::Var(_, decls) => {
            for (p, i) in decls {
                fp!(p);
                if let Some(e) = i {
                    fe!(e);
                }
            }
        },
        Stmt::If(t, _, _) | Stmt::While(t, _) | Stmt::DoWhile(_, t) => fe!(t),
        Stmt::For(init, t, u, _) => {
            match init {
                Some(ForInit::Var(_, decls)) => {
                    for (p, i) in decls {
                        fp!(p);
                        if let Some(e) = i {
                            fe!(e);
                        }
                    }
                },
                Some(ForInit::Expr(e)) => fe!(e),
                None => {},
            }
            if let Some(t) = t {
                fe!(t);
            }
            if let Some(u) = u {
                fe!(u);
            }
        },
        Stmt::ForIn(h, e, _) | Stmt::ForOf(h, e, _) | Stmt::ForAwaitOf(h, e, _) => {
            match h {
                ForHead::Var(_, p) => fp!(p),
                ForHead::Target(p) => v.target_pat(p),
            }
            fe!(e);
        },
        Stmt::Try(_, Some((Some(p), _)), _) => fp!(p),
        Stmt::Switch(d, cases) => {
            fe!(d);
            for c in cases {
                if let Some(t) = &c.test {
                    fe!(t);
                }
            }
        },
        Stmt::Class(c) => {
            if let Some(x) = &c.extends {
                fe!(x);
            }
        },
        _ => {},
    }
}

fn for_each_substmt(s: &Stmt, f: &mut dyn FnMut(&Stmt)) {
    match s {
        Stmt::If(_, a, b) => {
            f(a);
            if let Some(b) = b {
                f(b);
            }
        },
        Stmt::Block(b) => b.iter().for_each(f),
        Stmt::For(_, _, _, b) | Stmt::ForIn(_, _, b) | Stmt::ForOf(_, _, b) | Stmt::ForAwaitOf(_, _, b) | Stmt::While(_, b) | Stmt::DoWhile(b, _) | Stmt::Labeled(_, b) => f(b),
        Stmt::Try(b, c, fi) => {
            b.iter().for_each(&mut *f);
            if let Some((_, cb)) = c {
                cb.iter().for_each(&mut *f);
            }
            if let Some(fb) = fi {
                fb.iter().for_each(&mut *f);
            }
        },
        Stmt::Switch(_, cases) => {
            for c in cases {
                c.body.iter().for_each(&mut *f);
            }
        },
        _ => {},
    }
}

fn walk_expr_children(e: &Expr, v: &mut dyn Visit) {
    match e {
        Expr::ClassField(_, Some(x)) | Expr::Import(x) => v.ex(x),
        Expr::Template(_, xs) | Expr::Seq(xs) => xs.iter().for_each(|x| v.ex(x)),
        Expr::Tagged(t, _, xs) => {
            v.ex(t);
            xs.iter().for_each(|x| v.ex(x));
        },
        Expr::Array(items) => items.iter().flatten().for_each(|x| v.ex(x)),
        Expr::Object(props) => {
            for p in props {
                match p {
                    ObjProp::Spread(x) => v.ex(x),
                    ObjProp::Prop(k, _, val) => {
                        if let PropKey::Computed(x) = k {
                            v.ex(x);
                        }
                        v.ex(val);
                    },
                }
            }
        },
        Expr::Unary(_, x) | Expr::Update(_, _, x) | Expr::Spread(x) | Expr::Await(x) | Expr::OptChain(x) => v.ex(x),
        Expr::Yield(Some(x), _) => v.ex(x),
        Expr::Binary(_, a, b) | Expr::Logical(_, a, b) | Expr::Index(a, b, _) => {
            v.ex(a);
            v.ex(b);
        },
        Expr::Assign(_, p, val) => {
            v.target_pat(p);
            v.ex(val);
        },
        Expr::Cond(a, b, c) => {
            v.ex(a);
            v.ex(b);
            v.ex(c);
        },
        Expr::Call(c, args, _) | Expr::New(c, args) => {
            v.ex(c);
            args.iter().for_each(|x| v.ex(x));
        },
        Expr::Member(o, _, _) => v.ex(o),
        Expr::Class(c) => {
            if let Some(x) = &c.extends {
                v.ex(x);
            }
        },
        _ => {},
    }
}

/// free-variable analysis of one function
struct FreeWalker<'c, 'a> {
    scopes: Vec<BTreeSet<Name>>,
    free: BTreeSet<Name>,
    comp: &'c mut Compiler<'a>,
}

impl FreeWalker<'_, '_> {
    fn declared(&self, n: &str) -> bool {
        self.scopes.iter().any(|s| s.contains(n))
    }

    fn reference(&mut self, n: &Name) {
        if !self.declared(n) {
            self.free.insert(n.clone());
        }
    }

    fn inner_fn(&mut self, f: &Rc<FuncNode>) {
        let fv = self.comp.free_vars(f);
        for n in fv.iter() {
            if !self.declared(n) {
                self.free.insert(n.clone());
            }
        }
    }

    fn block(&mut self, body: &[Stmt]) {
        let mut lex = Vec::new();
        for s in body {
            lexical_names(s, &mut lex, false);
            if let Stmt::Function(f) = s {
                if let Some(n) = &f.name {
                    lex.push((n.clone(), VarKind::Let));
                }
            }
        }
        self.scopes.push(lex.into_iter().map(|(n, _)| n).collect());
        for s in body {
            self.stmt(s);
        }
        self.scopes.pop();
    }

    fn stmt(&mut self, s: &Stmt) {
        match s {
            Stmt::Function(f) => self.inner_fn(f),
            Stmt::Class(c) => self.class(c),
            Stmt::Block(b) => self.block(b),
            Stmt::Try(b, c, f) => {
                self.block(b);
                if let Some((p, cb)) = c {
                    let mut ns = Vec::new();
                    if let Some(p) = p {
                        p.names(&mut ns);
                    }
                    self.scopes.push(ns.into_iter().collect());
                    if let Some(p) = p {
                        self.pattern_exprs(p);
                    }
                    self.block(cb);
                    self.scopes.pop();
                }
                if let Some(fb) = f {
                    self.block(fb);
                }
            },
            Stmt::For(init, t, u, body) => {
                let mut ns = Vec::new();
                if let Some(ForInit::Var(k, decls)) = init {
                    if *k != VarKind::Var {
                        for (p, _) in decls {
                            p.names(&mut ns);
                        }
                    }
                }
                self.scopes.push(ns.into_iter().collect());
                match init {
                    Some(ForInit::Var(_, decls)) => {
                        for (p, i) in decls {
                            self.pattern_exprs(p);
                            if let Some(e) = i {
                                self.expr(e);
                            }
                        }
                    },
                    Some(ForInit::Expr(e)) => self.expr(e),
                    None => {},
                }
                if let Some(t) = t {
                    self.expr(t);
                }
                if let Some(u) = u {
                    self.expr(u);
                }
                self.stmt(body);
                self.scopes.pop();
            },
            Stmt::ForIn(h, e, body) | Stmt::ForOf(h, e, body) | Stmt::ForAwaitOf(h, e, body) => {
                self.expr(e);
                let mut ns = Vec::new();
                if let ForHead::Var(k, p) = h {
                    if *k != VarKind::Var {
                        p.names(&mut ns);
                    }
                }
                self.scopes.push(ns.into_iter().collect());
                match h {
                    ForHead::Var(_, p) => self.pattern_exprs(p),
                    ForHead::Target(p) => self.pattern_refs(p),
                }
                self.stmt(body);
                self.scopes.pop();
            },
            Stmt::Switch(d, cases) => {
                self.expr(d);
                let all: Vec<Stmt> = cases.iter().flat_map(|c| c.body.iter().cloned()).collect();
                let mut lex = Vec::new();
                for s in &all {
                    lexical_names(s, &mut lex, false);
                }
                self.scopes.push(lex.into_iter().map(|(n, _)| n).collect());
                for c in cases {
                    if let Some(t) = &c.test {
                        self.expr(t);
                    }
                    for s in &c.body {
                        self.stmt(s);
                    }
                }
                self.scopes.pop();
            },
            _ => {
                walk_stmt_children(s, self);
                for_each_substmt(s, &mut |st| self.stmt(st));
            },
        }
    }

    fn class(&mut self, c: &Rc<ClassNode>) {
        if let Some(x) = &c.extends {
            self.expr(x);
        }
        for m in &c.members {
            if let PropKey::Computed(k) = &m.key {
                self.expr(k);
            }
        }
        for f in class_fns(c) {
            self.inner_fn(&f);
        }
    }

    /// a declaration pattern: its defaults and computed keys are expressions
    fn pattern_exprs(&mut self, p: &Pattern) {
        match p {
            Pattern::Ident(_) => {},
            Pattern::Expr(e) => self.expr(e),
            Pattern::Array(items, rest) => {
                for pe in items.iter().flatten() {
                    self.pattern_exprs(&pe.target);
                    if let Some(d) = &pe.default {
                        self.expr(d);
                    }
                }
                if let Some(r) = rest {
                    self.pattern_exprs(r);
                }
            },
            Pattern::Object(props, rest) => {
                for (k, pe) in props {
                    if let PropKey::Computed(x) = k {
                        self.expr(x);
                    }
                    self.pattern_exprs(&pe.target);
                    if let Some(d) = &pe.default {
                        self.expr(d);
                    }
                }
                if let Some(r) = rest {
                    self.pattern_exprs(r);
                }
            },
        }
    }

    /// an assignment pattern: its names are references
    fn pattern_refs(&mut self, p: &Pattern) {
        let mut ns = Vec::new();
        p.names(&mut ns);
        for n in &ns {
            self.reference(n);
        }
        self.pattern_exprs(p);
    }

    fn expr(&mut self, e: &Expr) {
        match e {
            Expr::Ident(n) => self.reference(n),
            Expr::This => self.reference(&Rc::from("this")),
            Expr::NewTarget => self.reference(&Rc::from("%nt")),
            Expr::Super => {
                self.reference(&Rc::from("this"));
                self.reference(&Rc::from("%ctor"));
                self.reference(&Rc::from("%nt"));
            },
            Expr::Function(f) => self.inner_fn(f),
            Expr::Class(c) => {
                let mut s = BTreeSet::new();
                if let Some(n) = &c.name {
                    s.insert(n.clone());
                }
                self.scopes.push(s);
                self.class(c);
                self.scopes.pop();
            },
            Expr::Assign(_, p, v) => {
                self.pattern_refs(p);
                self.expr(v);
            },
            _ => walk_expr_children(e, self),
        }
    }
}

impl Visit for FreeWalker<'_, '_> {
    fn ex(&mut self, e: &Expr) {
        self.expr(e)
    }
    fn decl_pat(&mut self, p: &Pattern) {
        self.pattern_exprs(p)
    }
    fn target_pat(&mut self, p: &Pattern) {
        self.pattern_refs(p)
    }
}

// the label-carrying state lives on the compiler (labels wait for the loop they name)
impl<'a> Compiler<'a> {
    #[allow(dead_code)]
    fn _unused(&self) {}
}

pub(crate) fn _box<T>(t: T) -> Box<T> {
    Box::new(t)
}

pub(crate) fn _s() -> String {
    String::new()
}

// ── generators, async ──
impl<'a> Compiler<'a> {
    fn yield_expr(&mut self, arg: Option<&Expr>, delegate: bool) -> CResult<()> {
        let async_gen = self.f().async_gen;
        if !delegate {
            match arg {
                Some(x) => self.expr(x)?,
                None => {
                    self.emit(Op::Undef);
                },
            }
            if async_gen {
                // an async generator awaits what it yields
                self.emit(Op::Yield(2));
            }
            self.emit(Op::Yield(0));
            return Ok(());
        }
        let x = arg.unwrap();
        if async_gen {
            // yield* in an async generator: each value of the (async) iterable, awaited and yielded
            let it = self.temp();
            self.emit(Op::Intrinsic(intr::GET_ASYNC_ITERATOR));
            self.expr(x)?;
            self.emit(Op::Call(1));
            self.emit(Op::SetLocal(it));
            self.emit(Op::Pop);
            let top = self.here();
            self.emit(Op::GetLocal(it));
            let next_a = self.atom("next");
            self.emit(Op::GetPropKeep(next_a));
            self.emit(Op::CallMethod(0));
            self.emit(Op::Yield(2));
            self.emit(Op::Dup);
            let done_a = self.atom("done");
            self.emit(Op::GetProp(done_a));
            let exit = self.emit(Op::JumpIfTrue(0));
            let value_a = self.atom("value");
            self.emit(Op::GetProp(value_a));
            self.emit(Op::Yield(0));
            self.emit(Op::Pop);
            self.emit(Op::Jump(top));
            self.patch_here(exit);
            self.emit(Op::GetProp(value_a));
            self.free_temp(it);
            return Ok(());
        }
        let it = self.temp();
        let recv = self.temp();
        let mode = self.temp();
        self.emit(Op::Intrinsic(intr::GET_ITERATOR));
        self.expr(x)?;
        self.emit(Op::Call(1));
        self.emit(Op::SetLocal(it));
        self.emit(Op::Pop);
        self.emit(Op::Undef);
        self.emit(Op::SetLocal(recv));
        self.emit(Op::Pop);
        self.emit(Op::Int(0));
        self.emit(Op::SetLocal(mode));
        self.emit(Op::Pop);
        let top = self.here();
        self.emit(Op::Intrinsic(intr::YIELD_STEP));
        self.emit(Op::GetLocal(it));
        self.emit(Op::GetLocal(mode));
        self.emit(Op::GetLocal(recv));
        self.emit(Op::Call(3));
        self.emit(Op::Dup);
        let done_a = self.atom("done");
        self.emit(Op::GetProp(done_a));
        let done = self.emit(Op::JumpIfTrue(0));
        self.emit(Op::Yield(1));
        self.emit(Op::SetLocal(mode));
        self.emit(Op::Pop);
        self.emit(Op::SetLocal(recv));
        self.emit(Op::Pop);
        self.emit(Op::Jump(top));
        self.patch_here(done);
        let value_a = self.atom("value");
        self.emit(Op::GetProp(value_a));
        self.emit(Op::GetLocal(mode));
        self.emit(Op::Int(2));
        self.emit(Op::StrictEq);
        let not_ret = self.emit(Op::JumpIfFalse(0));
        self.exit_to(None, true)?;
        self.emit(Op::Return);
        self.patch_here(not_ret);
        self.free_temp(mode);
        self.free_temp(recv);
        self.free_temp(it);
        Ok(())
    }

    fn for_await_of(&mut self, head: &ForHead, obj: &Expr, body: &Stmt) -> CResult<()> {
        let labels = self.take_labels();
        self.emit(Op::Intrinsic(intr::GET_ASYNC_ITERATOR));
        self.expr(obj)?;
        self.emit(Op::Call(1));
        let it = self.new_local();
        self.emit(Op::SetLocal(it));
        self.emit(Op::Pop);
        let top = self.here();
        self.emit(Op::GetLocal(it));
        let next_a = self.atom("next");
        self.emit(Op::GetPropKeep(next_a));
        self.emit(Op::CallMethod(0));
        self.emit(Op::Yield(2));
        self.emit(Op::Dup);
        let done_a = self.atom("done");
        self.emit(Op::GetProp(done_a));
        let exit = self.emit(Op::JumpIfTrue(0));
        let value_a = self.atom("value");
        self.emit(Op::GetProp(value_a));
        let outer = self.env_depth();
        let scoped = self.for_head_scope(head)?;
        self.for_head_assign(head)?;
        let inner = self.env_depth();
        self.push_ctl(Ctl { labels, is_loop: true, ..Default::default() }, outer, inner);
        self.sub_stmt(body)?;
        let ctl = self.f().ctl.pop().unwrap();
        let cont = self.here();
        for c in ctl.continues {
            self.patch(c, cont);
        }
        if scoped {
            self.leave_block();
        }
        self.emit(Op::Jump(top));
        self.patch_here(exit);
        self.emit(Op::Pop);
        let skip = self.emit(Op::Jump(0));
        for b in ctl.breaks {
            self.patch_here(b);
        }
        // a break closes the iterator: await it.return()
        self.emit(Op::GetLocal(it));
        let ret_a = self.atom("return");
        self.emit(Op::GetPropKeep(ret_a));
        let j = self.emit(Op::OptCheckCall(0));
        self.emit(Op::CallMethod(0));
        self.emit(Op::Yield(2));
        self.patch_here(j);
        self.emit(Op::Pop);
        self.patch_here(skip);
        Ok(())
    }
}

// ── modules: a module becomes a generator function ──
//
// function* (%ns, %meta, %export, %m0, %m1, ...) {
//     %export(%ns, "name", () => binding) ...   (live export getters)
//     yield;                                    (instantiated: its exports exist)
//     ...the module's body...                   (evaluated by the next resume; await = TLA)
// }
//
// %mN is the namespace object of the N-th module it requests; an imported name compiles to a
// read of that namespace (so imports are live). The script made from a module evaluates to
// [the function, [requested specifiers], [indexes of `export *` sources]].

fn arrow_returning(e: Expr) -> Expr {
    Expr::Function(Rc::new(FuncNode {
        name: None,
        params: vec![],
        body: vec![Stmt::Return(Some(e))],
        arrow: true,
        expr_body: true,
        strict: true,
        is_async: false,
        generator: false,
        method: false,
        kind: FuncKind::Normal,
        src: (0, 0),
        decl: false,
    }))
}

pub fn compile_module(atoms: &mut Atoms, src: &str) -> Result<Rc<FuncProto>, CompileError> {
    use crate::parser::ModuleItem;
    let items = crate::parser::parse_module(src).map_err(|e| CompileError(format!("{} (line {})", e.msg, e.line)))?;
    let mut requests: Vec<JsStr> = Vec::new();
    let mut req = |s: &JsStr| -> usize {
        match requests.iter().position(|x| x == s) {
            Some(i) => i,
            None => {
                requests.push(s.clone());
                requests.len() - 1
            },
        }
    };
    let param = |i: usize| -> Name { Rc::from(format!("%m{}", i).as_str()) };
    let mut imports: BTreeMap<String, (Name, Option<Name>)> = BTreeMap::new();
    let mut exports: Vec<(Name, Expr)> = Vec::new();
    let mut stars: Vec<usize> = Vec::new();
    let mut body: Vec<Stmt> = Vec::new();
    for it in items {
        match it {
            ModuleItem::Stmt(s) => body.push(s),
            ModuleItem::Import { spec, default, ns, named } => {
                let i = req(&spec);
                if let Some(d) = default {
                    imports.insert(String::from(&*d), (param(i), Some(Rc::from("default"))));
                }
                if let Some(n) = ns {
                    imports.insert(String::from(&*n), (param(i), None));
                }
                for (imported, local) in named {
                    imports.insert(String::from(&*local), (param(i), Some(imported)));
                }
            },
            ModuleItem::ExportDecl(s) => {
                let mut names = Vec::new();
                match &s {
                    Stmt::Var(_, decls) => {
                        for (p, _) in decls {
                            p.names(&mut names);
                        }
                    },
                    Stmt::Function(f) => names.extend(f.name.clone()),
                    Stmt::Class(c) => names.extend(c.name.clone()),
                    _ => {},
                }
                for n in names {
                    exports.push((n.clone(), Expr::Ident(n)));
                }
                body.push(s);
            },
            ModuleItem::ExportDefaultDecl(s) => {
                let n = match &s {
                    Stmt::Function(f) => f.name.clone(),
                    Stmt::Class(c) => c.name.clone(),
                    _ => None,
                }
                .unwrap_or_else(|| Rc::from("default"));
                exports.push((Rc::from("default"), Expr::Ident(n)));
                body.push(s);
            },
            ModuleItem::ExportDefaultExpr(e) => {
                let n: Name = Rc::from("default");
                body.push(Stmt::Var(VarKind::Let, vec![(Pattern::Ident(n.clone()), Some(e))]));
                exports.push((n.clone(), Expr::Ident(n)));
            },
            ModuleItem::ExportNamed { specs, from } => match from {
                None => {
                    for (local, exported) in specs {
                        exports.push((exported, Expr::Ident(local)));
                    }
                },
                Some(s) => {
                    let i = req(&s);
                    for (name, exported) in specs {
                        exports.push((exported, Expr::Member(Box::new(Expr::Ident(param(i))), name, false)));
                    }
                },
            },
            ModuleItem::ExportAll { from, as_name } => {
                let i = req(&from);
                match as_name {
                    Some(n) => exports.push((n, Expr::Ident(param(i)))),
                    None => stars.push(i),
                }
            },
        }
    }
    let mut stmts = Vec::new();
    for (name, getter) in exports {
        stmts.push(Stmt::Expr(Expr::Call(
            Box::new(Expr::Ident(Rc::from("%export"))),
            vec![Expr::Ident(Rc::from("%ns")), Expr::Str(js(&name)), arrow_returning(getter)],
            false,
        )));
    }
    stmts.push(Stmt::Expr(Expr::Yield(None, false)));
    stmts.extend(body);
    let mut params: Vec<Param> = ["%ns", "%meta", "%export"].iter().map(|n| Param { pat: Pattern::Ident(Rc::from(*n)), default: None, rest: false }).collect();
    for i in 0..requests.len() {
        params.push(Param { pat: Pattern::Ident(param(i)), default: None, rest: false });
    }
    let f = Rc::new(FuncNode {
        name: None,
        params,
        body: stmts,
        arrow: false,
        expr_body: false,
        strict: true,
        is_async: false,
        generator: true,
        method: false,
        kind: FuncKind::Normal,
        src: (0, src.len()),
        decl: false,
    });
    let reqs = Expr::Array(requests.iter().map(|s| Some(Expr::Str(s.clone()))).collect());
    let st = Expr::Array(stars.iter().map(|i| Some(Expr::Num(*i as f64))).collect());
    let prog = crate::parser::Program { body: vec![Stmt::Expr(Expr::Array(vec![Some(Expr::Function(f)), Some(reqs), Some(st)]))], strict: true };
    let mut c = Compiler::new(atoms, src);
    c.module_imports = imports;
    c.compile_script(&prog)
}
