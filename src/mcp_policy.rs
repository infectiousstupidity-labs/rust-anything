//! Filesystem and execution authorization for the stdio MCP server.
//! This is a server-side policy check, not a process/container sandbox.
use std::collections::{BTreeSet, VecDeque};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;

/// MCP tool capabilities. Every tool must be classified before dispatch.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Effects {
    pub write: bool,
    pub execute: bool,
    pub destructive: bool,
}

impl Effects {
    pub fn label(self) -> &'static str {
        if self.destructive {
            "DESTRUCTIVE + PROCESS EXECUTION + FILESYSTEM WRITE"
        } else if self.execute {
            "PROCESS EXECUTION + FILESYSTEM ACCESS"
        } else if self.write {
            "FILESYSTEM WRITE"
        } else {
            "READ ONLY"
        }
    }
}

/// Treat formatters, builds and test runners as process execution too.
pub(crate) fn effects(name: &str) -> Option<Effects> {
    let (write, execute, destructive) = match name {
        "analyze" => (false, false, false),
        "bridge" | "refresh" | "record" | "build" | "shadow" | "gate" | "app_record"
        | "app_replay" | "app_front_door" | "app_wire" => {
            (true, true, false)
        }
        "diff" | "profile" | "bench" => (true, true, false),
        "app_lift" => (true, true, true),
        _ => return None,
    };
    Some(Effects {
        write,
        execute,
        destructive,
    })
}

/// Default: restrict paths to the MCP process working directory and deny
/// subprocess execution. More roots and powers require server startup flags.
/// An explicit --unrestricted grant restores legacy host-level behavior.
#[derive(Debug, Clone)]
pub struct McpPolicy {
    roots: Vec<PathBuf>,
    allow_execute: bool,
    allow_destructive: bool,
    unrestricted: bool,
}

impl McpPolicy {
    pub fn new(
        roots: &[PathBuf],
        allow_execute: bool,
        allow_destructive: bool,
        unrestricted: bool,
    ) -> Result<Self> {
        if unrestricted {
            return Ok(Self {
                roots: Vec::new(),
                allow_execute: true,
                allow_destructive: true,
                unrestricted: true,
            });
        }
        ensure!(
            !allow_destructive || allow_execute,
            "--allow-destructive also requires --allow-exec"
        );
        let roots_requested_implicitly = roots.is_empty();
        let selected = if roots_requested_implicitly {
            vec![std::env::current_dir().context("reading MCP working directory")?]
        } else {
            roots.to_vec()
        };
        let roots = selected
            .iter()
            .map(|root| {
                let canonical = root
                    .canonicalize()
                    .with_context(|| format!("MCP --root {} does not exist", root.display()))?;
                ensure!(
                    canonical.is_dir(),
                    "MCP root {} is not a directory",
                    root.display()
                );
                Ok(canonical)
            })
            .collect::<Result<Vec<_>>>()?;
        // A globally registered MCP server may start in HOME or /. Never
        // silently interpret either as a safe project workspace.
        if roots.len() == 1 && selected.len() == 1 && roots_requested_implicitly {
            let root = &roots[0];
            let home = std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .and_then(|home| PathBuf::from(home).canonicalize().ok());
            ensure!(
                root.parent().is_some() && home.as_ref() != Some(root),
                "MCP working directory is too broad for an implicit workspace root;                  pass --root /absolute/path/to/project explicitly"
            );
        }
        Ok(Self {
            roots,
            allow_execute,
            allow_destructive,
            unrestricted: false,
        })
    }

    pub fn summary(&self) -> String {
        if self.unrestricted {
            return "UNRESTRICTED: all paths, subprocesses and destructive tools allowed; host permissions apply".into();
        }
        format!(
            "RESTRICTED MCP: allowed roots [{}], process execution {}, destructive operations {}. \
             Permissions are enforced by the server; host approval UIs are additional safeguards.",
            self.roots
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", "),
            if self.allow_execute { "enabled" } else { "DENIED" },
            if self.allow_destructive { "enabled" } else { "DENIED" }
        )
    }

    pub fn authorize(&self, name: &str, args: &Value) -> Result<()> {
        let action = effects(name).with_context(|| format!("unsupported MCP tool {name:?}"))?;
        if self.unrestricted {
            return Ok(());
        }
        ensure!(
            !action.execute || self.allow_execute,
            "MCP tool {name} can execute subprocesses; restart the MCP server with --allow-exec \
             for a trusted workspace, or use external OS isolation"
        );
        ensure!(
            !action.destructive || self.allow_destructive,
            "MCP tool {name} may delete or replace files; explicit --allow-destructive is required"
        );
        ensure!(args.is_object(), "MCP tool arguments must be an object");

        let key = match name {
            "analyze" | "bridge" | "profile" => "root",
            "app_record" | "app_replay" | "app_front_door" | "app_wire" | "app_lift" => "repo",
            _ => "crate",
        };
        let selected = args[key]
            .as_str()
            .with_context(|| format!("missing or invalid {key:?} path"))?;
        let main = PathBuf::from(selected);
        self.path(&main, key, false)?;

        // Paths supplied in secondary arguments are subject to the same grants.
        for key in ["out", "inputs", "module", "resolve_from"] {
            if let Some(value) = args.get(key).filter(|v| !v.is_null()) {
                let text = value
                    .as_str()
                    .with_context(|| format!("{key} must be a path string"))?;
                self.path(Path::new(text), key, false)?;
            }
        }
        if let Some(value) = args.get("isolate_ro").filter(|v| !v.is_null()) {
            let mounts = value.as_array().context("isolate_ro must be an array")?;
            for mount in mounts {
                let path = mount.as_str().context("isolate_ro entries must be paths")?;
                self.path(Path::new(path), "isolate_ro", false)?;
            }
        }

        // Check code-generated and recorded output directories, even if a
        // symlink hides an escape inside an otherwise allowed project path.
        if action.write {
            match name {
                "bridge" => {
                    self.path(&main.join("rust"), "bridge rust output", true)?;
                }
                "app_record" | "app_replay" => {
                    let destination = main.join("rust/app-cases");
                    self.path(&destination, "app cases output", true)?;
                    self.check_symlink_tree(&destination)?;
                }
                "app_front_door" | "app_wire" | "app_lift" => {
                    let destination = main.join("rust/app");
                    self.path(&destination, "app output", true)?;
                    self.check_symlink_tree(&destination)?;
                }
                _ => {}
            }
        }
        if matches!(
            name,
            "app_replay" | "app_front_door" | "app_wire" | "app_lift"
        ) {
            self.front_door_state(&main)?;
        }
        if key == "crate" {
            self.manifest(&main)?;
            if action.write {
                for directory in [
                    "src",
                    "target",
                    ".rust-anything",
                    "shadow",
                    "go",
                    "python",
                    "java",
                    "ruby",
                ] {
                    let destination = main.join(directory);
                    self.path(&destination, "generated crate output", true)?;
                    if directory != "target" {
                        self.check_symlink_tree(&destination)?;
                    }
                }
            }
        }

        if matches!(name, "bridge" | "app_wire") {
            let unit = args["unit"].as_str().context("missing or invalid unit")?;
            ensure!(!unit.is_empty(), "unit cannot be empty");
            if Path::new(unit)
                .components()
                .any(|part| matches!(part, Component::ParentDir))
            {
                bail!("unit path traversal is forbidden");
            }
            if unit.contains('/') || unit.contains('\\') || Path::new(unit).is_absolute() {
                let base = if Path::new(unit).is_absolute() {
                    PathBuf::from(unit)
                } else {
                    main.join(unit)
                };
                self.path(&base, "unit", false)?;
            }
        }
        // A direct command line is *never* a grant to invoke arbitrary code:
        // it can only reach the executor if --allow-exec was supplied above.
        for key in ["command", "run", "test_command"] {
            if let Some(value) = args.get(key).filter(|v| !v.is_null()) {
                let parts = value
                    .as_array()
                    .with_context(|| format!("{key} must be an array of command strings"))?;
                ensure!(
                    parts.iter().all(|p| p.as_str().is_some()),
                    "{key} must contain only strings"
                );
                ensure!(
                    !parts.iter().any(|p| p.as_str().is_some_and(str::is_empty)),
                    "{key} cannot contain empty command arguments"
                );
            }
        }
        Ok(())
    }

    /// Allow symlink-free generated destinations and symlinks resolving inside
    /// the granted workspace. Nonexistent path tails are checked by resolving
    /// the nearest existing ancestor. Explicit user paths reject '..' upfront.
    fn path(&self, path: &Path, field: &str, internal: bool) -> Result<PathBuf> {
        ensure!(!path.as_os_str().is_empty(), "{field}: path cannot be empty");
        if !internal {
            ensure!(
                !path.components().any(|c| matches!(c, Component::ParentDir)),
                "{field}: parent-directory traversal is forbidden: {}",
                path.display()
            );
        }
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .context("reading MCP working directory")?
                .join(path)
        };
        let resolved = canonicalize_with_missing_tail(&absolute)
            .with_context(|| format!("resolving {field} {}", path.display()))?;
        ensure!(
            self.roots.iter().any(|root| resolved.starts_with(root)),
            "{field}: {} resolves outside allowed MCP roots; grant an additional --root explicitly",
            path.display()
        );
        Ok(resolved)
    }


    /// Catch nested output symlinks before entering a workflow that writes
    /// generated files. Follow permitted in-root links without revisiting directories.
    /// Large/opaque trees fail closed rather than silently skipping entries.
    fn check_symlink_tree(&self, directory: &Path) -> Result<()> {
        let metadata = match std::fs::symlink_metadata(directory) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(error).with_context(|| format!("checking {}", directory.display()));
            }
        };
        let root = self.path(directory, "generated output directory", true)?;
        if !metadata.is_dir() && !metadata.file_type().is_symlink() {
            return Ok(());
        }
        let mut pending = VecDeque::from([root]);
        let mut visited = BTreeSet::new();
        let mut seen = 0usize;
        while let Some(dir) = pending.pop_front() {
            let real = self.path(&dir, "generated output directory", true)?;
            if !real.is_dir() || !visited.insert(real.clone()) {
                continue;
            }
            for entry in std::fs::read_dir(&real)
                .with_context(|| format!("scanning MCP output directory {}", real.display()))?
            {
                let entry = entry?;
                seen += 1;
                ensure!(
                    seen <= 100_000,
                    "MCP output directory {} contains too many entries to validate safely",
                    directory.display()
                );
                let kind = entry.file_type()?;
                if kind.is_symlink() {
                    let target = self.path(&entry.path(), "nested output symlink", true)?;
                    if target.is_dir() {
                        pending.push_back(target);
                    }
                } else if kind.is_dir() {
                    pending.push_back(entry.path());
                }
            }
        }
        Ok(())
    }

    /// Saved front-door state also carries crate paths that may be dereferenced
    /// or copied by wire/lift, independent of the direct MCP request.
    fn front_door_state(&self, repo: &Path) -> Result<()> {
        let app_dir = repo.join(crate::front_door::APP_DIR);
        let config = app_dir.join(crate::front_door::STATE_FILE);
        if !config.exists() && !config.is_symlink() {
            return Ok(());
        }
        self.path(&config, "front-door state", true)?;
        let state = crate::front_door::State::load(repo)?;
        for unit in &state.units {
            ensure!(
                !unit.crate_name.is_empty()
                    && unit
                        .crate_name
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-'),
                "front-door unit crate_name must be a safe ASCII crate identifier"
            );
            self.path(&app_dir.join(&unit.path), "front-door wired crate", true)?;
            if let Some(shadow) = &unit.shadow {
                self.path(Path::new(shadow), "front-door shadow", true)?;
            }
        }
        Ok(())
    }

    /// A crate manifest may contain an absolute root and relative source files
    /// that are not visible in the direct MCP request.
    fn manifest(&self, crate_dir: &Path) -> Result<()> {
        let config = crate_dir.join(crate::bridge::MANIFEST);
        if !config.exists() && !config.is_symlink() {
            return Ok(());
        }
        self.path(&config, "crate manifest", true)?;
        let manifest = crate::bridge::Manifest::load(crate_dir)?;
        ensure!(
            !manifest.crate_name.is_empty()
                && manifest
                    .crate_name
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-'),
            "manifest crate_name must be a safe ASCII crate identifier"
        );
        let root = self.path(&manifest.root, "manifest.root", true)?;
        for file in manifest
            .exports
            .iter()
            .map(|item| item.file.as_str())
            .chain(manifest.classes.iter().map(|item| item.file.as_str()))
            .chain(manifest.js_only.iter().map(|item| item.file.as_str()))
        {
            ensure!(
                !Path::new(file).is_absolute(),
                "manifest source file must be relative: {file}"
            );
            self.path(&root.join(file), "manifest source", true)?;
        }
        Ok(())
    }
}

/// Resolve symlinks in every existing component, then append missing leaf
/// components. A dangling symlink is rejected rather than treated as missing.
fn canonicalize_with_missing_tail(path: &Path) -> Result<PathBuf> {
    let mut cursor = path.to_path_buf();
    let mut missing = VecDeque::new();
    loop {
        match std::fs::symlink_metadata(&cursor) {
            Ok(_) => {
                let mut resolved = cursor
                    .canonicalize()
                    .with_context(|| format!("resolving symlink at {}", cursor.display()))?;
                for part in missing {
                    resolved.push(part);
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let part = cursor
                    .file_name()
                    .context("cannot resolve path above filesystem root")?
                    .to_os_string();
                missing.push_front(part);
                cursor = cursor
                    .parent()
                    .context("cannot resolve path parent")?
                    .to_path_buf();
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("checking {}", cursor.display()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sandbox(name: &str) -> PathBuf {
        static NEXT: std::sync::atomic::AtomicUsize =
            std::sync::atomic::AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "rust-anything-mcp-policy-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn denies_outside_paths_and_parent_traversal() {
        let root = sandbox("paths");
        let outside = sandbox("external");
        let policy = McpPolicy::new(&[root.clone()], true, false, false).unwrap();
        policy
            .authorize("bridge", &json!({"root":root,"unit":"src/lib.rs"}))
            .unwrap();
        assert!(policy.authorize("analyze", &json!({"root":outside})).is_err());
        assert!(policy.authorize(
            "bridge",
            &json!({"root":root,"unit":"src/lib.rs","out":outside.join("escaped")})
        ).is_err());
        assert!(policy.authorize(
            "shadow",
            &json!({"crate":root,"out":"../../elsewhere"})
        ).is_err());
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    fn additional_workspace_root_is_an_explicit_path_grant() {
        let root = sandbox("primary-grant");
        let outside = sandbox("secondary-grant");
        let restricted = McpPolicy::new(&[root.clone()], true, false, false).unwrap();
        assert!(restricted
            .authorize(
                "bridge",
                &json!({"root":root,"unit":"src/index.ts","out":outside.join("port")}),
            )
            .is_err());
        let granted = McpPolicy::new(&[root.clone(), outside.clone()], true, false, false)
            .unwrap();
        assert!(granted
            .authorize(
                "bridge",
                &json!({"root":root,"unit":"src/index.ts","out":outside.join("port")}),
            )
            .is_ok());
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    fn in_repository_operations_with_an_explicit_exec_grant_are_allowed() {
        let repo = sandbox("ordinary-workflow");
        let crate_dir = repo.join("rust/component");
        std::fs::create_dir_all(crate_dir.join("src")).unwrap();
        let policy = McpPolicy::new(&[repo.clone()], true, false, false).unwrap();
        for (tool, arguments) in [
            ("bridge", json!({"root":repo,"unit":"src/module.ts"})),
            ("refresh", json!({"crate":crate_dir})),
            ("app_record", json!({"repo":repo,"run":["python3","cli.py"]})),
            ("app_replay", json!({"repo":repo,"build":"python3 cli.py"})),
            ("shadow", json!({"crate":crate_dir,"out":crate_dir.join("shadow/example.ts")})),
        ] {
            assert!(policy.authorize(tool, &arguments).is_ok(), "{tool}");
        }
        std::fs::remove_dir_all(repo).unwrap();
    }

    #[test]
    fn front_door_state_paths_cannot_escape_the_workspace() {
        let root = sandbox("front-door");
        let other = sandbox("front-door-external");
        let state = root.join(crate::front_door::APP_DIR);
        std::fs::create_dir_all(&state).unwrap();
        let policy = McpPolicy::new(&[root.clone()], true, true, false).unwrap();
        std::fs::write(
            state.join(crate::front_door::STATE_FILE),
            serde_json::json!({
                "schema": "rust-anything/front-door@1",
                "name": "test",
                "commands": [],
                "units": [{
                    "name":"unit",
                    "crate":"unit",
                    "path":other,
                    "commands":[],
                    "switch":"RUST_ANYTHING_UNIT_UNIT"
                }]
            })
            .to_string(),
        )
        .unwrap();
        assert!(policy.authorize("app_lift", &json!({"repo":root})).is_err());

        std::fs::write(
            state.join(crate::front_door::STATE_FILE),
            serde_json::json!({
                "schema": "rust-anything/front-door@1",
                "name": "test",
                "commands": [],
                "units": [{
                    "name":"unit",
                    "crate":"unit",
                    "path":"../unit",
                    "commands":[],
                    "switch":"RUST_ANYTHING_UNIT_UNIT"
                }]
            })
            .to_string(),
        )
        .unwrap();
        assert!(policy.authorize("app_lift", &json!({"repo":root})).is_ok());
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(other).unwrap();
    }

    #[test]
    fn process_and_destructive_tools_need_independent_grants() {
        let root = sandbox("powers");
        let default = McpPolicy::new(&[root.clone()], false, false, false).unwrap();
        assert!(default.authorize("analyze", &json!({"root":root})).is_ok());
        for tool in ["bridge", "record", "profile", "gate", "app_record", "app_replay", "app_lift"] {
            let args = json!({"root":root,"crate":root,"repo":root});
            assert!(default.authorize(tool, &args).is_err(), "{tool}");
        }
        let exec = McpPolicy::new(&[root.clone()], true, false, false).unwrap();
        assert!(exec.authorize("app_lift", &json!({"repo":root})).is_err());
        assert!(exec.authorize(
            "profile",
            &json!({"root":root,"command":["/bin/sh","-c","touch /tmp/outside"]})
        ).is_ok()); // Explicit execution grant is host execution, not a sandbox.
        let full = McpPolicy::new(&[root.clone()], true, true, false).unwrap();
        assert!(full.authorize("app_lift", &json!({"repo":root})).is_ok());
        assert!(default.authorize("unknown", &json!({"root":root})).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn invalid_command_arguments_are_rejected() {
        let root = sandbox("commands");
        let policy = McpPolicy::new(&[root.clone()], true, false, false).unwrap();
        assert!(policy.authorize("profile", &json!({"root":root,"command":["sh",123]})).is_err());
        assert!(policy.authorize("app_record", &json!({"repo":root,"run":["python3",""]})).is_err());
        assert!(policy.authorize("profile", &json!({"root":root,"command":["sh","-c","echo ok"]})).is_ok());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn rejects_directory_and_manifest_symlink_escapes() {
        use std::os::unix::fs::symlink;

        let root = sandbox("symlink");
        let outside = sandbox("private");
        symlink(&outside, root.join("rust")).unwrap();
        let policy = McpPolicy::new(&[root.clone()], true, false, false).unwrap();
        assert!(policy.authorize(
            "bridge",
            &json!({"root":root,"unit":"src/index.ts"})
        ).is_err());
        std::fs::remove_file(root.join("rust")).unwrap();

        std::fs::create_dir_all(root.join("rust/sample/src")).unwrap();
        symlink(&outside, root.join("rust/sample/target")).unwrap();
        assert!(policy.authorize("build", &json!({"crate":root.join("rust/sample")})).is_err());
        std::fs::remove_file(root.join("rust/sample/target")).unwrap();

        std::fs::write(outside.join("outside.txt"), "secret").unwrap();
        symlink(outside.join("outside.txt"), root.join("rust/sample/src/port.rs")).unwrap();
        assert!(policy.authorize(
            "refresh",
            &json!({"crate":root.join("rust/sample")})
        ).is_err());
        std::fs::remove_file(root.join("rust/sample/src/port.rs")).unwrap();

        // Follow in-root symlinks to their real directory when checking
        // nested entries: the second symlink must still be rejected.
        std::fs::create_dir_all(root.join("shared-output")).unwrap();
        symlink(&root.join("shared-output"), root.join("rust/sample/src/linked")).unwrap();
        symlink(outside.join("outside.txt"), root.join("shared-output/escape.txt")).unwrap();
        assert!(policy.authorize(
            "refresh",
            &json!({"crate":root.join("rust/sample")})
        ).is_err());
        std::fs::remove_file(root.join("rust/sample/src/linked")).unwrap();
        std::fs::remove_file(root.join("shared-output/escape.txt")).unwrap();

        std::fs::write(
            root.join("rust/sample/rust-anything.json"),
            format!(r#"{{"schema":"rust-anything/bridge@1","root":{:?},"unit":"x","language":"ts","kind":"napi","crate_name":"demo","exports":[],"js_only":[]}}"#, outside.to_string_lossy()),
        )
        .unwrap();
        crate::bridge::Manifest::load(&root.join("rust/sample")).unwrap();
        assert!(policy.authorize("build", &json!({"crate":root.join("rust/sample")})).is_err());

        // A hostile manifest must not manufacture output paths through
        // an unsanitized crate_name, even when the manifest root is allowed.
        std::fs::write(
            root.join("rust/sample/rust-anything.json"),
            format!(
                r#"{{"schema":"rust-anything/bridge@1","root":{:?},"unit":"x","language":"ts","kind":"napi","crate_name":"../../private","exports":[],"js_only":[]}}"#,
                root.to_string_lossy()
            ),
        )
        .unwrap();
        assert!(policy.authorize("shadow", &json!({"crate":root.join("rust/sample")})).is_err());
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(outside).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn rejects_windows_directory_symlink_escape_when_available() {
        use std::os::windows::fs::symlink_dir;
        let root = sandbox("windows-symlink");
        let outside = sandbox("windows-external");
        if symlink_dir(&outside, root.join("rust")).is_ok() {
            let policy = McpPolicy::new(&[root.clone()], true, false, false).unwrap();
            assert!(policy.authorize(
                "bridge",
                &json!({"root":root,"unit":"index.ts"})
            ).is_err());
        }
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(outside).unwrap();
    }
}
