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

App replay treats \`rust/app-cases/\` as the fixture root inside the selected
repository. An explicit TOML \`[fs] base\` must be a relative path inside that
directory. Absolute paths and \`..\` components are rejected; canonicalized
directories must remain inside the fixture root. The default \`<case>.in\`
directory continues to work without an explicit setting.

Companion \`<case>.stdin\`, \`<case>.stdout\`, \`<case>.stderr\` and
\`<case>.out/\` paths receive the same canonical-root check before replay launches
the application. Input/output snapshot trees reject nested symlinks rather than
copying their targets. The fixture TOML and the cases directory itself must also
resolve inside their expected roots. Invalid fixtures fail closed.

These checks prevent static fixture path escapes; they are not a sandbox for the
application being replayed. Concurrent adversarial filesystem modification can
race path validation, so treat a writable shared checkout as untrusted and use
OS isolation or an immutable checkout where needed.

## Disclosure

Once a fix is released, the advisory is published with credit to the reporter, unless they prefer
otherwise.
