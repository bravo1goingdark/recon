//! Symbol, reference, and file metadata types.

use crate::lang::Language;
use compact_str::CompactString;
use serde::{Deserialize, Serialize};
use std::ops::{Range, RangeInclusive};
use std::path::PathBuf;
use std::sync::Arc;

/// Classification of a code symbol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SymbolKind {
    /// A free function.
    Function,
    /// A method on a type.
    Method,
    /// A struct definition.
    Struct,
    /// A class definition.
    Class,
    /// An interface definition.
    Interface,
    /// An enum definition.
    Enum,
    /// A variant of an enum.
    EnumVariant,
    /// A trait definition.
    Trait,
    /// A constant binding.
    Const,
    /// A static binding.
    Static,
    /// A type alias.
    Type,
    /// A module.
    Module,
    /// A macro definition.
    Macro,
    /// A struct or class field.
    Field,
}

impl SymbolKind {
    /// Short label for display.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Function => "fn",
            Self::Method => "method",
            Self::Struct => "struct",
            Self::Class => "class",
            Self::Interface => "interface",
            Self::Enum => "enum",
            Self::EnumVariant => "variant",
            Self::Trait => "trait",
            Self::Const => "const",
            Self::Static => "static",
            Self::Type => "type",
            Self::Module => "mod",
            Self::Macro => "macro",
            Self::Field => "field",
        }
    }
}

impl std::fmt::Display for SymbolKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// A code symbol extracted from source.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Symbol {
    /// Database row ID.
    pub id: u64,
    /// File containing this symbol (Arc-shared to avoid per-symbol clone cost).
    pub path: Arc<PathBuf>,
    /// Simple name (e.g. `new`).
    pub name: CompactString,
    /// Fully qualified name (e.g. `my_crate::Foo::new`).
    pub qualified_name: CompactString,
    /// Kind of symbol.
    pub kind: SymbolKind,
    /// Signature line (e.g. `pub fn new(x: i32) -> Self`).
    pub signature: Option<CompactString>,
    /// Leading doc comment.
    pub doc: Option<CompactString>,
    /// Parent symbol ID (e.g. the struct for a method).
    pub parent_id: Option<u64>,
    /// Byte range in the source file.
    pub byte_range: Range<usize>,
    /// Line range (1-indexed, inclusive).
    pub line_range: RangeInclusive<u32>,
    /// blake3 hash of the symbol body.
    pub body_hash: [u8; 32],
    /// Source language.
    pub lang: Language,
}

/// A reference edge between symbols.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ref {
    /// File containing the reference (Arc-shared to avoid per-ref clone cost).
    pub src_path: Arc<PathBuf>,
    /// Symbol making the reference.
    pub src_symbol_id: u64,
    /// Identifier being referenced.
    pub ident: CompactString,
    /// Resolved target symbol, if known.
    pub dst_symbol_id: Option<u64>,
    /// Edge weight for PageRank.
    pub weight: f32,
}

/// Metadata about an indexed file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileMeta {
    /// File path (relative to repo root).
    pub path: PathBuf,
    /// Detected language.
    pub lang: Language,
    /// File size in bytes.
    pub size_bytes: u64,
    /// blake3 content hash.
    pub content_hash: [u8; 32],
    /// Last modification time (unix timestamp).
    pub mtime: i64,
    /// When this file was last indexed (unix timestamp).
    pub indexed_at: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbol_kind_display() {
        assert_eq!(SymbolKind::Function.to_string(), "fn");
        assert_eq!(SymbolKind::Struct.to_string(), "struct");
        assert_eq!(SymbolKind::EnumVariant.to_string(), "variant");
    }

    #[test]
    fn symbol_serde_roundtrip() {
        let sym = Symbol {
            id: 1,
            path: Arc::new(PathBuf::from("src/main.rs")),
            name: CompactString::new("main"),
            qualified_name: CompactString::new("crate::main"),
            kind: SymbolKind::Function,
            signature: Some(CompactString::new("fn main()")),
            doc: None,
            parent_id: None,
            byte_range: 0..50,
            line_range: 1..=5,
            body_hash: [0u8; 32],
            lang: Language::Rust,
        };
        let json = serde_json::to_string(&sym).unwrap();
        let back: Symbol = serde_json::from_str(&json).unwrap();
        assert_eq!(back.name.as_str(), "main");
        assert_eq!(back.kind, SymbolKind::Function);
        assert_eq!(back.signature.as_deref(), Some("fn main()"));
    }

    #[test]
    fn symbol_optional_fields_none() {
        let sym = Symbol {
            id: 2,
            path: Arc::new(PathBuf::from("src/lib.rs")),
            name: CompactString::new("helper"),
            qualified_name: CompactString::new("crate::helper"),
            kind: SymbolKind::Function,
            signature: None,
            doc: None,
            parent_id: None,
            byte_range: 0..10,
            line_range: 1..=1,
            body_hash: [0u8; 32],
            lang: Language::Rust,
        };
        let json = serde_json::to_string(&sym).unwrap();
        let back: Symbol = serde_json::from_str(&json).unwrap();
        assert!(back.signature.is_none());
        assert!(back.doc.is_none());
    }

    #[test]
    fn symbol_doc_roundtrip() {
        let sym = Symbol {
            id: 3,
            path: Arc::new(PathBuf::from("src/lib.rs")),
            name: CompactString::new("documented"),
            qualified_name: CompactString::new("crate::documented"),
            kind: SymbolKind::Function,
            signature: Some(CompactString::new("fn documented() -> u32")),
            doc: Some(CompactString::new("Returns a number.")),
            parent_id: None,
            byte_range: 0..30,
            line_range: 1..=3,
            body_hash: [0u8; 32],
            lang: Language::Rust,
        };
        let json = serde_json::to_string(&sym).unwrap();
        let back: Symbol = serde_json::from_str(&json).unwrap();
        assert_eq!(back.doc.as_deref(), Some("Returns a number."));
    }
}
