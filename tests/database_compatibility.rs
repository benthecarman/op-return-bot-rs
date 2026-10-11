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

/// The data migration decodes a value only when the old reader did: valid
/// hex of even length, stored as text or as a blob. Everything else keeps
/// its bytes.
#[tokio::test]
async fn converts_legacy_hex_messages_to_raw_bytes() {
    let temp = tempfile::tempdir().unwrap();
    let database = Database::connect(&DatabaseConfig {
        path: temp.path().join("invoices.sqlite"),
        max_connections: 1,
        busy_timeout_seconds: 5,
    })
    .await
    .unwrap();
    sqlx::raw_sql(
        "CREATE TABLE op_return_requests (id INTEGER PRIMARY KEY, message_bytes BLOB NOT NULL)",
    )
    .execute(database.pool())
    .await
    .unwrap();
    sqlx::raw_sql(
        "INSERT INTO op_return_requests (id, message_bytes) VALUES \
         (1, '68656C6c6f'), (2, X'FF002A'), (3, 'hello'), \
         (4, CAST('6869' AS BLOB)), (5, 'abc'), (6, '6g')",
    )
    .execute(database.pool())
    .await
    .unwrap();

    sqlx::raw_sql(include_str!("../migrations/0009_raw_message_bytes.sql"))
        .execute(database.pool())
        .await
        .unwrap();

    let converted: Vec<(i64, String, Vec<u8>)> = sqlx::query_as(
        "SELECT id, typeof(message_bytes), message_bytes FROM op_return_requests ORDER BY id",
    )
    .fetch_all(database.pool())
    .await
    .unwrap();
    let expected: [&[u8]; 6] = [b"hello", b"\xff\x00\x2a", b"hello", b"hi", b"abc", b"6g"];
    for ((id, kind, bytes), want) in converted.into_iter().zip(expected) {
        assert_eq!(kind, "blob", "row {id}");
        assert_eq!(bytes, want, "row {id}");
    }
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
