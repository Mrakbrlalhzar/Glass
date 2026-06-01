//! JS templates for Stalker-based instrumentation.
//!
//! These build pure-JS scripts (no Java bridge) that the
//! gadget runs to drive Frida's Stalker code-tracer. The
//! script does in-process aggregation — host doesn't see
//! per-block events — so a few seconds of coverage on a
//! busy thread stays well inside frida-core's `send` limits.

use std::fmt::Write as _;

/// Render a basic-block coverage script.
///
/// * `tid`         — thread to follow. `None` means "the thread
///   the script runs on", which is the right default for a
///   spawned-paused-then-resumed app (frida runs scripts on
///   the main thread).
/// * `modules`     — whitelist of module names. Blocks outside
///   these modules are still counted by Stalker but discarded
///   in the JS-side filter so the host never sees them. Empty
///   list ⇒ keep everything (useful for tiny processes; almost
///   never what you want on Android).
/// * `duration_ms` — how long to follow before stopping and
///   flushing the table back.
///
/// The script `send`s exactly one message: `{ kind:
/// "stalker-coverage", tid, rows: [{module, offset, hits}] }`.
/// `offset` is the byte offset from the module's runtime base
/// — which equals the file vaddr for ET_DYN binaries (every
/// .so), so it can be fed straight into `bundle.symbol_at`.
pub fn render_coverage_script(
    tid: Option<u32>,
    modules: &[String],
    duration_ms: u64,
) -> String {
    let modules_js = render_string_array(modules);
    let tid_js = match tid {
        Some(t) => t.to_string(),
        None => "null".to_string(),
    };
    let mut s = String::with_capacity(2048);
    let _ = writeln!(s, "(function () {{");
    let _ = writeln!(s, "  const WANT = {modules_js};");
    let _ = writeln!(s, "  const DURATION = {duration_ms};");
    let _ = writeln!(s, "  const TID_ARG = {tid_js};");
    s.push_str(COVERAGE_BODY);
    let _ = writeln!(s, "}})();");
    s
}

/// JSON-array-of-strings literal, safe to splice into JS.
fn render_string_array(names: &[String]) -> String {
    // serde handles escaping; the result is valid JS too.
    serde_json::to_string(names).unwrap_or_else(|_| "[]".to_string())
}

const COVERAGE_BODY: &str = r#"
  const want = (name) => WANT.length === 0 || WANT.indexOf(name) !== -1;

  // One range entry per kept module; checked in a tight loop
  // on every block so we keep it flat instead of building a
  // map. With 50–100 modules this is plenty fast.
  const ranges = [];
  Process.enumerateModules().forEach(function (m) {
    if (want(m.name)) {
      ranges.push({ name: m.name, base: m.base, end: m.base.add(m.size) });
    }
  });

  // In-process hit table. Keying on a string ("modIdx:offset")
  // is much faster than nested objects under V8 and uses less
  // memory than a Map keyed on NativePointer.
  const hits = new Map();

  // Resolve threads to follow.
  //
  //   * Explicit `tid` argument: follow exactly that.
  //   * Otherwise: follow the target's main thread
  //     (Linux/Android: main TID == pid). Following every
  //     thread we enumerate instruments frida-core's own
  //     worker pool and the kernel's binder threads —
  //     instrumenting everything blocks the script's event
  //     loop so `setTimeout` never fires.
  const myTid = Process.getCurrentThreadId();
  let followTids;
  if (TID_ARG !== null) {
    followTids = [TID_ARG];
  } else {
    const mainTid = Process.id;
    followTids = (mainTid !== myTid) ? [mainTid] : [];
  }

  const transform = function (iterator) {
    const first = iterator.next();
    if (first !== null) {
      const addr = first.address;
      for (let i = 0; i < ranges.length; i++) {
        const r = ranges[i];
        if (addr.compare(r.base) >= 0 && addr.compare(r.end) < 0) {
          const key = i + ':' + addr.sub(r.base).toString(16);
          hits.set(key, (hits.get(key) || 0) + 1);
          break;
        }
      }
      iterator.keep();
    }
    let inst;
    while ((inst = iterator.next()) !== null) {
      iterator.keep();
    }
  };

  const followOpts = { events: { compile: true }, transform: transform };
  const followErrors = [];
  const followedActually = [];
  followTids.forEach(function (t) {
    try {
      Stalker.follow(t, followOpts);
      followedActually.push(t);
    } catch (e) {
      followErrors.push({ tid: t, error: String(e) });
    }
  });
  // Tell the host immediately what we managed to follow so a
  // hang during the recording window doesn't look identical
  // to a configuration error.
  send({
    kind: 'stalker-coverage-init',
    requested_tids: followTids,
    followed_tids: followedActually,
    errors: followErrors,
    modules: ranges.map(function (r) { return r.name; }),
  });

  setTimeout(function () {
    followTids.forEach(function (t) {
      try { Stalker.unfollow(t); } catch (e) {}
    });
    try { Stalker.flush(); } catch (e) {}
    // Also stop the GC retaining instrumented blocks — calls
    // back through `unfollow` need this to release resources
    // before the script unloads, otherwise frida-core's
    // teardown blocks waiting for Stalker to quiesce.
    try { Stalker.garbageCollect(); } catch (e) {}
    const rows = [];
    hits.forEach(function (count, key) {
      const sep = key.indexOf(':');
      const modIdx = parseInt(key.substring(0, sep), 10);
      const offset = key.substring(sep + 1);
      rows.push({
        module: ranges[modIdx].name,
        offset: '0x' + offset,
        hits: count,
      });
    });
    send({
      kind: 'stalker-coverage',
      followed_tids: followTids,
      rows: rows,
    });
  }, DURATION);
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_with_modules_and_tid() {
        let js = render_coverage_script(
            Some(12345),
            &["libfoo.so".into(), "libbar.so".into()],
            500,
        );
        assert!(js.contains("[\"libfoo.so\",\"libbar.so\"]"));
        assert!(js.contains("const DURATION = 500;"));
        assert!(js.contains("const TID_ARG = 12345;"));
        assert!(js.contains("Stalker.follow"));
    }

    #[test]
    fn renders_with_defaults() {
        let js = render_coverage_script(None, &[], 1000);
        assert!(js.contains("const WANT = [];"));
        assert!(js.contains("const TID_ARG = null;"));
        assert!(js.contains("const DURATION = 1000;"));
    }

    #[test]
    fn escapes_module_names() {
        // A module name with a quote is malformed in practice
        // but we still want safe escaping rather than broken JS.
        let js = render_coverage_script(None, &["evil\".so".into()], 100);
        // serde escapes it: "evil\".so" -> "evil\\\".so"
        assert!(js.contains("evil\\\""));
    }
}
