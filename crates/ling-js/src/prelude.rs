//! Loads the JavaScript half of the standard library into a new engine.

use crate::vm::{Engine, Status};

pub const SOURCE: &str = include_str!("prelude.js");

pub fn load(e: &mut Engine) {
    match e.eval(SOURCE) {
        Status::Done(_) => {},
        Status::Error(v) => {
            let m = e.display(&v);
            panic!("LingJS prelude failed: {}", m);
        },
        Status::Host(_) => panic!("LingJS prelude asked the host for something"),
    }
}
