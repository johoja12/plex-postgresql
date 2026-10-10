//! Run without LD_PRELOAD; benchmarks the Rust result-conversion helpers directly.
use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use plex_pg_core::db_interpose_helpers::{
    rust_decltype_cache_insert, rust_decltype_cache_lookup_alias,
    rust_should_mask_collection_metadata_type,
};
use std::ffi::CString;

fn bench_result_hotpath(c: &mut Criterion) {
    let metadata = CString::new("metadata_items_metadata_type").unwrap();
    let ordinary = CString::new("metadata_items_id").unwrap();
    let mut group = c.benchmark_group("collection_mask_ordinary_cell");
    for length in [64, 64_000, 700_000] {
        let sql = CString::new(format!("select {}", "x".repeat(length))).unwrap();
        for (label, column, value) in [
            ("ordinary_value", &metadata, 4),
            ("ordinary_column", &ordinary, 18),
        ] {
            group.bench_with_input(BenchmarkId::new(label, length), &sql, |b, sql| {
                b.iter(|| {
                    black_box(rust_should_mask_collection_metadata_type(
                        black_box(sql.as_ptr()),
                        black_box(column.as_ptr()),
                        black_box(value),
                    ))
                })
            });
        }
    }
    group.finish();

    let decl = CString::new("INTEGER").unwrap();
    let mut group = c.benchmark_group("decltype_alias_cached");
    for count in [10, 100, 1000] {
        for n in 0..count {
            let key = CString::new(format!("bench_table_{n}_id")).unwrap();
            rust_decltype_cache_insert(key.as_ptr(), decl.as_ptr());
        }
        for (label, alias) in [
            ("hit", "bench_table_0_parent_id"),
            ("miss", "unknown_expression"),
        ] {
            let alias = CString::new(alias).unwrap();
            black_box(rust_decltype_cache_lookup_alias(alias.as_ptr()));
            group.bench_with_input(BenchmarkId::new(label, count), &alias, |b, alias| {
                b.iter(|| black_box(rust_decltype_cache_lookup_alias(black_box(alias.as_ptr()))))
            });
        }
    }
    group.finish();
}
criterion_group!(benches, bench_result_hotpath);
criterion_main!(benches);
