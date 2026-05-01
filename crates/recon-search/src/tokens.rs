//! Token counting using tiktoken-rs (cl100k_base encoding).
//!
//! Provides accurate BPE token counts for output budget enforcement
//! in `code_repo_map` and tool response `token_estimate` fields.

use std::sync::OnceLock;

static BPE: OnceLock<Option<tiktoken_rs::CoreBPE>> = OnceLock::new();

fn bpe() -> Option<&'static tiktoken_rs::CoreBPE> {
    BPE.get_or_init(|| tiktoken_rs::cl100k_base().ok()).as_ref()
}

/// Count tokens in `text` using the cl100k_base encoding.
///
/// Returns a heuristic estimate if the BPE model fails to load.
pub fn count_tokens(text: &str) -> usize {
    match bpe() {
        Some(bpe) => bpe.encode_ordinary(text).len(),
        None => estimate_tokens(text),
    }
}

/// Eagerly load the cl100k_base BPE merge table so the first
/// `count_tokens` call doesn't pay the ~100 ms initialization cost.
/// Call once at server startup. Idempotent — `OnceLock` guarantees
/// the heavy load runs at most once across the whole process.
///
/// Without this, the first agent tool call after `recon serve`
/// boot would see a p99 spike (the merge-table load lands on the
/// hot path instead of during init); see `bench-watcher 50` results
/// in `docs/PERF_BASELINE.md`.
pub fn prewarm() {
    let _ = bpe();
}

/// Soft cap on BPE-encoded bytes per call. Files larger than this
/// are encoded up to the cap and the result is linearly extrapolated.
/// Set so that p99 of `count_tokens_capped` stays under ~2 ms on
/// commodity hardware (cl100k_base encodes at roughly 4–5 MB/s).
pub const COUNT_TOKENS_CAP_BYTES: usize = 32 * 1024;

/// Bounded-cost token count. For payloads at or below
/// [`COUNT_TOKENS_CAP_BYTES`] this is identical to [`count_tokens`].
/// For larger payloads, the first `COUNT_TOKENS_CAP_BYTES` are
/// BPE-encoded and the count is linearly scaled to the full byte
/// length. Caps p99 latency for the measured-baseline path so a
/// pathological 1 MB file doesn't blow up a single `code_outline`
/// call by 50–100 ms.
///
/// The byte cutoff is moved back to the nearest UTF-8 char boundary
/// — slicing through a multi-byte codepoint would panic — so the
/// effective encoded slice may be a few bytes shorter than the
/// nominal cap.
pub fn count_tokens_capped(text: &str) -> usize {
    if text.len() <= COUNT_TOKENS_CAP_BYTES {
        return count_tokens(text);
    }
    let mut cut = COUNT_TOKENS_CAP_BYTES;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    if cut == 0 {
        // Pathological: a single codepoint > 32 KB. Fall back to
        // the full encode rather than reporting zero.
        return count_tokens(text);
    }
    let head = &text[..cut];
    let head_tokens = count_tokens(head) as u64;
    // Linear extrapolation: tokens-per-byte from the head, applied
    // to the full byte length. Slight skew on files whose
    // tokenization shape changes mid-file (e.g. code header + huge
    // base64 blob), but the alternative — paying the full encode —
    // is exactly what the cap exists to avoid.
    let scaled = head_tokens.saturating_mul(text.len() as u64) / cut as u64;
    scaled as usize
}

/// Fast heuristic token estimate (~4 chars per token for code).
/// Use in tight loops where accuracy can be verified once at the end.
pub fn estimate_tokens(text: &str) -> usize {
    // Code averages ~3.5-4.5 chars/token with cl100k_base.
    // Use 4 as a conservative divisor (slightly overestimates).
    text.len().div_ceil(4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_string() {
        assert_eq!(count_tokens(""), 0);
    }

    #[test]
    fn basic_code() {
        let tokens = count_tokens("fn main() {}");
        assert!(tokens > 0 && tokens < 20);
    }

    #[test]
    fn more_accurate_than_heuristic() {
        let code = "pub fn validate_email_address(email: &str) -> Result<bool, Error> { todo!() }";
        let heuristic = code.len() / 4;
        let actual = count_tokens(code);
        assert!(actual > 0);
        // tiktoken gives a meaningfully different count from len/4
        assert_ne!(actual, heuristic);
    }

    #[test]
    fn capped_matches_full_below_cap() {
        let code = "fn add(a: i32, b: i32) -> i32 { a + b }\n".repeat(50);
        assert!(code.len() < COUNT_TOKENS_CAP_BYTES);
        assert_eq!(count_tokens_capped(&code), count_tokens(&code));
    }

    #[test]
    fn capped_extrapolates_above_cap() {
        // Build a payload well above the cap with regular structure
        // so the linear extrapolation has a fair shot. The exact
        // count won't match a full encode — that's the point —
        // but it must be in the right ballpark.
        let unit = "fn add(a: i32, b: i32) -> i32 { a + b }\n";
        let big = unit.repeat(2_000); // ~80 KB, well over 32 KB cap
        assert!(big.len() > COUNT_TOKENS_CAP_BYTES * 2);

        let capped = count_tokens_capped(&big);
        let full = count_tokens(&big);
        // Within ±25 % of the true value — the cap trades exactness
        // for predictable cost on huge files.
        let lo = full * 75 / 100;
        let hi = full * 125 / 100;
        assert!(
            capped >= lo && capped <= hi,
            "capped={capped} not in [{lo}, {hi}] (full={full})"
        );
    }

    #[test]
    fn capped_handles_utf8_boundary_near_cut() {
        // A payload whose nominal cap byte falls inside a multi-byte
        // codepoint. Should not panic; should still produce a count
        // in the right ballpark.
        let prefix = "x".repeat(COUNT_TOKENS_CAP_BYTES - 1);
        // 'é' is 2 bytes (0xC3 0xA9); placing it at index CAP-1
        // means the cap falls between its bytes.
        let text = format!("{prefix}é{}", "y".repeat(10_000));
        let _ = count_tokens_capped(&text); // must not panic
    }

    #[test]
    fn capped_empty_is_zero() {
        assert_eq!(count_tokens_capped(""), 0);
    }
}
