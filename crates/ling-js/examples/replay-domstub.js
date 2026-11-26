// a do-nothing page for replaying captured scripts: every DOM thing is an inert stand-in
(function (g) {
  var NULLS = ["parentNode", "parentElement", "ownerDocument", "host", "nextSibling", "previousSibling", "firstChild", "lastChild", "frameElement", "defaultView", "offsetParent", "nextElementSibling", "previousElementSibling", "firstElementChild", "lastElementChild"];
  function any(name) {
    var f = function () { return any(name); };
    return new Proxy(f, {
      get: function (t, k) {
        if (k === Symbol.toPrimitive) return function () { return ""; };
        if (k === Symbol.iterator) return function () { return [][Symbol.iterator](); };
        if (k === "then") return undefined;
        if (k === "length") return 0;
        if (typeof k === "symbol") return undefined;
        if (NULLS.indexOf(k) >= 0) return null;
        if (!(k in t)) t[k] = any(String(k));
        return t[k];
      },
      set: function (t, k, v) { t[k] = v; return true; },
      construct: function () { return any(name); }
    });
  }
  g.document = any("document");
  g.navigator = { userAgent: "LingJS replay", language: "en-US", languages: ["en-US"], sendBeacon: function () { return true; } };
  g.location = { href: "https://www.reddit.com/", origin: "https://www.reddit.com", protocol: "https:", host: "www.reddit.com", hostname: "www.reddit.com", pathname: "/", search: "", hash: "" };
  ["addEventListener", "removeEventListener", "dispatchEvent", "setTimeout", "clearTimeout", "setInterval", "clearInterval", "requestAnimationFrame", "cancelAnimationFrame", "requestIdleCallback", "matchMedia", "getComputedStyle", "postMessage", "scrollTo"].forEach(function (n) { g[n] = function () { return any(n + "()"); }; });
  g.fetch = function () { return new Promise(function () {}); };
  class EventTarget { addEventListener() {} removeEventListener() {} dispatchEvent() { return true; } }
  class Node extends EventTarget {}
  class Element extends Node { setAttribute() {} getAttribute() { return null; } hasAttribute() { return false; } removeAttribute() {} attachShadow() { return any("shadowRoot"); } querySelector() { return null; } querySelectorAll() { return []; } }
  class HTMLElement extends Element {}
  g.EventTarget = EventTarget; g.Node = Node; g.Element = Element; g.HTMLElement = HTMLElement;
  ["HTMLDivElement", "HTMLAnchorElement", "HTMLInputElement", "HTMLButtonElement", "HTMLFormElement", "HTMLImageElement", "HTMLScriptElement", "HTMLIFrameElement", "HTMLVideoElement", "HTMLMediaElement", "SVGElement", "Document", "DocumentFragment", "ShadowRoot", "Text", "Comment", "CharacterData"].forEach(function (n) { g[n] = class extends HTMLElement {}; });
  class Event { constructor(type, init) { this.type = type; Object.assign(this, init || {}); } preventDefault() {} stopPropagation() {} }
  g.Event = Event; g.CustomEvent = class extends Event {}; g.MessageEvent = class extends Event {};
  g.customElements = { define: function () {}, get: function () { return undefined; }, whenDefined: function () { return new Promise(function () {}); } };
  ["MutationObserver", "IntersectionObserver", "ResizeObserver", "PerformanceObserver", "BroadcastChannel", "MessageChannel", "AbortController", "Headers", "Request", "Response", "FormData", "Blob", "File", "Image", "XMLHttpRequest", "WebSocket", "Worker", "DOMParser", "CSSStyleSheet"].forEach(function (n) { g[n] = class { constructor() { return any("new " + n); } }; });
  g.URLSearchParams = class { constructor(s) { this._s = String(s || ""); } get() { return null; } has() { return false; } set() {} append() {} toString() { return this._s; } };
  g.URL = class {
    constructor(u, base) {
      var s = String(u);
      if (!/^[a-z][a-z0-9+.-]*:/i.test(s)) { var b = String(base || "https://www.reddit.com/"); var m = /^([a-z]+:\/\/[^\/]+)(\/[^?#]*)?/i.exec(b); s = s.charAt(0) === "/" ? m[1] + s : m[1] + (m[2] || "/").replace(/[^\/]*$/, "") + s.replace(/^\.\//, ""); }
      this.href = s; var mm = /^([a-z]+:)\/\/([^\/?#]*)([^?#]*)(\?[^#]*)?(#.*)?/i.exec(s) || [];
      this.protocol = mm[1] || ""; this.host = this.hostname = mm[2] || ""; this.pathname = mm[3] || "/"; this.search = mm[4] || ""; this.hash = mm[5] || ""; this.origin = this.protocol + "//" + this.host;
      this.searchParams = new URLSearchParams(this.search);
    }
    toString() { return this.href; }
  };
  g.localStorage = g.sessionStorage = { getItem: function () { return null; }, setItem: function () {}, removeItem: function () {} };
  g.performance = { now: function () { return 0; }, mark: function () {}, measure: function () {}, getEntriesByType: function () { return []; }, timeOrigin: 0 };
  g.crypto = { getRandomValues: function (a) { return a; }, randomUUID: function () { return "00000000-0000-4000-8000-000000000000"; } };
})(globalThis);
