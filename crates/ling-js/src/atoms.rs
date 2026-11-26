//! Interned property names. Every property key that is not an array index is an [`Atom`].
//! Atoms are never freed (a page's keys are a bounded set in practice).
//!
//! A symbol is an atom too: one made by [`Atoms::new_symbol`], which never enters the string
//! table, so no string can ever name it. Private names (`#x`) are symbols nothing enumerates.

use crate::strs::{hash, js, JsStr};
use alloc::vec;
use alloc::vec::Vec;

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub struct Atom(pub u32);

pub struct Atoms {
    names: Vec<JsStr>,
    /// 0 a string key, 1 a symbol, 2 a private name (a symbol nothing can enumerate)
    kinds: Vec<u8>,
    table: Vec<u32>, // open addressing: atom + 1, 0 = empty
}

macro_rules! well_known {
    ($($id:ident = $s:expr),* $(,)?) => {
        #[allow(non_upper_case_globals, dead_code)]
        pub mod k {
            use super::Atom;
            well_known!(@consts 0u32, $($id),*);
        }
        const WELL_KNOWN: &[&str] = &[$($s),*];
    };
    (@consts $n:expr, $id:ident $(, $rest:ident)*) => {
        pub const $id: Atom = Atom($n);
        well_known!(@consts $n + 1u32 $(, $rest)*);
    };
    (@consts $n:expr) => {};
}

well_known! {
    EMPTY = "", length = "length", prototype = "prototype", constructor = "constructor",
    name = "name", message = "message", stack = "stack", toString = "toString", valueOf = "valueOf",
    value = "value", done = "done", next = "next", get = "get", set = "set",
    writable = "writable", enumerable = "enumerable", configurable = "configurable",
    callee = "callee", arguments = "arguments", caller = "caller", lastIndex = "lastIndex",
    index = "index", input = "input", groups = "groups", source = "source", flags = "flags",
    global = "global", ignoreCase = "ignoreCase", multiline = "multiline", sticky = "sticky",
    unicode = "unicode", dotAll = "dotAll", toJSON = "toJSON", undefined = "undefined",
    NaN = "NaN", Infinity = "Infinity", this_ = "this", cause = "cause", join = "join",
    then = "then", description = "description", errors = "errors",
}

/// the well-known symbols: atoms made right after the well-known strings, in this order
#[allow(non_upper_case_globals, dead_code)]
pub mod sym {
    use super::Atom;
    const B: u32 = super::WELL_KNOWN.len() as u32;
    pub const iterator: Atom = Atom(B);
    pub const asyncIterator: Atom = Atom(B + 1);
    pub const hasInstance: Atom = Atom(B + 2);
    pub const toPrimitive: Atom = Atom(B + 3);
    pub const toStringTag: Atom = Atom(B + 4);
    pub const species: Atom = Atom(B + 5);
    pub const isConcatSpreadable: Atom = Atom(B + 6);
    pub const unscopables: Atom = Atom(B + 7);
    pub const match_: Atom = Atom(B + 8);
    pub const matchAll: Atom = Atom(B + 9);
    pub const replace: Atom = Atom(B + 10);
    pub const search: Atom = Atom(B + 11);
    pub const split: Atom = Atom(B + 12);
    pub const NAMES: &[&str] = &[
        "iterator", "asyncIterator", "hasInstance", "toPrimitive", "toStringTag", "species", "isConcatSpreadable",
        "unscopables", "match", "matchAll", "replace", "search", "split",
    ];
}

impl Atoms {
    pub fn new() -> Self {
        let mut a = Atoms { names: Vec::new(), kinds: Vec::new(), table: vec![0; 1024] };
        for s in WELL_KNOWN {
            a.intern(&js(s));
        }
        for s in sym::NAMES {
            a.new_symbol(&js(&alloc::format!("Symbol.{}", s)), false);
        }
        a
    }

    pub fn intern(&mut self, s: &[u16]) -> Atom {
        let mask = self.table.len() - 1;
        let mut i = hash(s) as usize & mask;
        loop {
            let e = self.table[i];
            if e == 0 {
                break;
            }
            if &*self.names[(e - 1) as usize] == s {
                return Atom(e - 1);
            }
            i = (i + 1) & mask;
        }
        let id = self.names.len() as u32;
        self.names.push(JsStr::from(s));
        self.kinds.push(0);
        self.table[i] = id + 1;
        if self.names.len() * 2 > self.table.len() {
            self.grow();
        }
        Atom(id)
    }

    /// a new symbol (never equal to any string key); `desc` is what error messages show
    pub fn new_symbol(&mut self, desc: &[u16], private: bool) -> Atom {
        let id = self.names.len() as u32;
        self.names.push(JsStr::from(desc));
        self.kinds.push(if private { 2 } else { 1 });
        Atom(id)
    }

    #[inline]
    pub fn is_symbol(&self, a: Atom) -> bool {
        self.kinds[a.0 as usize] != 0
    }

    /// a symbol made without a description (`Symbol()`): its description is undefined
    pub fn new_symbol_undescribed(&mut self) -> Atom {
        let id = self.names.len() as u32;
        self.names.push(JsStr::from(&[][..]));
        self.kinds.push(3);
        Atom(id)
    }

    #[inline]
    pub fn is_undescribed(&self, a: Atom) -> bool {
        self.kinds[a.0 as usize] == 3
    }

    #[inline]
    pub fn is_private(&self, a: Atom) -> bool {
        self.kinds[a.0 as usize] == 2
    }

    pub fn intern_str(&mut self, s: &str) -> Atom {
        self.intern(&js(s))
    }

    fn grow(&mut self) {
        let n = self.table.len() * 2;
        self.table = vec![0; n];
        let mask = n - 1;
        for (id, s) in self.names.iter().enumerate() {
            if self.kinds[id] != 0 {
                continue;
            }
            let mut i = hash(s) as usize & mask;
            while self.table[i] != 0 {
                i = (i + 1) & mask;
            }
            self.table[i] = id as u32 + 1;
        }
    }

    pub fn name(&self, a: Atom) -> &JsStr {
        &self.names[a.0 as usize]
    }
}
