# home-still

Academic research engine: 211M+ vector search with OpenAlex + PMC OA + Qdrant.

## Build & Test

```bash
cargo check                        # Check all crates
cargo clippy                       # Fix issues
cargo test                         # Run all tests
cargo test -p paper                # Test paper crate
cargo build --release -p paper     # Build paper CLI
cargo check -p pdf-mash            # Check pdf-mash
```

