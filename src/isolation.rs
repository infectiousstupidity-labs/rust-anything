//! Opt-in Linux Bubblewrap process isolation for app record/replay.
//! A disposable cwd by itself is not a security boundary.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
#[cfg(target_os = "linux")]
use std::process::Stdio;

use anyhow::{Context, Result, bail, ensure};

/// Trusted execution is the default for backwards compatibility. Enable
/// isolation explicitly for untrusted CLI apps and servers.
#[derive(Debug, Clone, Default)]
pub struct Isolation {
    pub enabled: bool,
    pub network: bool,
    pub read_only_mounts: Vec<PathBuf>,
    pub environment: BTreeMap<String, String>,
}

impl Isolation {
    pub fn from_args(
        enabled: bool,
        network: bool,
        read_only_mounts: Vec<PathBuf>,
        environment: &[String],
    ) -> Result<Self> {
        ensure!(
            enabled || (!network && read_only_mounts.is_empty() && environment.is_empty()),
            "--isolate-network, --isolate-ro and --isolate-env require --isolate"
        );
        ensure!(read_only_mounts.len() <= 16, "at most 16 extra read-only mounts");
        let mut vars = BTreeMap::new();
        for item in environment {
            let (name, value) = item
                .split_once('=')
                .context("--isolate-env expects NAME=VALUE")?;
            ensure!(
                valid_env_name(name),
                "invalid environment variable name {name:?}"
            );
            ensure!(
                !matches!(
                    name,
                    "PATH" | "HOME" | "PWD" | "TMPDIR" | "RUST_ANYTHING_FALLBACK_LOG" | "PORT"
                ),
                "isolation reserves environment variable {name}"
            );
            vars.insert(name.to_string(), value.to_string());
        }
        Ok(Self {
            enabled,
            network,
            read_only_mounts,
            environment: vars,
        })
    }

    /// Probe actual kernel namespace support before running the original code.
    /// This prevents a missing/blocked Bubblewrap from being mistaken for an
    /// ordinary recorded program error.
    pub fn validate(&self, repo: &Path) -> Result<()> {
        if !self.enabled {
            ensure!(
                !self.network && self.read_only_mounts.is_empty() && self.environment.is_empty(),
                "isolation options require isolated mode"
            );
            return Ok(());
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = repo;
            bail!(
                "--isolate requires Linux with working Bubblewrap; no fallback to host execution"
            );
        }
        #[cfg(target_os = "linux")]
        {
            let scratch = crate::app::temp_dir("isolate-probe")?;
            std::fs::create_dir(scratch.join("cwd"))?;
            let result = (|| -> Result<()> {
                let command = ["/usr/bin/true".to_string()];
                let mut cmd =
                    self.command(repo, &scratch, &command, Workdir::Work, &BTreeMap::new())?;
                let out = cmd.output().context(
                    "starting Bubblewrap (install bubblewrap and enable unprivileged user namespaces)",
                )?;
                ensure!(
                    out.status.success(),
                    "Bubblewrap cannot isolate this host (status {}): {}",
                    out.status,
                    String::from_utf8_lossy(&out.stderr).trim()
                );
                Ok(())
            })();
            let _ = std::fs::remove_dir_all(scratch);
            result
        }
    }

    /// Construct a Bubblewrap command. The only host mounts are the system
    /// runtime, the read-only repository, a private writable session, and
    /// operator-provided read-only directories.
    pub(crate) fn command(
        &self,
        repo: &Path,
        session: &Path,
        words: &[String],
        workdir: Workdir,
        extra_env: &BTreeMap<String, String>,
    ) -> Result<Command> {
        ensure!(self.enabled, "isolated command requires --isolate");
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (repo, session, words, workdir, extra_env);
            bail!("Bubblewrap isolation is supported only on Linux");
        }
        #[cfg(target_os = "linux")]
        {
            let root = repo
                .canonicalize()
                .with_context(|| format!("resolving project {}", repo.display()))?;
            let session = session
                .canonicalize()
                .with_context(|| format!("resolving private session {}", session.display()))?;
            let mut cmd = Command::new("bwrap");
            // Bubblewrap runs with an empty host environment, too. Only system
            // runtime paths are searched to locate bwrap itself.
            cmd.env_clear().env("PATH", "/usr/bin:/bin");
            cmd.args([
                "--die-with-parent",
                "--new-session",
                "--unshare-all",
                "--clearenv",
                "--dir",
                "/etc",
                "--dir",
                "/home",
                "--dir",
                "/mnt",
                "--dir",
                "/mnt/ro",
                "--dir",
                "/run",
                "--proc",
                "/proc",
                "--dev",
                "/dev",
                "--tmpfs",
                "/tmp",
            ]);
            if self.network {
                // Allow the host loopback interface for HTTP servers and,
                // explicitly, all other host-network access.
                cmd.arg("--share-net");
            }
            for path in ["/usr", "/bin", "/sbin", "/lib", "/lib64"] {
                if Path::new(path).exists() {
                    cmd.args(["--ro-bind", path, path]);
                }
            }
            for path in [
                "/etc/ld.so.cache",
                "/etc/ssl",
                "/etc/ca-certificates",
                "/etc/resolv.conf",
                "/etc/hosts",
                "/etc/nsswitch.conf",
            ] {
                if Path::new(path).exists() {
                    cmd.args(["--ro-bind", path, path]);
                }
            }
            cmd.arg("--ro-bind")
                .arg(&root)
                .arg("/project")
                .arg("--bind")
                .arg(&session)
                .arg("/session");
            for (i, mount) in self.read_only_mounts.iter().enumerate() {
                let source = mount
                    .canonicalize()
                    .with_context(|| format!("resolving --isolate-ro {}", mount.display()))?;
                ensure!(
                    source.is_dir(),
                    "--isolate-ro {} must refer to a directory",
                    mount.display()
                );
                cmd.arg("--ro-bind")
                    .arg(source)
                    .arg(format!("/mnt/ro/{i}"));
            }
            cmd.args([
                "--setenv",
                "PATH",
                "/usr/local/bin:/usr/bin:/bin",
                "--setenv",
                "HOME",
                "/home/sandbox",
                "--setenv",
                "TMPDIR",
                "/tmp",
                "--setenv",
                "LANG",
                "C.UTF-8",
            ]);
            let mut vars = self.environment.clone();
            vars.extend(extra_env.iter().map(|(key, value)| (key.clone(), value.clone())));
            for (key, value) in vars {
                ensure!(valid_env_name(&key), "invalid fixture environment key {key:?}");
                if matches!(
                    key.as_str(),
                    "PATH" | "HOME" | "PWD" | "TMPDIR" | "RUST_ANYTHING_FALLBACK_LOG"
                ) {
                    continue;
                }
                cmd.arg("--setenv").arg(key).arg(value);
            }
            // Inaccessible outside the private writable mount.
            cmd.args([
                "--setenv",
                "RUST_ANYTHING_FALLBACK_LOG",
                "/session/fallback.log",
                "--setenv",
                "PWD",
                match workdir {
                    Workdir::Work => "/session/cwd",
                    Workdir::Project => "/project",
                },
                "--chdir",
                match workdir {
                    Workdir::Work => "/session/cwd",
                    Workdir::Project => "/project",
                },
                "--",
            ]);
            let (program, arguments) = words.split_first().context("empty isolated command")?;
            cmd.arg(remap_repo_path(&root, program));
            for arg in arguments {
                cmd.arg(remap_repo_path(&root, arg));
            }
            cmd.stdin(Stdio::null());
            Ok(cmd)
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum Workdir {
    Work,
    Project,
}

fn valid_env_name(name: &str) -> bool {
    let mut chars = name.bytes();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == b'_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == b'_')
}

/// Preserve the old command's repo-local absolute path resolution, but mount
/// the repo at /project rather than exposing the surrounding host home.
fn remap_repo_path(repo: &Path, word: &str) -> String {
    let path = Path::new(word);
    let resolved = if path.is_absolute() {
        path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
    } else {
        // A bare program name is resolved through PATH. It is not a
        // repository-relative file unless a real project file exists.
        let candidate = repo.join(path);
        let Ok(real) = candidate.canonicalize() else {
            return word.to_string();
        };
        real
    };
    if let Ok(relative) = resolved.strip_prefix(repo) {
        return Path::new("/project")
            .join(relative)
            .to_string_lossy()
            .into_owned();
    }
    word.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isolation_options_are_explicit_and_validate_names() {
        assert!(Isolation::from_args(false, true, Vec::new(), &[]).is_err());
        assert!(Isolation::from_args(false, false, Vec::new(), &["X=1".into()]).is_err());
        assert!(Isolation::from_args(true, false, Vec::new(), &["PATH=/tmp".into()]).is_err());
        assert!(Isolation::from_args(true, false, Vec::new(), &["PWD=/secret".into()]).is_err());
        assert!(Isolation::from_args(true, false, Vec::new(), &["BAD-NAME=1".into()]).is_err());
        let config =
            Isolation::from_args(true, false, Vec::new(), &["MY_FLAG=allowed".into()]).unwrap();
        assert_eq!(config.environment["MY_FLAG"], "allowed");
        assert!(!config.network);
    }

    #[cfg(unix)]
    #[test]
    fn only_project_paths_are_remapped() {
        let repo = Path::new("/fictional/checkout");
        assert_eq!(remap_repo_path(repo, "/fictional/checkout/cli.py"), "/project/cli.py");
        assert_eq!(remap_repo_path(repo, "/usr/bin/python3"), "/usr/bin/python3");
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn isolation_refuses_unsupported_platforms() {
        let isolation = Isolation::from_args(true, false, Vec::new(), &[]).unwrap();
        let error = isolation.validate(Path::new(".")).err().unwrap().to_string();
        assert!(error.contains("Linux"), "{error}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn bubblewrap_restricts_files_environment_and_network() {
        use std::net::TcpListener;

        let repo = crate::app::temp_dir("isolation-tests").unwrap();
        let session = crate::app::temp_dir("isolation-session").unwrap();
        std::fs::create_dir(session.join("cwd")).unwrap();
        std::fs::write(repo.join("allowed.txt"), "ALLOWED").unwrap();
        let secret = repo.with_extension("private-marker");
        std::fs::write(&secret, "DO-NOT-READ").unwrap();

        let safe = Isolation::from_args(true, false, Vec::new(), &[]).unwrap();
        if let Err(err) = safe.validate(&repo) {
            eprintln!("Bubblewrap not supported on this runner: {err:#}; isolated mode refuses");
            std::fs::remove_dir_all(repo).unwrap();
            std::fs::remove_dir_all(session).unwrap();
            std::fs::remove_file(secret).unwrap();
            return;
        }
        let listen = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listen.local_addr().unwrap().port();
        let script = format!(
            "cat /project/allowed.txt; test ! -e '{}'; test -z \"$INJECTED_SECRET\"; \
             (echo test > /dev/tcp/127.0.0.1/{port}) >/dev/null 2>&1 && exit 90; exit 0",
            secret.display()
        );
        let command = vec!["/bin/bash".into(), "-c".into(), script];
        let mut cmd = safe
            .command(&repo, &session, &command, Workdir::Work, &BTreeMap::new())
            .unwrap();
        // The launcher environment is not automatically passed inside.
        cmd.env("INJECTED_SECRET", "DO-NOT-LEAK");
        let out = cmd.output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(out.stdout, b"ALLOWED");

        let permitted = Isolation::from_args(
            true,
            true,
            vec![secret.parent().unwrap().to_path_buf()],
            &["SAFE_FLAG=present".into()],
        )
        .unwrap();
        let word = format!(
            "test -n \"$SAFE_FLAG\" && test -r /mnt/ro/0/{} && \
             echo hi > /dev/tcp/127.0.0.1/{port}",
            secret.file_name().unwrap().to_string_lossy()
        );
        let command = vec!["/bin/bash".into(), "-c".into(), word];
        let result = permitted
            .command(&repo, &session, &command, Workdir::Work, &BTreeMap::new())
            .unwrap()
            .output()
            .unwrap();
        assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));

        std::fs::remove_dir_all(repo).unwrap();
        std::fs::remove_dir_all(session).unwrap();
        std::fs::remove_file(secret).unwrap();
    }
}
