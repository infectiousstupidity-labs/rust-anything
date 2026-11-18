//! MCP server over stdio: every step as a tool, for any agent.
//!
//! Newline-delimited JSON-RPC 2.0, as the MCP stdio transport specifies. The protocol surface
//! is small (initialize, tools/list, tools/call, ping), so it is implemented directly on
//! serde_json instead of pulling in an SDK.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::{analyze, app, bridge, front_door, gate, harness, isolation, lift, shadow};

const PROTOCOL_VERSION: &str = "2025-06-18";

pub fn serve() -> Result<()> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Value>(&line) {
            Ok(request) => handle(&request),
            Err(e) => Some(error(Value::Null, -32700, &format!("parse error: {e}"))),
        };
        if let Some(response) = response {
            writeln!(stdout, "{response}")?;
            stdout.flush()?;
        }
    }
    Ok(())
}

/// The response to one JSON-RPC message; `None` for notifications.
pub fn handle(request: &Value) -> Option<Value> {
    let id = request.get("id").cloned();
    let method = request["method"].as_str().unwrap_or_default();
    let id = id?;
    let result = match method {
        "initialize" => json!({
            "protocolVersion": request["params"]["protocolVersion"].as_str().unwrap_or(PROTOCOL_VERSION),
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "rust-anything", "version": env!("CARGO_PKG_VERSION") },
            "instructions": "Incremental TS/JS to Rust migration. Loop per unit: analyze -> bridge -> \
                record -> (port src/port.rs) -> build -> diff -> shadow -> gate. Never mark a port done \
                before gate passes. Execution uses host permissions unless explicitly                 isolated via app_record/app_replay on Linux.",
        }),
        "ping" => json!({}),
        "tools/list" => json!({ "tools": tools() }),
        "tools/call" => {
            let name = request["params"]["name"].as_str().unwrap_or_default();
            let args = &request["params"]["arguments"];
            match call(name, args) {
                Ok(value) => json!({
                    "content": [{ "type": "text", "text": serde_json::to_string_pretty(&value).unwrap_or_default() }],
                    "structuredContent": value,
                    "isError": false,
                }),
                Err(e) => json!({
                    "content": [{ "type": "text", "text": format!("{e:#}") }],
                    "isError": true,
                }),
            }
        }
        _ => return Some(error(id, -32601, &format!("unknown method `{method}`"))),
    };
    Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn tools() -> Value {
    let crate_dir = json!({ "type": "string", "description": "Folder of the generated crate (holds rust-anything.json)." });
    json!([
        {
            "name": "analyze",
            "description": "Map a repository (TypeScript/JavaScript, Python, C, Go, Ruby or Java): units, port order (waves), risks with file:line, bridge per unit, dependency-to-crate fit, and `start_here`: the best first ports.",
            "inputSchema": { "type": "object", "properties": {
                "root": { "type": "string", "description": "Repository root." },
                "whole_app": { "type": "boolean", "description": "Plan a whole-app port: entry points and shape (cli, server, library), every dependency imported or declared with its crate and fit, pieces in port order (units that import each other together), and numbered decisions with a recommended answer for the grilling round." },
            }, "required": ["root"] },
        },
        {
            "name": "bridge",
            "description": "Scaffold a Rust crate for one unit: napi-rs or wasm-bindgen bindings in src/lib.rs, stubs to port in src/port.rs, UTF-16 helpers when the unit uses string offsets. Never overwrites an existing folder.",
            "inputSchema": { "type": "object", "properties": {
                "root": { "type": "string" },
                "unit": { "type": "string", "description": "Unit id from analyze." },
                "kind": { "type": "string", "enum": ["napi", "wasm"], "description": "Defaults to the plan's bridge." },
                "out": { "type": "string", "description": "Crate folder; defaults to <root>/rust/<name>." },
                "force": { "type": "boolean", "description": "Bridge a unit the plan marks blocked or keep." },
                "keep": { "type": "array", "items": { "type": "string" }, "description": "Exports to keep in the source language: `name` or `name: reason`." },
                "batch": { "type": "array", "items": { "type": "string" }, "description": "Batch classes, `function:param`: the function runs over many `param` values kept in Rust, on all cores, in one call. A `<Function>Index` struct in port.rs replaces the default per-item loop." },
            }, "required": ["root", "unit"] },
        },
        {
            "name": "refresh",
            "description": "Regenerate a crate's bindings (lib.rs, index.cjs, js.rs) from the signatures in src/port.rs, after changing them, adding any new batch classes. Never touches port.rs.",
            "inputSchema": { "type": "object", "properties": {
                "crate": crate_dir,
                "batch": { "type": "array", "items": { "type": "string" }, "description": "Batch classes to add, `function:param`: the function runs over many `param` values kept in Rust, on all cores, in one call. A `<Function>Index` struct in port.rs replaces the default per-item loop." },
            }, "required": ["crate"] },
        },
        {
            "name": "record",
            "description": "TRUSTED HOST EXECUTION (not sandboxed): run the original functions on generated edge-case inputs (non-ASCII, emoji, -0, NaN, ...) plus optional extra inputs, and save the results as cases.jsonl: the behavior the port must match.",
            "inputSchema": { "type": "object", "properties": {
                "crate": crate_dir,
                "count": { "type": "integer", "description": "Generated cases per function (default 200)." },
                "inputs": { "type": "string", "description": "JSONL file of extra cases: {\"fn\": name, \"args\": [...]}." },
                "module": { "type": "string", "description": "JS only: import the original from this JS module instead of the TS source." },
                "resolve_from": { "type": "string", "description": "Resolve npm packages from this folder's node_modules." },
                "float_tolerance": { "type": "number", "description": "Numeric code: floats match when they differ by at most this times the larger magnitude (absolute below 1), e.g. 1e-9. Saved with the crate for diff and gate; 0 removes it." },
                "test_command": { "type": "array", "items": { "type": "string" }, "description": "TS/JS or Python: also record every call the original's own test suite makes to the unit's functions, e.g. [\"npx\", \"jest\"] or [\"pytest\"]. Runs in the original's folder." },
            }, "required": ["crate"] },
        },
        {
            "name": "build",
            "description": "TRUSTED HOST EXECUTION (not sandboxed): build the crate (napi-rs .node, wasm-bindgen output, Python extension or shared library).",
            "inputSchema": { "type": "object", "properties": { "crate": crate_dir }, "required": ["crate"] },
        },
        {
            "name": "diff",
            "description": "TRUSTED HOST EXECUTION (not sandboxed): replay cases.jsonl against the built Rust module and list mismatches.",
            "inputSchema": { "type": "object", "properties": {
                "crate": crate_dir,
                "ignore_key_order": { "type": "boolean" },
                "float_tolerance": { "type": "number", "description": "Floats match within this tolerance for this run (the one saved by record otherwise)." },
            }, "required": ["crate"] },
        },
        {
            "name": "profile",
            "description": "TRUSTED HOST EXECUTION (not sandboxed): run a real workload (a command: node or python3 script, `go test -bench`, java, ruby) under the language's profiler and rank the repository's hot functions by inclusive time, with their callers, call counts and advice: what to port together, and which loops to batch (`bridge` with `batch`). Profile before porting: leaf functions called one at a time are slower in Rust.",
            "inputSchema": { "type": "object", "properties": {
                "root": { "type": "string" },
                "command": { "type": "array", "items": { "type": "string" }, "description": "The workload, run in root: [\"node\", \"bench.mjs\"]." },
                "lang": { "type": "string", "enum": ["ts", "python", "go", "java", "ruby"], "description": "Detected from the repository when omitted." },
            }, "required": ["root", "command"] },
        },
        {
            "name": "bench",
            "description": "TRUSTED HOST EXECUTION (not sandboxed): time the recorded cases on the original and on the built Rust module, bridge included; speedup below 1 means the port is slower as its callers call it. Returns the rows and a verdict with advice: ship, keep the slower functions, or port a bigger unit.",
            "inputSchema": { "type": "object", "properties": {
                "crate": crate_dir,
                "resolve_from": { "type": "string" },
            }, "required": ["crate"] },
        },
        {
            "name": "shadow",
            "description": "Generate a drop-in shadow (a TS or Python module, Go package, Ruby file, Java classes or C file) that runs the original and the Rust port side by side and reports mismatches; RUST_ANYTHING_MODE=old|shadow|new switches at runtime.",
            "inputSchema": { "type": "object", "properties": { "crate": crate_dir, "out": { "type": "string" } }, "required": ["crate"] },
        },
        {
            "name": "gate",
            "description": "TRUSTED HOST EXECUTION (not sandboxed): definition of done: fmt, clippy -D warnings, cargo test, unsafe with SAFETY comments only, no unimplemented functions, build, API present, recorded behavior matches.",
            "inputSchema": { "type": "object", "properties": { "crate": crate_dir }, "required": ["crate"] },
        },
        {
            "name": "app_record",
            "description": "Record the whole CLI app's behavior (HOST EXECUTION BY DEFAULT; set isolate=true for Linux Bubblewrap): run the original on its own documented examples, an inputs file and semantic traps (non-ASCII and emoji, negative division, overflow, float extremes, map order), twice each. What differs between runs is reported by kind in `noise`: values (now [..] wildcards), order (lines compared in any order) and lines (a ... that matches anything): show every one to the user. Cases are trycmd files in <repo>/rust/app-cases/ (argv, stdin, env, files in; stdout, stderr, exit code, files out).",
            "inputSchema": { "type": "object", "properties": {
                "repo": { "type": "string", "description": "Repository root." },
                "run": { "type": "array", "items": { "type": "string" }, "description": "Command that runs the original, paths relative to repo: [\"python3\", \"cli.py\"]. Optional after the first record." },
                "inputs": { "type": "string", "description": "JSONL of extra cases: {\"name\", \"args\": [...], \"stdin\", \"env\": {...}, \"files\": {path: text}}." },
                "normalize": { "type": "array", "items": { "type": "string" }, "description": "Normalizers: timestamps, uuids, key-order, or name=regex." },
                "name": { "type": "string", "description": "Name the app is invoked by in its docs." },
                "traps": { "type": "boolean", "description": "Add semantic-trap cases (default true)." },
                "http": { "type": "boolean", "description": "The app is a server: `run` starts it (it takes its port from PORT or {port}); cases are requests from curl lines in its docs, `inputs` ({\"name\", \"method\", \"path\", \"headers\", \"body\"}) and traps." },
                "port": { "type": "integer", "description": "A port the server always listens on (implies http)." },
                "float_tolerance": { "type": "number", "description": "Numeric output: numbers match when they differ by at most this times the larger magnitude (absolute below 1), e.g. 1e-9. Saved with the cases; 0 removes it." },
                "isolate": { "type": "boolean", "description": "Use Bubblewrap isolation (Linux only; fails closed if unavailable). Trusted host execution by default." },
                "isolate_network": { "type": "boolean", "description": "Share full host networking in isolated mode, required for HTTP; disabled by default." },
                "isolate_ro": { "type": "array", "items": { "type": "string" }, "description": "Explicit read-only host directories exposed at /mnt/ro/0, /mnt/ro/1, etc. (isolated mode only)." },
                "isolate_env": { "type": "array", "items": { "type": "string" }, "description": "Explicit NAME=VALUE environment entries (isolated mode only). No inherited host environment." },
            }, "required": ["repo"] },
        },
        {
            "name": "app_replay",
            "description": "Replay <repo>/rust/app-cases/ against a build (the original, front door, or final Rust binary). HOST EXECUTION BY DEFAULT; set isolate=true for Linux Bubblewrap. Reports passed/total, fallback rate and a diff per failing case (rust/app-cases/replay-report.md).",
            "inputSchema": { "type": "object", "properties": {
                "repo": { "type": "string", "description": "Repository root." },
                "build": { "type": "string", "description": "Command line of the build to test; the original when omitted." },
                "port": { "type": "integer", "description": "Servers: the port the build always listens on; otherwise it gets a free one through PORT and {port}." },
                "case": { "type": "string", "description": "Replay only this case, by the name the report gives it (a library: only this unit): the fast loop for fixing one mismatch." },
                "isolate": { "type": "boolean", "description": "Use Bubblewrap isolation (Linux only; fails closed if unavailable). Trusted host execution by default." },
                "isolate_network": { "type": "boolean", "description": "Share full host networking in isolated mode, required for HTTP; disabled by default." },
                "isolate_ro": { "type": "array", "items": { "type": "string" }, "description": "Explicit read-only host directories exposed at /mnt/ro/0, /mnt/ro/1, etc. (isolated mode only)." },
                "isolate_env": { "type": "array", "items": { "type": "string" }, "description": "Explicit NAME=VALUE environment entries (isolated mode only). No inherited host environment." },
            }, "required": ["repo"] },
        },
        {
            "name": "app_front_door",
            "description": "TRUSTED HOST EXECUTION (not sandboxed): scaffold <repo>/rust/app, the Rust binary that owns the CLI's entry point: a clap parser for the recorded commands and flags, every command running the original with the same argv, stdin and env (each fallback counted). For a server: an axum app that starts the original on a port of its own and proxies every route no unit answers. Builds it and replays the cases: 100% passed at 100% fallback. Needs app_record first; never overwrites rust/app.",
            "inputSchema": { "type": "object", "properties": {
                "repo": { "type": "string", "description": "Repository root." },
                "check": { "type": "boolean", "description": "Build and replay (default true)." },
                "library": { "type": "boolean", "description": "Treat the repository as a library even if it has a main: the front door is a Rust crate with the public API, re-exporting each wired unit's port. Found by default when there is no main and no server." },
            }, "required": ["repo"] },
        },
        {
            "name": "app_wire",
            "description": "TRUSTED HOST EXECUTION (not sandboxed): after a unit passes gate, hand its commands to its port.rs: adds the unit crate to rust/app as a library, writes a glue stub (src/units/<unit>.rs, yours to write: parse args, call <crate>::port, print what the original prints), regenerates the routing with a kill switch RUST_ANYTHING_UNIT_<NAME>=original, then builds and replays. Run it again after writing the glue: 100% passed and a lower fallback rate.",
            "inputSchema": { "type": "object", "properties": {
                "repo": { "type": "string", "description": "Repository root." },
                "unit": { "type": "string", "description": "Unit crate folder, its name under rust/, or the unit id it was bridged from." },
                "commands": { "type": "array", "items": { "type": "string" }, "description": "Commands the unit answers; kept from the last wire when omitted." },
                "routes": { "type": "array", "items": { "type": "string" }, "description": "Servers: routes the unit answers, axum syntax: \"GET /users/{id}\", \"/path\" for any method, \"*\" for everything else; kept from the last wire when omitted." },
                "skip_gate": { "type": "boolean", "description": "Do not run gate on the unit first." },
                "check": { "type": "boolean", "description": "Build and replay (default true)." },
            }, "required": ["repo", "unit"] },
        },
        {
            "name": "app_lift",
            "description": "TRUSTED HOST EXECUTION (not sandboxed): make rust/app a pure Rust app. Refuses (and lists every blocker) unless replay passes with fallback rate 0 and every unit passes gate. Then, on a copy first: removes the fallback and kill switches, turns each unit into a plain library (no cdylib, bindings or generated wrappers), adds a trycmd test running rust/app-cases on cargo test, checks fmt/clippy/tests, DECISIONS.md targets and assets, and replays the pure binary. Never touches the original.",
            "inputSchema": { "type": "object", "properties": {
                "repo": { "type": "string", "description": "Repository root." },
            }, "required": ["repo"] },
        },
    ])
}

fn call(name: &str, args: &Value) -> Result<Value> {
    let path = |key: &str| -> Result<PathBuf> {
        args[key]
            .as_str()
            .map(PathBuf::from)
            .with_context(|| format!("missing `{key}`"))
    };
    let opt_path = |key: &str| args[key].as_str().map(PathBuf::from);
    let strings = |key: &str| -> Vec<String> {
        args[key]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    let no_node_args: [String; 0] = [];
    Ok(match name {
        "analyze" => {
            let root = path("root")?;
            let mut report = analyze::analyze(&root)?;
            if args["whole_app"].as_bool().unwrap_or(false) {
                report.whole_app = Some(crate::whole_app::whole_app(&root, &report));
            }
            serde_json::to_value(report)?
        }
        "bridge" => {
            let kind = match args["kind"].as_str() {
                Some("napi") => Some(bridge::Kind::Napi),
                Some("wasm") => Some(bridge::Kind::Wasm),
                _ => None,
            };
            let root = path("root")?;
            serde_json::to_value(bridge::bridge(&bridge::Options {
                root: &root,
                unit: args["unit"].as_str().context("missing `unit`")?,
                kind,
                out: opt_path("out"),
                name: None,
                force: args["force"].as_bool().unwrap_or(false),
                keep: strings("keep"),
                lang: None,
                batch: strings("batch"),
            })?)?
        }
        "record" => {
            let crate_dir = path("crate")?;
            let mut inputs = opt_path("inputs");
            let test_command = strings("test_command");
            let captured = if test_command.is_empty() {
                None
            } else {
                let c = crate::from_tests::capture(&crate_dir, &test_command)?;
                inputs = Some(crate::from_tests::with_own_inputs(
                    &crate_dir,
                    inputs.as_deref(),
                    &c.inputs,
                )?);
                Some(serde_json::json!({
                    "per_function": c.per_function,
                    "not_captured": c.skipped,
                    "tests_passed": c.tests_passed,
                }))
            };
            let module = opt_path("module");
            let resolve_from = opt_path("resolve_from");
            if let Some(t) = args["float_tolerance"].as_f64() {
                let mut manifest = bridge::Manifest::load(&crate_dir)?;
                manifest.float_tolerance = (crate::tolerance::check(t)? > 0.0).then_some(t);
                manifest.save(&crate_dir)?;
            }
            let mut out = serde_json::to_value(harness::record(&harness::RecordOptions {
                crate_dir: &crate_dir,
                count: args["count"]
                    .as_u64()
                    .map_or(200, |n| usize::try_from(n).unwrap_or(200)),
                inputs: inputs.as_deref(),
                module: module.as_deref(),
                resolve_from: resolve_from.as_deref(),
                node_args: &no_node_args,
            })?)?;
            if let Some(c) = captured {
                out["from_tests"] = c;
            }
            out
        }
        "refresh" => {
            let batch = strings("batch");
            let crate_dir = path("crate")?;
            serde_json::to_value(if batch.is_empty() {
                bridge::refresh(&crate_dir)?
            } else {
                bridge::add_batches(&crate_dir, &batch)?
            })?
        }
        "profile" => {
            let command = strings("command");
            anyhow::ensure!(!command.is_empty(), "missing `command`");
            let lang = match args.get("lang") {
                Some(v) if !v.is_null() => {
                    Some(serde_json::from_value(v.clone()).context("`lang`")?)
                }
                _ => None,
            };
            serde_json::to_value(crate::profile::profile(&path("root")?, &command, lang)?)?
        }
        "build" => json!({ "artifact": harness::build(&path("crate")?)? }),
        "diff" => {
            let crate_dir = path("crate")?;
            serde_json::to_value(harness::diff(&harness::DiffOptions {
                crate_dir: &crate_dir,
                module: None,
                ignore_key_order: args["ignore_key_order"].as_bool().unwrap_or(false),
                node_args: &no_node_args,
                float_tolerance: args["float_tolerance"].as_f64(),
            })?)?
        }
        "bench" => {
            let crate_dir = path("crate")?;
            let resolve_from = opt_path("resolve_from");
            let rows = harness::bench(&harness::BenchOptions {
                crate_dir: &crate_dir,
                min_ms: 200,
                resolve_from: resolve_from.as_deref(),
                node_args: &no_node_args,
            })?;
            let lang = bridge::Manifest::load(&crate_dir)
                .map(|m| m.language)
                .unwrap_or_default();
            let verdict = harness::verdict(&rows, lang);
            serde_json::json!({ "rows": rows, "verdict": verdict })
        }
        "shadow" => {
            let out = opt_path("out");
            serde_json::to_value(shadow::shadow(&path("crate")?, out.as_deref())?)?
        }
        "gate" => serde_json::to_value(gate::gate(Path::new(&path("crate")?), &no_node_args)?)?,
        "app_record" => {
            let repo = path("repo")?;
            let inputs = opt_path("inputs");
            let isolate = isolation::Isolation::from_args(
                args["isolate"].as_bool().unwrap_or(false),
                args["isolate_network"].as_bool().unwrap_or(false),
                strings("isolate_ro").into_iter().map(PathBuf::from).collect(),
                &strings("isolate_env"),
            )?;
            serde_json::to_value(app::record_with_isolation(
                &app::RecordOptions {
                    repo: &repo,
                    run: &strings("run"),
                    inputs: inputs.as_deref(),
                    normalize: &strings("normalize"),
                    name: args["name"].as_str(),
                    traps: args["traps"].as_bool().unwrap_or(true),
                    timeout_secs: None,
                    http: args["http"].as_bool().unwrap_or(false),
                    port: args["port"].as_u64().and_then(|p| u16::try_from(p).ok()),
                    float_tolerance: args["float_tolerance"].as_f64(),
                },
                &isolate,
            )?)?
        }
        "app_replay" => {
            let repo = path("repo")?;
            let isolate = isolation::Isolation::from_args(
                args["isolate"].as_bool().unwrap_or(false),
                args["isolate_network"].as_bool().unwrap_or(false),
                strings("isolate_ro").into_iter().map(PathBuf::from).collect(),
                &strings("isolate_env"),
            )?;
            serde_json::to_value(app::replay_with_isolation(
                &app::ReplayOptions {
                    repo: &repo,
                    build: args["build"].as_str(),
                    timeout_secs: None,
                    port: args["port"].as_u64().and_then(|p| u16::try_from(p).ok()),
                    case: args["case"].as_str(),
                },
                &isolate,
            )?)?
        }
        "app_front_door" => {
            let repo = path("repo")?;
            serde_json::to_value(front_door::front_door(&front_door::FrontDoorOptions {
                repo: &repo,
                library: args["library"].as_bool().unwrap_or(false),
                check: args["check"].as_bool().unwrap_or(true),
            })?)?
        }
        "app_wire" => {
            let repo = path("repo")?;
            serde_json::to_value(front_door::wire(&front_door::WireOptions {
                repo: &repo,
                unit: args["unit"].as_str().context("missing `unit`")?,
                commands: &strings("commands"),
                routes: &strings("routes"),
                skip_gate: args["skip_gate"].as_bool().unwrap_or(false),
                check: args["check"].as_bool().unwrap_or(true),
            })?)?
        }
        "app_lift" => serde_json::to_value(lift::lift(&lift::LiftOptions {
            repo: &path("repo")?,
        })?)?,
        _ => anyhow::bail!("unknown tool `{name}`"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speaks_mcp() {
        let init = handle(&json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2025-06-18" } })).unwrap();
        assert_eq!(init["result"]["serverInfo"]["name"], "rust-anything");
        assert!(
            handle(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })).is_none()
        );
        let list = handle(&json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" })).unwrap();
        let names: Vec<&str> = list["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "analyze",
                "bridge",
                "refresh",
                "record",
                "build",
                "diff",
                "profile",
                "bench",
                "shadow",
                "gate",
                "app_record",
                "app_replay",
                "app_front_door",
                "app_wire",
                "app_lift"
            ]
        );
        let bad = handle(&json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": { "name": "gate", "arguments": {} } })).unwrap();
        assert_eq!(bad["result"]["isError"], true);
        let unknown = handle(&json!({ "jsonrpc": "2.0", "id": 4, "method": "nope" })).unwrap();
        assert_eq!(unknown["error"]["code"], -32601);
    }
}
