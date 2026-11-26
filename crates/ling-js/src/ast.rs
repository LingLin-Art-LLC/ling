//! The syntax tree.

use crate::strs::JsStr;
use alloc::boxed::Box;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;

pub type Name = Rc<str>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VarKind {
    Var,
    Let,
    Const,
}

#[derive(Debug, Clone)]
pub enum PropKey {
    Named(Name),
    Computed(Box<Expr>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PropKind {
    Init,
    Get,
    Set,
}

#[derive(Debug, Clone)]
pub enum ObjProp {
    Prop(PropKey, PropKind, Expr),
    Spread(Expr),
}

#[derive(Debug, Clone)]
pub enum Pattern {
    Ident(Name),
    /// a member expression as an assignment target (`a.b = `, `[x.y] = arr`)
    Expr(Box<Expr>),
    Array(Vec<Option<PatElem>>, Option<Box<Pattern>>),
    Object(Vec<(PropKey, PatElem)>, Option<Box<Pattern>>),
}

#[derive(Debug, Clone)]
pub struct PatElem {
    pub target: Pattern,
    pub default: Option<Expr>,
}

#[derive(Debug, Clone)]
pub enum Expr {
    Num(f64),
    Str(JsStr),
    Bool(bool),
    Null,
    Undefined,
    Regex(String, String),
    Template(Vec<JsStr>, Vec<Expr>),
    Tagged(Box<Expr>, Rc<TemplateParts>, Vec<Expr>),
    Ident(Name),
    This,
    Array(Vec<Option<Expr>>),
    Object(Vec<ObjProp>),
    Function(Rc<FuncNode>),
    Class(Rc<ClassNode>),
    Unary(&'static str, Box<Expr>),
    Update(&'static str, bool, Box<Expr>),
    Binary(&'static str, Box<Expr>, Box<Expr>),
    Logical(&'static str, Box<Expr>, Box<Expr>),
    Assign(&'static str, Box<Pattern>, Box<Expr>),
    Cond(Box<Expr>, Box<Expr>, Box<Expr>),
    /// callee, arguments, optional call (`f?.()`)
    Call(Box<Expr>, Vec<Expr>, bool),
    New(Box<Expr>, Vec<Expr>),
    /// object, name, optional (`a?.b`)
    Member(Box<Expr>, Name, bool),
    Index(Box<Expr>, Box<Expr>, bool),
    /// the end of an optional chain: a short-circuit lands here with `undefined`
    OptChain(Box<Expr>),
    Seq(Vec<Expr>),
    Spread(Box<Expr>),
    Yield(Option<Box<Expr>>, bool),
    Await(Box<Expr>),
    NewTarget,
    Super,
    /// a BigInt literal (decimal digits)
    BigInt(Rc<str>),
    /// `#x` (only before `in`)
    PrivateName(Name),
    /// `import(x)`
    Import(Box<Expr>),
    /// a class field definition (only in a class's synthetic field initialiser)
    ClassField(ClassFieldKey, Option<Box<Expr>>),
}

#[derive(Debug, Clone)]
pub enum ClassFieldKey {
    Named(Name),
    Private(Name),
    /// the n-th computed field key (evaluated when the class was made)
    Computed(u32),
}

#[derive(Debug, Clone)]
pub struct TemplateParts {
    pub cooked: Vec<Option<JsStr>>,
    pub raw: Vec<JsStr>,
}

#[derive(Debug, Clone)]
pub struct Param {
    pub pat: Pattern,
    pub default: Option<Expr>,
    pub rest: bool,
}

#[derive(Debug)]
pub struct FuncNode {
    pub name: Option<Name>,
    pub params: Vec<Param>,
    pub body: Vec<Stmt>,
    pub arrow: bool,
    /// an arrow function whose body is one expression (body holds one Return)
    pub expr_body: bool,
    pub strict: bool,
    pub is_async: bool,
    pub generator: bool,
    /// a method (object literal / class): no prototype, not a constructor
    pub method: bool,
    pub kind: FuncKind,
    /// the source text, for Function.prototype.toString
    pub src: (usize, usize),
    /// a declaration (`function f(){}` as a statement)
    pub decl: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FuncKind {
    Normal,
    Getter,
    Setter,
    ClassConstructor,
    DerivedConstructor,
}

#[derive(Debug)]
pub struct ClassNode {
    pub name: Option<Name>,
    pub extends: Option<Expr>,
    pub ctor: Option<Rc<FuncNode>>,
    pub members: Vec<ClassMember>,
    /// instance fields, as a method run on each new instance
    pub field_init: Option<Rc<FuncNode>>,
    /// static fields and static blocks, as a method run on the class once
    pub static_init: Option<Rc<FuncNode>>,
    /// the source text
    pub src: (usize, usize),
}

#[derive(Debug, Clone)]
pub struct ClassMember {
    pub key: PropKey,
    pub is_static: bool,
    pub kind: ClassMemberKind,
}

#[derive(Debug, Clone)]
pub enum ClassMemberKind {
    Method(Rc<FuncNode>, PropKind),
    Field(Option<Expr>),
    StaticBlock(Rc<FuncNode>),
}

#[derive(Debug, Clone)]
pub enum ForInit {
    Var(VarKind, Vec<(Pattern, Option<Expr>)>),
    Expr(Expr),
}

#[derive(Debug, Clone)]
pub enum ForHead {
    Var(VarKind, Pattern),
    Target(Pattern),
}

#[derive(Debug, Clone)]
pub struct Case {
    pub test: Option<Expr>,
    pub body: Vec<Stmt>,
}

#[derive(Debug, Clone)]
pub enum Stmt {
    Expr(Expr),
    Var(VarKind, Vec<(Pattern, Option<Expr>)>),
    Function(Rc<FuncNode>),
    Class(Rc<ClassNode>),
    Return(Option<Expr>),
    If(Expr, Box<Stmt>, Option<Box<Stmt>>),
    Block(Vec<Stmt>),
    For(Option<ForInit>, Option<Expr>, Option<Expr>, Box<Stmt>),
    ForIn(ForHead, Expr, Box<Stmt>),
    ForOf(ForHead, Expr, Box<Stmt>),
    ForAwaitOf(ForHead, Expr, Box<Stmt>),
    While(Expr, Box<Stmt>),
    DoWhile(Box<Stmt>, Expr),
    Break(Option<Name>),
    Continue(Option<Name>),
    Throw(Expr),
    Try(Vec<Stmt>, Option<(Option<Pattern>, Vec<Stmt>)>, Option<Vec<Stmt>>),
    Switch(Expr, Vec<Case>),
    Labeled(Name, Box<Stmt>),
    Empty,
    Debugger,
}

impl Pattern {
    /// every name the pattern binds
    pub fn names(&self, out: &mut Vec<Name>) {
        match self {
            Pattern::Ident(n) => out.push(n.clone()),
            Pattern::Expr(_) => {},
            Pattern::Array(items, rest) => {
                for e in items.iter().flatten() {
                    e.target.names(out);
                }
                if let Some(r) = rest {
                    r.names(out);
                }
            },
            Pattern::Object(props, rest) => {
                for (_, e) in props {
                    e.target.names(out);
                }
                if let Some(r) = rest {
                    r.names(out);
                }
            },
        }
    }
}
