# Security policy

## Supported versions

Security fixes go into the latest release. Upgrade with `cargo install rust-anything` or
`npm i -g rust-anything` before reporting, in case it is already fixed.

| Version | Supported |
| --- | --- |
| Latest minor (1.x) | Yes |
| Older | No |

## Reporting a vulnerability

Please report it privately, not in a public issue: use
[Report a vulnerability](https://github.com/erkamyaman/rust-anything/security/advisories/new) on
the repository's Security tab.

Include what you found, how to reproduce it, which version and platform you used, and what an
attacker could do with it. You'll get a reply within a few days, and you'll hear when a fix is out.
If you'd like credit in the release notes, say how you want to be named.

## What counts

rust-anything runs your code on purpose. `record`, `diff`, `app record` and `app replay` execute
the original app and the Rust port to compare them, so running them on an untrusted repository is
like running that repository yourself. That alone is not a vulnerability.

These are in scope:

- Code that rust-anything generates (bindings, drivers, front doors) with memory-unsafe or
  undefined behavior that the port didn't ask for.
- Reading or writing files outside the project, the crate and the system temp folder.
- A crafted recording (`cases.jsonl`, `rust/app-cases/`) that makes rust-anything run commands it
  wasn't told to run.
- The release pipeline: the published binaries, the npm package or the crates.io crate not
  matching the tagged source.
- The MCP server doing more than the tool call asked for.

## Executing untrusted projects

The default execution mode is **trusted host execution**. Commands that run original
applications, test suites, builds or profilers (including unit `record`, `diff`,
`profile`, `gate`, whole-app `app front-door`, `app wire` and `app lift`) have
the invoking user's filesystem, network and environment permissions. MCP tools
use those same permissions. A temporary working directory is not a sandbox.
Do not use these commands on untrusted source without an external VM/container.

`app record` and `app replay` alone offer opt-in `--isolate` on **Linux** using
Bubblewrap. `--isolate` sets up separate namespaces, a private `/tmp` and
`/session` writable working tree, a read-only `/project` mount, and only
system runtime directories. The sandbox does not inherit the host environment.
Explicit `--isolate-env NAME=VALUE` and values recorded in input fixtures are
passed; reserved runtime variables cannot be overridden using `--isolate-env`.
An operator may add a read-only directory with `--isolate-ro /host/path`,
available at `/mnt/ro/N`; this deliberately grants access to its contents.
There is no default mount of the host home directory, agent credentials, other
repositories, or arbitrary writable host paths.

Network namespaces are isolated by default. HTTP isolation requires
`--isolate-network`, which shares the **full host network** (not a localhost-only
allowlist); in that configuration the app can contact external endpoints.
When Bubblewrap is absent or the kernel refuses namespace setup, isolation
fails closed before launching app code. On macOS and Windows `--isolate`
refuses to run. Isolated library replay is not supported.

No OS container is an absolute security guarantee. A kernel sandbox escape,
host-file changes racing input validation, overly broad operator-provided mounts,
and compromised system runtime binaries remain risks. Build steps, unrelated
tool commands and network-enabled servers are outside this mode's narrower
guarantee. Use a dedicated VM for hostile workloads that need stronger isolation.

## Resource limits during recording

Drivers have a startup/no-progress timeout (15 seconds), a per-case timeout
(10 seconds), and an overall recording deadline (600 seconds). They terminate
the process tree and preserve completed records, listing skipped case indices
and reasons. `RUST_ANYTHING_MEMORY_MIB` defaults to 8192 MiB and enforces an
inherited Linux RLIMIT_AS (virtual address space, per process) during recording,
app replay, and HTTP server execution. This is **not** a cgroup aggregate RSS
or fork-bomb limit. Override only in an appropriately resourced host/container.

On macOS and Windows, process trees are stopped best-effort on timeout, but
there is **no hard RAM limit**. Use external VM/container resource limits for
untrusted applications on those platforms; the built-in watchdog does not
prevent a rapid memory spike. The recording limits do not protect unrelated
build/profile/gate commands and are not an execution sandbox.

## Documented HTTP curl examples

When recording HTTP examples with `app record --http`, curl `@file` request bodies
are accepted only from regular files within the repository root. Relative paths are
resolved first from the repository root and then from the documentation file's folder.
Absolute paths, parent-directory traversal and symlinks resolving outside the
repository are rejected with a warning; the example is skipped. The `--data-raw`
option treats `@` literally.

This restriction applies to importing curl request bodies, **not** to the code
being executed: `app record` still starts the original application with the
invoking user's filesystem permissions. Do not run untrusted projects without OS
isolation. As with other path checks, concurrently modified filesystem trees are
outside the protection guarantee; use immutable or isolated input checkouts for
adversarial local writers.

## Replay fixture filesystem boundary

App replay treats `rust/app-cases/` as the fixture root inside the selected
repository. An explicit TOML `[fs] base` must be a relative path inside that
directory. Absolute paths and `..` components are rejected; canonicalized
directories must remain inside the fixture root. The default `<case>.in`
directory continues to work without an explicit setting.

Companion `<case>.stdin`, `<case>.stdout`, `<case>.stderr` and
`<case>.out/` paths receive the same canonical-root check before replay launches
the application. Input/output snapshot trees reject nested symlinks rather than
copying their targets. The fixture TOML, saved replay configuration, and the cases directory itself
must also resolve inside their expected roots. Invalid fixtures fail closed.

These checks prevent static fixture path escapes; they are not a sandbox for the
application being replayed. Concurrent adversarial filesystem modification can
race path validation, so treat a writable shared checkout as untrusted and use
OS isolation or an immutable checkout where needed.

## Disclosure

Once a fix is released, the advisory is published with credit to the reporter, unless they prefer
otherwise.
