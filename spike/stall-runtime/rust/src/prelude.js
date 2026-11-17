// Stall realm prelude: the Node-flavoured globals the pinned sourcefed providers
// use, plus globalThis.__stall.http(url, headers) — synchronous, because it blocks
// the realm thread on the broker, and returning a Response-like the providers read.
"use strict";

(function () {
  var config = JSON.parse(globalThis.__stall_cfg);
  var faults = config.faults || [];
  var env = config.env || {};

  function stallLog(level, message) { __stallLog(String(level), String(message)); }
  function joined(args) { var parts = []; for (var i = 0; i < args.length; i += 1) parts.push(String(args[i])); return parts.join(" "); }

  globalThis.console = {
    log: function () { stallLog("info", joined(arguments)); },
    info: function () { stallLog("info", joined(arguments)); },
    warn: function () { stallLog("warn", joined(arguments)); },
    error: function () { stallLog("error", joined(arguments)); },
  };
  globalThis.process = { env: env };

  // Timers never fire: a poll must not depend on an event loop the realm does not run.
  var nextTimer = 0;
  globalThis.setTimeout = function () { nextTimer += 1; return nextTimer; };
  globalThis.clearTimeout = function () {}; globalThis.queueMicrotask = function (callback) { callback(); };

  function encodePair(value) { return encodeURIComponent(String(value)).replace(/%20/g, "+"); }
  function decodePair(value) { return decodeURIComponent(String(value).replace(/\+/g, " ")); }
  function pairIndex(pairs, key) { for (var i = 0; i < pairs.length; i += 1) if (pairs[i][0] === key) return i; return -1; }

  function initialPairs(init) {
    var pairs = [], index;
    if (init === undefined || init === null) return pairs;
    if (typeof init === "string") {
      var parts = init.replace(/^\?/, "").split("&");
      for (index = 0; index < parts.length; index += 1) {
        if (parts[index] === "") continue;
        var at = parts[index].indexOf("=");
        pairs.push([decodePair(at < 0 ? parts[index] : parts[index].slice(0, at)), at < 0 ? "" : decodePair(parts[index].slice(at + 1))]);
      }
      return pairs;
    }
    if (typeof init.entries === "function") {
      var entries = init.entries();
      for (index = 0; index < entries.length; index += 1) pairs.push([String(entries[index][0]), String(entries[index][1])]);
      return pairs;
    }
    var names = Object.keys(init);
    for (index = 0; index < names.length; index += 1) pairs.push([names[index], String(init[names[index]])]);
    return pairs;
  }

  function URLSearchParams(init) { this.pairs = initialPairs(init); }
  URLSearchParams.prototype.append = function (name, value) { this.pairs.push([String(name), String(value)]); };
  URLSearchParams.prototype.set = function (name, value) {
    var key = String(name), at = pairIndex(this.pairs, key), index;
    if (at < 0) { this.pairs.push([key, String(value)]); return; }
    this.pairs[at] = [key, String(value)];
    for (index = this.pairs.length - 1; index > at; index -= 1) if (this.pairs[index][0] === key) this.pairs.splice(index, 1);
  };
  URLSearchParams.prototype.get = function (name) { var at = pairIndex(this.pairs, String(name)); return at < 0 ? null : this.pairs[at][1]; };
  URLSearchParams.prototype.has = function (name) { return pairIndex(this.pairs, String(name)) >= 0; };
  URLSearchParams.prototype.entries = function () { return this.pairs.slice(); };
  URLSearchParams.prototype.toString = function () {
    var parts = [];
    for (var i = 0; i < this.pairs.length; i += 1) parts.push(encodePair(this.pairs[i][0]) + "=" + encodePair(this.pairs[i][1]));
    return parts.join("&");
  };
  globalThis.URLSearchParams = URLSearchParams;

  var URL_PATTERN = /^([a-zA-Z][a-zA-Z0-9+.-]*:)\/\/([^/?#]*)([^?#]*)(\?[^#]*)?(#.*)?$/;
  function URL(input) {
    var raw = String(input), match = URL_PATTERN.exec(raw);
    if (!match) throw new TypeError("Invalid URL: " + raw);
    this.href = raw; this.protocol = match[1]; this.host = match[2];
    this.pathname = match[3] || "/"; this.search = match[4] || ""; this.hash = match[5] || "";
    this.searchParams = new URLSearchParams(this.search);
  }
  URL.prototype.toString = function () { return this.href; };
  URL.parse = function (input) { try { return new URL(input); } catch (error) { return null; } }; globalThis.URL = URL;

  function plainSignal() { return { aborted: false, reason: undefined, addEventListener: function () {}, removeEventListener: function () {} }; }
  globalThis.AbortSignal = { timeout: plainSignal, aborted: false };

  function utf8Decode(bytes) {
    if (bytes && typeof bytes.byteLength === "number" && typeof bytes.length !== "number") bytes = new Uint8Array(bytes);
    var text = "", index = 0, length = bytes ? bytes.length : 0;
    while (index < length) {
      var first = bytes[index] & 255;
      var extra = first < 0x80 ? 0 : first < 0xe0 ? 1 : first < 0xf0 ? 2 : 3;
      var code = first & (first < 0x80 ? 0x7f : 0x3f >> extra);
      index += 1;
      for (var step = 0; step < extra; step += 1) code = (code << 6) | (bytes[index++] & 63);
      if (code > 0xffff) { code -= 0x10000; text += String.fromCharCode(0xd800 + (code >> 10), 0xdc00 + (code & 0x3ff)); }
      else text += String.fromCharCode(code);
    }
    return text;
  }

  function utf8Encode(input) {
    var text = String(input), bytes = [];
    for (var index = 0; index < text.length; index += 1) {
      var code = text.charCodeAt(index);
      if (code >= 0xd800 && code <= 0xdbff && index + 1 < text.length) {
        var low = text.charCodeAt(index + 1);
        if (low >= 0xdc00 && low <= 0xdfff) { code = 0x10000 + ((code - 0xd800) << 10) + (low - 0xdc00); index += 1; }
      }
      if (code < 0x80) bytes.push(code);
      else if (code < 0x800) bytes.push(0xc0 | (code >> 6), 0x80 | (code & 63));
      else if (code < 0x10000) bytes.push(0xe0 | (code >> 12), 0x80 | ((code >> 6) & 63), 0x80 | (code & 63));
      else bytes.push(0xf0 | (code >> 18), 0x80 | ((code >> 12) & 63), 0x80 | ((code >> 6) & 63), 0x80 | (code & 63));
    }
    return new Uint8Array(bytes);
  }
  globalThis.TextDecoder = function () { this.decode = function (bytes) { return utf8Decode(bytes); }; };
  globalThis.TextEncoder = function () { this.encode = function (input) { return utf8Encode(input); }; };

  var BASE64 = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
  globalThis.btoa = function (input) {
    var text = String(input), output = "";
    for (var index = 0; index < text.length; index += 3) {
      var first = text.charCodeAt(index) & 255;
      var second = index + 1 < text.length ? text.charCodeAt(index + 1) & 255 : undefined;
      var third = index + 2 < text.length ? text.charCodeAt(index + 2) & 255 : undefined;
      var tripled = (first << 16) | ((second === undefined ? 0 : second) << 8) | (third === undefined ? 0 : third);
      output += BASE64.charAt((tripled >> 18) & 63) + BASE64.charAt((tripled >> 12) & 63);
      output += second === undefined ? "=" : BASE64.charAt((tripled >> 6) & 63);
      output += third === undefined ? "=" : BASE64.charAt(tripled & 63);
    }
    return output;
  };
  function headerBag(raw) {
    var values = {}, headers = raw || {}, names = Object.keys(headers);
    for (var index = 0; index < names.length; index += 1) {
      var value = headers[names[index]];
      values[String(names[index]).toLowerCase()] = Array.isArray(value) ? value.join(", ") : String(value);
    }
    return values;
  }
  function makeResponse(reply) {
    var status = Number(reply && reply.status);
    var body = reply && typeof reply.body === "string" ? reply.body : "";
    var values = headerBag(reply && reply.headers);
    var parsed, hasParsed = false;
    return {
      status: status,
      ok: status >= 200 && status < 300,
      headers: { get: function (name) { var key = String(name).toLowerCase(); return Object.prototype.hasOwnProperty.call(values, key) ? values[key] : null; } },
      text: function () { return Promise.resolve(body); },
      json: function () {
        return Promise.resolve().then(function () { if (!hasParsed) { parsed = JSON.parse(body); hasParsed = true; } return parsed; });
      },
      clone: function () { return makeResponse(reply); },
    };
  }
  // Faults apply before the native call: "throw" rejects the request the way a
  // transport error would, "loop" runs bytecode only the interrupt handler stops.
  function applyFaults(url) {
    for (var index = 0; index < faults.length; index += 1) {
      var fault = faults[index];
      if (url.indexOf(fault.match) === -1) continue;
      if (fault.mode === "throw") { stallLog("warn", "injected fault: throwing for " + url); throw new Error("injected fault"); }
      if (fault.mode === "loop") { for (;;) {} }
    }
  }

  // The pinned provider request functions only ever GET.
  function http(url, headers) {
    var target = String(url);
    applyFaults(target);
    var request = { method: "GET", url: target, headers: {} };
    if (headers) {
      var names = Object.keys(headers);
      for (var index = 0; index < names.length; index += 1) request.headers[String(names[index])] = String(headers[names[index]]);
    }
    var reply = JSON.parse(__stallHostHttp(JSON.stringify(request)));
    if (!reply.ok) throw new Error(reply.error || "http request failed");
    return makeResponse(reply.response);
  }

  globalThis.__stall = {
    plugin: config.plugin, faults: faults, http: http,
    log: function (message) { stallLog("info", String(message)); },
  };

  // Always answers with a JSON envelope, so a throwing provider never crosses the
  // boundary as an exception (an interrupt still does and poisons the realm).
  globalThis.__stallInvokePoll = async function (inputJson) {
    try {
      var result = await globalThis.__stall.pluginPoll(JSON.parse(inputJson));
      return JSON.stringify({ ok: true, result: result });
    } catch (error) {
      var message = error && error.message ? String(error.message) : String(error);
      return JSON.stringify({ ok: false, error: message });
    }
  };
})();
