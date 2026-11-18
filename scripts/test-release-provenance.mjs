import { test } from 'node:test'
import assert from 'node:assert/strict'
import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'
import { execFileSync, spawnSync } from 'node:child_process'
import crypto from 'node:crypto'

const tool = path.join(import.meta.dirname, 'release-provenance.mjs')
const version = '9.9.9'
const tag = `v${version}`
const commit = 'a'.repeat(40)
const targets = {
  'darwin-arm64': 'aarch64-apple-darwin',
  'darwin-x64': 'x86_64-apple-darwin',
  'linux-arm64': 'aarch64-unknown-linux-gnu',
  'linux-x64': 'x86_64-unknown-linux-gnu',
  'win32-x64': 'x86_64-pc-windows-msvc',
}
const hash = (value) => crypto.createHash('sha256').update(value).digest('hex')
function command(...args) {
  const result = spawnSync(process.execPath, [tool, ...args], { encoding: 'utf8' })
  return result
}
function fixture() {
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'release-provenance-test-'))
  const release = path.join(tmp, 'release')
  const binDir = path.join(tmp, 'npm-bin')
  const stage = path.join(tmp, 'stage')
  const pkg = path.join(tmp, 'package')
  for (const dir of [release, binDir, stage, pkg]) fs.mkdirSync(dir, { recursive: true })
  const checksums = []
  for (const [key, target] of Object.entries(targets)) {
    const exe = key.startsWith('win32') ? 'rust-anything.exe' : 'rust-anything'
    const basename = `rust-anything-${tag}-${target}`
    const bytes = Buffer.from(`test-build:${key}:${commit}\n`)
    const native = path.join(binDir, `npm-${key}`, exe)
    fs.mkdirSync(path.dirname(native), { recursive: true })
    fs.writeFileSync(native, bytes)
    const dir = path.join(stage, basename)
    fs.mkdirSync(dir)
    fs.writeFileSync(path.join(dir, exe), bytes)
    const archive = path.join(release, `${basename}.${key.startsWith('win32') ? 'zip' : 'tar.gz'}`)
    if (key.startsWith('win32')) execFileSync('zip', ['-q', '-r', archive, basename], { cwd: stage })
    else execFileSync('tar', ['-czf', archive, '-C', stage, basename])
    const relative = `binaries/${key}/${exe}`
    const installed = path.join(pkg, relative)
    fs.mkdirSync(path.dirname(installed), { recursive: true })
    fs.writeFileSync(installed, bytes)
    checksums.push(`${hash(bytes)}  ${relative}`)
  }
  fs.writeFileSync(path.join(pkg, 'binaries/SHA256SUMS'), checksums.join('\n') + '\n')
  fs.writeFileSync(path.join(pkg, 'package.json'), JSON.stringify({
    name: 'rust-anything', version, files: ['binaries', 'bin'], bin: { 'rust-anything': 'bin/rust-anything.js' }
  }))
  fs.mkdirSync(path.join(pkg, 'bin'))
  fs.writeFileSync(path.join(pkg, 'bin/rust-anything.js'), '#!/usr/bin/env node\n')
  execFileSync('npm', ['pack', pkg, '--pack-destination', release, '--silent'], { stdio: 'pipe' })
  fs.copyFileSync(path.join(pkg, 'binaries/SHA256SUMS'), path.join(release, 'NATIVE-SHA256SUMS'))
  const args = ['--dir', release, '--repo', 'example/repo', '--tag', tag, '--commit', commit]
  return { tmp, release, binDir, args, check: () => fs.rmSync(tmp, { recursive: true, force: true }) }
}
function created(fixture) {
  const output = command('create', ...fixture.args, '--npm-bin', fixture.binDir)
  assert.equal(output.status, 0, output.stderr)
}
test('valid release matches all five native CI binaries, npm tarball, archives and checksums', () => {
  const f = fixture()
  try {
    created(f)
    const ok = command('verify', ...f.args)
    assert.equal(ok.status, 0, ok.stderr)
    const installed = path.join(f.binDir, 'npm-linux-x64', 'rust-anything')
    assert.equal(command('verify-installed', '--dir', f.release, '--platform', 'linux-x64', '--binary', installed).status, 0)
    assert.equal(command('verify-published', '--dir', f.release, '--tarball', path.join(f.release, `rust-anything-${version}.tgz`)).status, 0)
    const wrongCommit = command('verify', ...f.args.slice(0, -1), 'b'.repeat(40))
    assert.notEqual(wrongCommit.status, 0)
  } finally { f.check() }
})
test('tampered archive, npm tarball, and installed binary are rejected', () => {
  const f = fixture()
  try {
    created(f)
    const archive = path.join(f.release, `rust-anything-${tag}-x86_64-unknown-linux-gnu.tar.gz`)
    fs.appendFileSync(archive, 'tamper')
    assert.notEqual(command('verify', ...f.args).status, 0)
    const tamper = Buffer.from('malicious-installed')
    const installed = path.join(f.tmp, 'installed')
    fs.writeFileSync(installed, tamper)
    assert.notEqual(command('verify-installed', '--dir', f.release, '--platform', 'linux-x64', '--binary', installed).status, 0)
  } finally { f.check() }
})
test('npm package mismatch fails even when archive bytes are unchanged', () => {
  const f = fixture()
  try {
    created(f)
    const tgz = path.join(f.release, `rust-anything-${version}.tgz`)
    const published = path.join(f.tmp, 'published.tgz')
    fs.copyFileSync(tgz, published)
    fs.appendFileSync(published, 'not-the-release')
    assert.notEqual(command('verify-published', '--dir', f.release, '--tarball', published).status, 0)
  } finally { f.check() }
})
test('different CI native binary from release archive fails before manifest publication', () => {
  const f = fixture()
  try {
    fs.writeFileSync(path.join(f.binDir, 'npm-linux-x64', 'rust-anything'), 'unexpected-binary')
    const result = command('create', ...f.args, '--npm-bin', f.binDir)
    assert.notEqual(result.status, 0)
    assert.match(result.stderr, /archive\/npm CI build disagreement/)
  } finally { f.check() }
})

test('npm package assembler writes checksums for every shipped executable', () => {
  const f = fixture()
  try {
    const checkout = path.join(f.tmp, 'fake-checkout')
    fs.mkdirSync(path.join(checkout, 'scripts'), { recursive: true })
    fs.mkdirSync(path.join(checkout, 'npm/rust-anything/bin'), { recursive: true })
    fs.writeFileSync(path.join(checkout, 'Cargo.toml'), `[package]\nname = \"rust-anything\"\nversion = \"${version}\"\n`)
    fs.writeFileSync(path.join(checkout, 'npm/rust-anything/package.json'), JSON.stringify({
      name: 'rust-anything', version: '0.0.0', files: ['bin', 'binaries']
    }))
    fs.writeFileSync(path.join(checkout, 'npm/rust-anything/bin/rust-anything.js'), '#!/usr/bin/env node\n')
    fs.writeFileSync(path.join(checkout, 'README.md'), 'readme')
    fs.writeFileSync(path.join(checkout, 'LICENSE'), 'license')
    const generator = path.join(checkout, 'scripts/npm-packages.mjs')
    fs.copyFileSync(path.join(import.meta.dirname, 'npm-packages.mjs'), generator)
    const dest = path.join(f.tmp, 'assembled')
    const output = execFileSync(process.execPath, [generator, version, f.binDir, dest], { encoding: 'utf8' }).trim()
    const pkg = JSON.parse(fs.readFileSync(path.join(output, 'package.json'), 'utf8'))
    assert.equal(pkg.version, version)
    const sumLines = fs.readFileSync(path.join(output, 'binaries/SHA256SUMS'), 'utf8').trim().split('\n')
    assert.equal(sumLines.length, 5)
    for (const line of sumLines) {
      const [, digest, filename] = /^(\w{64})  (binaries\/[^ ]+)$/.exec(line) || []
      assert.equal(hash(fs.readFileSync(path.join(output, filename))), digest)
    }
  } finally { f.check() }
})
