#!/usr/bin/env node
"use strict";

// Shim. Finds the prebuilt binary that npm installed for this machine, runs it,
// and gets out of the way.
//
// The whole point of shipping on npm is that `npx anomalift` works for someone
// who has Node and nothing else - no Rust, no cargo, no compiler. So this file
// must never try to build anything, and must never be more than a hop.
//
// CommonJS on purpose: a `bin` entry is resolved and run directly by npm, and
// ESM here would break on any consumer whose package.json says "type": "module"
// in the wrong direction. `require` always works.

const fs = require("fs");
const path = require("path");
const { spawnSync } = require("child_process");

// npm reads the `os` and `cpu` fields of each optional dependency and installs
// only the one that matches, silently skipping the others - that is the whole
// trick. It also means exactly one of these is ever present on disk, so we do
// not choose between them, we just ask for the one this machine needs.
//
// Unscoped names on purpose: a scoped package needs the npm organisation to
// exist before the first publish, and publishes as private unless every single
// publish remembers `--access public`. Getting that wrong ships a root package
// whose dependencies nobody can install.
const PLATFORM_PACKAGES = {
  "darwin-arm64": "anomalift-darwin-arm64",
  "darwin-x64": "anomalift-darwin-x64",
  "linux-x64": "anomalift-linux-x64",
  "linux-arm64": "anomalift-linux-arm64",
  "win32-x64": "anomalift-windows-x64",
  // Windows on ARM runs x64 binaries under emulation, and there is no native
  // arm64 Windows build. Without this entry the shim reports "no prebuilt
  // binary for win32-arm64" on a machine that can run the one it has.
  "win32-arm64": "anomalift-windows-x64",
};

const REPO = "kailash16dev/Anomalift";

// process.arch reports the architecture of the *Node process*, not the CPU. An
// x64 Node under Rosetta on Apple Silicon says "x64", and will then get the
// x86_64 build - slower, but correct, and the alternative is running an arm64
// binary that machine's Node cannot even spawn.
const key = process.platform + "-" + process.arch;
const pkgName = PLATFORM_PACKAGES[key];

function fail(message) {
  process.stderr.write("\nanomalift: " + message + "\n\n");
  process.exit(1);
}

function installHint() {
  return (
    "Install the binary directly instead - it does not involve npm at all:\n\n" +
    "  macOS / Linux   curl -fsSL https://raw.githubusercontent.com/" +
    REPO +
    "/main/install.sh | sh\n" +
    "  Windows         irm https://raw.githubusercontent.com/" +
    REPO +
    "/main/install.ps1 | iex"
  );
}

if (!pkgName) {
  fail(
    "no prebuilt binary exists for " +
      key +
      ".\n\n" +
      "This package only wraps binaries produced by the release workflow; it\n" +
      "cannot build from source. Supported: " +
      Object.keys(PLATFORM_PACKAGES).join(", ") +
      ".\n\n" +
      "If you need " +
      key +
      ", build it with `cargo build --release` from the repo."
  );
}

const exe = process.platform === "win32" ? "anomalift.exe" : "anomalift";

let pkgRoot;
try {
  // Resolve the *manifest*, not the binary, and join the rest by hand.
  // require.resolve throws the same MODULE_NOT_FOUND when the target file is
  // absent, so resolving `<pkg>/bin/anomalift` directly cannot tell "npm never
  // installed this package" apart from "this package was published without its
  // binary in it" - and the second is a botched release needing a completely
  // different fix. Resolving package.json, which is always there if the
  // package is, separates the two.
  //
  // The platform packages deliberately have no "exports" field, which is what
  // keeps this legal: an "exports" map seals off every subpath not listed in
  // it, package.json included.
  pkgRoot = path.dirname(require.resolve(pkgName + "/package.json"));
} catch (err) {
  // A stack trace here helps nobody. The realistic causes are all install-time
  // and all have the same two fixes.
  fail(
    "the platform package " +
      pkgName +
      " is not installed.\n\n" +
      "npm skips optional dependencies when installed with --no-optional or\n" +
      "--omit=optional, and some CI caches restore a lockfile that predates the\n" +
      "platform packages. Try:\n\n" +
      "  npm install --force " +
      pkgName +
      "\n\n" +
      installHint()
  );
}

const binPath = path.join(pkgRoot, "bin", exe);

if (!fs.existsSync(binPath)) {
  fail(
    "the platform package " +
      pkgName +
      " resolved but its binary is missing:\n  " +
      binPath +
      "\n\n" +
      "That package was published without the binary copied in, or the install\n" +
      "was interrupted. " +
      installHint()
  );
}

const result = spawnSync(binPath, process.argv.slice(2), {
  stdio: "inherit",
  // Do not use `shell: true`. Arguments here are file paths and globs from a
  // user's command line, and a shell would re-interpret every quote and space
  // in them - and would also be an injection hole in any script that builds an
  // anomalift invocation from repository contents.
  shell: false,
});

if (result.error) {
  if (result.error.code === "EACCES") {
    fail(
      "the binary is not executable:\n  " +
        binPath +
        "\n\n" +
        "npm normally preserves the executable bit from the published tarball.\n" +
        "Fix it with `chmod +x " +
        binPath +
        "`, or reinstall."
    );
  }
  fail("failed to run " + binPath + ": " + result.error.message);
}

// Exit code propagation is the reason this file is not a one-liner. anomalift
// is meant to be used in scripts and pre-commit hooks, where a swallowed
// non-zero exit turns "this found problems" into "this passed".
if (result.signal) {
  // A child killed by a signal exits with status === null, and reporting 0 for
  // that would be a lie. Re-raise the same signal on ourselves so the parent
  // shell sees a signalled death, which is what it would see without this shim
  // in the middle. Windows has no real signals, so fall through to 1 if we are
  // somehow still alive afterwards.
  try {
    process.kill(process.pid, result.signal);
  } catch (err) {
    /* not fatal - handled by the exit below */
  }
  process.exit(1);
}

process.exit(result.status === null ? 1 : result.status);
