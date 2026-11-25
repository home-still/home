# Contributing to home-still

This page is for people changing home-still: building it, running the gates,
cutting a release and writing its documentation. To use home-still, start at
the [documentation index](docs/README.md).

## Build and test

The toolchain is pinned in `rust-toolchain.toml`; `rustup` installs it on the
first `cargo` call.

```sh
cargo build -p hs                    # the hs CLI, version from `git describe`
cargo build -p hs-gateway            # the cloud gateway
cargo build -p hs-mcp                # the MCP server
cargo check -p hs-scribe --features server    # the scribe server
cargo check -p hs-distill --features server   # the distill server
```

A `--release` build bakes its version from the tag it ships as, so it needs
`HS_RELEASE_TAG`. Without it the build of `hs`, `hs-gateway`, `hs-mcp`,
`hs-scribe`, `hs-distill`, or anything that depends on the last two, fails on
purpose (the rules are in `build-support/version.rs`):

```sh
HS_RELEASE_TAG=v0.0.1-rc.N cargo build --release -p hs
./target/release/hs --version        # must print the tag
```

The distill server only builds with `--features cuda` (or `server` plus
`cuda`): `crates/hs-distill/src/server_main.rs` stops a CPU-only build with a
`compile_error!`.

### The gates

Every push runs these, exactly as `.github/workflows/ci.yaml` does. Run them
before you push:

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --exclude hs-scribe --all-targets -- -D warnings
cargo clippy --locked -p hs-scribe -- -D warnings
cargo test --locked --workspace --exclude hs-scribe

# Linux only, feature-gated server code:
cargo clippy --locked -p hs-scribe --features server --all-targets -- -D warnings
cargo test --locked -p hs-scribe --features server
cargo clippy --locked -p hs-scribe --features cuda --all-targets -- -D warnings
cargo test --locked -p hs-scribe --features cuda --lib
cargo clippy --locked -p hs-scribe --features eval --all-targets -- -D warnings
cargo test --locked -p hs-scribe --features eval --lib eval
cargo clippy --locked -p hs-distill --features cuda --all-targets -- -D warnings
cargo test --locked -p hs-distill --features server --lib
```

To prove the tests do not reach the network, run them in a network namespace.
Bring the loopback interface up first, or every test that starts a local HTTP
stand-in fails with `NetworkUnreachable`:

```sh
unshare -rn sh -c 'ip link set lo up && cargo test --locked --offline --workspace --exclude hs-scribe'
```

## Releasing

1. Pass every gate above on the commit you release.
2. Tag it `v0.0.1-rc.N` and push the tag:
   ```sh
   git tag v0.0.1-rc.N && git push origin v0.0.1-rc.N
   ```
3. GitHub Actions (`.github/workflows/release.yaml`) reruns the gates, builds
   every artifact, checks each binary's version and architecture, and
   publishes a GitHub Release.
4. Each host installs it with:
   ```sh
   hs upgrade --pre --yes
   ```

Never copy a locally built binary to a host. If `hs upgrade` does not cover a
path, fix `hs upgrade`. Tags are immutable: a broken release is fixed by the
next `rc`.

Record anything an operator must do when upgrading in
[`CHANGELOG.md`](CHANGELOG.md).

## Writing documentation

The user documentation lives in `docs/`, indexed by
[`docs/README.md`](docs/README.md). The root `README.md` is the project's front
page, and each crate's README is a short page for developers of that crate.
Every page follows these rules.

1. **Every page has exactly one kind, decided by its directory** (the
   [Diátaxis](https://diataxis.fr) framework):
   - `docs/tutorials/` is a learning path. It has numbered steps and one way
     through; every step has a command and what you should see next. It offers
     no options or alternatives.
   - `docs/how-to/` is a task. The title is the task, then `## Before you
     start` lists the prerequisites, then numbered steps, then `## Check it
     worked`.
   - `docs/reference/` is complete and terse and mirrors the code's structure.
     It uses tables and gives no instructions or opinions.
   - `docs/explanation/` covers why and how it works, in prose, with no step
     lists.
2. **Each fact has one home.** Tables of flags, keys, ports, endpoints and
   tools live in `docs/reference/` only. Other pages link there instead of
   copying.
3. **Show examples.** Every command a page mentions appears as a runnable
   example with the real flag names from `hs <cmd> --help`.
4. **No release history outside `CHANGELOG.md`.** Phrases like "since rc.N",
   "used to", "new keys", "no longer" or "before, …" belong there and nowhere
   else. A page describes how things are.
5. **No counts that drift.** Do not write how many tools, commands or
   providers exist; link the reference page.
6. **Privacy** (see `CLAUDE.md`). Never write a real LAN IP, subnet, hostname
   or secret.
   - Hosts are role placeholders: `<gateway-host>`, `<storage-host>`,
     `<gpu-host>`, `<qdrant-host>`, `<scribe-host>`, `<laptop>`.
   - Subnets are `<lan-cidr>`; tokens and keys are `<token>`.
   - Example addresses use `192.0.2.x`, the documentation range.
7. **Voice.** Second person, present tense, sentence-case headings.
8. **No links from user docs into internal records** (`docs/loop/`,
   `BACKLOG.md`).
9. **Drift tests keep the reference honest:**
   - every `hs` subcommand has a heading ``### `hs <path>` `` in
     [`docs/reference/cli.md`](docs/reference/cli.md), and every such heading
     is a real command (`crates/hs/src/docs_tests.rs`);
   - every `hs …` command shown in a code block or inline code in the user
     docs walks a real subcommand path (same file);
   - every MCP tool has a heading ``### `<tool>` `` in
     [`docs/reference/mcp-tools.md`](docs/reference/mcp-tools.md), and every
     such heading is a real tool (`docs_tests` in `crates/hs-mcp/src/main.rs`);
   - every `HS_SCRIBE_*` and `HS_DISTILL_*` variable the docs name maps to a
     real setting (`every_documented_variable_*` in each crate's
     `src/config.rs`).

   When you add, rename or remove a command or tool, these tests fail until
   the reference matches.

### Citing the sources of these rules

The rules above come from these works:

- Lee, B. D. (2018). *Ten simple rules for documenting scientific software.*
  PLoS Computational Biology 14(12): e1006561.
  [doi:10.1371/journal.pcbi.1006561](https://doi.org/10.1371/journal.pcbi.1006561).
  The source of rules 2, 3 and 9: write a quick start, give many examples,
  treat the README as the homepage, document the `--help` output, keep the
  docs in version control and check them with tools.
- Prana, G. A. A., Treude, C., Thung, F., Atapattu, T. and Lo, D. (2018).
  *Categorizing the content of GitHub README files.* Empirical Software
  Engineering 24: 1296–1327.
  [doi:10.1007/s10664-018-9660-3](https://doi.org/10.1007/s10664-018-9660-3).
  READMEs usually say what a project is and how to use it but often omit why
  it exists and its status; the root README covers both.
- Tan, W. S., Wagner, M. and Treude, C. (2023). *Detecting outdated code
  element references in software repository documentation.* Empirical
  Software Engineering 29: 5.
  [doi:10.1007/s10664-023-10397-6](https://doi.org/10.1007/s10664-023-10397-6).
  References to code in documentation go stale often, which is why rule 9
  tests them.
- Carroll, J. M. (2014). *Creating minimalist instruction.* International Journal of
  Designs for Learning 5(2).
  [doi:10.14434/ijdl.v5i2.12887](https://doi.org/10.14434/ijdl.v5i2.12887).
  Guide real tasks, let readers act at once, and treat errors and recovery as
  part of the task: the shape of the tutorials and how-to guides.
- Procida, D. *Diátaxis: a systematic approach to technical documentation
  authoring.* [https://diataxis.fr](https://diataxis.fr). Four needs map to
  four forms: tutorials, how-to guides, reference and explanation (rule 1).
