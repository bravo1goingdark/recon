//! Thread-safe parser pool (Parser is Send but not Sync).

use ahash::AHashMap;
use crossbeam_queue::ArrayQueue;
use recon_core::lang::Language;
use tracing::error;
use tree_sitter::{Language as TsLanguage, Parser};

/// A pool of tree-sitter parsers for one language.
pub struct ParserPool {
    lang: TsLanguage,
    pool: ArrayQueue<Parser>,
}

impl ParserPool {
    /// Create a pool with the given capacity.
    pub fn new(lang: TsLanguage, capacity: usize) -> Self {
        Self {
            lang,
            pool: ArrayQueue::new(capacity),
        }
    }

    /// Borrow a parser, run the closure, return the parser.
    ///
    /// If a fresh parser cannot have its language set (ABI mismatch), an error
    /// is logged and the closure still runs — `parser.parse()` will return `None`,
    /// which callers already handle as "no tree available".
    pub fn with<R>(&self, f: impl FnOnce(&mut Parser) -> R) -> R {
        let mut parser = self.pool.pop().unwrap_or_else(|| {
            let mut p = Parser::new();
            if let Err(e) = p.set_language(&self.lang) {
                error!("failed to set language on pooled parser (ABI mismatch?): {e}");
            }
            p
        });
        let result = f(&mut parser);
        // Best-effort return; if pool is full, parser is dropped
        let _ = self.pool.push(parser);
        result
    }
}

/// Registry of parser pools, one per language. Thread-safe for rayon.
pub struct LanguagePools {
    pools: AHashMap<Language, ParserPool>,
}

impl LanguagePools {
    /// Create pools for all supported languages.
    pub fn new(capacity_per_lang: usize) -> Self {
        let mut pools = AHashMap::with_capacity(9);
        let langs = [
            Language::Rust,
            Language::Python,
            Language::TypeScript,
            Language::Tsx,
            Language::JavaScript,
            Language::Go,
            Language::Java,
            Language::C,
            Language::Cpp,
        ];
        for lang in &langs {
            if let Some(ts_lang) = crate::languages::ts_language(*lang) {
                pools.insert(*lang, ParserPool::new(ts_lang, capacity_per_lang));
            }
        }
        Self { pools }
    }

    /// Get the pool for a language.
    pub fn get(&self, lang: Language) -> Option<&ParserPool> {
        self.pools.get(&lang)
    }
}

// Safety: ParserPool is Send because Parser is Send and ArrayQueue is Send+Sync.
// LanguagePools is Sync because ParserPool::with takes &self and ArrayQueue handles concurrency.
unsafe impl Sync for ParserPool {}

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

    #[test]
    fn language_pools_all_languages() {
        let pools = LanguagePools::new(2);
        assert!(pools.get(ReconLang::Rust).is_some());
        assert!(pools.get(ReconLang::Python).is_some());
        assert!(pools.get(ReconLang::Go).is_some());
        assert!(pools.get(ReconLang::Unknown).is_none());
    }

    #[test]
    fn language_pools_concurrent() {
        use std::sync::Arc;
        let pools = Arc::new(LanguagePools::new(4));

        let handles: Vec<_> = (0..8)
            .map(|_| {
                let pools = pools.clone();
                std::thread::spawn(move || {
                    let pool = pools.get(ReconLang::Rust).unwrap();
                    pool.with(|p| {
                        let tree = p.parse("fn test() {}", None).unwrap();
                        assert!(tree.root_node().child_count() > 0);
                    });
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }
    }
}
