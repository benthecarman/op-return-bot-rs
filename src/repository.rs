use sqlx::{FromRow, Sqlite, Transaction};

use crate::{
    AppError, AppResult, Database,
    domain::{
        Invoice, LightningBackend, OnChainPayment, OpReturnRequest, decode_legacy_bytes,
        decode_legacy_fee_rate, encode_legacy_bytes, encode_legacy_fee_rate,
    },
};

#[derive(Clone)]
pub struct Repository {
    database: Database,
}

#[derive(Clone, Debug)]
pub struct NewRequest<'a> {
    pub message: &'a [u8],
    pub no_twitter: bool,
    pub fee_rate_sat_vb: u64,
    pub node_id: Option<&'a str>,
    pub telegram_id: Option<i64>,
    pub nostr_key: Option<&'a str>,
    pub created_at: i64,
}

#[derive(Clone, Debug)]
pub struct NewInvoice<'a> {
    pub payment_hash: &'a str,
    pub bolt11: &'a str,
    pub backend: LightningBackend,
    pub amount_sats: i64,
    pub claim_preimage: Option<&'a [u8]>,
}

#[derive(Clone, Debug)]
pub struct NewOnChainPayment<'a> {
    pub address: &'a str,
    pub expected_amount_sats: i64,
}

#[derive(Clone, Debug)]
pub struct NewNip5<'a> {
    pub name: &'a str,
    pub public_key: &'a str,
}

#[derive(Clone, Debug)]
pub struct NewZap<'a> {
    pub payment_hash: &'a str,
    pub bolt11: &'a str,
    pub recipient_key: &'a str,
    pub amount_msats: i64,
    pub request_json: &'a str,
    pub backend: LightningBackend,
    pub created_at: i64,
}

#[derive(Clone, Debug)]
pub struct PaymentRecord {
    pub request: OpReturnRequest,
    pub invoice: Option<Invoice>,
    pub on_chain: Option<OnChainPayment>,
}

#[derive(Clone, Debug)]
pub struct ZapRecord {
    pub payment_hash: String,
    pub bolt11: String,
    pub amount_msats: i64,
    pub request_json: String,
    pub backend: LightningBackend,
}

/// An unpaid Lightning invoice that the reconciler still checks.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpenInvoice {
    pub request_id: i64,
    pub payment_hash: String,
}

/// An unpaid request that is old enough to close.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExpiredCandidate {
    pub request_id: i64,
    pub payment_hash: Option<String>,
    pub backend: Option<LightningBackend>,
}

/// The values written when an `OP_RETURN` transaction is published.
#[derive(Clone, Debug)]
pub struct CompletedRequest<'a> {
    pub txid: &'a str,
    pub chain_fee_sats: i64,
    pub vsize: i64,
    pub profit_sats: Option<i64>,
    pub btc_price_cents: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AccountingReport {
    pub completed_requests: i64,
    pub non_standard_requests: i64,
    pub on_chain_requests: i64,
    pub pending_requests: i64,
    pub profit_sats: i64,
    pub chain_fees_sats: i64,
    pub chain_vbytes: i64,
    pub non_standard_vbytes: i64,
    pub completed_nip5s: i64,
    pub zapped_sats: i64,
}

#[derive(FromRow)]
struct RequestRow {
    id: i64,
    message_bytes: Vec<u8>,
    no_twitter: bool,
    fee_rate: String,
    node_id: Option<String>,
    telegram_id: Option<i64>,
    nostr_key: Option<String>,
    time: i64,
    transaction: Option<String>,
    txid: Option<String>,
    profit: Option<i64>,
    chain_fee: Option<i64>,
    vsize: Option<i64>,
    closed: bool,
    btc_price: i64,
}

impl TryFrom<RequestRow> for OpReturnRequest {
    type Error = AppError;

    fn try_from(row: RequestRow) -> AppResult<Self> {
        Ok(Self {
            id: row.id,
            message: decode_legacy_bytes(&row.message_bytes)?,
            no_twitter: row.no_twitter,
            fee_rate_sat_vb: decode_legacy_fee_rate(&row.fee_rate)?,
            node_id: row.node_id,
            telegram_id: row.telegram_id,
            nostr_key: row.nostr_key,
            created_at: row.time,
            transaction: row.transaction,
            txid: row.txid,
            profit_sats: row.profit,
            chain_fee_sats: row.chain_fee,
            vsize: row.vsize,
            closed: row.closed,
            btc_price_cents: row.btc_price,
        })
    }
}

#[derive(FromRow)]
struct InvoiceRow {
    r_hash: String,
    op_return_request_id: i64,
    invoice: String,
    paid: bool,
    amount_sats: Option<i64>,
    lightning_backend: String,
    claim_preimage: Option<Vec<u8>>,
}

impl TryFrom<InvoiceRow> for Invoice {
    type Error = AppError;

    fn try_from(row: InvoiceRow) -> AppResult<Self> {
        Ok(Self {
            payment_hash: row.r_hash,
            request_id: row.op_return_request_id,
            bolt11: row.invoice,
            paid: row.paid,
            amount_sats: row.amount_sats,
            lightning_backend: LightningBackend::try_from(row.lightning_backend.as_str())?,
            claim_preimage: row.claim_preimage,
        })
    }
}

#[derive(FromRow)]
struct OnChainRow {
    address: String,
    op_return_request_id: i64,
    expected_amount: i64,
    amount_paid: Option<i64>,
    txid: Option<String>,
}

impl From<OnChainRow> for OnChainPayment {
    fn from(row: OnChainRow) -> Self {
        Self {
            address: row.address,
            request_id: row.op_return_request_id,
            expected_amount_sats: row.expected_amount,
            amount_paid_sats: row.amount_paid,
            txid: row.txid,
        }
    }
}

impl Repository {
    #[must_use]
    pub const fn new(database: Database) -> Self {
        Self { database }
    }

    pub async fn create_invoice_request(
        &self,
        request: &NewRequest<'_>,
        invoice: &NewInvoice<'_>,
    ) -> AppResult<PaymentRecord> {
        let mut transaction = self.database.pool().begin().await?;
        let created = insert_request(&mut transaction, request).await?;
        insert_invoice(&mut transaction, created.id, invoice).await?;
        transaction.commit().await?;
        let request_id = created.id;
        Ok(PaymentRecord {
            request: created,
            invoice: Some(Invoice {
                payment_hash: invoice.payment_hash.to_owned(),
                request_id,
                bolt11: invoice.bolt11.to_owned(),
                paid: false,
                amount_sats: Some(invoice.amount_sats),
                lightning_backend: invoice.backend,
                claim_preimage: invoice.claim_preimage.map(ToOwned::to_owned),
            }),
            on_chain: None,
        })
    }

    pub async fn create_unified_request(
        &self,
        request: &NewRequest<'_>,
        invoice: &NewInvoice<'_>,
        on_chain: &NewOnChainPayment<'_>,
        nip5: Option<&NewNip5<'_>>,
    ) -> AppResult<PaymentRecord> {
        let mut transaction = self.database.pool().begin().await?;
        if let Some(nip5) = nip5 {
            // The check runs in the same transaction as the insert, as in
            // the Scala service. The unique index is the last guard.
            let exists: bool = sqlx::query_scalar(NIP5_NAME_EXISTS_SQL)
                .bind(nip5.name)
                .fetch_one(&mut *transaction)
                .await?;
            if exists {
                return Err(nip5_name_taken(nip5.name));
            }
        }
        let created = insert_request(&mut transaction, request).await?;
        insert_invoice(&mut transaction, created.id, invoice).await?;
        sqlx::query(
            "INSERT INTO on_chain_payments \
             (address, op_return_request_id, expected_amount, amount_paid, txid) \
             VALUES (?, ?, ?, NULL, NULL)",
        )
        .bind(on_chain.address)
        .bind(created.id)
        .bind(on_chain.expected_amount_sats)
        .execute(&mut *transaction)
        .await?;
        if let Some(nip5) = nip5 {
            sqlx::query(
                "INSERT INTO nip5 (op_return_request_id, name, public_key) VALUES (?, ?, ?)",
            )
            .bind(created.id)
            .bind(nip5.name)
            .bind(nip5.public_key)
            .execute(&mut *transaction)
            .await
            .map_err(|error| {
                if is_unique_violation(&error) {
                    nip5_name_taken(nip5.name)
                } else {
                    error.into()
                }
            })?;
        }
        transaction.commit().await?;

        Ok(PaymentRecord {
            request: created.clone(),
            invoice: Some(Invoice {
                payment_hash: invoice.payment_hash.to_owned(),
                request_id: created.id,
                bolt11: invoice.bolt11.to_owned(),
                paid: false,
                amount_sats: Some(invoice.amount_sats),
                lightning_backend: invoice.backend,
                claim_preimage: invoice.claim_preimage.map(ToOwned::to_owned),
            }),
            on_chain: Some(OnChainPayment {
                address: on_chain.address.to_owned(),
                request_id: created.id,
                expected_amount_sats: on_chain.expected_amount_sats,
                amount_paid_sats: None,
                txid: None,
            }),
        })
    }

    pub async fn find_by_payment_hash(&self, payment_hash: &str) -> AppResult<PaymentRecord> {
        self.find_by_invoice_identifier(payment_hash).await
    }

    /// Finds a payment by its payment hash or its BOLT11 invoice. Both are
    /// case-insensitive, so the uppercase invoice of a BIP21 string works.
    pub async fn find_by_invoice_identifier(&self, identifier: &str) -> AppResult<PaymentRecord> {
        let identifier = identifier.trim().to_ascii_lowercase();
        let invoice_row = sqlx::query_as::<_, InvoiceRow>(
            "SELECT r_hash, op_return_request_id, invoice, paid, amount_sats, lightning_backend, \
             claim_preimage FROM invoices WHERE r_hash = ? OR invoice = ?",
        )
        .bind(identifier.as_str())
        .bind(identifier.as_str())
        .fetch_optional(self.database.pool())
        .await?
        .ok_or_else(|| AppError::NotFound("invoice was not found".to_owned()))?;
        let request = self.find_request(invoice_row.op_return_request_id).await?;
        let on_chain = self.find_on_chain(request.id).await?;
        Ok(PaymentRecord {
            request,
            invoice: Some(invoice_row.try_into()?),
            on_chain,
        })
    }

    pub async fn find_by_txid(&self, txid: &str) -> AppResult<OpReturnRequest> {
        let row = sqlx::query_as::<_, RequestRow>(
            "SELECT id, message_bytes, no_twitter, fee_rate, node_id, telegram_id, \
             nostr_key, time, \"transaction\", txid, profit, chain_fee, vsize, closed, \
             btc_price FROM op_return_requests WHERE txid = ?",
        )
        .bind(txid.trim().to_ascii_lowercase())
        .fetch_optional(self.database.pool())
        .await?
        .ok_or_else(|| AppError::NotFound("transaction was not found".to_owned()))?;
        row.try_into()
    }

    /// The public key that a NIP-05 request registers, if the request is one.
    pub async fn nip5_public_key(&self, request_id: i64) -> AppResult<Option<String>> {
        Ok(
            sqlx::query_scalar("SELECT public_key FROM nip5 WHERE op_return_request_id = ?")
                .bind(request_id)
                .fetch_optional(self.database.pool())
                .await?,
        )
    }

    pub async fn recent_public_txids(&self, limit: u32) -> AppResult<Vec<String>> {
        let rows = sqlx::query_scalar::<_, String>(
            "SELECT txid FROM op_return_requests \
             WHERE txid IS NOT NULL AND no_twitter = 0 \
             ORDER BY id DESC LIMIT ?",
        )
        .bind(i64::from(limit))
        .fetch_all(self.database.pool())
        .await?;
        Ok(rows)
    }

    pub async fn mark_invoice_paid(&self, payment_hash: &str) -> AppResult<bool> {
        let result = sqlx::query("UPDATE invoices SET paid = 1 WHERE r_hash = ? AND paid = 0")
            .bind(payment_hash)
            .execute(self.database.pool())
            .await?;
        Ok(result.rows_affected() == 1)
    }

    pub async fn mark_on_chain_paid(
        &self,
        address: &str,
        amount_sats: i64,
        txid: &str,
    ) -> AppResult<bool> {
        let result = sqlx::query(
            "UPDATE on_chain_payments SET amount_paid = ?, txid = ? \
             WHERE address = ? AND txid IS NULL AND ? >= expected_amount",
        )
        .bind(amount_sats)
        .bind(txid)
        .bind(address)
        .bind(amount_sats)
        .execute(self.database.pool())
        .await?;
        Ok(result.rows_affected() == 1)
    }

    pub async fn find_by_address(&self, address: &str) -> AppResult<PaymentRecord> {
        let on_chain_row = sqlx::query_as::<_, OnChainRow>(
            "SELECT address, op_return_request_id, expected_amount, amount_paid, txid \
             FROM on_chain_payments WHERE address = ?",
        )
        .bind(address)
        .fetch_optional(self.database.pool())
        .await?
        .ok_or_else(|| AppError::NotFound("on-chain payment was not found".to_owned()))?;
        let request = self.find_request(on_chain_row.op_return_request_id).await?;
        let invoice = self.find_invoice(request.id).await?;
        Ok(PaymentRecord {
            request,
            invoice,
            on_chain: Some(on_chain_row.into()),
        })
    }

    /// Stores a signed transaction before broadcast so that a crash cannot
    /// lose it. The chain fee and size are written at completion, as in the
    /// Scala service.
    pub async fn store_signed_transaction(
        &self,
        request_id: i64,
        transaction_hex: &str,
    ) -> AppResult<()> {
        sqlx::query(
            "UPDATE op_return_requests SET \"transaction\" = ? WHERE id = ? AND txid IS NULL",
        )
        .bind(transaction_hex)
        .bind(request_id)
        .execute(self.database.pool())
        .await?;
        Ok(())
    }

    pub async fn complete_request(
        &self,
        request_id: i64,
        completed: &CompletedRequest<'_>,
    ) -> AppResult<()> {
        sqlx::query(
            "UPDATE op_return_requests SET txid = ?, chain_fee = ?, vsize = ?, profit = ?, \
             btc_price = ?, closed = 1 WHERE id = ? AND txid IS NULL",
        )
        .bind(completed.txid)
        .bind(completed.chain_fee_sats)
        .bind(completed.vsize)
        .bind(completed.profit_sats)
        .bind(completed.btc_price_cents)
        .bind(request_id)
        .execute(self.database.pool())
        .await?;
        Ok(())
    }

    /// Loads a request with its invoice and on-chain payment.
    pub async fn find_record(&self, request_id: i64) -> AppResult<PaymentRecord> {
        let request = self.find_request(request_id).await?;
        let invoice = self.find_invoice(request_id).await?;
        let on_chain = self.find_on_chain(request_id).await?;
        Ok(PaymentRecord {
            request,
            invoice,
            on_chain,
        })
    }

    /// Unpaid on-chain payment requests, newest first.
    pub async fn open_on_chain_payments(
        &self,
        created_after: i64,
    ) -> AppResult<Vec<OnChainPayment>> {
        let rows = sqlx::query_as::<_, OnChainRow>(
            "SELECT p.address, p.op_return_request_id, p.expected_amount, p.amount_paid, p.txid \
             FROM on_chain_payments p JOIN op_return_requests r \
             ON r.id = p.op_return_request_id \
             WHERE r.closed = 0 AND r.txid IS NULL AND p.txid IS NULL AND r.time >= ? \
             ORDER BY r.id DESC",
        )
        .bind(created_after)
        .fetch_all(self.database.pool())
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// Requests that were paid but have no transaction yet.
    pub async fn paid_unpublished_request_ids(&self) -> AppResult<Vec<i64>> {
        Ok(sqlx::query_scalar(
            "SELECT r.id FROM op_return_requests r \
             LEFT JOIN invoices i ON i.op_return_request_id = r.id \
             LEFT JOIN on_chain_payments p ON p.op_return_request_id = r.id \
             WHERE r.closed = 0 AND r.txid IS NULL \
             AND (i.paid = 1 OR p.txid IS NOT NULL) ORDER BY r.id",
        )
        .fetch_all(self.database.pool())
        .await?)
    }

    /// Unpaid requests that can be closed. Lightning-only requests qualify
    /// after `lightning_before`; requests with an on-chain address qualify
    /// after `on_chain_before`.
    pub async fn expired_request_candidates(
        &self,
        lightning_before: i64,
        on_chain_before: i64,
        limit: u32,
    ) -> AppResult<Vec<ExpiredCandidate>> {
        let rows: Vec<(i64, Option<String>, Option<String>)> = sqlx::query_as(
            "SELECT r.id, i.r_hash, i.lightning_backend FROM op_return_requests r \
             LEFT JOIN invoices i ON i.op_return_request_id = r.id \
             LEFT JOIN on_chain_payments p ON p.op_return_request_id = r.id \
             WHERE r.closed = 0 AND r.txid IS NULL \
             AND coalesce(i.paid, 0) = 0 AND p.txid IS NULL \
             AND ((p.address IS NULL AND r.time < ?) OR r.time < ?) \
             ORDER BY r.id LIMIT ?",
        )
        .bind(lightning_before)
        .bind(on_chain_before)
        .bind(i64::from(limit))
        .fetch_all(self.database.pool())
        .await?;
        rows.into_iter()
            .map(|(request_id, payment_hash, backend)| {
                Ok(ExpiredCandidate {
                    request_id,
                    payment_hash,
                    backend: backend
                        .as_deref()
                        .map(LightningBackend::try_from)
                        .transpose()?,
                })
            })
            .collect()
    }

    /// Closes an unpaid request. Returns false when the request was already
    /// closed, completed, or paid in the meantime. An unpaid NIP-05 name is
    /// released so that a later buyer can reserve it.
    pub async fn close_request(&self, request_id: i64) -> AppResult<bool> {
        let mut transaction = self.database.pool().begin().await?;
        let result = sqlx::query(
            "UPDATE op_return_requests SET closed = 1 \
             WHERE id = ? AND closed = 0 AND txid IS NULL \
             AND NOT EXISTS (SELECT 1 FROM invoices i \
                 WHERE i.op_return_request_id = op_return_requests.id AND i.paid = 1) \
             AND NOT EXISTS (SELECT 1 FROM on_chain_payments p \
                 WHERE p.op_return_request_id = op_return_requests.id AND p.txid IS NOT NULL)",
        )
        .bind(request_id)
        .execute(&mut *transaction)
        .await?;
        if result.rows_affected() != 1 {
            return Ok(false);
        }
        sqlx::query("DELETE FROM nip5 WHERE op_return_request_id = ?")
            .bind(request_id)
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        Ok(true)
    }

    /// Closes a legacy request whose transaction was already completed.
    pub async fn close_completed_request(&self, request_id: i64) -> AppResult<bool> {
        let result = sqlx::query(
            "UPDATE op_return_requests SET closed = 1 \
             WHERE id = ? AND closed = 0 AND txid IS NOT NULL",
        )
        .bind(request_id)
        .execute(self.database.pool())
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Oldest open payment records, with an optional command limit.
    pub async fn unclosed_payment_records(
        &self,
        limit: Option<u32>,
    ) -> AppResult<Vec<PaymentRecord>> {
        let ids: Vec<i64> = sqlx::query_scalar(
            "SELECT r.id FROM op_return_requests r \
             WHERE r.closed = 0 \
             AND (EXISTS (SELECT 1 FROM invoices i \
                    WHERE i.op_return_request_id = r.id) \
                  OR EXISTS (SELECT 1 FROM on_chain_payments p \
                    WHERE p.op_return_request_id = r.id)) \
             ORDER BY r.time LIMIT ?",
        )
        .bind(limit.map_or(i64::MAX, i64::from))
        .fetch_all(self.database.pool())
        .await?;
        let mut records = Vec::with_capacity(ids.len());
        for id in ids {
            records.push(self.find_record(id).await?);
        }
        Ok(records)
    }

    /// Removes a stored signed transaction so that the next publish attempt
    /// builds a new one.
    pub async fn clear_signed_transaction(&self, request_id: i64) -> AppResult<()> {
        sqlx::query(
            "UPDATE op_return_requests SET \"transaction\" = NULL, chain_fee = NULL, vsize = NULL \
             WHERE id = ? AND txid IS NULL",
        )
        .bind(request_id)
        .execute(self.database.pool())
        .await?;
        Ok(())
    }

    pub async fn nip5_name_exists(&self, name: &str) -> AppResult<bool> {
        let exists: bool = sqlx::query_scalar(NIP5_NAME_EXISTS_SQL)
            .bind(name)
            .fetch_one(self.database.pool())
            .await?;
        Ok(exists)
    }

    pub async fn completed_nip5_public_key(&self, name: &str) -> AppResult<Option<String>> {
        Ok(sqlx::query_scalar(
            "SELECT n.public_key FROM nip5 n JOIN op_return_requests r \
             ON r.id = n.op_return_request_id \
             WHERE lower(n.name) = lower(?) AND r.txid IS NOT NULL LIMIT 1",
        )
        .bind(name)
        .fetch_optional(self.database.pool())
        .await?)
    }

    pub async fn create_zap(&self, zap: &NewZap<'_>) -> AppResult<()> {
        sqlx::query(
            "INSERT INTO zaps \
             (r_hash, invoice, my_key, amount, request, note_id, time, lightning_backend) \
             VALUES (?, ?, ?, ?, ?, NULL, ?, ?)",
        )
        .bind(zap.payment_hash)
        .bind(zap.bolt11)
        .bind(zap.recipient_key)
        .bind(zap.amount_msats)
        .bind(zap.request_json)
        .bind(zap.created_at)
        .bind(zap.backend.as_str())
        .execute(self.database.pool())
        .await?;
        Ok(())
    }

    pub async fn unpublished_zaps(
        &self,
        backend: LightningBackend,
        created_after: i64,
        limit: u32,
    ) -> AppResult<Vec<ZapRecord>> {
        let rows: Vec<(String, String, i64, String, String)> = sqlx::query_as(
            "SELECT r_hash, invoice, amount, request, lightning_backend FROM zaps \
             WHERE note_id IS NULL AND time >= ? AND lightning_backend = ? \
             ORDER BY time LIMIT ?",
        )
        .bind(created_after)
        .bind(backend.as_str())
        .bind(i64::from(limit))
        .fetch_all(self.database.pool())
        .await?;
        rows.into_iter().map(zap_record).collect()
    }

    /// A zap without a receipt, by payment hash.
    pub async fn find_unpublished_zap(&self, payment_hash: &str) -> AppResult<Option<ZapRecord>> {
        let row: Option<(String, String, i64, String, String)> = sqlx::query_as(
            "SELECT r_hash, invoice, amount, request, lightning_backend FROM zaps \
             WHERE r_hash = ? AND note_id IS NULL",
        )
        .bind(payment_hash)
        .fetch_optional(self.database.pool())
        .await?;
        row.map(zap_record).transpose()
    }

    pub async fn service_state(&self, key: &str) -> AppResult<Option<String>> {
        Ok(
            sqlx::query_scalar("SELECT value FROM service_state WHERE key = ?")
                .bind(key)
                .fetch_optional(self.database.pool())
                .await?,
        )
    }

    pub async fn set_service_state(&self, key: &str, value: &str) -> AppResult<()> {
        sqlx::query(
            "INSERT INTO service_state (key, value) VALUES (?, ?) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        )
        .bind(key)
        .bind(value)
        .execute(self.database.pool())
        .await?;
        Ok(())
    }

    /// All-time totals for the Telegram report.
    pub async fn accounting_report(&self) -> AppResult<AccountingReport> {
        self.accounting_report_since(None).await
    }

    /// Telegram report totals after `created_after`. The queue is always an
    /// all-time current value, as it was in the Scala service.
    pub async fn accounting_report_since(
        &self,
        created_after: Option<i64>,
    ) -> AppResult<AccountingReport> {
        let (
            completed_requests,
            non_standard_requests,
            on_chain_requests,
            chain_fees_sats,
            profit_sats,
            chain_vbytes,
            non_standard_vbytes,
            completed_nip5s,
            zapped_msats,
            pending_requests,
        ): (i64, i64, i64, i64, i64, i64, i64, i64, i64, i64) = sqlx::query_as(
            "WITH filtered AS (\
                 SELECT * FROM op_return_requests WHERE ? IS NULL OR time > ?\
             ) SELECT \
             (SELECT count(*) FROM filtered WHERE txid IS NOT NULL), \
             (SELECT count(*) FROM filtered WHERE txid IS NOT NULL AND \
                CASE WHEN typeof(message_bytes) = 'text' \
                     THEN length(message_bytes) / 2 ELSE length(message_bytes) END > 80), \
             (SELECT count(*) FROM filtered r JOIN on_chain_payments p \
                ON p.op_return_request_id = r.id \
                WHERE r.txid IS NOT NULL AND p.txid IS NOT NULL), \
             (SELECT coalesce(sum(chain_fee), 0) FROM filtered), \
             (SELECT coalesce(sum(profit), 0) FROM filtered), \
             (SELECT coalesce(sum(vsize), 0) FROM filtered), \
             (SELECT coalesce(sum(vsize), 0) FROM filtered WHERE \
                CASE WHEN typeof(message_bytes) = 'text' \
                     THEN length(message_bytes) / 2 ELSE length(message_bytes) END > 80), \
             (SELECT count(*) FROM nip5 n JOIN filtered r \
                ON r.id = n.op_return_request_id WHERE r.txid IS NOT NULL), \
             (SELECT coalesce(sum(amount), 0) FROM zaps \
                WHERE note_id IS NOT NULL AND (? IS NULL OR time > ?)), \
             (SELECT count(*) FROM op_return_requests r \
                LEFT JOIN invoices i ON i.op_return_request_id = r.id \
                LEFT JOIN on_chain_payments p ON p.op_return_request_id = r.id \
                WHERE r.closed = 0 AND r.txid IS NULL \
                AND (i.paid = 1 OR p.txid IS NOT NULL))",
        )
        .bind(created_after)
        .bind(created_after)
        .bind(created_after)
        .bind(created_after)
        .fetch_one(self.database.pool())
        .await?;
        Ok(AccountingReport {
            completed_requests,
            non_standard_requests,
            on_chain_requests,
            pending_requests,
            profit_sats,
            chain_fees_sats,
            chain_vbytes,
            non_standard_vbytes,
            completed_nip5s,
            zapped_sats: zapped_msats / 1_000,
        })
    }

    pub async fn mark_zap_published(&self, payment_hash: &str, note_id: &str) -> AppResult<()> {
        sqlx::query("UPDATE zaps SET note_id = ? WHERE r_hash = ? AND note_id IS NULL")
            .bind(note_id)
            .bind(payment_hash)
            .execute(self.database.pool())
            .await?;
        Ok(())
    }

    async fn find_request(&self, id: i64) -> AppResult<OpReturnRequest> {
        let row = sqlx::query_as::<_, RequestRow>(
            "SELECT id, message_bytes, no_twitter, fee_rate, node_id, telegram_id, \
             nostr_key, time, \"transaction\", txid, profit, chain_fee, vsize, closed, \
             btc_price FROM op_return_requests WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(self.database.pool())
        .await?
        .ok_or_else(|| AppError::NotFound(format!("request {id} was not found")))?;
        row.try_into()
    }

    async fn find_on_chain(&self, request_id: i64) -> AppResult<Option<OnChainPayment>> {
        let row = sqlx::query_as::<_, OnChainRow>(
            "SELECT address, op_return_request_id, expected_amount, amount_paid, txid \
             FROM on_chain_payments WHERE op_return_request_id = ?",
        )
        .bind(request_id)
        .fetch_optional(self.database.pool())
        .await?;
        Ok(row.map(Into::into))
    }

    async fn find_invoice(&self, request_id: i64) -> AppResult<Option<Invoice>> {
        let row = sqlx::query_as::<_, InvoiceRow>(
            "SELECT r_hash, op_return_request_id, invoice, paid, amount_sats, \
             lightning_backend, claim_preimage FROM invoices \
             WHERE op_return_request_id = ?",
        )
        .bind(request_id)
        .fetch_optional(self.database.pool())
        .await?;
        row.map(TryInto::try_into).transpose()
    }
}

const NIP5_NAME_EXISTS_SQL: &str = "SELECT EXISTS(\
     SELECT 1 FROM nip5 n JOIN op_return_requests r \
     ON r.id = n.op_return_request_id \
     WHERE lower(n.name) = lower(?) \
     AND (r.closed = 0 OR r.txid IS NOT NULL))";

fn nip5_name_taken(name: &str) -> AppError {
    AppError::InvalidRequest(format!("NIP-05 name '{name}' is already reserved"))
}

fn is_unique_violation(error: &sqlx::Error) -> bool {
    matches!(error, sqlx::Error::Database(database) if database.is_unique_violation())
}

fn zap_record(
    (payment_hash, bolt11, amount_msats, request_json, backend): (
        String,
        String,
        i64,
        String,
        String,
    ),
) -> AppResult<ZapRecord> {
    Ok(ZapRecord {
        payment_hash,
        bolt11,
        amount_msats,
        request_json,
        backend: LightningBackend::try_from(backend.as_str())?,
    })
}

async fn insert_request(
    transaction: &mut Transaction<'_, Sqlite>,
    request: &NewRequest<'_>,
) -> AppResult<OpReturnRequest> {
    let message = encode_legacy_bytes(request.message);
    let fee_rate = encode_legacy_fee_rate(request.fee_rate_sat_vb);
    let result = sqlx::query(
        "INSERT INTO op_return_requests \
         (message_bytes, no_twitter, fee_rate, node_id, telegram_id, nostr_key, \
          dvm_event, time, \"transaction\", txid, profit, chain_fee, vsize, closed, btc_price) \
         VALUES (?, ?, ?, ?, ?, ?, NULL, ?, NULL, NULL, NULL, NULL, NULL, 0, 0)",
    )
    .bind(message)
    .bind(request.no_twitter)
    .bind(fee_rate)
    .bind(request.node_id)
    .bind(request.telegram_id)
    .bind(request.nostr_key)
    .bind(request.created_at)
    .execute(&mut **transaction)
    .await?;

    Ok(OpReturnRequest {
        id: result.last_insert_rowid(),
        message: request.message.to_vec(),
        no_twitter: request.no_twitter,
        fee_rate_sat_vb: request.fee_rate_sat_vb,
        node_id: request.node_id.map(ToOwned::to_owned),
        telegram_id: request.telegram_id,
        nostr_key: request.nostr_key.map(ToOwned::to_owned),
        created_at: request.created_at,
        transaction: None,
        txid: None,
        profit_sats: None,
        chain_fee_sats: None,
        vsize: None,
        closed: false,
        btc_price_cents: 0,
    })
}

async fn insert_invoice(
    transaction: &mut Transaction<'_, Sqlite>,
    request_id: i64,
    invoice: &NewInvoice<'_>,
) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO invoices \
         (r_hash, op_return_request_id, invoice, paid, amount_sats, lightning_backend, claim_preimage) \
         VALUES (?, ?, ?, 0, ?, ?, ?)",
    )
    .bind(invoice.payment_hash)
    .bind(request_id)
    .bind(invoice.bolt11)
    .bind(invoice.amount_sats)
    .bind(invoice.backend.as_str())
    .bind(invoice.claim_preimage)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}
