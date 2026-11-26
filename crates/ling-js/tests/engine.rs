use ling_js::{Engine, HostReq, Status, Value};

fn show(e: &mut Engine, st: Status) -> String {
    match st {
        Status::Done(v) => match &v {
            Value::Str(s) => ling_js::strs::to_rust(s),
            _ => e.display(&v),
        },
        Status::Error(v) => format!("Error: {}", e.display(&v)),
        Status::Host(r) => format!("Host: {:?}", r),
    }
}

fn run(src: &str) -> String {
    let mut e = Engine::new();
    let st = e.eval(src);
    show(&mut e, st)
}

macro_rules! check {
    ($($src:expr => $want:expr),* $(,)?) => {{
        let mut bad = Vec::new();
        $(
            if std::env::var("JS_TRACE").is_ok() { eprintln!("RUN {}", $src); }
            let got = run($src);
            if got != $want {
                bad.push(format!("\n  {}\n    want: {}\n    got:  {}", $src, $want, got));
            }
        )*
        if !bad.is_empty() {
            panic!("{} failing:{}", bad.len(), bad.join(""));
        }
    }};
}

#[test]
fn expressions() {
    check! {
        "1 + 2 * 3" => "7",
        "(1 + 2) * 3" => "9",
        "2 ** 10" => "1024",
        "2 ** 3 ** 2" => "512",
        "7 % 3" => "1",
        "-7 % 3" => "-1",
        "1 / 0" => "Infinity",
        "0.1 + 0.2" => "0.30000000000000004",
        "1e21" => "1e+21",
        "123456789012" => "123456789012",
        "0.000001" => "0.000001",
        "1e-7" => "1e-7",
        "'a' + 1" => "a1",
        "1 + '2'" => "12",
        "'3' * '4'" => "12",
        "true + 1" => "2",
        "null + 1" => "1",
        "undefined + 1" => "NaN",
        "[] + []" => "",
        "[1,2] + ''" => "1,2",
        "({}) + ''" => "[object Object]",
        "1 < 2 && 'yes'" => "yes",
        "0 || 'b'" => "b",
        "null ?? 'd'" => "d",
        "0 ?? 'd'" => "0",
        "typeof 1" => "number",
        "typeof 'x'" => "string",
        "typeof undefined" => "undefined",
        "typeof nope" => "undefined",
        "typeof null" => "object",
        "typeof function(){}" => "function",
        "1 == '1'" => "true",
        "1 === '1'" => "false",
        "null == undefined" => "true",
        "null == 0" => "false",
        "NaN == NaN" => "false",
        "'b' > 'a'" => "true",
        "5 >>> 1" => "2",
        "-5 >> 1" => "-3",
        "-1 >>> 0" => "4294967295",
        "~5" => "-6",
        "1 << 31" => "-2147483648",
        "!!''" => "false",
        "void 0" => "undefined",
        "(1, 2, 3)" => "3",
        "true ? 'a' : 'b'" => "a",
        "`a${1+1}b${'c'}`" => "a2bc",
        "'x'.length" => "1",
        "'abc'[1]" => "b",
    }
}

#[test]
fn statements_and_scopes() {
    check! {
        "var s = 0; for (var i = 0; i < 10; i++) s += i; s" => "45",
        "var s = 0, i = 0; while (i < 5) { s += i; i++ } s" => "10",
        "var i = 0; do { i++ } while (i < 3); i" => "3",
        "var out = []; for (var k in {a:1, b:2, c:3}) out.push(k); out.join()" => "a,b,c",
        "var out = []; for (const x of [1,2,3]) out.push(x * 2); out.join()" => "2,4,6",
        "var out = []; for (const c of 'héllo') out.push(c); out.join('-')" => "h-é-l-l-o",
        "var s = ''; outer: for (var i = 0; i < 3; i++) { for (var j = 0; j < 3; j++) { if (j == 1) continue outer; if (i == 2) break outer; s += i + '' + j + ' '; } } s" => "00 10 ",
        "function f(x) { switch (x) { case 1: return 'one'; case 2: case 3: return 'few'; default: return 'many' } } [f(1), f(3), f(9)].join()" => "one,few,many",
        "var r = ''; switch (2) { case 1: r += 'a'; case 2: r += 'b'; case 3: r += 'c'; break; case 4: r += 'd' } r" => "bc",
        "let x = 1; { let x = 2; } x" => "1",
        "const fs = []; for (let i = 0; i < 3; i++) fs.push(() => i); fs.map(f => f()).join()" => "0,1,2",
        "var fs = []; for (var i = 0; i < 3; i++) fs.push(function () { return i }); fs.map(function (f) { return f() }).join()" => "3,3,3",
        "function mk() { let n = 0; return { inc: () => ++n, get: () => n } } var c = mk(); c.inc(); c.inc(); c.get()" => "2",
        "(function () { try { x; let x = 1 } catch (e) { return e.name } })()" => "ReferenceError",
        "const c = 1; try { c = 2 } catch (e) { e.name }" => "TypeError",
        "try { early; } catch (e) { e.name } let early = 1;" => "ReferenceError",
        "let q = 5; q = 6; q + typeof globalThis.q" => "6undefined",
        "function f() { return typeof g; function g() {} } f()" => "function",
        "hoisted(); function hoisted() { return 1 }" => "1",
        "var a = 1; (function () { var a = 2; })(); a" => "1",
        "(function () { if (true) { function inner() { return 5 } } return inner() })()" => "5",
        "var x = 10; function f() { return x } (function () { var x = 20; return f() })()" => "10",
        "(function f(n) { return n <= 1 ? 1 : n * f(n - 1) })(5)" => "120",
        "var fe = function self() { return typeof self }; fe()" => "function",
    }
}

#[test]
fn exceptions() {
    check! {
        "try { throw new Error('boom') } catch (e) { e.message }" => "boom",
        "try { null.x } catch (e) { e instanceof TypeError }" => "true",
        "var log = []; try { try { throw 1 } finally { log.push('f') } } catch (e) { log.push('c' + e) } log.join()" => "f,c1",
        "function f() { try { return 'try' } finally { log = 'fin' } } var log; [f(), log].join()" => "try,fin",
        "function f() { for (var i = 0; i < 3; i++) { try { if (i == 1) break } finally { n++ } } return n } var n = 0; f()" => "2",
        "function f() { try { throw 1 } catch { return 'no binding' } } f()" => "no binding",
        "try { undefinedFn() } catch (e) { e.message }" => "undefinedFn is not defined",
        "try { (void 0)() } catch (e) { e.constructor === TypeError }" => "true",
        "String(new RangeError('r'))" => "RangeError: r",
        "function A() { throw new Error('x') } try { A() } catch (e) { typeof e.stack }" => "string",
        "throw 'plain'" => "Error: plain",
    }
}

#[test]
fn objects_and_functions() {
    check! {
        "var o = { a: 1, b: { c: 2 } }; o.b.c" => "2",
        "var o = { ['x' + 1]: 5 }; o.x1" => "5",
        "var a = 1, o = { a, f() { return this.a } }; o.f()" => "1",
        "var o = { get v() { return 42 }, set v(x) { this._v = x } }; o.v = 7; [o.v, o._v].join()" => "42,7",
        "var o = {}; Object.defineProperty(o, 'x', { get: function () { return 9 } }); o.x" => "9",
        "Object.keys({ b: 1, a: 2, 1: 3, 0: 4 }).join()" => "0,1,b,a",
        "JSON.stringify(Object.entries({ a: 1 }))" => "[[\"a\",1]]",
        "function P(n) { this.n = n } P.prototype.get = function () { return this.n }; new P(3).get()" => "3",
        "function P() {} var p = new P(); [p instanceof P, p instanceof Object, P.prototype.constructor === P].join()" => "true,true,true",
        "function F() { return { x: 1 } } new F().x" => "1",
        "var o = Object.create({ inherited: 1 }); o.own = 2; [o.inherited, o.hasOwnProperty('inherited'), 'inherited' in o].join()" => "1,false,true",
        "function f(a, b) { return arguments.length + ':' + arguments[1] } f(1, 2, 3)" => "3:2",
        "function f(a, b = a + 1, ...rest) { return [a, b, rest.length].join() } f(1)" => "1,2,0",
        "function f(...r) { return r.join('-') } f(1, 2, 3)" => "1-2-3",
        "function f() { return this } f.call(5) == 5" => "true",
        "function f(a, b) { return a + b } f.apply(null, [2, 3])" => "5",
        "function f(a, b) { return this.x + a + b } var g = f.bind({ x: 1 }, 2); g(3)" => "6",
        "function f(a, b) {} [f.length, f.name].join()" => "2,f",
        "var arrow = () => 1; arrow.name" => "arrow",
        "var o = { m() { return [1, 2].map(x => this.k * x) }, k: 10 }; o.m().join()" => "10,20",
        "Math.max.apply(Math, [1, 5, 3])" => "5",
        "var o = {a: 1}; delete o.a; 'a' in o" => "false",
        "Object.freeze({ a: 1 }).a" => "1",
        "'use strict'; var o = Object.freeze({ a: 1 }); try { o.a = 2 } catch (e) { e.name }" => "TypeError",
        "var o = {valueOf() { return 41 }}; o + 1" => "42",
        "var o = {toString() { return 'S' }}; `${o}` + o" => "SS",
        "var o = {toString() { return 'S' }}; String(o)" => "S",
        "var a = {}, b = {}; a[b] = 1; Object.keys(a)[0]" => "[object Object]",
        "Object.prototype.toString.call([])" => "[object Array]",
        "(function () {}).constructor === Function" => "true",
        "var o = { __proto__: { p: 1 } }; o.p" => "1",
        "typeof Object.getPrototypeOf(Object.create(null))" => "object",
        "Object.getPrototypeOf(Object.create(null)) === null" => "true",
        "var { a, b: { c = 5 } = {}, ...rest } = { a: 1, x: 2, y: 3 }; [a, c, Object.keys(rest).join('')].join()" => "1,5,xy",
        "var [x, , y = 9, ...z] = [1, 2, undefined, 4, 5]; [x, y, z.join('')].join()" => "1,9,45",
        "var a = 1, b = 2; [a, b] = [b, a]; a + '' + b" => "21",
        "function f({ p, q = 2 }) { return p + q } f({ p: 1 })" => "3",
        "var o = { a: { b: null } }; [o?.a?.b?.c, o.x?.y, o.a?.['b']].join()" => ",,",
        "var o = { f() { return 1 } }; [o.f?.(), o.g?.()].join()" => "1,",
        "var x = null; x ??= 5; var y = 1; y ||= 9; var z = 1; z &&= 7; [x, y, z].join()" => "5,1,7",
        "var o = { ...{ a: 1 }, b: 2 }; JSON.stringify(o)" => "{\"a\":1,\"b\":2}",
        "Math.max(...[1, 9, 3])" => "9",
        "[0, ...'ab', ...[1, 2]].join()" => "0,a,b,1,2",
        "function T(s, ...v) { return s.raw.join('|') + v.join('+') } T`a${1}b${2}c`" => "a|b|c1+2",
    }
}

#[test]
fn builtins() {
    check! {
        "[3, 1, 2].sort().join()" => "1,2,3",
        "[10, 9, 1].sort().join()" => "1,10,9",
        "[10, 9, 1].sort((a, b) => a - b).join()" => "1,9,10",
        "[1, 2, 3].map(x => x * 2).filter(x => x > 2).reduce((a, b) => a + b, 0)" => "10",
        "[1, [2, [3, [4]]]].flat(2).join()" => "1,2,3,4",
        "[1, 2, 3].indexOf(2)" => "1",
        "[1, 2, 3].includes(NaN)" => "false",
        "[NaN].includes(NaN)" => "true",
        "var a = [1, 2, 3, 4]; a.splice(1, 2, 'x'); a.join()" => "1,x,4",
        "[1, 2, 3].slice(-2).join()" => "2,3",
        "Array.from('abc').join('-')" => "a-b-c",
        "Array.from({ length: 3 }, (_, i) => i * i).join()" => "0,1,4",
        "Array.isArray([])" => "true",
        "new Array(3).length" => "3",
        "[1, 2, 3].find(x => x > 1)" => "2",
        "[1, 2, 3].at(-1)" => "3",
        "Array.prototype.slice.call({ length: 2, 0: 'a', 1: 'b' }).join()" => "a,b",
        "(function () { return Array.prototype.join.call(arguments, '+') })(1, 2)" => "1+2",
        "'Hello'.toUpperCase()" => "HELLO",
        "'a,b,,c'.split(',').length" => "4",
        "'abc'.split('').join('|')" => "a|b|c",
        "' x '.trim() + '!'" => "x!",
        "'abc'.padStart(5, '-')" => "--abc",
        "'ab'.repeat(3)" => "ababab",
        "'Hello'.slice(1, -1)" => "ell",
        "'Hello'.substring(4, 1)" => "ell",
        "'abc'.charCodeAt(1)" => "98",
        "String.fromCharCode(72, 105)" => "Hi",
        "'aaa'.replace('a', 'b')" => "baa",
        "'aaa'.replaceAll('a', 'b')" => "bbb",
        "'x-y-z'.replace(/-/g, '+')" => "x+y+z",
        "'John Smith'.replace(/(\\w+)\\s(\\w+)/, '$2, $1')" => "Smith, John",
        "'abc'.replace(/b/, m => m.toUpperCase())" => "aBc",
        "'a1b22c333'.match(/\\d+/g).join()" => "1,22,333",
        "'2026-11-12'.match(/(\\d+)-(\\d+)-(\\d+)/).slice(1).join('/')" => "2026/11/12",
        "/(?<y>\\d{4})/.exec('in 2026').groups.y" => "2026",
        "/^[a-z]+$/i.test('HeLLo')" => "true",
        "'aBc'.search(/[A-Z]/)" => "1",
        "'one  two'.split(/\\s+/).join('|')" => "one|two",
        "/a.c/s.test('a\\nc')" => "true",
        "/(?<=\\$)\\d+/.exec('cost $42')[0]" => "42",
        "/(a)\\1/.test('aa')" => "true",
        "var r = /o/g, n = 0; while (r.exec('foo boo')) n++; n" => "4",
        "(255).toString(16)" => "ff",
        "(0.5).toString(2)" => "0.1",
        "(3.14159).toFixed(2)" => "3.14",
        "(1234.5678).toPrecision(6)" => "1234.57",
        "(1e21).toLocaleString()" => "1e+21",
        "(1234567.891).toLocaleString()" => "1,234,567.891",
        "parseInt('42px')" => "42",
        "parseInt('ff', 16)" => "255",
        "parseInt('0x1A')" => "26",
        "parseFloat('3.5e2abc')" => "350",
        "Number('  12  ')" => "12",
        "Number('12px')" => "NaN",
        "Number('')" => "0",
        "Number.isInteger(5.0)" => "true",
        "Math.round(2.5) + Math.round(-2.5) + Math.round(0.49)" => "1",
        "Math.floor(-1.5)" => "-2",
        "Math.trunc(-1.7)" => "-1",
        "Math.hypot(3, 4)" => "5",
        "Math.imul(0xffffffff, 5)" => "-5",
        "Math.max()" => "-Infinity",
        "var r = Math.random(); r >= 0 && r < 1" => "true",
        "JSON.stringify({ a: [1, 'x', null, true], b: undefined, c: () => 1 })" => "{\"a\":[1,\"x\",null,true]}",
        "JSON.stringify('he said \"hi\"\\n')" => "\"he said \\\"hi\\\"\\n\"",
        "JSON.stringify({ a: 1, b: [2] }, null, 2)" => "{\n  \"a\": 1,\n  \"b\": [\n    2\n  ]\n}",
        "JSON.parse('{\"a\":[1,2,{\"b\":null}],\"c\":\"\\\\u0041\"}').c" => "A",
        "JSON.parse('[1,2,3]').length" => "3",
        "try { JSON.parse('{bad') } catch (e) { e.name }" => "SyntaxError",
        "JSON.stringify({ d: new Date(0) })" => "{\"d\":\"1970-01-01T00:00:00.000Z\"}",
        "JSON.stringify({ toJSON() { return 'custom' } })" => "\"custom\"",
        "JSON.stringify({ a: 1, b: 2 }, (k, v) => k === 'a' ? undefined : v)" => "{\"b\":2}",
        "JSON.parse('{\"a\":1,\"b\":2}', (k, v) => typeof v === 'number' ? v * 10 : v).b" => "20",
        "new Date(Date.UTC(2026, 10, 12, 4, 5, 6)).toISOString()" => "2026-11-12T04:05:06.000Z",
        "new Date('2026-11-12T00:00:00Z').getTime()" => "1794441600000",
        "new Date(2020, 1, 29).getDate()" => "29",
        "new Date('2026-11-12T10:00:00Z').getUTCDay()" => "4",
        "Date.parse('Thu, 12 Nov 2026 10:00:00 GMT') === Date.parse('2026-11-12T10:00:00Z')" => "true",
        "encodeURIComponent('a b&c/é')" => "a%20b%26c%2F%C3%A9",
        "decodeURIComponent('a%20b%C3%A9')" => "a bé",
        "encodeURI('http://x.y/a b?q=1')" => "http://x.y/a%20b?q=1",
        "escape('a b')" => "a%20b",
        "isNaN('x')" => "true",
        "[1, 2, 3].toString()" => "1,2,3",
        "String([1, [2, 3]])" => "1,2,3",
        "String(null) + String(undefined)" => "nullundefined",
        "Boolean('') + ',' + Boolean('0')" => "false,true",
        "Object.is(-0, 0)" => "false",
        "(function () {}).toString()" => "function () {}",
        "Math.max.toString()" => "function max() { [native code] }",
        "var o = { a: 1 }; Object.assign({}, o, { b: 2 }).b" => "2",
        "Object.fromEntries([['a', 1], ['b', 2]]).b" => "2",
        "typeof globalThis" => "object",
        "[...'ab'].length" => "2",
    }
}

#[test]
fn host_protocol() {
    let mut e = Engine::new();
    let doc = e.host_object("Document", 1.0, None);
    let g = e.global();
    let gv = Value::Obj(g);
    let key = e.atom("document");
    let _ = key;
    e.define(g, "document", Value::Obj(doc), 7);
    // a host method on a prototype
    let proto = e.new_object();
    let f = e.host_function("getElementById", 1);
    e.define(proto, "getElementById", Value::Obj(f), 3);
    let el = e.host_object("Element", 7.0, Some(proto));
    let _ = (gv, el);
    let mut st = e.eval("var t = document.title; var got = [1,2].map(function (i) { return document.title + i }); t + '|' + got.join(',') + '|' + document.title.length");
    let mut asked = 0;
    loop {
        match st {
            Status::Host(HostReq::Get { key, .. }) => {
                assert_eq!(ling_js::strs::to_rust(&key), "title");
                asked += 1;
                st = e.resume(Value::str("Home"));
            },
            other => {
                let s = show(&mut e, other);
                assert_eq!(s, "Home|Home1,Home2|4");
                break;
            },
        }
    }
    assert_eq!(asked, 4);
    // a set the host does not take becomes an expando
    let st = e.eval("document.myThing = 5; document.myThing");
    let st = match st {
        Status::Host(HostReq::Set { .. }) => e.resume(Value::Bool(false)),
        o => o,
    };
    let st = match st {
        Status::Host(HostReq::Get { .. }) => e.resume(Value::Empty),
        o => o,
    };
    assert_eq!(show(&mut e, st), "5");
}

#[test]
fn host_call_and_nested_run() {
    let mut e = Engine::new();
    let g = e.global();
    let f = e.host_function("dispatch", 1);
    e.define(g, "dispatch", Value::Obj(f), 3);
    // the script registers a handler, then asks the host to dispatch: the host runs the
    // handler (a nested run) before answering
    let st = e.eval("var log = []; function onEvt(x) { log.push('evt' + x); return 7 } var r = dispatch(onEvt); log.push('r' + r); log.join()");
    let (func, args) = match st {
        Status::Host(HostReq::Call { name, args, .. }) => {
            assert_eq!(ling_js::strs::to_rust(&name), "dispatch");
            (args[0].clone(), args)
        },
        o => panic!("expected a host call, got {}", show(&mut e, o)),
    };
    let _ = args;
    let inner = e.call(func, Value::Undefined, vec![Value::Num(1.0)]);
    let v = match inner {
        Status::Done(v) => v,
        o => panic!("inner: {}", show(&mut e, o)),
    };
    let st = e.resume(v);
    assert_eq!(show(&mut e, st), "evt1,r7");
}

#[test]
fn budget_stops_a_runaway_loop() {
    let mut e = Engine::new();
    e.budget = 100_000;
    let st = e.eval("while (true) {}");
    assert!(show(&mut e, st).contains("ran too long"));
    // the engine is still usable
    let st = e.eval("1 + 1");
    assert_eq!(show(&mut e, st), "2");
}

#[test]
fn gc_keeps_live_objects() {
    let mut e = Engine::new();
    let st = e.eval("var keep = []; for (var i = 0; i < 200000; i++) { var o = { i: i }; if (i % 1000 == 0) keep.push(o) } keep.length + ':' + keep[199].i");
    assert_eq!(show(&mut e, st), "200:199000");
    assert!(e.heap.live < 100_000, "garbage was not collected: {}", e.heap.live);
}

#[test]
fn deep_recursion_is_a_range_error() {
    check! {
        "function f() { return f() } try { f() } catch (e) { e.name }" => "RangeError",
    }
}

/// eval, then drain the microtask queue (answering no host requests); the script's value, or
/// the value of `out` when the script defines one, after the jobs ran
fn run_all(src: &str) -> String {
    let mut e = Engine::new();
    let st = e.eval(src);
    let first = show(&mut e, st);
    let st = e.run_jobs();
    let after = show(&mut e, st);
    if after != "undefined" {
        return format!("jobs: {}", after);
    }
    let g = Value::Obj(e.global());
    let out = e.get_str(&g, "out").unwrap_or(Value::Undefined);
    let mut res = match out {
        Value::Undefined => first,
        v => show(&mut e, Status::Done(v)),
    };
    for l in e.log.drain(..) {
        res.push_str(&format!(" [{}]", l));
    }
    res
}

macro_rules! check_jobs {
    ($($src:expr => $want:expr),* $(,)?) => {{
        let mut bad = Vec::new();
        $(
            let got = run_all($src);
            if got != $want {
                bad.push(format!("\n  {}\n    want: {}\n    got:  {}", $src, $want, got));
            }
        )*
        if !bad.is_empty() {
            panic!("{} failing:{}", bad.len(), bad.join(""));
        }
    }};
}

#[test]
fn microtasks() {
    check_jobs! {
        "var out = []; queueMicrotask(() => out.push('a')); out.push('sync'); queueMicrotask(() => { out.push('b'); queueMicrotask(() => out.push('c')) }); 0" => "['sync', 'a', 'b', 'c']",
        "var out = 1; queueMicrotask(() => { throw new Error('bad') }); queueMicrotask(() => { out = 2 }); out" => "2 [uncaught Error: bad]",
    }
}

#[test]
fn jobs_suspend_for_the_host() {
    let mut e = Engine::new();
    let doc = e.host_object("Document", 1.0, None);
    let g = e.global();
    e.define(g, "document", Value::Obj(doc), 7);
    let st = e.eval("var log = []; queueMicrotask(() => log.push(document.title)); queueMicrotask(() => log.push('next')); 0");
    assert_eq!(show(&mut e, st), "0");
    assert!(e.has_jobs());
    let mut st = e.run_jobs();
    let mut asked = 0;
    while let Status::Host(HostReq::Get { .. }) = st {
        asked += 1;
        st = e.resume(Value::str("T"));
    }
    assert_eq!(asked, 1);
    assert_eq!(show(&mut e, st), "undefined");
    assert!(!e.has_jobs());
    let st = e.eval("log.join()");
    assert_eq!(show(&mut e, st), "T,next");
}

#[test]
fn promises_and_job_order() {
    check_jobs! {
        "var out = []; Promise.resolve(1).then(v => out.push('a' + v)); out.push('sync'); Promise.reject(2).catch(v => out.push('c' + v)); 0" => "['sync', 'a1', 'c2']",
        "var out = []; var p = new Promise(r => { out.push('exec'); r(5) }); p.then(v => out.push(v)).then(() => out.push('chain')); out.push('after'); 0" => "['exec', 'after', 5, 'chain']",
        // a then-returned promise takes two more ticks (the thenable job)
        "var out = []; Promise.resolve().then(() => { out.push(1); return Promise.resolve() }).then(() => out.push(4)); Promise.resolve().then(() => out.push(2)).then(() => out.push(3)).then(() => out.push(5)).then(() => out.push(6)); 0" => "[1, 2, 3, 5, 4, 6]",
        "var out; Promise.all([1, Promise.resolve(2), new Promise(r => queueMicrotask(() => r(3)))]).then(v => out = v.join()); 0" => "1,2,3",
        "var out; Promise.allSettled([Promise.reject('x'), 1]).then(v => out = v.map(r => r.status).join()); 0" => "rejected,fulfilled",
        "var out; Promise.race([new Promise(() => {}), Promise.resolve('fast')]).then(v => out = v); 0" => "fast",
        "var out; Promise.any([Promise.reject(1), Promise.reject(2)]).catch(e => out = e.constructor.name + ':' + e.errors.join()); 0" => "AggregateError:1,2",
        "var out = []; Promise.resolve(1).finally(() => out.push('f')).then(v => out.push(v)); 0" => "['f', 1]",
        "Promise.reject(new Error('lost')); 1" => "1 [uncaught (in promise) Error: lost]",
        "var p = Promise.reject(1); p.catch(() => {}); 2" => "2",
        "var out; var { promise, resolve } = Promise.withResolvers(); promise.then(v => out = v); resolve('wr'); 0" => "wr",
        "var out = typeof Promise.prototype.then + Object.prototype.toString.call(Promise.resolve())" => "function[object Promise]",
        "var out; class P2 extends Promise {} var p = P2.resolve(1); out = (p instanceof P2) + ',' + (p.then(x => x) instanceof P2)" => "true,true",
    }
}

#[test]
fn async_functions() {
    check_jobs! {
        "var out = []; async function f() { out.push('f1'); await null; out.push('f2'); return 'r' } f().then(v => out.push(v)); out.push('sync'); 0" => "['f1', 'sync', 'f2', 'r']",
        "var out = []; async function f(x) { var y = await x; out.push('f2:' + y); return y + 1 } f(Promise.resolve(41)).then(v => out.push('then:' + v)); Promise.resolve().then(() => out.push('p1')).then(() => out.push('p2')); out.push('sync'); 0" => "['sync', 'f2:41', 'p1', 'then:42', 'p2']",
        "var out; async function f() { throw new Error('bad') } f().catch(e => out = 'caught ' + e.message); 0" => "caught bad",
        "var out; async function f() { try { await Promise.reject('no') } catch (e) { return 'c:' + e } finally { out = 'fin' } } f().then(v => out += ',' + v); 0" => "fin,c:no",
        "var out; var o = { v: 7, m() { return (async () => this.v + (await 1))() } }; o.m().then(v => out = v); 0" => "8",
        "var out; class A { async m(x) { return x * 2 } } new A().m(4).then(v => out = v); 0" => "8",
        "var out; (async function () { var s = 0; for (const p of [1, 2, 3].map(x => Promise.resolve(x))) s += await p; out = s })(); 0" => "6",
        "var out = []; async function* g() { yield 1; await null; yield 2; return 3 } (async () => { for await (const v of g()) out.push(v); out.push('end') })(); 0" => "[1, 2, 'end']",
        "var out = []; (async () => { for await (const v of [Promise.resolve('a'), 'b']) out.push(v) })(); 0" => "['a', 'b']",
        "var out; async function f(a = x) {} f().catch(e => out = e.name); 0" => "ReferenceError",
        "var out = typeof (async () => {})().then" => "function",
    }
}

#[test]
fn generators() {
    check! {
        "function* g() { var x = yield 1; yield x * 2; return 9 } var it = g(); [it.next().value, it.next(5).value, it.next().value, it.next().done].join()" => "1,10,9,true",
        "function* g() { yield 1; yield 2; yield 3 } [...g()].join()" => "1,2,3",
        "function* g() { try { yield 1; yield 2 } finally { log.push('fin') } } var log = []; var it = g(); it.next(); var r = it.return(7); [r.value, r.done, log.join()].join()" => "7,true,fin",
        "function* g() { try { yield 1 } catch (e) { yield 'caught ' + e } } var it = g(); it.next(); it.throw('x').value" => "caught x",
        "function* g() { yield 1 } var it = g(); try { it.throw(new Error('t')) } catch (e) { e.message + it.next().done }" => "ttrue",
        "function* inner() { var r = yield 'a'; return r + '!' } function* outer() { var x = yield* inner(); yield x } var it = outer(); [it.next().value, it.next('b').value].join()" => "a,b!",
        "function* g() { yield* [1, 2]; yield* 'ab' } [...g()].join()" => "1,2,a,b",
        "var o = { *[Symbol.iterator]() { yield 'x'; yield 'y' } }; Array.from(o).join()" => "x,y",
        "class C { *gen() { yield this.v } constructor() { this.v = 3 } } [...new C().gen()].join()" => "3",
        "function* fib() { var a = 0, b = 1; for (;;) { yield a; [a, b] = [b, a + b] } } var out = []; for (const v of fib()) { if (v > 20) break; out.push(v) } out.join()" => "0,1,1,2,3,5,8,13",
        "function* g() {} var it = g(); [typeof it.next, it[Symbol.iterator]() === it, Object.prototype.toString.call(it)].join()" => "function,true,[object Generator]",
        "function* g() { yield 1 } try { new g() } catch (e) { e.name }" => "TypeError",
        "function* g() { var a = arguments.length; yield a } g(1, 2, 3).next().value" => "3",
        "function* g() { const it = yield; yield it } var i = g(); i.next(); i.next('again').value" => "again",
        "function* g(n) { if (n > 0) { yield n; yield* g(n - 1) } } [...g(3)].join()" => "3,2,1",
    }
}

#[test]
fn generator_suspends_for_the_host() {
    let mut e = Engine::new();
    let doc = e.host_object("Document", 1.0, None);
    let g = e.global();
    e.define(g, "document", Value::Obj(doc), 7);
    let mut st = e.eval("function* g() { yield document.title; yield document.title + '!' } var out = [...g()].join('|'); out");
    let mut asked = 0;
    while let Status::Host(HostReq::Get { .. }) = st {
        asked += 1;
        st = e.resume(Value::str(&format!("T{}", asked)));
    }
    assert_eq!(asked, 2);
    assert_eq!(show(&mut e, st), "T1|T2!");
    // an async function touching the page from a job
    let st = e.eval("var log = []; (async () => { await null; log.push(document.title) })(); 0");
    assert_eq!(show(&mut e, st), "0");
    let mut st = e.run_jobs();
    while let Status::Host(HostReq::Get { .. }) = st {
        st = e.resume(Value::str("Page"));
    }
    assert_eq!(show(&mut e, st), "undefined");
    let st = e.eval("log.join()");
    assert_eq!(show(&mut e, st), "Page");
}

#[test]
fn classes() {
    check! {
        "class A { constructor(x) { this.x = x } get double() { return this.x * 2 } static make() { return new A(4) } } A.make().double" => "8",
        "class A { m() { return 'A' } } class B extends A { m() { return 'B' + super.m() } } class C extends B { m() { return 'C' + super.m() } } new C().m()" => "CBA",
        "class A { constructor() { this.n = new.target.name } } class B extends A {} new B().n" => "B",
        "class A { x = 1; #y = 2; static z = 3; getY() { return this.#y } } var a = new A(); [a.x, a.getY(), A.z, Object.keys(a).join()].join()" => "1,2,3,x",
        "class A { #p() { return 'p' } q() { return this.#p() } static #s = 5; static s() { return A.#s } } [new A().q(), A.s()].join()" => "p,5",
        "class A { #x; static has(o) { return #x in o } } [A.has(new A()), A.has({})].join()" => "true,false",
        "class A { static count = 0; static { A.count = 10 } } A.count" => "10",
        "class E extends Error { constructor(m) { super(m); this.name = 'E' } } var e = new E('m'); [e instanceof E, e instanceof Error, e.message, String(e)].join()" => "true,true,m,E: m",
        "function F() { this.f = new.target === G } class G extends F { constructor() { super(); this.g = 1 } } var g = new G(); [g.f, g.g, g instanceof F].join()" => "true,1,true",
        "function H() { return { made: new.target.name } } class X extends H { constructor() { super(); this.mine = 1 } } var x = new X(); [x.made, x.mine].join()" => "X,1",
        "class L extends Array {} var l = new L(); l.push(1, 2); [l.length, l instanceof L, Array.isArray(l)].join()" => "2,true,true",
        "class A {} try { A() } catch (e) { e.name }" => "TypeError",
        "class A extends Object { constructor() { this.x = 1 } } try { new A() } catch (e) { e.name }" => "ReferenceError",
        "class A { constructor() { this.f = () => this } } class B extends A { constructor() { const s = () => super(); s() } } var b = new B(); b.f() === b" => "true",
        "class A { ['k' + 1]() { return 1 } [Symbol.iterator]() { return [7][Symbol.iterator]() } } [new A().k1(), ...new A()].join()" => "1,7",
        "var C = class Named { who() { return Named.name } }; new C().who()" => "Named",
        "class A { static #n = 0; static inc() { return ++A.#n } } A.inc(); A.inc()" => "2",
        "class A { a = this.constructor.name; b = () => this.a } new (class B extends A {})().b()" => "B",
        "var o = { __proto__: { hi() { return 'p' } }, hi() { return 'o' + super.hi() } }; o.hi()" => "op",
        "class A { get x() { return 1 } set x(v) { this._x = v } } var a = new A(); a.x = 5; [a.x, a._x, Object.keys(A.prototype).length].join()" => "1,5,0",
        "typeof class {}" => "function",
        "class A { static m() { return this.name } } class B extends A {} B.m()" => "B",
        "class B extends null { constructor() { return Object.create(B.prototype) } } typeof new B()" => "object",
        "var o = {}; Object.setPrototypeOf(o, Array.prototype); o instanceof Array" => "true",
    }
}

#[test]
fn symbols() {
    check! {
        "typeof Symbol()" => "symbol",
        "typeof Symbol.iterator" => "symbol",
        "String(Symbol('d'))" => "Symbol(d)",
        "Symbol('d').description" => "d",
        "Symbol.for('k') === Symbol.for('k')" => "true",
        "Symbol('k') === Symbol('k')" => "false",
        "Symbol.keyFor(Symbol.for('q'))" => "q",
        "var s = Symbol(); var o = { [s]: 1, a: 2 }; [o[s], Object.keys(o).join(), Object.getOwnPropertySymbols(o).length, JSON.stringify(o)].join()" => "1,a,1,{\"a\":2}",
        "try { Symbol() + '' } catch (e) { e.name }" => "TypeError",
        "try { new Symbol() } catch (e) { e.name }" => "TypeError",
        "var o = { [Symbol.toPrimitive](h) { return h === 'number' ? 1 : 'str' } }; `${o}`" => "str",
        "Object.prototype.toString.call({ [Symbol.toStringTag]: 'T' })" => "[object T]",
        "class Even { static [Symbol.hasInstance](n) { return n % 2 === 0 } } 2 instanceof Even" => "true",
        "var s = Symbol('x'); var o = {}; o[s] = 5; for (var k in o) throw 1; s in o" => "true",
    }
}

#[test]
fn collections() {
    check! {
        "var m = new Map([[1, 'a'], ['1', 'b']]); m.set(NaN, 'n').set(-0, 'z'); [m.size, m.get(1), m.get('1'), m.get(NaN), m.get(0)].join()" => "4,a,b,n,z",
        "var m = new Map(); m.set('a', 1).set('b', 2).set('a', 3); [...m].join(';')" => "a,3;b,2",
        "var m = new Map([['x', 1]]); var out = []; m.forEach((v, k) => out.push(k + v)); out.join()" => "x1",
        "var s = new Set([1, 2, 2, 3]); [s.size, s.has(2), s.delete(2), s.has(2), [...s].join()].join()" => "3,true,true,false,1,3",
        "var s = new Set([1, 2, 3]), seen = []; for (const v of s) { seen.push(v); if (v === 1) { s.delete(2); s.add(4) } } seen.join()" => "1,3,4",
        "var m = new Map(); for (var i = 0; i < 1000; i++) m.set(i, i); for (var i = 0; i < 990; i++) m.delete(i); [m.size, m.get(995), [...m.keys()][0]].join()" => "10,995,990",
        "var wm = new WeakMap(), k = {}; wm.set(k, 'v'); [wm.get(k), wm.has(k), wm.has({}), Object.keys(k).length, wm.delete(k), wm.has(k)].join()" => "v,true,false,0,true,false",
        "var ws = new WeakSet(), o = {}; ws.add(o); [ws.has(o), ws.has({})].join()" => "true,false",
        "try { new WeakMap().set(1, 1) } catch (e) { e.name }" => "TypeError",
        "new WeakRef({ a: 1 }).deref().a" => "1",
        "new Set('abca').size" => "3",
        "var m = new Map(Object.entries({ a: 1, b: 2 })); [...m.values()].join()" => "1,2",
        "Object.prototype.toString.call(new Set())" => "[object Set]",
        "[...new Set([3, 1]).union(new Set([1, 2]))].join()" => "3,1,2",
        "Map.groupBy([1, 2, 3, 4], x => x % 2 ? 'odd' : 'even').get('odd').join()" => "1,3",
        "class M extends Map { constructor() { super([[1, 1]]); this.t = 1 } } var m = new M(); [m.get(1), m instanceof M, m.t].join()" => "1,true,1",
    }
}

#[test]
fn eval_and_function() {
    check! {
        "eval('1 + 2')" => "3",
        "eval('var ev = 5'); ev" => "5",
        "(0, eval)('typeof globalThis')" => "object",
        "eval('let a = 1; a + 1') + eval('let a = 2; a')" => "4",
        "eval(42)" => "42",
        "try { eval('(') } catch (e) { e.name }" => "SyntaxError",
        "new Function('a', 'b', 'return a + b')(2, 3)" => "5",
        "Function('return this')() === globalThis" => "true",
        "Function('a, b', 'return a * b')(3, 4)" => "12",
        "var f = new Function(); typeof f + f()" => "functionundefined",
        "eval('function evf() { return 9 }'); evf()" => "9",
        "\u{feff}1 + 1" => "2",
    }
}

#[test]
fn proxy_and_reflect() {
    check! {
        "var log = []; var p = new Proxy({ a: 1 }, { get(t, k, r) { log.push(String(k)); return k in t ? t[k] : 'dflt' } }); [p.a, p.b, log.join()].join()" => "1,dflt,a,b",
        "var t = {}; var p = new Proxy(t, { set(t, k, v) { t[k] = v * 2; return true } }); p.x = 5; t.x" => "10",
        "var p = new Proxy({}, { has(t, k) { return k === 'magic' } }); ['magic' in p, 'other' in p].join()" => "true,false",
        "var p = new Proxy({ a: 1, b: 2 }, { deleteProperty(t, k) { return false } }); delete p.a" => "false",
        "var p = new Proxy({ a: 1, b: 2 }, { ownKeys(t) { return ['b'] }, getOwnPropertyDescriptor(t, k) { return { value: t[k], enumerable: true, configurable: true } } }); Object.keys(p).join()" => "b",
        "var p = new Proxy(function (a, b) { return a + b }, { apply(t, self, args) { return t(...args) * 10 } }); p(1, 2)" => "30",
        "var P = new Proxy(class { constructor(x) { this.x = x } }, { construct(t, args) { return { made: args[0] } } }); new P(7).made" => "7",
        "var p = new Proxy({ v: 3 }, {}); p.w = 4; [p.v, p.w, Object.keys(p).join(), 'v' in p].join()" => "3,4,v,w,true",
        "var { proxy, revoke } = Proxy.revocable({}, {}); revoke(); try { proxy.x } catch (e) { e.name }" => "TypeError",
        "typeof new Proxy(function () {}, {})" => "function",
        "var p = new Proxy([1, 2, 3], {}); [Array.isArray([]), p.length, p[1]].join()" => "true,3,2",
        "Reflect.ownKeys({ a: 1, [Symbol.iterator]: 0 }).length" => "2",
        "Reflect.has({ x: 1 }, 'x') + ',' + Reflect.get({ get y() { return this.z }, z: 9 }, 'y')" => "true,9",
        "var o = {}; Reflect.defineProperty(o, 'k', { value: 1 }); [o.k, Reflect.getOwnPropertyDescriptor(o, 'k').writable].join()" => "1,false",
        "Reflect.apply(Math.max, null, [1, 5, 2])" => "5",
        "function F(a) { this.a = a } function G() {} var o = Reflect.construct(F, [3], G); [o.a, o instanceof G].join()" => "3,true",
        "var o = Object.freeze({}); Reflect.set(o, 'x', 1)" => "false",
        "Reflect.getPrototypeOf([]) === Array.prototype" => "true",
        "var t = { n: 1 }; var p = new Proxy(t, { set(t, k, v, r) { return Reflect.set(t, k, v, r) } }); p.n = 5; p.m = 6; [t.n, t.m].join()" => "5,6",
    }
}

#[test]
fn typed_arrays_and_text() {
    check! {
        "var a = new Uint8Array([1, 2, 300]); [a.length, a[2], a.byteLength, a instanceof Uint8Array].join()" => "3,44,3,true",
        "var b = new ArrayBuffer(8); var v = new DataView(b); v.setUint16(0, 0x1234); v.setFloat32(4, 1.5, true); [v.getUint8(0), v.getUint8(1), new Float32Array(b, 4, 1)[0]].join()" => "18,52,1.5",
        "var f = new Float64Array(2); f[0] = 0.1; f[1] = -1; Array.from(f).join()" => "0.1,-1",
        "var i = new Int16Array([-1, 32768]); [i[0], i[1]].join()" => "-1,-32768",
        "var c = new Uint8ClampedArray([300, -5, 1.5, 2.5]); Array.from(c).join()" => "255,0,2,2",
        "var a = new Uint32Array([5, 1, 4]); a.sort(); a.join()" => "1,4,5",
        "var a = new Int8Array([1, 2, 3, 4]); var s = a.subarray(1, 3); s[0] = 9; [a.join(), s.length].join('|')" => "1,9,3,4|2",
        "Array.from(new Uint8Array([1, 2]).map(x => x * 3)).join()" => "3,6",
        "var a = new Uint8Array(4); a.set([7, 8], 1); a.join()" => "0,7,8,0",
        "[...new Uint16Array([3, 4])].join()" => "3,4",
        "Uint8Array.from(new Set([5, 6])).join()" => "5,6",
        "Object.prototype.toString.call(new Float32Array(1))" => "[object Float32Array]",
        "new Uint8Array(new ArrayBuffer(16), 4).length" => "12",
        "var e = new TextEncoder().encode('hé€'); [e.length, e[1], e[2]].join()" => "6,195,169",
        "new TextDecoder().decode(new Uint8Array([104, 195, 169, 226, 130, 172]))" => "hé€",
        "btoa('hello')" => "aGVsbG8=",
        "atob('aGVsbG8=')" => "hello",
        "atob(btoa('\\u00ff\\u0001'))" => "\u{ff}\u{1}",
        "try { btoa('€') } catch (e) { e.name }" => "InvalidCharacterError",
        "var b = new BigInt64Array(1); b[0] = -5n; typeof b[0] + b[0]" => "bigint-5",
        "ArrayBuffer.isView(new DataView(new ArrayBuffer(1)))" => "true",
    }
}

/// run a module graph: `files` answers the host's imports by URL; then the jobs; returns the
/// value of the global `out` and the URLs the host was asked for
fn run_modules(main: &str, files: &[(&str, &str)]) -> (String, Vec<String>) {
    let mut e = Engine::new();
    let mut asked = Vec::new();
    let mut st = e.eval_module(main, "https://site.test/app/main.js");
    loop {
        st = match st {
            Status::Host(HostReq::Import { url }) => {
                let u = ling_js::strs::to_rust(&url);
                asked.push(u.clone());
                match files.iter().find(|(k, _)| *k == u) {
                    Some((_, src)) => e.resume(Value::str(src)),
                    None => {
                        let err = e.type_error(&format!("404 {}", u));
                        e.resume_throw(err)
                    },
                }
            },
            Status::Error(v) => {
                let m = e.display(&v);
                return (format!("Error: {}", m), asked);
            },
            Status::Done(_) => {
                if e.has_jobs() {
                    e.run_jobs()
                } else {
                    break;
                }
            },
            other => panic!("unexpected {:?}", other),
        };
    }
    let g = Value::Obj(e.global());
    let out = e.get_str(&g, "out").unwrap_or(Value::Undefined);
    let mut s = show(&mut e, Status::Done(out));
    for l in e.log.drain(..) {
        s.push_str(&format!(" [{}]", l));
    }
    (s, asked)
}

#[test]
fn modules() {
    // imports, live bindings, default, namespace, renames, re-exports
    let (out, asked) = run_modules(
        "import def, { count, inc as bump } from './lib.js'; import * as ns from './lib.js'; import { twice } from '../shared/util.js';
         bump(); bump(); globalThis.out = [def, count, ns.count, twice(count), typeof ns.inc, Object.keys(ns).join('+')].join();",
        &[
            ("https://site.test/app/lib.js", "export let count = 0; export function inc() { count++ } export default 'D'; export { twice } from '../shared/util.js';"),
            ("https://site.test/shared/util.js", "export const twice = x => x * 2;"),
        ],
    );
    assert_eq!(out, "D,2,2,4,function,count+inc+default+twice");
    assert_eq!(asked, vec!["https://site.test/app/lib.js", "https://site.test/shared/util.js"]);

    // a cycle: a.js and b.js import each other; functions are hoisted across it
    let (out, _) = run_modules(
        "import { a } from './a.js'; globalThis.out = a();",
        &[
            ("https://site.test/app/a.js", "import { b, order } from './b.js'; order.push('a'); export function a() { return 'a>' + b() + '|' + order.join() } export function fromA() { return 'A' }"),
            ("https://site.test/app/b.js", "import { fromA } from './a.js'; export const order = ['b:' + fromA()]; export function b() { return 'b' }"),
        ],
    );
    assert_eq!(out, "a>b|b:A,a");

    // export *, export * as, a module evaluated once
    let (out, asked) = run_modules(
        "import { x, y, all } from './star.js'; import './once.js'; import './once.js'; globalThis.out = [x, y, all.x, globalThis.ran].join();",
        &[
            ("https://site.test/app/star.js", "export * from './x.js'; export * as all from './x.js'; export const y = 'Y';"),
            ("https://site.test/app/x.js", "export const x = 'X'; export default 'not re-exported';"),
            ("https://site.test/app/once.js", "globalThis.ran = (globalThis.ran || 0) + 1;"),
        ],
    );
    assert_eq!(out, "X,Y,X,1");
    assert_eq!(asked.len(), 3);

    // dynamic import (relative to the importing module), data: URLs, import.meta, top-level await
    let (out, asked) = run_modules(
        "const m = await import('./dyn.js'); const d = await import('data:text/javascript,export%20const%20v%20%3D%2042%3B');
         const b = await import('data:text/javascript;base64,ZXhwb3J0IGRlZmF1bHQgNw==');
         globalThis.out = [m.default, m.where, d.v, typeof b, import.meta.url].join();",
        &[("https://site.test/app/dyn.js", "await null; export default 'dyn'; export const where = import.meta.url;")],
    );
    assert_eq!(out, "dyn,https://site.test/app/dyn.js,42,object,https://site.test/app/main.js");
    assert_eq!(asked, vec!["https://site.test/app/dyn.js"]);

    // a failed fetch rejects import(); bare specifiers are a TypeError
    let (out, _) = run_modules(
        "globalThis.out = []; import('./missing.js').catch(e => out.push(e.message)); import('lodash').catch(e => out.push(e.name));",
        &[],
    );
    assert_eq!(out, "['404 https://site.test/app/missing.js', 'TypeError']");

    // a static import that fails fails the module; a syntax error is a SyntaxError
    let (out, _) = run_modules("import './nope.js'; globalThis.out = 1;", &[]);
    assert_eq!(out, "Error: TypeError: 404 https://site.test/app/nope.js");
    let (out, _) = run_modules("export const = 1;", &[]);
    assert!(out.starts_with("Error: SyntaxError"), "{}", out);
    // imports are read-only; module scope is not global
    let (out, _) = run_modules(
        "import { k } from './k.js'; var local = 1; try { k = 2 } catch (e) { globalThis.out = e.name + ',' + typeof globalThis.local }",
        &[("https://site.test/app/k.js", "export const k = 1;")],
    );
    assert_eq!(out, "TypeError,undefined");
    // classic scripts: import() too
    let mut e = Engine::new();
    e.base_url = "https://site.test/page.html".into();
    let mut st = e.eval("var got; import('./m.js').then(m => got = m.v); 0");
    loop {
        st = match st {
            Status::Host(HostReq::Import { url }) => {
                assert_eq!(ling_js::strs::to_rust(&url), "https://site.test/m.js");
                e.resume(Value::str("export const v = 'classic'"))
            },
            Status::Done(_) if e.has_jobs() => e.run_jobs(),
            _ => break,
        };
    }
    let st = e.eval("got");
    assert_eq!(show(&mut e, st), "classic");
}

#[test]
fn inline_modules_with_one_url() {
    let mut e = Engine::new();
    let st = e.eval_module("globalThis.n = (globalThis.n || 0) + 1;", "https://site.test/");
    assert_eq!(show(&mut e, st), "undefined");
    let st = e.eval_module("globalThis.n += 10;", "https://site.test/");
    assert_eq!(show(&mut e, st), "undefined");
    let st = e.eval("n");
    assert_eq!(show(&mut e, st), "11");
}

#[test]
fn methods_named_like_outer_variables() {
    check! {
        // a getter / method / class method with the name of a variable it reads (Reddit's chunks)
        "var o = (() => { var I = class {}; return { get I() { return I } } })(); typeof o.I" => "function",
        "var o = (function () { let m = 5; return { m() { return m } } })(); o.m()" => "5",
        "(() => { const A = 1; class B { A() { return A } static A() { return A } } return new B().A() + B.A() })()" => "2",
        "var f = (() => { function D(e) { return e } return { D: (x) => D(x), get D2() { return D } } })(); f.D(3) + typeof f.D2" => "3function",
        "var g = function h() { return typeof h }; g()" => "function",
    }
}

#[test]
fn budget_cannot_be_caught() {
    let mut e = Engine::new();
    e.budget = 200_000;
    let st = e.eval("var n = 0; while (true) { try { while (true) n++ } catch (x) {} finally { n++ } }");
    assert!(show(&mut e, st).contains("ran too long"));
    let st = e.eval("2 + 2");
    assert_eq!(show(&mut e, st), "4");
}
