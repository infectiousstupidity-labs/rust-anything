use rust_anything::walk::Canonical;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::ExitCode;

/// `println!` that stops quietly when stdout is closed (`rust-anything ... | head`).
macro_rules! say {
    ($($arg:tt)*) => {{
        let _ = writeln!(std::io::stdout().lock(), $($arg)*);
    }};
}

macro_rules! say_raw {
    ($($arg:tt)*) => {{
        let _ = write!(std::io::stdout().lock(), $($arg)*);
    }};
}

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use rust_anything::{analyze, app, bridge, front_door, gate, harness, lift, mcp, report, shadow};

#[derive(Parser)]
#[command(
    name = "rust-anything",
    version,
    about = "Rewrite TypeScript/JavaScript, Python, C, Go, Ruby or Java apps in Rust, one verified piece at a time",
    after_help = "Loop per unit: analyze -> bridge -> record -> port src/port.rs -> build -> diff -> shadow -> gate"
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Clone, Copy, ValueEnum)]
enum KindArg {
    Napi,
    Wasm,
}

#[derive(Subcommand)]
enum Cmd {
    /// 1. Map a repository and write a ranked, ordered migration plan.
    Analyze {
        /// Repository root.
        #[arg(default_value = ".")]
        root: PathBuf,
        /// Print the machine-readable report (schema `rust-anything/analyze@1`) instead of Markdown.
        #[arg(long)]
        json: bool,
        /// Write to this file instead of stdout.
        #[arg(long, short)]
        out: Option<PathBuf>,
        /// Source language; detected from the repository when omitted.
        #[arg(long, value_enum)]
        lang: Option<rust_anything::lang::Lang>,
        /// Plan a whole-app port: entry points and shape, every dependency (imported or
        /// declared) with its crate, pieces in port order, and decisions for the user.
        #[arg(long)]
        whole_app: bool,
    },
    /// 2. Scaffold a Rust crate with JS bindings for one unit.
    Bridge {
        /// Repository root.
        root: PathBuf,
        /// Unit id, as `analyze` lists it.
        unit: String,
        /// Bridge to generate; defaults to the plan's choice.
        #[arg(long, value_enum)]
        kind: Option<KindArg>,
        /// Crate folder; defaults to `<root>/rust/<name>`.
        #[arg(long, short)]
        out: Option<PathBuf>,
        /// Crate name; defaults to `<unit>-rs`.
        #[arg(long)]
        name: Option<String>,
        /// Bridge a unit the plan marks blocked or keep.
        #[arg(long)]
        force: bool,
        /// Keep an export in TS: `name` or `"name: reason"`. Repeatable.
        #[arg(long)]
        keep: Vec<String>,
        /// Add a batch class: `function:param` runs `function` over many `param` values kept in
        /// Rust, on all cores (what `profile` recommends for leaves called in loops).
        #[arg(long)]
        batch: Vec<String>,
    },
    /// 2b. Regenerate the bindings (`lib.rs`, `index.cjs`, `js.rs`) from the signatures in `port.rs`.
    Refresh {
        #[arg(default_value = ".")]
        crate_dir: PathBuf,
        /// Add a batch class: `function:param` runs `function` over many `param` values kept in
        /// Rust, on all cores. Repeat for more.
        #[arg(long)]
        batch: Vec<String>,
    },
    /// 3a. Record what the TS functions return on generated and given inputs.
    Record {
        /// Crate folder made by `bridge`.
        #[arg(default_value = ".")]
        crate_dir: PathBuf,
        /// Generated cases per function.
        #[arg(long, default_value_t = 200)]
        count: usize,
        /// Extra cases: JSONL of `{"fn": name, "args": [...]}`.
        #[arg(long)]
        inputs: Option<PathBuf>,
        /// Import the original from this JS module (e.g. built output) instead of the TS source.
        #[arg(long)]
        module: Option<PathBuf>,
        /// Resolve npm packages from this folder's `node_modules` (when the repo has none installed).
        #[arg(long)]
        resolve_from: Option<PathBuf>,
        /// Extra arguments for node, e.g. `--node-arg=--import=tsx`.
        #[arg(long = "node-arg", allow_hyphen_values = true)]
        node_args: Vec<String>,
        /// Floats match when they differ by at most this times the larger magnitude (absolute
        /// below 1), e.g. 1e-9 for numeric code. Saved with the crate for diff and gate; 0 removes it.
        #[arg(long)]
        float_tolerance: Option<f64>,
        /// Also record every call the original's own test suite makes to the unit's functions:
        /// `--from-tests -- npx jest` (TS/JS, any runner) or `-- pytest` (Python).
        #[arg(long)]
        from_tests: bool,
        /// The test command for `--from-tests`, after `--`; it runs in the original's folder.
        #[arg(last = true)]
        test_command: Vec<String>,
    },
    /// 3b. Build the crate's JS module (napi-rs `.node` or wasm-bindgen output).
    Build {
        #[arg(default_value = ".")]
        crate_dir: PathBuf,
    },
    /// 3c. Replay recorded cases against the Rust module.
    Diff {
        #[arg(default_value = ".")]
        crate_dir: PathBuf,
        /// Test this JS module instead of the crate's `index.cjs`.
        #[arg(long)]
        module: Option<PathBuf>,
        /// Compare objects regardless of key order.
        #[arg(long)]
        ignore_key_order: bool,
        /// Floats match within this tolerance for this run (the one saved by `record` otherwise).
        #[arg(long)]
        float_tolerance: Option<f64>,
        #[arg(long)]
        json: bool,
        #[arg(long = "node-arg", allow_hyphen_values = true)]
        node_args: Vec<String>,
    },
    /// 3d. Time the recorded cases on the TS original and on Rust, bridge included.
    Bench {
        #[arg(default_value = ".")]
        crate_dir: PathBuf,
        /// Minimum measured time per function and side, in milliseconds.
        #[arg(long, default_value_t = 200)]
        min_ms: u64,
        /// Resolve npm packages the original imports from this folder's `node_modules`.
        #[arg(long)]
        resolve_from: Option<PathBuf>,
        #[arg(long)]
        json: bool,
        #[arg(long = "node-arg", allow_hyphen_values = true)]
        node_args: Vec<String>,
    },
    /// Run a real workload under the language's profiler and say what to port: hot functions,
    /// their callers, and loops to batch. `rust-anything profile <repo> -- node bench.mjs` (also
    /// `python3 x.py`, `go test -bench . ./pkg`, `java ...`, `ruby x.rb`)
    Profile {
        /// Repository root.
        root: PathBuf,
        #[arg(long)]
        json: bool,
        /// Source language; detected from the repository when omitted.
        #[arg(long, value_enum)]
        lang: Option<rust_anything::lang::Lang>,
        /// The workload, after `--`.
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    /// 4. Generate a drop-in module that runs TS and Rust side by side.
    Shadow {
        #[arg(default_value = ".")]
        crate_dir: PathBuf,
        /// Where to write the module; defaults to `<crate>/shadow/<name>.shadow.ts`.
        #[arg(long, short)]
        out: Option<PathBuf>,
    },
    /// 5. Check the definition of done for a port.
    Gate {
        #[arg(default_value = ".")]
        crate_dir: PathBuf,
        #[arg(long)]
        json: bool,
        #[arg(long = "node-arg", allow_hyphen_values = true)]
        node_args: Vec<String>,
    },
    /// Record the whole app's behavior and replay any build against it (CLIs).
    App {
        #[command(subcommand)]
        command: AppCmd,
    },
    /// Serve every step as MCP tools over stdio.
    Mcp,
    /// Register rust-anything with your coding agents (Claude Code, Codex, Cursor, Gemini CLI):
    /// the MCP server and, where the agent reads skills, the workflow skill. Keeps your other
    /// settings; run it again any time.
    Setup {
        /// Agents to set up (repeatable); every agent found by default.
        #[arg(long = "agent", value_enum)]
        agents: Vec<rust_anything::setup::Agent>,
        /// Take the rust-anything entries out again.
        #[arg(long)]
        remove: bool,
        /// Show what would change, write nothing.
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum AppCmd {
    /// Run the original CLI on cases (its own examples, `--inputs`, semantic traps), twice
    /// each, and store them as trycmd files in `<repo>/rust/app-cases/`.
    /// `rust-anything app record . -- python3 cli.py`
    Record {
        /// Repository root.
        #[arg(default_value = ".")]
        repo: PathBuf,
        /// Extra cases: JSONL of `{"name", "args": [...], "stdin", "env": {...}, "files": {path: text}}`.
        #[arg(long)]
        inputs: Option<PathBuf>,
        /// Normalizer: `timestamps`, `uuids`, `key-order`, or `name=regex` for your own. Repeatable;
        /// saved with the cases.
        #[arg(long)]
        normalize: Vec<String>,
        /// The name the app is invoked by in its docs (found from the command and manifests when omitted).
        #[arg(long)]
        name: Option<String>,
        /// Leave out the semantic-trap inputs.
        #[arg(long)]
        no_traps: bool,
        /// Seconds before a run is killed (default 30).
        #[arg(long)]
        timeout: Option<u64>,
        /// The app is a server: the command starts it, taking its port from PORT or `{port}`;
        /// cases are requests (curl lines in its docs, `--inputs`, traps), kept in `http/`.
        /// Curl @file bodies must be relative, regular files within the repository (docs included).
        #[arg(long)]
        http: bool,
        /// A port the server always listens on (implies --http).
        #[arg(long)]
        port: Option<u16>,
        /// Numbers in the output match when they differ by at most this times the larger
        /// magnitude (absolute below 1), e.g. 1e-9 for numeric code. Saved with the cases; 0
        /// removes it.
        #[arg(long)]
        float_tolerance: Option<f64>,
        #[arg(long)]
        json: bool,
        /// The command that runs the original, after `--`; paths are relative to the repository.
        /// Taken from the saved cases when omitted.
        #[arg(last = true)]
        run: Vec<String>,
    },
    /// Run a build against `<repo>/rust/app-cases/`, print passed/total, write a report with a
    /// diff per failing case; exits non-zero on any mismatch.
    Replay {
        /// Repository root.
        #[arg(default_value = ".")]
        repo: PathBuf,
        /// Command line of the build to test (the original, the front door, the final binary);
        /// the original when omitted.
        #[arg(long)]
        build: Option<String>,
        /// Seconds before a run is killed.
        #[arg(long)]
        timeout: Option<u64>,
        /// Servers: the port the build always listens on; otherwise it gets a free one through
        /// PORT and `{port}`.
        #[arg(long)]
        port: Option<u16>,
        /// Replay only this case, by the name the report gives it (a library: only this unit).
        /// The fast, repeatable loop for fixing one mismatch.
        #[arg(long)]
        case: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Scaffold `<repo>/rust/app`, the Rust binary that owns the CLI's entry point: a clap parser
    /// for the recorded commands and flags, every command running the original until a unit is
    /// wired. Builds it and replays the cases (100% passed, 100% fallback).
    FrontDoor {
        /// Repository root.
        #[arg(default_value = ".")]
        repo: PathBuf,
        /// Only write the files; skip the build and replay.
        #[arg(long)]
        no_check: bool,
        /// Treat the repository as a library even if it has a `main`: the front door is a Rust
        /// crate with the public API (found by default when there is no main and no server).
        #[arg(long)]
        library: bool,
        #[arg(long)]
        json: bool,
    },
    /// Hand commands to a unit's `port.rs` (after it passes gate): adds the crate to `rust/app`,
    /// a glue file to write once, the routing and a kill switch
    /// (`RUST_ANYTHING_UNIT_<NAME>=original`), then builds and replays.
    Wire {
        /// Repository root.
        repo: PathBuf,
        /// Unit crate: its folder, its name under `rust/`, or the unit id it was bridged from.
        unit: String,
        /// A command the unit answers. Repeatable; kept from the last wire when omitted.
        #[arg(long = "command")]
        commands: Vec<String>,
        /// Servers: a route the unit answers, axum syntax (`GET /users/{id}`, `/path` for any
        /// method, `*` for everything else). Repeatable; kept from the last wire when omitted.
        #[arg(long = "route")]
        routes: Vec<String>,
        /// Do not run gate on the unit first (it just passed).
        #[arg(long)]
        skip_gate: bool,
        /// Only write the files; skip the build and replay.
        #[arg(long)]
        no_check: bool,
        #[arg(long)]
        json: bool,
    },
    /// Make `rust/app` a pure Rust app, once replay passes with fallback rate 0 and every unit
    /// passes gate: removes the fallback and the kill switches, turns the units into plain
    /// libraries, runs the lift checklist and replays the pure binary. Checked on a copy first;
    /// the original stays in place.
    Lift {
        /// Repository root.
        #[arg(default_value = ".")]
        repo: PathBuf,
        #[arg(long)]
        json: bool,
    },
}

fn say_replay(replay: &app::ReplayOutcome) {
    say!("{}/{} cases passed", replay.passed, replay.total);
    if let Some(f) = &replay.fallback {
        say!(
            "Fallback rate: {:.1}% ({}/{})",
            f.rate * 100.0,
            f.cases,
            f.total
        );
    }
    for f in replay.failures.iter().take(20) {
        let fields: Vec<&str> = f.problems.iter().map(|p| p.field.as_str()).collect();
        say!("- {}: {}", f.case, fields.join(", "));
    }
    if replay.failures.len() > 20 {
        say!("- ... and {} more", replay.failures.len() - 20);
    }
    say!("Report: {}", replay.report.display());
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<ExitCode> {
    match Cli::parse().command {
        Cmd::Analyze {
            root,
            json,
            out,
            lang,
            whole_app,
        } => {
            let lang = lang.unwrap_or_else(|| rust_anything::lang::Lang::detect(&root));
            let mut report = analyze::analyze_lang(&root, lang)
                .with_context(|| format!("analyzing {}", root.display()))?;
            if whole_app {
                report.whole_app = Some(rust_anything::whole_app::whole_app(&root, &report));
            }
            let text = if json {
                serde_json::to_string_pretty(&report)? + "\n"
            } else {
                let title = root
                    .canonical()
                    .ok()
                    .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                    .unwrap_or_else(|| root.display().to_string());
                report::markdown(&report, &title)
            };
            match out {
                Some(path) => std::fs::write(&path, text)
                    .with_context(|| format!("writing {}", path.display()))?,
                None => say_raw!("{text}"),
            }
        }
        Cmd::Bridge {
            root,
            unit,
            kind,
            out,
            name,
            force,
            keep,
            batch,
        } => {
            let outcome = bridge::bridge(&bridge::Options {
                root: &root,
                unit: &unit,
                kind: kind.map(|k| match k {
                    KindArg::Napi => bridge::Kind::Napi,
                    KindArg::Wasm => bridge::Kind::Wasm,
                }),
                out,
                name,
                force,
                keep,
                lang: None,
                batch,
            })?;
            say!(
                "Created {} ({:?}) for {}",
                outcome.crate_dir.display(),
                outcome.kind,
                outcome.exports.join(", ")
            );
            for file in &outcome.files {
                say!("  {file}");
            }
            for note in &outcome.notes {
                say!("- {note}");
            }
        }
        Cmd::Record {
            crate_dir,
            count,
            inputs,
            module,
            resolve_from,
            node_args,
            float_tolerance,
            from_tests,
            test_command,
        } => {
            anyhow::ensure!(
                from_tests || test_command.is_empty(),
                "a command after `--` is the test suite for `--from-tests`; add the flag"
            );
            let inputs = if from_tests {
                let c = rust_anything::from_tests::capture(&crate_dir, &test_command)?;
                say!(
                    "Captured {} call(s) from the tests{}:",
                    c.per_function.values().sum::<usize>(),
                    if c.tests_passed {
                        ""
                    } else {
                        " (the test command failed; calls made before that are kept)"
                    }
                );
                for (name, n) in &c.per_function {
                    say!("  {name}: {n}");
                }
                for s in &c.skipped {
                    say!("  not captured: {s}");
                }
                Some(rust_anything::from_tests::with_own_inputs(
                    &crate_dir,
                    inputs.as_deref(),
                    &c.inputs,
                )?)
            } else {
                inputs
            };
            if let Some(t) = float_tolerance {
                let mut manifest = bridge::Manifest::load(&crate_dir)?;
                manifest.float_tolerance = (rust_anything::tolerance::check(t)? > 0.0).then_some(t);
                manifest.save(&crate_dir)?;
            }
            let outcome = harness::record(&harness::RecordOptions {
                crate_dir: &crate_dir,
                count,
                inputs: inputs.as_deref(),
                module: module.as_deref(),
                resolve_from: resolve_from.as_deref(),
                node_args: &node_args,
            })?;
            say!(
                "Recorded {} cases ({} threw) in {}",
                outcome.cases,
                outcome.errors,
                outcome.path.display()
            );
            for (name, n) in &outcome.per_function {
                say!("  {name}: {n}");
            }
            if !outcome.regex_functions.is_empty() {
                say!(
                    "Regex traps (non-ASCII \\w, \\b and case folding) added for: {}",
                    outcome.regex_functions.join(", ")
                );
            }
            if !outcome.timed_out.is_empty() {
                say!(
                    "Left out {} input(s) the original ran too long on (over the per-case limit, \
                     RUST_ANYTHING_CASE_TIMEOUT seconds, 10 by default). The original does this \
                     itself, so the port need not match it there:",
                    outcome.timed_out.len()
                );
                for c in &outcome.timed_out {
                    say!("  {}({})", c["fn"].as_str().unwrap_or("?"), c["args"]);
                }
            }
        }
        Cmd::Refresh { crate_dir, batch } => {
            let r = if batch.is_empty() {
                bridge::refresh(&crate_dir)?
            } else {
                bridge::add_batches(&crate_dir, &batch)?
            };
            say!(
                "Regenerated {} for {}",
                r.files.join(", "),
                r.functions.join(", ")
            );
        }
        Cmd::Build { crate_dir } => {
            say!("Built {}", harness::build(&crate_dir)?.display());
        }
        Cmd::Diff {
            crate_dir,
            module,
            ignore_key_order,
            float_tolerance,
            json,
            node_args,
        } => {
            let d = harness::diff(&harness::DiffOptions {
                crate_dir: &crate_dir,
                module: module.as_deref(),
                ignore_key_order,
                node_args: &node_args,
                float_tolerance,
            })?;
            if json {
                say!("{}", serde_json::to_string_pretty(&d)?);
            } else {
                if let Ok(m) = rust_anything::bridge::Manifest::load(&crate_dir)
                    && let Some(why) = rust_anything::recording::stale(&crate_dir, &m)
                {
                    say!("Warning: {why}");
                }
                if d.accepted > 0 {
                    say!(
                        "{}/{} cases match ({} by {})",
                        d.passed,
                        d.total,
                        d.accepted,
                        rust_anything::recording::KNOWN_DIFFS
                    );
                } else {
                    say!("{}/{} cases match", d.passed, d.total);
                }
                for e in &d.stale_known {
                    say!(
                        "Known diff no longer happens, remove it from {}: {}({})",
                        rust_anything::recording::KNOWN_DIFFS,
                        e["fn"].as_str().unwrap_or_default(),
                        e["args"]
                    );
                }
                if !d.missing.is_empty() {
                    say!("Missing exports: {}", d.missing.join(", "));
                }
                for m in &d.mismatches {
                    say!(
                        "- {}({}): expected {}, got {}",
                        m["fn"].as_str().unwrap_or_default(),
                        m["args"]
                            .as_array()
                            .map(|a| a
                                .iter()
                                .map(ToString::to_string)
                                .collect::<Vec<_>>()
                                .join(", "))
                            .unwrap_or_default(),
                        m["expected"],
                        m["actual"]
                    );
                }
            }
            if d.failed > 0 {
                return Ok(ExitCode::FAILURE);
            }
        }
        Cmd::Bench {
            crate_dir,
            min_ms,
            resolve_from,
            json,
            node_args,
        } => {
            let rows = harness::bench(&harness::BenchOptions {
                crate_dir: &crate_dir,
                min_ms,
                resolve_from: resolve_from.as_deref(),
                node_args: &node_args,
            })?;
            if json {
                say!("{}", serde_json::to_string_pretty(&rows)?);
            } else {
                say!(
                    "{:<24} {:>6} {:>12} {:>12} {:>9}",
                    "function",
                    "cases",
                    "orig ns/call",
                    "Rust ns/call",
                    "speedup"
                );
                for r in &rows {
                    say!(
                        "{:<24} {:>6} {:>12.0} {:>12.0} {:>8.2}x",
                        r.name,
                        r.cases,
                        r.original_ns,
                        r.rust_ns,
                        r.speedup
                    );
                }
                let lang = bridge::Manifest::load(&crate_dir)
                    .map(|m| m.language)
                    .unwrap_or_default();
                let v = harness::verdict(&rows, lang);
                say!(
                    "\nTimed as callers call it, crossing included. Overall {:.2}x (geometric mean); {} of {} functions at least 1.1x faster.",
                    v.speedup,
                    v.faster.len(),
                    rows.len()
                );
                say!("{}", v.advice);
            }
        }
        Cmd::Profile {
            root,
            json,
            lang,
            command,
        } => {
            let report = rust_anything::profile::profile(&root, &command, lang)?;
            if json {
                say!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                say!("{}", rust_anything::profile::markdown(&report));
            }
        }
        Cmd::Shadow { crate_dir, out } => {
            let outcome = shadow::shadow(&crate_dir, out.as_deref())?;
            say!(
                "Wrote {} ({})",
                outcome.path.display(),
                outcome.exports.join(", ")
            );
            if outcome.path.extension().is_some_and(|e| e == "java") {
                say!(
                    "Call the <Class>Shadow classes in place of the originals; compile them with the originals and java/rust-anything. -Drust-anything.mode=old|shadow|new switches at runtime."
                );
            } else if outcome.path.extension().is_some_and(|e| e == "rb") {
                say!(
                    "Require it after the original files. RUST_ANYTHING_MODE=old|shadow|new switches at runtime."
                );
            } else if outcome.path.extension().is_some_and(|e| e == "go") {
                say!(
                    "Import it in place of the original package (require and replace both modules in go.mod). RUST_ANYTHING_MODE=old|shadow|new switches at runtime."
                );
            } else if outcome.path.extension().is_some_and(|e| e == "c") {
                say!(
                    "Compile the original with the -D flags in shadow/flags.txt, build the crate with --features shadow, and link all three."
                );
            } else {
                say!(
                    "Import it in place of the original. RUST_ANYTHING_MODE=old|shadow|new switches at runtime."
                );
            }
        }
        Cmd::Gate {
            crate_dir,
            json,
            node_args,
        } => {
            let outcome = gate::gate(&crate_dir, &node_args)?;
            if json {
                say!("{}", serde_json::to_string_pretty(&outcome)?);
            } else {
                for check in &outcome.checks {
                    let mark = if check.passed { "pass" } else { "FAIL" };
                    let first = check.detail.lines().next().unwrap_or_default();
                    say!("{mark}  {:<10} {first}", check.name);
                    if !check.passed {
                        for line in check.detail.lines().skip(1) {
                            say!("            {line}");
                        }
                    }
                }
                say!(
                    "{}",
                    if outcome.passed {
                        "Gate passed."
                    } else {
                        "Gate failed."
                    }
                );
            }
            if !outcome.passed {
                return Ok(ExitCode::FAILURE);
            }
        }
        Cmd::App { command } => return run_app(command),
        Cmd::Mcp => mcp::serve()?,
        Cmd::Setup {
            agents,
            remove,
            dry_run,
            json,
        } => {
            let home = rust_anything::setup::home()?;
            let command = std::env::current_exe()
                .and_then(|p| p.canonical())
                .context("finding the rust-anything executable")?;
            let report = rust_anything::setup::setup(&rust_anything::setup::Options {
                home: &home,
                agents,
                command,
                remove,
                dry_run,
            })?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print!("{}", rust_anything::setup::text(&report));
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn run_app(command: AppCmd) -> Result<ExitCode> {
    match command {
        AppCmd::Record {
            repo,
            inputs,
            normalize,
            name,
            no_traps,
            timeout,
            http,
            port,
            float_tolerance,
            json,
            run,
        } => {
            let outcome = app::record(&app::RecordOptions {
                repo: &repo,
                run: &run,
                inputs: inputs.as_deref(),
                normalize: &normalize,
                name: name.as_deref(),
                traps: !no_traps,
                timeout_secs: timeout,
                http,
                port,
                float_tolerance,
            })?;
            if json {
                say!("{}", serde_json::to_string_pretty(&outcome)?);
                return Ok(ExitCode::SUCCESS);
            }
            say!(
                "Recorded {} cases in {} ({} from examples, {} from inputs, {} traps)",
                outcome.cases,
                outcome.dir.display(),
                outcome.examples,
                outcome.inputs,
                outcome.traps
            );
            for (kind, what) in [
                (
                    app::NoiseKind::Lines,
                    "Lines (a different number of lines between two runs; now `...`, which matches anything: check these first)",
                ),
                (
                    app::NoiseKind::Order,
                    "Order (the same lines in another order between two runs; replay compares them in any order)",
                ),
                (
                    app::NoiseKind::Values,
                    "Values (parts of lines differed between two runs; now `[..]` wildcards)",
                ),
            ] {
                let hits: Vec<String> = outcome
                    .noise
                    .iter()
                    .filter(|n| n.kind == kind)
                    .map(|n| format!("{} ({})", n.case, n.field))
                    .collect();
                if !hits.is_empty() {
                    say!("Noise, {what}: {}", hits.join(", "));
                }
            }
            if !outcome.unstable.is_empty() {
                say!(
                    "Unstable (exit code or files differed between runs; check them): {}",
                    outcome.unstable.join(", ")
                );
            }
            if !outcome.timed_out.is_empty() {
                say!("Timed out, not recorded: {}", outcome.timed_out.join(", "));
            }
        }
        AppCmd::Replay {
            repo,
            build,
            timeout,
            port,
            case,
            json,
        } => {
            let outcome = app::replay(&app::ReplayOptions {
                repo: &repo,
                build: build.as_deref(),
                timeout_secs: timeout,
                port,
                case: case.as_deref(),
            })?;
            if json {
                say!("{}", serde_json::to_string_pretty(&outcome)?);
            } else {
                say_replay(&outcome);
            }
            if !outcome.failures.is_empty() {
                return Ok(ExitCode::FAILURE);
            }
        }
        AppCmd::FrontDoor {
            repo,
            no_check,
            library,
            json,
        } => {
            let outcome = front_door::front_door(&front_door::FrontDoorOptions {
                repo: &repo,
                library,
                check: !no_check,
            })?;
            if json {
                say!("{}", serde_json::to_string_pretty(&outcome)?);
            } else {
                say!(
                    "Created {} ({} `{}`), {}",
                    outcome.dir.display(),
                    if outcome.api.is_empty() {
                        "binary"
                    } else {
                        "crate"
                    },
                    outcome.binary,
                    if !outcome.api.is_empty() {
                        format!(
                            "a library; public API: {}",
                            outcome
                                .api
                                .iter()
                                .map(|a| format!("{} ({} exports)", a.unit, a.exports.len()))
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    } else if !outcome.routes.is_empty() {
                        format!("requests recorded: {}", outcome.routes.join(", "))
                    } else if outcome.commands.is_empty() {
                        "commands: none (one root command)".to_string()
                    } else {
                        format!("commands: {}", outcome.commands.join(", "))
                    }
                );
                for file in &outcome.files {
                    say!("  {file}");
                }
                if let Some(replay) = &outcome.replay {
                    say_replay(replay);
                }
                if !outcome.api.is_empty() {
                    say!(
                        "Every export is still only in the original. After a unit's crate (from `bridge`) passes gate: rust-anything app wire {} <unit crate>",
                        repo.display()
                    );
                } else if outcome.routes.is_empty() {
                    say!(
                        "Every command runs the original. After a unit passes gate: rust-anything app wire {} <unit> --command <name>",
                        repo.display()
                    );
                } else {
                    say!(
                        "Every request goes to the original. After a unit passes gate: rust-anything app wire {} <unit> --route 'GET /path/{{param}}'",
                        repo.display()
                    );
                }
            }
            if outcome
                .replay
                .as_ref()
                .is_some_and(|r| !r.failures.is_empty())
            {
                return Ok(ExitCode::FAILURE);
            }
        }
        AppCmd::Wire {
            repo,
            unit,
            commands,
            routes,
            skip_gate,
            no_check,
            json,
        } => {
            let outcome = front_door::wire(&front_door::WireOptions {
                repo: &repo,
                unit: &unit,
                commands: &commands,
                routes: &routes,
                skip_gate,
                check: !no_check,
            })?;
            if json {
                say!("{}", serde_json::to_string_pretty(&outcome)?);
            } else {
                say!(
                    "Wired {} ({}) to {}; {}=original sends it back to the original",
                    outcome.unit,
                    outcome.crate_dir.display(),
                    if outcome.shadow.is_some() {
                        "the public API".to_string()
                    } else if outcome.routes.is_empty() {
                        outcome.commands.join(", ")
                    } else {
                        outcome.routes.join(", ")
                    },
                    outcome.switch
                );
                if let Some(shadow) = &outcome.shadow {
                    say!(
                        "The original package imports {} in place of the unit until the lift.",
                        shadow.display()
                    );
                    if shadow.extension().is_some_and(|e| e == "c") {
                        say!(
                            "C switches at build time: compile without the -D flags in shadow/flags.txt to send it back."
                        );
                    }
                }
                if outcome.glue_pending {
                    say!(
                        "Write the glue in {}, then run this again.",
                        outcome.glue.display()
                    );
                }
                if let Some(replay) = &outcome.replay {
                    say_replay(replay);
                }
            }
            if outcome
                .replay
                .as_ref()
                .is_some_and(|r| !r.failures.is_empty())
            {
                return Ok(ExitCode::FAILURE);
            }
        }
        AppCmd::Lift { repo, json } => {
            let outcome = lift::lift(&lift::LiftOptions { repo: &repo })?;
            if json {
                say!("{}", serde_json::to_string_pretty(&outcome)?);
            } else {
                for item in &outcome.checklist {
                    let mark = match item.mark {
                        lift::Mark::Pass => "pass",
                        lift::Mark::Warn => "warn",
                        lift::Mark::Fail => "FAIL",
                        lift::Mark::Skip => "skip",
                    };
                    let mut lines = item.detail.lines();
                    say!(
                        "{mark}  {:<24} {}",
                        item.name,
                        lines.next().unwrap_or_default()
                    );
                    if item.mark == lift::Mark::Fail {
                        for line in lines {
                            say!("      {line}");
                        }
                    }
                }
                if outcome.lifted {
                    say!(
                        "Lifted: {} is a pure Rust app ({}). Removed {} generated files; the original is untouched.",
                        repo.join(front_door::APP_DIR).display(),
                        outcome
                            .binary
                            .as_ref()
                            .map(|b| b.display().to_string())
                            .unwrap_or_default(),
                        outcome.removed.len()
                    );
                } else {
                    say!("Not lifted:");
                    for b in &outcome.blockers {
                        say!("- {b}");
                    }
                    if let Some(stage) = &outcome.stage {
                        say!("The lifted copy is in {} for a look.", stage.display());
                    }
                }
            }
            if !outcome.lifted {
                return Ok(ExitCode::FAILURE);
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}
