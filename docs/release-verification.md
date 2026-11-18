# Verify native release artifacts

These instructions apply to GitHub releases created **after** the provenance workflow was
enabled in infectiousstupidity-labs/rust-anything. Earlier releases have no automatic
attestation guarantee. A digest manifest and SHA256SUMS establish byte identity, **not**
authenticity on their own; verify the GitHub signatures separately.

Use an up-to-date GitHub CLI (2.102.0 or later), Node.js 22+, git, tar, and unzip.
Replace v1.5.2 below with a **real published** tag. The example is not a claim that
v1.5.2 has been released.

## Verify the GitHub release and source identity

    REPO=infectiousstupidity-labs/rust-anything
    TAG=v1.5.2
    mkdir -p release-check
    gh release download "$TAG" --repo "$REPO" --dir release-check

    # Fetch the verification code from the exact release tag, not mutable main.
    git clone --depth 1 --branch "$TAG" "https://github.com/$REPO.git" verify-source
    COMMIT=$(git -C verify-source rev-parse HEAD)

    # Byte checks: all 5 archived native binaries must equal the npm payload,
    # and every release archive, tarball and inventory must match its SHA256.
    node verify-source/scripts/release-provenance.mjs verify \
      --dir release-check --repo "$REPO" --tag "$TAG" --commit "$COMMIT"

Expected Node output:

    verified v1.5.2: all native binaries, archives and npm tarball match

**Then verify signed provenance for EVERY release asset** (including the manifest,
npm tarball and archive files) against the specific source repository, ref, commit
and build workflow:

    for file in release-check/*; do
      gh attestation verify "$file" \
        --repo "$REPO" \
        --signer-workflow "$REPO/.github/workflows/release.yml" \
        --source-ref "refs/tags/$TAG" \
        --source-digest "$COMMIT" \
        --deny-self-hosted-runners
    done

The GitHub CLI must report a verified attestation for every input. Failures
are **not** safe to ignore. The tag/commit chosen should also be checked
independently against the repository. Signed workflow provenance connects
the file digest to a GitHub workflow and commit. It does **not** prove that
separate rebuilds are reproducible, or that the source/build tools are safe.

The signed release contains:
- release-manifest.json: Git SHA, tagged ref, workflow, five Rust targets,
  and each native binary, release archive and npm tarball SHA-256.
- NATIVE-SHA256SUMS: digest for each installed platform executable.
- SHA256SUMS: digest for each release archive, npm tarball and both inventories.
- rust-anything-VERSION.tgz: the **same tarball** passed to npm publish.

The npm tarball contains binaries/SHA256SUMS with the same five hashes.

## Compare with npm and a local installation

Download the immutable published npm tarball and compare its **entire compressed
file** with the attested GitHub release tarball:

    mkdir -p npm-check
    npm pack "rust-anything@${TAG#v}" --pack-destination npm-check --silent
    node verify-source/scripts/release-provenance.mjs verify-published \
      --dir release-check --tarball "npm-check/rust-anything-${TAG#v}.tgz"

Expected:

    published npm tarball matches v1.5.2 byte-for-byte

For a globally installed npm binary on Linux x86-64, compare its digest with
the now-verified release manifest:

    PLATFORM=linux-x64
    BIN="$(npm root -g)/rust-anything/binaries/$PLATFORM/rust-anything"
    node verify-source/scripts/release-provenance.mjs verify-installed \
      --dir release-check --platform "$PLATFORM" --binary "$BIN"

Expected:

    installed linux-x64 native binary matches v1.5.2 build digest

Other supported keys: linux-arm64, darwin-x64, darwin-arm64, win32-x64.
On Windows the filename is rust-anything.exe. The installed-binary check
alone is **not** a signature verification; do the GitHub attestation step first.

## How publishing is gated

Each of the five platform build jobs signs attestations for its raw native
executable and release archive. The assemble job downloads **those exact CI
outputs** and compares the native binary bytes within release archives and
the npm package. It builds a single multi-platform npm tarball, generates
SHA256 inventories, verifies everything, and signs all release assets.
The GitHub release job verifies artifact identity **and the signatures** before
publishing. npm receives that **same tarball** only after the release job
succeeds; it downloads the registry copy afterward and requires an exact
tarball match, even if that npm version already existed.

Run the synthetic positive and tamper-negative regression suite locally:

    node --test scripts/test-release-provenance.mjs

These checks establish artifact identity, not independently reproducible
builds. Compiler toolchains, runners, action dependencies and source-code
trust remain separate security considerations.

**Publishing precondition:** The npm package owner must configure trusted
publishing for the exact fork repository and release.yml workflow, and must
have rights to publish the npm name. The package's repository.url must match
that repository for npm OIDC provenance. The name rust-anything may still be
owned by the upstream maintainer; this fork does not assume publication rights.
If the publisher is misconfigured, the workflow must fail rather than claim
a successful npm release.

**Validation record:** No tagged release built by this revised pipeline has
been verified yet. Record the release tag, source SHA, signed-attestation checks,
npm tarball hash and CI results in issue #6 before closing the issue.
