// LingJS prelude: the parts of the standard library written in JavaScript (anything that calls
// back into script), the slow paths of natives that met a getter / host object / user method,
// and the helpers compiled code uses (iteration, spread, destructuring).
(function (global) {
"use strict";
var AP = Array.prototype, SP = String.prototype, OP = Object.prototype, FP = Function.prototype;
var isArrayLike = __isArrayLike, hide = __hide, slow = __slow, intrinsic = __intrinsic;
var SYM_IT = Symbol.iterator, SYM_AIT = Symbol.asyncIterator, SYM_SPECIES = Symbol.species, SYM_TAG = Symbol.toStringTag;
var IterProto = __proto("Iterator"), AsyncIterProto = __proto("AsyncIterator"), AsyncGenProto = __proto("AsyncGenerator");
var enqueue = __enqueueJob, track = __trackRejection, genResume = __genResume, genState = __genState, privateSymbol = __privateSymbol, isGenerator = __isGenerator;
function tag(o, t) { Object.defineProperty(o, SYM_TAG, { value: t, configurable: true }); }

function def(o, name, f) { o[name] = f; hide(o, name); }
function toLen(v) { v = +v; if (!(v > 0)) return 0; v = Math.floor(v); return v > 9007199254740991 ? 9007199254740991 : v; }
function toInt(v) { v = +v; if (v !== v) return 0; return v < 0 ? Math.ceil(v) : Math.floor(v); }
function relIndex(v, len, d) { if (v === undefined) return d; v = toInt(v); return v < 0 ? Math.max(len + v, 0) : Math.min(v, len); }
function callable(f, what) { if (typeof f !== "function") throw new TypeError(String(f) + " is not a function" + (what ? " (" + what + ")" : "")); return f; }
function obj(v, what) { if (v === null || v === undefined) throw new TypeError(what + " called on null or undefined"); return Object(v); }

// ── iteration ──
function ArrayIterator(a, kind) { this._a = a; this._i = 0; this._k = kind; }
ArrayIterator.prototype = Object.create(IterProto);
hide(ArrayIterator, "prototype");
def(ArrayIterator.prototype, "next", function () {
  var a = this._a;
  if (a === undefined) return { value: undefined, done: true };
  var i = this._i;
  if (i >= toLen(a.length)) { this._a = undefined; return { value: undefined, done: true }; }
  this._i = i + 1;
  var k = this._k;
  return { value: k === 0 ? a[i] : k === 1 ? i : [i, a[i]], done: false };
});
tag(ArrayIterator.prototype, "Array Iterator");

function StringIterator(s) { this._s = s; this._i = 0; }
StringIterator.prototype = Object.create(IterProto);
tag(StringIterator.prototype, "String Iterator");
def(StringIterator.prototype, "next", function () {
  var s = this._s, i = this._i;
  if (s === undefined || i >= s.length) { this._s = undefined; return { value: undefined, done: true }; }
  var c = s.charCodeAt(i), n = 1;
  if (c >= 0xD800 && c < 0xDC00 && i + 1 < s.length) { var d = s.charCodeAt(i + 1); if (d >= 0xDC00 && d < 0xE000) n = 2; }
  this._i = i + n;
  return { value: s.slice(i, i + n), done: false };
});

function getIterator(o) {
  if (o === null || o === undefined) throw new TypeError(String(o) + " is not iterable");
  var f = o[SYM_IT];
  if (typeof f === "function") return f.call(o);
  if (typeof o === "string") return new StringIterator(o);
  if (typeof o === "object" && typeof o.length === "number") return new ArrayIterator(o, 0);
  if (typeof o === "object" && typeof o.next === "function") return o;
  throw new TypeError("object is not iterable");
}
function iterToArray(o) {
  if (isArrayLike(o) && Array.isArray(o) && o[SYM_IT] === AP_values) return o;
  if (typeof o === "string") { var out = [], it = new StringIterator(o), r; while (!(r = it.next()).done) out.push(r.value); return out; }
  var it2 = getIterator(o), res = [], r2;
  while (!(r2 = it2.next()).done) res.push(r2.value);
  return res;
}
function iterClose(it) { if (it && typeof it["return"] === "function") it["return"](); }
function copyProps(target, src) {
  if (src === null || src === undefined) return target;
  if (typeof src === "string") { for (var i = 0; i < src.length; i++) target[i] = src[i]; return target; }
  var ks = Object.keys(src);
  for (var j = 0; j < ks.length; j++) {
    var k = ks[j];
    if (k === "__proto__") Object.defineProperty(target, k, { value: src[k], writable: true, enumerable: true, configurable: true });
    else target[k] = src[k];
  }
  var ss = Object.getOwnPropertySymbols(src);
  for (var m = 0; m < ss.length; m++) if (OP.propertyIsEnumerable.call(src, ss[m])) target[ss[m]] = src[ss[m]];
  return target;
}
function objectRest(src, excluded) {
  var out = {}, ks = Object.keys(Object(src));
  for (var i = 0; i < ks.length; i++) if (excluded.indexOf(ks[i]) < 0) out[ks[i]] = src[ks[i]];
  return out;
}
function templateObject(cooked, raw) {
  Object.freeze(raw);
  Object.defineProperty(cooked, "raw", { value: raw });
  return Object.freeze(cooked);
}
intrinsic(0, iterToArray);
intrinsic(1, getIterator);
intrinsic(2, copyProps);
intrinsic(3, objectRest);
intrinsic(4, templateObject);
intrinsic(5, iterClose);
// a native met an object argument it could not convert without script: convert, then retry
function toPrimDefault(x) {
  var order = x instanceof Date ? ["toString", "valueOf"] : ["valueOf", "toString"];
  for (var i = 0; i < 2; i++) {
    var f = x[order[i]];
    if (typeof f === "function") { var v = f.call(x); if (v === null || (typeof v !== "object" && typeof v !== "function")) return v; }
  }
  throw new TypeError("Cannot convert object to primitive value");
}
intrinsic(6, function (nat, self, args) {
  var a = [];
  for (var i = 0; i < args.length; i++) { var x = args[i]; a.push(x !== null && (typeof x === "object" || typeof x === "function") ? toPrimDefault(x) : x); }
  return nat.apply(self, a);
});

def(AP, "values", function () { return new ArrayIterator(obj(this, "Array.prototype.values"), 0); });
def(AP, "keys", function () { return new ArrayIterator(obj(this, "Array.prototype.keys"), 1); });
def(AP, "entries", function () { return new ArrayIterator(obj(this, "Array.prototype.entries"), 2); });
var AP_values = AP.values;
def(AP, SYM_IT, AP.values);
def(SP, SYM_IT, function () { return new StringIterator(String(obj(this, "String.prototype[@@iterator]"))); });

// ── Array: the methods that take callbacks ──
def(AP, "forEach", function (f, t) { var o = obj(this, "Array.prototype.forEach"), n = toLen(o.length); callable(f); for (var i = 0; i < n; i++) if (i in o) f.call(t, o[i], i, o); });
def(AP, "map", function (f, t) { var o = obj(this, "Array.prototype.map"), n = toLen(o.length); callable(f); var r = new Array(n); for (var i = 0; i < n; i++) if (i in o) r[i] = f.call(t, o[i], i, o); return r; });
def(AP, "filter", function (f, t) { var o = obj(this, "Array.prototype.filter"), n = toLen(o.length); callable(f); var r = []; for (var i = 0; i < n; i++) if (i in o) { var v = o[i]; if (f.call(t, v, i, o)) r.push(v); } return r; });
def(AP, "some", function (f, t) { var o = obj(this, "Array.prototype.some"), n = toLen(o.length); callable(f); for (var i = 0; i < n; i++) if (i in o && f.call(t, o[i], i, o)) return true; return false; });
def(AP, "every", function (f, t) { var o = obj(this, "Array.prototype.every"), n = toLen(o.length); callable(f); for (var i = 0; i < n; i++) if (i in o && !f.call(t, o[i], i, o)) return false; return true; });
def(AP, "find", function (f, t) { var o = obj(this, "Array.prototype.find"), n = toLen(o.length); callable(f); for (var i = 0; i < n; i++) { var v = o[i]; if (f.call(t, v, i, o)) return v; } return undefined; });
def(AP, "findIndex", function (f, t) { var o = obj(this, "Array.prototype.findIndex"), n = toLen(o.length); callable(f); for (var i = 0; i < n; i++) if (f.call(t, o[i], i, o)) return i; return -1; });
def(AP, "findLast", function (f, t) { var o = obj(this, "Array.prototype.findLast"), n = toLen(o.length); callable(f); for (var i = n - 1; i >= 0; i--) { var v = o[i]; if (f.call(t, v, i, o)) return v; } return undefined; });
def(AP, "findLastIndex", function (f, t) { var o = obj(this, "Array.prototype.findLastIndex"), n = toLen(o.length); callable(f); for (var i = n - 1; i >= 0; i--) if (f.call(t, o[i], i, o)) return i; return -1; });
def(AP, "reduce", function (f, init) {
  var o = obj(this, "Array.prototype.reduce"), n = toLen(o.length), i = 0, acc; callable(f);
  if (arguments.length > 1) acc = init;
  else { while (i < n && !(i in o)) i++; if (i >= n) throw new TypeError("Reduce of empty array with no initial value"); acc = o[i++]; }
  for (; i < n; i++) if (i in o) acc = f(acc, o[i], i, o);
  return acc;
});
def(AP, "reduceRight", function (f, init) {
  var o = obj(this, "Array.prototype.reduceRight"), i = toLen(o.length) - 1, acc; callable(f);
  if (arguments.length > 1) acc = init;
  else { while (i >= 0 && !(i in o)) i--; if (i < 0) throw new TypeError("Reduce of empty array with no initial value"); acc = o[i--]; }
  for (; i >= 0; i--) if (i in o) acc = f(acc, o[i], i, o);
  return acc;
});
function defaultCompare(a, b) {
  if (a === undefined) return b === undefined ? 0 : 1;
  if (b === undefined) return -1;
  var x = String(a), y = String(b);
  return x < y ? -1 : x > y ? 1 : 0;
}
function mergeSort(a, cmp) {
  var n = a.length; if (n < 2) return a;
  var tmp = new Array(n);
  for (var w = 1; w < n; w *= 2) {
    for (var lo = 0; lo < n; lo += 2 * w) {
      var mid = Math.min(lo + w, n), hi = Math.min(lo + 2 * w, n), i = lo, j = mid, k = lo;
      while (i < mid && j < hi) { var c = cmp(a[j], a[i]); if (c < 0) tmp[k++] = a[j++]; else tmp[k++] = a[i++]; }
      while (i < mid) tmp[k++] = a[i++];
      while (j < hi) tmp[k++] = a[j++];
    }
    var t = a; a = tmp; tmp = t;
  }
  return a;
}
def(AP, "sort", function (cmp) {
  if (cmp !== undefined) callable(cmp, "the comparison function");
  var o = obj(this, "Array.prototype.sort"), n = toLen(o.length), items = [], holes = 0, undefs = 0;
  for (var i = 0; i < n; i++) { if (i in o) { var v = o[i]; if (v === undefined) undefs++; else items.push(v); } else holes++; }
  var c = cmp === undefined ? defaultCompare : function (a, b) { var r = +cmp(a, b); return r !== r ? 0 : r; };
  items = mergeSort(items, c);
  var k = 0;
  for (; k < items.length; k++) o[k] = items[k];
  for (var u = 0; u < undefs; u++) o[k++] = undefined;
  for (; k < n; k++) delete o[k];
  return o;
});
def(AP, "toSorted", function (cmp) { return AP.slice.call(this).sort(cmp); });
def(AP, "toReversed", function () { return AP.slice.call(this).reverse(); });
def(AP, "with", function (i, v) { var a = AP.slice.call(this); i = toInt(i); if (i < 0) i += a.length; if (i < 0 || i >= a.length) throw new RangeError("Invalid index"); a[i] = v; return a; });
def(AP, "toSpliced", function () { var a = AP.slice.call(this); AP.splice.apply(a, arguments); return a; });
function flatten(out, src, depth) {
  for (var i = 0; i < src.length; i++) {
    if (!(i in src)) continue;
    var v = src[i];
    if (depth > 0 && Array.isArray(v)) flatten(out, v, depth - 1); else out.push(v);
  }
  return out;
}
def(AP, "flat", function (d) { return flatten([], obj(this, "Array.prototype.flat"), d === undefined ? 1 : toInt(d)); });
def(AP, "flatMap", function (f, t) { return flatten([], AP.map.call(this, f, t), 1); });
def(AP, "copyWithin", function (target, start, end) {
  var o = obj(this, "Array.prototype.copyWithin"), n = toLen(o.length);
  var to = relIndex(target, n, 0), from = relIndex(start, n, 0), fin = relIndex(end, n, n), count = Math.min(fin - from, n - to);
  if (from < to && to < from + count) { for (var i = count - 1; i >= 0; i--) { if (from + i in o) o[to + i] = o[from + i]; else delete o[to + i]; } }
  else for (var j = 0; j < count; j++) { if (from + j in o) o[to + j] = o[from + j]; else delete o[to + j]; }
  return o;
});
def(Array, "from", function (src, f, t) {
  if (f !== undefined) callable(f, "Array.from");
  if (src === null || src === undefined) throw new TypeError(String(src) + " is not iterable");
  var items;
  if (typeof src[SYM_IT] === "function" || typeof src === "string") items = iterToArray(src);
  else { var n = toLen(src.length); items = new Array(n); for (var i = 0; i < n; i++) items[i] = src[i]; }
  if (items === src) items = AP.slice.call(items);
  if (f !== undefined) for (var j = 0; j < items.length; j++) items[j] = f.call(t, items[j], j);
  return items;
});

// slow paths of the native Array methods: generic array-likes (arguments, NodeLists, frozen arrays)
slow(AP.push, function (o) { o = Object(o); var n = toLen(o.length); for (var i = 1; i < arguments.length; i++) o[n++] = arguments[i]; o.length = n; return n; });
slow(AP.pop, function (o) { o = Object(o); var n = toLen(o.length); if (n === 0) { o.length = 0; return undefined; } var v = o[n - 1]; delete o[n - 1]; o.length = n - 1; return v; });
slow(AP.shift, function (o) { o = Object(o); var n = toLen(o.length); if (n === 0) { o.length = 0; return undefined; } var v = o[0]; for (var i = 1; i < n; i++) { if (i in o) o[i - 1] = o[i]; else delete o[i - 1]; } delete o[n - 1]; o.length = n - 1; return v; });
slow(AP.unshift, function (o) { o = Object(o); var n = toLen(o.length), k = arguments.length - 1; for (var i = n - 1; i >= 0; i--) { if (i in o) o[i + k] = o[i]; else delete o[i + k]; } for (var j = 0; j < k; j++) o[j] = arguments[j + 1]; o.length = n + k; return n + k; });
slow(AP.slice, function (o, s, e) { o = Object(o); var n = toLen(o.length), a = relIndex(s, n, 0), b = relIndex(e, n, n), r = []; for (var i = a; i < b; i++) if (i in o) r[i - a] = o[i]; r.length = Math.max(b - a, 0); return r; });
slow(AP.splice, function (o, s, dc) {
  o = Object(o); var n = toLen(o.length), st = relIndex(s, n, 0);
  var del = arguments.length < 2 ? 0 : arguments.length < 3 ? n - st : Math.min(Math.max(toInt(dc), 0), n - st);
  var items = AP.slice.call(arguments, 3), removed = [];
  for (var i = 0; i < del; i++) if (st + i in o) removed[i] = o[st + i];
  var diff = items.length - del, j;
  if (diff < 0) { for (j = st + del; j < n; j++) { if (j in o) o[j + diff] = o[j]; else delete o[j + diff]; } for (j = n; j > n + diff; j--) delete o[j - 1]; }
  else if (diff > 0) { for (j = n - 1; j >= st + del; j--) { if (j in o) o[j + diff] = o[j]; else delete o[j + diff]; } }
  for (j = 0; j < items.length; j++) o[st + j] = items[j];
  o.length = n + diff;
  return removed;
});
slow(AP.join, function (o, sep) { o = Object(o); var n = toLen(o.length), s = sep === undefined ? "," : String(sep), r = ""; for (var i = 0; i < n; i++) { if (i > 0) r += s; var v = o[i]; if (v !== undefined && v !== null) r += String(v); } return r; });
slow(AP.indexOf, function (o, x, from) { o = Object(o); var n = toLen(o.length); for (var i = relIndex(from, n, 0); i < n; i++) if (i in o && o[i] === x) return i; return -1; });
slow(AP.lastIndexOf, function (o, x, from) { o = Object(o); var n = toLen(o.length), i = arguments.length > 2 ? toInt(from) : n - 1; if (i < 0) i += n; else if (i > n - 1) i = n - 1; for (; i >= 0; i--) if (i in o && o[i] === x) return i; return -1; });
slow(AP.includes, function (o, x, from) { o = Object(o); var n = toLen(o.length); for (var i = relIndex(from, n, 0); i < n; i++) { var v = o[i]; if (v === x || (v !== v && x !== x)) return true; } return false; });
slow(AP.reverse, function (o) { o = Object(o); var n = toLen(o.length); for (var i = 0, j = n - 1; i < j; i++, j--) { var a = o[i], b = o[j]; o[i] = b; o[j] = a; } return o; });
slow(AP.fill, function (o, v, s, e) { o = Object(o); var n = toLen(o.length); for (var i = relIndex(s, n, 0), end = relIndex(e, n, n); i < end; i++) o[i] = v; return o; });
slow(AP.at, function (o, i) { o = Object(o); var n = toLen(o.length); i = toInt(i); if (i < 0) i += n; return i < 0 || i >= n ? undefined : o[i]; });
slow(AP.concat, function (o) {
  var out = [], parts = [Object(o)].concat ? null : null, all = [Object(o)];
  for (var a = 1; a < arguments.length; a++) all.push(arguments[a]);
  for (var p = 0; p < all.length; p++) {
    var x = all[p];
    if (Array.isArray(x)) { for (var i = 0; i < x.length; i++) if (i in x) out[out.length] = x[i]; else out.length++; }
    else out.push(x);
  }
  return out;
});
slow(AP.toString, function (o) { var j = o.join; return typeof j === "function" ? j.call(o) : OP.toString.call(o); });

// ── Object ──
slow(Object.keys, function (_, o) { o = Object(o); var r = []; for (var k in o) if (OP.hasOwnProperty.call(o, k)) r.push(k); return r; });
slow(Object.values, function (_, o) { o = Object(o); var ks = Object.keys(o), r = []; for (var i = 0; i < ks.length; i++) r.push(o[ks[i]]); return r; });
slow(Object.entries, function (_, o) { o = Object(o); var ks = Object.keys(o), r = []; for (var i = 0; i < ks.length; i++) r.push([ks[i], o[ks[i]]]); return r; });
slow(Object.assign, function (_, t) {
  t = Object(t);
  for (var a = 2; a < arguments.length; a++) {
    var s = arguments[a]; if (s === null || s === undefined) continue; s = Object(s);
    var ks = Object.keys(s); for (var i = 0; i < ks.length; i++) t[ks[i]] = s[ks[i]];
  }
  return t;
});
slow(OP.hasOwnProperty, function (o, k) { o = Object(o); k = keyOf(k); if (Object.getOwnPropertyDescriptor(o, k) !== undefined) return true; for (var x in o) if (x === String(k)) return true; return false; });
def(OP, "toLocaleString", function () { return this.toString(); });
def(Object, "fromEntries", function (it) { var o = {}, a = iterToArray(it); for (var i = 0; i < a.length; i++) { var e = a[i]; o[e[0]] = e[1]; } return o; });
def(Object, "groupBy", function (items, f) { var o = Object.create(null), a = iterToArray(items); for (var i = 0; i < a.length; i++) { var k = f(a[i], i); if (!(k in o)) o[k] = []; o[k].push(a[i]); } return o; });
// key-taking natives met an object key (a polyfilled Symbol) or a descriptor with getters
function keyOf(k) { return (k !== null && typeof k === "object") || typeof k === "function" ? String(k) : k; }
function plainDesc(d) {
  if (d === null || typeof d !== "object") throw new TypeError("Property description must be an object");
  var r = {};
  if ("enumerable" in d) r.enumerable = !!d.enumerable;
  if ("configurable" in d) r.configurable = !!d.configurable;
  if ("value" in d) r.value = d.value;
  if ("writable" in d) r.writable = !!d.writable;
  if ("get" in d) r.get = d.get;
  if ("set" in d) r.set = d.set;
  return r;
}
slow(Object.defineProperty, function (_, o, k, d) { return Object.defineProperty(o, keyOf(k), plainDesc(d)); });
slow(Object.defineProperties, function (_, o, props) { var ks = Object.keys(Object(props)); for (var i = 0; i < ks.length; i++) Object.defineProperty(o, ks[i], plainDesc(props[ks[i]])); return o; });
slow(Object.create, function (_, p, props) { var o = Object.create(p); if (props !== undefined) Object.defineProperties(o, props); return o; });
slow(Object.getOwnPropertyDescriptor, function (_, o, k) { return Object.getOwnPropertyDescriptor(o, keyOf(k)); });
slow(OP.propertyIsEnumerable, function (o, k) { return OP.propertyIsEnumerable.call(o, keyOf(k)); });
slow(Object.hasOwn, function (_, o, k) { return Object.hasOwn(o, keyOf(k)); });

// ── String ──
function replaceWith(s, pat, rep, all) {
  s = String(s);
  if (typeof rep !== "function") {
    rep = String(rep);
    return all ? SP.replaceAll.call(s, pat, rep) : SP.replace.call(s, pat, rep);
  }
  var out = "", last = 0, m;
  if (pat instanceof RegExp) {
    var g = pat.global;
    if (all && !g) throw new TypeError("replaceAll must be called with a global RegExp");
    if (g) pat.lastIndex = 0;
    while ((m = pat.exec(s)) !== null) {
      var args = AP.slice.call(m); args.push(m.index); args.push(s); if (m.groups !== undefined) args.push(m.groups);
      out += s.slice(last, m.index) + String(rep.apply(undefined, args));
      last = m.index + m[0].length;
      if (!g) break;
      if (m[0].length === 0) pat.lastIndex = m.index + 1;
    }
  } else {
    pat = String(pat);
    var i = s.indexOf(pat);
    while (i >= 0) {
      out += s.slice(last, i) + String(rep(pat, i, s));
      last = i + pat.length;
      if (!all) break;
      i = s.indexOf(pat, pat.length ? last : last + 1);
      if (!pat.length && i > s.length) break;
    }
  }
  return out + s.slice(last);
}
slow(SP.replace, function (s, pat, rep) { return replaceWith(s, pat, rep, false); });
slow(SP.replaceAll, function (s, pat, rep) { return replaceWith(s, pat, rep, true); });
def(SP, "matchAll", function (re) {
  var s = String(this), r = re instanceof RegExp ? new RegExp(re.source, re.flags.indexOf("g") < 0 ? re.flags + "g" : re.flags) : new RegExp(re, "g");
  if (re instanceof RegExp && !re.global) throw new TypeError("matchAll must be called with a global RegExp");
  var out = [], m;
  while ((m = r.exec(s)) !== null) { out.push(m); if (m[0].length === 0) r.lastIndex++; }
  return new ArrayIterator(out, 0);
});
def(String, "raw", function (strs) { var raw = strs.raw, n = toLen(raw.length), out = ""; for (var i = 0; i < n; i++) { out += String(raw[i]); if (i + 1 < n && i + 1 < arguments.length) out += String(arguments[i + 1]); } return out; });

// ── JSON with a replacer / reviver / user toJSON ──
function jsonQuote(s) { return JSON.stringify(String(s)); }
function jsonStr(holder, key, rep, allow, gap, indent, stack) {
  var v = holder[key];
  if (v !== null && (typeof v === "object" || typeof v === "function") && typeof v.toJSON === "function") v = v.toJSON(key);
  if (typeof rep === "function") v = rep.call(holder, key, v);
  if (v === null) return "null";
  if (typeof v === "object") {
    if (v instanceof Number) v = Number(v); else if (v instanceof String) v = String(v); else if (v instanceof Boolean) v = v.valueOf();
  }
  switch (typeof v) {
    case "string": return jsonQuote(v);
    case "number": return isFinite(v) ? String(v) : "null";
    case "boolean": return String(v);
    case "object": break;
    default: return undefined;
  }
  if (stack.indexOf(v) >= 0) throw new TypeError("Converting circular structure to JSON");
  stack.push(v);
  var inner = indent + gap, parts = [], i, s;
  if (Array.isArray(v)) {
    for (i = 0; i < v.length; i++) { s = jsonStr(v, String(i), rep, allow, gap, inner, stack); parts.push(s === undefined ? "null" : s); }
    stack.pop();
    if (!parts.length) return "[]";
    return gap ? "[\n" + inner + parts.join(",\n" + inner) + "\n" + indent + "]" : "[" + parts.join(",") + "]";
  }
  var ks = allow || Object.keys(v);
  for (i = 0; i < ks.length; i++) {
    s = jsonStr(v, ks[i], rep, allow, gap, inner, stack);
    if (s !== undefined) parts.push(jsonQuote(ks[i]) + (gap ? ": " : ":") + s);
  }
  stack.pop();
  if (!parts.length) return "{}";
  return gap ? "{\n" + inner + parts.join(",\n" + inner) + "\n" + indent + "}" : "{" + parts.join(",") + "}";
}
slow(JSON.stringify, function (_, v, rep, space) {
  var allow = null, gap = "";
  if (Array.isArray(rep)) { allow = []; for (var i = 0; i < rep.length; i++) { var x = rep[i]; if (typeof x === "string" || typeof x === "number") allow.push(String(x)); } rep = undefined; }
  else if (typeof rep !== "function") rep = undefined;
  if (typeof space === "number") gap = "          ".slice(0, Math.max(0, Math.min(10, toInt(space))));
  else if (typeof space === "string") gap = space.slice(0, 10);
  return jsonStr({ "": v }, "", rep, allow, gap, "", []);
});
slow(JSON.parse, function (_, text, reviver) {
  var root = JSON.parse(text);
  if (typeof reviver !== "function") return root;
  function walk(holder, key) {
    var v = holder[key];
    if (v !== null && typeof v === "object") {
      var ks = Array.isArray(v) ? null : Object.keys(v), n = ks ? ks.length : v.length;
      for (var i = 0; i < n; i++) {
        var k = ks ? ks[i] : String(i), nv = walk(v, k);
        if (nv === undefined) delete v[k]; else v[k] = nv;
      }
    }
    return reviver.call(holder, key, v);
  }
  return walk({ "": root }, "");
});

// ── Map / Set: the parts that call back into script ──
var MapP = Map.prototype, SetP = Set.prototype;
slow(Map, function (_, src) {
  var m = new Map();
  if (src === null || src === undefined) return m;
  var it = getIterator(src), r;
  while (!(r = it.next()).done) {
    var e = r.value;
    if (e === null || (typeof e !== "object" && typeof e !== "function")) { iterClose(it); throw new TypeError("Iterator value " + String(e) + " is not an entry object"); }
    m.set(e[0], e[1]);
  }
  return m;
});
slow(Set, function (_, src) {
  var s = new Set();
  if (src === null || src === undefined) return s;
  var it = getIterator(src), r;
  while (!(r = it.next()).done) s.add(r.value);
  return s;
});
def(MapP, "forEach", function forEach(f, t) {
  callable(f, "Map.prototype.forEach");
  var it = MapP.entries.call(this), r;
  while (!(r = it.next()).done) f.call(t, r.value[1], r.value[0], this);
});
def(SetP, "forEach", function forEach(f, t) {
  callable(f, "Set.prototype.forEach");
  var it = SetP.values.call(this), r;
  while (!(r = it.next()).done) f.call(t, r.value, r.value, this);
});
def(Map, "groupBy", function groupBy(items, f) {
  var m = new Map(), a = iterToArray(items);
  for (var i = 0; i < a.length; i++) { var k = f(a[i], i), g = m.get(k); if (g === undefined) m.set(k, g = []); g.push(a[i]); }
  return m;
});
function setLike(o) {
  if (o === null || typeof o !== "object") throw new TypeError("The argument must be set-like");
  callable(o.has, "has"); callable(o.keys, "keys");
  return o;
}
function setKeys(o) { return iterToArray({ [SYM_IT]: function () { return o.keys(); } }); }
def(SetP, "union", function union(o) { setLike(o); var r = new Set(this), k = setKeys(o); for (var i = 0; i < k.length; i++) r.add(k[i]); return r; });
def(SetP, "intersection", function intersection(o) { setLike(o); var r = new Set(), self = this; this.forEach(function (v) { if (o.has(v)) r.add(v); }); return r; });
def(SetP, "difference", function difference(o) { setLike(o); var r = new Set(), self = this; this.forEach(function (v) { if (!o.has(v)) r.add(v); }); return r; });
def(SetP, "symmetricDifference", function symmetricDifference(o) {
  setLike(o); var r = new Set(this), k = setKeys(o), self = this;
  for (var i = 0; i < k.length; i++) { if (self.has(k[i])) r.delete(k[i]); else r.add(k[i]); }
  return r;
});
def(SetP, "isSubsetOf", function isSubsetOf(o) { setLike(o); var ok = true; this.forEach(function (v) { if (!o.has(v)) ok = false; }); return ok; });
def(SetP, "isSupersetOf", function isSupersetOf(o) { setLike(o); var k = setKeys(o); for (var i = 0; i < k.length; i++) if (!this.has(k[i])) return false; return true; });
def(SetP, "isDisjointFrom", function isDisjointFrom(o) { setLike(o); var ok = true; this.forEach(function (v) { if (o.has(v)) ok = false; }); return ok; });

// ── typed arrays: the methods that call back into script ──
var TA = __proto("%TypedArray"), TAP = TA.prototype;
["forEach", "some", "every", "find", "findIndex", "findLast", "findLastIndex", "reduce", "reduceRight", "indexOf", "lastIndexOf", "includes", "join", "keys", "values", "entries", "toString", "toLocaleString"].forEach(function (n) { def(TAP, n, AP[n]); });
def(TAP, SYM_IT, TAP.values);
function taSpecies(ta, n) { return new ta.constructor(n); }
def(TAP, "map", function map(f, t) { callable(f); var n = this.length, r = taSpecies(this, n); for (var i = 0; i < n; i++) r[i] = f.call(t, this[i], i, this); return r; });
def(TAP, "filter", function filter(f, t) { callable(f); var out = [], n = this.length; for (var i = 0; i < n; i++) { var v = this[i]; if (f.call(t, v, i, this)) out.push(v); } return new this.constructor(out); });
function numCompare(x, y) {
  if (x !== x) return y !== y ? 0 : 1;
  if (y !== y) return -1;
  if (x < y) return -1;
  if (x > y) return 1;
  if (x === 0 && y === 0) return 1 / x < 1 / y ? -1 : 1 / x > 1 / y ? 1 : 0;
  return 0;
}
def(TAP, "sort", function sort(cmp) {
  if (cmp !== undefined) callable(cmp, "the comparison function");
  var a = AP.slice.call(this);
  a.sort(cmp === undefined ? numCompare : cmp);
  for (var i = 0; i < a.length; i++) this[i] = a[i];
  return this;
});
def(TAP, "toSorted", function (cmp) { return this.slice().sort(cmp); });
def(TAP, "toReversed", function () { return this.slice().reverse(); });
def(TAP, "with", function (i, v) { var r = this.slice(); i = toInt(i); if (i < 0) i += r.length; if (i < 0 || i >= r.length) throw new RangeError("Invalid typed array index"); r[i] = v; return r; });
def(TA, "from", function from(src, f, t) { return new this(Array.from(src, f, t)); });
def(TA, "of", function of() { return new this(AP.slice.call(arguments)); });
["Int8Array", "Uint8Array", "Uint8ClampedArray", "Int16Array", "Uint16Array", "Int32Array", "Uint32Array", "Float32Array", "Float64Array", "BigInt64Array", "BigUint64Array"].forEach(function (name) {
  var C = global[name];
  slow(C, function (_, src) { return new C(Array.from(src)); });
});

// ── Reflect: the operations that go through getters, setters and proxy traps ──
var R = global.Reflect, isProxy = __isProxy;
function needObj(o, what) { if (o === null || (typeof o !== "object" && typeof o !== "function")) throw new TypeError("Reflect." + what + " called on non-object"); return o; }
def(R, "get", function get(o, k, receiver) {
  needObj(o, "get");
  if (receiver === undefined || receiver === o || isProxy(o)) return o[k];
  for (var p = o; p !== null; p = Object.getPrototypeOf(p)) {
    var d = Object.getOwnPropertyDescriptor(p, k);
    if (d) return d.get ? d.get.call(receiver) : d.value;
  }
  return undefined;
});
def(R, "set", function set(o, k, v, receiver) {
  needObj(o, "set");
  if (receiver !== undefined && receiver !== o && !isProxy(o)) {
    for (var p = o; p !== null; p = Object.getPrototypeOf(p)) {
      var d = Object.getOwnPropertyDescriptor(p, k);
      if (d) {
        if (d.set) { d.set.call(receiver, v); return true; }
        if (d.get || !d.writable) return false;
        break;
      }
    }
    if (receiver === null || (typeof receiver !== "object" && typeof receiver !== "function")) return false;
    var own = Object.getOwnPropertyDescriptor(receiver, k);
    try {
      if (own) { if (own.get || own.set || !own.writable) return false; Object.defineProperty(receiver, k, { value: v }); }
      else Object.defineProperty(receiver, k, { value: v, writable: true, enumerable: true, configurable: true });
      return true;
    } catch (e) { return false; }
  }
  try { o[k] = v; return true; } catch (e) { return false; }
});
def(R, "has", function has(o, k) { return k in needObj(o, "has"); });
def(R, "deleteProperty", function deleteProperty(o, k) { needObj(o, "deleteProperty"); try { return delete o[k]; } catch (e) { return false; } });
slow(Object.keys, function (_, o) {
  o = Object(o);
  var r = [];
  if (isProxy(o)) {
    var ks = R.ownKeys(o);
    for (var i = 0; i < ks.length; i++) { var k = ks[i]; if (typeof k === "string") { var d = R.getOwnPropertyDescriptor(o, k); if (d && d.enumerable) r.push(k); } }
    return r;
  }
  for (var k2 in o) if (OP.hasOwnProperty.call(o, k2)) r.push(k2);
  return r;
});

// ── Promise (the job queue is the engine's: run_jobs drains it) ──
var PSTATE = privateSymbol("PromiseState"), PVALUE = privateSymbol("PromiseResult"), PREACT = privateSymbol("PromiseReactions"), PHANDLED = privateSymbol("PromiseIsHandled");
function isPromise(x) { return x !== null && typeof x === "object" && x[PSTATE] !== undefined; }
function Promise(executor) {
  if (new.target === undefined) throw new TypeError("Promise constructor cannot be invoked without 'new'");
  if (typeof executor !== "function") throw new TypeError("Promise resolver " + String(executor) + " is not a function");
  this[PSTATE] = 0; this[PREACT] = []; this[PHANDLED] = false;
  var r = resolvingFunctions(this);
  try { executor(r.resolve, r.reject); } catch (e) { r.reject(e); }
}
var PP = Promise.prototype;
function makePromise() { var p = Object.create(PP); p[PSTATE] = 0; p[PREACT] = []; p[PHANDLED] = false; return p; }
function resolvingFunctions(p) {
  var done = false;
  return {
    resolve: function (x) { if (done) return; done = true; resolvePromise(p, x); },
    reject: function (r) { if (done) return; done = true; settle(p, 2, r); }
  };
}
function resolvePromise(p, x) {
  if (x === p) return settle(p, 2, new TypeError("Chaining cycle detected for promise"));
  if (x === null || (typeof x !== "object" && typeof x !== "function")) return settle(p, 1, x);
  var then;
  try { then = x.then; } catch (e) { return settle(p, 2, e); }
  if (typeof then !== "function") return settle(p, 1, x);
  enqueue(resolveThenableJob, p, x, then);
}
function resolveThenableJob(p, x, then) {
  var r = resolvingFunctions(p);
  try { then.call(x, r.resolve, r.reject); } catch (e) { r.reject(e); }
}
function settle(p, state, v) {
  if (p[PSTATE] !== 0) return;
  var reactions = p[PREACT];
  p[PSTATE] = state; p[PVALUE] = v; p[PREACT] = undefined;
  if (state === 2 && !p[PHANDLED]) track(p, 0, v);
  for (var i = 0; i < reactions.length; i++) enqueue(reactionJob, reactions[i], state, v);
}
// a reaction: [capability promise, its resolve, its reject, onFulfilled, onRejected]
function reactionJob(r, state, v) {
  var h = state === 1 ? r[3] : r[4], res;
  if (h === undefined) {
    if (r[1] !== undefined) { if (state === 1) r[1](v); else r[2](v); }
    return;
  }
  try { res = h(v); } catch (e) { if (r[2] !== undefined) r[2](e); return; }
  if (r[1] !== undefined) r[1](res);
}
function performThen(p, onF, onR, cap) {
  var r = [cap ? cap[0] : undefined, cap ? cap[1] : undefined, cap ? cap[2] : undefined,
    typeof onF === "function" ? onF : undefined, typeof onR === "function" ? onR : undefined];
  var s = p[PSTATE];
  if (s === 0) p[PREACT].push(r);
  else { if (s === 2 && !p[PHANDLED]) track(p, 1); enqueue(reactionJob, r, s, p[PVALUE]); }
  p[PHANDLED] = true;
}
function newCapability(C) {
  if (C === Promise) { var p = makePromise(), f = resolvingFunctions(p); return [p, f.resolve, f.reject]; }
  if (typeof C !== "function") throw new TypeError("Promise capability: not a constructor");
  var res, rej;
  var p2 = new C(function (a, b) {
    if (res !== undefined || rej !== undefined) throw new TypeError("Promise executor has already been invoked");
    res = a; rej = b;
  });
  callable(res, "Promise resolve function"); callable(rej, "Promise reject function");
  return [p2, res, rej];
}
function speciesCtor(p) {
  var C = p.constructor;
  if (C === undefined || C === Promise) return Promise;
  if (C === null || (typeof C !== "object" && typeof C !== "function")) throw new TypeError("The promise's constructor is not an object");
  var S = C[SYM_SPECIES];
  return S === undefined || S === null ? Promise : S;
}
function promiseResolve(C, x) {
  if (isPromise(x) && x.constructor === C) return x;
  var cap = newCapability(C); cap[1](x); return cap[0];
}
def(PP, "then", function then(onF, onR) {
  if (!isPromise(this)) throw new TypeError("Promise.prototype.then called on an incompatible receiver");
  var cap = newCapability(speciesCtor(this));
  performThen(this, onF, onR, cap);
  return cap[0];
});
def(PP, "catch", function (onR) { return this.then(undefined, onR); });
def(PP, "finally", function (f) {
  var p = this;
  if (p === null || typeof p !== "object") throw new TypeError("Promise.prototype.finally called on a non-object");
  var C = speciesCtor(p);
  if (typeof f !== "function") return p.then(f, f);
  return p.then(function (v) { return promiseResolve(C, f()).then(function () { return v; }); },
    function (e) { return promiseResolve(C, f()).then(function () { throw e; }); });
});
tag(PP, "Promise");
def(Promise, "resolve", function resolve(x) {
  if (this === null || (typeof this !== "object" && typeof this !== "function")) throw new TypeError("Promise.resolve called on a non-object");
  return promiseResolve(this, x);
});
def(Promise, "reject", function reject(r) { var cap = newCapability(this); cap[2](r); return cap[0]; });
def(Promise, "withResolvers", function () { var cap = newCapability(this); return { promise: cap[0], resolve: cap[1], reject: cap[2] }; });
Object.defineProperty(Promise, SYM_SPECIES, { get: function () { return this; }, configurable: true });
function allStep(C, iterable, cap, each, finish) {
  var resolveFn = callable(C.resolve, "Promise resolve"), values = [], left = { n: 1 }, i = 0, it = getIterator(iterable), r;
  while (!(r = it.next()).done) {
    values.push(undefined);
    left.n++;
    each(resolveFn.call(C, r.value), i++, values, left);
  }
  if (--left.n === 0) finish(values);
}
def(Promise, "all", function all(iterable) {
  var C = this, cap = newCapability(C);
  try {
    allStep(C, iterable, cap, function (p, i, values, left) {
      var called = false;
      p.then(function (v) { if (called) return; called = true; values[i] = v; if (--left.n === 0) cap[1](values); }, cap[2]);
    }, cap[1]);
  } catch (e) { cap[2](e); }
  return cap[0];
});
def(Promise, "allSettled", function allSettled(iterable) {
  var C = this, cap = newCapability(C);
  try {
    allStep(C, iterable, cap, function (p, i, values, left) {
      var called = false;
      p.then(function (v) { if (called) return; called = true; values[i] = { status: "fulfilled", value: v }; if (--left.n === 0) cap[1](values); },
        function (e) { if (called) return; called = true; values[i] = { status: "rejected", reason: e }; if (--left.n === 0) cap[1](values); });
    }, cap[1]);
  } catch (e) { cap[2](e); }
  return cap[0];
});
def(Promise, "any", function any(iterable) {
  var C = this, cap = newCapability(C);
  function fail(errors) { cap[2](new AggregateError(errors, "All promises were rejected")); }
  try {
    allStep(C, iterable, cap, function (p, i, errors, left) {
      var called = false;
      p.then(cap[1], function (e) { if (called) return; called = true; errors[i] = e; if (--left.n === 0) fail(errors); });
    }, fail);
  } catch (e) { cap[2](e); }
  return cap[0];
});
def(Promise, "race", function race(iterable) {
  var C = this, cap = newCapability(C);
  try {
    var resolveFn = callable(C.resolve, "Promise resolve"), it = getIterator(iterable), r;
    while (!(r = it.next()).done) resolveFn.call(C, r.value).then(cap[1], cap[2]);
  } catch (e) { cap[2](e); }
  return cap[0];
});
def(global, "Promise", Promise);
function AggregateError(errors, message, options) {
  var e = new Error(message, options);
  Object.setPrototypeOf(e, new.target ? new.target.prototype : AggregateError.prototype);
  Object.defineProperty(e, "errors", { value: Array.from(errors), writable: true, configurable: true });
  return e;
}
AggregateError.prototype = Object.create(Error.prototype);
hide(AggregateError, "prototype");
def(AggregateError.prototype, "constructor", AggregateError);
def(AggregateError.prototype, "name", "AggregateError");
def(AggregateError.prototype, "message", "");
Object.setPrototypeOf(AggregateError, Error);
def(global, "AggregateError", AggregateError);

// ── async functions: their generator, stepped from promise reactions ──
function awaitValue(v, onF, onR) {
  if (v === null || (typeof v !== "object" && typeof v !== "function")) { enqueue(onF, v); return; }
  var p = isPromise(v) && v.constructor === Promise ? v : promiseResolve(Promise, v);
  performThen(p, onF, onR, undefined);
}
intrinsic(7, function asyncRun(gen) {
  var p = makePromise(), f = resolvingFunctions(p);
  function step(mode, v) {
    var r;
    try { r = genResume(gen, mode, v); } catch (e) { f.reject(e); return; }
    if (genState(gen) === 3) f.resolve(r); else awaitValue(r, onF, onR);
  }
  function onF(v) { step(0, v); }
  function onR(e) { step(1, e); }
  step(0, undefined);
  return p;
});
// async generators: requests queue up; each runs the generator to its next yield
var AGQ = privateSymbol("AsyncGeneratorQueue"), AGRUN = privateSymbol("AsyncGeneratorRunning");
intrinsic(8, function asyncGen(gen) { gen[AGQ] = []; gen[AGRUN] = false; return gen; });
function agEnqueue(gen, mode, v) {
  var cap = newCapability(Promise);
  if (!isGenerator(gen) || gen[AGQ] === undefined) { cap[2](new TypeError("not an async generator")); return cap[0]; }
  gen[AGQ].push([mode, v, cap]);
  if (!gen[AGRUN]) agNext(gen);
  return cap[0];
}
function agNext(gen) {
  var q = gen[AGQ];
  if (!q.length) return;
  var req = q[0];
  if (genState(gen) === 3 && req[0] === 2) {
    gen[AGRUN] = true;
    awaitValue(req[1], function (v) { q.shift(); gen[AGRUN] = false; req[2][1]({ value: v, done: true }); agNext(gen); },
      function (e) { q.shift(); gen[AGRUN] = false; req[2][2](e); agNext(gen); });
    return;
  }
  gen[AGRUN] = true;
  agStep(gen, req[0], req[1]);
}
function agStep(gen, mode, v) {
  var r, q = gen[AGQ];
  try { r = genResume(gen, mode, v); } catch (e) { var bad = q.shift(); gen[AGRUN] = false; bad[2][2](e); agNext(gen); return; }
  var st = genState(gen);
  if (st === 2) { awaitValue(r, function (x) { agStep(gen, 0, x); }, function (x) { agStep(gen, 1, x); }); return; }
  var req = q.shift(); gen[AGRUN] = false;
  req[2][1]({ value: r, done: st === 3 });
  agNext(gen);
}
def(AsyncGenProto, "next", function next(v) { return agEnqueue(this, 0, v); });
def(AsyncGenProto, "throw", function (v) { return agEnqueue(this, 1, v); });
def(AsyncGenProto, "return", function (v) { return agEnqueue(this, 2, v); });
// a step of yield*: the inner iterator's next / throw / return
intrinsic(9, function yieldStep(it, mode, v) {
  var r;
  if (mode === 0) r = it.next(v);
  else if (mode === 1) {
    var t = it["throw"];
    if (t === undefined || t === null) { iterClose(it); throw new TypeError("The iterator does not provide a 'throw' method"); }
    r = t.call(it, v);
  } else {
    var rt = it["return"];
    if (rt === undefined || rt === null) return { value: v, done: true };
    r = rt.call(it, v);
  }
  if (r === null || typeof r !== "object") throw new TypeError("Iterator result " + String(r) + " is not an object");
  return r;
});
function getAsyncIterator(o) {
  if (o === null || o === undefined) throw new TypeError(String(o) + " is not async iterable");
  var f = o[SYM_AIT];
  if (f !== undefined && f !== null) return f.call(o);
  var it = getIterator(o);
  function wrap(r) { return promiseResolve(Promise, r.value).then(function (x) { return { value: x, done: !!r.done }; }); }
  return {
    next: function (v) { return wrap(it.next(v)); },
    "return": function (v) { var rf = it["return"]; return rf === undefined || rf === null ? promiseResolve(Promise, { value: v, done: true }) : wrap(rf.call(it, v)); }
  };
}
intrinsic(10, getAsyncIterator);
intrinsic(12, function (r) { if (r === null || typeof r !== "object") throw new TypeError("Iterator result " + String(r) + " is not an object"); return r; });

// ── odds and ends ──
[["anchor", "a", "name"], ["big", "big"], ["blink", "blink"], ["bold", "b"], ["fixed", "tt"], ["fontcolor", "font", "color"], ["fontsize", "font", "size"], ["italics", "i"], ["link", "a", "href"], ["small", "small"], ["strike", "strike"], ["sub", "sub"], ["sup", "sup"]].forEach(function (d) {
  def(SP, d[0], function (v) { var s = String(obj(this, "String.prototype." + d[0])); return "<" + d[1] + (d[2] ? " " + d[2] + "=\"" + String(v).replace(/"/g, "&quot;") + "\"" : "") + ">" + s + "</" + d[1] + ">"; });
});
def(Math, "sumPrecise", function (it) { var a = iterToArray(it), s = 0, c = 0; for (var i = 0; i < a.length; i++) { var y = a[i] - c, t = s + y; c = (t - s) - y; s = t; } return a.length ? s : -0; });
def(Number, "parseFloat", global.parseFloat);
def(Number, "parseInt", global.parseInt);
def(SP, "isWellFormed", function isWellFormed() {
  var s = String(obj(this, "String.prototype.isWellFormed"));
  for (var i = 0; i < s.length; i++) {
    var c = s.charCodeAt(i);
    if (c >= 0xD800 && c < 0xDC00) { var d = s.charCodeAt(i + 1); if (d >= 0xDC00 && d < 0xE000) { i++; continue; } return false; }
    if (c >= 0xDC00 && c < 0xE000) return false;
  }
  return true;
});
def(SP, "toWellFormed", function toWellFormed() {
  var s = String(obj(this, "String.prototype.toWellFormed")), out = "";
  for (var i = 0; i < s.length; i++) {
    var c = s.charCodeAt(i);
    if (c >= 0xD800 && c < 0xDC00) { var d = s.charCodeAt(i + 1); if (d >= 0xDC00 && d < 0xE000) { out += s[i] + s[i + 1]; i++; continue; } out += "�"; continue; }
    if (c >= 0xDC00 && c < 0xE000) { out += "�"; continue; }
    out += s[i];
  }
  return out;
});
def(Promise, "try", function (f) { var C = this, args = AP.slice.call(arguments, 1); return new C(function (res) { res(f.apply(undefined, args)); }); });
def(Array, "fromAsync", function fromAsync(src, f, t) {
  return (async function () {
    var out = [], i = 0;
    if (src !== null && src !== undefined && (src[SYM_AIT] !== undefined || src[SYM_IT] !== undefined)) {
      var it = getAsyncIterator(src), r;
      while (!(r = await it.next()).done) out.push(f ? await f.call(t, r.value, i++) : r.value);
    } else {
      var n = toLen(src.length);
      for (; i < n; i++) out.push(f ? await f.call(t, await src[i], i) : await src[i]);
    }
    return out;
  })();
});

// structuredClone: plain data, arrays, Date, RegExp, Map, Set, typed arrays, errors; cycles kept
function cloneValue(v, seen) {
  if (v === null || typeof v !== "object") {
    if (typeof v === "function" || typeof v === "symbol") throw new TypeError("DataCloneError: " + String(typeof v) + " could not be cloned");
    return v;
  }
  if (seen.has(v)) return seen.get(v);
  var r, i;
  if (Array.isArray(v)) { r = []; seen.set(v, r); for (i = 0; i < v.length; i++) r[i] = cloneValue(v[i], seen); return r; }
  if (v instanceof Date) { r = new Date(v.getTime()); seen.set(v, r); return r; }
  if (v instanceof RegExp) { r = new RegExp(v.source, v.flags); seen.set(v, r); return r; }
  if (v instanceof Map) { r = new Map(); seen.set(v, r); v.forEach(function (x, k) { r.set(cloneValue(k, seen), cloneValue(x, seen)); }); return r; }
  if (v instanceof Set) { r = new Set(); seen.set(v, r); v.forEach(function (x) { r.add(cloneValue(x, seen)); }); return r; }
  if (v instanceof ArrayBuffer) { r = v.slice(0); seen.set(v, r); return r; }
  if (ArrayBuffer.isView(v)) { r = v instanceof DataView ? new DataView(v.buffer.slice(0), v.byteOffset, v.byteLength) : v.slice(); seen.set(v, r); return r; }
  if (v instanceof Error) { r = new Error(v.message); r.name = v.name; seen.set(v, r); return r; }
  if (v instanceof Boolean || v instanceof Number || v instanceof String) return Object(v.valueOf());
  r = {}; seen.set(v, r);
  var ks = Object.keys(v);
  for (i = 0; i < ks.length; i++) r[ks[i]] = cloneValue(v[ks[i]], seen);
  return r;
}
def(global, "structuredClone", function structuredClone(v) { return cloneValue(v, new Map()); });

// Iterator helpers (Iterator.prototype.map / filter / take / ... and Iterator.from)
function Iterator() { if (new.target === undefined || new.target === Iterator) throw new TypeError("Iterator is an abstract class"); }
Iterator.prototype = IterProto;
hide(Iterator, "prototype");
def(IterProto, "constructor", Iterator);
tag(IterProto, "Iterator");
function IterHelper(next, src) { this._next = next; this._src = src; this._done = false; }
IterHelper.prototype = Object.create(IterProto);
tag(IterHelper.prototype, "Iterator Helper");
def(IterHelper.prototype, "next", function () {
  if (this._done) return { value: undefined, done: true };
  var r = this._next();
  if (r.done) this._done = true;
  return r;
});
def(IterHelper.prototype, "return", function () { this._done = true; iterClose(this._src); return { value: undefined, done: true }; });
function stepOf(it) { var n = it.next; return function () { return n.call(it); }; }
def(IterProto, "map", function map(f) { callable(f); var it = this, step = stepOf(it), i = 0; return new IterHelper(function () { var r = step(); return r.done ? r : { value: f(r.value, i++), done: false }; }, it); });
def(IterProto, "filter", function filter(f) { callable(f); var it = this, step = stepOf(it), i = 0; return new IterHelper(function () { for (;;) { var r = step(); if (r.done || f(r.value, i++)) return r.done ? r : { value: r.value, done: false }; } }, it); });
def(IterProto, "take", function take(n) { n = toInt(n); var it = this, step = stepOf(it); return new IterHelper(function () { if (n-- <= 0) { iterClose(it); return { value: undefined, done: true }; } return step(); }, it); });
def(IterProto, "drop", function drop(n) { n = toInt(n); var it = this, step = stepOf(it); return new IterHelper(function () { while (n > 0) { n--; var s = step(); if (s.done) return s; } return step(); }, it); });
def(IterProto, "flatMap", function flatMap(f) {
  callable(f); var it = this, step = stepOf(it), inner = null, i = 0;
  return new IterHelper(function () {
    for (;;) {
      if (inner) { var x = inner.next(); if (!x.done) return { value: x.value, done: false }; inner = null; }
      var r = step(); if (r.done) return r;
      inner = getIterator(f(r.value, i++));
    }
  }, it);
});
def(IterProto, "reduce", function reduce(f, init) { var it = this, r, acc, i = 0; if (arguments.length < 2) { r = it.next(); if (r.done) throw new TypeError("Reduce of empty iterator with no initial value"); acc = r.value; i = 1; } else acc = init; while (!(r = it.next()).done) acc = f(acc, r.value, i++); return acc; });
def(IterProto, "toArray", function toArray() { var out = [], r; while (!(r = this.next()).done) out.push(r.value); return out; });
def(IterProto, "forEach", function forEach(f) { var r, i = 0; while (!(r = this.next()).done) f(r.value, i++); });
def(IterProto, "some", function some(f) { var r, i = 0; while (!(r = this.next()).done) if (f(r.value, i++)) { iterClose(this); return true; } return false; });
def(IterProto, "every", function every(f) { var r, i = 0; while (!(r = this.next()).done) if (!f(r.value, i++)) { iterClose(this); return false; } return true; });
def(IterProto, "find", function find(f) { var r, i = 0; while (!(r = this.next()).done) if (f(r.value, i++)) { iterClose(this); return r.value; } return undefined; });
def(Iterator, "from", function from(o) {
  var it = typeof o === "string" ? new StringIterator(o) : (o !== null && typeof o === "object" && typeof o[SYM_IT] === "function") ? o[SYM_IT]() : o;
  if (it instanceof Iterator) return it;
  var step = stepOf(it);
  return new IterHelper(step, it);
});
def(global, "Iterator", Iterator);

// Intl: enough for pages that format numbers and dates (en-US conventions)
var Intl = {};
tag(Intl, "Intl");
function groupDigits(s, sep) { var out = ""; for (var i = 0; i < s.length; i++) { if (i > 0 && (s.length - i) % 3 === 0) out += sep; out += s[i]; } return out; }
function NumberFormat(locales, opts) {
  if (!(this instanceof NumberFormat)) return new NumberFormat(locales, opts);
  opts = opts || {};
  this._style = opts.style || "decimal";
  this._currency = opts.currency;
  this._group = opts.useGrouping !== false;
  this._notation = opts.notation || "standard";
  var cur = this._style === "currency";
  this._min = opts.minimumFractionDigits !== undefined ? +opts.minimumFractionDigits : cur ? 2 : 0;
  this._max = opts.maximumFractionDigits !== undefined ? Math.max(+opts.maximumFractionDigits, this._min) : cur ? Math.max(2, this._min) : this._style === "percent" ? Math.max(0, this._min) : Math.max(3, this._min);
  this._locale = typeof locales === "string" ? locales : Array.isArray(locales) && locales.length ? locales[0] : "en-US";
}
def(NumberFormat.prototype, "format", function format(n) {
  n = typeof n === "bigint" ? Number(n) : +n;
  if (this._style === "percent") n *= 100;
  if (n !== n) return "NaN";
  if (!isFinite(n)) return n < 0 ? "-∞" : "∞";
  var neg = n < 0 || (n === 0 && 1 / n < 0 && false), x = Math.abs(n), suffix = "";
  if (this._notation === "compact") {
    var units = [[1e12, "T"], [1e9, "B"], [1e6, "M"], [1e3, "K"]];
    for (var u = 0; u < units.length; u++) if (x >= units[u][0]) { x = x / units[u][0]; suffix = units[u][1]; break; }
    if (suffix) { x = Math.round(x * 10) / 10; }
  }
  var s = x.toFixed(this._max);
  if (s.indexOf("e") < 0) {
    var dot = s.indexOf("."), ip = dot < 0 ? s : s.slice(0, dot), fp = dot < 0 ? "" : s.slice(dot + 1);
    while (fp.length > this._min && fp[fp.length - 1] === "0") fp = fp.slice(0, -1);
    if (this._group) ip = groupDigits(ip, ",");
    s = fp ? ip + "." + fp : ip;
  }
  s += suffix;
  if (this._style === "percent") s += "%";
  if (this._style === "currency") { var sym = { USD: "$", EUR: "€", GBP: "£", JPY: "¥", CNY: "CN¥", INR: "₹", KRW: "₩" }[this._currency] || (this._currency + " "); s = sym + s; }
  return neg ? "-" + s : s;
});
def(NumberFormat.prototype, "formatToParts", function (n) { return [{ type: "literal", value: this.format(n) }]; });
def(NumberFormat.prototype, "resolvedOptions", function () { return { locale: this._locale, numberingSystem: "latn", style: this._style, minimumFractionDigits: this._min, maximumFractionDigits: this._max, useGrouping: this._group }; });
var MONTHS = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];
var DAYS = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
function pad2(n) { return n < 10 ? "0" + n : "" + n; }
function DateTimeFormat(locales, opts) {
  if (!(this instanceof DateTimeFormat)) return new DateTimeFormat(locales, opts);
  this._o = opts || {};
  this._locale = typeof locales === "string" ? locales : Array.isArray(locales) && locales.length ? locales[0] : "en-US";
}
def(DateTimeFormat.prototype, "format", function format(d) {
  d = d === undefined ? new Date() : new Date(+d);
  if (isNaN(d.getTime())) throw new RangeError("Invalid time value");
  var o = this._o, utc = o.timeZone === "UTC";
  var Y = utc ? d.getUTCFullYear() : d.getFullYear(), M = utc ? d.getUTCMonth() : d.getMonth(), D = utc ? d.getUTCDate() : d.getDate();
  var h = utc ? d.getUTCHours() : d.getHours(), m = utc ? d.getUTCMinutes() : d.getMinutes(), s = utc ? d.getUTCSeconds() : d.getSeconds(), wd = utc ? d.getUTCDay() : d.getDay();
  var hasDate = o.year || o.month || o.day || o.weekday || o.dateStyle, hasTime = o.hour || o.minute || o.second || o.timeStyle;
  if (!hasDate && !hasTime) { hasDate = true; o = { year: "numeric", month: "numeric", day: "numeric" }; }
  var parts = [];
  if (o.dateStyle === "full" || o.dateStyle === "long") parts.push((o.dateStyle === "full" ? DAYS[wd] + ", " : "") + MONTHS[M] + " " + D + ", " + Y);
  else if (o.dateStyle === "medium") parts.push(MONTHS[M].slice(0, 3) + " " + D + ", " + Y);
  else if (o.dateStyle === "short") parts.push((M + 1) + "/" + D + "/" + String(Y).slice(-2));
  else if (hasDate) {
    var wds = o.weekday ? (o.weekday === "long" ? DAYS[wd] : DAYS[wd].slice(0, 3)) : "";
    var ms = o.month === "long" ? MONTHS[M] : o.month === "short" ? MONTHS[M].slice(0, 3) : o.month === "2-digit" ? pad2(M + 1) : o.month ? String(M + 1) : "";
    var ds = o.day === "2-digit" ? pad2(D) : o.day ? String(D) : "", ys = o.year === "2-digit" ? String(Y).slice(-2) : o.year ? String(Y) : "";
    var txt;
    if (o.month === "long" || o.month === "short") txt = ms + (ds ? " " + ds : "") + (ys ? (ds ? ", " : " ") + ys : "");
    else txt = [ms, ds, ys].filter(function (x) { return x; }).join("/");
    parts.push(wds ? wds + (txt ? ", " + txt : "") : txt);
  }
  if (hasTime) {
    var h12 = o.hour12 !== false && o.hourCycle !== "h23", hh = h12 ? (h % 12 || 12) : h;
    var t = (o.hour === "2-digit" || !h12 ? pad2(hh) : String(hh));
    if (o.minute || o.timeStyle || !o.hour) t += ":" + pad2(m);
    if (o.second || o.timeStyle === "medium" || o.timeStyle === "long") t += ":" + pad2(s);
    if (h12) t += h < 12 ? " AM" : " PM";
    parts.push(t);
  }
  return parts.join(", ");
});
def(DateTimeFormat.prototype, "formatToParts", function (d) { return [{ type: "literal", value: this.format(d) }]; });
def(DateTimeFormat.prototype, "resolvedOptions", function () { return { locale: this._locale, calendar: "gregory", numberingSystem: "latn", timeZone: this._o.timeZone || "UTC" }; });
function Collator(locales, opts) { if (!(this instanceof Collator)) return new Collator(locales, opts); this._o = opts || {}; var self = this; this.compare = function (a, b) { a = String(a); b = String(b); if (self._o.sensitivity === "base" || self._o.sensitivity === "accent") { a = a.toLowerCase(); b = b.toLowerCase(); } if (self._o.numeric) { var na = parseFloat(a), nb = parseFloat(b); if (na === na && nb === nb && na !== nb) return na < nb ? -1 : 1; } return a.localeCompare(b); }; }
def(Collator.prototype, "resolvedOptions", function () { return { locale: "en-US", sensitivity: this._o.sensitivity || "variant" }; });
function PluralRules(locales, opts) { if (!(this instanceof PluralRules)) throw new TypeError("Constructor Intl.PluralRules requires 'new'"); this._o = opts || {}; }
def(PluralRules.prototype, "select", function (n) { n = +n; if (this._o.type === "ordinal") { var t = n % 10, h = n % 100; return t === 1 && h !== 11 ? "one" : t === 2 && h !== 12 ? "two" : t === 3 && h !== 13 ? "few" : "other"; } return n === 1 ? "one" : "other"; });
function RelativeTimeFormat(locales, opts) { if (!(this instanceof RelativeTimeFormat)) throw new TypeError("Constructor Intl.RelativeTimeFormat requires 'new'"); this._o = opts || {}; }
def(RelativeTimeFormat.prototype, "format", function (v, unit) { unit = String(unit).replace(/s$/, ""); v = +v; var a = Math.abs(v), u = unit + (a === 1 ? "" : "s"); return v < 0 ? a + " " + u + " ago" : "in " + a + " " + u; });
function ListFormat(locales, opts) { if (!(this instanceof ListFormat)) throw new TypeError("Constructor Intl.ListFormat requires 'new'"); this._o = opts || {}; }
def(ListFormat.prototype, "format", function (list) { var a = Array.from(list).map(String), w = this._o.type === "disjunction" ? "or" : "and"; if (a.length < 2) return a.join(""); if (a.length === 2) return a[0] + " " + w + " " + a[1]; return a.slice(0, -1).join(", ") + ", " + w + " " + a[a.length - 1]; });
function Segmenter(locales, opts) { if (!(this instanceof Segmenter)) throw new TypeError("Constructor Intl.Segmenter requires 'new'"); this._o = opts || {}; }
def(Segmenter.prototype, "segment", function (s) { s = String(s); var out = [], g = this._o.granularity || "grapheme", i = 0; if (g === "word") { var re = /\w+|\W/g, m; while ((m = re.exec(s)) !== null) out.push({ segment: m[0], index: m.index, input: s, isWordLike: /\w/.test(m[0]) }); } else { var arr = Array.from(s); for (var k = 0; k < arr.length; k++) { out.push({ segment: arr[k], index: i, input: s }); i += arr[k].length; } } return out; });
[["NumberFormat", NumberFormat], ["DateTimeFormat", DateTimeFormat], ["Collator", Collator], ["PluralRules", PluralRules], ["RelativeTimeFormat", RelativeTimeFormat], ["ListFormat", ListFormat], ["Segmenter", Segmenter]].forEach(function (p) {
  def(Intl, p[0], p[1]);
  def(p[1], "supportedLocalesOf", function (l) { return typeof l === "string" ? [l] : Array.isArray(l) ? l.slice() : []; });
});
def(Intl, "getCanonicalLocales", function (l) { return typeof l === "string" ? [l] : Array.isArray(l) ? l.slice() : []; });
def(Intl, "DisplayNames", function DisplayNames() { this.of = function (c) { return c; }; });
def(global, "Intl", Intl);
var numToLocale = Number.prototype.toLocaleString;
def(Number.prototype, "toLocaleString", function toLocaleString(locales, opts) {
  if (opts === undefined) return numToLocale.call(this);
  return new NumberFormat(locales, opts).format(Number(this));
});
def(Date.prototype, "toLocaleDateString", (function (orig) { return function (l, o) { return o === undefined ? orig.call(this) : new DateTimeFormat(l, o).format(this); }; })(Date.prototype.toLocaleDateString));

// ── ES modules: load (asking the host for sources), link, evaluate ──
var fetchModule = __fetchModule, compileModule = __compileModule, baseUrl = __baseUrl;
var moduleMap = new Map();
var SCHEME = /^[a-zA-Z][a-zA-Z0-9+.\-]*:/;
function resolveSpecifier(spec, base) {
  spec = String(spec);
  if (SCHEME.test(spec)) return spec;
  var rel = spec.charAt(0) === "/" || spec.slice(0, 2) === "./" || spec.slice(0, 3) === "../";
  if (!rel) throw new TypeError("Failed to resolve module specifier \"" + spec + "\": relative references must start with \"/\", \"./\" or \"../\"");
  if (!base || base.slice(0, 5) === "data:" || !SCHEME.test(base)) throw new TypeError("Cannot resolve \"" + spec + "\" against " + (base || "no base URL"));
  var m = /^([a-zA-Z][a-zA-Z0-9+.\-]*:)(\/\/[^\/?#]*)?([^?#]*)/.exec(base);
  var scheme = m[1], auth = m[2] || "", path = m[3] || "/";
  if (spec.slice(0, 2) === "//") return scheme + spec;
  var q = spec.search(/[?#]/), tail = q >= 0 ? spec.slice(q) : "", sp = q >= 0 ? spec.slice(0, q) : spec;
  var segs = sp.charAt(0) === "/" ? [] : path.split("/").slice(0, -1);
  if (sp.charAt(0) === "/") segs = [""];
  var parts = sp.split("/");
  for (var i = 0; i < parts.length; i++) {
    var p = parts[i];
    if (p === "." ) continue;
    if (p === "..") { if (segs.length > 1) segs.pop(); continue; }
    if (p === "" && i > 0 && i < parts.length - 1) continue;
    segs.push(p);
  }
  var out = segs.join("/");
  if (out.charAt(0) !== "/") out = "/" + out;
  return scheme + auth + out + tail;
}
function moduleSource(url) {
  if (url.slice(0, 5) === "data:") {
    var comma = url.indexOf(",");
    if (comma < 0) throw new TypeError("Invalid data: URL");
    var head = url.slice(5, comma), body = url.slice(comma + 1);
    if (/;base64$/i.test(head)) {
      var bin = atob(decodeURIComponent(body)), bytes = new Uint8Array(bin.length);
      for (var i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
      return new TextDecoder().decode(bytes);
    }
    return decodeURIComponent(body);
  }
  var src = fetchModule(url);
  if (typeof src !== "string") throw new TypeError("Failed to fetch module " + url);
  return src;
}
function defineExport(ns, name, getter) {
  Object.defineProperty(ns, name, { get: getter, set: undefined, enumerable: true, configurable: false });
}
function newNamespace() { var ns = Object.create(null); Object.defineProperty(ns, SYM_TAG, { value: "Module" }); return ns; }
// fetch + compile a module and (depth first) everything it imports
var inlineCount = 0;
function loadModule(url, src) {
  var key = url;
  if (src !== undefined) {
    // module code the embedder gives (an inline <script type=module>): its own module, even
    // when another has the same URL; an import of that URL gets the first one
    if (moduleMap.has(url)) key = url + "#inline-" + (++inlineCount);
  } else {
    var known = moduleMap.get(url);
    if (known !== undefined) return known;
    src = moduleSource(url);
  }
  var c = compileModule(src);
  var rec = { url: url, fn: c[0], requests: c[1], stars: c[2], deps: [], ns: newNamespace(), meta: { url: url, resolve: function (s) { return resolveSpecifier(s, url); } }, gen: undefined, status: 0, error: undefined, promise: undefined };
  moduleMap.set(key, rec);
  try {
    for (var i = 0; i < rec.requests.length; i++) rec.deps.push(loadModule(resolveSpecifier(rec.requests[i], url)));
  } catch (e) { moduleMap.delete(key); throw e; }
  return rec;
}
// instantiate: run each module up to its `yield` (its export getters exist), then `export *`
function linkModule(rec, order) {
  if (rec.gen !== undefined) return;
  var args = [rec.ns, rec.meta, defineExport];
  for (var i = 0; i < rec.deps.length; i++) args.push(rec.deps[i].ns);
  rec.gen = rec.fn.apply(undefined, args);
  genResume(rec.gen, 0, undefined);
  for (var j = 0; j < rec.deps.length; j++) linkModule(rec.deps[j], order);
  order.push(rec);
}
function linkStars(order) {
  var changed = true;
  while (changed) {
    changed = false;
    for (var i = 0; i < order.length; i++) {
      var rec = order[i];
      for (var j = 0; j < rec.stars.length; j++) {
        var src = rec.deps[rec.stars[j]].ns, ks = Object.keys(src);
        for (var k = 0; k < ks.length; k++) {
          var key = ks[k];
          if (key === "default" || Object.prototype.hasOwnProperty.call(rec.ns, key)) continue;
          (function (s, n) { defineExport(rec.ns, n, function () { return s[n]; }); })(src, key);
          changed = true;
        }
      }
    }
  }
}
// run the body; a promise when it awaited (top-level await)
function runModuleBody(rec) {
  var r = genResume(rec.gen, 0, undefined);
  if (genState(rec.gen) === 3) return undefined;
  return new Promise(function (res, rej) {
    function step(mode, v) {
      try { r = genResume(rec.gen, mode, v); } catch (e) { rec.error = e; rej(e); return; }
      if (genState(rec.gen) === 3) res(); else awaitValue(r, onF, onR);
    }
    function onF(v) { step(0, v); }
    function onR(e) { step(1, e); }
    awaitValue(r, onF, onR);
  });
}
// evaluate dependencies first (a cycle sees a module that is still running); sync unless awaiting
function evaluateModule(rec) {
  if (rec.status === 2) { if (rec.error !== undefined) throw rec.error; return rec.promise; }
  if (rec.status === 1) return undefined;
  rec.status = 1;
  var pending = [];
  try {
    for (var i = 0; i < rec.deps.length; i++) { var p = evaluateModule(rec.deps[i]); if (p !== undefined) pending.push(p); }
    if (pending.length) rec.promise = Promise.all(pending).then(function () { return runModuleBody(rec); });
    else rec.promise = runModuleBody(rec);
  } catch (e) { rec.status = 2; rec.error = e; throw e; }
  rec.status = 2;
  return rec.promise;
}
function importModule(url, src) {
  var rec = loadModule(url, src), order = [];
  linkModule(rec, order);
  linkStars(order);
  return { rec: rec, done: evaluateModule(rec) };
}
intrinsic(13, function moduleEval(src, url) { var r = importModule(String(url), String(src)); return r.done; });
intrinsic(11, function dynamicImport(spec, referrer) {
  return new Promise(function (res, rej) {
    var r;
    try { r = importModule(resolveSpecifier(spec, referrer === undefined ? baseUrl() : referrer)); }
    catch (e) { rej(e); return; }
    if (r.done === undefined) res(r.rec.ns); else r.done.then(function () { res(r.rec.ns); }, rej);
  });
});

// ── Function / misc ──
delete global.__slow;
delete global.__intrinsic;
delete global.__hide;
delete global.__isArrayLike;
delete global.__privateSymbol;
delete global.__proto;
delete global.__enqueueJob;
delete global.__trackRejection;
delete global.__genResume;
delete global.__genState;
delete global.__isGenerator;
delete global.__isProxy;
delete global.__fetchModule;
delete global.__compileModule;
delete global.__baseUrl;
})(this);
