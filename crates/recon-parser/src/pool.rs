//! Thread-safe parser pool (Parser is Send but not Sync).

use crossbeam_queue::ArrayQueue;
use tree_sitter::{Language, Parser};

/// A pool of tree-sitter parsers for one language.
pub struct ParserPool {
    lang: Language,
    pool: ArrayQueue<Parser>,
}

impl ParserPool {
    /// Create a pool with the given capacity.
    pub fn new(lang: Language, capacity: usize) -> Self {
        Self {
            lang,
            pool: ArrayQueue::new(capacity),
        }
    }

    /// Borrow a parser, run the closure, return the parser.
    pub fn with<R>(&self, f: impl FnOnce(&mut Parser) -> R) -> R {
        let mut parser = self.pool.pop().unwrap_or_else(|| {
            let mut p = Parser::new();
            p.set_language(&self.lang)
                .expect("failed to set language on parser");
            p
        });
        let result = f(&mut parser);
        // Best-effort return; if pool is full, parser is dropped
        let _ = self.pool.push(parser);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::languages::ts_language;
    use recon_core::lang::Language as ReconLang;

    #[test]
    fn pool_reuses_parser() {
        let ts_lang = ts_language(ReconLang::Rust).unwrap();
        let pool = ParserPool::new(ts_lang, 2);

        let tree1 = pool.with(|p| p.parse("fn main() {}", None).unwrap());
        assert!(tree1.root_node().child_count() > 0);

        let tree2 = pool.with(|p| p.parse("struct Foo;", None).unwrap());
        assert!(tree2.root_node().child_count() > 0);
    }
}
