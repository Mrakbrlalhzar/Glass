//! Render the Frida JS that traces a single Java method.
//!
//! The script we ship to the gadget does three things:
//!   1. `Java.perform(() => { … })` — ensures we're inside the
//!      ART runtime context. Without this any `Java.use` call
//!      throws "Java API not yet available."
//!   2. Resolves the right overload of the target method via
//!      `.overload(arg1, arg2, …)`. We need the overload form
//!      whenever the class has multiple methods with the same
//!      name — calling `.method` directly throws in that case.
//!   3. Replaces the implementation with a wrapper that calls
//!      `send({kind:"call",args:[…]})` on entry, runs the
//!      original, then `send({kind:"return",value:…})` on exit.
//!
//! Arg / return values are rendered host-side via `safeRepr`
//! (a try/catch around `.toString()` for objects, primitives
//! pass through). Heavy serialization is out of scope for this
//! first cut — we just want "what was called, what came back."

/// Convert a JNI method signature like `(Ljava/lang/String;I[B)V`
/// into the list of Java type names Frida's overload picker
/// expects: `["java.lang.String", "int", "[B"]`.
///
/// We intentionally keep array types in their JNI form (`[B`,
/// `[Ljava/lang/String;`) — Frida accepts both notations for
/// arrays, and the JNI form is unambiguous about array
/// dimensionality.
pub fn jni_params_to_java(signature: &str) -> Result<Vec<String>, JsRenderError> {
    let inner = signature
        .strip_prefix('(')
        .and_then(|s| s.split(')').next())
        .ok_or(JsRenderError::BadSignature)?;
    let mut out = Vec::new();
    let bytes = inner.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let start = i;
        // Count leading `[` for arrays.
        while i < bytes.len() && bytes[i] == b'[' {
            i += 1;
        }
        if i >= bytes.len() {
            return Err(JsRenderError::BadSignature);
        }
        match bytes[i] {
            b'L' => {
                // Object — `L<class>;`. Class name uses `/`
                // separators in JNI; Java's overload picker
                // wants `.` separators.
                let semi = inner[i..]
                    .find(';')
                    .ok_or(JsRenderError::BadSignature)?;
                let end = i + semi + 1;
                if start < i {
                    // Array of objects — keep the `[…]L…;`
                    // form unchanged; Frida understands it.
                    out.push(inner[start..end].to_string());
                } else {
                    let class = &inner[i + 1..end - 1];
                    out.push(class.replace('/', "."));
                }
                i = end;
            }
            _c if start < i => {
                // Array of primitive — keep JNI form
                // (`[B`, `[I`, etc.). Frida accepts that
                // notation; converting to `int[]` etc isn't
                // worth the complexity.
                out.push(inner[start..i + 1].to_string());
                i += 1;
            }
            c => {
                out.push(
                    primitive_jni_to_java(c)
                        .ok_or(JsRenderError::BadSignature)?
                        .to_string(),
                );
                i += 1;
            }
        }
    }
    Ok(out)
}

/// Extract the return-type token from a JNI method signature.
/// `(I)Ljava/lang/String;` → `"java.lang.String"`, `()V` →
/// `"void"`, `([B)V` → `"void"`, `()[B` → `"[B"`. Returns
/// `None` only when the signature can't be parsed.
pub fn jni_return_type_to_java(signature: &str) -> Option<String> {
    let rest = signature.rsplit(')').next()?;
    let mut depth = 0;
    let bytes = rest.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i] == b'[' {
        depth += 1;
        i += 1;
    }
    if i >= bytes.len() {
        return None;
    }
    let leading = "[".repeat(depth);
    match bytes[i] {
        b'L' => {
            let semi = rest[i..].find(';')?;
            let class = &rest[i + 1..i + semi];
            if depth == 0 {
                Some(class.replace('/', "."))
            } else {
                Some(format!("{leading}L{class};"))
            }
        }
        c if depth > 0 => {
            // Primitive-array return — keep `[B` etc form.
            Some(format!("{leading}{}", c as char))
        }
        c => primitive_jni_to_java(c).map(|s| s.to_string()),
    }
}

fn primitive_jni_to_java(c: u8) -> Option<&'static str> {
    match c {
        b'Z' => Some("boolean"),
        b'B' => Some("byte"),
        b'S' => Some("short"),
        b'C' => Some("char"),
        b'I' => Some("int"),
        b'J' => Some("long"),
        b'F' => Some("float"),
        b'D' => Some("double"),
        b'V' => Some("void"),
        _ => None,
    }
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum JsRenderError {
    #[error("malformed JNI method signature")]
    BadSignature,
}

/// Render the full trace script for `class.method` at the
/// given signature. The result is ready to feed to
/// [`crate::Session::create_script`].
pub fn render_trace_script(
    class_jni: &str,
    method_name: &str,
    method_signature: &str,
) -> Result<String, JsRenderError> {
    let params = jni_params_to_java(method_signature)?;
    // Convert JNI class form `Lcom/example/Foo;` → dotted
    // `com.example.Foo` for `Java.use`.
    let dotted_class = class_jni
        .strip_prefix('L')
        .and_then(|s| s.strip_suffix(';'))
        .map(|s| s.replace('/', "."))
        .ok_or(JsRenderError::BadSignature)?;
    // Frida exposes constructors as `$init`. `<clinit>` is
    // unreachable from `Java.use`; the caller should refuse
    // to trace static initialisers earlier.
    let frida_method = match method_name {
        "<init>" => "$init".to_string(),
        "<clinit>" => return Err(JsRenderError::BadSignature),
        other => other.to_string(),
    };
    // Build the JS literal array of overload type names.
    let overload_args = params
        .iter()
        .map(|p| format!("\"{p}\""))
        .collect::<Vec<_>>()
        .join(", ");
    // Also embed the param + return type list so the runtime
    // repr formatter can label each arg with its declared
    // type (e.g. `Intent=…`, `byte[]=…`).
    let param_types_lit = format!(
        "[{}]",
        params
            .iter()
            .map(|p| format!("\"{p}\""))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let return_type_lit = format!(
        "\"{}\"",
        jni_return_type_to_java(method_signature).unwrap_or_default()
    );
    // Heredoc-style: the JS is small enough to embed
    // verbatim. We keep it on one line per logical statement
    // so any runtime error's line/column points clearly.
    // Gate on `Java.available` — on early-attach or
    // non-Android contexts the global `Java` may be undefined,
    // and `Java.perform` will throw "'Java' is not defined."
    // We do a typeof check, then poll briefly so traces set up
    // shortly after process spawn still come up cleanly.
    let inner = format!(
        r#"(function () {{
  // Render-cap. Strings, arrays, and stringified objects
  // longer than this get truncated with a "…(+N more)"
  // suffix. Keeps the dock log readable when a method
  // takes a chunky JSON payload.
  var MAX_LEN = 200;
  // Bytes per byte-array shown before truncating. Each
  // byte renders as two hex chars, so 32 bytes = 64 chars
  // — leaves room for the type prefix.
  var MAX_HEX = 32;

  function truncate(s) {{
    if (typeof s !== "string") s = String(s);
    if (s.length <= MAX_LEN) return s;
    return s.slice(0, MAX_LEN) + "…(+" + (s.length - MAX_LEN) + " more)";
  }}

  function hexBytes(arr) {{
    var n = Math.min(arr.length, MAX_HEX);
    var out = "";
    for (var i = 0; i < n; i++) {{
      var b = arr[i] & 0xff;
      if (b < 0x10) out += "0";
      out += b.toString(16);
    }}
    if (arr.length > MAX_HEX) out += "…(+" + (arr.length - MAX_HEX) + " more)";
    return out;
  }}

  function reprPrimitive(v) {{
    if (v === null) return "null";
    if (v === undefined) return "undefined";
    var t = typeof v;
    if (t === "number" || t === "boolean") return String(v);
    if (t === "string") return JSON.stringify(truncate(v));
    return null;
  }}

  // One-level walk of a Java object's declared fields.
  //
  // DANGEROUS — reflecting on a Java object from inside our
  // wrapper triggers JNI calls, lazy class loading, and
  // synchronization. For methods called from native code at
  // high frequency (touch / gesture / render callbacks)
  // this can deadlock or crash the host app.
  //
  // Disabled by default. Switched on per-trace later via a
  // user toggle; the conservative default formats objects
  // as bare identifiers so tracing a touch callback is
  // safe by construction.
  var ENABLE_REFLECT = false;
  function shallowFields(obj) {{
    if (!ENABLE_REFLECT) return [];
    var pairs = [];
    try {{
      var cls = obj.getClass ? obj.getClass() : null;
      if (cls && cls.getDeclaredFields) {{
        var fs = cls.getDeclaredFields();
        var n = Math.min(fs.length, 8);
        for (var i = 0; i < n; i++) {{
          var f = fs[i];
          try {{
            f.setAccessible(true);
            var name = f.getName();
            var val = f.get(obj);
            pairs.push(name + "=" + truncate(String(val)));
          }} catch (_) {{ /* private / non-accessible */ }}
        }}
        if (fs.length > 8) pairs.push("…(+" + (fs.length - 8) + " fields)");
      }}
    }} catch (_) {{ /* not a wrapped Java object */ }}
    return pairs;
  }}

  function reprWithType(v, declaredType) {{
    var p = reprPrimitive(v);
    if (p !== null) return declaredType ? declaredType + "=" + p : p;
    // Byte arrays — hex.
    if (declaredType === "[B") {{
      try {{ return "byte[" + v.length + "]=" + hexBytes(v); }}
      catch (_) {{}}
    }}
    // String/Object arrays — comma-join the elements after a
    // light per-element repr (no recursion into objects).
    if (declaredType && declaredType.charAt(0) === "[") {{
      try {{
        var parts = [];
        var max = Math.min(v.length, 8);
        for (var i = 0; i < max; i++) {{
          var el = v[i];
          var elRepr = reprPrimitive(el);
          if (elRepr === null) {{
            try {{ elRepr = String(el); }} catch (_) {{ elRepr = "?"; }}
          }}
          parts.push(elRepr);
        }}
        if (v.length > max) parts.push("…(+" + (v.length - max) + " more)");
        return declaredType + "{{" + truncate(parts.join(", ")) + "}}";
      }} catch (_) {{}}
    }}
    // Generic object — toString + a shallow field walk.
    var head = "";
    try {{ head = String(v); }} catch (e) {{ head = "[unrepresentable]"; }}
    var fields = shallowFields(v);
    var body = fields.length ? "{{" + fields.join(", ") + "}}" : "";
    var prefix = declaredType || "Object";
    return prefix + "=" + truncate(head + body);
  }}

  function describeOverloads(slot) {{
    // Frida exposes each overload's argument types on
    // `.overloads[i].argumentTypes`. Listing them helps the
    // user spot a signature mismatch (e.g. they asked for
    // `(Ljava/lang/String;)V` but the real method is
    // `(Ljava/lang/CharSequence;)V`).
    try {{
      var lines = [];
      var os = slot.overloads || [];
      for (var i = 0; i < os.length; i++) {{
        var types = (os[i].argumentTypes || [])
          .map(function (t) {{ return t.className || t.name || String(t); }})
          .join(", ");
        lines.push("(" + types + ")");
      }}
      return lines.join(" | ");
    }} catch (_) {{
      return "(unable to enumerate)";
    }}
  }}

  // Retry Java.use for up to 10s — the target class may
  // not be loaded by ART yet on first attach. Once the
  // class is resolvable, attach the implementation.
  var classRetries = 0;
  function setup() {{
    Java.perform(function () {{
      var target;
      try {{
        target = Java.use({class_lit});
      }} catch (e) {{
        classRetries++;
        if (classRetries < 200) {{
          setTimeout(setup, 50);
          return;
        }}
        send({{
          kind: "setup-error",
          error: "class not loaded after 10s: " + {class_lit} +
                 " — open the screen that uses it, then re-hook. " +
                 "Raw: " + String(e)
        }});
        return;
      }}
      var slot = target[{method_lit}];
      if (!slot) {{
        send({{
          kind: "setup-error",
          error: "method " + {method_lit} + " not found on " + {class_lit}
        }});
        return;
      }}
      var impl;
      try {{
        impl = slot.overload({overload_args});
      }} catch (e) {{
        send({{
          kind: "setup-error",
          error: "overload failed: " + String(e) +
                 " — available overloads: " + describeOverloads(slot)
        }});
        return;
      }}

      var paramTypes = {param_types_lit};
      var returnType = {return_type_lit};

      impl.implementation = function () {{
        // Defensive: if our own JS throws while formatting
        // args we must NOT let it bubble — that crashes
        // the app on the thread the method was called on.
        // Catch + emit a wrapper-error event instead.
        try {{
          var args = [];
          for (var i = 0; i < arguments.length; i++) {{
            args.push(reprWithType(arguments[i], paramTypes[i] || ""));
          }}
          send({{ kind: "call", args: args }});
        }} catch (wrapErr) {{
          send({{ kind: "wrapper-error", phase: "format-args",
                  error: String(wrapErr) }});
        }}
        var ret;
        try {{
          ret = impl.apply(this, arguments);
        }} catch (e) {{
          send({{ kind: "throw", error: String(e) }});
          throw e;
        }}
        try {{
          send({{ kind: "return", value: reprWithType(ret, returnType) }});
        }} catch (wrapErr) {{
          send({{ kind: "wrapper-error", phase: "format-return",
                  error: String(wrapErr) }});
        }}
        return ret;
      }};

      send({{ kind: "ready" }});
    }});
  }}

  // Wait for Frida's Java bridge to come online. On the
  // gadget this can take several seconds depending on the
  // host app's startup path. Poll for 30s before giving up,
  // emit a one-line diagnostic up front so the user can
  // tell whether `Java` exists at all vs `Java.available`
  // staying false.
  send({{
    kind: "setup-info",
    typeofJava: (typeof Java),
    available: (typeof Java !== "undefined" ? !!Java.available : false)
  }});
  var attempts = 0;
  function waitForJava() {{
    if (typeof Java !== "undefined" && Java.available) {{
      setup();
      return;
    }}
    attempts++;
    if (attempts > 600) {{
      send({{
        kind: "setup-error",
        error: "Java bridge never became available — typeof Java=" + (typeof Java)
                + ", Java.available=" + (typeof Java !== "undefined" ? !!Java.available : "n/a")
      }});
      return;
    }}
    setTimeout(waitForJava, 50);
  }}
  waitForJava();
}})();
"#,
        class_lit = json_string(&dotted_class),
        method_lit = json_string(&frida_method),
    );
    // Splice the wrapper IIFE into the frida-java-bridge
    // bundle's entry.js position so the gadget loads the
    // bridge first, then our wrapper.
    Ok(build_bridged_script(&inner))
}

fn json_string(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string())
}

/// Bundled frida-java-bridge in frida-compile's `📦` format.
/// We don't ship this verbatim — instead, [`build_bridged_script`]
/// substitutes the bundle's `entry.js` content for our own
/// wrapper code so the gadget runs the Java bridge import
/// followed by our trace/hook logic as a single bundle.
pub const JAVA_BRIDGE_BUNDLE: &str =
    include_str!("../assets/frida-java-bridge.js");

/// Splice `user_body` into the bundled frida-java-bridge as
/// its `entry.js`. The result is a complete frida-compile
/// bundle that:
///   1. Initialises frida-java-bridge (sets `globalThis.Java`).
///   2. Runs the user's wrapper script.
///
/// We have to do this at the bundle level because
/// `create_script_sync` parses the whole input through the
/// frida-compile envelope — trailing raw JS after the bundle
/// is silently dropped. Patching the entry's body keeps
/// everything inside one bundle so all of it runs.
///
/// The patch is purely textual: locate the entry.js manifest
/// line, find its `✄`-separated body, replace the body, and
/// rewrite the manifest's byte count. The other modules and
/// re-exports stay untouched.
pub fn build_bridged_script(user_body: &str) -> String {
    let bundle = JAVA_BRIDGE_BUNDLE;
    // We want to:
    //   1. Find the manifest line for /entry.js.
    //   2. Skip past the manifest (everything up to + including
    //      the first `\n✄\n`).
    //   3. Replace the first module body (everything between
    //      the first `\n✄\n` and the second `\n✄\n` or EOF) with
    //      our prelude that imports Java + runs user_body.
    //   4. Rewrite the manifest line with the new byte count.
    //
    // The prelude does the same imports the original entry
    // did (sets globalThis.Java), then runs user_body in the
    // module scope so it can reference Java directly.
    let prelude_template =
        "import a from\"frida-java-bridge\";globalThis.Java=a;\n";
    let new_entry = format!("{prelude_template}{user_body}");
    let new_entry_bytes = new_entry.len();

    // Find the entry.js manifest line and update it.
    // Format: "<bytes> /entry.js"
    let manifest_marker = " /entry.js\n";
    let Some(line_end) = bundle.find(manifest_marker) else {
        // Bundle layout has changed — fall back to raw user
        // body. Will fail at runtime with a useful error in
        // the dock log (Java still undefined) but won't
        // panic.
        return user_body.to_string();
    };
    // Walk backwards from line_end to find the start of the
    // byte count.
    let line_start = bundle[..line_end]
        .rfind('\n')
        .map(|i| i + 1)
        .unwrap_or(0);
    let new_manifest_line =
        format!("{new_entry_bytes} /entry.js");

    // Find the bundle's first `\n✄\n` — separator before the
    // first module body.
    let Some(sep1) = bundle.find("\n✄\n") else {
        return user_body.to_string();
    };
    let body_start = sep1 + "\n✄\n".len();
    // Find the next `\n✄\n` — end of the entry.js body.
    let Some(sep2_rel) = bundle[body_start..].find("\n✄\n") else {
        return user_body.to_string();
    };
    let body_end = body_start + sep2_rel;

    // Reassemble: header up to the manifest line,
    // new manifest line, header after, `\n✄\n`,
    // new entry body, rest of bundle.
    let mut out = String::with_capacity(bundle.len() + new_entry_bytes);
    out.push_str(&bundle[..line_start]);
    out.push_str(&new_manifest_line);
    out.push_str(&bundle[line_end..sep1]);
    out.push_str("\n✄\n");
    out.push_str(&new_entry);
    // Skip the original body, splice from the trailing sep
    // (we keep the `\n✄\n` so the next module starts cleanly).
    out.push_str(&bundle[body_end..]);
    out
}

#[cfg(test)]
mod bridge_tests {
    use super::*;

    #[test]
    fn bridged_script_contains_user_body() {
        let user = "send({kind:'info',stage:'user-body-ran'});";
        let bundled = build_bridged_script(user);
        assert!(bundled.starts_with("📦"));
        assert!(bundled.contains("user-body-ran"));
        assert!(bundled.contains("frida-java-bridge"));
    }

    #[test]
    fn bridged_script_rewrites_manifest_count() {
        // Run with two different-length user bodies; the
        // emitted manifest count for entry.js should differ.
        let a = build_bridged_script("send({});");
        let b = build_bridged_script("send({a:1,b:2,c:3,d:4,e:5});");
        // Extract the count line for /entry.js out of each.
        // Format: "<n> /entry.js". Split on space to grab the
        // count.
        let count_of = |s: &str| -> usize {
            let m = " /entry.js\n";
            // `find` returns the start of the match (the
            // space). `s[..i]` therefore includes the digit
            // run before the space; rfind('\n') finds the
            // newline before the digits. The slice between
            // those is the bytes count we want, plus
            // potentially the path because the search may
            // pick up the entry.js *body* later in the
            // bundle where "/entry.js" appears in source
            // map paths. Use the first whitespace-separated
            // token, which is always the digits.
            let i = s.find(m).unwrap();
            let line_start = s[..i].rfind('\n').map(|j| j + 1).unwrap_or(0);
            s[line_start..i]
                .split_whitespace()
                .next()
                .unwrap()
                .parse::<usize>()
                .unwrap()
        };
        assert!(count_of(&a) < count_of(&b));
    }
}

/// What the hook should do when the method fires. Same shape
/// as the host-side enum but kept here so glass-frida is
/// self-contained.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookBody {
    /// Pass-through: call the original and return what it
    /// returned. The wrapper still emits `call`/`return`
    /// events so the user sees the invocation in the log.
    LogOnly,
    /// Skip the original; evaluate `literal` as a JS
    /// expression and return that. `literal` is embedded
    /// verbatim into the script — e.g. `true`, `42`, `"abc"`.
    ReturnLiteral(String),
    /// User-supplied JS body. Receives `args` (array) and
    /// `originalImpl` (the wrapped method); should return the
    /// value the caller sees. `this` is bound to the
    /// instance. `send(...)` and `Java.*` are in scope.
    Custom(String),
}

/// Render the hook script. Mirrors [`render_trace_script`]
/// but the body inside `implementation = function() {...}`
/// is determined by `body`. The host-side wiring is
/// identical (one Script, one ScriptId, same message types).
pub fn render_hook_script(
    class_jni: &str,
    method_name: &str,
    method_signature: &str,
    body: &HookBody,
) -> Result<String, JsRenderError> {
    let params = jni_params_to_java(method_signature)?;
    let dotted_class = class_jni
        .strip_prefix('L')
        .and_then(|s| s.strip_suffix(';'))
        .map(|s| s.replace('/', "."))
        .ok_or(JsRenderError::BadSignature)?;
    let frida_method = match method_name {
        "<init>" => "$init".to_string(),
        "<clinit>" => return Err(JsRenderError::BadSignature),
        other => other.to_string(),
    };
    let overload_args = params
        .iter()
        .map(|p| format!("\"{p}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let param_types_lit = format!(
        "[{}]",
        params
            .iter()
            .map(|p| format!("\"{p}\""))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let return_type_lit = format!(
        "\"{}\"",
        jni_return_type_to_java(method_signature).unwrap_or_default()
    );
    // Render the inside-of-implementation body.
    let body_js: String = match body {
        HookBody::LogOnly => {
            // Mirrors the trace template — call original, send
            // both events, return its value. Kept distinct so
            // the user can later flip the action without
            // touching the wrapper structure.
            r#"send({ kind: "call", args: callArgs });
        var ret;
        try {
          ret = originalImpl.apply(this, args);
        } catch (e) {
          send({ kind: "throw", error: String(e) }); throw e;
        }
        send({ kind: "return", value: reprWithType(ret, returnType) });
        return ret;"#
                .to_string()
        }
        HookBody::ReturnLiteral(lit) => {
            // Don't call the original. Send a `call` event so
            // the user sees the override fire. Eval the
            // literal so things like `"hi"` or `[1,2,3]` work
            // without the user thinking about quoting in the
            // wrapper.
            format!(
                r#"send({{ kind: "call", args: callArgs }});
        var ret = ({lit});
        send({{ kind: "return", value: reprWithType(ret, returnType), overridden: true }});
        return ret;"#,
                lit = lit
            )
        }
        HookBody::Custom(user_body) => {
            // Wrap the user body so it sees the same locals
            // the LogOnly path does (args, originalImpl,
            // callArgs, reprWithType). The body must return
            // a value; that's what the caller sees.
            format!(
                r#"send({{ kind: "call", args: callArgs }});
        var ret;
        try {{
          ret = (function () {{
{user_body}
          }}).call(this);
        }} catch (e) {{
          send({{ kind: "throw", error: String(e) }}); throw e;
        }}
        send({{ kind: "return", value: reprWithType(ret, returnType), overridden: true }});
        return ret;"#,
                user_body = user_body
            )
        }
    };

    let inner = format!(
        r#"(function () {{
  var MAX_LEN = 200;
  var MAX_HEX = 32;
  function truncate(s) {{
    if (typeof s !== "string") s = String(s);
    if (s.length <= MAX_LEN) return s;
    return s.slice(0, MAX_LEN) + "…(+" + (s.length - MAX_LEN) + " more)";
  }}
  function hexBytes(arr) {{
    var n = Math.min(arr.length, MAX_HEX);
    var out = "";
    for (var i = 0; i < n; i++) {{
      var b = arr[i] & 0xff;
      if (b < 0x10) out += "0";
      out += b.toString(16);
    }}
    if (arr.length > MAX_HEX) out += "…(+" + (arr.length - MAX_HEX) + " more)";
    return out;
  }}
  function reprPrimitive(v) {{
    if (v === null) return "null";
    if (v === undefined) return "undefined";
    var t = typeof v;
    if (t === "number" || t === "boolean") return String(v);
    if (t === "string") return JSON.stringify(truncate(v));
    return null;
  }}
  // Reflection disabled by default — see notes on the trace
  // template's shallowFields. Hooks can be called from
  // native UI callbacks at high frequency; reflecting on
  // their args is genuinely dangerous.
  var ENABLE_REFLECT = false;
  function shallowFields(obj) {{
    if (!ENABLE_REFLECT) return [];
    var pairs = [];
    try {{
      var cls = obj.getClass ? obj.getClass() : null;
      if (cls && cls.getDeclaredFields) {{
        var fs = cls.getDeclaredFields();
        var n = Math.min(fs.length, 8);
        for (var i = 0; i < n; i++) {{
          var f = fs[i];
          try {{
            f.setAccessible(true);
            pairs.push(f.getName() + "=" + truncate(String(f.get(obj))));
          }} catch (_) {{}}
        }}
        if (fs.length > 8) pairs.push("…(+" + (fs.length - 8) + " fields)");
      }}
    }} catch (_) {{}}
    return pairs;
  }}
  function reprWithType(v, declaredType) {{
    var p = reprPrimitive(v);
    if (p !== null) return declaredType ? declaredType + "=" + p : p;
    if (declaredType === "[B") {{
      try {{ return "byte[" + v.length + "]=" + hexBytes(v); }} catch (_) {{}}
    }}
    if (declaredType && declaredType.charAt(0) === "[") {{
      try {{
        var parts = []; var max = Math.min(v.length, 8);
        for (var i = 0; i < max; i++) {{
          var el = v[i];
          var er = reprPrimitive(el);
          if (er === null) {{
            try {{ er = String(el); }} catch (_) {{ er = "?"; }}
          }}
          parts.push(er);
        }}
        if (v.length > max) parts.push("…(+" + (v.length - max) + " more)");
        return declaredType + "{{" + truncate(parts.join(", ")) + "}}";
      }} catch (_) {{}}
    }}
    var head = "";
    try {{ head = String(v); }} catch (e) {{ head = "[unrepresentable]"; }}
    var fields = shallowFields(v);
    var body = fields.length ? "{{" + fields.join(", ") + "}}" : "";
    return (declaredType || "Object") + "=" + truncate(head + body);
  }}

  function describeOverloads(slot) {{
    try {{
      var lines = [];
      var os = slot.overloads || [];
      for (var i = 0; i < os.length; i++) {{
        var types = (os[i].argumentTypes || [])
          .map(function (t) {{ return t.className || t.name || String(t); }})
          .join(", ");
        lines.push("(" + types + ")");
      }}
      return lines.join(" | ");
    }} catch (_) {{
      return "(unable to enumerate)";
    }}
  }}

  var classRetries = 0;
  function setup() {{
    Java.perform(function () {{
      var target;
      try {{
        target = Java.use({class_lit});
      }} catch (e) {{
        classRetries++;
        if (classRetries < 200) {{
          setTimeout(setup, 50); return;
        }}
        send({{
          kind: "setup-error",
          error: "class not loaded after 10s: " + {class_lit} +
                 " — open the screen that uses it, then re-hook. " +
                 "Raw: " + String(e)
        }});
        return;
      }}
      var slot = target[{method_lit}];
      if (!slot) {{
        send({{
          kind: "setup-error",
          error: "method " + {method_lit} + " not found on " + {class_lit}
        }});
        return;
      }}
      var originalImpl;
      try {{
        originalImpl = slot.overload({overload_args});
      }} catch (e) {{
        send({{
          kind: "setup-error",
          error: "overload failed: " + String(e) +
                 " — available overloads: " + describeOverloads(slot)
        }});
        return;
      }}
      var paramTypes = {param_types_lit};
      var returnType = {return_type_lit};
      originalImpl.implementation = function () {{
        var args = [];
        var callArgs = [];
        try {{
          for (var i = 0; i < arguments.length; i++) {{
            args.push(arguments[i]);
            callArgs.push(reprWithType(arguments[i], paramTypes[i] || ""));
          }}
        }} catch (wrapErr) {{
          send({{ kind: "wrapper-error", phase: "format-args",
                  error: String(wrapErr) }});
        }}
        {body_js}
      }};
      send({{ kind: "ready" }});
    }});
  }}
  send({{
    kind: "setup-info",
    typeofJava: (typeof Java),
    available: (typeof Java !== "undefined" ? !!Java.available : false)
  }});
  var attempts = 0;
  function waitForJava() {{
    if (typeof Java !== "undefined" && Java.available) {{
      setup(); return;
    }}
    attempts++;
    if (attempts > 600) {{
      send({{
        kind: "setup-error",
        error: "Java bridge never became available — typeof Java=" + (typeof Java)
                + ", Java.available=" + (typeof Java !== "undefined" ? !!Java.available : "n/a")
      }}); return;
    }}
    setTimeout(waitForJava, 50);
  }}
  waitForJava();
}})();
"#,
        class_lit = json_string(&dotted_class),
        method_lit = json_string(&frida_method),
    );
    // Same bundling trick as render_trace_script — the hook
    // wrapper IIFE goes in the bundle's entry.js slot so the
    // frida-java-bridge initialises before our code runs.
    Ok(build_bridged_script(&inner))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jni_params_simple() {
        let p = jni_params_to_java("(Ljava/lang/String;I)V").unwrap();
        assert_eq!(p, vec!["java.lang.String", "int"]);
    }

    #[test]
    fn jni_params_no_args() {
        assert_eq!(jni_params_to_java("()V").unwrap(), Vec::<String>::new());
    }

    #[test]
    fn jni_params_array_of_primitive() {
        let p = jni_params_to_java("([B)V").unwrap();
        assert_eq!(p, vec!["[B"]);
    }

    #[test]
    fn jni_params_array_of_object() {
        let p = jni_params_to_java("([Ljava/lang/String;)V").unwrap();
        assert_eq!(p, vec!["[Ljava/lang/String;"]);
    }

    #[test]
    fn jni_params_all_primitives() {
        let p = jni_params_to_java("(ZBSCIJFD)V").unwrap();
        assert_eq!(
            p,
            vec!["boolean", "byte", "short", "char", "int", "long", "float", "double"]
        );
    }

    #[test]
    fn render_trace_script_contains_pieces() {
        let js = render_trace_script(
            "Lcom/example/Foo;",
            "bar",
            "(Ljava/lang/String;)V",
        )
        .unwrap();
        assert!(js.contains("Java.use(\"com.example.Foo\")"));
        assert!(js.contains("[\"bar\"]") || js.contains("\"bar\""));
        assert!(js.contains(".overload(\"java.lang.String\")"));
        // Type-aware repr — emits `reprWithType` calls and
        // injects the per-param + return type arrays.
        assert!(js.contains("reprWithType"));
        assert!(js.contains("shallowFields"));
        assert!(js.contains("[\"java.lang.String\"]"));
        assert!(js.contains("kind: \"ready\""));
        // Guard against the "Java not defined" error that
        // bit us in dev — the script must wait for the
        // Java namespace before touching it.
        assert!(js.contains("typeof Java"));
        assert!(js.contains("Java.available"));
    }

    #[test]
    fn return_type_void() {
        assert_eq!(jni_return_type_to_java("()V"), Some("void".to_string()));
    }

    #[test]
    fn return_type_object() {
        assert_eq!(
            jni_return_type_to_java("()Ljava/lang/String;"),
            Some("java.lang.String".to_string())
        );
    }

    #[test]
    fn return_type_byte_array() {
        assert_eq!(jni_return_type_to_java("()[B"), Some("[B".to_string()));
    }

    #[test]
    fn hook_log_only_calls_original() {
        let js = render_hook_script(
            "Lcom/example/Foo;",
            "bar",
            "()V",
            &HookBody::LogOnly,
        )
        .unwrap();
        assert!(js.contains("originalImpl.apply(this, args)"));
        assert!(!js.contains("overridden: true"));
    }

    #[test]
    fn hook_return_literal_skips_original() {
        let js = render_hook_script(
            "Lcom/example/Foo;",
            "isPremium",
            "()Z",
            &HookBody::ReturnLiteral("true".into()),
        )
        .unwrap();
        assert!(js.contains("var ret = (true);"));
        assert!(js.contains("overridden: true"));
        assert!(!js.contains("originalImpl.apply(this, args)"));
    }

    #[test]
    fn hook_custom_js_embeds_body() {
        let js = render_hook_script(
            "Lcom/example/Foo;",
            "encode",
            "(Ljava/lang/String;)Ljava/lang/String;",
            &HookBody::Custom("return \"intercepted\";".into()),
        )
        .unwrap();
        assert!(js.contains("return \"intercepted\""));
        assert!(js.contains("overridden: true"));
    }

    #[test]
    fn return_type_2d_object_array() {
        assert_eq!(
            jni_return_type_to_java("()[[Ljava/lang/String;"),
            Some("[[Ljava/lang/String;".to_string())
        );
    }

    #[test]
    fn render_constructor_uses_dollar_init() {
        let js = render_trace_script("Lcom/example/Foo;", "<init>", "()V").unwrap();
        assert!(js.contains("\"$init\""));
    }

    #[test]
    fn render_static_initialiser_refused() {
        assert!(
            render_trace_script("Lcom/example/Foo;", "<clinit>", "()V").is_err()
        );
    }
}
