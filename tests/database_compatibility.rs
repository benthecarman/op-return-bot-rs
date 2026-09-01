use std::{env, path::PathBuf};

use op_return_bot::{Database, config::DatabaseConfig};
use sqlx::Row;

#[tokio::test]
async fn migrates_a_fresh_database() {
    let temp = tempfile::tempdir().unwrap();
    let database = Database::connect(&DatabaseConfig {
        path: temp.path().join("invoices.sqlite"),
        max_connections: 1,
        busy_timeout_seconds: 5,
    })
    .await
    .unwrap();

    database.migrate().await.unwrap();

    let columns = sqlx::query("PRAGMA table_info(invoices)")
        .fetch_all(database.pool())
        .await
        .unwrap();
    let names: Vec<String> = columns
        .iter()
        .map(|row| row.get::<String, _>("name"))
        .collect();

    assert!(names.iter().any(|name| name == "lightning_backend"));
    assert!(names.iter().any(|name| name == "claim_preimage"));
}

#[tokio::test]
#[ignore = "requires ORB_PRODUCTION_DB"]
async fn migrates_a_production_snapshot_without_losing_rows() {
    let source = PathBuf::from(
        env::var_os("ORB_PRODUCTION_DB")
            .expect("set ORB_PRODUCTION_DB to the path of a production database snapshot"),
    );
    let temp = tempfile::tempdir().unwrap();
    let destination = temp.path().join("invoices.sqlite");
    std::fs::copy(&source, &destination).unwrap();

    let database = Database::connect(&DatabaseConfig {
        path: destination,
        max_connections: 1,
        busy_timeout_seconds: 5,
    })
    .await
    .unwrap();
    let before: i64 = sqlx::query_scalar("SELECT count(*) FROM op_return_requests")
        .fetch_one(database.pool())
        .await
        .unwrap();

    database.migrate().await.unwrap();

    let after: i64 = sqlx::query_scalar("SELECT count(*) FROM op_return_requests")
        .fetch_one(database.pool())
        .await
        .unwrap();
    let bad_backend: i64 =
        sqlx::query_scalar("SELECT count(*) FROM invoices WHERE lightning_backend != 'lnd'")
            .fetch_one(database.pool())
            .await
            .unwrap();
    let integrity: String = sqlx::query_scalar("PRAGMA quick_check")
        .fetch_one(database.pool())
        .await
        .unwrap();

    assert_eq!(before, after);
    assert_eq!(bad_backend, 0);
    assert_eq!(integrity, "ok");
}
