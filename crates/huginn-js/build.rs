//! Joins the prelude, the JavaScript DOM layer (`src/js/*.js` in name
//! order), into `$OUT_DIR/prelude.js`. With the `v8` feature it also runs the
//! prelude once and saves the isolate as a V8 startup snapshot,
//! `$OUT_DIR/prelude.snap`, so that each page's isolate starts with the
//! prelude already run instead of compiling and running 260 KB of
//! JavaScript (see `src/v8engine/startup.rs`).

use std::env;
use std::fs;
use std::path::PathBuf;

#[cfg(feature = "v8")]
#[path = "src/v8engine/startup.rs"]
mod startup;

fn main() {
    println!("cargo::rustc-check-cfg=cfg(huginn_snapshot)");
    // A directory is scanned as a whole: new and removed files count too.
    println!("cargo::rerun-if-changed=src/js");
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    let mut files: Vec<PathBuf> = fs::read_dir("src/js")
        .expect("read src/js")
        .map(|e| e.expect("read src/js").path())
        .filter(|p| p.extension().is_some_and(|e| e == "js"))
        .collect();
    files.sort();
    let mut prelude = String::new();
    for f in &files {
        let text = fs::read_to_string(f).unwrap_or_else(|e| panic!("{}: {e}", f.display()));
        prelude.push_str(&text);
    }
    fs::write(out.join("prelude.js"), &prelude).expect("write prelude.js");
    #[cfg(feature = "v8")]
    snapshot(&prelude, &out);
}

/// Write the startup snapshot and set `cfg(huginn_snapshot)`, which makes
/// the engine use it. A snapshot belongs to one V8 build and one CPU
/// architecture, so a cross build gets none: its engine runs the prelude
/// for each page.
#[cfg(feature = "v8")]
fn snapshot(prelude: &str, out: &std::path::Path) {
    println!("cargo::rerun-if-changed=src/v8engine/startup.rs");
    if env::var("HOST") != env::var("TARGET") {
        return;
    }
    startup::init_v8(startup::V8_FLAGS);
    let mut creator = v8::Isolate::snapshot_creator(None, None);
    {
        v8::scope!(let scope, &mut creator);
        let context = v8::Context::new(scope, Default::default());
        let scope = &mut v8::ContextScope::new(scope, context);
        v8::tc_scope!(let tc, scope);
        let Some((native, hooks)) = startup::run_prelude(tc, prelude) else {
            panic!("the prelude failed: {}", startup::exception_message(tc));
        };
        tc.set_default_context(context);
        assert_eq!(tc.add_context_data(context, native), startup::N_INDEX);
        assert_eq!(tc.add_context_data(context, hooks), startup::HOOKS_INDEX);
    }
    // Keep the bytecode of the functions that ran, so pages do not compile
    // them again.
    let blob = creator
        .create_blob(v8::FunctionCodeHandling::Keep)
        .expect("V8 made no snapshot");
    fs::write(out.join("prelude.snap"), &*blob).expect("write prelude.snap");
    println!("cargo::rustc-cfg=huginn_snapshot");
}
