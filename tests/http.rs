//! HTTP servers end to end, on the same small server written in each of the six languages:
//! record and replay (a changed error message is caught), then the axum front door, wiring
//! every route to a Rust unit, the lift, and a replay of the pure binary with the original gone.
//! A language is skipped when its runtime is missing.

use std::path::{Path, PathBuf};
use std::process::Command;

use rust_anything::app::{self, RecordOptions, ReplayOptions};
use rust_anything::front_door::{self, FrontDoorOptions, WireOptions};
use rust_anything::lift::{self, LiftOptions};

fn available(program: &str, arg: &str) -> bool {
    Command::new(program)
        .arg(arg)
        .output()
        .is_ok_and(|o| o.status.success())
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap().flatten() {
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
}

fn fixture(lang: &str, phase: &str) -> PathBuf {
    let repo = std::env::temp_dir().join(format!(
        "rust-anything-http-{lang}-{phase}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&repo);
    copy_dir(
        &Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/http")
            .join(lang),
        &repo,
    );
    repo
}

fn sh(dir: &Path, program: &str, args: &[&str]) {
    let out = Command::new(program)
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{program} {args:?}: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn words(line: &str) -> Vec<String> {
    line.split_whitespace().map(str::to_string).collect()
}

fn record(repo: &Path, run: &str, traps: bool) -> app::RecordOutcome {
    app::record(&RecordOptions {
        repo,
        run: &words(run),
        inputs: None,
        normalize: &[],
        name: None,
        traps,
        timeout_secs: None,
        http: true,
        port: None,
        float_tolerance: None,
    })
    .unwrap()
}

fn replay(repo: &Path, build: Option<&str>) -> app::ReplayOutcome {
    app::replay(&ReplayOptions {
        case: None,
        repo,
        build,
        timeout_secs: None,
        port: None,
    })
    .unwrap()
}

#[test]
fn http_record_keeps_distinct_requests_with_the_same_name() {
    if !available("node", "--version") {
        return;
    }
    let repo = fixture("ts", "duplicate-inputs");
    let inputs = repo.join("inputs.jsonl");
    std::fs::write(
        &inputs,
        concat!(
            "{\"name\":\"request\",\"path\":\"/hello\"}\n",
            "{\"name\":\"request-003\",\"path\":\"/div/1/1\"}\n",
            "{\"name\":\"request\",\"path\":\"/div/2/1\"}\n",
        ),
    )
    .unwrap();
    std::fs::remove_file(repo.join("README.md")).unwrap();
    let out = app::record(&RecordOptions {
        repo: &repo,
        run: &words("node server.js"),
        inputs: Some(&inputs),
        normalize: &[],
        name: None,
        traps: false,
        timeout_secs: None,
        http: true,
        port: None,
        float_tolerance: None,
    })
    .unwrap();
    let repeated = replay(&repo, None);
    let mut recorded = std::fs::read_dir(&out.dir)
        .unwrap()
        .map(|r| r.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".toml"))
        .collect::<Vec<_>>();
    recorded.sort();
    std::fs::remove_dir_all(repo).unwrap();
    assert_eq!(out.cases, 3);
    assert_eq!(repeated.passed, 3, "{:?}", repeated.failures);
    assert_eq!(
        recorded,
        ["request-003.toml", "request-3.toml", "request.toml"]
    );
}

#[test]
fn http_record_does_not_persist_curl_files_outside_repository() {
    if !available("node", "--version") {
        return;
    }
    let repo = fixture("ts", "file-boundary");
    let outside = repo.with_extension("external-marker");
    std::fs::write(&outside, "PRIVATE-OUTSIDE-PAYLOAD").unwrap();
    std::fs::write(repo.join("payload.txt"), "allowed root body").unwrap();
    std::fs::create_dir_all(repo.join("docs")).unwrap();
    std::fs::write(repo.join("docs/payload.txt"), "allowed docs body").unwrap();

    let link = repo.join("escaped-link");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    #[cfg(windows)]
    let _ = std::os::windows::fs::symlink_file(&outside, &link);

    let readme = format!(
        "~~~sh\ncurl --data-binary @payload.txt http://localhost/upper\n\
         curl --data-binary @docs/payload.txt http://localhost/upper\n\
         curl --data-binary '@{}' http://localhost/upper\n\
         curl --data-binary '@../{}' http://localhost/upper\n\
         curl --data-binary @escaped-link http://localhost/upper\n~~~\n",
        outside.display(),
        outside.file_name().unwrap().to_string_lossy()
    );
    std::fs::write(repo.join("README.md"), readme).unwrap();
    let out = record(&repo, "node server.js", false);
    assert_eq!(out.examples, 2, "{out:?}");
    assert_eq!(out.cases, 2, "{out:?}");
    let cases = std::fs::read_dir(&out.dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "toml"))
        .map(|path| std::fs::read_to_string(path).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(cases.len(), 2);
    assert!(cases.iter().any(|case| case.contains("allowed root body")));
    assert!(cases.iter().any(|case| case.contains("allowed docs body")));
    assert!(cases.iter().all(|case| !case.contains("PRIVATE-OUTSIDE-PAYLOAD")));

    std::fs::remove_dir_all(repo).unwrap();
    std::fs::remove_file(outside).unwrap();
}

/// How to build and run one language's server, and a copy with a changed error message.
struct Lang {
    name: &'static str,
    runtime: (&'static str, &'static str),
    source: &'static str,
    compiled: bool,
    build: fn(&Path, &str, &str),
    run: fn(&str) -> String,
    original: &'static str,
}

fn no_build(_: &Path, _: &str, _: &str) {}

#[test]
fn http_rerecord_uses_the_existing_fixed_port() {
    if !available("node", "--version") {
        return;
    }
    let repo = fixture("ts", "saved-port");
    let port = rust_anything::http::free_port().unwrap();
    let server = repo.join("server.js");
    let text = std::fs::read_to_string(&server).unwrap();
    let condition = format!("if (Number(process.env.PORT) !== {port}) process.exit(2);");
    std::fs::write(&server, format!("{condition}\n{text}")).unwrap();
    let run = &words("node server.js");
    let record = |port| {
        app::record(&RecordOptions {
            repo: &repo,
            run,
            inputs: None,
            normalize: &[],
            name: None,
            traps: false,
            timeout_secs: None,
            http: true,
            port,
            float_tolerance: None,
        })
    };
    let first = record(Some(port));
    let second = record(None);
    std::fs::remove_dir_all(repo).unwrap();
    assert!(first.unwrap().cases > 0);
    assert!(second.unwrap().cases > 0);
}

/// The command that runs the server; `buggy` builds a copy whose division-by-zero message changed.
fn prepare(lang: &Lang, repo: &Path, buggy: bool) -> String {
    let mut source = lang.source.to_string();
    if buggy {
        let text = std::fs::read_to_string(repo.join(lang.source)).unwrap();
        assert!(text.contains("division by zero"));
        let text = text.replace("division by zero", "divide by zero");
        source = match lang.name {
            "go" => {
                let dir = repo.join("bug");
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::copy(repo.join("go.mod"), dir.join("go.mod")).unwrap();
                std::fs::write(dir.join("main.go"), text).unwrap();
                "bug".to_string()
            }
            "java" => {
                std::fs::create_dir_all(repo.join("bug")).unwrap();
                std::fs::write(repo.join("bug/Server.java"), text).unwrap();
                "bug/Server.java".to_string()
            }
            "csharp" => {
                let dir = repo.join("bug");
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::copy(repo.join("server.csproj"), dir.join("server.csproj")).unwrap();
                std::fs::write(dir.join("Program.cs"), text).unwrap();
                "bug".to_string()
            }
            _ => {
                let (stem, ext) = lang.source.rsplit_once('.').unwrap();
                let bug = format!("{stem}_bug.{ext}");
                std::fs::write(repo.join(&bug), text).unwrap();
                bug
            }
        };
    }
    if lang.compiled {
        let artifact = if buggy { "server-bug" } else { "server" };
        (lang.build)(repo, &source, artifact);
        (lang.run)(artifact)
    } else {
        (lang.run)(&source)
    }
}

const API_PORT: &str = "pub fn upper(text: &str) -> String {
    text.to_uppercase()
}

pub fn div(a: i64, b: i64) -> Option<(i64, i64)> {
    (b != 0).then(|| (a / b, a % b))
}
";

const API_GLUE: &str = "use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use api_rs::port;
use axum::body::Body;
use axum::extract::Request;
use axum::http::{StatusCode, header};
use axum::response::Response;

const TEXT: &str = \"text/plain; charset=utf-8\";

fn reply(status: StatusCode, content_type: &str, body: String) -> Response {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, content_type)
        .body(Body::from(body))
        .unwrap_or_default()
}

pub async fn handle(route: &str, params: HashMap<String, String>, req: Request) -> Response {
    match route {
        \"GET /hello\" => reply(
            StatusCode::OK,
            \"application/json\",
            \"{\\\"message\\\":\\\"hello\\\"}\".to_string(),
        ),
        \"POST /upper\" => {
            let body = axum::body::to_bytes(req.into_body(), usize::MAX)
                .await
                .unwrap_or_default();
            reply(
                StatusCode::OK,
                TEXT,
                port::upper(&String::from_utf8_lossy(&body)),
            )
        }
        \"GET /div/{a}/{b}\" => {
            let a = params.get(\"a\").and_then(|a| a.parse::<i64>().ok());
            let b = params.get(\"b\").and_then(|b| b.parse::<i64>().ok());
            match (a, b) {
                (Some(a), Some(b)) => match port::div(a, b) {
                    Some((q, r)) => reply(
                        StatusCode::OK,
                        TEXT,
                        format!(\"{a} / {b} = {q} remainder {r}\\n\"),
                    ),
                    None => reply(
                        StatusCode::BAD_REQUEST,
                        TEXT,
                        \"error: division by zero\\n\".to_string(),
                    ),
                },
                _ => reply(
                    StatusCode::BAD_REQUEST,
                    TEXT,
                    \"error: not a number\\n\".to_string(),
                ),
            }
        }
        \"GET /now\" => {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos());
            reply(StatusCode::OK, TEXT, format!(\"now: {nanos}\\n\"))
        }
        _ => reply(StatusCode::NOT_FOUND, TEXT, \"not found\\n\".to_string()),
    }
}
";

fn check_language(lang: &Lang) {
    let (program, arg) = lang.runtime;
    if !available(program, arg) {
        eprintln!("skipped: {program} not found");
        return;
    }

    let repo = fixture(lang.name, "replay");
    let run = prepare(lang, &repo, false);
    let buggy = prepare(lang, &repo, true);
    let recorded = record(&repo, &run, true);
    assert_eq!(recorded.examples, 6, "{recorded:?}");
    assert!(recorded.traps > 20, "{recorded:?}");
    assert_eq!(recorded.noisy, ["example-005-get-now"], "{recorded:?}");
    assert!(
        recorded.unstable.is_empty() && recorded.timed_out.is_empty(),
        "{recorded:?}"
    );
    let dir = app::cases_dir(&repo).join("http");
    assert_eq!(
        std::fs::read_to_string(dir.join("example-005-get-now.body")).unwrap(),
        "now: [..]\n"
    );
    let upper = std::fs::read_to_string(dir.join("example-002-post-upper.toml")).unwrap();
    assert!(upper.contains("body = \"hello world\""), "{upper}");
    assert!(upper.contains("status = 200"), "{upper}");
    assert!(
        upper.contains("content-type = \"text/plain; charset=utf-8\""),
        "{upper}"
    );
    assert!(
        std::fs::read_to_string(dir.join("example-006-get-missing.toml"))
            .unwrap()
            .contains("status = 404")
    );
    let original = replay(&repo, None);
    assert_eq!(original.passed, original.total, "{:?}", original.failures);
    let bug = replay(&repo, Some(&buggy));
    assert!(!bug.failures.is_empty());
    for f in &bug.failures {
        assert!(f.args[1].starts_with("/div/"), "{f:?}");
        assert!(f.problems.iter().all(|p| p.field == "body"), "{f:?}");
    }
    std::fs::remove_dir_all(&repo).unwrap();

    if !available("cargo", "--version") {
        return;
    }
    let repo = fixture(lang.name, "lift");
    let run = prepare(lang, &repo, false);
    record(&repo, &run, false);
    let door = front_door::front_door(&FrontDoorOptions {
        repo: &repo,
        library: false,
        check: true,
    })
    .unwrap();
    assert!(
        door.routes.contains(&"GET /div/7/2".to_string()),
        "{door:?}"
    );
    let replayed = door.replay.unwrap();
    assert_eq!(replayed.passed, replayed.total, "{:?}", replayed.failures);
    assert_eq!(replayed.fallback.unwrap().cases, replayed.total);

    let unit = repo.join("rust/api-rs");
    std::fs::create_dir_all(unit.join("src")).unwrap();
    std::fs::write(
        unit.join("Cargo.toml"),
        "[package]\nname = \"api-rs\"\nversion = \"0.1.0\"\nedition = \"2024\"\npublish = false\n\n[lib]\ncrate-type = [\"cdylib\", \"rlib\"]\n",
    )
    .unwrap();
    std::fs::write(unit.join("src/lib.rs"), "pub mod port;\n").unwrap();
    std::fs::write(unit.join("src/port.rs"), API_PORT).unwrap();
    let wire = |routes: &[&str], check: bool| {
        front_door::wire(&WireOptions {
            repo: &repo,
            unit: "api",
            commands: &[],
            routes: &routes.iter().map(|r| r.to_string()).collect::<Vec<_>>(),
            skip_gate: true,
            check,
        })
        .unwrap()
    };
    let routes = ["GET /hello", "POST /upper", "GET /div/{a}/{b}", "GET /now"];
    let stub = wire(&routes, false);
    assert!(stub.glue_pending);
    std::fs::write(&stub.glue, API_GLUE).unwrap();
    let partial = wire(&[], true).replay.unwrap();
    assert_eq!(partial.passed, partial.total, "{:?}", partial.failures);
    let fallback = partial.fallback.unwrap();
    assert_eq!(fallback.names, ["example-006-get-missing"], "{fallback:?}");
    let blocked = lift::lift(&LiftOptions { repo: &repo }).unwrap();
    assert!(
        blocked
            .blockers
            .iter()
            .any(|b| b.contains("still fall back") && b.contains("example-006-get-missing")),
        "{:?}",
        blocked.blockers
    );

    let mut all = routes.to_vec();
    all.push("*");
    let full = wire(&all, true).replay.unwrap();
    assert_eq!(full.passed, full.total, "{:?}", full.failures);
    assert_eq!(full.fallback.unwrap().cases, 0);
    sh(&repo.join("rust/app"), "cargo", &["fmt"]);
    sh(&unit, "cargo", &["fmt"]);

    let lifted = lift::lift(&LiftOptions { repo: &repo }).unwrap();
    assert!(lifted.lifted, "{:?}", lifted.blockers);
    assert!(
        lifted.checklist.iter().all(|i| i.mark != lift::Mark::Fail),
        "{:?}",
        lifted.checklist
    );
    assert!(
        lifted
            .checklist
            .iter()
            .any(|i| i.name == "replay in CI" && i.mark == lift::Mark::Warn)
    );
    let app_dir = repo.join(front_door::APP_DIR);
    assert!(!app_dir.join("src/original.rs").exists());
    let cargo = std::fs::read_to_string(app_dir.join("Cargo.toml")).unwrap();
    assert!(!cargo.contains("reqwest"), "{cargo}");
    let units = std::fs::read_to_string(app_dir.join("src/units/mod.rs")).unwrap();
    assert!(!units.contains("original"), "{units}");

    let hidden = repo.join(format!("{}.hidden", lang.original));
    std::fs::rename(repo.join(lang.original), &hidden).unwrap();
    let pure = replay(
        &repo,
        Some(&lifted.binary.as_ref().unwrap().to_string_lossy()),
    );
    assert_eq!(pure.passed, pure.total, "{:?}", pure.failures);
    assert!(pure.fallback.is_none());
    std::fs::rename(&hidden, repo.join(lang.original)).unwrap();
    std::fs::remove_dir_all(&repo).unwrap();
}

#[test]
fn ts_server() {
    check_language(&Lang {
        name: "ts",
        compiled: false,
        runtime: ("node", "--version"),
        source: "server.js",
        build: no_build,
        run: |s| format!("node {s}"),
        original: "server.js",
    });
}

#[test]
fn python_server() {
    check_language(&Lang {
        name: "python",
        compiled: false,
        runtime: ("python3", "--version"),
        source: "server.py",
        build: no_build,
        run: |s| format!("python3 {s}"),
        original: "server.py",
    });
}

#[test]
fn php_server() {
    check_language(&Lang {
        name: "php",
        compiled: false,
        runtime: ("php", "--version"),
        source: "server.php",
        build: no_build,
        run: |s| format!("php -S 127.0.0.1:{{port}} {s}"),
        original: "server.php",
    });
}

#[test]
fn ruby_server() {
    check_language(&Lang {
        name: "ruby",
        compiled: false,
        runtime: ("ruby", "--version"),
        source: "server.rb",
        build: no_build,
        run: |s| format!("ruby {s}"),
        original: "server.rb",
    });
}

#[test]
fn c_server() {
    check_language(&Lang {
        name: "c",
        compiled: true,
        runtime: ("cc", "--version"),
        source: "server.c",
        build: |repo, source, out| sh(repo, "cc", &["-O1", "-o", out, source]),
        run: |s| format!("./{s}"),
        original: "server",
    });
}

#[test]
fn go_server() {
    check_language(&Lang {
        name: "go",
        compiled: true,
        runtime: ("go", "version"),
        source: "main.go",
        build: |repo, source, out| {
            let dir = if source == "bug" {
                repo.join("bug")
            } else {
                repo.to_path_buf()
            };
            let out = repo.join(out);
            sh(&dir, "go", &["build", "-o", &out.to_string_lossy(), "."]);
        },
        run: |s| format!("./{s}"),
        original: "server",
    });
}

#[test]
fn java_server() {
    check_language(&Lang {
        name: "java",
        compiled: true,
        runtime: ("javac", "--version"),
        source: "Server.java",
        build: |repo, source, out| sh(repo, "javac", &["-d", out, source]),
        run: |s| format!("java -cp ./{s} Server"),
        original: "server",
    });
}

#[test]
fn csharp_server() {
    check_language(&Lang {
        name: "csharp",
        compiled: true,
        runtime: ("dotnet", "--version"),
        source: "Program.cs",
        build: |repo, source, out| {
            let dir = if source == "bug" {
                repo.join("bug")
            } else {
                repo.to_path_buf()
            };
            let out = repo.join(out);
            sh(
                &dir,
                "dotnet",
                &[
                    "build",
                    "-c",
                    "Release",
                    "--nologo",
                    "-v",
                    "q",
                    "-o",
                    &out.to_string_lossy(),
                ],
            );
        },
        run: |s| format!("dotnet {s}/server.dll --urls http://127.0.0.1:{{port}}"),
        original: "server",
    });
}
