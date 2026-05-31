//! CP10: page instrumentation for `browser_console_messages` /
//! `browser_network_requests`. Injected at document-start (like the design-mode
//! script) so it captures from page load. Wraps `console.*` + error events and
//! `fetch`/`XMLHttpRequest` transparently (always calls the originals) and
//! buffers into page-side ring buffers (`window.__zedConsole` /
//! `window.__zedNetwork`). The tools read these back via `Runtime.evaluate`.
//!
//! Coverage (documented divergences from Playwright's CDP-backed tools):
//! - Console: `console.{log,info,warn,error,debug}` + `error` /
//!   `unhandledrejection` events. Not browser-internal messages (CSP, blocked
//!   requests the browser logs itself).
//! - Network: `fetch` + `XMLHttpRequest` (status + timing). Not document /
//!   image / script subresource loads.

/// The instrumentation script. Idempotent (guarded by `__zedInstrumented`).
pub const SCRIPT: &str = r#"(function(){
  if (window.__zedInstrumented) return;
  window.__zedInstrumented = true;
  var MAX = 500;
  window.__zedConsole = [];
  window.__zedNetwork = [];
  var seq = 0;
  function pushC(level, args){
    try {
      var text = Array.prototype.map.call(args, function(a){
        try { return (typeof a === 'string') ? a : JSON.stringify(a); } catch(e){ return String(a); }
      }).join(' ');
      window.__zedConsole.push({ level: level, text: text, ts: Date.now() });
      if (window.__zedConsole.length > MAX) window.__zedConsole.shift();
    } catch(e){}
  }
  ['log','info','warn','error','debug'].forEach(function(m){
    var orig = console[m] ? console[m].bind(console) : function(){};
    console[m] = function(){ pushC(m, arguments); return orig.apply(console, arguments); };
  });
  window.addEventListener('error', function(e){
    var where = (e && e.filename) ? (' (' + e.filename + ':' + e.lineno + ')') : '';
    pushC('error', [ ((e && e.message) ? e.message : 'error') + where ]);
  });
  window.addEventListener('unhandledrejection', function(e){
    var r = (e && e.reason) ? (e.reason.message || e.reason) : '';
    pushC('error', [ 'Unhandled promise rejection: ' + r ]);
  });
  function pushN(rec){
    rec.id = ++seq;
    window.__zedNetwork.push(rec);
    if (window.__zedNetwork.length > MAX) window.__zedNetwork.shift();
    return rec;
  }
  if (window.fetch){
    var of = window.fetch.bind(window);
    window.fetch = function(input, init){
      var url = (typeof input === 'string') ? input : (input && input.url) || '';
      var method = (init && init.method) || (input && input.method) || 'GET';
      var t0 = Date.now();
      var rec = pushN({ url: url, method: method, status: null, ok: null, ms: null, type: 'fetch', error: null });
      return of(input, init).then(function(resp){
        rec.status = resp.status; rec.ok = resp.ok; rec.ms = Date.now() - t0; return resp;
      }, function(err){
        rec.error = String(err); rec.ms = Date.now() - t0; throw err;
      });
    };
  }
  var OX = window.XMLHttpRequest;
  if (OX){
    var op = OX.prototype.open, sn = OX.prototype.send;
    OX.prototype.open = function(m, u){ this.__zm = m; this.__zu = u; return op.apply(this, arguments); };
    OX.prototype.send = function(){
      var self = this, t0 = Date.now();
      var rec = pushN({ url: self.__zu || '', method: self.__zm || 'GET', status: null, ok: null, ms: null, type: 'xhr', error: null });
      self.addEventListener('loadend', function(){
        rec.status = self.status; rec.ok = (self.status >= 200 && self.status < 400); rec.ms = Date.now() - t0;
      });
      return sn.apply(self, arguments);
    };
  }
})();"#;
