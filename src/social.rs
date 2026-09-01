use nostr_sdk::prelude::{
    Client as NostrClient, Event, EventBuilder, FinalizeEvent, Keys, Kind, PublicKey, RelayUrl,
    Tag, ToBech32,
};
use reqwest_oauth1::{OAuthClientProvider, Secrets};

use crate::{
    AppConfig, AppError, AppResult,
    config::{NostrConfig, TelegramConfig, TwitterConfig},
    payment_service::{CreateRequest, PaymentService},
    repository::{AccountingReport, PaymentRecord, Repository},
};

/// Telegram rejects messages above 4,096 characters.
const TELEGRAM_MESSAGE_CHARS: usize = 3_000;
/// Upper bound on relays taken from a zap request.
const ZAP_REQUEST_RELAYS: usize = 5;

#[derive(Clone, Default)]
pub struct SocialPublisher {
    nostr: Option<NostrPublisher>,
    twitter: Option<TwitterPublisher>,
    telegram: Option<TelegramPublisher>,
}

#[derive(Clone)]
struct NostrPublisher {
    client: NostrClient,
    keys: Keys,
    relays: Vec<RelayUrl>,
}

#[derive(Clone)]
struct TwitterPublisher {
    client: reqwest12::Client,
    consumer_key: String,
    consumer_secret: String,
    access_token: String,
    access_secret: String,
    banned_words: Vec<String>,
}

#[derive(Clone)]
struct TelegramPublisher {
    client: reqwest::Client,
    token: String,
    chat_id: i64,
}

#[derive(serde::Deserialize)]
struct TelegramResponse<T> {
    ok: bool,
    result: T,
}

#[derive(serde::Deserialize)]
struct TelegramUpdate {
    update_id: i64,
    message: Option<TelegramMessage>,
}

#[derive(serde::Deserialize)]
struct TelegramMessage {
    chat: TelegramChat,
    from: Option<TelegramUser>,
    text: Option<String>,
}

#[derive(serde::Deserialize)]
struct TelegramChat {
    id: i64,
}

#[derive(serde::Deserialize)]
struct TelegramUser {
    id: i64,
}

#[derive(serde::Deserialize)]
struct TwitterCreateResponse {
    data: TwitterCreateData,
}

#[derive(serde::Deserialize)]
struct TwitterCreateData {
    id: String,
}

impl SocialPublisher {
    pub async fn connect(config: &AppConfig) -> AppResult<Self> {
        Ok(Self {
            nostr: NostrPublisher::connect(&config.nostr).await?,
            twitter: TwitterPublisher::connect(&config.twitter).await?,
            telegram: TelegramPublisher::connect(&config.telegram).await?,
        })
    }

    /// Announces a completed request. `nip5_public_key` is the buyer of a
    /// NIP-05 name, who is tagged in the Nostr note.
    pub async fn publish_completion(
        &self,
        record: &PaymentRecord,
        txid: &str,
        nip5_public_key: Option<&str>,
        report: Option<&AccountingReport>,
    ) {
        let link = format!("https://mempool.space/tx/{txid}");
        let message = record.request.message_text();
        let is_json_object = serde_json::from_str::<serde_json::Value>(&message)
            .is_ok_and(|value| value.is_object());
        let mut nostr_note = None;
        let mut tweet_id = None;
        if !record.request.no_twitter && !is_json_object {
            if let Some(nostr) = &self.nostr {
                let tagged = tagged_public_key(nip5_public_key, &message);
                match nostr.publish(&message, &link, tagged).await {
                    Ok(note) => nostr_note = Some(note),
                    Err(error) => {
                        tracing::error!(%error, %txid, "could not publish Nostr note");
                    }
                }
            }
            if let Some(twitter) = &self.twitter {
                match twitter.publish(&message, &link).await {
                    Ok(id) => tweet_id = Some(id),
                    Err(error) => tracing::error!(%error, %txid, "could not publish tweet"),
                }
            }
        }
        if let Some(telegram) = &self.telegram
            && let Err(error) = telegram
                .publish(
                    record,
                    txid,
                    tweet_id.as_deref(),
                    nostr_note.as_deref(),
                    report,
                )
                .await
        {
            tracing::error!(%error, %txid, "could not send Telegram notification");
        }
    }

    pub async fn run_telegram_bot(
        self,
        payments: PaymentService,
        repository: Repository,
        public_url: url::Url,
    ) {
        let Some(telegram) = self.telegram else {
            return;
        };
        telegram.run(payments, repository, public_url).await;
    }

    #[must_use]
    pub fn nostr_public_key(&self) -> Option<String> {
        self.nostr
            .as_ref()
            .map(|publisher| publisher.keys.public_key().to_hex())
    }

    /// Publishes a NIP-57 zap receipt to the configured relays and to the
    /// relays named in the zap request.
    pub async fn publish_zap_receipt(
        &self,
        request_json: &str,
        bolt11: &str,
        preimage: &[u8],
    ) -> AppResult<String> {
        let publisher = self.nostr.as_ref().ok_or_else(|| {
            AppError::Config("Nostr is required to publish zap receipts".to_owned())
        })?;
        let request = parse_zap_request(request_json)?;
        let event = zap_receipt_builder(&request, request_json, bolt11, preimage)?
            .finalize(&publisher.keys)
            .map_err(|error| AppError::Upstream(format!("could not sign zap receipt: {error}")))?;
        publisher
            .send_to_relays_and(&event, zap_request_relays(&request))
            .await
    }

    pub fn validate_zap_request(&self, request_json: &str, amount_msats: u64) -> AppResult<()> {
        let recipient = self.nostr_public_key().ok_or_else(|| {
            AppError::Config("Nostr is required to accept zap requests".to_owned())
        })?;
        let request = parse_zap_request(request_json)?;
        if request.kind != Kind::ZapRequest {
            return Err(AppError::InvalidRequest(
                "event is not a zap request".to_owned(),
            ));
        }
        let tagged_amount = request
            .tags
            .iter()
            .find(|tag| tag.kind() == "amount")
            .and_then(|tag| tag.content())
            .and_then(|amount| amount.parse::<u64>().ok());
        if tagged_amount.is_some_and(|amount| amount != amount_msats) {
            return Err(AppError::InvalidRequest(
                "zap request amount does not match callback amount".to_owned(),
            ));
        }
        let correct_recipient = request
            .tags
            .iter()
            .filter(|tag| tag.kind() == "p")
            .filter_map(|tag| tag.content())
            .any(|public_key| public_key.eq_ignore_ascii_case(&recipient));
        if !correct_recipient {
            return Err(AppError::InvalidRequest(
                "zap request recipient does not match this server".to_owned(),
            ));
        }
        Ok(())
    }
}

impl NostrPublisher {
    async fn connect(config: &NostrConfig) -> AppResult<Option<Self>> {
        let Some(path) = &config.private_key_file else {
            return Ok(None);
        };
        let secret = read_secret(path, "Nostr private key").await?;
        let keys = Keys::parse(&secret)
            .map_err(|error| AppError::Config(format!("Nostr private key is invalid: {error}")))?;
        let client = NostrClient::new();
        let mut relays = Vec::new();
        for relay in &config.relays {
            let url = RelayUrl::parse(relay.as_str()).map_err(|error| {
                AppError::Config(format!("Nostr relay {relay} is invalid: {error}"))
            })?;
            client.add_relay(url.clone()).await.map_err(|error| {
                AppError::Config(format!("could not add Nostr relay {relay}: {error}"))
            })?;
            relays.push(url);
        }
        client.connect().await;
        Ok(Some(Self {
            client,
            keys,
            relays,
        }))
    }

    async fn publish(
        &self,
        message: &str,
        link: &str,
        tagged: Option<PublicKey>,
    ) -> AppResult<String> {
        let mut builder = EventBuilder::new(Kind::TextNote, format!("{message}\n\n{link}"));
        if let Some(public_key) = tagged {
            builder = builder.tags([Tag::public_key(public_key)]);
        }
        let event = builder
            .finalize(&self.keys)
            .map_err(|error| AppError::Upstream(format!("could not sign Nostr note: {error}")))?;
        self.client
            .send_event(&event)
            .await
            .map_err(|error| AppError::Upstream(format!("could not send Nostr note: {error}")))?;
        Ok(event.id.to_bech32().unwrap_or_else(|_| event.id.to_hex()))
    }

    /// Sends an event to the configured relays plus `extra` relays, which
    /// are added for this send only.
    async fn send_to_relays_and(&self, event: &Event, extra: Vec<RelayUrl>) -> AppResult<String> {
        let mut temporary = Vec::new();
        for url in &extra {
            if self.relays.contains(url) {
                continue;
            }
            match self.client.add_relay(url.clone()).await {
                Ok(true) => {
                    temporary.push(url.clone());
                    if let Err(error) = self.client.connect_relay(url.clone()).await {
                        tracing::debug!(%error, relay = %url, "could not connect to zap request relay");
                    }
                }
                Ok(false) => {}
                Err(error) => {
                    tracing::debug!(%error, relay = %url, "could not add zap request relay");
                }
            }
        }
        let targets: Vec<RelayUrl> = self
            .relays
            .iter()
            .chain(temporary.iter())
            .cloned()
            .collect();
        let output = self.client.send_event(event).to(targets).await;
        for url in temporary {
            if let Err(error) = self.client.remove_relay(url.clone()).await {
                tracing::debug!(%error, relay = %url, "could not remove zap request relay");
            }
        }
        let output = output
            .map_err(|error| AppError::Upstream(format!("could not send zap receipt: {error}")))?;
        Ok(output.id().to_hex())
    }
}

fn parse_zap_request(request_json: &str) -> AppResult<Event> {
    let request = Event::from_json(request_json)
        .map_err(|error| AppError::InvalidRequest(format!("zap request is invalid: {error}")))?;
    request.verify().map_err(|error| {
        AppError::InvalidRequest(format!("zap request signature is invalid: {error}"))
    })?;
    Ok(request)
}

/// Builds a NIP-57 zap receipt. The description tag holds the zap request
/// exactly as the sender submitted it, because the invoice description hash
/// commits to those bytes. A re-serialized event can order its keys
/// differently, and receipt validation would then fail.
fn zap_receipt_builder(
    request: &Event,
    request_json: &str,
    bolt11: &str,
    preimage: &[u8],
) -> AppResult<EventBuilder> {
    let mut tags = vec![
        receipt_tag("bolt11", bolt11)?,
        receipt_tag("description", request_json)?,
    ];
    if !preimage.is_empty() {
        tags.push(receipt_tag("preimage", &hex::encode(preimage))?);
    }
    for kind in ["e", "a", "p"] {
        if let Some(tag) = request.tags.iter().find(|tag| tag.kind() == kind) {
            tags.push(tag.clone());
        }
    }
    tags.push(receipt_tag("P", &request.pubkey.to_hex())?);
    Ok(EventBuilder::new(Kind::ZapReceipt, "").tags(tags))
}

fn receipt_tag(kind: &str, value: &str) -> AppResult<Tag> {
    Tag::parse([kind, value]).map_err(|error| {
        AppError::Internal(format!("could not build zap receipt tag {kind}: {error}"))
    })
}

/// The key that a completion note tags: the buyer of a NIP-05 name, or the
/// message itself when the message is a Nostr public key.
fn tagged_public_key(nip5_public_key: Option<&str>, message: &str) -> Option<PublicKey> {
    nip5_public_key
        .and_then(|key| PublicKey::parse(key).ok())
        .or_else(|| PublicKey::parse(message.trim()).ok())
}

/// Relays listed in the `relays` tag of a zap request.
fn zap_request_relays(request: &Event) -> Vec<RelayUrl> {
    request
        .tags
        .iter()
        .find(|tag| tag.kind() == "relays")
        .map(|tag| {
            tag.as_slice()
                .iter()
                .skip(1)
                .filter_map(|url| public_zap_relay(url))
                .take(ZAP_REQUEST_RELAYS)
                .collect()
        })
        .unwrap_or_default()
}

/// Accepts only public `wss://` relays. A zap receipt must not open
/// WebSockets to loopback, link-local, or other private hosts.
fn public_zap_relay(url: &str) -> Option<RelayUrl> {
    let parsed = url::Url::parse(url).ok()?;
    if parsed.scheme() != "wss" {
        return None;
    }
    let host = parsed.host()?;
    if !host_is_public(&host) {
        return None;
    }
    RelayUrl::parse(url).ok()
}

fn host_is_public(host: &url::Host<&str>) -> bool {
    match host {
        url::Host::Domain(domain) => {
            let domain = domain.trim_end_matches('.');
            !domain.eq_ignore_ascii_case("localhost")
                && !domain.ends_with(".localhost")
                && !domain.ends_with(".local")
                && !domain.ends_with(".internal")
        }
        url::Host::Ipv4(ip) => {
            !(ip.is_loopback()
                || ip.is_private()
                || ip.is_link_local()
                || ip.is_multicast()
                || ip.is_unspecified()
                || ip.is_broadcast()
                || ip.octets()[0] == 0)
        }
        url::Host::Ipv6(ip) => {
            !(ip.is_loopback()
                || ip.is_multicast()
                || ip.is_unspecified()
                || ip.is_unique_local()
                || ip.is_unicast_link_local())
        }
    }
}

impl TwitterPublisher {
    async fn connect(config: &TwitterConfig) -> AppResult<Option<Self>> {
        if !config.enabled {
            return Ok(None);
        }
        Ok(Some(Self {
            client: reqwest12::Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .build()
                .map_err(|error| AppError::Upstream(error.to_string()))?,
            consumer_key: read_required(config.consumer_key_file.as_ref(), "Twitter consumer key")
                .await?,
            consumer_secret: read_required(
                config.consumer_secret_file.as_ref(),
                "Twitter consumer secret",
            )
            .await?,
            access_token: read_required(config.access_token_file.as_ref(), "Twitter access token")
                .await?,
            access_secret: read_required(
                config.access_secret_file.as_ref(),
                "Twitter access secret",
            )
            .await?,
            banned_words: config.banned_words.clone(),
        }))
    }

    async fn publish(&self, message: &str, link: &str) -> AppResult<String> {
        let text = format!("{}\n\n{link}", censor(message, &self.banned_words));
        let secrets = Secrets::new(&self.consumer_key, &self.consumer_secret)
            .token(&self.access_token, &self.access_secret);
        let response = self
            .client
            .clone()
            .oauth1(secrets)
            .post("https://api.twitter.com/2/tweets")
            .json(&serde_json::json!({ "text": text }))
            .send()
            .await
            .map_err(|error| AppError::Upstream(format!("Twitter request failed: {error}")))?
            .error_for_status()
            .map_err(|error| AppError::Upstream(format!("Twitter rejected tweet: {error}")))?
            .json::<TwitterCreateResponse>()
            .await
            .map_err(|error| {
                AppError::Upstream(format!("Twitter response was invalid: {error}"))
            })?;
        Ok(response.data.id)
    }
}

impl TelegramPublisher {
    async fn connect(config: &TelegramConfig) -> AppResult<Option<Self>> {
        if !config.enabled {
            return Ok(None);
        }
        Ok(Some(Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(40))
                .build()
                .map_err(|error| AppError::Upstream(error.to_string()))?,
            token: read_required(config.token_file.as_ref(), "Telegram bot token").await?,
            chat_id: config.admin_chat_id,
        }))
    }

    async fn publish(
        &self,
        record: &PaymentRecord,
        txid: &str,
        tweet_id: Option<&str>,
        nostr_note: Option<&str>,
        report: Option<&AccountingReport>,
    ) -> AppResult<()> {
        let link = format!("https://mempool.space/tx/{txid}");
        let user_result = if let Some(chat_id) = record.request.telegram_id {
            self.send_text(chat_id, &format!("OP_RETURN Created!\n\n{link}"))
                .await
        } else {
            Ok(())
        };
        let text = completion_notification(record, txid, tweet_id, nostr_note, report);
        let admin_result = self.send_text(self.chat_id, &text).await;
        user_result.and(admin_result)
    }

    async fn run(&self, payments: PaymentService, repository: Repository, public_url: url::Url) {
        let mut offset = match repository.service_state("telegram_update_offset").await {
            Ok(Some(value)) => value.parse::<i64>().unwrap_or_default(),
            Ok(None) => 0,
            Err(error) => {
                tracing::error!(%error, "could not read Telegram update offset");
                0
            }
        };
        loop {
            let updates = match self.get_updates(offset).await {
                Ok(updates) => updates,
                Err(error) => {
                    tracing::error!(%error, "Telegram update request failed");
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    continue;
                }
            };
            for update in updates {
                if let Some(message) = update.message {
                    self.handle_update(
                        update.update_id,
                        message,
                        &payments,
                        &repository,
                        &public_url,
                    )
                    .await;
                }
                // Always move past the update. A message that cannot be
                // handled must not block every later message.
                offset = update.update_id.saturating_add(1);
                if let Err(error) = repository
                    .set_service_state("telegram_update_offset", &offset.to_string())
                    .await
                {
                    tracing::error!(%error, "could not save Telegram update offset");
                }
            }
        }
    }

    async fn handle_update(
        &self,
        update_id: i64,
        message: TelegramMessage,
        payments: &PaymentService,
        repository: &Repository,
        public_url: &url::Url,
    ) {
        let chat_id = message.chat.id;
        let Err(error) = self
            .handle_message(update_id, message, payments, repository, public_url)
            .await
        else {
            return;
        };
        tracing::error!(%error, update_id, chat_id, "could not handle Telegram message");
        let notice = match &error {
            AppError::InvalidRequest(reason) | AppError::NotFound(reason) => reason.clone(),
            AppError::RateLimited => {
                "Too many requests. Please wait a minute and try again.".to_owned()
            }
            _ => "Something went wrong. Please try again later.".to_owned(),
        };
        if let Err(error) = self.send_text(chat_id, &notice).await {
            tracing::warn!(%error, chat_id, "could not send Telegram error notice");
        }
    }

    async fn get_updates(&self, offset: i64) -> AppResult<Vec<TelegramUpdate>> {
        let response: TelegramResponse<Vec<TelegramUpdate>> = self
            .client
            .get(format!(
                "https://api.telegram.org/bot{}/getUpdates",
                self.token
            ))
            .query(&serde_json::json!({ "offset": offset, "timeout": 30 }))
            .send()
            .await
            .map_err(|error| AppError::Upstream(format!("Telegram request failed: {error}")))?
            .error_for_status()
            .map_err(|error| AppError::Upstream(format!("Telegram rejected request: {error}")))?
            .json()
            .await
            .map_err(|error| {
                AppError::Upstream(format!("Telegram response was invalid: {error}"))
            })?;
        if !response.ok {
            return Err(AppError::Upstream(
                "Telegram returned an unsuccessful response".to_owned(),
            ));
        }
        Ok(response.result)
    }

    async fn handle_message(
        &self,
        update_id: i64,
        message: TelegramMessage,
        payments: &PaymentService,
        repository: &Repository,
        public_url: &url::Url,
    ) -> AppResult<()> {
        let Some(text) = message.text.as_deref() else {
            return Ok(());
        };
        let (command, argument) = text.split_once(' ').unwrap_or((text, ""));
        let command = command.split('@').next().unwrap_or(command);
        let is_admin = message
            .from
            .as_ref()
            .is_some_and(|user| user.id == self.chat_id);
        match command {
            "/create" if argument.is_empty() => {
                self.send_text(message.chat.id, "Usage: /create <message>")
                    .await
            }
            "/create" => {
                let state_key = format!("telegram_create_{update_id}");
                let invoice = if let Some(invoice) = repository.service_state(&state_key).await? {
                    invoice
                } else {
                    payments.check_create_limit(&format!("telegram:{}", message.chat.id))?;
                    let created = payments
                        .create_telegram_invoice(
                            &CreateRequest {
                                message: argument.as_bytes().to_vec(),
                                no_twitter: false,
                            },
                            message.chat.id,
                        )
                        .await?;
                    let invoice = created
                        .record
                        .invoice
                        .ok_or_else(|| {
                            AppError::Internal("created payment has no invoice".to_owned())
                        })?
                        .bolt11;
                    repository.set_service_state(&state_key, &invoice).await?;
                    invoice
                };
                self.send_text(message.chat.id, &invoice).await?;
                self.send_invoice_qr(message.chat.id, &invoice, public_url)
                    .await
            }
            "/report" => {
                if !is_admin {
                    return self
                        .send_text(message.chat.id, "You are not allowed to use this command!")
                        .await;
                }
                let report = repository
                    .accounting_report_since(report_start_time(argument))
                    .await?;
                self.send_text(
                    message.chat.id,
                    &format_report(&report, payments.mempool_limit()),
                )
                .await
            }
            "/processunhandled" => {
                if !is_admin {
                    return self
                        .send_text(message.chat.id, "You are not allowed to use this command!")
                        .await;
                }
                let (limit, lift_mempool_limit) = process_unhandled_arguments(argument);
                let updated = payments
                    .process_unhandled_requests(limit, lift_mempool_limit)
                    .await;
                self.send_text(message.chat.id, &format!("Updated {updated} requests"))
                    .await
            }
            "/rebroadcast" => {
                if !is_admin {
                    return self
                        .send_text(message.chat.id, "You are not allowed to use this command!")
                        .await;
                }
                payments.reconcile_once().await;
                self.send_text(message.chat.id, "rebroadcasted ancestors!")
                    .await
            }
            command if command.starts_with('/') => {
                self.send_text(message.chat.id, "Unknown command").await
            }
            _ => Ok(()),
        }
    }

    async fn send_text(&self, chat_id: i64, text: &str) -> AppResult<()> {
        self.client
            .post(format!(
                "https://api.telegram.org/bot{}/sendMessage",
                self.token
            ))
            .json(&serde_json::json!({ "chat_id": chat_id, "text": text }))
            .send()
            .await
            .map_err(|error| AppError::Upstream(format!("Telegram request failed: {error}")))?
            .error_for_status()
            .map_err(|error| AppError::Upstream(format!("Telegram rejected message: {error}")))?;
        Ok(())
    }

    async fn send_invoice_qr(
        &self,
        chat_id: i64,
        invoice: &str,
        public_url: &url::Url,
    ) -> AppResult<()> {
        let mut photo = public_url
            .join("qr")
            .map_err(|error| AppError::Config(format!("invalid public URL: {error}")))?;
        photo
            .query_pairs_mut()
            .append_pair("string", &format!("lightning:{invoice}"))
            .append_pair("width", "300")
            .append_pair("height", "300");
        self.client
            .post(format!(
                "https://api.telegram.org/bot{}/sendPhoto",
                self.token
            ))
            .json(&serde_json::json!({ "chat_id": chat_id, "photo": photo }))
            .send()
            .await
            .map_err(|error| AppError::Upstream(format!("Telegram request failed: {error}")))?
            .error_for_status()
            .map_err(|error| AppError::Upstream(format!("Telegram rejected photo: {error}")))?;
        Ok(())
    }
}

async fn read_required(path: Option<&std::path::PathBuf>, label: &str) -> AppResult<String> {
    let path =
        path.ok_or_else(|| AppError::Config(format!("{label} file is required when enabled")))?;
    read_secret(path, label).await
}

async fn read_secret(path: &std::path::Path, label: &str) -> AppResult<String> {
    tokio::fs::read_to_string(path)
        .await
        .map(|value| value.trim().to_owned())
        .map_err(|error| {
            AppError::Config(format!(
                "could not read {label} {}: {error}",
                path.display()
            ))
        })
}

fn completion_notification(
    record: &PaymentRecord,
    txid: &str,
    tweet_id: Option<&str>,
    nostr_note: Option<&str>,
    report: Option<&AccountingReport>,
) -> String {
    let request = &record.request;
    let message = truncate_chars(&request.message_text(), TELEGRAM_MESSAGE_CHARS);
    let delivery = if request.nostr_key.is_some() {
        "Nostr"
    } else if request.telegram_id.is_some() {
        "Telegram"
    } else if request.node_id.is_some() {
        "Lightning Onion Message"
    } else if record
        .on_chain
        .as_ref()
        .is_some_and(|payment| payment.txid.is_some())
    {
        "Web (On-chain)"
    } else {
        "Web (Lightning)"
    };
    let tweet = tweet_id.map_or_else(
        || "Hidden".to_owned(),
        |id| format!("https://x.com/OP_RETURN_Bot/status/{id}"),
    );
    let nostr = nostr_note.unwrap_or("Hidden");
    let amount_sats = payment_amount_sats(record).unwrap_or_default();
    let chain_fee_sats = request.chain_fee_sats.unwrap_or_default();
    let profit_sats = request
        .profit_sats
        .unwrap_or_else(|| amount_sats.saturating_sub(chain_fee_sats));
    let total_chain_fees = report.map_or(0, |report| report.chain_fees_sats);
    let total_profit = report.map_or(0, |report| report.profit_sats);
    let pending = report.map_or(0, |report| report.pending_requests);
    let non_standard = if request.message.len() > 80 {
        format!(
            "Non-standard output! {}\n",
            print_size(i64::try_from(request.message.len()).unwrap_or(i64::MAX))
        )
    } else {
        String::new()
    };
    let btc_price = if request.btc_price_cents > 0 {
        let profit_usd = format_fixed_ratio(
            i128::from(profit_sats) * i128::from(request.btc_price_cents),
            10_000_000_000,
            4,
        );
        let total_profit_usd = format_fixed_ratio(
            i128::from(total_profit) * i128::from(request.btc_price_cents),
            10_000_000_000,
            2,
        );
        format!(
            "BTC price: ${}\nprofit (USD): ${profit_usd}\ntotal profit (USD): ${total_profit_usd}\n",
            format_integer(request.btc_price_cents / 100)
        )
    } else {
        String::new()
    };

    format!(
        "🔔 🔔 NEW OP_RETURN 🔔 🔔\n\
         Message: {message}\n\
         Delivery: {delivery}\n\
         id: {}\n\
         tx: https://mempool.space/tx/{txid}\n\
         tweet: {tweet}\n\
         nostr: {nostr}\n\
         {non_standard}\
         fee rate: {} sats/vbyte\n\
         invoice amount: {}\n\
         tx fee: {}\n\
         profit: {}\n\
         {btc_price}\n\
         total chain fees: {}\n\
         total profit: {}\n\
         remaining in queue: {}",
        request.id,
        request.fee_rate_sat_vb,
        print_amount(amount_sats),
        print_amount(chain_fee_sats),
        print_amount(profit_sats),
        print_amount(total_chain_fees),
        print_amount(total_profit),
        format_integer(pending),
    )
}

fn payment_amount_sats(record: &PaymentRecord) -> Option<i64> {
    record
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
        })
}

fn format_report(report: &AccountingReport, mempool_limit: bool) -> String {
    let non_standard_percent = percent(report.non_standard_requests, report.completed_requests);
    let on_chain_percent = percent(report.on_chain_requests, report.completed_requests);
    format!(
        "Total OP_RETURNs: {}\n\
         Total Non-standard: {} ({non_standard_percent}%)\n\
         Paid On-Chain: {} ({on_chain_percent}%)\n\n\
         Total chain size: {}\n\
         Total non-std chain size: {}\n\
         Total chain fees: {}\n\
         Total profit: {}\n\n\
         Total NIP-05s: {}\n\
         Total Zapped: {}\n\n\
         Remaining in Queue: {}\n\
         Mempool limit: {mempool_limit}",
        format_integer(report.completed_requests),
        format_integer(report.non_standard_requests),
        format_integer(report.on_chain_requests),
        print_size(report.chain_vbytes),
        print_size(report.non_standard_vbytes),
        print_amount(report.chain_fees_sats),
        print_amount(report.profit_sats),
        format_integer(report.completed_nip5s),
        print_amount(report.zapped_sats),
        format_integer(report.pending_requests),
    )
}

fn report_start_time(argument: &str) -> Option<i64> {
    let argument = argument.trim();
    let (number, multiplier) = if let Some(number) = argument.strip_suffix("hr") {
        (number, 3_600_i64)
    } else if let Some(number) = argument.strip_suffix('h') {
        (number, 3_600)
    } else if let Some(number) = argument.strip_suffix('d') {
        (number, 86_400)
    } else if let Some(number) = argument.strip_suffix('w') {
        (number, 604_800)
    } else if let Some(number) = argument.strip_suffix('m') {
        (number, 2_629_800)
    } else {
        (argument.strip_suffix('y')?, 31_557_600)
    };
    let seconds = number.trim().parse::<i64>().ok()?.checked_mul(multiplier)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_secs()).ok())?;
    Some(now.saturating_sub(seconds))
}

fn process_unhandled_arguments(argument: &str) -> (Option<u32>, bool) {
    let pieces: Vec<&str> = argument.split_whitespace().collect();
    match pieces.as_slice() {
        [limit] => limit
            .parse::<u32>()
            .map_or((None, false), |limit| (Some(limit), false)),
        [limit, lift] => limit
            .parse::<u32>()
            .ok()
            .zip(lift.parse::<bool>().ok())
            .map_or((None, false), |(limit, lift)| (Some(limit), lift)),
        _ => (None, false),
    }
}

fn percent(value: i64, total: i64) -> String {
    if total == 0 {
        "0.00".to_owned()
    } else {
        format_fixed_ratio(i128::from(value) * 100, i128::from(total), 2)
    }
}

fn print_size(size: i64) -> String {
    if size < 1_000 {
        format!("{} vbytes", format_integer(size))
    } else if size < 1_000_000 {
        format!("{} vKB", format_fixed_ratio(i128::from(size), 1_000, 2))
    } else if size < 1_000_000_000 {
        format!("{} vMB", format_fixed_ratio(i128::from(size), 1_000_000, 2))
    } else {
        format!(
            "{} vGB",
            format_fixed_ratio(i128::from(size), 1_000_000_000, 2)
        )
    }
}

fn print_amount(amount_sats: i64) -> String {
    format!("{} sats", format_integer(amount_sats))
}

fn format_integer(value: i64) -> String {
    let mut grouped = group_digits(&value.unsigned_abs().to_string());
    if value.is_negative() {
        grouped.insert(0, '-');
    }
    grouped
}

fn format_fixed_ratio(numerator: i128, denominator: i128, decimal_places: u32) -> String {
    let denominator = denominator.unsigned_abs();
    let scale = 10_u128.pow(decimal_places);
    let scaled = numerator
        .unsigned_abs()
        .saturating_mul(scale)
        .saturating_add(denominator / 2)
        / denominator;
    let whole = scaled / scale;
    let fraction = scaled % scale;
    let sign = if numerator.is_negative() { "-" } else { "" };
    format!(
        "{sign}{}.{fraction:0width$}",
        group_digits(&whole.to_string()),
        width = decimal_places as usize,
    )
}

fn group_digits(digits: &str) -> String {
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, character) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(character);
    }
    grouped
}

fn censor(message: &str, banned_words: &[String]) -> String {
    banned_words.iter().fold(message.to_owned(), |text, word| {
        if word.is_empty() {
            text
        } else {
            text.replace(word, "*****")
        }
    })
}

/// Cuts `text` to at most `limit` characters and marks the cut.
fn truncate_chars(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    let mut truncated: String = text.chars().take(limit).collect();
    truncated.push_str("...");
    truncated
}

#[cfg(test)]
mod tests {
    use nostr_sdk::prelude::ToBech32;

    use super::*;

    #[test]
    fn zap_receipt_keeps_the_request_bytes() {
        let sender = Keys::generate();
        let recipient = Keys::generate();
        let request = EventBuilder::new(Kind::ZapRequest, "thanks")
            .tags([
                Tag::parse(["relays", "wss://relay.one"]).unwrap(),
                Tag::parse(["p", &recipient.public_key().to_hex()]).unwrap(),
                Tag::parse(["amount", "1000"]).unwrap(),
            ])
            .finalize(&sender)
            .unwrap();
        // The key order that nostr-tools produces differs from nostr-sdk.
        let raw = format!(
            r#"{{"kind":{},"created_at":{},"tags":{},"content":{},"pubkey":"{}","id":"{}","sig":"{}"}}"#,
            request.kind.as_u16(),
            request.created_at,
            serde_json::to_string(&request.tags).unwrap(),
            serde_json::to_string(&request.content).unwrap(),
            request.pubkey.to_hex(),
            request.id.to_hex(),
            request.sig,
        );
        let parsed = parse_zap_request(&raw).unwrap();
        assert_ne!(parsed.as_json(), raw);

        let receipt = zap_receipt_builder(&parsed, &raw, "lnbc1receipt", &[7; 32])
            .unwrap()
            .finalize(&recipient)
            .unwrap();
        let description = receipt
            .tags
            .iter()
            .find(|tag| tag.kind() == "description")
            .unwrap();
        assert_eq!(description.content(), Some(raw.as_str()));
        for kind in ["bolt11", "preimage", "p", "P"] {
            assert!(receipt.tags.iter().any(|tag| tag.kind() == kind), "{kind}");
        }
    }

    #[test]
    fn tags_the_nip5_buyer_or_a_public_key_message() {
        let key = Keys::generate().public_key();
        assert_eq!(tagged_public_key(Some(&key.to_hex()), "hello"), Some(key));
        assert_eq!(
            tagged_public_key(None, &key.to_bech32().unwrap()),
            Some(key)
        );
        assert_eq!(tagged_public_key(None, "hello"), None);
    }

    #[test]
    fn censors_literal_words_without_regex_behavior() {
        assert_eq!(censor("a.b and aXb", &["a.b".to_owned()]), "***** and aXb");
    }

    #[test]
    fn truncates_long_messages_on_character_boundaries() {
        assert_eq!(truncate_chars("héllo", 10), "héllo");
        assert_eq!(truncate_chars("héllo wörld", 5), "héllo...");
    }

    #[test]
    fn parses_legacy_admin_command_arguments() {
        assert_eq!(process_unhandled_arguments(""), (None, false));
        assert_eq!(process_unhandled_arguments("25"), (Some(25), false));
        assert_eq!(process_unhandled_arguments("25 true"), (Some(25), true));
        assert_eq!(process_unhandled_arguments("invalid"), (None, false));

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .cast_signed();
        let hour_ago = report_start_time("1hr").unwrap();
        assert!((3_599..=3_601).contains(&(now - hour_ago)));
        assert!(report_start_time("not-a-range").is_none());
    }

    #[test]
    fn formats_the_legacy_purchase_notification() {
        let record = PaymentRecord {
            request: crate::domain::OpReturnRequest {
                id: 42,
                message: b"hello".to_vec(),
                no_twitter: false,
                fee_rate_sat_vb: 12,
                node_id: None,
                telegram_id: Some(7),
                nostr_key: None,
                created_at: 1,
                transaction: None,
                txid: Some("ab".repeat(32)),
                profit_sats: Some(4_000),
                chain_fee_sats: Some(1_000),
                vsize: Some(120),
                closed: true,
                btc_price_cents: 10_000_000,
            },
            invoice: Some(crate::domain::Invoice {
                payment_hash: "hash".to_owned(),
                request_id: 42,
                bolt11: "invoice".to_owned(),
                paid: true,
                amount_sats: Some(5_000),
                lightning_backend: crate::domain::LightningBackend::Lnd,
                claim_preimage: None,
            }),
            on_chain: None,
        };
        let report = AccountingReport {
            completed_requests: 10,
            non_standard_requests: 1,
            on_chain_requests: 2,
            pending_requests: 3,
            profit_sats: 40_000,
            chain_fees_sats: 10_000,
            chain_vbytes: 2_000,
            non_standard_vbytes: 500,
            completed_nip5s: 4,
            zapped_sats: 21,
        };
        let notification = completion_notification(
            &record,
            "ab",
            Some("tweet"),
            Some("note1test"),
            Some(&report),
        );
        for expected in [
            "🔔 🔔 NEW OP_RETURN 🔔 🔔",
            "Message: hello",
            "Delivery: Telegram",
            "id: 42",
            "status/tweet",
            "nostr: note1test",
            "invoice amount: 5,000 sats",
            "total profit: 40,000 sats",
            "remaining in queue: 3",
        ] {
            assert!(notification.contains(expected), "missing {expected}");
        }

        let formatted_report = format_report(&report, true);
        assert!(formatted_report.contains("Total OP_RETURNs: 10"));
        assert!(formatted_report.contains("Total Non-standard: 1 (10.00%)"));
        assert!(formatted_report.contains("Mempool limit: true"));
    }

    #[test]
    fn reads_relays_from_a_zap_request() {
        let keys = Keys::generate();
        let event = EventBuilder::new(Kind::ZapRequest, "")
            .tags([
                Tag::parse(["relays", "wss://relay.one", "not a url", "wss://relay.two"]).unwrap(),
                Tag::parse(["p", &keys.public_key().to_hex()]).unwrap(),
            ])
            .finalize(&keys)
            .unwrap();
        let relays = zap_request_relays(&event);
        assert_eq!(relays.len(), 2);
        assert_eq!(relays[0].as_str(), "wss://relay.one");
        assert_eq!(relays[1].as_str(), "wss://relay.two");
    }

    #[test]
    fn rejects_private_zap_relays() {
        assert!(public_zap_relay("wss://relay.damus.io").is_some());
        assert!(public_zap_relay("ws://relay.damus.io").is_none());
        assert!(public_zap_relay("wss://127.0.0.1").is_none());
        assert!(public_zap_relay("wss://10.0.0.1").is_none());
        assert!(public_zap_relay("wss://169.254.169.254").is_none());
        assert!(public_zap_relay("wss://[::1]").is_none());
        assert!(public_zap_relay("wss://localhost").is_none());
    }
}
