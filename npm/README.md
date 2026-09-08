# anomalift (npm)

Find the mistakes your coding agent repeats, and stop them.

```sh
npx anomalift
```

Or install it:

```sh
npm install -g anomalift
```

## What this package actually is

A shim, and nothing more. It does not contain anomalift, and it cannot build
it. The real program is a single Rust binary; this package downloads nothing at
install time and runs no install scripts. What it does is declare one optional
dependency per platform, each containing one prebuilt binary:

| package                  | os       | cpu     | release asset                  |
| ------------------------ | -------- | ------- | ------------------------------ |
| `anomalift-darwin-arm64` | `darwin` | `arm64` | `anomalift-macos-arm64`        |
| `anomalift-darwin-x64`   | `darwin` | `x64`   | `anomalift-macos-x86_64`       |
| `anomalift-linux-x64`    | `linux`  | `x64`   | `anomalift-linux-x86_64`       |
| `anomalift-linux-arm64`  | `linux`  | `arm64` | `anomalift-linux-arm64`        |
| `anomalift-windows-x64`    | `win32`  | `x64`   | `anomalift-windows-x86_64.exe` |

The names are unscoped rather than `@anomalift/*` because a scoped package
needs the npm organisation to exist before the first publish, and publishes
private unless every publish remembers `--access public`. Getting either wrong
ships a wrapper whose dependencies nobody can install.

npm reads the `os` and `cpu` fields, installs the one package that matches the
machine, and silently skips the other four. `bin/anomalift.js` then resolves
that package and execs the binary inside it, forwarding stdio and propagating
the exit code. That last part matters: anomalift is meant to run in scripts and
pre-commit hooks, and a swallowed non-zero exit turns "this found problems"
into "this passed".

You do not need Rust, and nothing is compiled on your machine.

## If you would rather not involve npm

The binary is the product; npm is one of three ways to get it.

```sh
# macOS / Linux
curl -fsSL https://raw.githubusercontent.com/kailash16dev/Anomalift/main/install.sh | sh

# Windows (PowerShell)
irm https://raw.githubusercontent.com/kailash16dev/Anomalift/main/install.ps1 | iex
```

Both verify the published SHA-256 before installing. So does nothing about npm,
which is worth knowing: `npm install` trusts the registry and its own lockfile
integrity hashes instead.

## Windows on ARM

There is no native arm64 Windows build, so `anomalift-windows-x64` is what a
Windows ARM machine gets, running under the OS's x64 emulation. That package
therefore declares `cpu: ["x64", "arm64"]` rather than x64 alone: npm matches
`cpu` against `process.arch`, so an x64-only declaration is skipped outright on
an arm64 machine and the user is told no binary exists for their platform -
while the binary sitting one field away would have run. `install.ps1` makes the
same substitution and says so when it does.

## Publishing a release

One command, and it refuses rather than guesses:

```sh
gh release download v0.1.0 --dir dist        # the workflow's artifacts
scripts/npm-release.sh --assets ./dist --dry-run
scripts/npm-release.sh --assets ./dist
```

The release workflow also runs this on a tag when an `NPM_TOKEN` secret exists,
so publishing by hand is the fallback rather than the routine.

What the script checks before anything leaves the machine, because
`npm publish` cannot be taken back — npm refuses to reuse a version number even
after `npm unpublish`:

- **every asset is present**, so a half-published release is impossible
- **all six manifests agree on the version.** The wrapper pins its optional
  dependencies exactly; a mismatch means npm installs no platform package at
  all and every user gets "platform package is not installed"
- **no binary carries local paths.** The compiler bakes absolute source paths
  into panic locations, so a laptop build published as-is puts the
  maintainer's home directory in front of everyone. The workflow remaps them
- **the executable bit is set.** npm preserves the mode from the tarball, and a
  binary published without it fails with `EACCES` on every machine, after the
  version is already public
- **each tarball contains exactly what it should** — the binary and the
  manifest, nothing else
- **platform packages publish first, wrapper last.** The other order leaves a
  window where `npm install anomalift` resolves to a version whose
  dependencies do not exist yet

Then verify from an empty directory:

```sh
npx --yes anomalift@latest --version
```
