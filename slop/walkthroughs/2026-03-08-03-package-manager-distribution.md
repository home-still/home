# Walkthrough: Package Manager Distribution

**Date:** 2026-03-08
**Status:** Planning
**Checkpoint:** ea5ec4aa1f7eaa92ff3b6235e16e24432b9604bb
**Walkthrough:** 3 of 3
**Prerequisites:** Complete Walkthroughs 1 (rename) and 2 (CI/CD workflows)

## Goal

Set up Homebrew, Scoop, and WinGet distribution so users can install `paper` with a single command on any platform, and releases auto-update all package managers.

## Acceptance Criteria

- [ ] `brew install home-still/tap/paper` works on macOS/Linux
- [ ] `scoop install home-still/paper` works on Windows
- [ ] WinGet first submission completed (`winget install HomeStill.Paper`)
- [ ] Package manager jobs uncommented in release.yml
- [ ] A new tag auto-updates all package managers
- [ ] `cargo install --git https://github.com/home-still/paper paper-cli` still works

## Technical Approach

### Architecture

Three external repos + secrets, wired into the release workflow:

```
home-still/paper (release.yml)
  ├── triggers → home-still/homebrew-tap (update-formula.yml)
  ├── triggers → home-still/scoop-bucket (update-manifest.yml)
  └── uses    → vedantmgoyal9/winget-releaser (submits PR to microsoft/winget-pkgs)
```

### Key Decisions

- **Scoop + WinGet** for Windows (not Chocolatey): No moderation delays, no admin rights required
- **Template-based formula/manifest**: Each repo stores a template with `__VERSION__` / `__SHA256_*__` placeholders. Update workflows substitute real values from the release checksums.
- **`workflow_dispatch`** trigger: The paper repo's release workflow triggers the tap/bucket workflows via `gh workflow run`. This is more reliable than `repository_dispatch`.

### Secrets Required

| Secret | Where to set | How to get |
|--------|-------------|------------|
| `TAP_GITHUB_TOKEN` | `home-still/paper` repo → Settings → Secrets | GitHub → Settings → Developer settings → PAT (Classic), `repo` scope on `home-still/homebrew-tap` |
| `SCOOP_GITHUB_TOKEN` | `home-still/paper` repo → Settings → Secrets | Same PAT works if it has `repo` scope on `home-still/scoop-bucket` |
| `WINGET_TOKEN` | `home-still/paper` repo → Settings → Secrets | PAT with `public_repo` scope (for PRs to `microsoft/winget-pkgs`) |

**Tip:** A single PAT with `repo` scope on the home-still org covers both `TAP_GITHUB_TOKEN` and `SCOOP_GITHUB_TOKEN`.

---

## Steps

### Step 1: Create the Homebrew tap repo

**What you'll build:** A GitHub repo that Homebrew uses to find your formula

1. Go to github.com/organizations/home-still → New repository
2. Name: `homebrew-tap` (the `homebrew-` prefix is a Homebrew convention)
3. Public, no template, initialize with README

Then clone it locally:
```bash
git clone git@github.com:home-still/homebrew-tap.git /tmp/homebrew-tap
cd /tmp/homebrew-tap
mkdir -p Formula .github/workflows
```

---

### Step 2: Create the Homebrew formula template

**File:** `Formula/paper.rb.template` (in the `homebrew-tap` repo)

```ruby
class Paper < Formula
  desc "Meta-search tool for academic papers"
  homepage "https://github.com/home-still/paper"
  version "__VERSION__"
  license "GPL-3.0-only"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/home-still/paper/releases/download/v__VERSION__/paper-v__VERSION__-aarch64-apple-darwin.tar.gz"
      sha256 "__SHA256_MACOS_ARM64__"
    else
      url "https://github.com/home-still/paper/releases/download/v__VERSION__/paper-v__VERSION__-x86_64-apple-darwin.tar.gz"
      sha256 "__SHA256_MACOS_X86_64__"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/home-still/paper/releases/download/v__VERSION__/paper-v__VERSION__-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "__SHA256_LINUX_ARM64__"
    else
      url "https://github.com/home-still/paper/releases/download/v__VERSION__/paper-v__VERSION__-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "__SHA256_LINUX_X86_64__"
    end
  end

  def install
    bin.install "paper"
  end

  test do
    system "#{bin}/paper", "--version"
  end
end
```

**Key details:**
- `__VERSION__` appears without the `v` prefix (e.g., `0.1.0` not `v0.1.0`)
- URLs include `v__VERSION__` because git tags use the `v` prefix
- The `test` block runs `paper --version` — Homebrew runs this during `brew test`

---

### Step 3: Create the Homebrew update workflow

**File:** `.github/workflows/update-formula.yml` (in the `homebrew-tap` repo)

```yaml
name: Update Formula

on:
  workflow_dispatch:
    inputs:
      tool:
        required: true
      version:
        required: true
      repo:
        required: true

jobs:
  update:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - name: Download checksums
        run: |
          gh release download "${{ inputs.version }}" \
            --repo "${{ inputs.repo }}" \
            --pattern "checksums-sha256.txt" \
            --dir /tmp/checksums
        env:
          GH_TOKEN: ${{ github.token }}

      - name: Extract SHAs and render formula
        run: |
          TOOL="${{ inputs.tool }}"
          VERSION="${{ inputs.version }}"
          # Strip the v prefix for the formula version field
          VER_NUM="${VERSION#v}"
          CHECKSUMS=/tmp/checksums/checksums-sha256.txt

          sha_macos_arm64=$(grep  "aarch64-apple-darwin.tar.gz"      "$CHECKSUMS" | awk '{print $1}')
          sha_macos_x86=$(grep    "x86_64-apple-darwin.tar.gz"       "$CHECKSUMS" | awk '{print $1}')
          sha_linux_arm64=$(grep  "aarch64-unknown-linux-gnu.tar.gz" "$CHECKSUMS" | awk '{print $1}')
          sha_linux_x86=$(grep    "x86_64-unknown-linux-gnu.tar.gz"  "$CHECKSUMS" | awk '{print $1}')

          sed \
            -e "s/__VERSION__/${VER_NUM}/g" \
            -e "s/__SHA256_MACOS_ARM64__/${sha_macos_arm64}/g" \
            -e "s/__SHA256_MACOS_X86_64__/${sha_macos_x86}/g" \
            -e "s/__SHA256_LINUX_ARM64__/${sha_linux_arm64}/g" \
            -e "s/__SHA256_LINUX_X86_64__/${sha_linux_x86}/g" \
            "Formula/${TOOL}.rb.template" > "Formula/${TOOL}.rb"

      - name: Commit and push
        run: |
          git config user.name  "github-actions[bot]"
          git config user.email "github-actions[bot]@users.noreply.github.com"
          git add "Formula/${{ inputs.tool }}.rb"
          git commit -m "chore: bump ${{ inputs.tool }} to ${{ inputs.version }}"
          git push
```

**Commit and push the tap repo:**
```bash
cd /tmp/homebrew-tap
git add -A
git commit -m "feat: add paper formula template and update workflow"
git push
```

---

### Step 4: Create the Scoop bucket repo

**What you'll build:** A GitHub repo that Scoop uses to find your package

1. Go to github.com/organizations/home-still → New repository
2. Name: `scoop-bucket`
3. Public, use template: `ScoopInstaller/BucketTemplate` (gives you CI that validates manifests)

Then clone and add files:
```bash
git clone git@github.com:home-still/scoop-bucket.git /tmp/scoop-bucket
cd /tmp/scoop-bucket
mkdir -p bucket .github/workflows
```

---

### Step 5: Create the Scoop manifest

**File:** `bucket/paper.json` (in the `scoop-bucket` repo)

```json
{
  "version": "__VERSION__",
  "description": "Meta-search tool for academic papers",
  "homepage": "https://github.com/home-still/paper",
  "license": "GPL-3.0-only",
  "architecture": {
    "64bit": {
      "url": "https://github.com/home-still/paper/releases/download/v__VERSION__/paper-v__VERSION__-x86_64-pc-windows-msvc.zip",
      "hash": "__SHA256_WINDOWS_X86_64__"
    }
  },
  "bin": "paper.exe",
  "checkver": {
    "github": "https://github.com/home-still/paper"
  },
  "autoupdate": {
    "architecture": {
      "64bit": {
        "url": "https://github.com/home-still/paper/releases/download/v$version/paper-v$version-x86_64-pc-windows-msvc.zip"
      }
    },
    "hash": {
      "url": "$url.sha256"
    }
  }
}
```

**Key details:**
- `checkver.github` — Scoop's tooling auto-detects new releases from this URL
- `autoupdate` — Scoop can auto-generate PRs for new versions using `shovel` or `scoop-checkver`
- The `__VERSION__` / `__SHA256_*__` placeholders get substituted by the update workflow

---

### Step 6: Create the Scoop update workflow

**File:** `.github/workflows/update-manifest.yml` (in the `scoop-bucket` repo)

```yaml
name: Update Manifest

on:
  workflow_dispatch:
    inputs:
      tool:
        required: true
      version:
        required: true

jobs:
  update:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - name: Download Windows checksum
        run: |
          gh release download "v${{ inputs.version }}" \
            --repo "home-still/${{ inputs.tool }}" \
            --pattern "*windows-msvc.zip.sha256" \
            --dir /tmp/chk
        env:
          GH_TOKEN: ${{ github.token }}

      - name: Update manifest
        run: |
          TOOL="${{ inputs.tool }}"
          VERSION="${{ inputs.version }}"
          # Strip the v prefix
          VER_NUM="${VERSION#v}"
          SHA=$(cat /tmp/chk/*.sha256 | awk '{print $1}')

          jq \
            --arg v  "$VER_NUM" \
            --arg h  "$SHA" \
            --arg url "https://github.com/home-still/${TOOL}/releases/download/v${VER_NUM}/${TOOL}-v${VER_NUM}-x86_64-pc-windows-msvc.zip" \
            '.version = $v | .architecture["64bit"].url = $url | .architecture["64bit"].hash = $h' \
            "bucket/${TOOL}.json" > /tmp/manifest.json
          mv /tmp/manifest.json "bucket/${TOOL}.json"

      - name: Commit and push
        run: |
          git config user.name  "github-actions[bot]"
          git config user.email "github-actions[bot]@users.noreply.github.com"
          git add "bucket/${{ inputs.tool }}.json"
          git commit -m "chore: bump ${{ inputs.tool }} to v${{ inputs.version }}"
          git push
```

**Commit and push the bucket repo:**
```bash
cd /tmp/scoop-bucket
git add -A
git commit -m "feat: add paper manifest and update workflow"
git push
```

---

### Step 7: Configure secrets

Go to `github.com/home-still/paper` → Settings → Secrets and variables → Actions → New repository secret:

1. **`TAP_GITHUB_TOKEN`** — A GitHub PAT (Classic) with `repo` scope that has access to `home-still/homebrew-tap`
2. **`SCOOP_GITHUB_TOKEN`** — Same PAT works if it has `repo` scope on `home-still/scoop-bucket`
3. **`WINGET_TOKEN`** — PAT with `public_repo` scope (for opening PRs against `microsoft/winget-pkgs`)

**To create a PAT:**
GitHub → Settings → Developer settings → Personal access tokens → Tokens (classic) → Generate new token → Select `repo` scope → Generate

---

### Step 8: Uncomment package manager jobs in release.yml

**File:** `.github/workflows/release.yml` (in the `paper` repo)

Uncomment the `homebrew` and `scoop` jobs. Leave `winget` commented until after the first manual submission.

Commit and push:
```bash
git add .github/workflows/release.yml
git commit -m "ci: enable Homebrew and Scoop auto-update jobs"
git push
```

---

### Step 9: Test with an RC tag

```bash
git tag v0.0.2-rc.1
git push origin v0.0.2-rc.1
```

Watch the Actions tab. After the release job completes, check:
1. **homebrew-tap repo** — Should have a new commit with `Formula/paper.rb` (real checksums, not placeholders)
2. **scoop-bucket repo** — Should have a new commit with `bucket/paper.json` (real hash and URL)

**Verify Homebrew locally:**
```bash
brew tap home-still/tap
brew install home-still/tap/paper
paper --version
```

---

### Step 10: WinGet first submission (manual, one-time)

This requires a Windows machine (or VM). Install `komac`:
```powershell
winget install Komac
```

Then submit:
```powershell
komac create --identifier HomeStill.Paper --version 0.1.0 `
  --urls https://github.com/home-still/paper/releases/download/v0.1.0/paper-v0.1.0-x86_64-pc-windows-msvc.zip `
  --submit
```

This opens a PR against `microsoft/winget-pkgs`. Review takes 4-24 hours.

After approval, uncomment the `winget` job in `release.yml` — subsequent versions are fully automated.

---

### Step 11: Full release

Once all channels are wired up:

```bash
git tag v0.1.0
git push origin v0.1.0
```

This triggers the full pipeline:
1. Build 5 binaries
2. Create GitHub Release
3. Update Homebrew formula
4. Update Scoop manifest
5. Submit WinGet PR

**Verify all install methods:**
```bash
# macOS/Linux
brew install home-still/tap/paper
paper --version

# Windows (PowerShell)
scoop bucket add home-still https://github.com/home-still/scoop-bucket
scoop install home-still/paper
paper --version

winget install HomeStill.Paper
paper --version

# Any platform with Rust
cargo install --git https://github.com/home-still/paper paper-cli
paper --version
```

---

## Known Dragons

- **Homebrew formula SHA mismatch**: If the release workflow is interrupted and some artifacts are missing from `checksums-sha256.txt`, the formula will have wrong hashes. Re-run the release workflow.
- **Scoop `__VERSION__` in template**: After the first update, the template placeholders are gone from `bucket/paper.json`. The `jq` workflow replaces by field name, not placeholder, so subsequent updates work fine.
- **WinGet moderation**: First package takes 4-24 hours. Don't resubmit — check the PR on `microsoft/winget-pkgs`.
- **PAT expiration**: GitHub Classic PATs expire. Set a reminder to rotate them. Consider using fine-grained tokens for longer life.
- **Private submodule**: If `hs-style` is ever made private, pass `token: ${{ secrets.TAP_GITHUB_TOKEN }}` to the `actions/checkout` step in both workflows.

---

*Plan created: 2026-03-08*
*Implementation proven: [to be updated]*
*User implementation started: [to be updated]*
