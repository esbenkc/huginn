//! Starting V8 and running the prelude: shared by the engine and by the
//! build script, which compiles this file too (`#[path]` in build.rs) and so
//! uses only the `v8` crate here.
//!
//! The build runs the prelude once and saves the isolate as a V8 startup
//! snapshot; the engine makes each page's isolate from it, with the prelude
//! already run (or runs the prelude itself when there is no snapshot). The
//! prelude does not call its native operations `N` while it runs: they and
//! the page's values come per page, through the `start` hook (see the top
//! of `src/js/00-base.js`).

use std::sync::Once;

/// V8's flags. A startup snapshot only works with the flags it was made
/// with, so the build uses these too. One thread: no background compiler or
/// GC threads (less memory, and a page load is short). Lite mode:
/// interpreter only, no JIT code and lazy feedback vectors. On the corpus it
/// cuts the V8 heap by up to half (bbc 58 -> 28 MB) at the same wall time,
/// because page scripts mostly run once.
pub const V8_FLAGS: &str = "--single-threaded --lite-mode";

/// The prelude's script name, in errors and stack traces.
pub const PRELUDE_URL: &str = "huginn:prelude";

/// Where the snapshot keeps the prelude's `N` object and its hooks (context
/// data, added in this order).
pub const N_INDEX: usize = 0;
pub const HOOKS_INDEX: usize = 1;

#[repr(C, align(16))]
pub struct Aligned<T: ?Sized>(pub T);

/// ICU data for `Intl`, `toLocaleString` and `localeCompare`. This is the
/// small (1.8 MB) Flutter desktop subset shipped with the `v8` crate; its
/// pages are only touched when a script uses them.
static ICU_DATA: &Aligned<[u8]> = &Aligned(*include_bytes!("../../data/icudtl.dat"));

/// Set V8 up for this process with `flags`. Only the first call counts.
pub fn init_v8(flags: &str) {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let _ = v8::icu::set_common_data_78(&ICU_DATA.0);
        v8::V8::set_flags_from_string(flags);
        v8::V8::initialize_platform(v8::new_single_threaded_default_platform(false).make_shared());
        v8::V8::initialize();
    });
}

/// Run the prelude `source` in the current context, with a new empty object
/// as `N` and the global object as `G`. Returns `N` and the prelude's
/// hooks; `None` means an exception is pending.
pub fn run_prelude<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    source: &str,
) -> Option<(v8::Local<'s, v8::Object>, v8::Local<'s, v8::Object>)> {
    let source = v8::String::new(scope, source)?;
    let name = v8::String::new(scope, PRELUDE_URL)?;
    let origin = v8::ScriptOrigin::new(
        scope,
        name.into(),
        0,
        0,
        false,
        0,
        None,
        false,
        false,
        false,
        None,
    );
    let func = v8::Script::compile(scope, source, Some(&origin))?.run(scope)?;
    let Ok(func) = v8::Local::<v8::Function>::try_from(func) else {
        return throw(scope, "the prelude is not a function");
    };
    let native = v8::Object::new(scope);
    let global = scope.get_current_context().global(scope);
    let undefined = v8::undefined(scope).into();
    let hooks = func.call(scope, undefined, &[native.into(), global.into()])?;
    let Ok(hooks) = v8::Local::<v8::Object>::try_from(hooks) else {
        return throw(scope, "the prelude returned no hooks");
    };
    Some((native, hooks))
}

fn throw<T>(scope: &mut v8::PinScope<'_, '_>, message: &str) -> Option<T> {
    let message = v8::String::new(scope, message)?;
    let error = v8::Exception::error(scope, message);
    scope.throw_exception(error);
    None
}

/// "url:line: message" for the pending exception.
pub fn exception_message(tc: &mut v8::PinnedRef<'_, v8::TryCatch<v8::HandleScope>>) -> String {
    if tc.has_terminated() {
        return "script terminated (time or memory budget)".into();
    }
    let Some(exception) = tc.exception() else {
        return "unknown error".into();
    };
    let text = exception.to_rust_string_lossy(tc);
    match tc.message() {
        Some(m) => {
            let file = m
                .get_script_resource_name(tc)
                .map(|s| s.to_rust_string_lossy(tc))
                .unwrap_or_default();
            let line = m.get_line_number(tc).unwrap_or(0);
            format!("{file}:{line}: {text}")
        }
        None => text,
    }
}
