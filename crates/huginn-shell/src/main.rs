//! `huginn`: the browser binary.
//!
//! `huginn render <url-or-file> --out <png> [--width 1280] [--height 800] [--full-page] [--no-js]`
//! loads a page, renders it headless and prints one JSON line with timings.
//! `--no-js` (or `HUGINN_NO_JS=1`) skips scripts, like a browser with JavaScript off.
//!
//! `huginn open [<url-or-file>] [--width 1280] [--height 800] [--no-js]` opens a
//! window. With `--commands "<commands>" [--scale 1]` it runs the same browser
//! without a window (see `headless.rs`); `--script "<commands>"` runs the same
//! commands in the real window and quits after the last one.

mod browser;
mod clipboard;
mod engine;
mod headless;
mod input;
mod interact;
mod internal_pages;
mod live;
mod livepage;
mod pipeline;
mod resources;
mod scripts;
mod session;
mod shortcuts;
mod tab;
mod tabstrip;
mod toolbar;
mod ui;
mod window;

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

const USAGE: &str = "usage:
  huginn render <url-or-file> --out <png> [--width 1280] [--height 800] [--full-page] [--no-js]
  huginn open [<url-or-file>] [--width 1280] [--height 800] [--no-js] [--commands <commands> [--scale 1] | --script <commands>]";

fn main() -> ExitCode {
    let start = Instant::now();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("render") => match parse_render_args(&args[1..]) {
            Ok(opts) => run_render(opts, start),
            Err(e) => {
                eprintln!("huginn: {e}\n{USAGE}");
                ExitCode::from(2)
            }
        },
        Some("open") => match parse_open_args(&args[1..]) {
            Ok(opts) => run_open(opts),
            Err(e) => {
                eprintln!("huginn: {e}\n{USAGE}");
                ExitCode::from(2)
            }
        },
        _ => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

struct OpenOptions {
    url: Option<huginn_net::Url>,
    width: f32,
    height: f32,
    commands: Option<String>,
    script: Option<String>,
    js: bool,
    scale: f32,
}

fn parse_open_args(args: &[String]) -> Result<OpenOptions, String> {
    let mut opts = OpenOptions {
        url: None,
        width: 1280.0,
        height: 800.0,
        commands: None,
        script: None,
        js: std::env::var_os("HUGINN_NO_JS").is_none(),
        scale: 1.0,
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = |name: &str| {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{name} needs a value"))
        };
        match a.as_str() {
            "--width" => opts.width = value("--width")?.parse().map_err(|_| "bad --width")?,
            "--height" => opts.height = value("--height")?.parse().map_err(|_| "bad --height")?,
            "--scale" => opts.scale = value("--scale")?.parse().map_err(|_| "bad --scale")?,
            "--commands" => opts.commands = Some(value("--commands")?),
            "--script" => opts.script = Some(value("--script")?),
            "--no-js" => opts.js = false,
            s if s.starts_with('-') => return Err(format!("unknown option {s}")),
            s if opts.url.is_none() => {
                // An existing file wins over a bare host name ("page.html").
                let url = if std::path::Path::new(s).exists() {
                    huginn_net::parse_target(s).ok()
                } else {
                    internal_pages::parse_url(s).or_else(|| huginn_net::parse_target(s).ok())
                };
                opts.url = Some(url.ok_or_else(|| format!("not a URL or file: {s}"))?);
            }
            s => return Err(format!("unexpected argument {s}")),
        }
    }
    let ok = |v: f32| (64.0..=16384.0).contains(&v);
    if !ok(opts.width) || !ok(opts.height) || !(0.5..=4.0).contains(&opts.scale) {
        return Err("window must be 64..=16384 CSS px on each side, scale 0.5..=4".into());
    }
    Ok(opts)
}

fn run_open(opts: OpenOptions) -> ExitCode {
    let result = match &opts.commands {
        Some(c) => headless::run(opts.url, opts.width, opts.height, opts.scale, opts.js, c),
        None => window::run(
            opts.url,
            f64::from(opts.width),
            f64::from(opts.height),
            opts.js,
            opts.script.as_deref(),
        ),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("huginn: {e}");
            ExitCode::from(1)
        }
    }
}

fn parse_render_args(args: &[String]) -> Result<pipeline::RenderOptions, String> {
    let mut target = None;
    let mut out = None;
    let mut width = 1280u32;
    let mut height = 800u32;
    let mut full_page = false;
    let mut js = std::env::var_os("HUGINN_NO_JS").is_none();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = |name: &str| {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{name} needs a value"))
        };
        match a.as_str() {
            "--out" | "-o" => out = Some(PathBuf::from(value("--out")?)),
            "--width" => width = value("--width")?.parse().map_err(|_| "bad --width")?,
            "--height" => height = value("--height")?.parse().map_err(|_| "bad --height")?,
            "--full-page" => full_page = true,
            "--no-js" => js = false,
            s if s.starts_with('-') => return Err(format!("unknown option {s}")),
            s if target.is_none() => target = Some(s.to_owned()),
            s => return Err(format!("unexpected argument {s}")),
        }
    }
    if width == 0 || height == 0 || width > 16384 || height > 16384 {
        return Err("viewport must be 1..=16384 px on each side".into());
    }
    Ok(pipeline::RenderOptions {
        target: target.ok_or("missing <url-or-file>")?,
        out: out.ok_or("missing --out <png>")?,
        width,
        height,
        full_page,
        js,
    })
}

fn run_render(opts: pipeline::RenderOptions, start: Instant) -> ExitCode {
    // Deep DOMs recurse deeply in style and layout; give the pipeline a big
    // stack. Only touched pages are committed, so this costs no memory.
    let worker = std::thread::Builder::new()
        .name("render".into())
        .stack_size(256 << 20)
        .spawn(move || pipeline::render(&opts, start))
        .expect("spawn render thread");
    match worker.join() {
        Ok(Ok(report)) => {
            println!("{report}");
            ExitCode::SUCCESS
        }
        Ok(Err(e)) => {
            eprintln!("huginn: {e}");
            ExitCode::from(1)
        }
        Err(_) => {
            eprintln!("huginn: render thread panicked");
            ExitCode::from(101)
        }
    }
}
