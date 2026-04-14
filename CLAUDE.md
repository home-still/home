# home-still

Academic research engine: 211M+ vector search with OpenAlex + PMC OA + Qdrant.

## Non-negotiables

- **Distill MUST run on CUDA.** Do not "fall back to CPU" for distill / embedding — not as a quick fix, not temporarily, not as a workaround. `compute_device: cuda` stays in `~/.home-still/config.yaml`. If CUDA is broken, fix CUDA (driver, `libonnxruntime_providers_cuda.so`, pyke-ort `~/.cache/ort.pyke.io/dfbin`, `LD_LIBRARY_PATH`) — don't flip the switch. CPU embedding is too slow to be useful at this corpus size and will silently degrade throughput/latency for every downstream consumer.

## Build & Test

```bash
cargo check                        # Check all crates
cargo clippy --workspace --all-targets -- -D warnings # Fix issues
cargo test                         # Run all tests
cargo test -p paper                # Test paper crate
cargo build --release -p paper     # Build paper CLI
cargo check -p pdf-mash            # Check pdf-mash
```

