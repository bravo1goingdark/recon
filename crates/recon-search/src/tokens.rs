//! Token counting using tiktoken-rs (cl100k_base encoding).
//!
//! Provides accurate BPE token counts for output budget enforcement
//! in `code_repo_map` and tool response `token_estimate` fields.

use std::sync::OnceLock;

static BPE: OnceLock<tiktoken_rs::CoreBPE> = OnceLock::new();

fn bpe() -> &'static tiktoken_rs::CoreBPE {
    BPE.get_or_init(|| tiktoken_rs::cl100k_base().expect("cl100k_base BPE data"))
}

/// Count tokens in `text` using the cl100k_base encoding.
pub fn count_tokens(text: &str) -> usize {
    bpe().encode_ordinary(text).len()
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
}
