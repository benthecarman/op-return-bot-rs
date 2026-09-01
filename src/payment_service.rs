use std::{
    collections::HashSet,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime},
};

use bitcoin::{OutPoint, Transaction, Txid, consensus};
use futures_util::StreamExt;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use crate::{
    AppConfig, AppError, AppResult,
    bitcoin_rpc::{
        BitcoinClient, Funding, ReceivedPayment, Wallet, is_missing_inputs_error,
        is_policy_rejection,
    },
    lightning::{CreatedInvoice, InvoiceEvent, InvoiceState, InvoiceStream, Lightning},
    pricing::{PriceQuote, STANDARD_OP_RETURN_BYTES, quote},
    rate_limit::RateLimiter,
    repository::{
        CompletedRequest, ExpiredCandidate, NewInvoice, NewNip5, NewOnChainPayment, NewRequest,
        NewZap, OpenInvoice, PaymentRecord, Repository,
    },
    social::SocialPublisher,
};

/// Time after invoice expiry before an unpaid Lightning-only request closes.
const INVOICE_CLOSE_GRACE_SECONDS: i64 = 3_600;
/// Upper bound on expired requests handled in one reconciliation pass.
const EXPIRED_REQUESTS_PER_PASS: u32 = 200;
const ZAP_WINDOW_SECONDS: i64 = 86_400;
const ZAPS_PER_PASS: u32 = 256;
/// Wait between attempts to subscribe to Lightning invoice updates.
const SUBSCRIPTION_RETRY: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub struct PaymentService {
    config: Arc<AppConfig>,
    repository: Repository,
    bitcoin: BitcoinClient,
    lightning: Arc<dyn Lightning>,
    http: reqwest::Client,
    processing: Arc<Mutex<HashSet<i64>>>,
    publishing_zaps: Arc<Mutex<HashSet<String>>>,
    publishing: Arc<Mutex<()>>,
    mempool_limit: Arc<AtomicBool>,
    last_block_height: Arc<Mutex<Option<u64>>>,
    social: SocialPublisher,
    creates: Arc<RateLimiter>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CreateRequest {
    pub message: Vec<u8>,
    pub no_twitter: bool,
}

#[derive(Clone, Debug)]
pub struct CreatedPayment {
    pub record: PaymentRecord,
    pub quote: PriceQuote,
}

struct PreparedTransaction {
    transaction: Transaction,
    fee_sats: u64,
    wallet: Wallet,
    stored: bool,
}

#[derive(Deserialize)]
struct MempoolFees {
    #[serde(rename = "fastestFee")]
    fastest_fee: u64,
}

#[derive(Deserialize)]
struct CoinbasePrice {
    data: CoinbasePriceData,
}

#[derive(Deserialize)]
struct CoinbasePriceData {
    amount: String,
}

impl PaymentService {
    pub fn new(
        config: Arc<AppConfig>,
        repository: Repository,
        bitcoin: BitcoinClient,
        lightning: Arc<dyn Lightning>,
        social: SocialPublisher,
        creates: Arc<RateLimiter>,
    ) -> AppResult<Self> {
        Ok(Self {
            config,
            repository,
            bitcoin,
            lightning,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .build()
                .map_err(|error| AppError::Upstream(error.to_string()))?,
            processing: Arc::new(Mutex::new(HashSet::new())),
            publishing_zaps: Arc::new(Mutex::new(HashSet::new())),
            publishing: Arc::new(Mutex::new(())),
            mempool_limit: Arc::new(AtomicBool::new(false)),
            last_block_height: Arc::new(Mutex::new(None)),
            social,
            creates,
        })
    }

    /// Records one create attempt against the shared limiter.
    pub fn check_create_limit(&self, key: &str) -> AppResult<()> {
        self.creates.check(key)
    }

    #[must_use]
    pub fn creates(&self) -> Arc<RateLimiter> {
        self.creates.clone()
    }

    pub async fn create_invoice(&self, input: &CreateRequest) -> AppResult<CreatedPayment> {
        self.create_invoice_inner(input, None).await
    }

    pub async fn create_telegram_invoice(
        &self,
        input: &CreateRequest,
        telegram_id: i64,
    ) -> AppResult<CreatedPayment> {
        self.create_invoice_inner(input, Some(telegram_id)).await
    }

    async fn create_invoice_inner(
        &self,
        input: &CreateRequest,
        telegram_id: Option<i64>,
    ) -> AppResult<CreatedPayment> {
        let price = self.price(input).await?;
        let invoice = self
            .lightning
            .create_invoice_with_description_hash(
                msats(price.amount_sats)?,
                message_hash(&input.message),
                self.config.payments.invoice_expiry_seconds,
            )
            .await?;
        validate_invoice_network(&invoice.bolt11, self.config.bitcoin.network)?;
        let mut request = new_request(input, price.fee_rate_sat_vb)?;
        request.telegram_id = telegram_id;
        let invoice_row = NewInvoice {
            payment_hash: &invoice.payment_hash,
            bolt11: &invoice.bolt11,
            backend: invoice.backend,
            amount_sats: i64::try_from(price.amount_sats)
                .map_err(|_| AppError::InvalidRequest("payment amount is too large".to_owned()))?,
            claim_preimage: None,
        };
        let record = self
            .repository
            .create_invoice_request(&request, &invoice_row)
            .await?;
        Ok(CreatedPayment {
            record,
            quote: price,
        })
    }

    pub async fn create_unified(&self, input: &CreateRequest) -> AppResult<CreatedPayment> {
        self.create_unified_inner(input, None).await
    }

    async fn create_unified_inner(
        &self,
        input: &CreateRequest,
        nip5: Option<&NewNip5<'_>>,
    ) -> AppResult<CreatedPayment> {
        let price = self.price(input).await?;
        let (invoice, address) = tokio::try_join!(
            self.lightning.create_invoice_with_description_hash(
                msats(price.amount_sats)?,
                message_hash(&input.message),
                self.config.payments.invoice_expiry_seconds,
            ),
            self.bitcoin.new_receiving_address(),
        )?;
        validate_invoice_network(&invoice.bolt11, self.config.bitcoin.network)?;
        let expected_amount_sats = i64::try_from(price.amount_sats)
            .map_err(|_| AppError::InvalidRequest("payment amount is too large".to_owned()))?;
        let request = new_request(input, price.fee_rate_sat_vb)?;
        let invoice_row = NewInvoice {
            payment_hash: &invoice.payment_hash,
            bolt11: &invoice.bolt11,
            backend: invoice.backend,
            amount_sats: expected_amount_sats,
            claim_preimage: None,
        };
        let on_chain = NewOnChainPayment {
            address: &address,
            expected_amount_sats,
        };
        let record = self
            .repository
            .create_unified_request(&request, &invoice_row, &on_chain, nip5)
            .await?;
        Ok(CreatedPayment {
            record,
            quote: price,
        })
    }

    pub async fn create_nip5(&self, name: &str, public_key: &str) -> AppResult<CreatedPayment> {
        validate_nip5_name(name, &self.config.twitter.banned_words)?;
        if self.repository.nip5_name_exists(name).await? {
            return Err(AppError::InvalidRequest(format!(
                "NIP-05 name '{name}' is already reserved"
            )));
        }
        let public_key = normalize_nostr_public_key(public_key)?;
        let input = CreateRequest {
            message: format!("nip5:{name}:{public_key}").into_bytes(),
            no_twitter: false,
        };
        self.create_unified_inner(
            &input,
            Some(&NewNip5 {
                name,
                public_key: &public_key,
            }),
        )
        .await
    }

    /// Creates an invoice whose description hash commits to LNURL-pay
    /// metadata or to a zap request.
    pub async fn create_invoice_for_hash(
        &self,
        amount_msats: u64,
        description_hash: [u8; 32],
        expiry_seconds: u32,
    ) -> AppResult<CreatedInvoice> {
        let invoice = self
            .lightning
            .create_invoice_with_description_hash(amount_msats, description_hash, expiry_seconds)
            .await?;
        validate_invoice_network(&invoice.bolt11, self.config.bitcoin.network)?;
        Ok(invoice)
    }

    pub async fn create_zap_invoice(
        &self,
        amount_msats: u64,
        request_json: &str,
        expiry_seconds: u32,
    ) -> AppResult<CreatedInvoice> {
        let description_hash: [u8; 32] = Sha256::digest(request_json.as_bytes()).into();
        self.create_invoice_for_hash(amount_msats, description_hash, expiry_seconds)
            .await
    }

    pub async fn node_uri(&self) -> AppResult<String> {
        self.lightning.node_uri().await
    }

    pub async fn save_zap(
        &self,
        invoice: &CreatedInvoice,
        amount_msats: u64,
        request_json: &str,
        recipient_key: &str,
    ) -> AppResult<()> {
        self.repository
            .create_zap(&NewZap {
                payment_hash: &invoice.payment_hash,
                bolt11: &invoice.bolt11,
                recipient_key,
                amount_msats: i64::try_from(amount_msats)
                    .map_err(|_| AppError::InvalidRequest("zap amount is too large".to_owned()))?,
                request_json,
                backend: invoice.backend,
                created_at: unix_time()?,
            })
            .await
    }

    pub async fn fastest_fee(&self) -> AppResult<u64> {
        let url = self
            .config
            .external
            .mempool_url
            .join("api/v1/fees/recommended")
            .map_err(|error| AppError::Config(format!("invalid mempool URL: {error}")))?;
        let primary = async {
            self.http
                .get(url)
                .send()
                .await
                .map_err(|error| AppError::Upstream(format!("fee request failed: {error}")))?
                .error_for_status()
                .map_err(|error| AppError::Upstream(format!("fee request failed: {error}")))?
                .json::<MempoolFees>()
                .await
                .map_err(|error| AppError::Upstream(format!("fee response was invalid: {error}")))
        }
        .await;
        match primary {
            Ok(fees) => Ok(fees.fastest_fee),
            Err(error) => {
                tracing::warn!(%error, "mempool.space fee request failed; using bitcoiner.live");
                self.bitcoiner_live_fee().await
            }
        }
    }

    /// Handles a `walletnotify` call for one transaction. Returns the number
    /// of newly recorded payments.
    pub async fn process_wallet_transaction(&self, txid: &str) -> AppResult<usize> {
        let payments = self.bitcoin.received_in_transaction(txid).await?;
        let mut processed = 0;
        for payment in payments {
            match self.record_on_chain_payment(&payment).await {
                Ok(true) => processed += 1,
                Ok(false) => {}
                Err(error) => {
                    tracing::error!(%error, address = %payment.address, "could not record on-chain payment");
                }
            }
        }
        Ok(processed)
    }

    async fn record_on_chain_payment(&self, payment: &ReceivedPayment) -> AppResult<bool> {
        if payment.confirmations < 0 {
            return Ok(false);
        }
        let amount_sats = i64::try_from(payment.amount_sats)
            .map_err(|_| AppError::Upstream("received Bitcoin amount is too large".to_owned()))?;
        if !self
            .repository
            .mark_on_chain_paid(&payment.address, amount_sats, &payment.txid)
            .await?
        {
            return Ok(false);
        }
        let record = self.repository.find_by_address(&payment.address).await?;
        tracing::info!(request_id = record.request.id, txid = %payment.txid, amount_sats, "received on-chain payment");
        if let Some(invoice) = &record.invoice
            && let Err(error) = self.lightning.cancel_invoice(&invoice.payment_hash).await
        {
            tracing::warn!(%error, payment_hash = %invoice.payment_hash, "could not cancel the Lightning invoice after an on-chain payment");
        }
        if let Err(error) = self.publish_request(record.request.id).await {
            tracing::error!(%error, request_id = record.request.id, "could not publish request after on-chain payment");
        }
        Ok(true)
    }

    async fn settle_invoice(&self, invoice: &OpenInvoice) {
        match self
            .repository
            .mark_invoice_paid(&invoice.payment_hash)
            .await
        {
            Ok(true) => {
                tracing::info!(request_id = invoice.request_id, payment_hash = %invoice.payment_hash, "invoice settled");
            }
            Ok(false) => {}
            Err(error) => {
                tracing::error!(%error, payment_hash = %invoice.payment_hash, "could not mark invoice paid");
                return;
            }
        }
        if let Err(error) = self.publish_request(invoice.request_id).await {
            tracing::error!(%error, request_id = invoice.request_id, "could not publish request after Lightning payment");
        }
    }

    async fn poll_on_chain(&self) -> AppResult<()> {
        let now = unix_time()?;
        let created_after = now.saturating_sub(
            i64::try_from(self.config.payments.on_chain_expiry_seconds).unwrap_or(i64::MAX),
        );
        let open = self
            .repository
            .open_on_chain_payments(created_after)
            .await?;
        if open.is_empty() {
            return Ok(());
        }
        let received = self.bitcoin.received_payments().await?;
        for payment in open {
            let Ok(expected) = u64::try_from(payment.expected_amount_sats) else {
                continue;
            };
            let Some(found) = received
                .iter()
                .find(|row| row.address == payment.address && row.amount_sats >= expected)
            else {
                continue;
            };
            if let Err(error) = self.record_on_chain_payment(found).await {
                tracing::error!(%error, address = %payment.address, "could not record on-chain payment");
            }
        }
        Ok(())
    }

    async fn retry_unpublished(&self) -> AppResult<()> {
        for request_id in self.repository.paid_unpublished_request_ids().await? {
            if let Err(error) = self.publish_request(request_id).await {
                tracing::error!(%error, request_id, "could not publish paid request");
            }
        }
        Ok(())
    }

    async fn close_expired_requests(&self) -> AppResult<()> {
        let now = unix_time()?;
        let lightning_before = now
            .saturating_sub(i64::from(self.config.payments.invoice_expiry_seconds))
            .saturating_sub(INVOICE_CLOSE_GRACE_SECONDS);
        let on_chain_before = now.saturating_sub(
            i64::try_from(self.config.payments.on_chain_expiry_seconds).unwrap_or(i64::MAX),
        );
        let candidates = self
            .repository
            .expired_request_candidates(
                lightning_before,
                on_chain_before,
                EXPIRED_REQUESTS_PER_PASS,
            )
            .await?;
        for candidate in candidates {
            if self.recover_settled_candidate(&candidate).await {
                continue;
            }
            if self.repository.close_request(candidate.request_id).await? {
                tracing::debug!(request_id = candidate.request_id, "closed expired request");
            }
        }
        Ok(())
    }

    /// Returns true when the candidate must stay open: its invoice is
    /// settled, or the backend could not answer.
    async fn recover_settled_candidate(&self, candidate: &ExpiredCandidate) -> bool {
        let (Some(payment_hash), Some(backend)) = (&candidate.payment_hash, candidate.backend)
        else {
            return false;
        };
        if backend != self.lightning.backend() {
            return false;
        }
        match self.lightning.invoice_state(payment_hash).await {
            Ok(InvoiceState::Settled { .. }) => {
                tracing::warn!(
                    request_id = candidate.request_id,
                    "expired request has a settled invoice; publishing"
                );
                self.settle_invoice(&OpenInvoice {
                    request_id: candidate.request_id,
                    payment_hash: payment_hash.clone(),
                })
                .await;
                true
            }
            Ok(_) => false,
            Err(error) => {
                // The backend may hold a settled invoice. Keep the request
                // open and try again on the next pass.
                tracing::warn!(%error, request_id = candidate.request_id, "could not look up expired invoice; keeping it open");
                true
            }
        }
    }

    async fn publish_zap_receipts(&self) -> AppResult<()> {
        let now = unix_time()?;
        let zaps = self
            .repository
            .unpublished_zaps(
                self.lightning.backend(),
                now.saturating_sub(ZAP_WINDOW_SECONDS),
                ZAPS_PER_PASS,
            )
            .await?;
        for zap in zaps {
            let preimage = match self.lightning.invoice_state(&zap.payment_hash).await {
                Ok(InvoiceState::Settled { preimage }) => preimage,
                Ok(_) => continue,
                Err(error) => {
                    tracing::warn!(%error, payment_hash = %zap.payment_hash, "could not look up zap invoice");
                    continue;
                }
            };
            self.publish_zap(&zap.payment_hash, &preimage).await;
        }
        Ok(())
    }

    /// Publishes the receipt of one settled zap. Concurrent calls for the
    /// same zap are merged, and the zap is reloaded under the guard so that
    /// only one receipt is published.
    async fn publish_zap(&self, payment_hash: &str, preimage: &[u8]) {
        {
            let mut publishing = self.publishing_zaps.lock().await;
            if !publishing.insert(payment_hash.to_owned()) {
                return;
            }
        }
        self.publish_zap_locked(payment_hash, preimage).await;
        self.publishing_zaps.lock().await.remove(payment_hash);
    }

    async fn publish_zap_locked(&self, payment_hash: &str, preimage: &[u8]) {
        let zap = match self.repository.find_unpublished_zap(payment_hash).await {
            Ok(Some(zap)) => zap,
            Ok(None) => return,
            Err(error) => {
                tracing::error!(%error, payment_hash, "could not load zap");
                return;
            }
        };
        match self
            .social
            .publish_zap_receipt(&zap.request_json, &zap.bolt11, preimage)
            .await
        {
            Ok(note_id) => {
                if let Err(error) = self
                    .repository
                    .mark_zap_published(payment_hash, &note_id)
                    .await
                {
                    tracing::error!(%error, payment_hash, "could not record the zap receipt");
                }
            }
            Err(error) => {
                tracing::error!(%error, payment_hash, "could not publish zap receipt");
            }
        }
    }

    async fn refresh_block_height(&self) -> AppResult<()> {
        let block_height = self.lightning.block_height().await?;
        let mut last_block_height = self.last_block_height.lock().await;
        if last_block_height.is_some_and(|height| height != block_height) {
            self.mempool_limit.store(false, Ordering::Relaxed);
        }
        *last_block_height = Some(block_height);
        Ok(())
    }

    /// Runs one full reconciliation pass. Each step logs its own errors so
    /// that one failing request cannot block the others.
    pub async fn reconcile_once(&self) {
        if let Err(error) = self.refresh_block_height().await {
            tracing::error!(%error, "could not read the block height");
        }
        if let Err(error) = self.close_expired_requests().await {
            tracing::error!(%error, "could not close expired requests");
        }
        if let Err(error) = self.poll_on_chain().await {
            tracing::error!(%error, "could not check on-chain payments");
        }
        if let Err(error) = self.retry_unpublished().await {
            tracing::error!(%error, "could not retry paid requests");
        }
        if let Err(error) = self.publish_zap_receipts().await {
            tracing::error!(%error, "could not publish zap receipts");
        }
    }

    /// Processes the oldest open payment records for the Telegram admin
    /// command. The returned count is the number selected for processing.
    pub async fn process_unhandled_requests(
        &self,
        limit: Option<u32>,
        lift_mempool_limit: bool,
    ) -> usize {
        let records = match self.repository.unclosed_payment_records(limit).await {
            Ok(records) => records,
            Err(error) => {
                tracing::error!(%error, "could not load unhandled requests");
                return 0;
            }
        };
        if records.is_empty() {
            return 0;
        }
        if lift_mempool_limit {
            self.mempool_limit.store(false, Ordering::Relaxed);
        }
        tracing::info!(count = records.len(), "processing unhandled requests");

        let received = match self.bitcoin.received_payments().await {
            Ok(received) => Some(received),
            Err(error) => {
                tracing::warn!(%error, "could not list on-chain payments for unhandled requests");
                None
            }
        };
        let now = unix_time().unwrap_or_default();
        for record in &records {
            self.process_unhandled_record(record, received.as_deref(), now)
                .await;
        }
        records.len()
    }

    async fn process_unhandled_record(
        &self,
        record: &PaymentRecord,
        received: Option<&[ReceivedPayment]>,
        now: i64,
    ) {
        let request_id = record.request.id;
        if record.request.txid.is_some() {
            if let Err(error) = self.repository.close_completed_request(request_id).await {
                tracing::error!(%error, request_id, "could not close completed request");
            }
            return;
        }
        let already_paid = record.invoice.as_ref().is_some_and(|invoice| invoice.paid)
            || record
                .on_chain
                .as_ref()
                .is_some_and(|payment| payment.txid.is_some());
        if already_paid {
            if let Err(error) = self.publish_request(request_id).await {
                tracing::error!(%error, request_id, "could not publish unhandled paid request");
            }
            return;
        }

        let mut invoice_lookup_failed = false;
        let mut invoice_canceled = false;
        if let Some(invoice) = &record.invoice
            && invoice.lightning_backend == self.lightning.backend()
        {
            match self.lightning.invoice_state(&invoice.payment_hash).await {
                Ok(InvoiceState::Settled { .. }) => {
                    self.settle_invoice(&OpenInvoice {
                        request_id,
                        payment_hash: invoice.payment_hash.clone(),
                    })
                    .await;
                }
                Ok(InvoiceState::Canceled) => invoice_canceled = true,
                Ok(InvoiceState::Open) => {}
                Err(error) => {
                    invoice_lookup_failed = true;
                    tracing::warn!(%error, request_id, "could not look up unhandled invoice");
                }
            }
        }

        if self.request_is_complete(request_id).await {
            return;
        }

        if let (Some(payment), Some(received)) = (&record.on_chain, received)
            && payment.txid.is_none()
            && let Ok(expected) = u64::try_from(payment.expected_amount_sats)
            && let Some(found) = received
                .iter()
                .find(|row| row.address == payment.address && row.amount_sats >= expected)
            && let Err(error) = self.record_on_chain_payment(found).await
        {
            tracing::error!(%error, request_id, "could not process unhandled on-chain payment");
        }

        if self.request_is_complete(request_id).await || invoice_lookup_failed {
            return;
        }
        let expiry = if record.on_chain.is_some() {
            i64::try_from(self.config.payments.on_chain_expiry_seconds).unwrap_or(i64::MAX)
        } else {
            i64::from(self.config.payments.invoice_expiry_seconds)
                .saturating_add(INVOICE_CLOSE_GRACE_SECONDS)
        };
        if (invoice_canceled || record.request.created_at.saturating_add(expiry) < now)
            && let Err(error) = self.repository.close_request(request_id).await
        {
            tracing::error!(%error, request_id, "could not close unhandled request");
        }
    }

    async fn request_is_complete(&self, request_id: i64) -> bool {
        self.repository
            .find_record(request_id)
            .await
            .is_ok_and(|record| record.request.txid.is_some())
    }

    pub async fn run_reconciler(self) {
        let mut interval = tokio::time::interval(Duration::from_secs(
            self.config.payments.reconcile_interval_seconds,
        ));
        loop {
            interval.tick().await;
            self.reconcile_once().await;
        }
    }

    /// Watches for Lightning payments through the backend subscription and
    /// reconnects when the stream ends.
    pub async fn run_lightning_watch(self) {
        loop {
            match self.lightning.subscribe_invoices().await {
                Ok(events) => {
                    tracing::info!("subscribed to Lightning invoice updates");
                    self.consume_invoice_events(events).await;
                    tracing::warn!("Lightning invoice subscription ended; reconnecting");
                }
                Err(error) => {
                    tracing::error!(%error, "could not subscribe to Lightning invoice updates");
                }
            }
            tokio::time::sleep(SUBSCRIPTION_RETRY).await;
        }
    }

    async fn consume_invoice_events(&self, mut events: InvoiceStream) {
        while let Some(event) = events.next().await {
            match event {
                Ok(InvoiceEvent::Settled {
                    payment_hash,
                    preimage,
                }) => {
                    self.handle_settled_invoice(&payment_hash, &preimage).await;
                }
                Err(error) => {
                    tracing::error!(%error, "Lightning invoice stream failed");
                    return;
                }
            }
        }
    }

    /// Handles a settled invoice from the subscription. The invoice belongs
    /// to a request, to a zap, or to an LNURL-pay donation that needs no
    /// action.
    async fn handle_settled_invoice(&self, payment_hash: &str, preimage: &[u8]) {
        match self.repository.find_by_payment_hash(payment_hash).await {
            Ok(record) => {
                let Some(invoice) = record.invoice else {
                    return;
                };
                if invoice.lightning_backend != self.lightning.backend() {
                    return;
                }
                self.settle_invoice(&OpenInvoice {
                    request_id: record.request.id,
                    payment_hash: invoice.payment_hash,
                })
                .await;
                return;
            }
            Err(AppError::NotFound(_)) => {}
            Err(error) => {
                tracing::error!(%error, payment_hash, "could not load the request of a settled invoice");
                return;
            }
        }
        self.publish_zap(payment_hash, preimage).await;
    }

    /// Creates and broadcasts the `OP_RETURN` transaction for a paid request.
    /// Concurrent calls for the same request are merged, and the request is
    /// reloaded under the publishing lock so that no stale copy is used.
    async fn publish_request(&self, request_id: i64) -> AppResult<()> {
        {
            let mut processing = self.processing.lock().await;
            if !processing.insert(request_id) {
                return Ok(());
            }
        }
        let result = self.publish_locked(request_id).await;
        self.processing.lock().await.remove(&request_id);
        result
    }

    async fn publish_locked(&self, request_id: i64) -> AppResult<()> {
        let _publishing_guard = self.publishing.lock().await;
        let record = self.repository.find_record(request_id).await?;
        self.publish_record_inner(&record).await
    }

    async fn publish_record_inner(&self, record: &PaymentRecord) -> AppResult<()> {
        if record.request.txid.is_some() {
            return Ok(());
        }
        if self.mempool_limit() {
            tracing::debug!(
                request_id = record.request.id,
                "mempool limit is active; deferring publish"
            );
            return Ok(());
        }
        let prepared = self.prepare_transaction(record).await?;
        let raw = consensus::encode::serialize_hex(&prepared.transaction);
        let non_standard = record.request.message.len() > STANDARD_OP_RETURN_BYTES;
        let (txid, slipstream_only) = self
            .broadcast_prepared(record, &prepared, &raw, non_standard)
            .await?;
        let profit_sats = profit(record, prepared.fee_sats);
        let btc_price_cents = self.btc_price_cents().await.unwrap_or_else(|error| {
            tracing::error!(%error, "could not fetch BTC price");
            0
        });
        let chain_fee_sats = i64::try_from(prepared.fee_sats)
            .map_err(|_| AppError::Internal("chain fee is too large".to_owned()))?;
        let vsize = i64::try_from(prepared.transaction.vsize())
            .map_err(|_| AppError::Internal("transaction vsize is too large".to_owned()))?;
        let txid = txid.to_string();
        self.repository
            .complete_request(
                record.request.id,
                &CompletedRequest {
                    txid: &txid,
                    chain_fee_sats,
                    vsize,
                    profit_sats,
                    btc_price_cents,
                },
            )
            .await?;
        tracing::info!(request_id = record.request.id, %txid, "OP_RETURN transaction published");
        self.broadcast_secondary(&raw, non_standard, slipstream_only)
            .await;
        let nip5_public_key = self
            .repository
            .nip5_public_key(record.request.id)
            .await
            .unwrap_or_else(|error| {
                tracing::error!(%error, request_id = record.request.id, "could not load the NIP-05 key");
                None
            });
        let report = match self.repository.accounting_report().await {
            Ok(report) => Some(report),
            Err(error) => {
                tracing::error!(%error, "could not load Telegram accounting totals");
                None
            }
        };
        let mut completed_record = record.clone();
        completed_record.request.txid = Some(txid.clone());
        completed_record.request.chain_fee_sats = Some(chain_fee_sats);
        completed_record.request.vsize = Some(vsize);
        completed_record.request.profit_sats = profit_sats;
        completed_record.request.btc_price_cents = btc_price_cents;
        completed_record.request.closed = true;
        self.social
            .publish_completion(
                &completed_record,
                &txid,
                nip5_public_key.as_deref(),
                report.as_ref(),
            )
            .await;
        Ok(())
    }

    async fn prepare_transaction(&self, record: &PaymentRecord) -> AppResult<PreparedTransaction> {
        if let Some(raw) = &record.request.transaction {
            let bytes = hex::decode(raw).map_err(|error| {
                AppError::Internal(format!("stored transaction is not valid hex: {error}"))
            })?;
            let transaction: Transaction = consensus::deserialize(&bytes)?;
            let wallet = funding_wallet(record, &transaction)?;
            // The fee is written at completion, so recover it from the
            // input, which the funding wallet knows.
            let input_sats = self
                .bitcoin
                .output_value(wallet, first_input(&transaction)?)
                .await?;
            let fee_sats = transaction_fee(&transaction, input_sats)?;
            return Ok(PreparedTransaction {
                transaction,
                fee_sats,
                wallet,
                stored: true,
            });
        }
        let funding = self.funding_for(record).await?;
        let signed = self
            .bitcoin
            .create_signed_op_return(
                &record.request.message,
                record.request.fee_rate_sat_vb,
                &funding,
            )
            .await?;
        self.repository
            .store_signed_transaction(
                record.request.id,
                &consensus::encode::serialize_hex(&signed.transaction),
            )
            .await?;
        Ok(PreparedTransaction {
            transaction: signed.transaction,
            fee_sats: signed.fee_sats,
            wallet: signed.wallet,
            stored: false,
        })
    }

    /// Chooses the input for a request. A request paid on-chain spends its
    /// own payment output, so a replaced payment cannot leave a funded
    /// `OP_RETURN` behind.
    async fn funding_for(&self, record: &PaymentRecord) -> AppResult<Funding> {
        let Some(on_chain) = &record.on_chain else {
            return Ok(Funding::SendingWallet);
        };
        let Some(txid) = &on_chain.txid else {
            return Ok(Funding::SendingWallet);
        };
        let payment = self
            .bitcoin
            .payment_output(txid, &on_chain.address)
            .await?
            .ok_or_else(|| {
                AppError::Upstream(format!(
                    "the receiving wallet has no output for {} in {txid}",
                    on_chain.address
                ))
            })?;
        if payment.confirmations < 0 {
            return Err(AppError::Upstream(format!(
                "the on-chain payment {txid} was replaced or double-spent"
            )));
        }
        Ok(Funding::PaymentOutput {
            txid: payment.txid,
            vout: payment.vout,
            amount_sats: payment.amount_sats,
        })
    }

    async fn broadcast_prepared(
        &self,
        record: &PaymentRecord,
        prepared: &PreparedTransaction,
        raw: &str,
        non_standard: bool,
    ) -> AppResult<(Txid, bool)> {
        match self.bitcoin.broadcast(&prepared.transaction).await {
            Ok(txid) => {
                self.mempool_limit.store(false, Ordering::Relaxed);
                Ok((txid, false))
            }
            Err(error) if prepared.stored && is_missing_inputs_error(&error.to_string()) => {
                self.repository
                    .clear_signed_transaction(record.request.id)
                    .await?;
                Err(AppError::Upstream(format!(
                    "stored transaction for request {} has missing inputs and will be rebuilt: {error}",
                    record.request.id
                )))
            }
            Err(core_error) => {
                let message = core_error.to_string();
                if is_mempool_limit_error(&message) {
                    // The transaction is valid. Keep the request paid and
                    // unpublished, and retry after the next block.
                    tracing::warn!(
                        "Bitcoin Core reported the mempool chain limit; pausing publishing until the next block"
                    );
                    self.mempool_limit.store(true, Ordering::Relaxed);
                    return Err(core_error);
                }
                if !(non_standard && is_policy_rejection(&message)) {
                    return Err(core_error);
                }
                // Only a standardness rejection of a non-standard message goes
                // to Slipstream. Every other error is retried through Core.
                self.submit_slipstream(raw)
                    .await
                    .map_err(|slipstream_error| {
                        AppError::Upstream(format!(
                            "Bitcoin Core and MARA Slipstream rejected the transaction: \
                             {core_error}; {slipstream_error}"
                        ))
                    })?;
                // Bitcoin Core never saw this transaction, so lock its input
                // before another request can spend it.
                let outpoint = first_input(&prepared.transaction)?;
                if let Err(error) = self.bitcoin.lock_output(prepared.wallet, outpoint).await {
                    tracing::warn!(%error, %outpoint, "could not lock the input of a Slipstream-only transaction");
                }
                Ok((prepared.transaction.compute_txid(), true))
            }
        }
    }

    async fn bitcoiner_live_fee(&self) -> AppResult<u64> {
        let mut url = self
            .config
            .external
            .bitcoiner_live_url
            .join("api/fees/estimates/latest")
            .map_err(|error| AppError::Config(format!("invalid bitcoiner.live URL: {error}")))?;
        url.query_pairs_mut().append_pair("confidence", "0.8");
        let value: serde_json::Value = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|error| AppError::Upstream(format!("backup fee request failed: {error}")))?
            .error_for_status()
            .map_err(|error| AppError::Upstream(format!("backup fee request failed: {error}")))?
            .json()
            .await
            .map_err(|error| {
                AppError::Upstream(format!("backup fee response was invalid: {error}"))
            })?;
        let rate = value
            .get("estimates")
            .and_then(|estimates| estimates.get("30"))
            .and_then(|estimate| estimate.get("sat_per_vbyte"))
            .and_then(serde_json::Value::as_f64)
            .ok_or_else(|| {
                AppError::Upstream("backup fee response has no 30-minute estimate".to_owned())
            })?;
        if !rate.is_finite() || rate < 0.0 {
            return Err(AppError::Upstream(
                "backup fee response has an invalid rate".to_owned(),
            ));
        }
        rate.ceil().to_string().parse::<u64>().map_err(|error| {
            AppError::Upstream(format!("backup fee response has an invalid rate: {error}"))
        })
    }

    async fn btc_price_cents(&self) -> AppResult<i64> {
        let url = self
            .config
            .external
            .coinbase_url
            .join("v2/prices/BTC-USD/spot")
            .map_err(|error| AppError::Config(format!("invalid Coinbase URL: {error}")))?;
        let price: CoinbasePrice = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|error| AppError::Upstream(format!("Coinbase request failed: {error}")))?
            .error_for_status()
            .map_err(|error| AppError::Upstream(format!("Coinbase request failed: {error}")))?
            .json()
            .await
            .map_err(|error| {
                AppError::Upstream(format!("Coinbase response was invalid: {error}"))
            })?;
        dollars_to_cents(&price.data.amount)
    }

    async fn broadcast_secondary(&self, raw: &str, non_standard: bool, slipstream_submitted: bool) {
        if !non_standard && let Err(error) = self.submit_esplora(raw).await {
            tracing::warn!(%error, "secondary Esplora broadcast failed");
        }
        if !slipstream_submitted && let Err(error) = self.submit_slipstream(raw).await {
            tracing::warn!(%error, "MARA Slipstream broadcast failed");
        }
    }

    async fn submit_esplora(&self, raw: &str) -> AppResult<()> {
        let url = self
            .config
            .external
            .esplora_url
            .join("tx")
            .map_err(|error| AppError::Config(format!("invalid Esplora URL: {error}")))?;
        self.http
            .post(url)
            .body(raw.to_owned())
            .send()
            .await
            .map_err(|error| AppError::Upstream(format!("Esplora broadcast failed: {error}")))?
            .error_for_status()
            .map_err(|error| AppError::Upstream(format!("Esplora broadcast failed: {error}")))?;
        Ok(())
    }

    async fn submit_slipstream(&self, raw: &str) -> AppResult<()> {
        self.http
            .post(self.config.external.slipstream_url.clone())
            .json(&serde_json::json!({ "tx_hex": raw }))
            .send()
            .await
            .map_err(|error| AppError::Upstream(format!("Slipstream broadcast failed: {error}")))?
            .error_for_status()
            .map_err(|error| AppError::Upstream(format!("Slipstream broadcast failed: {error}")))?;
        Ok(())
    }

    async fn price(&self, input: &CreateRequest) -> AppResult<PriceQuote> {
        let (fastest_fee, block_height) =
            tokio::try_join!(self.fastest_fee(), self.lightning.block_height())?;
        quote(
            &self.config.payments,
            &input.message,
            input.no_twitter,
            fastest_fee,
            block_height,
        )
    }

    #[must_use]
    pub fn mempool_limit(&self) -> bool {
        self.mempool_limit.load(Ordering::Relaxed)
    }
}

/// The invoice description hash commits to the message, so a payer can
/// compare it with the hash shown on the invoice page.
fn message_hash(message: &[u8]) -> [u8; 32] {
    Sha256::digest(message).into()
}

fn msats(sats: u64) -> AppResult<u64> {
    sats.checked_mul(1_000)
        .ok_or_else(|| AppError::InvalidRequest("invoice amount is too large".to_owned()))
}

fn first_input(transaction: &Transaction) -> AppResult<OutPoint> {
    transaction
        .input
        .first()
        .map(|input| input.previous_output)
        .ok_or_else(|| AppError::Internal("stored transaction has no input".to_owned()))
}

/// The fee of a single-input transaction.
fn transaction_fee(transaction: &Transaction, input_sats: u64) -> AppResult<u64> {
    let output_sats = transaction
        .output
        .iter()
        .try_fold(0_u64, |total, output| {
            total.checked_add(output.value.to_sat())
        })
        .ok_or_else(|| AppError::Internal("transaction outputs overflowed".to_owned()))?;
    input_sats.checked_sub(output_sats).ok_or_else(|| {
        AppError::Internal("stored transaction spends more than its input".to_owned())
    })
}

/// Finds the wallet that owns the input of a stored transaction.
fn funding_wallet(record: &PaymentRecord, transaction: &Transaction) -> AppResult<Wallet> {
    let input_txid = first_input(transaction)?.txid.to_string();
    let paid_on_chain = record
        .on_chain
        .as_ref()
        .and_then(|payment| payment.txid.as_deref())
        .is_some_and(|txid| txid.eq_ignore_ascii_case(&input_txid));
    Ok(if paid_on_chain {
        Wallet::Receiving
    } else {
        Wallet::Sending
    })
}

/// Profit is the amount received minus the chain fee. On-chain payments use
/// the amount that was really received.
fn profit(record: &PaymentRecord, fee_sats: u64) -> Option<i64> {
    let paid_sats = record
        .on_chain
        .as_ref()
        .filter(|payment| payment.txid.is_some())
        .and_then(|payment| payment.amount_paid_sats)
        .or_else(|| {
            record
                .invoice
                .as_ref()
                .and_then(|invoice| invoice.amount_sats)
        })
        .or_else(|| {
            record
                .on_chain
                .as_ref()
                .map(|payment| payment.expected_amount_sats)
        })?;
    let fee = i64::try_from(fee_sats).ok()?;
    paid_sats.checked_sub(fee)
}

fn is_mempool_limit_error(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("too many unconfirmed ancestors") || message.contains("too-long-mempool-chain")
}

fn new_request(input: &CreateRequest, fee_rate_sat_vb: u64) -> AppResult<NewRequest<'_>> {
    Ok(NewRequest {
        message: &input.message,
        no_twitter: input.no_twitter,
        fee_rate_sat_vb,
        node_id: None,
        telegram_id: None,
        nostr_key: None,
        created_at: unix_time()?,
    })
}

fn unix_time() -> AppResult<i64> {
    let seconds = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_err(|error| AppError::Internal(format!("system clock is before Unix epoch: {error}")))?
        .as_secs();
    i64::try_from(seconds).map_err(|_| AppError::Internal("system time is too large".to_owned()))
}

fn validate_invoice_network(invoice: &str, network: bitcoin::Network) -> AppResult<()> {
    let lower = invoice.to_ascii_lowercase();
    let valid = match network {
        // "lnbc" must be followed by the amount or the bech32 separator so
        // that regtest invoices ("lnbcrt") are rejected.
        bitcoin::Network::Bitcoin => {
            lower.starts_with("lnbc")
                && lower
                    .as_bytes()
                    .get(4)
                    .is_some_and(|byte| byte.is_ascii_digit() || *byte == b'1')
        }
        bitcoin::Network::Regtest => lower.starts_with("lnbcrt"),
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err(AppError::Upstream(format!(
            "Lightning backend returned an invoice for the wrong network: {}",
            invoice.get(..12).unwrap_or(invoice)
        )))
    }
}

fn validate_nip5_name(name: &str, banned_words: &[String]) -> AppResult<()> {
    const RESERVED: [&str; 6] = [
        "_",
        "me",
        "opreturnbot",
        "op_return_bot",
        "OP_RETURN bot",
        "OP_RETURN Bot",
    ];
    if name.is_empty() || name.len() > 10 {
        return Err(AppError::InvalidRequest(
            "NIP-05 name must contain 1 to 10 ASCII characters".to_owned(),
        ));
    }
    if !name
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-".contains(&byte))
    {
        return Err(AppError::InvalidRequest(
            "NIP-05 name can contain lowercase letters, numbers, '.', '_' and '-'".to_owned(),
        ));
    }
    if RESERVED
        .iter()
        .copied()
        .chain(banned_words.iter().map(String::as_str))
        .any(|reserved| reserved.eq_ignore_ascii_case(name))
    {
        return Err(AppError::InvalidRequest(
            "that NIP-05 name is reserved".to_owned(),
        ));
    }
    Ok(())
}

fn normalize_nostr_public_key(public_key: &str) -> AppResult<String> {
    if public_key.len() == 64 && public_key.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Ok(public_key.to_ascii_lowercase());
    }
    let (hrp, bytes) = bitcoin::bech32::decode(public_key).map_err(|error| {
        AppError::InvalidRequest(format!("Nostr public key is invalid: {error}"))
    })?;
    if hrp.as_str() != "npub" || bytes.len() != 32 {
        return Err(AppError::InvalidRequest(
            "Nostr public key must be an npub or 32-byte hex key".to_owned(),
        ));
    }
    Ok(hex::encode(bytes))
}

fn dollars_to_cents(value: &str) -> AppResult<i64> {
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    if whole.is_empty()
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(AppError::Upstream(
            "Coinbase returned an invalid price".to_owned(),
        ));
    }
    let dollars = whole.parse::<i64>().map_err(|error| {
        AppError::Upstream(format!("Coinbase returned an invalid price: {error}"))
    })?;
    let mut digits = fraction.bytes();
    let tens = i64::from(digits.next().unwrap_or(b'0') - b'0');
    let ones = i64::from(digits.next().unwrap_or(b'0') - b'0');
    let round_up = digits.next().is_some_and(|digit| digit >= b'5');
    dollars
        .checked_mul(100)
        .and_then(|cents| cents.checked_add(tens * 10 + ones + i64::from(round_up)))
        .ok_or_else(|| AppError::Upstream("Coinbase price is too large".to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Invoice, LightningBackend, OnChainPayment, OpReturnRequest};

    fn record(on_chain: Option<OnChainPayment>) -> PaymentRecord {
        PaymentRecord {
            request: OpReturnRequest {
                id: 7,
                message: b"hello".to_vec(),
                no_twitter: false,
                fee_rate_sat_vb: 10,
                node_id: None,
                telegram_id: None,
                nostr_key: None,
                created_at: 0,
                transaction: None,
                txid: None,
                profit_sats: None,
                chain_fee_sats: None,
                vsize: None,
                closed: false,
                btc_price_cents: 0,
            },
            invoice: Some(Invoice {
                payment_hash: "ab".repeat(32),
                request_id: 7,
                bolt11: "lnbc1test".to_owned(),
                paid: false,
                amount_sats: Some(5_000),
                lightning_backend: LightningBackend::Lnd,
                claim_preimage: None,
            }),
            on_chain,
        }
    }

    #[test]
    fn validates_supported_invoice_networks() {
        assert!(validate_invoice_network("lnbc10u1test", bitcoin::Network::Bitcoin).is_ok());
        assert!(validate_invoice_network("lnbc1test", bitcoin::Network::Bitcoin).is_ok());
        assert!(validate_invoice_network("LNBC10U1TEST", bitcoin::Network::Bitcoin).is_ok());
        assert!(validate_invoice_network("lnbcrt10u1test", bitcoin::Network::Regtest).is_ok());
        assert!(validate_invoice_network("lnbc10u1test", bitcoin::Network::Regtest).is_err());
    }

    #[test]
    fn rejects_regtest_and_testnet_invoices_on_mainnet() {
        assert!(validate_invoice_network("lnbcrt10u1test", bitcoin::Network::Bitcoin).is_err());
        assert!(validate_invoice_network("lntb10u1test", bitcoin::Network::Bitcoin).is_err());
        assert!(validate_invoice_network("lntbs10u1test", bitcoin::Network::Bitcoin).is_err());
    }

    #[test]
    fn validates_nip5_names() {
        assert!(validate_nip5_name("alice_1", &[]).is_ok());
        assert!(validate_nip5_name("Alice", &[]).is_err());
        assert!(validate_nip5_name("opreturnbot", &[]).is_err());
    }

    #[test]
    fn rounds_coinbase_price_to_cents() {
        assert_eq!(dollars_to_cents("77991.705").unwrap(), 7_799_171);
        assert_eq!(dollars_to_cents("1").unwrap(), 100);
        assert!(dollars_to_cents("1.2x").is_err());
        assert!(dollars_to_cents("-1").is_err());
    }

    #[test]
    fn recovers_the_fee_of_a_stored_transaction() {
        use bitcoin::{Amount, ScriptBuf, TxOut, absolute, transaction};
        let transaction = Transaction {
            version: transaction::Version::TWO,
            lock_time: absolute::LockTime::from_consensus(106),
            input: Vec::new(),
            output: vec![
                TxOut {
                    value: Amount::from_sat(18_000),
                    script_pubkey: ScriptBuf::new(),
                },
                TxOut {
                    value: Amount::ZERO,
                    script_pubkey: ScriptBuf::new(),
                },
            ],
        };
        assert_eq!(transaction_fee(&transaction, 20_000).unwrap(), 2_000);
        assert!(transaction_fee(&transaction, 17_000).is_err());
    }

    #[test]
    fn identifies_mempool_ancestor_limits() {
        assert!(is_mempool_limit_error("too-long-mempool-chain"));
        assert!(is_mempool_limit_error("Too many unconfirmed ancestors"));
        assert!(!is_mempool_limit_error("insufficient fee"));
    }

    #[test]
    fn computes_profit_from_the_amount_received() {
        assert_eq!(profit(&record(None), 1_200), Some(3_800));
        let paid_on_chain = record(Some(OnChainPayment {
            address: "bcrt1qtest".to_owned(),
            request_id: 7,
            expected_amount_sats: 5_000,
            amount_paid_sats: Some(6_000),
            txid: Some("ab".repeat(32)),
        }));
        assert_eq!(profit(&paid_on_chain, 1_200), Some(4_800));
        let unpaid_on_chain = record(Some(OnChainPayment {
            address: "bcrt1qtest".to_owned(),
            request_id: 7,
            expected_amount_sats: 5_000,
            amount_paid_sats: None,
            txid: None,
        }));
        assert_eq!(profit(&unpaid_on_chain, 1_200), Some(3_800));
    }
}
