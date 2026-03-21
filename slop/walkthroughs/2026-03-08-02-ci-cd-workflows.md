# Walkthrough: CI/CD Workflows

**Date:** 2026-03-08
**Status:** Planning
**Checkpoint:** ea5ec4aa1f7eaa92ff3b6235e16e24432b9604bb
**Walkthrough:** 2 of 3
**Prerequisite:** Complete Walkthrough 1 (rename)

## Goal

Create GitHub Actions workflows for continuous integration and cross-platform release builds, so a single `git tag v*` push compiles binaries for 5 targets and publishes a GitHub Release.

## Acceptance Criteria

- [ ] `.github/workflows/ci.yml` runs fmt, clippy, test on 3 OSes on every push/PR
- [ ] `.github/workflows/release.yml` builds 5 platform binaries on tag push
- [ ] GitHub Release is created with archives + SHA256 checksums
- [ ] Release workflow includes (commented-out) jobs for Homebrew, Scoop, WinGet
- [ ] CI passes on a push to main
- [ ] Release passes on a `v0.0.1-rc.1` tag

## Technical Approach

### Architecture

Two workflows:
- **CI** — runs on every push to `main` and every PR. Checks formatting, linting, and tests across all 3 OS families.
- **Release** — runs on `v*` tags only. Builds release binaries for 5 targets, creates a GitHub Release with archives and checksums, then triggers package manager updates (initially commented out).

### Key Decisions

- **cargo-zigbuild** for ARM64 Linux (not `cross`): Actively maintained, no Docker overhead, supports glibc version pinning
- **Native runners** for everything else: GitHub has macOS Intel (macos-13), macOS ARM (macos-14), and Windows runners
- **Two-phase release**: Build matrix uploads artifacts → single release job publishes. Avoids race conditions with parallel release creation.
- **`softprops/action-gh-release@v2`**: Standard action for GitHub Releases. Pin to `@v2` (avoid v2.3.0-v2.3.1 which had crashes)

### Dependencies

- `dtolnay/rust-toolchain@stable` — Rust installer
- `Swatinem/rust-cache@v2` — Cargo cache
- `softprops/action-gh-release@v2` — Release creator
- `cargo-zigbuild` + `ziglang` (pip) — ARM64 Linux cross-compilation

### Files to Create

- `.github/workflows/ci.yml`
- `.github/workflows/release.yml`

## Build Order

1. **CI workflow**: Get basic checks running first
2. **Release workflow (build + release only)**: Cross-compilation matrix
3. **Test with RC tag**: Validate before adding package manager jobs
4. **Add package manager jobs (commented out)**: Ready for Walkthrough 3

---

## Steps

### Step 1: Create the directory structure

```bash
mkdir -p .github/workflows
```

---

### Step 2: Create CI workflow

**What you'll build:** A workflow that checks code quality on every push and PR
**File:** `.github/workflows/ci.yml`

```yaml
name: CI

on:
  push:
    branches: [main]
  pull_request:

env:
  CARGO_TERM_COLOR: always

jobs:
  check:
    name: Check (${{ matrix.os }})
    runs-on: ${{ matrix.os }}
    strategy:
      matrix:
        os: [ubuntu-latest, macos-latest, windows-latest]
    steps:
      - uses: actions/checkout@v4
        with:
          submodules: true

      - uses: dtolnay/rust-toolchain@stable
        with:
          components: rustfmt, clippy

      - uses: Swatinem/rust-cache@v2

      - run: cargo fmt --all --check
      - run: cargo clippy --workspace --all-targets -- -D warnings
      - run: cargo test --workspace
```

**Key details:**
- `submodules: true` — clones the hs-style submodule (uses the HTTPS URL from `.gitmodules`)
- Matrix runs on all 3 OS families — catches platform-specific issues early
- `clippy -- -D warnings` — treats warnings as errors (enforces clean code)

**Verify:** Commit and push to main. Check the Actions tab on GitHub — should see 3 green checks.

---

### Step 3: Create release workflow

**What you'll build:** A cross-platform build matrix that creates GitHub Releases
**File:** `.github/workflows/release.yml`

This is the longest file. Key sections explained below.

```yaml
name: Release

on:
  push:
    tags: ["v*"]

permissions:
  contents: write

env:
  CARGO_TERM_COLOR: always
  TOOL: paper
  CRATE: paper-cli

jobs:
  # ── Phase 1: Build all platform binaries ──────────────────────────────────
  build:
    name: Build ${{ matrix.target }}
    runs-on: ${{ matrix.runner }}
    strategy:
      fail-fast: false
      matrix:
        include:
          - target: x86_64-apple-darwin
            runner: macos-13
            archive: tar.gz
          - target: aarch64-apple-darwin
            runner: macos-14
            archive: tar.gz
          - target: x86_64-unknown-linux-gnu
            runner: ubuntu-latest
            archive: tar.gz
          - target: aarch64-unknown-linux-gnu
            runner: ubuntu-latest
            archive: tar.gz
            use_zigbuild: true
          - target: x86_64-pc-windows-msvc
            runner: windows-latest
            archive: zip

    steps:
      - uses: actions/checkout@v4
        with:
          submodules: true

      - uses: dtolnay/rust-toolchain@stable

      - uses: Swatinem/rust-cache@v2
        with:
          key: ${{ matrix.target }}

      - name: Install cargo-zigbuild + zig (ARM64 Linux only)
        if: matrix.use_zigbuild
        run: |
          pip3 install ziglang --break-system-packages
          cargo install cargo-zigbuild

      - name: Add target
        run: rustup target add ${{ matrix.target }}

      - name: Build (zigbuild)
        if: matrix.use_zigbuild
        run: >
          cargo zigbuild --release
          --target ${{ matrix.target }}.2.28
          -p ${{ env.CRATE }}

      - name: Build (native)
        if: ${{ !matrix.use_zigbuild }}
        run: >
          cargo build --release
          --target ${{ matrix.target }}
          -p ${{ env.CRATE }}

      - name: Package (Unix)
        if: matrix.archive == 'tar.gz'
        shell: bash
        run: |
          ASSET="${{ env.TOOL }}-${{ github.ref_name }}-${{ matrix.target }}.tar.gz"
          tar -czf "$ASSET" \
            -C "target/${{ matrix.target }}/release" "${{ env.TOOL }}"
          shasum -a 256 "$ASSET" > "$ASSET.sha256"
          echo "ASSET=$ASSET" >> $GITHUB_ENV

      - name: Package (Windows)
        if: matrix.archive == 'zip'
        shell: pwsh
        run: |
          $asset = "${{ env.TOOL }}-${{ github.ref_name }}-${{ matrix.target }}.zip"
          Compress-Archive `
            -Path "target\${{ matrix.target }}\release\${{ env.TOOL }}.exe" `
            -DestinationPath $asset
          $hash = (Get-FileHash $asset -Algorithm SHA256).Hash.ToLower()
          "$hash  $asset" | Out-File -Encoding ascii "$asset.sha256"
          echo "ASSET=$asset" | Out-File -FilePath $env:GITHUB_ENV -Append

      - uses: actions/upload-artifact@v4
        with:
          name: ${{ env.TOOL }}-${{ matrix.target }}
          path: |
            ${{ env.ASSET }}
            ${{ env.ASSET }}.sha256

  # ── Phase 2: Create GitHub Release ────────────────────────────────────────
  release:
    name: Publish GitHub Release
    needs: build
    runs-on: ubuntu-latest
    outputs:
      version: ${{ github.ref_name }}
    steps:
      - uses: actions/download-artifact@v4
        with:
          path: artifacts
          merge-multiple: true

      - name: Combine checksums
        run: cat artifacts/*.sha256 > checksums-sha256.txt

      - uses: softprops/action-gh-release@v2
        with:
          files: |
            artifacts/*
            checksums-sha256.txt
          generate_release_notes: true
          draft: false
          fail_on_unmatched_files: true

  # ── Phase 3: Update Homebrew tap (uncomment when tap repo is ready) ──────
  # homebrew:
  #   name: Update Homebrew tap
  #   needs: release
  #   runs-on: ubuntu-latest
  #   steps:
  #     - name: Trigger tap update
  #       run: |
  #         gh workflow run update-formula.yml \
  #           --repo home-still/homebrew-tap \
  #           --field tool=${{ env.TOOL }} \
  #           --field version=${{ needs.release.outputs.version }} \
  #           --field repo=home-still/${{ env.TOOL }}
  #       env:
  #         GH_TOKEN: ${{ secrets.TAP_GITHUB_TOKEN }}

  # ── Phase 4: Update Scoop bucket (uncomment when bucket repo is ready) ───
  # scoop:
  #   name: Update Scoop bucket
  #   needs: release
  #   runs-on: ubuntu-latest
  #   steps:
  #     - name: Trigger bucket update
  #       run: |
  #         gh workflow run update-manifest.yml \
  #           --repo home-still/scoop-bucket \
  #           --field tool=${{ env.TOOL }} \
  #           --field version=${{ needs.release.outputs.version }}
  #       env:
  #         GH_TOKEN: ${{ secrets.SCOOP_GITHUB_TOKEN }}

  # ── Phase 5: Submit WinGet manifest (uncomment after first manual submit) ─
  # winget:
  #   name: Submit WinGet manifest
  #   needs: release
  #   runs-on: ubuntu-latest
  #   steps:
  #     - uses: vedantmgoyal9/winget-releaser@main
  #       with:
  #         identifier: HomeStill.Paper
  #         token: ${{ secrets.WINGET_TOKEN }}
```

**Key details to understand:**

- **`fail-fast: false`** — If one target fails, the others still complete. You'll see which platform broke.
- **`use_zigbuild` flag** — Only the ARM64 Linux target uses zigbuild. The `.2.28` suffix pins to glibc 2.28 (RHEL 8 / Ubuntu 18.04 compatible).
- **Two packaging steps** — Unix gets `.tar.gz`, Windows gets `.zip`. Each generates its own `.sha256` file.
- **Artifact upload/download** — Each build job uploads its archive. The release job downloads all of them with `merge-multiple: true`.
- **`permissions: contents: write`** — Required for the release job to create a GitHub Release.
- **Package manager jobs** — Commented out. You'll uncomment them in Walkthrough 3 after setting up the tap/bucket repos.

**Verify:** Don't push yet — test with an RC tag in the next step.

---

### Step 4: Commit and push

```bash
git add .github/
git commit -m "ci: add CI and release workflows"
git push
```

Wait for CI to go green on the push to main. Check the Actions tab.

---

### Step 5: Test with a release candidate tag

```bash
git tag v0.0.1-rc.1
git push origin v0.0.1-rc.1
```

Watch the Actions tab. You should see:
1. Five build jobs running in parallel
2. A release job that creates a GitHub Release
3. The release should contain:
   - `paper-v0.0.1-rc.1-x86_64-apple-darwin.tar.gz` + `.sha256`
   - `paper-v0.0.1-rc.1-aarch64-apple-darwin.tar.gz` + `.sha256`
   - `paper-v0.0.1-rc.1-x86_64-unknown-linux-gnu.tar.gz` + `.sha256`
   - `paper-v0.0.1-rc.1-aarch64-unknown-linux-gnu.tar.gz` + `.sha256`
   - `paper-v0.0.1-rc.1-x86_64-pc-windows-msvc.zip` + `.sha256`
   - `checksums-sha256.txt`

**If a build fails:** Read the error log, fix, commit, and tag `v0.0.1-rc.2`.

**Verify:** Download one of the archives (e.g., the macOS ARM one), extract it, and run `./paper --version`.

---

## Known Dragons

- **`softprops/action-gh-release` v2.3.0-v2.3.1** had assertion crashes. If you pin a specific version, use v2.5.0+.
- **`ring` crate** needs a C compiler on each target. Native runners provide this automatically. zigbuild handles it for ARM64 Linux.
- **Fat LTO builds are slow** — expect 5-15 min per target. The matrix runs in parallel so wall-clock is ~15 min total.
- **`shasum` vs `sha256sum`** — macOS uses `shasum -a 256`, Linux uses `sha256sum`. The workflow uses `shasum` which works on macOS runners. For Linux targets, `sha256sum` is also available.

---

*Plan created: 2026-03-08*
*Implementation proven: [to be updated]*
*User implementation started: [to be updated]*
