use compact_str::CompactString;
use criterion::{criterion_group, criterion_main, Criterion};
use recon_core::lang::Language;
use recon_core::symbol::{FileMeta, Ref, Symbol, SymbolKind};
use recon_storage::store::Store;
use std::path::PathBuf;
use std::sync::Arc;

fn make_symbol(i: u64) -> Symbol {
    // Distribute symbols across ~45 symbols/file to mirror real repos (zed-main:
    // 80 K symbols across 1780 files). Path sharing is what makes the interner win.
    let file_idx = i / 45;
    Symbol {
        id: i,
        path: Arc::new(PathBuf::from(format!("src/file_{file_idx}.rs"))),
        name: CompactString::new(format!("sym_{i}")),
        qualified_name: CompactString::new(format!("crate::mod::sym_{i}")),
        kind: SymbolKind::Function,
        signature: Some(format!("fn sym_{i}()").into()),
        doc: None,
        parent_id: None,
        byte_range: 0..100,
        line_range: 1..=5,
        body_hash: [0u8; 32],
        lang: Language::Rust,
    }
}

fn make_ref(i: u64) -> Ref {
    let file_idx = i / 45;
    Ref {
        src_path: Arc::new(PathBuf::from(format!("src/file_{file_idx}.rs"))),
        src_symbol_id: i,
        ident: CompactString::new(format!("call_{i}")),
        dst_symbol_id: Some(i + 1),
        weight: 1.0,
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
    store.insert_symbols_batch(&symbols).unwrap();
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
    c.bench_function("insert_symbols_batch/1k", |b| {
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
            store.insert_symbols_batch(&symbols).unwrap();
        })
    });
}

/// Bulk load 80 K symbols — mirrors the `cached_all_symbols` refresh path.
/// Guards the `row_to_symbol_interned` Arc<PathBuf> dedup win.
fn bench_all_symbols_80k(c: &mut Criterion) {
    let store = setup_store_multi_file(80_000);
    c.bench_function("all_symbols/80k_across_1780_files", |b| {
        b.iter(|| store.all_symbols().unwrap())
    });
}

/// Bulk load 300 K refs — mirrors the PageRank / repo_map refresh path.
/// Guards the `all_refs` path interner win.
fn bench_all_refs_300k(c: &mut Criterion) {
    let store = setup_store_with_refs(300_000);
    c.bench_function("all_refs/300k_across_1780_files", |b| {
        b.iter(|| store.all_refs().unwrap())
    });
}

fn setup_store_multi_file(n: u64) -> Store {
    let store = Store::open_memory().unwrap();
    // Need file rows for FK, one per distinct path used in make_symbol.
    let n_files = n.div_ceil(45);
    for file_idx in 0..n_files {
        let meta = FileMeta {
            path: PathBuf::from(format!("src/file_{file_idx}.rs")),
            lang: Language::Rust,
            size_bytes: 1000,
            content_hash: [0u8; 32],
            mtime: 0,
            indexed_at: 0,
        };
        store.upsert_file(&meta).unwrap();
    }
    let symbols: Vec<Symbol> = (0..n).map(make_symbol).collect();
    store.insert_symbols_batch(&symbols).unwrap();
    store
}

fn setup_store_with_refs(n: u64) -> Store {
    let store = setup_store_multi_file(n.min(80_000));
    let refs: Vec<Ref> = (0..n).map(make_ref).collect();
    store.insert_refs(&refs).unwrap();
    store
}

criterion_group!(
    benches,
    bench_symbol_exact_lookup,
    bench_symbol_fuzzy_search,
    bench_batch_insert,
    bench_all_symbols_80k,
    bench_all_refs_300k,
);
criterion_main!(benches);
