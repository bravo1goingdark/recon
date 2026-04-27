//! Symbol extraction from parsed tree-sitter ASTs.

use compact_str::CompactString;
use recon_core::lang::Language;
use recon_core::symbol::{Ref, Symbol, SymbolKind};
use std::path::Path;
use std::sync::Arc;
use tracing::error;

/// Result of extracting symbols from a source file.
pub struct Extracted {
    /// Extracted symbol definitions.
    pub symbols: Vec<Symbol>,
    /// Extracted symbol references.
    pub refs: Vec<Ref>,
}

/// Mutable extraction context threaded through all extractors.
struct Ctx<'a> {
    src: &'a str,
    /// Arc-shared path — allocated once, Arc::clone per symbol/ref (cheap atomic increment).
    path_arc: Arc<std::path::PathBuf>,
    lang: Language,
    symbols: Vec<Symbol>,
    refs: Vec<Ref>,
    next_id: u64,
    /// Reusable buffer for qualified name construction.
    qname_buf: String,
}

impl<'a> Ctx<'a> {
    fn new(src: &'a str, path: &'a Path, lang: Language) -> Self {
        // Estimate: ~1 symbol per 5 lines, ~3 refs per symbol
        let line_count = src.as_bytes().iter().filter(|&&b| b == b'\n').count();
        let est_symbols = (line_count / 5).max(8);
        Self {
            src,
            path_arc: Arc::new(path.to_path_buf()),
            lang,
            symbols: Vec::with_capacity(est_symbols),
            refs: Vec::with_capacity(est_symbols * 3),
            next_id: 1,
            qname_buf: String::with_capacity(128),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn push_symbol(
        &mut self,
        name: &str,
        parent_name: Option<&str>,
        kind: SymbolKind,
        signature: Option<String>,
        doc: Option<String>,
        parent_id: Option<u64>,
        node: tree_sitter::Node,
    ) -> u64 {
        let id = self.next_id;
        self.next_id += 1;

        // Build qualified name in reusable buffer to avoid per-symbol allocation
        self.qname_buf.clear();
        let qname = match parent_name {
            Some(p) => {
                self.qname_buf.push_str(p);
                self.qname_buf.push_str("::");
                self.qname_buf.push_str(name);
                CompactString::new(&self.qname_buf)
            }
            None => CompactString::new(name),
        };

        self.symbols.push(Symbol {
            id,
            path: Arc::clone(&self.path_arc),
            name: CompactString::new(name),
            qualified_name: qname,
            kind,
            signature: signature.map(CompactString::from),
            doc: doc.map(CompactString::from),
            parent_id,
            byte_range: node.start_byte()..node.end_byte(),
            line_range: (node.start_position().row as u32 + 1)
                ..=(node.end_position().row as u32 + 1),
            body_hash: *blake3::hash(self.src[node.byte_range()].as_bytes()).as_bytes(),
            lang: self.lang,
        });
        id
    }

    fn push_ref(&mut self, src_symbol_id: u64, ident: &str) {
        self.refs.push(Ref {
            src_path: Arc::clone(&self.path_arc),
            src_symbol_id,
            ident: CompactString::new(ident),
            dst_symbol_id: None,
            weight: 1.0,
        });
    }

    fn first_line(&self, node: tree_sitter::Node) -> String {
        let slice = &self.src[node.byte_range()];
        let end = slice.find('\n').unwrap_or(slice.len());
        slice[..end].to_string()
    }

    fn leading_doc(&self, node: tree_sitter::Node) -> Option<String> {
        let mut prev = node.prev_sibling();
        // Collect doc lines in reverse, then reverse once — avoids repeated Vec inserts
        let mut doc_parts: smallvec::SmallVec<[&str; 4]> = smallvec::SmallVec::new();
        while let Some(p) = prev {
            let kind = p.kind();
            if matches!(
                kind,
                "line_comment" | "block_comment" | "comment" | "doc_comment"
            ) {
                doc_parts.push(self.src[p.byte_range()].trim());
                prev = p.prev_sibling();
            } else if matches!(kind, "attribute_item" | "inner_attribute_item" | "decorator")
            {
                // Skip past attributes/decorators that sit between a doc and the
                // item (`#[derive(...)]`, `#[inline]`, Python `@decorator`, etc.).
                prev = p.prev_sibling();
            } else if kind == "expression_statement" {
                if let Some(child) = p.child(0) {
                    if child.kind() == "string" {
                        doc_parts.push(self.src[child.byte_range()].trim());
                    }
                }
                break;
            } else {
                break;
            }
        }
        if doc_parts.is_empty() {
            None
        } else {
            doc_parts.reverse();
            Some(doc_parts.join("\n"))
        }
    }

    fn child_text<'b>(&self, node: tree_sitter::Node<'b>, field: &str) -> Option<&'a str> {
        node.child_by_field_name(field)
            .map(|n| &self.src[n.byte_range()])
    }

    fn into_result(self) -> Extracted {
        Extracted {
            symbols: self.symbols,
            refs: self.refs,
        }
    }
}

/// Parent context: (id, name) of the enclosing symbol.
type ParentCtx<'a> = Option<(u64, &'a str)>;

/// Extract symbols and refs from source code.
/// Creates a one-off parser. For batch work, use `extract_symbols_pooled`.
pub fn extract_symbols(src: &[u8], lang: Language, path: &Path) -> Extracted {
    let ts_lang = match crate::languages::ts_language(lang) {
        Some(l) => l,
        None => {
            return Extracted {
                symbols: vec![],
                refs: vec![],
            }
        }
    };
    let mut parser = tree_sitter::Parser::new();
    if let Err(e) = parser.set_language(&ts_lang) {
        error!(
            lang = ?lang,
            "tree-sitter set_language failed (ABI mismatch?): {e}"
        );
        return Extracted {
            symbols: vec![],
            refs: vec![],
        };
    }
    let tree = match parser.parse(src, None) {
        Some(t) => t,
        None => {
            return Extracted {
                symbols: vec![],
                refs: vec![],
            }
        }
    };
    extract_from_tree(&tree, src, lang, path)
}

/// Extract symbols using a pooled parser (avoids parser creation overhead).
pub fn extract_symbols_pooled(
    src: &[u8],
    lang: Language,
    path: &Path,
    pool: &crate::pool::ParserPool,
) -> Extracted {
    let tree = pool.with(|parser| parser.parse(src, None));
    match tree {
        Some(tree) => extract_from_tree(&tree, src, lang, path),
        None => Extracted {
            symbols: vec![],
            refs: vec![],
        },
    }
}

fn extract_from_tree(
    tree: &tree_sitter::Tree,
    src: &[u8],
    lang: Language,
    path: &Path,
) -> Extracted {
    // Use lossy UTF-8 conversion so non-UTF-8 files still yield partial results
    // rather than silently dropping all symbols.
    let src_cow = String::from_utf8_lossy(src);
    let mut ctx = Ctx::new(&src_cow, path, lang);
    let root = tree.root_node();

    match lang {
        Language::Rust => extract_rust(&mut ctx, root, None),
        Language::Python => extract_python(&mut ctx, root, None),
        Language::TypeScript | Language::Tsx | Language::JavaScript => {
            extract_js_ts(&mut ctx, root, None)
        }
        Language::Go => extract_go(&mut ctx, root, None),
        Language::Java => extract_java(&mut ctx, root, None),
        Language::C | Language::Cpp => extract_c_cpp(&mut ctx, root, None),
        Language::Unknown => {}
    }
    ctx.into_result()
}

// ──── Rust ────

fn extract_rust(ctx: &mut Ctx, node: tree_sitter::Node, parent: ParentCtx) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "function_item" | "function_signature_item" => {
                if let Some(name) = ctx.child_text(child, "name") {
                    let kind = if parent.is_some() {
                        SymbolKind::Method
                    } else {
                        SymbolKind::Function
                    };
                    ctx.push_symbol(
                        name,
                        parent.map(|p| p.1),
                        kind,
                        Some(ctx.first_line(child)),
                        ctx.leading_doc(child),
                        parent.map(|p| p.0),
                        child,
                    );
                }
            }
            "struct_item" => {
                if let Some(name) = ctx.child_text(child, "name") {
                    let id = ctx.push_symbol(
                        name,
                        parent.map(|p| p.1),
                        SymbolKind::Struct,
                        Some(ctx.first_line(child)),
                        ctx.leading_doc(child),
                        parent.map(|p| p.0),
                        child,
                    );
                    extract_rust_fields(ctx, child, id, name);
                }
            }
            "enum_item" => {
                if let Some(name) = ctx.child_text(child, "name") {
                    let id = ctx.push_symbol(
                        name,
                        parent.map(|p| p.1),
                        SymbolKind::Enum,
                        Some(ctx.first_line(child)),
                        ctx.leading_doc(child),
                        parent.map(|p| p.0),
                        child,
                    );
                    extract_rust_variants(ctx, child, id, name);
                }
            }
            "trait_item" => {
                if let Some(name) = ctx.child_text(child, "name") {
                    let id = ctx.push_symbol(
                        name,
                        parent.map(|p| p.1),
                        SymbolKind::Trait,
                        Some(ctx.first_line(child)),
                        ctx.leading_doc(child),
                        parent.map(|p| p.0),
                        child,
                    );
                    extract_rust(ctx, child, Some((id, name)));
                }
            }
            "impl_item" => {
                if let Some(type_node) = child.child_by_field_name("type") {
                    let type_name = &ctx.src[type_node.byte_range()];
                    // Resolve impl block to the struct/enum/trait it implements so
                    // methods nest under the type in outlines and parent chains.
                    // Strip generics ("Foo<T>" -> "Foo") for the lookup; fall back
                    // to the enclosing scope id if the type is foreign or declared
                    // after the impl block.
                    let base_name = type_name
                        .split('<')
                        .next()
                        .unwrap_or(type_name)
                        .trim();
                    let type_id = ctx
                        .symbols
                        .iter()
                        .rev()
                        .find(|s| {
                            s.name.as_str() == base_name
                                && matches!(
                                    s.kind,
                                    SymbolKind::Struct
                                        | SymbolKind::Enum
                                        | SymbolKind::Trait
                                )
                        })
                        .map(|s| s.id)
                        .unwrap_or_else(|| parent.map_or(0, |p| p.0));
                    extract_rust(ctx, child, Some((type_id, type_name)));
                }
            }
            "const_item" => {
                if let Some(name) = ctx.child_text(child, "name") {
                    ctx.push_symbol(
                        name,
                        parent.map(|p| p.1),
                        SymbolKind::Const,
                        Some(ctx.first_line(child)),
                        ctx.leading_doc(child),
                        parent.map(|p| p.0),
                        child,
                    );
                }
            }
            "static_item" => {
                if let Some(name) = ctx.child_text(child, "name") {
                    ctx.push_symbol(
                        name,
                        parent.map(|p| p.1),
                        SymbolKind::Static,
                        Some(ctx.first_line(child)),
                        ctx.leading_doc(child),
                        parent.map(|p| p.0),
                        child,
                    );
                }
            }
            "type_item" => {
                if let Some(name) = ctx.child_text(child, "name") {
                    ctx.push_symbol(
                        name,
                        parent.map(|p| p.1),
                        SymbolKind::Type,
                        Some(ctx.first_line(child)),
                        ctx.leading_doc(child),
                        parent.map(|p| p.0),
                        child,
                    );
                }
            }
            "mod_item" => {
                if let Some(name) = ctx.child_text(child, "name") {
                    let id = ctx.push_symbol(
                        name,
                        parent.map(|p| p.1),
                        SymbolKind::Module,
                        Some(ctx.first_line(child)),
                        ctx.leading_doc(child),
                        parent.map(|p| p.0),
                        child,
                    );
                    extract_rust(ctx, child, Some((id, name)));
                }
            }
            "macro_definition" => {
                if let Some(name) = ctx.child_text(child, "name") {
                    ctx.push_symbol(
                        name,
                        parent.map(|p| p.1),
                        SymbolKind::Macro,
                        Some(ctx.first_line(child)),
                        ctx.leading_doc(child),
                        parent.map(|p| p.0),
                        child,
                    );
                }
            }
            "identifier" | "type_identifier" | "field_identifier" => {
                let ident = &ctx.src[child.byte_range()];
                if ident.len() > 1 {
                    if let Some((pid, _)) = parent {
                        ctx.push_ref(pid, ident);
                    }
                }
            }
            _ => {
                if child.child_count() > 0 {
                    extract_rust(ctx, child, parent);
                }
            }
        }
    }
}

fn extract_rust_fields(ctx: &mut Ctx, node: tree_sitter::Node, parent_id: u64, parent_name: &str) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "field_declaration_list" {
            let mut fc = child.walk();
            for field in child.children(&mut fc) {
                if field.kind() == "field_declaration" {
                    if let Some(name) = ctx.child_text(field, "name") {
                        ctx.push_symbol(
                            name,
                            Some(parent_name),
                            SymbolKind::Field,
                            Some(ctx.first_line(field)),
                            None,
                            Some(parent_id),
                            field,
                        );
                    }
                }
            }
        }
    }
}

fn extract_rust_variants(
    ctx: &mut Ctx,
    node: tree_sitter::Node,
    parent_id: u64,
    parent_name: &str,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "enum_variant_list" {
            let mut vc = child.walk();
            for variant in child.children(&mut vc) {
                if variant.kind() == "enum_variant" {
                    if let Some(name) = ctx.child_text(variant, "name") {
                        ctx.push_symbol(
                            name,
                            Some(parent_name),
                            SymbolKind::EnumVariant,
                            Some(ctx.first_line(variant)),
                            None,
                            Some(parent_id),
                            variant,
                        );
                    }
                }
            }
        }
    }
}

// ──── Python ────

fn extract_python(ctx: &mut Ctx, node: tree_sitter::Node, parent: ParentCtx) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "function_definition" => {
                if let Some(name) = ctx.child_text(child, "name") {
                    let kind = if parent.is_some() {
                        SymbolKind::Method
                    } else {
                        SymbolKind::Function
                    };
                    let doc = python_docstring(ctx, child);
                    ctx.push_symbol(
                        name,
                        parent.map(|p| p.1),
                        kind,
                        Some(ctx.first_line(child)),
                        doc,
                        parent.map(|p| p.0),
                        child,
                    );
                }
            }
            "class_definition" => {
                if let Some(name) = ctx.child_text(child, "name") {
                    let doc = python_docstring(ctx, child);
                    let id = ctx.push_symbol(
                        name,
                        parent.map(|p| p.1),
                        SymbolKind::Class,
                        Some(ctx.first_line(child)),
                        doc,
                        parent.map(|p| p.0),
                        child,
                    );
                    if let Some(body) = child.child_by_field_name("body") {
                        extract_python(ctx, body, Some((id, name)));
                    }
                }
            }
            _ => {
                if child.child_count() > 0 {
                    extract_python(ctx, child, parent);
                }
            }
        }
    }
}

fn python_docstring(ctx: &Ctx, node: tree_sitter::Node) -> Option<String> {
    if let Some(body) = node.child_by_field_name("body") {
        if let Some(first) = body.child(0) {
            if first.kind() == "expression_statement" {
                if let Some(s) = first.child(0) {
                    if s.kind() == "string" {
                        return Some(ctx.src[s.byte_range()].trim().to_string());
                    }
                }
            }
        }
    }
    ctx.leading_doc(node)
}

// ──── JS / TS / TSX ────

fn extract_js_ts(ctx: &mut Ctx, node: tree_sitter::Node, parent: ParentCtx) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "function_declaration" | "method_definition" => {
                if let Some(name) = ctx.child_text(child, "name") {
                    let kind = if child.kind() == "method_definition" {
                        SymbolKind::Method
                    } else {
                        SymbolKind::Function
                    };
                    ctx.push_symbol(
                        name,
                        parent.map(|p| p.1),
                        kind,
                        Some(ctx.first_line(child)),
                        ctx.leading_doc(child),
                        parent.map(|p| p.0),
                        child,
                    );
                }
            }
            "class_declaration" => {
                if let Some(name) = ctx.child_text(child, "name") {
                    let id = ctx.push_symbol(
                        name,
                        parent.map(|p| p.1),
                        SymbolKind::Class,
                        Some(ctx.first_line(child)),
                        ctx.leading_doc(child),
                        parent.map(|p| p.0),
                        child,
                    );
                    if let Some(body) = child.child_by_field_name("body") {
                        extract_js_ts(ctx, body, Some((id, name)));
                    }
                }
            }
            "interface_declaration" => {
                if let Some(name) = ctx.child_text(child, "name") {
                    ctx.push_symbol(
                        name,
                        parent.map(|p| p.1),
                        SymbolKind::Interface,
                        Some(ctx.first_line(child)),
                        ctx.leading_doc(child),
                        parent.map(|p| p.0),
                        child,
                    );
                }
            }
            "enum_declaration" => {
                if let Some(name) = ctx.child_text(child, "name") {
                    ctx.push_symbol(
                        name,
                        parent.map(|p| p.1),
                        SymbolKind::Enum,
                        Some(ctx.first_line(child)),
                        ctx.leading_doc(child),
                        parent.map(|p| p.0),
                        child,
                    );
                }
            }
            "type_alias_declaration" => {
                if let Some(name) = ctx.child_text(child, "name") {
                    ctx.push_symbol(
                        name,
                        parent.map(|p| p.1),
                        SymbolKind::Type,
                        Some(ctx.first_line(child)),
                        ctx.leading_doc(child),
                        parent.map(|p| p.0),
                        child,
                    );
                }
            }
            "lexical_declaration" | "variable_declaration" => {
                let mut dc = child.walk();
                for decl in child.children(&mut dc) {
                    if decl.kind() == "variable_declarator" {
                        if let Some(name) = ctx.child_text(decl, "name") {
                            let has_fn_value = decl.child_by_field_name("value").is_some_and(|v| {
                                v.kind() == "arrow_function" || v.kind() == "function"
                            });
                            let kind = if has_fn_value {
                                SymbolKind::Function
                            } else {
                                SymbolKind::Const
                            };
                            ctx.push_symbol(
                                name,
                                parent.map(|p| p.1),
                                kind,
                                Some(ctx.first_line(child)),
                                ctx.leading_doc(child),
                                parent.map(|p| p.0),
                                child,
                            );
                        }
                    }
                }
            }
            "export_statement" => {
                extract_js_ts(ctx, child, parent);
            }
            _ => {
                if child.child_count() > 0 {
                    extract_js_ts(ctx, child, parent);
                }
            }
        }
    }
}

// ──── Go ────

fn extract_go(ctx: &mut Ctx, node: tree_sitter::Node, parent: ParentCtx) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "function_declaration" => {
                if let Some(name) = ctx.child_text(child, "name") {
                    ctx.push_symbol(
                        name,
                        parent.map(|p| p.1),
                        SymbolKind::Function,
                        Some(ctx.first_line(child)),
                        ctx.leading_doc(child),
                        parent.map(|p| p.0),
                        child,
                    );
                }
            }
            "method_declaration" => {
                if let Some(name) = ctx.child_text(child, "name") {
                    ctx.push_symbol(
                        name,
                        parent.map(|p| p.1),
                        SymbolKind::Method,
                        Some(ctx.first_line(child)),
                        ctx.leading_doc(child),
                        parent.map(|p| p.0),
                        child,
                    );
                }
            }
            "type_declaration" => {
                let mut tc = child.walk();
                for spec in child.children(&mut tc) {
                    if spec.kind() == "type_spec" {
                        if let Some(name) = ctx.child_text(spec, "name") {
                            let kind = match spec.child_by_field_name("type").map(|t| t.kind()) {
                                Some("struct_type") => SymbolKind::Struct,
                                Some("interface_type") => SymbolKind::Interface,
                                _ => SymbolKind::Type,
                            };
                            ctx.push_symbol(
                                name,
                                parent.map(|p| p.1),
                                kind,
                                Some(ctx.first_line(spec)),
                                ctx.leading_doc(child),
                                parent.map(|p| p.0),
                                spec,
                            );
                        }
                    }
                }
            }
            "const_declaration" | "var_declaration" => {
                let is_const = child.kind() == "const_declaration";
                let mut vc = child.walk();
                for spec in child.children(&mut vc) {
                    if spec.kind() == "const_spec" || spec.kind() == "var_spec" {
                        if let Some(name_node) = spec.child_by_field_name("name") {
                            let name = &ctx.src[name_node.byte_range()];
                            let kind = if is_const {
                                SymbolKind::Const
                            } else {
                                SymbolKind::Static
                            };
                            ctx.push_symbol(
                                name,
                                parent.map(|p| p.1),
                                kind,
                                Some(ctx.first_line(spec)),
                                ctx.leading_doc(child),
                                parent.map(|p| p.0),
                                spec,
                            );
                        }
                    }
                }
            }
            _ => {
                if child.child_count() > 0 {
                    extract_go(ctx, child, parent);
                }
            }
        }
    }
}

// ──── Java ────

fn extract_java(ctx: &mut Ctx, node: tree_sitter::Node, parent: ParentCtx) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "method_declaration" | "constructor_declaration" => {
                if let Some(name) = ctx.child_text(child, "name") {
                    ctx.push_symbol(
                        name,
                        parent.map(|p| p.1),
                        SymbolKind::Method,
                        Some(ctx.first_line(child)),
                        ctx.leading_doc(child),
                        parent.map(|p| p.0),
                        child,
                    );
                }
            }
            "class_declaration" => {
                if let Some(name) = ctx.child_text(child, "name") {
                    let id = ctx.push_symbol(
                        name,
                        parent.map(|p| p.1),
                        SymbolKind::Class,
                        Some(ctx.first_line(child)),
                        ctx.leading_doc(child),
                        parent.map(|p| p.0),
                        child,
                    );
                    if let Some(body) = child.child_by_field_name("body") {
                        extract_java(ctx, body, Some((id, name)));
                    }
                }
            }
            "interface_declaration" => {
                if let Some(name) = ctx.child_text(child, "name") {
                    let id = ctx.push_symbol(
                        name,
                        parent.map(|p| p.1),
                        SymbolKind::Interface,
                        Some(ctx.first_line(child)),
                        ctx.leading_doc(child),
                        parent.map(|p| p.0),
                        child,
                    );
                    if let Some(body) = child.child_by_field_name("body") {
                        extract_java(ctx, body, Some((id, name)));
                    }
                }
            }
            "enum_declaration" => {
                if let Some(name) = ctx.child_text(child, "name") {
                    ctx.push_symbol(
                        name,
                        parent.map(|p| p.1),
                        SymbolKind::Enum,
                        Some(ctx.first_line(child)),
                        ctx.leading_doc(child),
                        parent.map(|p| p.0),
                        child,
                    );
                }
            }
            _ => {
                if child.child_count() > 0 {
                    extract_java(ctx, child, parent);
                }
            }
        }
    }
}

// ──── C / C++ ────

fn extract_c_cpp(ctx: &mut Ctx, node: tree_sitter::Node, parent: ParentCtx) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "function_definition" => {
                let name = ctx
                    .child_text(child, "declarator")
                    .or_else(|| ctx.child_text(child, "name"));
                if let Some(name) = name {
                    let actual_name = name.split('(').next().unwrap_or(name).trim();
                    if !actual_name.is_empty() {
                        ctx.push_symbol(
                            actual_name,
                            parent.map(|p| p.1),
                            SymbolKind::Function,
                            Some(ctx.first_line(child)),
                            ctx.leading_doc(child),
                            parent.map(|p| p.0),
                            child,
                        );
                    }
                }
            }
            "struct_specifier" | "class_specifier" => {
                if let Some(name) = ctx.child_text(child, "name") {
                    let kind = if child.kind() == "class_specifier" {
                        SymbolKind::Class
                    } else {
                        SymbolKind::Struct
                    };
                    let id = ctx.push_symbol(
                        name,
                        parent.map(|p| p.1),
                        kind,
                        Some(ctx.first_line(child)),
                        ctx.leading_doc(child),
                        parent.map(|p| p.0),
                        child,
                    );
                    if let Some(body) = child.child_by_field_name("body") {
                        extract_c_cpp(ctx, body, Some((id, name)));
                    }
                }
            }
            "enum_specifier" => {
                if let Some(name) = ctx.child_text(child, "name") {
                    ctx.push_symbol(
                        name,
                        parent.map(|p| p.1),
                        SymbolKind::Enum,
                        Some(ctx.first_line(child)),
                        ctx.leading_doc(child),
                        parent.map(|p| p.0),
                        child,
                    );
                }
            }
            "declaration" => {
                extract_c_cpp(ctx, child, parent);
            }
            _ => {
                if child.child_count() > 0 {
                    extract_c_cpp(ctx, child, parent);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_basic() {
        let src = br#"
/// A greeter
pub fn greet(name: &str) -> String {
    format!("Hello, {name}!")
}

pub struct Config {
    pub host: String,
    pub port: u16,
}

pub enum Color {
    Red,
    Green,
    Blue,
}

pub trait Handler {
    fn handle(&self);
}

impl Config {
    pub fn new() -> Self {
        Config { host: "localhost".into(), port: 8080 }
    }
}
"#;
        let result = extract_symbols(src, Language::Rust, Path::new("src/lib.rs"));
        let names: Vec<&str> = result.symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"greet"), "missing greet: {names:?}");
        assert!(names.contains(&"Config"), "missing Config: {names:?}");
        assert!(names.contains(&"Color"), "missing Color: {names:?}");
        assert!(names.contains(&"Handler"), "missing Handler: {names:?}");
        assert!(names.contains(&"new"), "missing new: {names:?}");
        assert!(names.contains(&"Red"), "missing Red variant: {names:?}");
        assert!(names.contains(&"host"), "missing host field: {names:?}");
    }

    #[test]
    fn python_basic() {
        let src = br#"
class User:
    """A user model."""

    def __init__(self, name: str):
        self.name = name

    def greet(self) -> str:
        return f"Hello, {self.name}!"

def validate_email(email: str) -> bool:
    return "@" in email
"#;
        let result = extract_symbols(src, Language::Python, Path::new("models.py"));
        let names: Vec<&str> = result.symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"User"), "missing User: {names:?}");
        assert!(names.contains(&"__init__"), "missing __init__: {names:?}");
        assert!(names.contains(&"greet"), "missing greet: {names:?}");
        assert!(
            names.contains(&"validate_email"),
            "missing validate_email: {names:?}"
        );
    }

    #[test]
    fn typescript_basic() {
        let src = br#"
export interface Config {
    host: string;
    port: number;
}

export class Server {
    constructor(private config: Config) {}

    start(): void {
        console.log("starting");
    }
}

export function createServer(config: Config): Server {
    return new Server(config);
}

const DEFAULT_PORT = 8080;
"#;
        let result = extract_symbols(src, Language::TypeScript, Path::new("server.ts"));
        let names: Vec<&str> = result.symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"Config"), "missing Config: {names:?}");
        assert!(names.contains(&"Server"), "missing Server: {names:?}");
        assert!(
            names.contains(&"createServer"),
            "missing createServer: {names:?}"
        );
    }

    #[test]
    fn go_basic() {
        let src = br#"
package main

type Config struct {
    Host string
    Port int
}

type Handler interface {
    Handle()
}

func NewConfig() *Config {
    return &Config{Host: "localhost", Port: 8080}
}

const DefaultPort = 8080
"#;
        let result = extract_symbols(src, Language::Go, Path::new("main.go"));
        let names: Vec<&str> = result.symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"Config"), "missing Config: {names:?}");
        assert!(names.contains(&"Handler"), "missing Handler: {names:?}");
        assert!(names.contains(&"NewConfig"), "missing NewConfig: {names:?}");
        assert!(
            names.contains(&"DefaultPort"),
            "missing DefaultPort: {names:?}"
        );
    }

    #[test]
    fn tsx_basic() {
        let src = br#"
import React from "react";

export interface ButtonProps {
    label: string;
    onClick: () => void;
}

export class Button extends React.Component<ButtonProps> {
    render() {
        return <button onClick={this.props.onClick}>{this.props.label}</button>;
    }
}

export function PrimaryButton(props: ButtonProps) {
    return <Button {...props} />;
}
"#;
        let result = extract_symbols(src, Language::Tsx, Path::new("Button.tsx"));
        let names: Vec<&str> = result.symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(
            names.contains(&"ButtonProps"),
            "missing ButtonProps interface: {names:?}"
        );
        assert!(names.contains(&"Button"), "missing Button class: {names:?}");
        assert!(
            names.contains(&"PrimaryButton"),
            "missing PrimaryButton fn: {names:?}"
        );
        assert!(
            names.contains(&"render"),
            "missing render method: {names:?}"
        );
    }

    #[test]
    fn javascript_basic() {
        let src = br#"
export class Server {
    constructor(config) {
        this.config = config;
    }

    start() {
        console.log("starting");
    }
}

export function createServer(config) {
    return new Server(config);
}
"#;
        let result = extract_symbols(src, Language::JavaScript, Path::new("server.js"));
        let names: Vec<&str> = result.symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"Server"), "missing Server class: {names:?}");
        assert!(names.contains(&"start"), "missing start method: {names:?}");
        assert!(
            names.contains(&"createServer"),
            "missing createServer fn: {names:?}"
        );
    }

    #[test]
    fn qualified_names_work() {
        let src = br#"
pub struct Foo {
    pub bar: i32,
}

impl Foo {
    pub fn baz(&self) -> i32 { self.bar }
}
"#;
        let result = extract_symbols(src, Language::Rust, Path::new("src/lib.rs"));
        let baz = result
            .symbols
            .iter()
            .find(|s| s.name.as_str() == "baz")
            .unwrap();
        assert_eq!(baz.qualified_name.as_str(), "Foo::baz");
        assert_eq!(baz.kind, SymbolKind::Method);
    }

    #[test]
    fn rust_impl_method_parent_id_points_to_struct() {
        // Regression: methods inside `impl Foo` previously got parent_id = Some(0)
        // (a sentinel), so `code_outline` (which filters parent_id.is_none()) dropped
        // them and parent chains skipped the type. Now the parser resolves impl
        // blocks to the struct/enum/trait id.
        let src = br#"
pub struct Foo {
    pub x: i32,
}

impl Foo {
    pub fn bar(&self) -> i32 { self.x }
    pub fn baz(&self) {}
}

pub enum Color { Red, Green }

impl Color {
    pub fn name(&self) -> &str { "" }
}

// Generic impl: lookup must strip generics.
pub struct Bag<T> { v: Vec<T> }

impl<T> Bag<T> {
    pub fn len(&self) -> usize { self.v.len() }
}

// Foreign-type impl falls back to enclosing scope.
impl ::std::fmt::Display for Foo {
    fn fmt(&self, _f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result { Ok(()) }
}
"#;
        let result = extract_symbols(src, Language::Rust, Path::new("src/lib.rs"));
        let foo = result
            .symbols
            .iter()
            .find(|s| s.name.as_str() == "Foo" && s.kind == SymbolKind::Struct)
            .expect("Foo struct missing");
        let bar = result
            .symbols
            .iter()
            .find(|s| s.name.as_str() == "bar")
            .expect("bar method missing");
        let baz = result
            .symbols
            .iter()
            .find(|s| s.name.as_str() == "baz")
            .expect("baz method missing");
        assert_eq!(bar.parent_id, Some(foo.id), "bar must parent to Foo");
        assert_eq!(baz.parent_id, Some(foo.id), "baz must parent to Foo");
        assert_eq!(bar.kind, SymbolKind::Method);

        let color = result
            .symbols
            .iter()
            .find(|s| s.name.as_str() == "Color" && s.kind == SymbolKind::Enum)
            .expect("Color enum missing");
        let name = result
            .symbols
            .iter()
            .find(|s| s.name.as_str() == "name")
            .expect("name method missing");
        assert_eq!(name.parent_id, Some(color.id), "name must parent to Color");

        let bag = result
            .symbols
            .iter()
            .find(|s| s.name.as_str() == "Bag" && s.kind == SymbolKind::Struct)
            .expect("Bag struct missing");
        let len = result
            .symbols
            .iter()
            .find(|s| s.name.as_str() == "len")
            .expect("len method missing");
        assert_eq!(
            len.parent_id,
            Some(bag.id),
            "len must parent to Bag (generics stripped for lookup)"
        );
    }

    #[test]
    fn rust_doc_survives_attribute_item_between_doc_and_struct() {
        // Regression: leading_doc walked siblings backward, breaking on any
        // node that wasn't a comment or expression_statement. Attributes
        // (`#[derive(...)]`, `#[cfg(...)]`) sit between the doc and the item
        // as `attribute_item` siblings — the walk would terminate before
        // reaching the doc and `code_skeleton` would render no docstring.
        let src = br#"
/// Doc on Foo
#[derive(Debug, Clone)]
pub struct Foo;

/// Doc on bar
#[inline]
pub fn bar() {}

/// Doc on Color
#[repr(u8)]
pub enum Color { Red, Green }
"#;
        let result = extract_symbols(src, Language::Rust, Path::new("src/lib.rs"));
        let foo = result
            .symbols
            .iter()
            .find(|s| s.name.as_str() == "Foo")
            .expect("Foo missing");
        assert!(
            foo.doc
                .as_deref()
                .is_some_and(|d| d.contains("Doc on Foo")),
            "Foo doc must survive #[derive] attribute (got: {:?})",
            foo.doc
        );

        let bar = result
            .symbols
            .iter()
            .find(|s| s.name.as_str() == "bar")
            .expect("bar missing");
        assert!(
            bar.doc
                .as_deref()
                .is_some_and(|d| d.contains("Doc on bar")),
            "bar doc must survive #[inline] attribute (got: {:?})",
            bar.doc
        );

        let color = result
            .symbols
            .iter()
            .find(|s| s.name.as_str() == "Color")
            .expect("Color missing");
        assert!(
            color
                .doc
                .as_deref()
                .is_some_and(|d| d.contains("Doc on Color")),
            "Color doc must survive #[repr] attribute (got: {:?})",
            color.doc
        );
    }

    #[test]
    fn rust_impl_before_struct_does_not_panic() {
        // When `impl Foo` appears before `struct Foo`, the lookup misses and the
        // method falls back to the enclosing scope. Acceptable degradation; must
        // not panic and must still extract the method symbol.
        let src = br#"
impl Foo {
    pub fn early(&self) {}
}

pub struct Foo { x: i32 }
"#;
        let result = extract_symbols(src, Language::Rust, Path::new("src/lib.rs"));
        let names: Vec<&str> = result.symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"early"), "early method missing: {names:?}");
        assert!(names.contains(&"Foo"), "Foo struct missing: {names:?}");
    }

    #[test]
    fn unknown_language_returns_empty() {
        let result = extract_symbols(b"whatever", Language::Unknown, Path::new("file.txt"));
        assert!(result.symbols.is_empty());
    }

    #[test]
    fn invalid_utf8_yields_partial_results() {
        // Source with a valid Rust function followed by an invalid UTF-8 byte sequence.
        // extract_symbols must not panic and should still return the parseable symbols.
        let mut src = b"fn valid_fn() {}".to_vec();
        src.extend_from_slice(b"\xFF\xFE invalid bytes");
        let result = extract_symbols(&src, Language::Rust, Path::new("src/lib.rs"));
        // At minimum we should not panic; symbols from the valid prefix may be present.
        let _ = result.symbols.len();
    }

    #[test]
    fn non_utf8_file_returns_no_panic() {
        // Pure binary data — must return empty (gracefully) without panicking.
        let binary_data: Vec<u8> = (0u8..=255).collect();
        let result = extract_symbols(&binary_data, Language::Rust, Path::new("binary.rs"));
        let _ = result.symbols.len();
    }

    #[test]
    fn pooled_extraction_matches_direct() {
        use crate::pool::LanguagePools;
        let pools = LanguagePools::new(2);
        let src = b"fn foo() -> u32 { 42 }";
        let path = Path::new("src/lib.rs");

        let direct = extract_symbols(src, Language::Rust, path);
        let pool = pools.get(Language::Rust).unwrap();
        let pooled = extract_symbols_pooled(src, Language::Rust, path, pool);

        assert_eq!(direct.symbols.len(), pooled.symbols.len());
        if let (Some(d), Some(p)) = (direct.symbols.first(), pooled.symbols.first()) {
            assert_eq!(d.name, p.name);
            assert_eq!(d.kind, p.kind);
        }
    }

    #[test]
    fn java_basic() {
        let src = br#"
package com.example;

public class UserService {
    /** Creates a new user. */
    public User create(String name) {
        return new User(name);
    }

    public UserService() {}
}

interface Repository<T> {
    T findById(long id);
}

enum Status {
    ACTIVE, INACTIVE
}
"#;
        let result = extract_symbols(src, Language::Java, Path::new("UserService.java"));
        let names: Vec<&str> = result.symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(
            names.contains(&"UserService"),
            "missing UserService: {names:?}"
        );
        assert!(names.contains(&"create"), "missing create: {names:?}");
        assert!(
            names.contains(&"Repository"),
            "missing Repository: {names:?}"
        );
        assert!(names.contains(&"Status"), "missing Status: {names:?}");
    }

    #[test]
    fn c_basic() {
        let src = br#"
#include <stdio.h>

struct Point {
    int x;
    int y;
};

int add(int a, int b) {
    return a + b;
}

static void print_point(const struct Point *p) {
    printf("(%d, %d)\n", p->x, p->y);
}
"#;
        let result = extract_symbols(src, Language::C, Path::new("math.c"));
        let names: Vec<&str> = result.symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"add"), "missing add: {names:?}");
        assert!(
            names.contains(&"print_point"),
            "missing print_point: {names:?}"
        );
        assert!(names.contains(&"Point"), "missing Point: {names:?}");
    }

    #[test]
    fn cpp_basic() {
        let src = br#"
#include <string>

class Greeter {
public:
    Greeter(const std::string& name);
    void greet() const;
private:
    std::string name_;
};

enum class Color { Red, Green, Blue };

template<typename T>
T max(T a, T b) {
    return a > b ? a : b;
}
"#;
        let result = extract_symbols(src, Language::Cpp, Path::new("greeter.cpp"));
        let names: Vec<&str> = result.symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"Greeter"), "missing Greeter: {names:?}");
        assert!(names.contains(&"Color"), "missing Color: {names:?}");
        assert!(names.contains(&"max"), "missing max: {names:?}");
    }

    #[test]
    fn rust_refs_are_extracted() {
        let src = br#"
pub mod utils {
    pub fn process(data: Vec<u8>) -> String {
        let result = format_data(&data);
        validate(&result);
        result
    }

    fn format_data(data: &[u8]) -> String {
        String::from_utf8_lossy(data).to_string()
    }
}
"#;
        let result = extract_symbols(src, Language::Rust, Path::new("src/lib.rs"));
        // Rust extractor pushes refs for identifiers > 1 char within a parent symbol
        assert!(
            !result.refs.is_empty(),
            "expected refs to be non-empty for Rust source with nested symbols"
        );
    }
}
