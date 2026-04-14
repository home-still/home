# home-still

Academic research engine: 211M+ vector search with OpenAlex + PMC OA + Qdrant.

## Non-negotiables

- **Distill MUST run on CUDA.** Do not "fall back to CPU" for distill / embedding — not as a quick fix, not temporarily, not as a workaround. `compute_device: cuda` stays in `~/.home-still/config.yaml`. If CUDA is broken, fix CUDA (driver, `libonnxruntime_providers_cuda.so`, pyke-ort `~/.cache/ort.pyke.io/dfbin`, `LD_LIBRARY_PATH`) — don't flip the switch. CPU embedding is too slow to be useful at this corpus size and will silently degrade throughput/latency for every downstream consumer.

## Working Style

- **Guided walkthrough mode.** When the user asks for a "walkthrough", "guide me", or "mentor me", do NOT write the implementation. Propose ONE small chunk with explanation, wait for the user to write it, review their actual code, then propose the next chunk.
- **Execute remote commands directly.** Run SSH / build / deploy commands via Bash yourself. Do not hand shell work back to the user.

## Debugging Philosophy

- Before proposing any fix: state the observed symptom, list 2–3 competing hypotheses with evidence, and name a cheap test that discriminates between them. No code until the diagnosis is confirmed.
- No band-aid guards or legacy-compatibility fallbacks in this greenfield project — fix root causes.

## Release Process

- Before tagging any `rc.*`: run `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo test`. Only tag if all three pass.

## Documentation / Privacy

- Never hardcode real LAN IPs, hostnames, or private network details in docs/READMEs committed to the repo. Use `<host>` or `example.local` placeholders.

## Build & Test

```bash
cargo check                        # Check all crates
cargo clippy --workspace --all-targets -- -D warnings # Fix issues
cargo test                         # Run all tests
cargo test -p paper                # Test paper crate
cargo build --release -p paper     # Build paper CLI
cargo check -p pdf-mash            # Check pdf-mash
```

