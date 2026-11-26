//! The parser: tokens -> syntax tree. Hand-written recursive descent with precedence climbing;
//! arrow parameters and assignment patterns are read as expressions first and converted.

use crate::ast::*;
use crate::lexer::{Lexer, Tok, Token};
use crate::strs::JsStr;
use alloc::boxed::Box;
use alloc::format;
use alloc::rc::Rc;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

#[derive(Debug, Clone)]
pub struct SyntaxError {
    pub msg: String,
    pub line: u32,
}

type PResult<T> = Result<T, SyntaxError>;

pub struct Program {
    pub body: Vec<Stmt>,
    pub strict: bool,
}

const RESERVED: &[&str] = &[
    "break", "case", "catch", "class", "const", "continue", "debugger", "default", "delete", "do", "else", "export",
    "extends", "finally", "for", "function", "if", "import", "in", "instanceof", "new", "return", "super", "switch",
    "this", "throw", "try", "typeof", "var", "void", "while", "with", "null", "true", "false", "enum",
];

#[derive(Clone, Copy)]
struct Ctx {
    in_function: bool,
    generator: bool,
    is_async: bool,
    strict: bool,
}

pub struct Parser {
    lx: Lexer,
    cur: Token,
    prev_end: usize,
    prev_line: u32,
    ctx: Ctx,
    /// inside `${ }` of a template: how deep, so a `}` there continues the template
    tpl_depth: Vec<u32>,
    brace_depth: u32,
}

pub fn parse_program(src: &str) -> PResult<Program> {
    let mut p = Parser::new(src)?;
    let strict = p.directives();
    p.ctx.strict = strict;
    let mut body = Vec::new();
    while p.cur.tok != Tok::Eof {
        body.push(p.statement()?);
    }
    Ok(Program { body, strict })
}

fn name(s: &str) -> Name {
    Rc::from(s)
}

impl Parser {
    fn new(src: &str) -> PResult<Self> {
        let mut lx = Lexer::new(src);
        let cur = lx.next_token().map_err(|m| SyntaxError { msg: m, line: 1 })?;
        Ok(Parser {
            lx,
            cur,
            prev_end: 0,
            prev_line: 1,
            ctx: Ctx { in_function: false, generator: false, is_async: false, strict: false },
            tpl_depth: Vec::new(),
            brace_depth: 0,
        })
    }

    fn err<T>(&self, msg: &str) -> PResult<T> {
        Err(SyntaxError { msg: msg.to_string(), line: self.cur.line })
    }

    fn unexpected<T>(&self) -> PResult<T> {
        let what = match &self.cur.tok {
            Tok::Eof => "end of input".to_string(),
            Tok::Punct(p) => format!("'{}'", p),
            Tok::Ident(s, _) => format!("'{}'", s),
            Tok::Num(_) => "a number".to_string(),
            Tok::BigInt(_) => "a BigInt".to_string(),
            Tok::Str(_) => "a string".to_string(),
            Tok::Template(..) => "a template".to_string(),
            Tok::Regex(..) => "a regular expression".to_string(),
            Tok::PrivateName(n) => format!("'#{}'", n),
        };
        self.err(&format!("unexpected {}", what))
    }

    fn advance(&mut self) -> PResult<Token> {
        let next = self.lx.next_token().map_err(|m| SyntaxError { msg: m, line: self.lx.line })?;
        let old = core::mem::replace(&mut self.cur, next);
        self.prev_end = old.end;
        self.prev_line = old.line;
        match old.tok {
            Tok::Punct("{") => self.brace_depth += 1,
            Tok::Punct("}") => self.brace_depth = self.brace_depth.saturating_sub(1),
            _ => {},
        }
        Ok(old)
    }

    fn is(&self, p: &str) -> bool {
        matches!(&self.cur.tok, Tok::Punct(q) if *q == p)
    }
    fn is_kw(&self, k: &str) -> bool {
        matches!(&self.cur.tok, Tok::Ident(s, false) if s == k)
    }
    fn eat(&mut self, p: &str) -> PResult<bool> {
        if self.is(p) {
            self.advance()?;
            Ok(true)
        } else {
            Ok(false)
        }
    }
    fn eat_kw(&mut self, k: &str) -> PResult<bool> {
        if self.is_kw(k) {
            self.advance()?;
            Ok(true)
        } else {
            Ok(false)
        }
    }
    fn expect(&mut self, p: &str) -> PResult<()> {
        if self.eat(p)? {
            Ok(())
        } else {
            let t = self.cur.clone();
            let _ = t;
            self.err(&format!("expected '{}'", p)).or_else(|e: SyntaxError| {
                let u: PResult<()> = self.unexpected();
                Err(SyntaxError { msg: format!("{} ({})", e.msg, u.err().map(|x| x.msg).unwrap_or_default()), line: e.line })
            })
        }
    }

    fn peek(&self) -> PResult<Token> {
        let mut l = self.lx.clone();
        l.next_token().map_err(|m| SyntaxError { msg: m, line: self.lx.line })
    }

    fn semicolon(&mut self) -> PResult<()> {
        if self.eat(";")? {
            return Ok(());
        }
        if self.is("}") || self.cur.tok == Tok::Eof || self.cur.nl_before {
            return Ok(());
        }
        self.unexpected()
    }

    fn ident_name(&mut self) -> PResult<Name> {
        // any identifier, keywords included (after `.`, as a property key)
        match &self.cur.tok {
            Tok::Ident(s, _) => {
                let n = name(s);
                self.advance()?;
                Ok(n)
            },
            _ => self.unexpected(),
        }
    }

    fn binding_ident(&mut self) -> PResult<Name> {
        match &self.cur.tok {
            Tok::Ident(s, esc) => {
                if !*esc && RESERVED.contains(&s.as_str()) {
                    return self.unexpected();
                }
                if self.ctx.strict && (s == "eval" || s == "arguments") {
                    return self.err("cannot bind eval or arguments in strict mode");
                }
                let n = name(s);
                self.advance()?;
                Ok(n)
            },
            _ => self.unexpected(),
        }
    }

    /// "use strict" and other directives at the start of a body
    fn directives(&mut self) -> bool {
        let mut strict = false;
        let mut l = self.lx.clone();
        let mut t = self.cur.clone();
        loop {
            match &t.tok {
                Tok::Str(s) => {
                    let raw = self.lx.slice(t.start + 1, t.end.saturating_sub(1));
                    let _ = s;
                    let nt = match l.next_token() {
                        Ok(nt) => nt,
                        Err(_) => break,
                    };
                    let ends = matches!(nt.tok, Tok::Punct(";") | Tok::Punct("}") | Tok::Eof) || nt.nl_before;
                    if !ends {
                        break;
                    }
                    if raw == "use strict" {
                        strict = true;
                    }
                    t = if matches!(nt.tok, Tok::Punct(";")) {
                        match l.next_token() {
                            Ok(x) => x,
                            Err(_) => break,
                        }
                    } else {
                        nt
                    };
                },
                _ => break,
            }
        }
        strict
    }

    // ── statements ──

    fn statement(&mut self) -> PResult<Stmt> {
        match &self.cur.tok {
            Tok::Punct("{") => {
                self.advance()?;
                let b = self.block_body()?;
                Ok(Stmt::Block(b))
            },
            Tok::Punct(";") => {
                self.advance()?;
                Ok(Stmt::Empty)
            },
            Tok::Ident(s, false) => {
                let s = s.clone();
                match s.as_str() {
                    "var" => {
                        self.advance()?;
                        let d = self.var_decls(VarKind::Var, false)?;
                        self.semicolon()?;
                        Ok(Stmt::Var(VarKind::Var, d))
                    },
                    "let" | "const" if s == "const" || self.let_is_decl()? => {
                        let k = if s == "let" { VarKind::Let } else { VarKind::Const };
                        self.advance()?;
                        let d = self.var_decls(k, false)?;
                        self.semicolon()?;
                        Ok(Stmt::Var(k, d))
                    },
                    "function" => {
                        let f = self.function(false, true, false)?;
                        Ok(Stmt::Function(f))
                    },
                    "async" if self.async_function_ahead()? => {
                        self.advance()?;
                        let f = self.function(true, true, false)?;
                        Ok(Stmt::Function(f))
                    },
                    "class" => {
                        let c = self.class(true)?;
                        Ok(Stmt::Class(c))
                    },
                    "if" => {
                        self.advance()?;
                        self.expect("(")?;
                        let t = self.expression(false)?;
                        self.expect(")")?;
                        let a = self.sub_statement()?;
                        let b = if self.eat_kw("else")? { Some(Box::new(self.sub_statement()?)) } else { None };
                        Ok(Stmt::If(t, Box::new(a), b))
                    },
                    "for" => self.for_statement(),
                    "while" => {
                        self.advance()?;
                        self.expect("(")?;
                        let t = self.expression(false)?;
                        self.expect(")")?;
                        let b = self.sub_statement()?;
                        Ok(Stmt::While(t, Box::new(b)))
                    },
                    "do" => {
                        self.advance()?;
                        let b = self.sub_statement()?;
                        if !self.eat_kw("while")? {
                            return self.err("expected 'while'");
                        }
                        self.expect("(")?;
                        let t = self.expression(false)?;
                        self.expect(")")?;
                        self.eat(";")?;
                        Ok(Stmt::DoWhile(Box::new(b), t))
                    },
                    "return" => {
                        if !self.ctx.in_function {
                            return self.err("return outside a function");
                        }
                        self.advance()?;
                        let v = if self.is(";") || self.is("}") || self.cur.tok == Tok::Eof || self.cur.nl_before {
                            None
                        } else {
                            Some(self.expression(false)?)
                        };
                        self.semicolon()?;
                        Ok(Stmt::Return(v))
                    },
                    "break" | "continue" => {
                        self.advance()?;
                        let label = match &self.cur.tok {
                            Tok::Ident(l, false) if !self.cur.nl_before && !RESERVED.contains(&l.as_str()) => {
                                let l = name(l);
                                self.advance()?;
                                Some(l)
                            },
                            _ => None,
                        };
                        self.semicolon()?;
                        Ok(if s == "break" { Stmt::Break(label) } else { Stmt::Continue(label) })
                    },
                    "throw" => {
                        self.advance()?;
                        if self.cur.nl_before {
                            return self.err("no line break is allowed after 'throw'");
                        }
                        let e = self.expression(false)?;
                        self.semicolon()?;
                        Ok(Stmt::Throw(e))
                    },
                    "try" => self.try_statement(),
                    "switch" => self.switch_statement(),
                    "debugger" => {
                        self.advance()?;
                        self.semicolon()?;
                        Ok(Stmt::Debugger)
                    },
                    "with" => self.err("'with' is not supported"),
                    "import" if !matches!(self.peek()?.tok, Tok::Punct("(") | Tok::Punct(".")) => self.err("modules are not supported"),
                    "export" => self.err("modules are not supported"),
                    _ => {
                        // a label?
                        if !RESERVED.contains(&s.as_str()) && matches!(self.peek()?.tok, Tok::Punct(":")) {
                            self.advance()?;
                            self.advance()?;
                            let body = if self.is_kw("function") {
                                Stmt::Function(self.function(false, true, false)?)
                            } else {
                                self.statement()?
                            };
                            return Ok(Stmt::Labeled(name(&s), Box::new(body)));
                        }
                        self.expr_statement()
                    },
                }
            },
            _ => self.expr_statement(),
        }
    }

    /// the body of if / while / for: a declaration there is an error, except a function in sloppy mode
    fn sub_statement(&mut self) -> PResult<Stmt> {
        if self.is_kw("class") || self.is_kw("const") {
            return self.err("a declaration is not allowed here");
        }
        self.statement()
    }

    fn expr_statement(&mut self) -> PResult<Stmt> {
        let e = self.expression(false)?;
        self.semicolon()?;
        Ok(Stmt::Expr(e))
    }

    fn let_is_decl(&self) -> PResult<bool> {
        let t = self.peek()?;
        Ok(match &t.tok {
            Tok::Punct("[") | Tok::Punct("{") => true,
            Tok::Ident(s, _) => !(s == "in" || s == "instanceof") ,
            _ => false,
        })
    }

    fn async_function_ahead(&self) -> PResult<bool> {
        let t = self.peek()?;
        Ok(!t.nl_before && matches!(&t.tok, Tok::Ident(s, false) if s == "function"))
    }

    fn block_body(&mut self) -> PResult<Vec<Stmt>> {
        let mut out = Vec::new();
        while !self.is("}") {
            if self.cur.tok == Tok::Eof {
                return self.err("expected '}'");
            }
            out.push(self.statement()?);
        }
        self.advance()?;
        Ok(out)
    }

    fn var_decls(&mut self, kind: VarKind, no_in: bool) -> PResult<Vec<(Pattern, Option<Expr>)>> {
        let mut out = Vec::new();
        loop {
            let pat = self.binding_target()?;
            let init = if self.eat("=")? { Some(self.assign(no_in)?) } else { None };
            if init.is_none() && !no_in {
                if kind == VarKind::Const {
                    return self.err("a const needs a value");
                }
                if !matches!(pat, Pattern::Ident(_)) {
                    return self.err("a destructuring declaration needs a value");
                }
            }
            out.push((pat, init));
            if !self.eat(",")? {
                break;
            }
        }
        Ok(out)
    }

    /// a name, or an array / object binding pattern
    fn binding_target(&mut self) -> PResult<Pattern> {
        if self.is("[") {
            self.advance()?;
            let mut items = Vec::new();
            let mut rest = None;
            loop {
                if self.eat("]")? {
                    break;
                }
                if self.is(",") {
                    self.advance()?;
                    items.push(None);
                    continue;
                }
                if self.eat("...")? {
                    rest = Some(Box::new(self.binding_target()?));
                    self.expect("]")?;
                    break;
                }
                let t = self.binding_target()?;
                let d = if self.eat("=")? { Some(self.assign(false)?) } else { None };
                items.push(Some(PatElem { target: t, default: d }));
                if !self.is("]") {
                    self.expect(",")?;
                }
            }
            return Ok(Pattern::Array(items, rest));
        }
        if self.is("{") {
            self.advance()?;
            let mut props = Vec::new();
            let mut rest = None;
            loop {
                if self.eat("}")? {
                    break;
                }
                if self.eat("...")? {
                    rest = Some(Box::new(Pattern::Ident(self.binding_ident()?)));
                    self.eat(",")?;
                    self.expect("}")?;
                    break;
                }
                let (key, shorthand) = self.prop_key()?;
                let elem = if self.eat(":")? {
                    let t = self.binding_target()?;
                    let d = if self.eat("=")? { Some(self.assign(false)?) } else { None };
                    PatElem { target: t, default: d }
                } else {
                    let n = match shorthand {
                        Some(n) => n,
                        None => return self.err("expected ':'"),
                    };
                    let d = if self.eat("=")? { Some(self.assign(false)?) } else { None };
                    PatElem { target: Pattern::Ident(n), default: d }
                };
                props.push((key, elem));
                if !self.is("}") {
                    self.expect(",")?;
                }
            }
            return Ok(Pattern::Object(props, rest));
        }
        Ok(Pattern::Ident(self.binding_ident()?))
    }

    fn for_statement(&mut self) -> PResult<Stmt> {
        self.advance()?;
        let is_await = self.eat_kw("await")?;
        self.expect("(")?;
        let mut init = None;
        if self.is(";") {
        } else if self.is_kw("var") || self.is_kw("const") || (self.is_kw("let") && self.let_is_decl()?) {
            let kind = match &self.cur.tok {
                Tok::Ident(s, _) if s == "var" => VarKind::Var,
                Tok::Ident(s, _) if s == "let" => VarKind::Let,
                _ => VarKind::Const,
            };
            self.advance()?;
            let decls = self.var_decls(kind, true)?;
            if decls.len() == 1 && decls[0].1.is_none() && (self.is_kw("of") || self.is_kw("in")) {
                let of = self.is_kw("of");
                self.advance()?;
                let rhs = if of { self.assign(false)? } else { self.expression(false)? };
                self.expect(")")?;
                let body = self.sub_statement()?;
                let head = ForHead::Var(kind, decls.into_iter().next().unwrap().0);
                if is_await && of {
                    return Ok(Stmt::ForAwaitOf(head, rhs, Box::new(body)));
                }
                return Ok(if of { Stmt::ForOf(head, rhs, Box::new(body)) } else { Stmt::ForIn(head, rhs, Box::new(body)) });
            }
            // `for (var x = 1 in o)` is legacy; only plain declarations here
            for (p, i) in &decls {
                if i.is_none() && (kind == VarKind::Const || !matches!(p, Pattern::Ident(_))) {
                    return self.err("a declaration in a for loop needs a value");
                }
            }
            init = Some(ForInit::Var(kind, decls));
        } else {
            let start_is_let = self.is_kw("let");
            let e = self.expression(true)?;
            if self.is_kw("of") && !start_is_let || self.is_kw("in") {
                let of = self.is_kw("of");
                self.advance()?;
                let target = self.to_pattern(e)?;
                let rhs = if of { self.assign(false)? } else { self.expression(false)? };
                self.expect(")")?;
                let body = self.sub_statement()?;
                let head = ForHead::Target(target);
                if is_await && of {
                    return Ok(Stmt::ForAwaitOf(head, rhs, Box::new(body)));
                }
                return Ok(if of { Stmt::ForOf(head, rhs, Box::new(body)) } else { Stmt::ForIn(head, rhs, Box::new(body)) });
            }
            init = Some(ForInit::Expr(e));
        }
        self.expect(";")?;
        let test = if self.is(";") { None } else { Some(self.expression(false)?) };
        self.expect(";")?;
        let update = if self.is(")") { None } else { Some(self.expression(false)?) };
        self.expect(")")?;
        let body = self.sub_statement()?;
        Ok(Stmt::For(init, test, update, Box::new(body)))
    }

    fn try_statement(&mut self) -> PResult<Stmt> {
        self.advance()?;
        self.expect("{")?;
        let block = self.block_body()?;
        let mut catch = None;
        let mut finally = None;
        if self.eat_kw("catch")? {
            let param = if self.eat("(")? {
                let p = self.binding_target()?;
                self.expect(")")?;
                Some(p)
            } else {
                None
            };
            self.expect("{")?;
            let body = self.block_body()?;
            catch = Some((param, body));
        }
        if self.eat_kw("finally")? {
            self.expect("{")?;
            finally = Some(self.block_body()?);
        }
        if catch.is_none() && finally.is_none() {
            return self.err("try needs catch or finally");
        }
        Ok(Stmt::Try(block, catch, finally))
    }

    fn switch_statement(&mut self) -> PResult<Stmt> {
        self.advance()?;
        self.expect("(")?;
        let d = self.expression(false)?;
        self.expect(")")?;
        self.expect("{")?;
        let mut cases = Vec::new();
        let mut seen_default = false;
        while !self.eat("}")? {
            let test = if self.eat_kw("case")? {
                Some(self.expression(false)?)
            } else if self.eat_kw("default")? {
                if seen_default {
                    return self.err("more than one default in a switch");
                }
                seen_default = true;
                None
            } else {
                return self.unexpected();
            };
            self.expect(":")?;
            let mut body = Vec::new();
            while !(self.is_kw("case") || self.is_kw("default") || self.is("}")) {
                if self.cur.tok == Tok::Eof {
                    return self.err("expected '}'");
                }
                body.push(self.statement()?);
            }
            cases.push(Case { test, body });
        }
        Ok(Stmt::Switch(d, cases))
    }

    // ── functions and classes ──

    /// at `function` (after `async` when is_async)
    fn function(&mut self, is_async: bool, decl: bool, expr: bool) -> PResult<Rc<FuncNode>> {
        let start = self.cur.start;
        self.advance()?; // function
        let generator = self.eat("*")?;
        let fname = if let Tok::Ident(..) = &self.cur.tok {
            if self.is("(") { None } else { Some(self.binding_ident()?) }
        } else {
            None
        };
        if decl && fname.is_none() && !expr {
            return self.err("a function declaration needs a name");
        }
        self.function_rest(start, fname, is_async, generator, false, FuncKind::Normal, decl)
    }

    /// parameters and body
    fn function_rest(&mut self, start: usize, fname: Option<Name>, is_async: bool, generator: bool, method: bool, kind: FuncKind, decl: bool) -> PResult<Rc<FuncNode>> {
        let saved = self.ctx;
        self.ctx.in_function = true;
        self.ctx.generator = generator;
        self.ctx.is_async = is_async;
        self.expect("(")?;
        let params = self.params()?;
        self.expect("{")?;
        let strict = self.directives() || saved.strict || matches!(kind, FuncKind::ClassConstructor | FuncKind::DerivedConstructor);
        self.ctx.strict = strict;
        let body = self.block_body()?;
        self.ctx = saved;
        Ok(Rc::new(FuncNode {
            name: fname,
            params,
            body,
            arrow: false,
            expr_body: false,
            strict,
            is_async,
            generator,
            method,
            kind,
            src: (start, self.prev_end),
            decl,
        }))
    }

    /// after "(" up to and including ")"
    fn params(&mut self) -> PResult<Vec<Param>> {
        let mut out = Vec::new();
        loop {
            if self.eat(")")? {
                break;
            }
            if self.eat("...")? {
                let p = self.binding_target()?;
                out.push(Param { pat: p, default: None, rest: true });
                self.expect(")")?;
                break;
            }
            let p = self.binding_target()?;
            let d = if self.eat("=")? { Some(self.assign(false)?) } else { None };
            out.push(Param { pat: p, default: d, rest: false });
            if !self.is(")") {
                self.expect(",")?;
            }
        }
        Ok(out)
    }

    fn class(&mut self, decl: bool) -> PResult<Rc<ClassNode>> {
        let cstart = self.cur.start;
        self.advance()?; // class
        let saved = self.ctx;
        self.ctx.strict = true;
        let cname = if matches!(&self.cur.tok, Tok::Ident(s, _) if s != "extends") { Some(self.binding_ident()?) } else { None };
        if decl && cname.is_none() {
            return self.err("a class declaration needs a name");
        }
        let extends = if self.eat_kw("extends")? { Some(self.lhs()?) } else { None };
        self.expect("{")?;
        let mut ctor = None;
        let mut members = Vec::new();
        while !self.eat("}")? {
            if self.eat(";")? {
                continue;
            }
            let start = self.cur.start;
            let mut is_static = false;
            if self.is_kw("static") && !matches!(self.peek()?.tok, Tok::Punct("(") | Tok::Punct("=") | Tok::Punct(";") | Tok::Punct("}")) {
                self.advance()?;
                is_static = true;
                if self.is("{") {
                    self.advance()?;
                    let saved2 = self.ctx;
                    self.ctx.in_function = true;
                    let body = self.block_body()?;
                    self.ctx = saved2;
                    let f = Rc::new(FuncNode { name: None, params: vec![], body, arrow: false, expr_body: false, strict: true, is_async: false, generator: false, method: true, kind: FuncKind::Normal, src: (start, self.prev_end), decl: false });
                    members.push(ClassMember { key: PropKey::Named(name("")), is_static: true, kind: ClassMemberKind::StaticBlock(f) });
                    continue;
                }
            }
            let (is_async, generator, pkind) = self.method_prefix()?;
            let (key, _) = self.prop_key()?;
            if self.is("(") {
                let is_ctor = !is_static && matches!(&key, PropKey::Named(n) if &**n == "constructor") && pkind == PropKind::Init;
                let fk = if is_ctor {
                    if extends.is_some() { FuncKind::DerivedConstructor } else { FuncKind::ClassConstructor }
                } else {
                    match pkind {
                        PropKind::Get => FuncKind::Getter,
                        PropKind::Set => FuncKind::Setter,
                        PropKind::Init => FuncKind::Normal,
                    }
                };
                let fname = match &key {
                    PropKey::Named(n) => Some(n.clone()),
                    _ => None,
                };
                let f = self.function_rest(start, if is_ctor { cname.clone() } else { fname }, is_async, generator, true, fk, false)?;
                if is_ctor {
                    ctor = Some(f);
                } else {
                    members.push(ClassMember { key, is_static, kind: ClassMemberKind::Method(f, pkind) });
                }
            } else {
                // a field
                let saved2 = self.ctx;
                self.ctx.in_function = true;
                let init = if self.eat("=")? { Some(self.assign(false)?) } else { None };
                self.ctx = saved2;
                self.semicolon()?;
                members.push(ClassMember { key, is_static, kind: ClassMemberKind::Field(init) });
            }
        }
        self.ctx = saved;
        let src = (cstart, self.prev_end);
        let mk = |body: Vec<Stmt>| Rc::new(FuncNode { name: None, params: vec![], body, arrow: false, expr_body: false, strict: true, is_async: false, generator: false, method: true, kind: FuncKind::Normal, src: (0, 0), decl: false });
        let mut inst = Vec::new();
        let mut stat = Vec::new();
        let mut n = 0u32;
        for m in &members {
            match &m.kind {
                ClassMemberKind::Field(init) => {
                    let key = match &m.key {
                        PropKey::Named(s) if s.starts_with('#') => ClassFieldKey::Private(s.clone()),
                        PropKey::Named(s) => ClassFieldKey::Named(s.clone()),
                        PropKey::Computed(_) => {
                            n += 1;
                            ClassFieldKey::Computed(n - 1)
                        },
                    };
                    let st = Stmt::Expr(Expr::ClassField(key, init.clone().map(Box::new)));
                    if m.is_static {
                        stat.push(st);
                    } else {
                        inst.push(st);
                    }
                },
                ClassMemberKind::StaticBlock(f) => stat.push(Stmt::Block(f.body.clone())),
                _ => {},
            }
        }
        let field_init = if inst.is_empty() { None } else { Some(mk(inst)) };
        let static_init = if stat.is_empty() { None } else { Some(mk(stat)) };
        let derived = extends.is_some();
        let ctor = Some(match ctor {
            Some(c) => c,
            None => {
                let args = name("args");
                Rc::new(FuncNode {
                    name: cname.clone(),
                    params: if derived { vec![Param { pat: Pattern::Ident(args.clone()), default: None, rest: true }] } else { vec![] },
                    body: if derived { vec![Stmt::Expr(Expr::Call(Box::new(Expr::Super), vec![Expr::Spread(Box::new(Expr::Ident(args)))], false))] } else { vec![] },
                    arrow: false,
                    expr_body: false,
                    strict: true,
                    is_async: false,
                    generator: false,
                    method: true,
                    kind: if derived { FuncKind::DerivedConstructor } else { FuncKind::ClassConstructor },
                    src,
                    decl: false,
                })
            },
        });
        Ok(Rc::new(ClassNode { name: cname, extends, ctor, members, field_init, static_init, src }))
    }

    /// `async`, `*`, `get`, `set` before a method name
    fn method_prefix(&mut self) -> PResult<(bool, bool, PropKind)> {
        let mut is_async = false;
        let mut generator = false;
        let mut kind = PropKind::Init;
        let next_is_key = |t: &Token| !matches!(t.tok, Tok::Punct("(") | Tok::Punct("=") | Tok::Punct(";") | Tok::Punct("}") | Tok::Punct(",") | Tok::Punct(":")) ;
        if self.is_kw("async") {
            let t = self.peek()?;
            if next_is_key(&t) && !t.nl_before {
                self.advance()?;
                is_async = true;
            }
        }
        if self.eat("*")? {
            generator = true;
        }
        if !is_async && !generator && (self.is_kw("get") || self.is_kw("set")) {
            let t = self.peek()?;
            if next_is_key(&t) {
                kind = if self.is_kw("get") { PropKind::Get } else { PropKind::Set };
                self.advance()?;
            }
        }
        Ok((is_async, generator, kind))
    }

    /// a property name; the second value is the name when it could be a shorthand `{ a }`
    fn prop_key(&mut self) -> PResult<(PropKey, Option<Name>)> {
        match &self.cur.tok {
            Tok::Ident(s, esc) => {
                let n = name(s);
                let short = if !*esc && RESERVED.contains(&s.as_str()) { None } else { Some(n.clone()) };
                self.advance()?;
                Ok((PropKey::Named(n), short))
            },
            Tok::PrivateName(s) => {
                let n = name(&format!("#{}", s));
                self.advance()?;
                Ok((PropKey::Named(n), None))
            },
            Tok::Str(s) => {
                let n = name(&crate::strs::to_rust(s));
                self.advance()?;
                Ok((PropKey::Named(n), None))
            },
            Tok::Num(v) => {
                let n = name(&crate::num::to_string_rust(*v));
                self.advance()?;
                Ok((PropKey::Named(n), None))
            },
            Tok::Punct("[") => {
                self.advance()?;
                let e = self.assign(false)?;
                self.expect("]")?;
                Ok((PropKey::Computed(Box::new(e)), None))
            },
            _ => self.unexpected(),
        }
    }

    // ── expressions ──

    pub fn expression(&mut self, no_in: bool) -> PResult<Expr> {
        let e = self.assign(no_in)?;
        if self.is(",") {
            let mut v = vec![e];
            while self.eat(",")? {
                v.push(self.assign(no_in)?);
            }
            return Ok(Expr::Seq(v));
        }
        Ok(e)
    }

    fn arrow_ahead_ident(&self) -> PResult<bool> {
        let t = self.peek()?;
        Ok(matches!(t.tok, Tok::Punct("=>")) && !t.nl_before)
    }

    pub fn assign(&mut self, no_in: bool) -> PResult<Expr> {
        // arrows: `x =>`, `async x =>`, `async (...) =>`, `(...) =>`
        if let Tok::Ident(s, esc) = &self.cur.tok {
            let s = s.clone();
            let esc = *esc;
            if s == "yield" && self.ctx.generator {
                return self.yield_expr(no_in);
            }
            if (esc || !RESERVED.contains(&s.as_str())) && self.arrow_ahead_ident()? {
                let start = self.cur.start;
                let p = self.binding_ident()?;
                self.advance()?; // =>
                return self.arrow_body(start, vec![Param { pat: Pattern::Ident(p), default: None, rest: false }], false);
            }
            if s == "async" && !esc {
                let t = self.peek()?;
                if !t.nl_before {
                    if let Tok::Ident(ref p, _) = t.tok {
                        if p != "function" {
                            // async x => ...
                            let mut l = self.lx.clone();
                            l.next_token().ok();
                            if let Ok(t2) = l.next_token() {
                                if matches!(t2.tok, Tok::Punct("=>")) {
                                    let start = self.cur.start;
                                    self.advance()?;
                                    let pn = self.binding_ident()?;
                                    self.advance()?;
                                    return self.arrow_body(start, vec![Param { pat: Pattern::Ident(pn), default: None, rest: false }], true);
                                }
                            }
                        }
                    }
                }
            }
        }
        let start = self.cur.start;
        let left = self.conditional(no_in)?;
        // (a, b) => ... : the parenthesised list was read as an expression
        if self.is("=>") {
            if self.cur.nl_before {
                return self.unexpected();
            }
            let (params, is_async) = match left {
                Expr::Call(callee, args, false) if matches!(&*callee, Expr::Ident(n) if &**n == "async") => (self.exprs_to_params(args)?, true),
                other => (self.cover_to_params(other)?, false),
            };
            self.advance()?;
            return self.arrow_body(start, params, is_async);
        }
        if let Tok::Punct(op) = self.cur.tok {
            if matches!(op, "=" | "+=" | "-=" | "*=" | "/=" | "%=" | "**=" | "<<=" | ">>=" | ">>>=" | "&=" | "|=" | "^=" | "&&=" | "||=" | "??=") {
                let target = if op == "=" { self.to_pattern(left)? } else { self.simple_target(left)? };
                self.advance()?;
                let rhs = self.assign(no_in)?;
                return Ok(Expr::Assign(op, Box::new(target), Box::new(rhs)));
            }
        }
        Ok(left)
    }

    fn yield_expr(&mut self, no_in: bool) -> PResult<Expr> {
        self.advance()?;
        let delegate = !self.cur.nl_before && self.eat("*")?;
        let ends = self.cur.nl_before || matches!(self.cur.tok, Tok::Punct(")") | Tok::Punct("]") | Tok::Punct("}") | Tok::Punct(",") | Tok::Punct(";") | Tok::Punct(":") | Tok::Eof);
        let arg = if ends && !delegate { None } else { Some(Box::new(self.assign(no_in)?)) };
        Ok(Expr::Yield(arg, delegate))
    }

    fn arrow_body(&mut self, start: usize, params: Vec<Param>, is_async: bool) -> PResult<Expr> {
        let saved = self.ctx;
        self.ctx.in_function = true;
        self.ctx.is_async = is_async;
        self.ctx.generator = false;
        let (body, expr_body, strict) = if self.is("{") {
            self.advance()?;
            let strict = self.directives() || saved.strict;
            self.ctx.strict = strict;
            (self.block_body()?, false, strict)
        } else {
            let e = self.assign(false)?;
            (vec![Stmt::Return(Some(e))], true, saved.strict)
        };
        self.ctx = saved;
        Ok(Expr::Function(Rc::new(FuncNode {
            name: None,
            params,
            body,
            arrow: true,
            expr_body,
            strict,
            is_async,
            generator: false,
            method: false,
            kind: FuncKind::Normal,
            src: (start, self.prev_end),
            decl: false,
        })))
    }

    /// the expression in `( ... ) =>`, back into parameters
    fn cover_to_params(&mut self, e: Expr) -> PResult<Vec<Param>> {
        match e {
            Expr::Seq(items) => self.exprs_to_params(items),
            Expr::Undefined => Ok(vec![]), // `()` (see primary)
            other => self.exprs_to_params(vec![other]),
        }
    }

    fn exprs_to_params(&mut self, items: Vec<Expr>) -> PResult<Vec<Param>> {
        let mut out = Vec::new();
        let n = items.len();
        for (i, it) in items.into_iter().enumerate() {
            match it {
                Expr::Spread(inner) if i == n - 1 => out.push(Param { pat: self.to_pattern(*inner)?, default: None, rest: true }),
                Expr::Assign("=", target, d) => out.push(Param { pat: *target, default: Some(*d), rest: false }),
                other => out.push(Param { pat: self.to_pattern(other)?, default: None, rest: false }),
            }
        }
        Ok(out)
    }

    fn simple_target(&self, e: Expr) -> PResult<Pattern> {
        match e {
            Expr::Ident(n) => Ok(Pattern::Ident(n)),
            Expr::Member(_, _, false) | Expr::Index(_, _, false) => Ok(Pattern::Expr(Box::new(e))),
            _ => self.err("invalid assignment target"),
        }
    }

    /// an expression on the left of `=` (or in `for (x of`), as a pattern
    pub fn to_pattern(&self, e: Expr) -> PResult<Pattern> {
        match e {
            Expr::Array(items) => {
                let mut out = Vec::new();
                let mut rest = None;
                let n = items.len();
                for (i, it) in items.into_iter().enumerate() {
                    match it {
                        None => out.push(None),
                        Some(Expr::Spread(inner)) if i == n - 1 => rest = Some(Box::new(self.to_pattern(*inner)?)),
                        Some(Expr::Assign("=", t, d)) => out.push(Some(PatElem { target: *t, default: Some(*d) })),
                        Some(other) => out.push(Some(PatElem { target: self.to_pattern(other)?, default: None })),
                    }
                }
                Ok(Pattern::Array(out, rest))
            },
            Expr::Object(props) => {
                let mut out = Vec::new();
                let mut rest = None;
                for p in props {
                    match p {
                        ObjProp::Spread(e) => rest = Some(Box::new(self.to_pattern(e)?)),
                        ObjProp::Prop(key, PropKind::Init, value) => {
                            let elem = match value {
                                Expr::Assign("=", t, d) => PatElem { target: *t, default: Some(*d) },
                                other => PatElem { target: self.to_pattern(other)?, default: None },
                            };
                            out.push((key, elem));
                        },
                        _ => return self.err("invalid destructuring target"),
                    }
                }
                Ok(Pattern::Object(out, rest))
            },
            Expr::Assign("=", t, d) => {
                // only inside patterns (handled above); a bare one is invalid
                let _ = (t, d);
                self.err("invalid assignment target")
            },
            other => self.simple_target(other),
        }
    }

    fn conditional(&mut self, no_in: bool) -> PResult<Expr> {
        let c = self.binary(0, no_in)?;
        if self.eat("?")? {
            let a = self.assign(false)?;
            self.expect(":")?;
            let b = self.assign(no_in)?;
            return Ok(Expr::Cond(Box::new(c), Box::new(a), Box::new(b)));
        }
        Ok(c)
    }

    fn binop(&self, no_in: bool) -> Option<(&'static str, u8)> {
        let op: &'static str = match &self.cur.tok {
            Tok::Punct(p) => p,
            Tok::Ident(s, false) if s == "instanceof" => "instanceof",
            Tok::Ident(s, false) if s == "in" && !no_in => "in",
            _ => return None,
        };
        let prec = match op {
            "??" => 1,
            "||" => 2,
            "&&" => 3,
            "|" => 4,
            "^" => 5,
            "&" => 6,
            "==" | "!=" | "===" | "!==" => 7,
            "<" | ">" | "<=" | ">=" | "instanceof" | "in" => 8,
            "<<" | ">>" | ">>>" => 9,
            "+" | "-" => 10,
            "*" | "/" | "%" => 11,
            "**" => 12,
            _ => return None,
        };
        Some((op, prec))
    }

    fn binary(&mut self, min: u8, no_in: bool) -> PResult<Expr> {
        let mut left = self.unary()?;
        while let Some((op, prec)) = self.binop(no_in) {
            if prec <= min && !(op == "**" && prec == min && false) {
                if prec <= min {
                    break;
                }
            }
            self.advance()?;
            // ** is right-associative
            let right = if op == "**" { self.binary(prec - 1, no_in)? } else { self.binary(prec, no_in)? };
            left = match op {
                "&&" | "||" | "??" => Expr::Logical(op, Box::new(left), Box::new(right)),
                _ => Expr::Binary(op, Box::new(left), Box::new(right)),
            };
        }
        Ok(left)
    }

    fn unary(&mut self) -> PResult<Expr> {
        match &self.cur.tok {
            Tok::Punct(p) if matches!(*p, "!" | "~" | "+" | "-") => {
                let op = *p;
                self.advance()?;
                let e = self.unary()?;
                if self.is("**") {
                    return self.err("a unary expression cannot be the base of **; add parentheses");
                }
                Ok(Expr::Unary(op, Box::new(e)))
            },
            Tok::Punct(p) if matches!(*p, "++" | "--") => {
                let op = *p;
                self.advance()?;
                let e = self.unary()?;
                let t = self.simple_target(e)?;
                let e = match t {
                    Pattern::Ident(n) => Expr::Ident(n),
                    Pattern::Expr(e) => *e,
                    _ => unreachable!(),
                };
                Ok(Expr::Update(op, true, Box::new(e)))
            },
            Tok::Ident(s, false) if matches!(s.as_str(), "typeof" | "void" | "delete") => {
                let op: &'static str = match s.as_str() {
                    "typeof" => "typeof",
                    "void" => "void",
                    _ => "delete",
                };
                self.advance()?;
                let e = self.unary()?;
                if op == "delete" && self.ctx.strict && matches!(e, Expr::Ident(_)) {
                    return self.err("cannot delete a variable in strict mode");
                }
                Ok(Expr::Unary(op, Box::new(e)))
            },
            Tok::Ident(s, false) if s == "await" && self.ctx.is_async => {
                self.advance()?;
                let e = self.unary()?;
                Ok(Expr::Await(Box::new(e)))
            },
            _ => self.postfix(),
        }
    }

    fn postfix(&mut self) -> PResult<Expr> {
        let e = self.lhs()?;
        if let Tok::Punct(p) = self.cur.tok {
            if (p == "++" || p == "--") && !self.cur.nl_before {
                self.advance()?;
                let t = self.simple_target(e)?;
                let e = match t {
                    Pattern::Ident(n) => Expr::Ident(n),
                    Pattern::Expr(e) => *e,
                    _ => unreachable!(),
                };
                return Ok(Expr::Update(p, false, Box::new(e)));
            }
        }
        Ok(e)
    }

    fn arguments(&mut self) -> PResult<Vec<Expr>> {
        // after "("
        let mut args = Vec::new();
        loop {
            if self.eat(")")? {
                break;
            }
            if self.eat("...")? {
                args.push(Expr::Spread(Box::new(self.assign(false)?)));
            } else {
                args.push(self.assign(false)?);
            }
            if !self.is(")") {
                self.expect(",")?;
            }
        }
        Ok(args)
    }

    /// member access, calls, `new`, optional chains
    fn lhs(&mut self) -> PResult<Expr> {
        let mut e = if self.is_kw("new") {
            self.new_expr()?
        } else {
            self.primary()?
        };
        let mut in_chain = false;
        loop {
            match &self.cur.tok {
                Tok::Punct(".") => {
                    self.advance()?;
                    if let Tok::PrivateName(n) = &self.cur.tok {
                        let n = name(&format!("#{}", n));
                        self.advance()?;
                        e = Expr::Member(Box::new(e), n, false);
                    } else {
                        let n = self.ident_name()?;
                        e = Expr::Member(Box::new(e), n, false);
                    }
                },
                Tok::Punct("?.") => {
                    self.advance()?;
                    in_chain = true;
                    if self.eat("(")? {
                        let args = self.arguments()?;
                        e = Expr::Call(Box::new(e), args, true);
                    } else if self.eat("[")? {
                        let k = self.expression(false)?;
                        self.expect("]")?;
                        e = Expr::Index(Box::new(e), Box::new(k), true);
                    } else {
                        let n = self.ident_name()?;
                        e = Expr::Member(Box::new(e), n, true);
                    }
                },
                Tok::Punct("[") => {
                    self.advance()?;
                    let k = self.expression(false)?;
                    self.expect("]")?;
                    e = Expr::Index(Box::new(e), Box::new(k), false);
                },
                Tok::Punct("(") => {
                    self.advance()?;
                    let args = self.arguments()?;
                    e = Expr::Call(Box::new(e), args, false);
                },
                Tok::Template(..) => {
                    if in_chain {
                        return self.err("a tagged template cannot be in an optional chain");
                    }
                    let (parts, exprs) = self.template_parts()?;
                    e = Expr::Tagged(Box::new(e), Rc::new(parts), exprs);
                },
                _ => break,
            }
        }
        if in_chain {
            e = Expr::OptChain(Box::new(e));
        }
        Ok(e)
    }

    fn new_expr(&mut self) -> PResult<Expr> {
        self.advance()?; // new
        if self.eat(".")? {
            let n = self.ident_name()?;
            if &*n != "target" {
                return self.err("expected new.target");
            }
            return Ok(Expr::NewTarget);
        }
        let mut callee = if self.is_kw("new") { self.new_expr()? } else { self.primary()? };
        loop {
            match &self.cur.tok {
                Tok::Punct(".") => {
                    self.advance()?;
                    let n = self.ident_name()?;
                    callee = Expr::Member(Box::new(callee), n, false);
                },
                Tok::Punct("[") => {
                    self.advance()?;
                    let k = self.expression(false)?;
                    self.expect("]")?;
                    callee = Expr::Index(Box::new(callee), Box::new(k), false);
                },
                _ => break,
            }
        }
        let args = if self.eat("(")? { self.arguments()? } else { Vec::new() };
        Ok(Expr::New(Box::new(callee), args))
    }

    fn template_parts(&mut self) -> PResult<(TemplateParts, Vec<Expr>)> {
        let mut cooked = Vec::new();
        let mut raw = Vec::new();
        let mut exprs = Vec::new();
        loop {
            let t = self.cur.clone();
            match t.tok {
                Tok::Template(c, r, tail) => {
                    cooked.push(c);
                    raw.push(r);
                    if tail {
                        self.advance()?;
                        break;
                    }
                    self.advance()?;
                    exprs.push(self.expression(false)?);
                    if !self.is("}") {
                        return self.err("expected '}' in a template");
                    }
                    // continue the template from this `}`
                    let rt = self.lx.rescan_template(&self.cur).map_err(|m| SyntaxError { msg: m, line: self.cur.line })?;
                    self.cur = rt;
                },
                _ => return self.unexpected(),
            }
        }
        Ok((TemplateParts { cooked, raw }, exprs))
    }

    fn primary(&mut self) -> PResult<Expr> {
        let t = self.cur.clone();
        match t.tok {
            Tok::PrivateName(n) => {
                self.advance()?;
                if !self.is_kw("in") {
                    return self.err("a private name here must be followed by 'in'");
                }
                Ok(Expr::PrivateName(name(&format!("#{}", n))))
            },
            Tok::BigInt(s) => {
                self.advance()?;
                Ok(Expr::BigInt(Rc::from(s.as_str())))
            },
            Tok::Num(v) => {
                self.advance()?;
                Ok(Expr::Num(v))
            },
            Tok::Str(s) => {
                self.advance()?;
                Ok(Expr::Str(s))
            },
            Tok::Template(..) => {
                let (parts, exprs) = self.template_parts()?;
                let mut cooked: Vec<JsStr> = Vec::new();
                for c in parts.cooked {
                    match c {
                        Some(c) => cooked.push(c),
                        None => return self.err("bad escape in a template"),
                    }
                }
                Ok(Expr::Template(cooked, exprs))
            },
            Tok::Punct("/") | Tok::Punct("/=") => {
                let rt = self.lx.rescan_regex(&t).map_err(|m| SyntaxError { msg: m, line: t.line })?;
                self.cur = rt;
                if let Tok::Regex(b, f) = self.cur.tok.clone() {
                    self.advance()?;
                    return Ok(Expr::Regex(b, f));
                }
                self.unexpected()
            },
            Tok::Punct("(") => {
                self.advance()?;
                // () => ...
                if self.is(")") {
                    self.advance()?;
                    if !self.is("=>") {
                        return self.unexpected();
                    }
                    return Ok(Expr::Undefined);
                }
                // (a, ...rest) => ...
                let mut items = Vec::new();
                loop {
                    if self.eat("...")? {
                        let p = self.binding_target()?;
                        let pe = pattern_to_expr(p);
                        items.push(Expr::Spread(Box::new(pe)));
                        self.expect(")")?;
                        if !self.is("=>") {
                            return self.unexpected();
                        }
                        return Ok(Expr::Seq(items));
                    }
                    items.push(self.assign(false)?);
                    if self.eat(",")? {
                        if self.is(")") {
                            // trailing comma: only in arrow parameters
                            self.advance()?;
                            if !self.is("=>") {
                                return self.unexpected();
                            }
                            return Ok(Expr::Seq(items));
                        }
                        continue;
                    }
                    self.expect(")")?;
                    break;
                }
                if items.len() == 1 {
                    let e = items.pop().unwrap();
                    // keep parenthesised arrow params in a Seq so `(a) => ` still converts
                    if self.is("=>") {
                        return Ok(Expr::Seq(vec![e]));
                    }
                    return Ok(e);
                }
                Ok(Expr::Seq(items))
            },
            Tok::Punct("[") => {
                self.advance()?;
                let mut items = Vec::new();
                loop {
                    if self.eat("]")? {
                        break;
                    }
                    if self.is(",") {
                        self.advance()?;
                        items.push(None);
                        continue;
                    }
                    let e = if self.eat("...")? { Expr::Spread(Box::new(self.assign(false)?)) } else { self.assign(false)? };
                    items.push(Some(e));
                    if !self.is("]") {
                        self.expect(",")?;
                    }
                }
                Ok(Expr::Array(items))
            },
            Tok::Punct("{") => self.object_literal(),
            Tok::Ident(ref s, esc) => {
                let s = s.clone();
                if !esc {
                    match s.as_str() {
                        "function" => return Ok(Expr::Function(self.function(false, false, true)?)),
                        "async" if self.async_function_ahead()? => {
                            self.advance()?;
                            return Ok(Expr::Function(self.function(true, false, true)?));
                        },
                        "class" => return Ok(Expr::Class(self.class(false)?)),
                        "this" => {
                            self.advance()?;
                            return Ok(Expr::This);
                        },
                        "null" => {
                            self.advance()?;
                            return Ok(Expr::Null);
                        },
                        "true" => {
                            self.advance()?;
                            return Ok(Expr::Bool(true));
                        },
                        "false" => {
                            self.advance()?;
                            return Ok(Expr::Bool(false));
                        },
                        "super" => {
                            self.advance()?;
                            return Ok(Expr::Super);
                        },
                        "new" => return self.new_expr(),
                        "import" => {
                            self.advance()?;
                            if self.eat(".")? {
                                let m = self.ident_name()?;
                                if &*m != "meta" {
                                    return self.err("expected import.meta");
                                }
                                return Ok(Expr::Ident(name("%meta")));
                            }
                            if !self.eat("(")? {
                                return self.err("expected '(' after import");
                            }
                            let a = self.assign(false)?;
                            if self.eat(",")? && !self.is(")") {
                                self.assign(false)?;
                                self.eat(",")?;
                            }
                            self.expect(")")?;
                            return Ok(Expr::Import(Box::new(a)));
                        },
                        _ => {},
                    }
                    if RESERVED.contains(&s.as_str()) {
                        return self.unexpected();
                    }
                }
                self.advance()?;
                Ok(Expr::Ident(name(&s)))
            },
            _ => self.unexpected(),
        }
    }

    fn object_literal(&mut self) -> PResult<Expr> {
        self.advance()?; // {
        let mut props = Vec::new();
        loop {
            if self.eat("}")? {
                break;
            }
            if self.eat("...")? {
                props.push(ObjProp::Spread(self.assign(false)?));
            } else {
                let start = self.cur.start;
                let (is_async, generator, kind) = self.method_prefix()?;
                let (key, short) = self.prop_key()?;
                if self.is("(") {
                    let fk = match kind {
                        PropKind::Get => FuncKind::Getter,
                        PropKind::Set => FuncKind::Setter,
                        PropKind::Init => FuncKind::Normal,
                    };
                    let fname = match &key {
                        PropKey::Named(n) => Some(n.clone()),
                        _ => None,
                    };
                    let f = self.function_rest(start, fname, is_async, generator, true, fk, false)?;
                    props.push(ObjProp::Prop(key, kind, Expr::Function(f)));
                } else if kind != PropKind::Init || is_async || generator {
                    return self.err("expected '(' after a method name");
                } else if self.eat(":")? {
                    let v = self.assign(false)?;
                    props.push(ObjProp::Prop(key, PropKind::Init, v));
                } else {
                    let n = match short {
                        Some(n) => n,
                        None => return self.err("expected ':'"),
                    };
                    if self.eat("=")? {
                        // { a = 1 }: only valid as a pattern
                        let d = self.assign(false)?;
                        props.push(ObjProp::Prop(key, PropKind::Init, Expr::Assign("=", Box::new(Pattern::Ident(n)), Box::new(d))));
                    } else {
                        props.push(ObjProp::Prop(key, PropKind::Init, Expr::Ident(n)));
                    }
                }
            }
            if !self.is("}") {
                self.expect(",")?;
            }
        }
        Ok(Expr::Object(props))
    }
}

/// a binding pattern back to an expression (for `(...rest) =>` covers)
fn pattern_to_expr(p: Pattern) -> Expr {
    match p {
        Pattern::Ident(n) => Expr::Ident(n),
        Pattern::Expr(e) => *e,
        Pattern::Array(items, rest) => {
            let mut v: Vec<Option<Expr>> = items
                .into_iter()
                .map(|e| {
                    e.map(|pe| match pe.default {
                        Some(d) => Expr::Assign("=", Box::new(pe.target), Box::new(d)),
                        None => pattern_to_expr(pe.target),
                    })
                })
                .collect();
            if let Some(r) = rest {
                v.push(Some(Expr::Spread(Box::new(pattern_to_expr(*r)))));
            }
            Expr::Array(v)
        },
        Pattern::Object(props, rest) => {
            let mut v: Vec<ObjProp> = props
                .into_iter()
                .map(|(k, pe)| {
                    let val = match pe.default {
                        Some(d) => Expr::Assign("=", Box::new(pe.target), Box::new(d)),
                        None => pattern_to_expr(pe.target),
                    };
                    ObjProp::Prop(k, PropKind::Init, val)
                })
                .collect();
            if let Some(r) = rest {
                v.push(ObjProp::Spread(pattern_to_expr(*r)));
            }
            Expr::Object(v)
        },
    }
}

// ── modules ──

/// one top-level item of a module
#[derive(Debug)]
pub enum ModuleItem {
    Stmt(Stmt),
    /// import ... from "spec": default name, namespace name, (imported, local) pairs
    Import { spec: JsStr, default: Option<Name>, ns: Option<Name>, named: Vec<(Name, Name)> },
    /// export var / let / const / function / class
    ExportDecl(Stmt),
    /// export default function / class (always named; "default" when anonymous)
    ExportDefaultDecl(Stmt),
    ExportDefaultExpr(Expr),
    /// export { local as exported } [from "spec"]
    ExportNamed { specs: Vec<(Name, Name)>, from: Option<JsStr> },
    /// export * [as name] from "spec"
    ExportAll { from: JsStr, as_name: Option<Name> },
}

pub fn parse_module(src: &str) -> PResult<Vec<ModuleItem>> {
    let mut p = Parser::new(src)?;
    p.ctx.strict = true;
    p.ctx.is_async = true; // top-level await
    let mut out = Vec::new();
    while p.cur.tok != Tok::Eof {
        if p.is_kw("import") && !matches!(p.peek()?.tok, Tok::Punct("(") | Tok::Punct(".")) {
            out.push(p.import_decl()?);
        } else if p.is_kw("export") {
            out.push(p.export_decl()?);
        } else {
            out.push(ModuleItem::Stmt(p.statement()?));
        }
    }
    Ok(out)
}

fn renamed_fn(f: &FuncNode, n: &str) -> Rc<FuncNode> {
    Rc::new(FuncNode {
        name: Some(name(n)),
        params: f.params.clone(),
        body: f.body.clone(),
        arrow: f.arrow,
        expr_body: f.expr_body,
        strict: f.strict,
        is_async: f.is_async,
        generator: f.generator,
        method: f.method,
        kind: f.kind,
        src: f.src,
        decl: true,
    })
}

impl Parser {
    fn module_spec(&mut self) -> PResult<JsStr> {
        match &self.cur.tok {
            Tok::Str(s) => {
                let s = s.clone();
                self.advance()?;
                // import attributes: with { type: "json" } / assert { ... }
                if (self.is_kw("with") || self.is_kw("assert")) && !self.cur.nl_before {
                    self.advance()?;
                    self.expect("{")?;
                    let mut depth = 1;
                    while depth > 0 {
                        if self.cur.tok == Tok::Eof {
                            return self.err("expected '}'");
                        }
                        if self.is("{") {
                            depth += 1;
                        } else if self.is("}") {
                            depth -= 1;
                        }
                        self.advance()?;
                    }
                }
                Ok(s)
            },
            _ => self.err("expected a module specifier string"),
        }
    }

    /// an import / export name: an identifier (keywords too) or a string
    fn module_export_name(&mut self) -> PResult<Name> {
        match &self.cur.tok {
            Tok::Str(s) => {
                let n = name(&crate::strs::to_rust(s));
                self.advance()?;
                Ok(n)
            },
            _ => self.ident_name(),
        }
    }

    fn expect_kw(&mut self, k: &str) -> PResult<()> {
        if self.eat_kw(k)? {
            Ok(())
        } else {
            self.err(&format!("expected '{}'", k))
        }
    }

    fn import_decl(&mut self) -> PResult<ModuleItem> {
        self.advance()?; // import
        let mut default = None;
        let mut ns = None;
        let mut named = Vec::new();
        if let Tok::Str(_) = &self.cur.tok {
            let spec = self.module_spec()?;
            self.semicolon()?;
            return Ok(ModuleItem::Import { spec, default, ns, named });
        }
        if let Tok::Ident(..) = &self.cur.tok {
            default = Some(self.binding_ident()?);
            if !self.eat(",")? {
                self.expect_kw("from")?;
                let spec = self.module_spec()?;
                self.semicolon()?;
                return Ok(ModuleItem::Import { spec, default, ns, named });
            }
        }
        if self.eat("*")? {
            self.expect_kw("as")?;
            ns = Some(self.binding_ident()?);
        } else {
            self.expect("{")?;
            loop {
                if self.eat("}")? {
                    break;
                }
                let imported = self.module_export_name()?;
                let local = if self.eat_kw("as")? { self.binding_ident()? } else { imported.clone() };
                named.push((imported, local));
                if !self.is("}") {
                    self.expect(",")?;
                }
            }
        }
        self.expect_kw("from")?;
        let spec = self.module_spec()?;
        self.semicolon()?;
        Ok(ModuleItem::Import { spec, default, ns, named })
    }

    fn export_decl(&mut self) -> PResult<ModuleItem> {
        self.advance()?; // export
        if self.eat("*")? {
            let as_name = if self.eat_kw("as")? { Some(self.module_export_name()?) } else { None };
            self.expect_kw("from")?;
            let from = self.module_spec()?;
            self.semicolon()?;
            return Ok(ModuleItem::ExportAll { from, as_name });
        }
        if self.eat_kw("default")? {
            if self.is_kw("function") || (self.is_kw("async") && self.async_function_ahead()?) {
                let is_async = self.eat_kw("async")?;
                let f = self.function(is_async, true, true)?;
                let f = if f.name.is_none() { renamed_fn(&f, "default") } else { f };
                return Ok(ModuleItem::ExportDefaultDecl(Stmt::Function(f)));
            }
            if self.is_kw("class") {
                let c = self.class(false)?;
                let c = if c.name.is_none() {
                    Rc::new(ClassNode {
                        name: Some(name("default")),
                        extends: c.extends.clone(),
                        ctor: c.ctor.clone(),
                        members: c.members.clone(),
                        field_init: c.field_init.clone(),
                        static_init: c.static_init.clone(),
                        src: c.src,
                    })
                } else {
                    c
                };
                return Ok(ModuleItem::ExportDefaultDecl(Stmt::Class(c)));
            }
            let e = self.assign(false)?;
            self.semicolon()?;
            return Ok(ModuleItem::ExportDefaultExpr(e));
        }
        if self.eat("{")? {
            let mut specs = Vec::new();
            loop {
                if self.eat("}")? {
                    break;
                }
                let local = self.module_export_name()?;
                let exported = if self.eat_kw("as")? { self.module_export_name()? } else { local.clone() };
                specs.push((local, exported));
                if !self.is("}") {
                    self.expect(",")?;
                }
            }
            let from = if self.eat_kw("from")? { Some(self.module_spec()?) } else { None };
            self.semicolon()?;
            return Ok(ModuleItem::ExportNamed { specs, from });
        }
        let s = self.statement()?;
        match &s {
            Stmt::Var(..) | Stmt::Function(_) | Stmt::Class(_) => Ok(ModuleItem::ExportDecl(s)),
            _ => self.err("unexpected token after 'export'"),
        }
    }
}
