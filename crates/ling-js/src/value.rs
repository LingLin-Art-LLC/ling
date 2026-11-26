//! Values.

use crate::object::ObjRef;
use crate::strs::JsStr;

#[derive(Clone, Debug)]
pub enum Value {
    Undefined,
    Null,
    Bool(bool),
    Num(f64),
    Str(JsStr),
    Obj(ObjRef),
    /// internal only: an uninitialised `let` (temporal dead zone), an array hole
    Empty,
}

impl Value {
    pub fn is_nullish(&self) -> bool {
        matches!(self, Value::Undefined | Value::Null)
    }
    pub fn is_undefined(&self) -> bool {
        matches!(self, Value::Undefined)
    }
    pub fn as_obj(&self) -> Option<ObjRef> {
        match self {
            Value::Obj(o) => Some(*o),
            _ => None,
        }
    }
    pub fn num(&self) -> Option<f64> {
        match self {
            Value::Num(n) => Some(*n),
            _ => None,
        }
    }
    pub fn str(s: &str) -> Value {
        Value::Str(crate::strs::js(s))
    }

    /// SameValueZero / === on primitives and identity on objects (`strict`: NaN != NaN)
    pub fn strict_eq(&self, o: &Value) -> bool {
        match (self, o) {
            (Value::Undefined, Value::Undefined) | (Value::Null, Value::Null) => true,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Num(a), Value::Num(b)) => a == b,
            (Value::Str(a), Value::Str(b)) => a == b,
            (Value::Obj(a), Value::Obj(b)) => a == b,
            (Value::Empty, Value::Empty) => true,
            _ => false,
        }
    }

    pub fn same_value_zero(&self, o: &Value) -> bool {
        match (self, o) {
            (Value::Num(a), Value::Num(b)) => a == b || (a.is_nan() && b.is_nan()),
            _ => self.strict_eq(o),
        }
    }

    pub fn same_value(&self, o: &Value) -> bool {
        match (self, o) {
            (Value::Num(a), Value::Num(b)) => {
                if a.is_nan() && b.is_nan() {
                    return true;
                }
                a == b && a.is_sign_negative() == b.is_sign_negative()
            },
            _ => self.strict_eq(o),
        }
    }
}
