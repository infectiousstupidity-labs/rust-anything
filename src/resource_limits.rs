//! Resource bounds for processes launched while recording or replaying.
//! Linux RLIMIT_AS is inherited by descendants; it is a per-process address-space
//! bound, NOT a combined cgroup memory bound. Other OSes require external isolation.
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, ensure};

#[derive(Debug, Clone, Copy)]
pub(crate) struct RecordingLimits {
    pub case: Duration,
    pub startup: Duration,
    pub overall: Duration,
    pub memory_mib: u64,
}

impl RecordingLimits {
    pub(crate) fn from_env() -> Result<Self> {
        Ok(Self {
            case: seconds("RUST_ANYTHING_CASE_TIMEOUT", 10.0)?,
            startup: seconds("RUST_ANYTHING_STARTUP_TIMEOUT", 15.0)?,
            overall: seconds("RUST_ANYTHING_RECORD_TIMEOUT", 600.0)?,
            memory_mib: memory_mib()?,
        })
    }
}

fn seconds(key: &str, default: f64) -> Result<Duration> {
    let value = match std::env::var(key) {
        Ok(text) => text
            .parse::<f64>()
            .with_context(|| format!("{key} must be a number of seconds"))?,
        Err(std::env::VarError::NotPresent) => default,
        Err(error) => return Err(error).with_context(|| format!("reading {key}")),
    };
    ensure!(
        value.is_finite() && (0.05..=86_400.0).contains(&value),
        "{key} must be a finite number of seconds from 0.05 to 86400"
    );
    Ok(Duration::from_secs_f64(value))
}

/// RLIMIT_AS is measured in KiB. 8192 MiB allows typical language runtimes
/// while rejecting enormous allocations. Override for large JVM/Node workloads.
pub(crate) fn memory_mib() -> Result<u64> {
    let key = "RUST_ANYTHING_MEMORY_MIB";
    let value = match std::env::var(key) {
        Ok(value) => value
            .parse::<u64>()
            .with_context(|| format!("{key} must be an integer number of MiB"))?,
        Err(std::env::VarError::NotPresent) => 8192,
        Err(error) => return Err(error).with_context(|| format!("reading {key}")),
    };
    ensure!(
        (128..=65536).contains(&value),
        "{key} must be between 128 and 65536 MiB"
    );
    Ok(value)
}

/// Apply an inherited Linux address-space limit without shell interpolation of
/// the executable or arguments. Input commands can already contain env removals
/// and an explicit cwd; keep them intact when wrapping.
pub(crate) fn bound_memory(
    command: Command,
    memory_mib: u64,
    clear_env: bool,
) -> Result<Command> {
    ensure!(
        (128..=65536).contains(&memory_mib),
        "memory limit must be between 128 and 65536 MiB"
    );
    #[cfg(target_os = "linux")]
    {
        let kb = memory_mib
            .checked_mul(1024)
            .context("memory limit overflow")?;
        let mut bounded = Command::new("/bin/sh");
        if clear_env {
            bounded.env_clear();
        }
        // Only the validated integer is inserted. "$@" preserves the original argv.
        bounded
            .arg("-c")
            .arg(format!("ulimit -v {kb} || exit 125; exec \"$@\""))
            .arg("rust-anything-memory-limit")
            .arg(command.get_program())
            .args(command.get_args());
        if let Some(cwd) = command.get_current_dir() {
            bounded.current_dir(cwd);
        }
        for (key, value) in command.get_envs() {
            match value {
                Some(value) => {
                    bounded.env(key, value);
                }
                None => {
                    bounded.env_remove(key);
                }
            }
        }
        Ok(bounded)
    }
    #[cfg(not(target_os = "linux"))]
    {
        // Timing/process-tree protections still apply, but address-space
        // enforcement on this host requires an external VM/container.
        let _ = clear_env;
        Ok(command)
    }
}

/// Make Unix descendants share a process group for group termination.
pub(crate) fn configure_tree(command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    #[cfg(not(unix))]
    let _ = command;
}

/// Kill the whole accessible process tree before reaping the direct child.
/// On Unix, the process group remains addressable after its leader exits.
pub(crate) fn terminate_tree(child: &mut Child) {
    #[cfg(unix)]
    {
        let _ = Command::new("/bin/kill")
            .args(["-KILL", "--", &format!("-{}", child.id())])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/F", "/T", "/PID", &child.id().to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
    let _ = child.wait();
}

pub(crate) fn likely_memory_exhaustion(stderr: &str, status: &std::process::ExitStatus) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("memoryerror")
        || lower.contains("out of memory")
        || lower.contains("cannot allocate memory")
        || lower.contains("allocation failed")
        || status.code() == Some(137)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_finite_positive_seconds() {
        assert_eq!(
            seconds("RUST_ANYTHING_TEST_UNSET_LIMIT", 0.2).unwrap(),
            Duration::from_millis(200)
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn memory_wrapper_keeps_env_clear_semantics() {
        let mut original = Command::new("/bin/sh");
        original.env_clear().env("ONLY_THIS", "yes");
        let mut bounded = bound_memory(original, 128, true).unwrap();
        bounded.arg("-c").arg(
            "test \"$ONLY_THIS\" = yes && test -z \"$HOME\" && test -z \"$SSH_AUTH_SOCK\"",
        );
        let output = bounded.output().unwrap();
        assert!(output.status.success(), "{output:?}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn memory_limit_is_inherited_by_the_executed_process() {
        let command = Command::new("/bin/sh");
        let mut cmd = bound_memory(command, 128, false).unwrap();
        cmd.arg("-c")
            .arg("test \"$(ulimit -v)\" -eq 131072");
        let output = cmd.output().unwrap();
        assert!(output.status.success(), "{output:?}");
    }
}
