//! Page-script-facing globals that heso deliberately does not expose.
//!
//! `Atomics` and `SharedArrayBuffer` are gated on the open web by
//! `Cross-Origin-Opener-Policy` + `Cross-Origin-Embedder-Policy` HTTP
//! headers; ordinary public pages don't set them and don't use these
//! APIs. The stage-4 `Iterator.concat` / `Iterator.prototype.concat`
//! proposal is not relied on by React, Next.js, Vue, or Angular. Keeping
//! these absent from the global object trims the JavaScript attack
//! surface exposed to evaluated page code.
//!
//! Called once per [`crate::JsEngine`] from
//! [`crate::engine::JsEngine::new_inner`], immediately after the
//! `Context` is created and before any `install_*` step runs so no
//! later bootstrap can observe the removed globals.

use rquickjs::{Context, Ctx};

use crate::engine::EvalError;

/// Remove `Atomics` and `SharedArrayBuffer` from the global object and
/// replace `Iterator.concat` / `Iterator.prototype.concat` with throwing
/// stubs when the host engine exposes them.
pub(crate) fn disable_dangerous_globals(context: &Context) -> Result<(), EvalError> {
    context
        .with(|ctx: Ctx<'_>| -> rquickjs::Result<()> {
            let globals = ctx.globals();
            globals.remove("Atomics")?;
            globals.remove("SharedArrayBuffer")?;
            ctx.eval::<(), _>(ITERATOR_CONCAT_STUB)?;
            Ok(())
        })
        .map_err(|e| EvalError::Engine(format!("disable dangerous globals: {e}")))?;
    Ok(())
}

/// JS source that replaces `Iterator.concat` and
/// `Iterator.prototype.concat` with throwing stubs. The `typeof` guard
/// makes the snippet a no-op on QuickJS builds that don't ship the
/// proposal, so the same code stays correct across engine bumps.
const ITERATOR_CONCAT_STUB: &str = r#"
(function() {
    if (typeof Iterator === 'undefined') return;
    if ('concat' in Iterator) {
        Object.defineProperty(Iterator, 'concat', {
            value: function() {
                throw new TypeError('Iterator.concat is disabled in this environment');
            },
            writable: false,
            configurable: false,
        });
    }
    if (Iterator.prototype && 'concat' in Iterator.prototype) {
        Object.defineProperty(Iterator.prototype, 'concat', {
            value: function() {
                throw new TypeError('Iterator.prototype.concat is disabled in this environment');
            },
            writable: false,
            configurable: false,
        });
    }
})();
"#;

#[cfg(test)]
mod tests {
    use crate::JsEngine;

    #[test]
    fn atomics_global_is_undefined() {
        let e = JsEngine::new().expect("engine new");
        let out = e.eval("typeof Atomics").expect("eval ok");
        assert_eq!(out.value, serde_json::json!("undefined"));
    }

    #[test]
    fn shared_array_buffer_global_is_undefined() {
        let e = JsEngine::new().expect("engine new");
        let out = e.eval("typeof SharedArrayBuffer").expect("eval ok");
        assert_eq!(out.value, serde_json::json!("undefined"));
    }

    #[test]
    fn iterator_concat_throws_or_is_absent() {
        let e = JsEngine::new().expect("engine new");
        // Returns "absent" when the vendored QuickJS doesn't ship the
        // proposal at all, "threw" when it does and our stub fires.
        // Either outcome means evaluated page code can't reach the
        // upstream `concat` implementation.
        let out = e
            .eval(
                r#"
                (function() {
                    if (typeof Iterator === 'undefined') return 'absent';
                    var hasCtor = 'concat' in Iterator;
                    var hasProto = Iterator.prototype && 'concat' in Iterator.prototype;
                    if (!hasCtor && !hasProto) return 'absent';
                    try {
                        if (hasCtor) {
                            Iterator.concat([]);
                        } else {
                            Iterator.prototype.concat.call([].values());
                        }
                        return 'did-not-throw';
                    } catch (e) {
                        return 'threw';
                    }
                })()
                "#,
            )
            .expect("eval ok");
        let s = out.value.as_str().unwrap_or("");
        assert!(
            s == "absent" || s == "threw",
            "expected 'absent' or 'threw', got {s:?}"
        );
    }
}
