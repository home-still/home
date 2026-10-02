// Included (`include!`) by the main file of every shipped binary: hs,
// hs-gateway, hs-mcp, hs-scribe-server, hs-distill-server.
//
// `env!("HS_VERSION")` alone is compiled into immediate moves, leaving no
// greppable string in the binary. This static is a framed, distinctive byte
// string that cannot be folded away: `.github/scripts/verify-binaries.sh`
// looks for exactly `hs-version-marker:<version>:end`, which works for
// binaries the runner cannot execute (cross-built targets).
const HS_VERSION_MARKER_TEXT: &str = concat!("hs-version-marker:", env!("HS_VERSION"), ":end");

const fn hs_version_marker_bytes<const N: usize>(text: &str) -> [u8; N] {
    let text = text.as_bytes();
    let mut out = [0u8; N];
    let mut i = 0;
    while i < N {
        out[i] = text[i];
        i += 1;
    }
    out
}

#[used]
static HS_VERSION_MARKER: [u8; HS_VERSION_MARKER_TEXT.len()] =
    hs_version_marker_bytes::<{ HS_VERSION_MARKER_TEXT.len() }>(HS_VERSION_MARKER_TEXT);

/// Referenced from `main` so no linker or optimizer pass can drop the marker.
#[inline(never)]
fn keep_version_marker() -> u8 {
    std::hint::black_box(&HS_VERSION_MARKER)[0]
}
