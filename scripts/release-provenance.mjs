#!/usr/bin/env node
// Verify byte-for-byte identity between CI binaries, release archives, and npm's packed binaries.
// This manifest is an inventory, NOT an independently authenticated signature.
import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'
import crypto from 'node:crypto'
import { execFileSync } from 'node:child_process'

const platforms = Object.freeze({
  'darwin-arm64': 'aarch64-apple-darwin',
  'darwin-x64': 'x86_64-apple-darwin',
  'linux-arm64': 'aarch64-unknown-linux-gnu',
  'linux-x64': 'x86_64-unknown-linux-gnu',
  'win32-x64': 'x86_64-pc-windows-msvc',
})
const schema = 'rust-anything/release-manifest@1'
const sha256 = (bytes) => crypto.createHash('sha256').update(bytes).digest('hex')
const fileHash = (filename) => sha256(fs.readFileSync(filename))
const assert = (condition, message) => { if (!condition) throw new Error(message) }
const hex = (value) => typeof value === 'string' && /^[a-f0-9]{64}$/.test(value)

function options(args) {
  const opts = {}
  for (let i = 0; i < args.length; i += 2) {
    assert(args[i]?.startsWith('--') && args[i + 1] !== undefined, `expected --name VALUE, got ${args[i]}`)
    assert(!Object.hasOwn(opts, args[i].slice(2)), `duplicate option ${args[i]}`)
    opts[args[i].slice(2)] = args[i + 1]
  }
  return opts
}
function required(opts, key) {
  assert(opts[key], `missing --${key}`)
  return opts[key]
}
function platformFiles(version, key, target) {
  const exe = key.startsWith('win32') ? 'rust-anything.exe' : 'rust-anything'
  const archiveBase = `rust-anything-v${version}-${target}`
  return {
    target,
    binaryPath: `binaries/${key}/${exe}`,
    archive: `${archiveBase}.${key.startsWith('win32') ? 'zip' : 'tar.gz'}`,
    archiveBinary: `${archiveBase}/${exe}`,
  }
}
function archiveEntry(archive, filename) {
  const command = archive.endsWith('.zip') ? 'unzip' : 'tar'
  const args = archive.endsWith('.zip')
    ? ['-p', archive, filename]
    : ['-xOzf', archive, filename]
  try {
    return execFileSync(command, args, { maxBuffer: 512 * 1024 * 1024, stdio: ['ignore', 'pipe', 'pipe'] })
  } catch (error) {
    throw new Error(`cannot extract ${filename} from ${archive}: ${error.stderr?.toString().trim() || error.message}`)
  }
}
function nativeChecksums(artifacts) {
  return Object.entries(artifacts)
    .sort(([a], [b]) => a.localeCompare(b))
    .map(([, entry]) => `${entry.binarySha256}  ${entry.binaryPath}`)
    .join('\n') + '\n'
}
function readManifest(dir) {
  const manifest = JSON.parse(fs.readFileSync(path.join(dir, 'release-manifest.json'), 'utf8'))
  assert(manifest.schema === schema, 'unrecognized release manifest schema')
  assert(typeof manifest.repository === 'string' && /^[\w.-]+\/[\w.-]+$/.test(manifest.repository), 'invalid repository')
  assert(/^\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?$/.test(manifest.version), 'invalid version')
  assert(manifest.tag === `v${manifest.version}`, 'tag/version mismatch')
  assert(typeof manifest.commit === 'string' && /^[0-9a-f]{40}$/.test(manifest.commit), 'invalid commit')
  assert(manifest.workflow === '.github/workflows/release.yml', 'unexpected workflow')
  assert(manifest.platforms && Object.keys(manifest.platforms).sort().join(',') === Object.keys(platforms).sort().join(','), 'platforms missing or unexpected')
  assert(manifest.npm && manifest.npm.file === `rust-anything-${manifest.version}.tgz` && hex(manifest.npm.sha256), 'invalid npm tarball entry')
  for (const [key, target] of Object.entries(platforms)) {
    const entry = manifest.platforms[key]
    const expected = platformFiles(manifest.version, key, target)
    for (const property of ['target', 'binaryPath', 'archive', 'archiveBinary']) {
      assert(entry[property] === expected[property], `unexpected ${key}.${property}`)
    }
    assert(hex(entry.binarySha256) && hex(entry.archiveSha256), `bad SHA256 in ${key}`)
  }
  return manifest
}
function packEntry(tarball, filename) {
  return archiveEntry(tarball, `package/${filename}`)
}
function checkNpm(tarball, manifest) {
  assert(fileHash(tarball) === manifest.npm.sha256, 'npm tarball digest differs from release manifest')
  const pkg = JSON.parse(packEntry(tarball, 'package.json').toString('utf8'))
  assert(pkg.name === 'rust-anything' && pkg.version === manifest.version, 'npm package identity/version differs')
  const expected = nativeChecksums(manifest.platforms)
  const recorded = packEntry(tarball, 'binaries/SHA256SUMS').toString('utf8')
  assert(recorded === expected, 'npm-embedded native SHA256SUMS does not match release manifest')
  for (const [key, entry] of Object.entries(manifest.platforms)) {
    assert(sha256(packEntry(tarball, entry.binaryPath)) === entry.binarySha256, `npm binary mismatch: ${key}`)
  }
}
function checksums(release, manifest) {
  const assets = [...Object.values(manifest.platforms).map((p) => p.archive), manifest.npm.file,
    'release-manifest.json', 'NATIVE-SHA256SUMS'].sort()
  return assets.map((name) => `${fileHash(path.join(release, name))}  ${name}`).join('\n') + '\n'
}
function verify(release, opts = {}) {
  const manifest = readManifest(release)
  const expectedFiles = [
    ...Object.values(manifest.platforms).map((entry) => entry.archive),
    manifest.npm.file, 'release-manifest.json', 'NATIVE-SHA256SUMS', 'SHA256SUMS',
  ].sort()
  assert(
    JSON.stringify(fs.readdirSync(release).sort()) === JSON.stringify(expectedFiles),
    'release contains missing or untracked assets',
  )
  for (const key of ['tag', 'commit']) {
    if (opts[key]) assert(opts[key] === manifest[key], `${key} does not match expected release`)
  }
  if (opts.repo) assert(opts.repo === manifest.repository, 'repository does not match expected release')
  for (const [key, entry] of Object.entries(manifest.platforms)) {
    const archive = path.join(release, entry.archive)
    assert(fileHash(archive) === entry.archiveSha256, `release archive digest mismatch: ${key}`)
    assert(sha256(archiveEntry(archive, entry.archiveBinary)) === entry.binarySha256, `archive binary mismatch: ${key}`)
  }
  assert(fs.readFileSync(path.join(release, 'NATIVE-SHA256SUMS'), 'utf8') === nativeChecksums(manifest.platforms), 'release native checksum list mismatch')
  checkNpm(path.join(release, manifest.npm.file), manifest)
  assert(fs.readFileSync(path.join(release, 'SHA256SUMS'), 'utf8') === checksums(release, manifest), 'release SHA256SUMS mismatch')
  return manifest
}
function create(opts) {
  const release = path.resolve(required(opts, 'dir'))
  const binDir = path.resolve(required(opts, 'npm-bin'))
  const tag = required(opts, 'tag')
  const commit = required(opts, 'commit')
  const repository = required(opts, 'repo')
  assert(/^v\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?$/.test(tag), 'invalid release tag')
  assert(/^[0-9a-f]{40}$/.test(commit), 'expected 40-character git SHA')
  assert(/^[\w.-]+\/[\w.-]+$/.test(repository), 'invalid source repository')
  const version = tag.slice(1)
  const out = { schema, repository, version, tag, commit, workflow: '.github/workflows/release.yml', platforms: {}, npm: {} }
  for (const [key, target] of Object.entries(platforms)) {
    const names = platformFiles(version, key, target)
    const source = path.join(binDir, `npm-${key}`, path.basename(names.binaryPath))
    const archive = path.join(release, names.archive)
    assert(fs.existsSync(source), `missing CI build binary: ${source}`)
    const binarySha256 = fileHash(source)
    assert(sha256(archiveEntry(archive, names.archiveBinary)) === binarySha256, `archive/npm CI build disagreement: ${key}`)
    out.platforms[key] = { ...names, binarySha256, archiveSha256: fileHash(archive) }
  }
  out.npm = { file: `rust-anything-${version}.tgz`, sha256: fileHash(path.join(release, `rust-anything-${version}.tgz`)) }
  assert(fs.readFileSync(path.join(release, 'NATIVE-SHA256SUMS'), 'utf8') === nativeChecksums(out.platforms), 'CI native binary checksum inventory differs')
  checkNpm(path.join(release, out.npm.file), out)
  fs.writeFileSync(path.join(release, 'release-manifest.json'), JSON.stringify(out, null, 2) + '\n')
  fs.writeFileSync(path.join(release, 'SHA256SUMS'), checksums(release, out))
  return verify(release, opts)
}
function verifyPublished(release, tarball) {
  const manifest = verify(release)
  const published = path.resolve(tarball)
  // npm versions cannot be overwritten; compare *exact package bytes* before
  // silently skipping an already existing version or claiming a successful publish.
  assert(fileHash(published) === manifest.npm.sha256, 'published npm tarball differs from signed release tarball')
  checkNpm(published, manifest)
  return manifest
}
function main() {
  const [command, ...rest] = process.argv.slice(2)
  const opts = options(rest)
  if (command === 'create') {
    const manifest = create(opts)
    console.log(`created and verified ${manifest.tag} with ${Object.keys(manifest.platforms).length} native binaries`)
  } else if (command === 'verify') {
    const manifest = verify(path.resolve(required(opts, 'dir')), opts)
    console.log(`verified ${manifest.tag}: all native binaries, archives and npm tarball match`)
  } else if (command === 'verify-published') {
    const manifest = verifyPublished(path.resolve(required(opts, 'dir')), required(opts, 'tarball'))
    console.log(`published npm tarball matches ${manifest.tag} byte-for-byte`)
  } else if (command === 'verify-installed') {
    const manifest = readManifest(path.resolve(required(opts, 'dir')))
    const key = required(opts, 'platform')
    assert(Object.hasOwn(manifest.platforms, key), `unsupported platform ${key}`)
    assert(fileHash(required(opts, 'binary')) === manifest.platforms[key].binarySha256, `installed native binary differs for ${key}`)
    console.log(`installed ${key} native binary matches ${manifest.tag} build digest`)
  } else {
    throw new Error('usage: release-provenance.mjs create|verify|verify-published|verify-installed --dir DIR [options]')
  }
}
try { main() } catch (error) { console.error(`release verification failed: ${error.message}`); process.exitCode = 1 }
