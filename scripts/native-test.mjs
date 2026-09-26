import assert from "node:assert/strict";
import { execFileSync, spawn, spawnSync } from "node:child_process";
import { once } from "node:events";
import { chmodSync, copyFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import test from "node:test";
import { binaryPaths, packageRoot, packageVersion, platformKey } from "./native-common.mjs";

const versionOutput = `slopdex ${packageVersion()}`;

test("platform keys distinguish libc and CPU without assuming host defaults", () => {
  assert.equal(platformKey("linux", "x64", { header: { glibcVersionRuntime: "2.31" } }), "linux-x64-gnu");
  assert.equal(platformKey("linux", "arm64", { header: {} }), "linux-arm64-musl");
  assert.equal(platformKey("darwin", "arm64"), "darwin-arm64");
  assert.equal(platformKey("win32", "x64"), "win32-x64");
});

// Shell/shebang fixtures stand in for native executables. Real Windows binaries
// are exercised by the Windows Rust build + npm smoke in CI.
const nativeTest = (name, fn) => test(name, { skip: process.platform === "win32" }, fn);

function fixture(t) {
  const root = mkdtempSync(path.join(tmpdir(), "slopdex npm test "));
  t.after(() => rmSync(root, { recursive: true, force: true }));
  mkdirSync(path.join(root, "scripts"));
  for (const name of ["native-common.mjs", "native-install.mjs", "native-launcher.mjs"]) {
    copyFileSync(path.join(packageRoot, "scripts", name), path.join(root, "scripts", name));
  }
  copyFileSync(path.join(packageRoot, "package.json"), path.join(root, "package.json"));
  copyFileSync(path.join(packageRoot, ".gitignore"), path.join(root, ".gitignore"));
  writeFileSync(path.join(root, "Cargo.toml"), "# fixture manifest\n");
  writeFileSync(path.join(root, "Cargo.lock"), "# fixture lockfile\n");
  mkdirSync(path.join(root, "src"));
  writeFileSync(path.join(root, "src", "main.rs"), "fn main() {}\n");
  const executable = `#!${process.execPath}
if (process.argv[2] === '--version') { console.log(${JSON.stringify(versionOutput)}); }
else if (process.argv[2] === 'wait') {
  console.log('ready'); setInterval(() => {}, 1000);
} else {
  console.log(JSON.stringify({ args: process.argv.slice(2), cwd: process.cwd(), token: process.env.SLOPDEX_TEST_TOKEN }));
  process.stderr.write('fixture stderr\\n');
  process.exitCode = Number(process.env.SLOPDEX_TEST_EXIT ?? 0);
}
`;
  const writeExecutable = (file, content = executable) => {
    mkdirSync(path.dirname(file), { recursive: true });
    writeFileSync(file, content);
    chmodSync(file, 0o755);
  };
  const fakeBin = path.join(root, "fake-bin");
  mkdirSync(fakeBin);
  // The fake compiler verifies the installer's build invocation and emits a
  // runnable fixture only when instructed to succeed.
  writeExecutable(path.join(fakeBin, "cargo"), `#!${process.execPath}
import fs from 'node:fs';
fs.writeFileSync('cargo-call.json', JSON.stringify({ args: process.argv.slice(2), cwd: process.cwd(), target: process.env.CARGO_BUILD_TARGET }));
if (process.env.SLOPDEX_TEST_CARGO_FAIL) process.exit(17);
fs.mkdirSync('target/release', { recursive: true });
fs.writeFileSync('target/release/slopdex', ${JSON.stringify(executable)}, { mode: 0o755 });
`);
  const env = { ...process.env, PATH: `${fakeBin}${path.delimiter}${process.env.PATH}` };
  const run = (script, args = [], overrides = {}) => spawnSync(process.execPath, [path.join(root, "scripts", script), ...args], {
    cwd: tmpdir(), encoding: "utf8", env: { ...env, ...overrides },
  });
  return { root, env, run, writeExecutable, ...binaryPaths(root) };
}

nativeTest("installer uses bundled prebuilt, repairs executable permissions, and skips Cargo", (t) => {
  const f = fixture(t);
  f.writeExecutable(f.prebuilt);
  chmodSync(f.prebuilt, 0o644);
  const result = f.run("native-install.mjs");
  assert.equal(result.status, 0, result.stderr);
  assert.ok(existsSync(f.built));
  assert.ok(!existsSync(path.join(f.root, "cargo-call.json")));
  assert.equal(f.run("native-launcher.mjs", ["--version"]).stdout.trim(), versionOutput);
});

nativeTest("source fallback builds locked release from package root and ignores cross-target environment", (t) => {
  const f = fixture(t);
  const result = f.run("native-install.mjs", [], { CARGO_BUILD_TARGET: "wrong-target" });
  assert.equal(result.status, 0, result.stderr);
  const call = JSON.parse(readFileSync(path.join(f.root, "cargo-call.json"), "utf8"));
  assert.equal(call.cwd, f.root);
  assert.equal(call.target, undefined);
  assert.deepEqual(call.args, ["build", "--locked", "--release", "--bin", "slopdex", "--manifest-path", path.join(f.root, "Cargo.toml"), "--target-dir", path.join(f.root, "target")]);
  assert.equal(f.run("native-launcher.mjs", ["--version"]).status, 0);
  assert.equal(f.run("native-install.mjs", [], { SLOPDEX_TEST_CARGO_FAIL: "1" }).status, 0);
});

nativeTest("an incompatible prebuilt falls back to source; launcher selects the successful build", (t) => {
  const f = fixture(t);
  f.writeExecutable(f.prebuilt, "#!/bin/sh\nexit 126\n");
  const result = f.run("native-install.mjs");
  assert.equal(result.status, 0, result.stderr);
  assert.match(result.stderr, /cannot use/);
  assert.equal(f.run("native-launcher.mjs", ["--version"]).stdout.trim(), versionOutput);
});

nativeTest("stale source binary is replaced by matching prebuilt", (t) => {
  const f = fixture(t);
  f.writeExecutable(f.built, "#!/bin/sh\nprintf 'slopdex 0.18.0\\n'\n");
  f.writeExecutable(f.prebuilt);
  assert.equal(f.run("native-install.mjs").status, 0);
  assert.equal(f.run("native-launcher.mjs", ["--version"]).stdout.trim(), versionOutput);
});

nativeTest("compiler failure is actionable and installation fails", (t) => {
  const f = fixture(t);
  const result = f.run("native-install.mjs", [], { SLOPDEX_TEST_CARGO_FAIL: "1" });
  assert.equal(result.status, 1);
  assert.match(result.stderr, /cargo exited 17/);
  assert.match(result.stderr, /Rust.*C\/C\+\+ compiler/);
});

nativeTest("missing Cargo produces explicit toolchain requirements", (t) => {
  const f = fixture(t);
  const emptyPath = path.join(f.root, "empty-path");
  mkdirSync(emptyPath);
  const result = f.run("native-install.mjs", [], { PATH: emptyPath });
  assert.equal(result.status, 1);
  assert.match(result.stderr, /ENOENT/);
  assert.match(result.stderr, /rustc and cargo/);
});

nativeTest("missing lockfile fails instead of resolving unlocked dependencies", (t) => {
  const f = fixture(t);
  rmSync(path.join(f.root, "Cargo.lock"));
  const result = f.run("native-install.mjs");
  assert.equal(result.status, 1);
  assert.match(result.stderr, /missing Cargo.lock/);
  assert.ok(!existsSync(path.join(f.root, "cargo-call.json")));
});

nativeTest("launcher preserves arguments, cwd, environment, stderr and exit code", (t) => {
  const f = fixture(t);
  f.writeExecutable(f.built);
  const args = ["search", "spaces 'quotes' ; $HOME", "--root", "a path"];
  const result = f.run("native-launcher.mjs", args, { SLOPDEX_TEST_TOKEN: "inherited", SLOPDEX_TEST_EXIT: "7" });
  assert.equal(result.status, 7);
  assert.deepEqual(JSON.parse(result.stdout), { args, cwd: tmpdir(), token: "inherited" });
  assert.equal(result.stderr, "fixture stderr\n");
});

nativeTest("launcher reports missing and non-executable binaries", (t) => {
  const f = fixture(t);
  let result = f.run("native-launcher.mjs");
  assert.equal(result.status, 1);
  assert.match(result.stderr, /install scripts enabled/);
  f.writeExecutable(f.built);
  chmodSync(f.built, 0o644);
  result = f.run("native-launcher.mjs");
  assert.equal(result.status, 1);
  assert.match(result.stderr, /failed to start/);
});

nativeTest("launcher forwards termination to its child and preserves the signal", async (t) => {
  const f = fixture(t);
  f.writeExecutable(f.built);
  const child = spawn(process.execPath, [path.join(f.root, "scripts", "native-launcher.mjs"), "wait"], { stdio: ["ignore", "pipe", "pipe"] });
  t.after(() => { if (child.exitCode === null && child.signalCode === null) child.kill("SIGKILL"); });
  const timeout = setTimeout(() => child.kill("SIGKILL"), 10_000);
  t.after(() => clearTimeout(timeout));
  await once(child.stdout, "data");
  const exit = once(child, "exit");
  child.kill("SIGTERM");
  const [code, signal] = await exit;
  assert.equal(code, null);
  assert.equal(signal, "SIGTERM");
});

for (const prebuilt of [true, false]) {
  nativeTest(`npm global tarball installation works with ${prebuilt ? "bundled prebuilt" : "source fallback"}`, (t) => {
    const f = fixture(t);
    if (prebuilt) f.writeExecutable(f.prebuilt);
    // Test the real package files allowlist against ignored native/target paths.
    f.writeExecutable(f.built);
    mkdirSync(path.join(f.root, "dist"));
    writeFileSync(path.join(f.root, "dist", "historical.js"), "export {};\n");
    const packed = JSON.parse(execFileSync("npm", ["pack", "--ignore-scripts", "--json"], { cwd: f.root, encoding: "utf8" }));
    const files = packed[0].files.map((file) => file.path);
    for (const file of ["Cargo.toml", "Cargo.lock", "src/main.rs", "scripts/native-launcher.mjs", "scripts/native-install.mjs", "scripts/native-common.mjs"]) {
      assert.ok(files.includes(file), `Missing package file ${file}`);
    }
    assert.ok(!files.some((file) => file.startsWith("target/") || file.startsWith("dist/")));
    if (prebuilt) assert.ok(files.includes(path.relative(f.root, f.prebuilt)));
    const prefix = path.join(f.root, "global prefix");
    mkdirSync(prefix);
    const result = spawnSync("npm", ["install", "--global", "--prefix", prefix, "--offline", "--no-audit", "--no-fund", path.join(f.root, packed[0].filename)], { cwd: tmpdir(), env: f.env, encoding: "utf8" });
    assert.equal(result.status, 0, result.stderr);
    const installed = path.join(prefix, "lib", "node_modules", "@ninjaxtools", "slopdex");
    assert.equal(existsSync(path.join(installed, "cargo-call.json")), !prebuilt);
    const output = execFileSync(path.join(prefix, "bin", "slopdex"), ["--version"], { cwd: tmpdir(), encoding: "utf8" });
    assert.equal(output.trim(), versionOutput);
  });
}
