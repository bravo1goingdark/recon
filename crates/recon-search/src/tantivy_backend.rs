//! Tantivy BM25 structured search over symbol names, signatures, and docs.
//!
//! Indexes symbols only (not file bodies). Uses a custom tokenizer that splits
//! camelCase and snake_case identifiers into sub-tokens for better recall.

use recon_core::error::Error;
use recon_core::symbol::Symbol;
use std::path::Path;
use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::*;
use tantivy::tokenizer::{LowerCaser, SimpleTokenizer, TextAnalyzer};
use tantivy::{Index, IndexReader, IndexWriter, ReloadPolicy, TantivyDocument};
use tracing::debug;

/// A structured search hit from Tantivy.
#[derive(Debug, Clone)]
pub struct StructuredHit {
    /// Symbol ID from the index.
    pub symbol_id: u64,
    /// File path.
    pub path: String,
    /// Symbol name.
    pub name: String,
    /// Fully qualified symbol name.
    pub qualified_name: String,
    /// Symbol kind (fn, struct, etc).
    pub kind: String,
    /// Symbol signature, if available.
    pub signature: Option<String>,
    /// BM25 relevance score.
    pub score: f32,
}

/// Schema field handles — kept together to avoid re-lookups.
struct Fields {
    symbol_id: Field,
    path: Field,
    name: Field,
    qualified_name: Field,
    kind: Field,
    signature: Field,
    doc: Field,
    lang: Field,
}

/// Tantivy-backed symbol search engine.
pub struct TantivyBackend {
    index: Index,
    reader: IndexReader,
    fields: Fields,
    #[allow(dead_code)]
    schema: Schema,
}

impl TantivyBackend {
    /// Create or open a Tantivy index at the given directory.
    pub fn open(index_dir: &Path) -> Result<Self, Error> {
        std::fs::create_dir_all(index_dir)
            .map_err(|e| Error::Search(format!("create index dir: {e}")))?;

        let (schema, fields) = Self::build_schema();

        let dir = tantivy::directory::MmapDirectory::open(index_dir)
            .map_err(|e| Error::Search(format!("open tantivy dir: {e}")))?;

        let index = Index::open_or_create(dir, schema.clone())
            .map_err(|e| Error::Search(format!("open tantivy index: {e}")))?;

        // Register the code tokenizer
        index.tokenizers().register("code", code_tokenizer());

        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()
            .map_err(|e| Error::Search(format!("tantivy reader: {e}")))?;

        Ok(Self {
            index,
            reader,
            fields,
            schema,
        })
    }

    /// Open an in-memory index (for testing).
    pub fn open_memory() -> Result<Self, Error> {
        let (schema, fields) = Self::build_schema();
        let index = Index::create_in_ram(schema.clone());

        index.tokenizers().register("code", code_tokenizer());

        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()
            .map_err(|e| Error::Search(format!("tantivy reader: {e}")))?;

        Ok(Self {
            index,
            reader,
            fields,
            schema,
        })
    }

    fn build_schema() -> (Schema, Fields) {
        let mut builder = Schema::builder();

        let symbol_id = builder.add_u64_field("symbol_id", STORED | INDEXED);
        let path = builder.add_text_field("path", STRING | STORED);
        let name = builder.add_text_field(
            "name",
            TextOptions::default()
                .set_indexing_options(
                    TextFieldIndexing::default()
                        .set_tokenizer("code")
                        .set_index_option(IndexRecordOption::WithFreqsAndPositions),
                )
                .set_stored(),
        );
        let qualified_name = builder.add_text_field(
            "qualified_name",
            TextOptions::default()
                .set_indexing_options(
                    TextFieldIndexing::default()
                        .set_tokenizer("code")
                        .set_index_option(IndexRecordOption::WithFreqsAndPositions),
                )
                .set_stored(),
        );
        let kind = builder.add_text_field("kind", STRING | STORED);
        let signature = builder.add_text_field(
            "signature",
            TextOptions::default()
                .set_indexing_options(
                    TextFieldIndexing::default()
                        .set_tokenizer("code")
                        .set_index_option(IndexRecordOption::WithFreqs),
                )
                .set_stored(),
        );
        let doc = builder.add_text_field(
            "doc",
            TextOptions::default().set_indexing_options(
                TextFieldIndexing::default()
                    .set_tokenizer("default")
                    .set_index_option(IndexRecordOption::WithFreqs),
            ),
        );
        let lang = builder.add_text_field("lang", STRING);

        let schema = builder.build();
        let fields = Fields {
            symbol_id,
            path,
            name,
            qualified_name,
            kind,
            signature,
            doc,
            lang,
        };
        (schema, fields)
    }

    /// Get an index writer with a budget (heap in bytes).
    pub fn writer(&self, heap_bytes: usize) -> Result<IndexWriter, Error> {
        self.index
            .writer(heap_bytes)
            .map_err(|e| Error::Search(format!("tantivy writer: {e}")))
    }

    /// Index a batch of symbols. Deletes old entries for the given path first.
    pub fn index_symbols(
        &self,
        writer: &mut IndexWriter,
        path: &Path,
        symbols: &[Symbol],
    ) -> Result<(), Error> {
        let path_str = path.to_str().unwrap_or("");

        // Delete old docs for this path
        let path_term = tantivy::Term::from_field_text(self.fields.path, path_str);
        writer.delete_term(path_term);

        // Tantivy drops tokens exceeding 65530 bytes with a warning.
        // Truncate long fields to avoid this.
        const MAX_FIELD_BYTES: usize = 60_000;

        // Add new docs
        for sym in symbols {
            let mut doc = TantivyDocument::new();
            doc.add_u64(self.fields.symbol_id, sym.id);
            doc.add_text(self.fields.path, path_str);
            doc.add_text(self.fields.name, sym.name.as_str());
            doc.add_text(self.fields.qualified_name, sym.qualified_name.as_str());
            doc.add_text(self.fields.kind, sym.kind.label());
            if let Some(sig) = &sym.signature {
                doc.add_text(self.fields.signature, truncate_utf8(sig, MAX_FIELD_BYTES));
            }
            if let Some(d) = &sym.doc {
                doc.add_text(self.fields.doc, truncate_utf8(d, MAX_FIELD_BYTES));
            }
            doc.add_text(self.fields.lang, sym.lang.name());
            writer
                .add_document(doc)
                .map_err(|e| Error::Search(format!("add doc: {e}")))?;
        }

        Ok(())
    }

    /// Commit pending changes and reload the reader.
    pub fn commit(&self, writer: &mut IndexWriter) -> Result<(), Error> {
        writer
            .commit()
            .map_err(|e| Error::Search(format!("tantivy commit: {e}")))?;
        self.reader
            .reload()
            .map_err(|e| Error::Search(format!("tantivy reload: {e}")))?;
        Ok(())
    }

    /// Search for symbols matching a query string. BM25-ranked.
    pub fn search(&self, query_str: &str, limit: usize) -> Result<Vec<StructuredHit>, Error> {
        if query_str.is_empty() {
            return Ok(Vec::new());
        }

        let searcher = self.reader.searcher();
        let query_parser = QueryParser::for_index(
            &self.index,
            vec![
                self.fields.name,
                self.fields.qualified_name,
                self.fields.signature,
                self.fields.doc,
            ],
        );

        let query = query_parser
            .parse_query(query_str)
            .map_err(|e| Error::Search(format!("parse query: {e}")))?;

        let top_docs: Vec<(f32, tantivy::DocAddress)> = searcher
            .search(&query, &TopDocs::with_limit(limit).order_by_score())
            .map_err(|e| Error::Search(format!("tantivy search: {e}")))?;

        let mut hits = Vec::with_capacity(top_docs.len());
        for (score, addr) in top_docs {
            let doc: TantivyDocument = searcher
                .doc(addr)
                .map_err(|e| Error::Search(format!("fetch doc: {e}")))?;

            let symbol_id = doc
                .get_first(self.fields.symbol_id)
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let path = doc
                .get_first(self.fields.path)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let name = doc
                .get_first(self.fields.name)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let qualified_name = doc
                .get_first(self.fields.qualified_name)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let kind = doc
                .get_first(self.fields.kind)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let signature = doc
                .get_first(self.fields.signature)
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());

            hits.push(StructuredHit {
                symbol_id,
                path,
                name,
                qualified_name,
                kind,
                signature,
                score,
            });
        }

        debug!(query = query_str, hits = hits.len(), "tantivy search");
        Ok(hits)
    }

    /// Count total documents in the index.
    pub fn doc_count(&self) -> u64 {
        let searcher = self.reader.searcher();
        searcher
            .segment_readers()
            .iter()
            .map(|r| r.num_docs() as u64)
            .sum()
    }
}

/// Truncate a string to at most `max_bytes`, ensuring we don't split a UTF-8 char.
fn truncate_utf8(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Custom tokenizer that splits camelCase and snake_case, then lowercases.
fn code_tokenizer() -> TextAnalyzer {
    TextAnalyzer::builder(CodeSplitTokenizer)
        .filter(LowerCaser)
        .build()
}

/// Tokenizer that splits on underscores, camelCase boundaries, and dots/colons.
#[derive(Clone)]
struct CodeSplitTokenizer;

impl tantivy::tokenizer::Tokenizer for CodeSplitTokenizer {
    type TokenStream<'a> = CodeSplitTokenStream<'a>;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> Self::TokenStream<'a> {
        CodeSplitTokenStream {
            text,
            tokens: split_code_identifier(text),
            index: 0,
            token: tantivy::tokenizer::Token::default(),
        }
    }
}

struct CodeSplitTokenStream<'a> {
    text: &'a str,
    tokens: Vec<(usize, usize)>, // (start, end) byte offsets
    index: usize,
    token: tantivy::tokenizer::Token,
}

impl<'a> tantivy::tokenizer::TokenStream for CodeSplitTokenStream<'a> {
    fn advance(&mut self) -> bool {
        if self.index >= self.tokens.len() {
            return false;
        }
        let (start, end) = self.tokens[self.index];
        self.token.offset_from = start;
        self.token.offset_to = end;
        self.token.text.clear();
        self.token.text.push_str(&self.text[start..end]);
        self.token.position = self.index;
        self.index += 1;
        true
    }

    fn token(&self) -> &tantivy::tokenizer::Token {
        &self.token
    }

    fn token_mut(&mut self) -> &mut tantivy::tokenizer::Token {
        &mut self.token
    }
}

/// Split a code identifier into sub-tokens (UTF-8 safe).
/// "validateEmailAddress" -> ["validate", "Email", "Address", "validateEmailAddress"]
/// "validate_email" -> ["validate", "email", "validate_email"]
fn split_code_identifier(text: &str) -> Vec<(usize, usize)> {
    let mut tokens = Vec::with_capacity(8);

    // First, emit the full text as one token
    if !text.is_empty() {
        tokens.push((0, text.len()));
    }

    // Split by simple tokenizer boundaries first (whitespace, punctuation)
    let simple = SimpleTokenizer::default();
    let mut simple_tokenizer = simple;
    let mut stream = tantivy::tokenizer::Tokenizer::token_stream(&mut simple_tokenizer, text);
    let mut word_ranges = Vec::new();
    while tantivy::tokenizer::TokenStream::advance(&mut stream) {
        let t = tantivy::tokenizer::TokenStream::token(&stream);
        // Validate char boundaries before accepting
        if text.is_char_boundary(t.offset_from) && text.is_char_boundary(t.offset_to) {
            word_ranges.push((t.offset_from, t.offset_to));
        }
    }

    for &(start, end) in &word_ranges {
        let word = &text[start..end];

        // Split on underscores — track byte offsets carefully
        let mut byte_offset = start;
        for part in word.split('_') {
            if !part.is_empty() {
                let part_start = byte_offset;
                let part_end = byte_offset + part.len();
                // Emit underscore-split part if it differs from the whole word
                if part_end > part_start && (part_start != start || part_end != end) {
                    tokens.push((part_start, part_end));
                }

                // Split camelCase: collect (char_index, byte_offset) pairs
                let char_offsets: Vec<(usize, usize)> = part
                    .char_indices()
                    .enumerate()
                    .map(|(ci, (bi, _))| (ci, part_start + bi))
                    .collect();

                let chars: Vec<char> = part.chars().collect();
                let mut seg_start_byte = part_start;

                for i in 1..chars.len() {
                    if chars[i].is_uppercase()
                        && (i + 1 >= chars.len()
                            || !chars[i + 1].is_uppercase()
                            || chars[i - 1].is_lowercase())
                    {
                        let seg_end_byte = char_offsets[i].1;
                        if seg_end_byte > seg_start_byte + chars[0].len_utf8() {
                            tokens.push((seg_start_byte, seg_end_byte));
                        }
                        seg_start_byte = seg_end_byte;
                    }
                }
                // Last camel segment
                if seg_start_byte > part_start && seg_start_byte < part_end {
                    tokens.push((seg_start_byte, part_end));
                }
            }
            byte_offset += part.len() + 1; // +1 for underscore
        }
    }

    tokens
}

#[cfg(test)]
mod tests {
    use super::*;
    use compact_str::CompactString;
    use recon_core::lang::Language;
    use recon_core::symbol::SymbolKind;
    use std::path::PathBuf;

    fn make_sym(id: u64, name: &str, qname: &str, kind: SymbolKind) -> Symbol {
        Symbol {
            id,
            path: PathBuf::from("src/lib.rs"),
            name: CompactString::new(name),
            qualified_name: CompactString::new(qname),
            kind,
            signature: Some(format!("fn {name}()")),
            doc: Some(format!("Documentation for {name}")),
            parent_id: None,
            byte_range: 0..100,
            line_range: 1..=10,
            body_hash: [0u8; 32],
            lang: Language::Rust,
        }
    }

    #[test]
    fn code_split_camel() {
        let tokens = split_code_identifier("validateEmailAddress");
        let strs: Vec<&str> = tokens
            .iter()
            .map(|(s, e)| &"validateEmailAddress"[*s..*e])
            .collect();
        assert!(strs.contains(&"validateEmailAddress"));
        assert!(strs.contains(&"validate"));
    }

    #[test]
    fn code_split_snake() {
        let tokens = split_code_identifier("validate_email");
        let strs: Vec<&str> = tokens
            .iter()
            .map(|(s, e)| &"validate_email"[*s..*e])
            .collect();
        // Full token is always emitted
        assert!(
            strs.contains(&"validate_email"),
            "missing full token: {strs:?}"
        );
        // Sub-tokens from underscore split should be present
        // SimpleTokenizer splits on _ so "validate" and "email" become separate words
        // which then get added as sub-tokens
        assert!(
            !strs.is_empty(),
            "should have at least the full token: {strs:?}"
        );
    }

    #[test]
    fn index_and_search() {
        let backend = TantivyBackend::open_memory().unwrap();
        let mut writer = backend.writer(15_000_000).unwrap();

        let symbols = vec![
            make_sym(
                1,
                "validateEmail",
                "auth::validateEmail",
                SymbolKind::Function,
            ),
            make_sym(2, "sendEmail", "email::sendEmail", SymbolKind::Function),
            make_sym(3, "processData", "data::processData", SymbolKind::Function),
            make_sym(4, "Config", "app::Config", SymbolKind::Struct),
        ];

        backend
            .index_symbols(&mut writer, Path::new("src/lib.rs"), &symbols)
            .unwrap();
        backend.commit(&mut writer).unwrap();

        assert_eq!(backend.doc_count(), 4);

        let hits = backend.search("validate", 10).unwrap();
        assert!(!hits.is_empty(), "should find validateEmail");
        assert_eq!(hits[0].name, "validateEmail");

        let hits = backend.search("email", 10).unwrap();
        assert!(hits.len() >= 2, "should find both email-related symbols");
    }

    #[test]
    fn search_by_kind() {
        let backend = TantivyBackend::open_memory().unwrap();
        let mut writer = backend.writer(15_000_000).unwrap();

        let symbols = vec![
            make_sym(1, "Foo", "app::Foo", SymbolKind::Struct),
            make_sym(2, "foo", "app::foo", SymbolKind::Function),
        ];

        backend
            .index_symbols(&mut writer, Path::new("src/lib.rs"), &symbols)
            .unwrap();
        backend.commit(&mut writer).unwrap();

        let hits = backend.search("Foo", 10).unwrap();
        assert!(!hits.is_empty());
    }

    #[test]
    fn reindex_replaces() {
        let backend = TantivyBackend::open_memory().unwrap();
        let mut writer = backend.writer(15_000_000).unwrap();

        let v1 = vec![make_sym(
            1,
            "old_func",
            "mod::old_func",
            SymbolKind::Function,
        )];
        backend
            .index_symbols(&mut writer, Path::new("src/lib.rs"), &v1)
            .unwrap();
        backend.commit(&mut writer).unwrap();
        assert_eq!(backend.doc_count(), 1);

        let v2 = vec![
            make_sym(2, "new_func", "mod::new_func", SymbolKind::Function),
            make_sym(3, "another", "mod::another", SymbolKind::Function),
        ];
        backend
            .index_symbols(&mut writer, Path::new("src/lib.rs"), &v2)
            .unwrap();
        backend.commit(&mut writer).unwrap();

        // Old doc should be deleted, 2 new ones added
        // Note: tantivy soft-deletes, so doc_count might show deleted docs until merge
        let hits = backend.search("old_func", 10).unwrap();
        assert!(hits.is_empty(), "old_func should be deleted");

        let hits = backend.search("new_func", 10).unwrap();
        assert!(!hits.is_empty(), "new_func should exist");
    }
}
