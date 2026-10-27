use criterion::{
    BatchSize, BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main,
};
use native_db::*;
use native_model::{Model, native_model};
use sandpolis_instance::database::{
    Data, DataCondition, DataCreation, DatabaseManager, Resident, ResidentVec, config,
};
use sandpolis_instance::realm::RealmName;
use sandpolis_macros::data;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

// The `#[data]` macro expands to `crate::database::...` and `crate::inventory`
// paths when compiled inside the `sandpolis-instance` package — which, for a
// bench, is this file's own crate root. Re-export the real modules so those
// paths resolve.
pub use sandpolis_instance::{database, inventory};

// Test data structures
#[data]
#[derive(Default)]
pub struct BenchData {
    #[secondary_key]
    pub name: String,
    #[secondary_key]
    pub value: i32,
    pub data: Vec<u8>,
}

#[data(temporal)]
#[derive(Default)]
pub struct BenchTemporalData {
    #[secondary_key]
    pub name: String,
    pub value: i32,
}

/// Helper to create test database
fn create_test_db() -> DatabaseManager {
    let models = Box::leak(Box::new(native_db::Models::new()));
    models.define::<BenchData>().unwrap();
    models.define::<BenchTemporalData>().unwrap();

    DatabaseManager::new(
        config::DatabaseConfig {
            storage: None,
            key: Default::default(),
        },
        models,
        sandpolis_instance::database::WriteAuthority::Full,
    )
    .unwrap()
}

/// Benchmark: Creating a Resident
fn bench_resident_create(c: &mut Criterion) {
    let runtime = tokio::runtime::Runtime::new().unwrap();

    c.bench_function("resident_create", |b| {
        b.to_async(&runtime).iter(|| async {
            let database = create_test_db();
            let db = database.realm(RealmName::default()).unwrap();

            black_box(db.resident::<BenchData>(()).unwrap());
        });
    });
}

/// Benchmark: Single update operation
fn bench_resident_update(c: &mut Criterion) {
    let runtime = tokio::runtime::Runtime::new().unwrap();

    c.bench_function("resident_update", |b| {
        b.to_async(&runtime).iter(|| async {
            let database = create_test_db();
            let db = database.realm(RealmName::default()).unwrap();
            let resident: Resident<BenchData> = db.resident(()).unwrap();

            black_box(
                resident
                    .update(|data| {
                        data.value = 42;
                        Ok(())
                    })
                    .unwrap(),
            );
        });
    });
}

/// Benchmark: Multiple sequential updates
fn bench_resident_sequential_updates(c: &mut Criterion) {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let mut group = c.benchmark_group("resident_sequential_updates");

    for count in [10, 100, 1000].iter() {
        group.throughput(Throughput::Elements(*count as u64));
        group.bench_with_input(BenchmarkId::from_parameter(count), count, |b, &count| {
            b.to_async(&runtime).iter(|| async move {
                let database = create_test_db();
                let db = database.realm(RealmName::default()).unwrap();
                let resident: Resident<BenchData> = db.resident(()).unwrap();

                for i in 0..count {
                    resident
                        .update(|data| {
                            data.value = i;
                            Ok(())
                        })
                        .unwrap();
                }
            });
        });
    }
    group.finish();
}

/// Benchmark: Concurrent updates
fn bench_resident_concurrent_updates(c: &mut Criterion) {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let mut group = c.benchmark_group("resident_concurrent_updates");

    for count in [10, 50, 100].iter() {
        group.throughput(Throughput::Elements(*count as u64));
        group.bench_with_input(BenchmarkId::from_parameter(count), count, |b, &count| {
            b.to_async(&runtime).iter(|| async move {
                let database = create_test_db();
                let db = database.realm(RealmName::default()).unwrap();
                let resident: Resident<BenchData> = db.resident(()).unwrap();

                let mut handles = vec![];
                for i in 0..count {
                    let r = resident.clone();
                    let handle = tokio::spawn(async move {
                        r.update(|data| {
                            data.value = i;
                            Ok(())
                        })
                    });
                    handles.push(handle);
                }

                for handle in handles {
                    handle.await.unwrap().unwrap();
                }
            });
        });
    }
    group.finish();
}

/// Benchmark: ResidentVec push operations
fn bench_resident_vec_push(c: &mut Criterion) {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let mut group = c.benchmark_group("resident_vec_push");

    for count in [10, 100, 1000].iter() {
        group.throughput(Throughput::Elements(*count as u64));
        group.bench_with_input(BenchmarkId::from_parameter(count), count, |b, &count| {
            b.to_async(&runtime).iter(|| async move {
                let database = create_test_db();
                let db = database.realm(RealmName::default()).unwrap();
                let resident_vec: ResidentVec<BenchData> = db.resident_vec(()).unwrap();

                for i in 0..count {
                    resident_vec
                        .push(BenchData {
                            name: format!("item_{}", i),
                            value: i,
                            data: vec![0u8; 100],
                            ..Default::default()
                        })
                        .unwrap();
                }
            });
        });
    }
    group.finish();
}

/// Benchmark: ResidentVec remove operations
fn bench_resident_vec_remove(c: &mut Criterion) {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let mut group = c.benchmark_group("resident_vec_remove");

    for count in [10, 100, 500].iter() {
        group.throughput(Throughput::Elements(*count as u64));
        group.bench_with_input(BenchmarkId::from_parameter(count), count, |b, &count| {
            b.to_async(&runtime).iter(|| async move {
                let database = create_test_db();
                let db = database.realm(RealmName::default()).unwrap();
                let resident_vec: ResidentVec<BenchData> = db.resident_vec(()).unwrap();

                // Setup: push items
                let mut ids = vec![];
                for i in 0..count {
                    let r = resident_vec
                        .push(BenchData {
                            name: format!("item_{}", i),
                            value: i,
                            data: vec![0u8; 100],
                            ..Default::default()
                        })
                        .unwrap();
                    ids.push(r.read().id());
                }

                // Benchmark: remove items
                for id in ids {
                    resident_vec.remove(id).unwrap();
                }

                tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
            });
        });
    }
    group.finish();
}

/// Benchmark: Query operations with different result sizes
fn bench_query_operations(c: &mut Criterion) {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let mut group = c.benchmark_group("query_operations");

    for dataset_size in [100, 1000, 5000].iter() {
        group.throughput(Throughput::Elements(*dataset_size as u64));
        group.bench_with_input(
            BenchmarkId::new("equal", dataset_size),
            dataset_size,
            |b, &size| {
                b.to_async(&runtime).iter(|| async move {
                    let database = create_test_db();
                    let db = database.realm(RealmName::default()).unwrap();

                    // Setup: insert test data
                    {
                        let rw = db
                            .write(sandpolis_instance::database::DataScope::Global)
                            .unwrap();
                        for i in 0..size {
                            rw.insert(BenchData {
                                name: format!("item_{}", i % 10),
                                value: i,
                                data: vec![0u8; 100],
                                ..Default::default()
                            })
                            .unwrap();
                        }
                        rw.commit().unwrap();
                    }

                    // Benchmark: query with equal condition
                    let _resident_vec: ResidentVec<BenchData> = db
                        .resident_vec(DataCondition::equal(
                            BenchDataKey::name,
                            "item_5".to_string(),
                        ))
                        .unwrap();
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("range", dataset_size),
            dataset_size,
            |b, &size| {
                b.to_async(&runtime).iter(|| async move {
                    let database = create_test_db();
                    let db = database.realm(RealmName::default()).unwrap();

                    // Setup: insert test data
                    {
                        let rw = db
                            .write(sandpolis_instance::database::DataScope::Global)
                            .unwrap();
                        for i in 0..size {
                            rw.insert(BenchData {
                                name: format!("item_{:04}", i),
                                value: i,
                                data: vec![0u8; 100],
                                ..Default::default()
                            })
                            .unwrap();
                        }
                        rw.commit().unwrap();
                    }

                    // Benchmark: query with range condition
                    let _resident_vec: ResidentVec<BenchData> = db
                        .resident_vec(DataCondition::range(
                            BenchDataKey::name,
                            "item_0100".to_string()..="item_0200".to_string(),
                        ))
                        .unwrap();
                });
            },
        );
    }
    group.finish();
}

/// Benchmark: Listener overhead
fn bench_listener_overhead(c: &mut Criterion) {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let mut group = c.benchmark_group("listener_overhead");

    for listener_count in [0, 1, 5, 10].iter() {
        group.bench_with_input(
            BenchmarkId::from_parameter(listener_count),
            listener_count,
            |b, &count| {
                b.to_async(&runtime).iter(|| async move {
                    let database = create_test_db();
                    let db = database.realm(RealmName::default()).unwrap();
                    let resident_vec: ResidentVec<BenchData> = db.resident_vec(()).unwrap();

                    // Setup: register listeners
                    for _ in 0..count {
                        let counter = Arc::new(AtomicUsize::new(0));
                        resident_vec.listen(move |_event| {
                            counter.fetch_add(1, Ordering::SeqCst);
                        });
                    }

                    // Benchmark: push with listeners active
                    for i in 0..10 {
                        resident_vec
                            .push(BenchData {
                                name: format!("item_{}", i),
                                value: i,
                                data: vec![0u8; 100],
                                ..Default::default()
                            })
                            .unwrap();
                    }

                    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
                });
            },
        );
    }
    group.finish();
}

/// Benchmark: Temporal data (historical revisions)
fn bench_temporal_data(c: &mut Criterion) {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let mut group = c.benchmark_group("temporal_data");

    for revision_count in [10, 50, 100].iter() {
        group.throughput(Throughput::Elements(*revision_count as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(revision_count),
            revision_count,
            |b, &count| {
                b.to_async(&runtime).iter(|| async move {
                    let database = create_test_db();
                    let db = database.realm(RealmName::default()).unwrap();
                    let resident: Resident<BenchTemporalData> = db.resident(()).unwrap();

                    // Create multiple revisions
                    for i in 0..count {
                        resident
                            .update(|data| {
                                data.value = i;
                                Ok(())
                            })
                            .unwrap();
                    }

                    // Query history
                    let _history = resident.history(DataCreation::all()).unwrap();
                });
            },
        );
    }
    group.finish();
}

/// Benchmark: Reading one record's history out of a realm holding many
/// independent temporal records of the same type.
fn bench_temporal_history(c: &mut Criterion) {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let mut group = c.benchmark_group("temporal_history");

    for other_records in [10, 100, 500].iter() {
        group.bench_with_input(
            BenchmarkId::from_parameter(other_records),
            other_records,
            |b, &other_records| {
                // `history` is read-only, so the fixture is built once and the
                // measured loop is the lookup alone.
                let (_database, resident) = runtime.block_on(async move {
                    let database = create_test_db();
                    let db = database.realm(RealmName::default()).unwrap();

                    // The record we'll ask about, with a history of its own.
                    let resident: Resident<BenchTemporalData> = db
                        .resident_with(
                            DataCondition::equal(BenchTemporalDataKey::name, "subject".to_string()),
                            || BenchTemporalData {
                                name: "subject".to_string(),
                                ..Default::default()
                            },
                        )
                        .unwrap();
                    for i in 0..10 {
                        resident
                            .update(|data| {
                                data.value = i;
                                Ok(())
                            })
                            .unwrap();
                    }

                    // ...surrounded by unrelated records of the same type, each
                    // carrying a revision chain of its own.
                    for i in 0..other_records {
                        let name = format!("other_{i}");
                        let other: Resident<BenchTemporalData> = db
                            .resident_with(
                                DataCondition::equal(BenchTemporalDataKey::name, name.clone()),
                                || BenchTemporalData {
                                    name: name.clone(),
                                    ..Default::default()
                                },
                            )
                            .unwrap();
                        other
                            .update(|data| {
                                data.value = 1;
                                Ok(())
                            })
                            .unwrap();
                    }

                    (database, resident)
                });

                b.iter(|| black_box(resident.history(DataCreation::all()).unwrap()));
            },
        );
    }
    group.finish();
}

/// Benchmark: Data size impact on operations
fn bench_data_size_impact(c: &mut Criterion) {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let mut group = c.benchmark_group("data_size_impact");

    for data_size in [100, 1000, 10000].iter() {
        group.throughput(Throughput::Bytes(*data_size as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(data_size),
            data_size,
            |b, &size| {
                b.to_async(&runtime).iter(|| async move {
                    let database = create_test_db();
                    let db = database.realm(RealmName::default()).unwrap();
                    let resident_vec: ResidentVec<BenchData> = db.resident_vec(()).unwrap();

                    for i in 0..10 {
                        resident_vec
                            .push(BenchData {
                                name: format!("item_{}", i),
                                value: i,
                                data: vec![0u8; size],
                                ..Default::default()
                            })
                            .unwrap();
                    }
                });
            },
        );
    }
    group.finish();
}

/// Benchmark: Transaction throughput
fn bench_transaction_throughput(c: &mut Criterion) {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let mut group = c.benchmark_group("transaction_throughput");

    for batch_size in [1, 10, 100].iter() {
        group.throughput(Throughput::Elements(*batch_size as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(batch_size),
            batch_size,
            |b, &size| {
                b.to_async(&runtime).iter(|| async move {
                    let database = create_test_db();
                    let db = database.realm(RealmName::default()).unwrap();

                    let rw = db
                        .write(sandpolis_instance::database::DataScope::Global)
                        .unwrap();
                    for i in 0..size {
                        rw.insert(BenchData {
                            name: format!("item_{}", i),
                            value: i,
                            data: vec![0u8; 100],
                            ..Default::default()
                        })
                        .unwrap();
                    }
                    rw.commit().unwrap();
                });
            },
        );
    }
    group.finish();
}

/// Benchmark: Applying a replicated snapshot, as a `SyncRequester` does when a
/// subscription opens (and as ownership hydration waits on).
fn bench_sync_apply(c: &mut Criterion) {
    use sandpolis_instance::database::sync::{SyncOp, SyncRecord, SyncRegistry};

    let mut registry = SyncRegistry::new();
    registry.register::<BenchData>();

    let mut group = c.benchmark_group("sync_apply");
    for count in [10, 100, 1000].iter() {
        let records: Vec<SyncRecord> = (0..*count)
            .map(|i| SyncRecord {
                model_id: BenchData::native_model_id(),
                op: SyncOp::Upsert,
                bytes: native_model::encode(&BenchData {
                    name: format!("item_{i}"),
                    value: i,
                    data: vec![0u8; 100],
                    ..Default::default()
                })
                .unwrap(),
            })
            .collect();

        group.throughput(Throughput::Elements(*count as u64));
        group.bench_with_input(BenchmarkId::from_parameter(count), &records, |b, records| {
            // Creating (and dropping) the database dwarfs a small snapshot, so
            // both stay out of the measurement.
            b.iter_batched(
                || {
                    let database = create_test_db();
                    let db = database.realm(RealmName::default()).unwrap();
                    (database, db)
                },
                |(database, db)| {
                    assert!(registry.apply_all(&db, records).is_empty());
                    (database, db)
                },
                BatchSize::PerIteration,
            );
        });
    }
    group.finish();
}

/// Benchmark: Watcher event processing latency
fn bench_watcher_latency(c: &mut Criterion) {
    let runtime = tokio::runtime::Runtime::new().unwrap();

    c.bench_function("watcher_event_latency", |b| {
        b.to_async(&runtime).iter(|| async {
            let database = create_test_db();
            let db = database.realm(RealmName::default()).unwrap();
            let resident: Resident<BenchData> = db.resident(()).unwrap();

            // Measure time from update to watcher processing
            let start = std::time::Instant::now();
            resident
                .update(|data| {
                    data.value = 42;
                    Ok(())
                })
                .unwrap();

            // Wait for watcher to process
            tokio::time::sleep(tokio::time::Duration::from_micros(100)).await;
            let _elapsed = start.elapsed();
        });
    });
}

criterion_group!(
    benches,
    bench_resident_create,
    bench_resident_update,
    bench_resident_sequential_updates,
    bench_resident_concurrent_updates,
    bench_resident_vec_push,
    bench_resident_vec_remove,
    bench_query_operations,
    bench_listener_overhead,
    bench_temporal_data,
    bench_temporal_history,
    bench_data_size_impact,
    bench_transaction_throughput,
    bench_sync_apply,
    bench_watcher_latency,
);

criterion_main!(benches);
