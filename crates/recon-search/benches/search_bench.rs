use compact_str::CompactString;
use criterion::{criterion_group, criterion_main, Criterion};
use recon_core::lang::Language;
use recon_core::symbol::{Ref, Symbol, SymbolKind};
use recon_search::tantivy_backend::TantivyBackend;
use recon_search::text::search_files;
use recon_search::tokens::count_tokens;
use recon_search::{fuzzy, pagerank};
use std::path::PathBuf;

fn make_symbol(i: usize, name: &str) -> Symbol {
    Symbol {
        id: i as u64,
        path: PathBuf::from("src/lib.rs"),
        name: CompactString::new(name),
        qualified_name: CompactString::new(format!("crate::{name}")),
        kind: SymbolKind::Function,
        signature: Some(format!("fn {name}()")),
        doc: None,
        parent_id: None,
        byte_range: 0..100,
        line_range: 1..=5,
        body_hash: [0u8; 32],
        lang: Language::Rust,
    }
}

fn bench_tantivy_search(c: &mut Criterion) {
    let backend = TantivyBackend::open_memory().unwrap();
    let mut writer = backend.writer(15_000_000).unwrap();
    let symbols: Vec<Symbol> = (0..1000)
        .map(|i| make_symbol(i, &format!("func_{i}")))
        .collect();
    backend
        .index_symbols(&mut writer, std::path::Path::new("src/lib.rs"), &symbols)
        .unwrap();
    backend.commit(&mut writer).unwrap();

    c.bench_function("tantivy_search/1k_symbols", |b| {
        b.iter(|| backend.search("func_500", 20).unwrap())
    });
}

fn bench_text_grep(c: &mut Criterion) {
    let dir = tempfile::tempdir().unwrap();
    let mut files = Vec::new();
    for i in 0..50 {
        let path = dir.path().join(format!("file_{i}.rs"));
        let content: String = (0..100)
            .map(|j| format!("fn func_{i}_{j}() {{ todo!() }}\n"))
            .collect();
        std::fs::write(&path, content).unwrap();
        files.push(path);
    }

    c.bench_function("text_grep/50_files", |b| {
        b.iter(|| search_files("func_25_", &files, false, 50).unwrap())
    });
}

fn bench_fuzzy_rank(c: &mut Criterion) {
    let symbols: Vec<Symbol> = (0..5000)
        .map(|i| make_symbol(i, &format!("validate_email_{i}")))
        .collect();

    c.bench_function("fuzzy_rank/5k", |b| {
        b.iter(|| fuzzy::fuzzy_rank(&symbols, "val_eml", 20))
    });
}

fn bench_repo_map_render(c: &mut Criterion) {
    let symbols: Vec<Symbol> = (0..500)
        .map(|i| make_symbol(i, &format!("handler_{i}")))
        .collect();
    let refs: Vec<Ref> = (0..500)
        .map(|i| Ref {
            src_path: PathBuf::from("src/lib.rs"),
            src_symbol_id: i as u64,
            ident: CompactString::new(format!("handler_{}", (i + 1) % 500)),
            dst_symbol_id: None,
            weight: 1.0,
        })
        .collect();
    let ranked = pagerank::pagerank(&symbols, &refs, &[], 0.85, 30);

    c.bench_function("render_repo_map/500_symbols", |b| {
        b.iter(|| pagerank::render_repo_map(&symbols, &ranked, 2000))
    });
}

fn bench_token_count(c: &mut Criterion) {
    let code =
        "pub fn validate_email_address(email: &str) -> Result<bool, ValidationError> {\n    \
                if email.is_empty() { return Err(ValidationError::Empty); }\n    \
                let parts: Vec<&str> = email.splitn(2, '@').collect();\n    \
                Ok(parts.len() == 2 && !parts[0].is_empty() && parts[1].contains('.'))\n}\n";

    c.bench_function("count_tokens/5_lines", |b| b.iter(|| count_tokens(code)));
}

criterion_group!(
    benches,
    bench_tantivy_search,
    bench_text_grep,
    bench_fuzzy_rank,
    bench_repo_map_render,
    bench_token_count
);
criterion_main!(benches);
