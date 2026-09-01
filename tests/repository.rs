use std::time::Duration;

use op_return_bot::{
    Database,
    config::DatabaseConfig,
    domain::{LightningBackend, PaymentStatus},
    repository::{
        CompletedRequest, ExpiredCandidate, NewInvoice, NewNip5, NewOnChainPayment, NewRequest,
        NewZap, Repository,
    },
};
use tempfile::TempDir;

async fn database() -> (TempDir, Database) {
    let directory = tempfile::tempdir().unwrap();
    let database = Database::connect(&DatabaseConfig {
        path: directory.path().join("test.sqlite"),
        max_connections: 1,
        busy_timeout_seconds: Duration::from_secs(1).as_secs(),
    })
    .await
    .unwrap();
    database.migrate().await.unwrap();
    (directory, database)
}

fn request(message: &[u8]) -> NewRequest<'_> {
    NewRequest {
        message,
        no_twitter: false,
        fee_rate_sat_vb: 12,
        node_id: None,
        telegram_id: None,
        nostr_key: None,
        created_at: 1_788_000_000,
    }
}

fn invoice() -> NewInvoice<'static> {
    NewInvoice {
        payment_hash: "abababababababababababababababababababababababababababababababab",
        bolt11: "lnbcrt1test",
        backend: LightningBackend::Lnd,
        amount_sats: 5_000,
        claim_preimage: None,
    }
}

#[tokio::test]
async fn creates_and_reads_a_legacy_compatible_invoice() {
    let (_directory, database) = database().await;
    let repository = Repository::new(database.clone());
    let created = repository
        .create_invoice_request(&request(b"hello"), &invoice())
        .await
        .unwrap();

    let stored_type: String =
        sqlx::query_scalar("SELECT typeof(message_bytes) FROM op_return_requests WHERE id = ?")
            .bind(created.request.id)
            .fetch_one(database.pool())
            .await
            .unwrap();
    let stored_message: String =
        sqlx::query_scalar("SELECT message_bytes FROM op_return_requests WHERE id = ?")
            .bind(created.request.id)
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert_eq!(stored_type, "text");
    assert_eq!(stored_message, "68656c6c6f");

    let found = repository
        .find_by_payment_hash(invoice().payment_hash)
        .await
        .unwrap();
    assert_eq!(found.request.message, b"hello");
    assert_eq!(found.request.fee_rate_sat_vb, 12);
    assert_eq!(
        found.request.payment_status(false, None),
        PaymentStatus::Unpaid
    );
}

#[tokio::test]
async fn creates_unified_payment_atomically() {
    let (_directory, database) = database().await;
    let repository = Repository::new(database);
    let on_chain = NewOnChainPayment {
        address: "bcrt1qexample",
        expected_amount_sats: 5_000,
    };
    let created = repository
        .create_unified_request(&request(b"unified"), &invoice(), &on_chain, None)
        .await
        .unwrap();
    assert_eq!(created.on_chain.unwrap().expected_amount_sats, 5_000);

    let found = repository
        .find_by_payment_hash(invoice().payment_hash)
        .await
        .unwrap();
    assert_eq!(found.on_chain.unwrap().address, "bcrt1qexample");
}

#[tokio::test]
async fn creates_nip5_with_its_payment() {
    let (_directory, database) = database().await;
    let repository = Repository::new(database);
    let on_chain = NewOnChainPayment {
        address: "bcrt1qnip5",
        expected_amount_sats: 5_000,
    };
    repository
        .create_unified_request(
            &request(b"nip5:alice:key"),
            &invoice(),
            &on_chain,
            Some(&NewNip5 {
                name: "alice",
                public_key: "key",
            }),
        )
        .await
        .unwrap();
    assert!(repository.nip5_name_exists("ALICE").await.unwrap());
}

#[tokio::test]
async fn stores_bot_state_and_reports_empty_database() {
    let (_directory, database) = database().await;
    let repository = Repository::new(database);
    assert_eq!(
        repository
            .accounting_report()
            .await
            .unwrap()
            .completed_requests,
        0
    );
    assert_eq!(repository.service_state("offset").await.unwrap(), None);
    repository.set_service_state("offset", "42").await.unwrap();
    assert_eq!(
        repository.service_state("offset").await.unwrap().as_deref(),
        Some("42")
    );
}

#[tokio::test]
async fn creates_the_legacy_accounting_report() {
    let (_directory, database) = database().await;
    let repository = Repository::new(database);
    let message = vec![b'x'; 81];
    let created = repository
        .create_unified_request(
            &request(&message),
            &invoice(),
            &on_chain("bcrt1qreport"),
            Some(&NewNip5 {
                name: "report",
                public_key: "key",
            }),
        )
        .await
        .unwrap();
    assert!(
        repository
            .mark_on_chain_paid("bcrt1qreport", 5_000, &"ab".repeat(32))
            .await
            .unwrap()
    );
    repository
        .complete_request(
            created.request.id,
            &CompletedRequest {
                txid: &"cd".repeat(32),
                chain_fee_sats: 123,
                vsize: 456,
                profit_sats: Some(4_877),
                btc_price_cents: 0,
            },
        )
        .await
        .unwrap();
    let zap_hash = "ef".repeat(32);
    repository
        .create_zap(&NewZap {
            payment_hash: &zap_hash,
            bolt11: "lnbcrt1reportzap",
            recipient_key: "recipient",
            amount_msats: 21_000,
            request_json: "{}",
            backend: LightningBackend::Lnd,
            created_at: request(b"").created_at,
        })
        .await
        .unwrap();
    repository
        .mark_zap_published(&zap_hash, &"12".repeat(32))
        .await
        .unwrap();

    let report = repository.accounting_report().await.unwrap();
    assert_eq!(report.completed_requests, 1);
    assert_eq!(report.non_standard_requests, 1);
    assert_eq!(report.on_chain_requests, 1);
    assert_eq!(report.chain_fees_sats, 123);
    assert_eq!(report.profit_sats, 4_877);
    assert_eq!(report.chain_vbytes, 456);
    assert_eq!(report.non_standard_vbytes, 456);
    assert_eq!(report.completed_nip5s, 1);
    assert_eq!(report.zapped_sats, 21);

    let filtered = repository
        .accounting_report_since(Some(request(b"").created_at))
        .await
        .unwrap();
    assert_eq!(filtered.completed_requests, 0);
    assert_eq!(filtered.zapped_sats, 0);
}

#[tokio::test]
async fn payment_updates_are_idempotent() {
    let (_directory, database) = database().await;
    let repository = Repository::new(database);
    repository
        .create_invoice_request(&request(b"hello"), &invoice())
        .await
        .unwrap();

    assert!(
        repository
            .mark_invoice_paid(invoice().payment_hash)
            .await
            .unwrap()
    );
    assert!(
        !repository
            .mark_invoice_paid(invoice().payment_hash)
            .await
            .unwrap()
    );
}

fn on_chain(address: &str) -> NewOnChainPayment<'_> {
    NewOnChainPayment {
        address,
        expected_amount_sats: 5_000,
    }
}

#[tokio::test]
async fn lists_on_chain_and_paid_but_unpublished_requests() {
    let (_directory, database) = database().await;
    let repository = Repository::new(database);
    let created = repository
        .create_unified_request(&request(b"open"), &invoice(), &on_chain("bcrt1qopen"), None)
        .await
        .unwrap();
    let request_id = created.request.id;

    let addresses = repository
        .open_on_chain_payments(1_000_000_000)
        .await
        .unwrap();
    assert_eq!(addresses.len(), 1);
    assert_eq!(addresses[0].address, "bcrt1qopen");
    assert!(
        repository
            .paid_unpublished_request_ids()
            .await
            .unwrap()
            .is_empty()
    );

    let payment_txid = "cd".repeat(32);
    assert!(
        repository
            .mark_on_chain_paid("bcrt1qopen", 6_000, &payment_txid)
            .await
            .unwrap()
    );
    assert!(
        repository
            .open_on_chain_payments(1_000_000_000)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        repository.paid_unpublished_request_ids().await.unwrap(),
        vec![request_id]
    );
    let record = repository.find_record(request_id).await.unwrap();
    let paid = record.on_chain.unwrap();
    assert_eq!(paid.txid.as_deref(), Some(payment_txid.as_str()));
    assert_eq!(paid.amount_paid_sats, Some(6_000));
    assert!(record.invoice.is_some());
    assert!(repository.find_record(request_id + 1).await.is_err());
}

#[tokio::test]
async fn closes_only_expired_unpaid_requests() {
    let (_directory, database) = database().await;
    let repository = Repository::new(database);
    let hashes = ["aa".repeat(32), "bb".repeat(32), "cc".repeat(32)];
    let bolt11s = ["lnbcrt1a", "lnbcrt1b", "lnbcrt1c"];
    let mut ids = Vec::new();
    for (index, hash) in hashes.iter().enumerate() {
        let new_invoice = NewInvoice {
            payment_hash: hash,
            bolt11: bolt11s[index],
            backend: LightningBackend::Lnd,
            amount_sats: 5_000,
            claim_preimage: None,
        };
        let created = if index == 1 {
            repository
                .create_unified_request(
                    &request(b"unified"),
                    &new_invoice,
                    &on_chain("bcrt1qunified"),
                    None,
                )
                .await
                .unwrap()
        } else {
            repository
                .create_invoice_request(&request(b"lightning"), &new_invoice)
                .await
                .unwrap()
        };
        ids.push(created.request.id);
    }
    // The third request was paid and must never close.
    assert!(repository.mark_invoice_paid(&hashes[2]).await.unwrap());
    let created_at = request(b"").created_at;

    // Only the Lightning-only window has passed.
    let candidates = repository
        .expired_request_candidates(created_at + 1, created_at - 1, 10)
        .await
        .unwrap();
    assert_eq!(
        candidates,
        vec![ExpiredCandidate {
            request_id: ids[0],
            payment_hash: Some(hashes[0].clone()),
            backend: Some(LightningBackend::Lnd),
        }]
    );

    // Both windows have passed.
    let candidates = repository
        .expired_request_candidates(created_at + 1, created_at + 1, 10)
        .await
        .unwrap();
    let candidate_ids: Vec<i64> = candidates.iter().map(|c| c.request_id).collect();
    assert_eq!(candidate_ids, vec![ids[0], ids[1]]);

    assert!(repository.close_request(ids[0]).await.unwrap());
    assert!(!repository.close_request(ids[0]).await.unwrap());
    let candidates = repository
        .expired_request_candidates(created_at + 1, created_at + 1, 10)
        .await
        .unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].request_id, ids[1]);
    assert_eq!(
        repository.paid_unpublished_request_ids().await.unwrap(),
        vec![ids[2]]
    );
    // A paid request is not closed by the expiry rule.
    assert!(
        !candidates
            .iter()
            .any(|candidate| candidate.request_id == ids[2])
    );
}

#[tokio::test]
async fn clears_a_stored_transaction_until_completion() {
    let (_directory, database) = database().await;
    let repository = Repository::new(database);
    let created = repository
        .create_invoice_request(&request(b"stored"), &invoice())
        .await
        .unwrap();
    let request_id = created.request.id;

    repository
        .store_signed_transaction(request_id, "0200")
        .await
        .unwrap();
    let record = repository.find_record(request_id).await.unwrap();
    assert_eq!(record.request.transaction.as_deref(), Some("0200"));
    // The chain fee and size are written at completion only.
    assert_eq!(record.request.chain_fee_sats, None);
    assert_eq!(record.request.vsize, None);
    assert_eq!(
        repository
            .accounting_report()
            .await
            .unwrap()
            .chain_fees_sats,
        0
    );

    repository
        .clear_signed_transaction(request_id)
        .await
        .unwrap();
    let record = repository.find_record(request_id).await.unwrap();
    assert_eq!(record.request.transaction, None);

    repository
        .store_signed_transaction(request_id, "0200")
        .await
        .unwrap();
    repository
        .complete_request(
            request_id,
            &CompletedRequest {
                txid: &"ef".repeat(32),
                chain_fee_sats: 1_234,
                vsize: 126,
                profit_sats: Some(1),
                btc_price_cents: 0,
            },
        )
        .await
        .unwrap();
    repository
        .clear_signed_transaction(request_id)
        .await
        .unwrap();
    let record = repository.find_record(request_id).await.unwrap();
    assert_eq!(record.request.transaction.as_deref(), Some("0200"));
    assert_eq!(record.request.chain_fee_sats, Some(1_234));
    assert_eq!(record.request.vsize, Some(126));
    assert!(record.request.closed);
    assert_eq!(
        repository
            .accounting_report()
            .await
            .unwrap()
            .chain_fees_sats,
        1_234
    );
}

#[tokio::test]
async fn never_closes_a_paid_request() {
    let (_directory, database) = database().await;
    let repository = Repository::new(database);
    let lightning = repository
        .create_invoice_request(&request(b"lightning"), &invoice())
        .await
        .unwrap();
    assert!(
        repository
            .mark_invoice_paid(invoice().payment_hash)
            .await
            .unwrap()
    );
    assert!(
        !repository
            .close_request(lightning.request.id)
            .await
            .unwrap()
    );

    let second = NewInvoice {
        payment_hash: "cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd",
        bolt11: "lnbcrt1second",
        ..invoice()
    };
    let unified = repository
        .create_unified_request(&request(b"unified"), &second, &on_chain("bcrt1qpaid"), None)
        .await
        .unwrap();
    assert!(
        repository
            .mark_on_chain_paid("bcrt1qpaid", 5_000, &"ab".repeat(32))
            .await
            .unwrap()
    );
    assert!(!repository.close_request(unified.request.id).await.unwrap());
    let record = repository.find_record(unified.request.id).await.unwrap();
    assert!(!record.request.closed);
}

#[tokio::test]
async fn rejects_a_duplicate_nip5_name_as_an_invalid_request() {
    let (_directory, database) = database().await;
    let repository = Repository::new(database.clone());
    let nip5 = NewNip5 {
        name: "alice",
        public_key: "key",
    };
    repository
        .create_unified_request(
            &request(b"first"),
            &invoice(),
            &on_chain("bcrt1qfirst"),
            Some(&nip5),
        )
        .await
        .unwrap();

    let second = NewInvoice {
        payment_hash: "cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd",
        bolt11: "lnbcrt1second",
        ..invoice()
    };
    let duplicate = NewNip5 {
        name: "Alice",
        public_key: "other",
    };
    let error = repository
        .create_unified_request(
            &request(b"second"),
            &second,
            &on_chain("bcrt1qsecond"),
            Some(&duplicate),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(error, op_return_bot::AppError::InvalidRequest(_)),
        "{error}"
    );

    // The whole request was rolled back.
    let requests: i64 = sqlx::query_scalar("SELECT count(*) FROM op_return_requests")
        .fetch_one(database.pool())
        .await
        .unwrap();
    assert_eq!(requests, 1);
}

#[tokio::test]
async fn finds_unpublished_zaps_by_payment_hash() {
    let (_directory, database) = database().await;
    let repository = Repository::new(database);
    let payment_hash = "ab".repeat(32);
    repository
        .create_zap(&NewZap {
            payment_hash: &payment_hash,
            bolt11: "lnbcrt1zap",
            recipient_key: "recipient",
            amount_msats: 21_000,
            request_json: "{}",
            backend: LightningBackend::Lnd,
            created_at: 1_788_000_000,
        })
        .await
        .unwrap();
    let zap = repository
        .find_unpublished_zap(&payment_hash)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(zap.bolt11, "lnbcrt1zap");
    assert_eq!(zap.amount_msats, 21_000);

    repository
        .mark_zap_published(&payment_hash, &"ef".repeat(32))
        .await
        .unwrap();
    assert!(
        repository
            .find_unpublished_zap(&payment_hash)
            .await
            .unwrap()
            .is_none()
    );
}
