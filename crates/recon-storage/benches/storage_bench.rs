use compact_str::CompactString;
use criterion::{criterion_group, criterion_main, Criterion};
use recon_core::lang::Language;
use recon_core::symbol::{FileMeta, Symbol, SymbolKind};
use recon_storage::store::Store;
use std::path::PathBuf;
use std::sync::Arc;

fn make_symbol(i: u64) -> Symbol {
    Symbol {
        id: i,
        path: Arc::new(PathBuf::from("src/lib.rs")),
        name: CompactString::new(format!("sym_{i}")),
        qualified_name: CompactString::new(format!("crate::mod::sym_{i}")),
        kind: SymbolKind::Function,
        signature: Some(format!("fn sym_{i}()")),
        doc: None,
        parent_id: None,
        byte_range: 0..100,
        line_range: 1..=5,
        body_hash: [0u8; 32],
        lang: Language::Rust,
    }
}

fn setup_store(n: u64) -> Store {
    let store = Store::open_memory().unwrap();
    let meta = FileMeta {
        path: PathBuf::from("src/lib.rs"),
        lang: Language::Rust,
        size_bytes: 1000,
        content_hash: [0u8; 32],
        mtime: 0,
        indexed_at: 0,
    };
    store.upsert_file(&meta).unwrap();
    let symbols: Vec<Symbol> = (0..n).map(make_symbol).collect();
    store.upsert_symbols_batch(&symbols).unwrap();
    store
}

fn bench_symbol_exact_lookup(c: &mut Criterion) {
    let store = setup_store(10_000);
    c.bench_function("find_symbols_exact/10k", |b| {
        b.iter(|| store.find_symbols_exact("sym_5000", 10).unwrap())
    });
}

fn bench_symbol_fuzzy_search(c: &mut Criterion) {
    let store = setup_store(10_000);
    c.bench_function("search_symbols_fuzzy/10k", |b| {
        b.iter(|| store.search_symbols_fuzzy("sym_50", 20).unwrap())
    });
}

fn bench_batch_insert(c: &mut Criterion) {
    let symbols: Vec<Symbol> = (0..1000).map(make_symbol).collect();
    c.bench_function("upsert_symbols_batch/1k", |b| {
        b.iter(|| {
            let store = Store::open_memory().unwrap();
            let meta = FileMeta {
                path: PathBuf::from("src/lib.rs"),
                lang: Language::Rust,
                size_bytes: 1000,
                content_hash: [0u8; 32],
                mtime: 0,
                indexed_at: 0,
            };
            store.upsert_file(&meta).unwrap();
            store.upsert_symbols_batch(&symbols).unwrap();
        })
    });
}

criterion_group!(
    benches,
    bench_symbol_exact_lookup,
    bench_symbol_fuzzy_search,
    bench_batch_insert
);
criterion_main!(benches);
