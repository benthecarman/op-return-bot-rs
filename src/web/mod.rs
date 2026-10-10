use std::{io::Cursor, net::SocketAddr, str::FromStr, sync::OnceLock};

use askama::Template;
use axum::{
    Form, Json, Router,
    body::{Body, to_bytes},
    extract::{ConnectInfo, FromRequest, Multipart, Path, Query, State},
    http::{HeaderMap, HeaderValue, Request, StatusCode, header},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use image::{DynamicImage, ImageFormat, Luma};
use qrcode::QrCode;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tower_http::{
    catch_panic::CatchPanicLayer,
    cors::{AllowOrigin, CorsLayer},
    request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer},
    services::ServeDir,
    trace::TraceLayer,
};

use crate::{
    AppError, AppResult, AppState,
    domain::{OpReturnRequest, PaymentStatus},
    payment_service::{CreateRequest, CreatedPayment},
    rate_limit,
    repository::RecentRequest,
};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateForm {
    message: String,
    #[serde(default)]
    no_twitter: bool,
}

#[derive(Deserialize)]
struct InvoiceQuery {
    invoice: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SuccessQuery {
    r_hash: String,
}

/// Width and height fall back to 300 pixels when missing or not numeric.
#[derive(Deserialize)]
struct QrQuery {
    string: String,
    width: Option<String>,
    height: Option<String>,
}

const DEFAULT_QR_SIZE: u32 = 300;
/// One satoshi. LNURL-pay amounts are millisatoshis.
const LNURL_MIN_SENDABLE_MSATS: u64 = 1_000;
/// Two million satoshis. Donations and zaps do not need a 100 BTC ceiling.
const LNURL_MAX_SENDABLE_MSATS: u64 = 2_000_000_000;

fn qr_dimension(value: Option<&str>) -> u32 {
    value
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(DEFAULT_QR_SIZE)
}

#[derive(Deserialize)]
struct RecentQuery {
    before: Option<i64>,
    limit: Option<u32>,
}

/// Tiles in the first batch of the home page, and the default page size.
const RECENT_PAGE: u32 = 24;
const RECENT_MAX_PAGE: u32 = 100;
/// A tile shows at most this much of a message.
const RECENT_PREVIEW_CHARS: usize = 200;
const RECENT_PREVIEW_HEX_BYTES: usize = 48;

/// One published message in the recent strip. A message that is not UTF-8
/// text is shown as hex.
#[derive(Serialize)]
struct RecentTile {
    id: i64,
    txid: String,
    time: i64,
    bytes: usize,
    message: Option<String>,
    hex: Option<String>,
    #[serde(skip)]
    ago: String,
}

impl RecentTile {
    fn new(recent: RecentRequest, now: i64) -> Self {
        let preview = MessagePreview::new(&recent.message);
        Self {
            id: recent.id,
            ago: time_ago(now, recent.created_at),
            txid: recent.txid,
            time: recent.created_at,
            bytes: preview.bytes,
            message: preview.text,
            hex: preview.hex,
        }
    }

    fn short_txid(&self) -> &str {
        self.txid.get(..8).unwrap_or(&self.txid)
    }
}

/// The start of a message as text, or as hex when it is not UTF-8.
struct MessagePreview {
    text: Option<String>,
    hex: Option<String>,
    bytes: usize,
}

impl MessagePreview {
    fn new(message: &[u8]) -> Self {
        let (text, hex) = match std::str::from_utf8(message) {
            Ok(text) => (
                Some(text.chars().take(RECENT_PREVIEW_CHARS).collect()),
                None,
            ),
            Err(_) => (
                None,
                Some(hex::encode(
                    &message[..message.len().min(RECENT_PREVIEW_HEX_BYTES)],
                )),
            ),
        };
        Self {
            text,
            hex,
            bytes: message.len(),
        }
    }
}

/// Matches `ago()` in `public/javascripts/home.js`.
fn time_ago(now: i64, then: i64) -> String {
    let seconds = now.saturating_sub(then).max(0);
    if seconds < 3_600 {
        format!("{} min ago", (seconds / 60).max(1))
    } else if seconds < 86_400 {
        let hours = seconds / 3_600;
        format!("{hours} hour{} ago", if hours == 1 { "" } else { "s" })
    } else {
        let days = seconds / 86_400;
        format!("{days} day{} ago", if days == 1 { "" } else { "s" })
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        })
}

async fn recent_tiles(
    state: &AppState,
    before: Option<i64>,
    limit: u32,
) -> AppResult<Vec<RecentTile>> {
    let now = unix_now();
    Ok(state
        .repository
        .recent_public_requests(before, limit)
        .await?
        .into_iter()
        .map(|recent| RecentTile::new(recent, now))
        .collect())
}

#[derive(Deserialize)]
struct WalletNotifyEvent {
    txid: String,
}

#[derive(Deserialize)]
struct Nip5Form {
    name: String,
    pubkey: String,
}

#[derive(Deserialize)]
struct Nip5Query {
    name: Option<String>,
}

#[derive(Deserialize)]
struct LnurlCallbackQuery {
    amount: Option<u64>,
    nostr: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct UnifiedResponse {
    address: String,
    invoice: String,
    amount_btc: String,
    r_hash: String,
    payment_string: String,
    offer: Option<String>,
}

#[derive(Template)]
#[template(path = "index.html")]
struct IndexTemplate<'a> {
    onion_url: &'a str,
    recent: &'a [RecentTile],
    recent_page: u32,
    error: &'a str,
    message: &'a str,
    /// The upload tab stays selected when a file was rejected.
    file_mode: bool,
}

#[derive(Template)]
#[template(path = "invoice.html")]
struct InvoiceTemplate<'a> {
    onion_url: &'a str,
    message: &'a str,
    /// The payload is not UTF-8, so `message` describes the bytes.
    binary: bool,
    message_hash: String,
    invoice: &'a str,
    payment_hash: &'a str,
    lightning_uri: String,
    unified: Option<UnifiedView>,
}

/// The payment options of a request that accepts Lightning or on-chain.
struct UnifiedView {
    uri: String,
    address: String,
    amount_btc: String,
    on_chain_uri: String,
    /// Requests made before BOLT12 support have no offer.
    offer: Option<String>,
}

#[derive(Template)]
#[template(path = "success.html")]
struct SuccessTemplate<'a> {
    onion_url: &'a str,
    txid: &'a str,
    preview: MessagePreview,
}

#[derive(Template)]
#[template(path = "pending.html")]
struct PendingTemplate<'a> {
    onion_url: &'a str,
    payment_hash: &'a str,
    preview: MessagePreview,
    at_capacity: bool,
}

#[derive(Template)]
#[template(path = "connect.html")]
struct ConnectTemplate<'a> {
    onion_url: &'a str,
    node_uri: &'a str,
}

#[derive(Template)]
#[template(path = "not_found.html")]
struct NotFoundTemplate<'a> {
    onion_url: &'a str,
}

#[derive(Template)]
#[template(path = "nip5.html")]
struct Nip5Template<'a> {
    onion_url: &'a str,
    error: &'a str,
    name: &'a str,
    pubkey: &'a str,
}

/// A short hash of the files under `public/`. Asset URLs carry it, so that
/// browsers fetch new assets after a deploy. The Nix store dates every file
/// to 1970, which otherwise lets browsers cache old assets for years.
pub fn asset_version() -> &'static str {
    static VERSION: OnceLock<String> = OnceLock::new();
    VERSION.get_or_init(|| {
        let mut hasher = Sha256::new();
        hash_directory(std::path::Path::new("public"), &mut hasher);
        hex::encode(&hasher.finalize()[..6])
    })
}

fn hash_directory(directory: &std::path::Path, hasher: &mut Sha256) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    let mut paths = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .collect::<Vec<_>>();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            hash_directory(&path, hasher);
        } else if let Ok(bytes) = std::fs::read(&path) {
            hasher.update(path.to_string_lossy().as_bytes());
            hasher.update(&bytes);
        }
    }
}

pub fn router(state: AppState) -> Router {
    // Hash the assets now rather than on the first page request.
    asset_version();
    let request_id_header = http::HeaderName::from_static("x-request-id");
    let cors = cors_layer(&state);

    // Only the machine-facing routes allow cross-origin calls. HTML pages and
    // the wallet notification endpoint keep the browser default.
    let api = Router::new()
        .route("/.well-known/nostr.json", get(nip5_lookup))
        .route("/.well-known/lnurlp/{*user}", get(lnurl_pay_info))
        .route("/lnurlp/{meta}", get(lnurl_pay_callback))
        .route("/api/create", post(api_create))
        .route("/api/unified", post(api_unified))
        .route("/api/status/{r_hash}", get(api_status))
        .route("/api/view/{txid}", get(api_view))
        .route("/api/recent", get(api_recent))
        .route("/api/mempool-limit", get(api_mempool_limit))
        .route("/.well-known/mcp.json", get(mcp_discovery))
        .nest_service("/mcp", crate::mcp::service(state.clone()))
        .layer(cors);

    Router::new()
        .route("/", get(index))
        .route("/createRequest", post(create_request))
        .route("/nip5", get(nip5_page))
        .route("/createNip5Request", post(create_nip5_request))
        .route("/invoice", get(invoice))
        .route("/success", get(success))
        .route("/connect", get(connect))
        .route("/qr", get(qr))
        .route("/admin/walletnotify", post(wallet_notify))
        .route("/sitemap.xml", get(sitemap))
        .route("/auth.md", get(auth_markdown))
        .route("/.well-known/api-catalog", get(api_catalog))
        .route(
            "/.well-known/oauth-protected-resource",
            get(oauth_protected_resource),
        )
        .route(
            "/.well-known/agent-skills/index.json",
            get(agent_skills_index),
        )
        .route(
            "/.well-known/agent-skills/{name}/SKILL.md",
            get(agent_skill),
        )
        .nest_service("/assets", ServeDir::new("public"))
        .merge(api)
        .fallback(not_found)
        .with_state(state)
        .layer(axum::middleware::from_fn(security_headers))
        .layer(PropagateRequestIdLayer::new(request_id_header.clone()))
        .layer(SetRequestIdLayer::new(request_id_header, MakeRequestUuid))
        .layer(TraceLayer::new_for_http())
        .layer(CatchPanicLayer::new())
}

async fn index(State(state): State<AppState>, headers: HeaderMap) -> AppResult<Response> {
    let accepts = headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let mut response = if accepts.contains("text/markdown") && !accepts.contains("text/html") {
        (
            [(header::CONTENT_TYPE, "text/markdown; charset=utf-8")],
            crate::agent_content::INDEX_MD,
        )
            .into_response()
    } else {
        render_index(&state, "", "", false).await?.into_response()
    };
    response.headers_mut().insert(
        "onion-location",
        HeaderValue::from_str(state.config.server.onion_url.as_str())
            .map_err(|error| AppError::Config(format!("invalid onion URL header: {error}")))?,
    );
    response.headers_mut().insert(
        header::LINK,
        HeaderValue::from_static(
            "</.well-known/mcp.json>; rel=\"service-desc\", \
             <https://github.com/benthecarman/OP-RETURN-Bot/blob/master/docs/API.md>; \
             rel=\"service-doc\", </.well-known/api-catalog>; rel=\"api-catalog\"",
        ),
    );
    response
        .headers_mut()
        .insert(header::VARY, HeaderValue::from_static("Accept"));
    Ok(response)
}

async fn render_index(
    state: &AppState,
    error: &str,
    message: &str,
    file_mode: bool,
) -> AppResult<Html<String>> {
    let recent = recent_tiles(state, None, RECENT_PAGE).await?;
    render(IndexTemplate {
        onion_url: state.config.server.onion_url.as_str(),
        recent: &recent,
        recent_page: RECENT_PAGE,
        error,
        message,
        file_mode,
    })
}

async fn create_request(
    State(state): State<AppState>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    request: Request<Body>,
) -> Response {
    if let Err(error) = check_create_limit(&state, &headers, Some(peer)) {
        return error.into_response();
    }
    let incoming = match parse_create_request(request).await {
        Ok(incoming) => incoming,
        Err(error) => {
            return match render_index(&state, &error.to_string(), "", false).await {
                Ok(html) => (StatusCode::BAD_REQUEST, html).into_response(),
                Err(render_error) => render_error.into_response(),
            };
        }
    };
    let from_file = incoming.from_file;
    if let Err(error) = state
        .social
        .screen_file(
            from_file,
            &incoming.message,
            state.config.payments.message_max_bytes,
        )
        .await
    {
        return screen_error_page(&state, error).await;
    }
    let input = incoming.into_request();
    // A binary file cannot go back into the text box.
    let echo = std::str::from_utf8(&input.message).unwrap_or("");
    match state.payments.create_unified(&input).await {
        Ok(created) => Redirect::to(&format!(
            "/invoice?invoice={}",
            created
                .record
                .invoice
                .as_ref()
                .map_or("", |row| &row.payment_hash)
        ))
        .into_response(),
        Err(error) => match render_index(&state, &error.to_string(), echo, from_file).await {
            Ok(html) => (StatusCode::BAD_REQUEST, html).into_response(),
            Err(render_error) => render_error.into_response(),
        },
    }
}

async fn nip5_page(State(state): State<AppState>) -> AppResult<Html<String>> {
    render(Nip5Template {
        onion_url: state.config.server.onion_url.as_str(),
        error: "",
        name: "",
        pubkey: "",
    })
}

async fn create_nip5_request(
    State(state): State<AppState>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    Form(form): Form<Nip5Form>,
) -> Response {
    if let Err(error) = check_create_limit(&state, &headers, Some(peer)) {
        return error.into_response();
    }
    match state.payments.create_nip5(&form.name, &form.pubkey).await {
        Ok(created) => Redirect::to(&format!(
            "/invoice?invoice={}",
            created
                .record
                .invoice
                .as_ref()
                .map_or("", |row| &row.payment_hash)
        ))
        .into_response(),
        Err(error) => match render(Nip5Template {
            onion_url: state.config.server.onion_url.as_str(),
            error: &error.to_string(),
            name: &form.name,
            pubkey: &form.pubkey,
        }) {
            Ok(html) => (StatusCode::BAD_REQUEST, html).into_response(),
            Err(render_error) => render_error.into_response(),
        },
    }
}

async fn nip5_lookup(
    State(state): State<AppState>,
    Query(query): Query<Nip5Query>,
) -> AppResult<Json<serde_json::Value>> {
    let mut names = serde_json::Map::new();
    let mut found = false;
    if let Some(name) = &query.name
        && let Some(public_key) = state.repository.completed_nip5_public_key(name).await?
    {
        names.insert(name.clone(), serde_json::Value::String(public_key));
        found = true;
    }
    if !found && let Some(public_key) = state.social.nostr_public_key() {
        for name in [
            "_",
            "me",
            "opreturnbot",
            "op_return_bot",
            "OP_RETURN bot",
            "OP_RETURN Bot",
        ] {
            names.insert(
                name.to_owned(),
                serde_json::Value::String(public_key.clone()),
            );
        }
    }
    Ok(Json(serde_json::json!({ "names": names })))
}

async fn lnurl_pay_info(
    State(state): State<AppState>,
    Path(user): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    let domain = state
        .config
        .server
        .public_url
        .host_str()
        .unwrap_or("opreturnbot.com");
    let metadata = serde_json::to_string(&serde_json::json!([
        ["text/plain", "A donation to ben!"],
        ["text/identifier", format!("{user}@{domain}")]
    ]))
    .map_err(|error| AppError::Internal(format!("could not encode LNURL metadata: {error}")))?;
    let hash = hex::encode(Sha256::digest(metadata.as_bytes()));
    let mut callback = state
        .config
        .server
        .public_url
        .join(&format!("lnurlp/{hash}"))
        .map_err(|error| AppError::Config(format!("invalid public URL: {error}")))?;
    callback.query_pairs_mut().append_pair("user", &user);
    Ok(Json(serde_json::json!({
        "callback": callback,
        "maxSendable": LNURL_MAX_SENDABLE_MSATS,
        "minSendable": LNURL_MIN_SENDABLE_MSATS,
        "metadata": metadata,
        "nostrPubkey": state.social.nostr_public_key(),
        "allowsNostr": state.social.nostr_public_key().is_some()
    })))
}

async fn lnurl_pay_callback(
    State(state): State<AppState>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    Path(meta): Path<String>,
    Query(query): Query<LnurlCallbackQuery>,
) -> Response {
    if let Err(error) = check_create_limit(&state, &headers, Some(peer)) {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(serde_json::json!({ "status": "ERROR", "reason": error.to_string() })),
        )
            .into_response();
    }
    match create_lnurl_invoice(&state, &meta, query).await {
        Ok(invoice) => Json(serde_json::json!({ "pr": invoice, "routes": [] })).into_response(),
        Err(error) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "status": "ERROR", "reason": error.to_string() })),
        )
            .into_response(),
    }
}

async fn create_lnurl_invoice(
    state: &AppState,
    meta: &str,
    query: LnurlCallbackQuery,
) -> AppResult<String> {
    let amount = query
        .amount
        .filter(|amount| (LNURL_MIN_SENDABLE_MSATS..=LNURL_MAX_SENDABLE_MSATS).contains(amount))
        .ok_or_else(|| {
            AppError::InvalidRequest(format!(
                "amount must be between {LNURL_MIN_SENDABLE_MSATS} and {LNURL_MAX_SENDABLE_MSATS} millisatoshis"
            ))
        })?;
    if let Some(request) = query.nostr {
        state.social.validate_zap_request(&request, amount)?;
        let invoice = state
            .payments
            .create_zap_invoice(amount, &request, 86_400)
            .await?;
        let recipient = state.social.nostr_public_key().ok_or_else(|| {
            AppError::Config("Nostr is required to accept zap requests".to_owned())
        })?;
        state
            .payments
            .save_zap(&invoice, amount, &request, &recipient)
            .await?;
        return Ok(invoice.bolt11);
    }
    // LUD-06: the invoice must commit to the metadata through its
    // description hash, which is the hash in the callback path.
    let description_hash = parse_metadata_hash(meta)?;
    let invoice = state
        .payments
        .create_invoice_for_hash(amount, description_hash, 86_400)
        .await?;
    Ok(invoice.bolt11)
}

fn parse_metadata_hash(meta: &str) -> AppResult<[u8; 32]> {
    hex::decode(meta)
        .ok()
        .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok())
        .ok_or_else(|| {
            AppError::InvalidRequest("callback path must contain the metadata hash".to_owned())
        })
}

async fn api_create(
    State(state): State<AppState>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    request: Request<Body>,
) -> AppResult<String> {
    check_create_limit(&state, &headers, Some(peer))?;
    let form = parse_create_request(request).await?;
    state
        .social
        .screen_file(
            form.from_file,
            &form.message,
            state.config.payments.message_max_bytes,
        )
        .await?;
    let created = state.payments.create_invoice(&form.into_request()).await?;
    Ok(created
        .record
        .invoice
        .ok_or_else(|| AppError::Internal("created payment has no invoice".to_owned()))?
        .bolt11)
}

async fn api_unified(
    State(state): State<AppState>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    request: Request<Body>,
) -> AppResult<Json<UnifiedResponse>> {
    check_create_limit(&state, &headers, Some(peer))?;
    let form = parse_create_request(request).await?;
    state
        .social
        .screen_file(
            form.from_file,
            &form.message,
            state.config.payments.message_max_bytes,
        )
        .await?;
    let created = state.payments.create_unified(&form.into_request()).await?;
    Ok(Json(unified_response(&created)?))
}

async fn invoice(
    State(state): State<AppState>,
    Query(query): Query<InvoiceQuery>,
) -> AppResult<Response> {
    let record = state
        .repository
        .find_by_invoice_identifier(&query.invoice)
        .await?;
    let invoice = record
        .invoice
        .as_ref()
        .ok_or_else(|| AppError::Internal("payment record has no invoice".to_owned()))?;
    if record.request.txid.is_some() {
        return Ok(
            Redirect::to(&format!("/success?rHash={}", invoice.payment_hash)).into_response(),
        );
    }
    let on_chain_txid = record
        .on_chain
        .as_ref()
        .and_then(|payment| payment.txid.as_deref());
    if record.request.payment_status(invoice.paid, on_chain_txid) == PaymentStatus::Pending {
        return pending_page(&state, &invoice.payment_hash, &record.request);
    }
    let offer = match &record.on_chain {
        Some(_) => state
            .repository
            .find_offer(record.request.id)
            .await?
            .map(|offer| offer.offer),
        None => None,
    };
    let unified = record
        .on_chain
        .as_ref()
        .map(|on_chain| -> AppResult<UnifiedView> {
            let amount_btc = sats_to_btc(on_chain.expected_amount_sats)?;
            Ok(UnifiedView {
                uri: unified_payment_string(
                    &on_chain.address,
                    on_chain.expected_amount_sats,
                    &invoice.bolt11,
                ),
                // The address keeps its case so that /qr can look it up.
                on_chain_uri: format!("bitcoin:{}?amount={amount_btc}", on_chain.address),
                address: on_chain.address.clone(),
                amount_btc,
                offer: offer.clone(),
            })
        })
        .transpose()?;
    // Hash the stored bytes. A file that is not text is described on the
    // page, and that description is not the payload.
    let message_hash = hex::encode(Sha256::digest(&record.request.message));
    let (message, binary) = message_for_page(&record.request.message);
    let page = InvoiceTemplate {
        onion_url: state.config.server.onion_url.as_str(),
        message: &message,
        binary,
        message_hash,
        invoice: &invoice.bolt11,
        payment_hash: &invoice.payment_hash,
        lightning_uri: format!("lightning:{}", invoice.bolt11),
        unified,
    };
    Ok(render(page)?.into_response())
}

async fn success(
    State(state): State<AppState>,
    Query(query): Query<SuccessQuery>,
) -> AppResult<Response> {
    let record = match state.repository.find_by_payment_hash(&query.r_hash).await {
        Ok(record) => record,
        Err(AppError::NotFound(_)) => return bad_request_index(&state).await,
        Err(error) => return Err(error),
    };
    if let Some(txid) = record.request.txid.as_deref() {
        return Ok(render(SuccessTemplate {
            onion_url: state.config.server.onion_url.as_str(),
            txid,
            preview: MessagePreview::new(&record.request.message),
        })?
        .into_response());
    }
    let invoice_paid = record.invoice.as_ref().is_some_and(|invoice| invoice.paid);
    let on_chain_txid = record
        .on_chain
        .as_ref()
        .and_then(|payment| payment.txid.as_deref());
    if record.request.payment_status(invoice_paid, on_chain_txid) == PaymentStatus::Pending {
        let payment_hash = record
            .invoice
            .as_ref()
            .map_or(query.r_hash.as_str(), |invoice| &invoice.payment_hash);
        return pending_page(&state, payment_hash, &record.request);
    }
    // Unpaid: show the home page with status 400, as the Scala service did.
    bad_request_index(&state).await
}

/// The page shown after payment while the transaction is being broadcast.
fn pending_page(
    state: &AppState,
    payment_hash: &str,
    request: &OpReturnRequest,
) -> AppResult<Response> {
    Ok(render(PendingTemplate {
        onion_url: state.config.server.onion_url.as_str(),
        payment_hash,
        preview: MessagePreview::new(&request.message),
        at_capacity: state.payments.mempool_limit(),
    })?
    .into_response())
}

async fn bad_request_index(state: &AppState) -> AppResult<Response> {
    Ok((
        StatusCode::BAD_REQUEST,
        render_index(state, "", "", false).await?,
    )
        .into_response())
}

async fn api_status(State(state): State<AppState>, Path(identifier): Path<String>) -> Response {
    match state
        .repository
        .find_by_invoice_identifier(&identifier)
        .await
    {
        Err(AppError::NotFound(_)) => {
            (StatusCode::BAD_REQUEST, "Invoice not from OP_RETURN Bot").into_response()
        }
        Err(error) => error.into_response(),
        Ok(record) => {
            if let Some(txid) = record.request.txid {
                (StatusCode::OK, txid).into_response()
            } else if record.invoice.is_some_and(|row| row.paid)
                || record.on_chain.is_some_and(|row| row.txid.is_some())
            {
                (StatusCode::OK, "null").into_response()
            } else {
                (StatusCode::BAD_REQUEST, "Invoice has not been paid").into_response()
            }
        }
    }
}

async fn api_view(State(state): State<AppState>, Path(txid): Path<String>) -> Response {
    match state.repository.find_by_txid(&txid).await {
        Ok(request) => (StatusCode::OK, request.message_text()).into_response(),
        Err(AppError::NotFound(_)) => (
            StatusCode::BAD_REQUEST,
            "Tx does not originate from OP_RETURN Bot",
        )
            .into_response(),
        Err(error) => error.into_response(),
    }
}

async fn api_recent(
    State(state): State<AppState>,
    Query(query): Query<RecentQuery>,
) -> AppResult<Json<Vec<RecentTile>>> {
    let limit = query.limit.unwrap_or(RECENT_PAGE).clamp(1, RECENT_MAX_PAGE);
    Ok(Json(recent_tiles(&state, query.before, limit).await?))
}

async fn api_mempool_limit(State(state): State<AppState>) -> &'static str {
    if state.payments.mempool_limit() {
        "true"
    } else {
        "false"
    }
}

async fn connect(State(state): State<AppState>) -> AppResult<Html<String>> {
    let node_uri = state.payments.node_uri().await?;
    render(ConnectTemplate {
        onion_url: state.config.server.onion_url.as_str(),
        node_uri: &node_uri,
    })
}

async fn qr(State(state): State<AppState>, Query(query): Query<QrQuery>) -> AppResult<Response> {
    let width = qr_dimension(query.width.as_deref());
    let height = qr_dimension(query.height.as_deref());
    if width == 0 || height == 0 || width > 1_000 || height > 1_000 {
        return Err(AppError::InvalidRequest(
            "QR dimensions must be between 1 and 1000 pixels".to_owned(),
        ));
    }
    if !known_qr_payload(&state, &query.string).await? {
        return Err(AppError::InvalidRequest(
            "QR string is not a payment from this service".to_owned(),
        ));
    }
    let code = QrCode::new(query.string.as_bytes())
        .map_err(|error| AppError::InvalidRequest(format!("could not encode QR code: {error}")))?;
    let image = code
        .render::<Luma<u8>>()
        .min_dimensions(width, height)
        .max_dimensions(width, height)
        .build();
    let mut png = Cursor::new(Vec::new());
    DynamicImage::ImageLuma8(image)
        .write_to(&mut png, ImageFormat::Png)
        .map_err(|error| AppError::Internal(format!("could not render QR code: {error}")))?;
    Ok((
        [(header::CONTENT_TYPE, "image/png")],
        Body::from(png.into_inner()),
    )
        .into_response())
}

async fn wallet_notify(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(event): Json<WalletNotifyEvent>,
) -> AppResult<Response> {
    if !rate_limit::is_loopback(peer.ip()) {
        return Ok((StatusCode::UNAUTHORIZED, "Unauthorized").into_response());
    }
    let expected = tokio::fs::read(&state.config.bitcoin.wallet_notify_key_file)
        .await
        .map_err(|error| {
            AppError::Config(format!(
                "could not read wallet notification key {}: {error}",
                state.config.bitcoin.wallet_notify_key_file.display()
            ))
        })?;
    let expected = trim_ascii(&expected);
    if expected.is_empty() {
        return Err(AppError::Config(
            "the wallet notification key file is empty".to_owned(),
        ));
    }
    let supplied = headers
        .get("x-wallet-notify-key")
        .map(HeaderValue::as_bytes)
        .unwrap_or_default();
    if !constant_time_eq(expected, supplied) {
        return Ok((StatusCode::UNAUTHORIZED, "Unauthorized").into_response());
    }
    let txid = event.txid.trim().to_owned();
    if bitcoin::Txid::from_str(&txid).is_err() {
        return Err(AppError::InvalidRequest("txid is not valid".to_owned()));
    }
    // Reply at once, as the Scala service did. The bitcoind notify script
    // must not wait for signing, broadcast, and social publishing.
    let payments = state.payments.clone();
    tokio::spawn(async move {
        match payments.process_wallet_transaction(&txid).await {
            Ok(processed) => {
                tracing::info!(%txid, processed, "processed wallet notification");
            }
            Err(error) => {
                tracing::error!(%error, %txid, "could not process wallet notification");
            }
        }
    });
    Ok((StatusCode::OK, "OK").into_response())
}

async fn mcp_discovery(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "name": "OP_RETURN Bot",
        "serverInfo": {"name": "OP_RETURN Bot", "version": env!("CARGO_PKG_VERSION")},
        "description": "Write messages to the Bitcoin blockchain via OP_RETURN outputs",
        "url": state.config.server.public_url.join("mcp").map_or_else(
            |_| "/mcp".to_owned(),
            |url| url.to_string()
        ),
        "transport": {"type": "streamable-http", "url": "/mcp"}
    }))
}

async fn sitemap(State(state): State<AppState>) -> Response {
    let base = state
        .config
        .server
        .public_url
        .as_str()
        .trim_end_matches('/');
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">\n\
         <url><loc>{base}/</loc></url>\n\
         <url><loc>{base}/nip5</loc></url>\n\
         <url><loc>{base}/connect</loc></url>\n\
         </urlset>\n"
    );
    (
        [(header::CONTENT_TYPE, "application/xml; charset=utf-8")],
        body,
    )
        .into_response()
}

async fn auth_markdown() -> Response {
    (
        [(header::CONTENT_TYPE, "text/markdown; charset=utf-8")],
        crate::agent_content::AUTH_MD,
    )
        .into_response()
}

async fn api_catalog(State(state): State<AppState>) -> Response {
    let base = state
        .config
        .server
        .public_url
        .as_str()
        .trim_end_matches('/');
    (
        [(header::CONTENT_TYPE, "application/linkset+json")],
        Json(serde_json::json!({
            "linkset": [{
                "anchor": format!("{base}/"),
                "service-desc": [{
                    "href": format!("{base}/.well-known/mcp.json"),
                    "type": "application/json"
                }],
                "service-doc": [{
                    "href": "https://github.com/benthecarman/OP-RETURN-Bot/blob/master/docs/API.md",
                    "type": "text/markdown"
                }]
            }]
        })),
    )
        .into_response()
}

async fn oauth_protected_resource(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "resource": state.config.server.public_url,
        "resource_name": "OP_RETURN Bot",
        "authorization_servers": []
    }))
}

async fn agent_skills_index() -> Json<serde_json::Value> {
    let digest = hex::encode(Sha256::digest(crate::agent_content::SKILL_MD.as_bytes()));
    Json(serde_json::json!({
        "$schema": "https://schemas.agentskills.io/discovery/0.2.0/schema.json",
        "skills": [{
            "name": "op-return-bot",
            "type": "skill-md",
            "description": "Write messages to Bitcoin OP_RETURN outputs through the OP_RETURN Bot REST API or MCP server.",
            "url": "/.well-known/agent-skills/op-return-bot/SKILL.md",
            "digest": format!("sha256:{digest}")
        }]
    }))
}

async fn agent_skill(Path(name): Path<String>) -> Response {
    if name != "op-return-bot" {
        return StatusCode::NOT_FOUND.into_response();
    }
    (
        [(header::CONTENT_TYPE, "text/markdown; charset=utf-8")],
        crate::agent_content::SKILL_MD,
    )
        .into_response()
}

async fn not_found(State(state): State<AppState>) -> Response {
    match render(NotFoundTemplate {
        onion_url: state.config.server.onion_url.as_str(),
    }) {
        Ok(html) => (StatusCode::NOT_FOUND, html).into_response(),
        Err(error) => error.into_response(),
    }
}

fn unified_response(created: &CreatedPayment) -> AppResult<UnifiedResponse> {
    let invoice = created
        .record
        .invoice
        .as_ref()
        .ok_or_else(|| AppError::Internal("created payment has no invoice".to_owned()))?;
    let on_chain =
        created.record.on_chain.as_ref().ok_or_else(|| {
            AppError::Internal("created payment has no on-chain address".to_owned())
        })?;
    Ok(UnifiedResponse {
        address: on_chain.address.clone(),
        invoice: invoice.bolt11.clone(),
        amount_btc: sats_to_btc(on_chain.expected_amount_sats)?,
        r_hash: invoice.payment_hash.clone(),
        payment_string: unified_payment_string(
            &on_chain.address,
            on_chain.expected_amount_sats,
            &invoice.bolt11,
        ),
        offer: created.offer.as_ref().map(|offer| offer.offer.clone()),
    })
}

fn unified_payment_string(address: &str, sats: i64, invoice: &str) -> String {
    format!(
        "bitcoin:{address}?amount={}&lightning={invoice}",
        sats_to_btc(sats).unwrap_or_else(|_| "0.00000000".to_owned())
    )
    .to_uppercase()
}

fn sats_to_btc(sats: i64) -> AppResult<String> {
    if sats < 0 {
        return Err(AppError::Internal("payment amount is negative".to_owned()));
    }
    Ok(format!("{}.{:08}", sats / 100_000_000, sats % 100_000_000))
}

async fn known_qr_payload(state: &AppState, payload: &str) -> AppResult<bool> {
    let found = match qr_payment_identifier(payload).ok_or_else(|| {
        AppError::InvalidRequest("QR string is not a payment from this service".to_owned())
    })? {
        QrPayment::Invoice(identifier) => {
            state
                .repository
                .find_by_invoice_identifier(&identifier)
                .await
        }
        QrPayment::Address(address) => state.repository.find_by_address(&address).await,
        QrPayment::Offer(offer) => {
            return Ok(state.repository.find_offer_by_text(&offer).await?.is_some());
        }
    };
    match found {
        Ok(_) => Ok(true),
        Err(AppError::NotFound(_)) => Ok(false),
        Err(error) => Err(error),
    }
}

#[derive(Debug, Eq, PartialEq)]
enum QrPayment {
    Invoice(String),
    Address(String),
    Offer(String),
}

fn qr_payment_identifier(payload: &str) -> Option<QrPayment> {
    let payload = payload.trim();
    let lower = payload.to_ascii_lowercase();
    if lower.starts_with("lno1") {
        return Some(QrPayment::Offer(lower));
    }
    if lower.starts_with("lnbc") || lower.starts_with("lnbcrt") {
        return Some(QrPayment::Invoice(lower));
    }
    if let Some(rest) = lower.strip_prefix("lightning:") {
        return Some(QrPayment::Invoice(rest.to_owned()));
    }
    if let Some(rest) = lower.strip_prefix("bitcoin:") {
        if let Some(invoice) = rest
            .split(['?', '&'])
            .find_map(|part| part.strip_prefix("lightning="))
        {
            return Some(QrPayment::Invoice(invoice.to_owned()));
        }
        // An on-chain-only URI. Keep the address as given: legacy addresses
        // are case-sensitive.
        let address = payload["bitcoin:".len()..].split('?').next()?;
        return (!address.is_empty()).then(|| QrPayment::Address(address.to_owned()));
    }
    None
}

async fn security_headers(request: Request<Body>, next: axum::middleware::Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    // The success page asks mempool.space when the transaction confirms.
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'self'; \
             script-src 'self' 'unsafe-inline'; \
             style-src 'self' 'unsafe-inline'; \
             img-src 'self' data:; \
             font-src 'self'; \
             connect-src 'self' https://mempool.space; \
             object-src 'none'; \
             base-uri 'self'; \
             form-action 'self'; \
             frame-ancestors 'none'",
        ),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        header::HeaderName::from_static("permissions-policy"),
        HeaderValue::from_static("camera=(), microphone=(), geolocation=()"),
    );
    response
}

fn cors_layer(state: &AppState) -> CorsLayer {
    let mut origins = vec![
        state
            .config
            .server
            .public_url
            .origin()
            .ascii_serialization(),
        state.config.server.onion_url.origin().ascii_serialization(),
    ];
    origins.sort();
    origins.dedup();
    let allowed = origins
        .into_iter()
        .filter_map(|origin| HeaderValue::from_str(&origin).ok())
        .collect::<Vec<_>>();
    CorsLayer::new()
        .allow_origin(AllowOrigin::list(allowed))
        .allow_methods([
            axum::http::Method::GET,
            axum::http::Method::POST,
            axum::http::Method::OPTIONS,
        ])
        .allow_headers([header::CONTENT_TYPE, header::ACCEPT])
}

/// A refused file stays on the file tab, and its bytes are not written back
/// into the form.
async fn screen_error_page(state: &AppState, error: AppError) -> Response {
    let status = match error {
        AppError::FileCheckFailed => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::BAD_REQUEST,
    };
    match render_index(state, &error.to_string(), "", true).await {
        Ok(html) => (status, html).into_response(),
        Err(render_error) => render_error.into_response(),
    }
}

fn check_create_limit(
    state: &AppState,
    headers: &HeaderMap,
    peer: Option<std::net::SocketAddr>,
) -> AppResult<()> {
    state.creates.check(&rate_limit::caller_key(headers, peer))
}

fn render(template: impl Template) -> AppResult<Html<String>> {
    template
        .render()
        .map(Html)
        .map_err(|error| AppError::Internal(format!("could not render page: {error}")))
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right.iter())
        .fold(0_u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

fn trim_ascii(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map_or(start, |index| index + 1);
    &bytes[start..end]
}

/// A create call before it becomes a payment request.
struct IncomingCreate {
    message: Vec<u8>,
    no_twitter: bool,
    /// The payload came from the file field.
    from_file: bool,
}

impl IncomingCreate {
    fn into_request(self) -> CreateRequest {
        CreateRequest {
            message: self.message,
            no_twitter: self.no_twitter,
        }
    }
}

async fn parse_create_request(request: Request<Body>) -> AppResult<IncomingCreate> {
    let content_type = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    if content_type.starts_with("multipart/form-data") {
        return parse_multipart_create(request).await;
    }
    let is_json = content_type.starts_with("application/json");
    let body = to_bytes(request.into_body(), 1_000_000)
        .await
        .map_err(|error| AppError::InvalidRequest(format!("could not read request: {error}")))?;
    let form: CreateForm = if is_json {
        serde_json::from_slice(&body)
            .map_err(|error| AppError::InvalidRequest(format!("invalid JSON request: {error}")))?
    } else {
        serde_urlencoded::from_bytes(&body)
            .map_err(|error| AppError::InvalidRequest(format!("invalid form request: {error}")))?
    };
    Ok(form.into_incoming())
}

impl CreateForm {
    fn into_incoming(self) -> IncomingCreate {
        IncomingCreate {
            message: self.message.into_bytes(),
            no_twitter: self.no_twitter,
            from_file: false,
        }
    }
}

/// Read a home-page upload. A non-empty `file` field is the payload. A text
/// `message` field is used when no file bytes were attached.
async fn parse_multipart_create(request: Request<Body>) -> AppResult<IncomingCreate> {
    let mut multipart = Multipart::from_request(request, &())
        .await
        .map_err(|error| AppError::InvalidRequest(format!("invalid upload: {error}")))?;
    let mut text: Option<Vec<u8>> = None;
    let mut file: Option<Vec<u8>> = None;
    let mut no_twitter = false;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|error| AppError::InvalidRequest(format!("invalid upload: {error}")))?
    {
        let name = field.name().unwrap_or("").to_owned();
        let data = field
            .bytes()
            .await
            .map_err(|error| AppError::InvalidRequest(format!("invalid upload: {error}")))?
            .to_vec();
        match name.as_str() {
            "file" => file = Some(data),
            "message" => text = Some(data),
            "noTwitter" => no_twitter = data != b"false" && !data.is_empty(),
            _ => {}
        }
    }
    // An empty file input is not a payload. Typed text then still wins.
    let file_has_bytes = file.as_ref().is_some_and(|bytes| !bytes.is_empty());
    let (message, from_file) = if file_has_bytes {
        (file.unwrap_or_default(), true)
    } else if let Some(text) = text {
        (text, false)
    } else {
        (Vec::new(), file.is_some())
    };
    // A file is never posted. The checkbox cannot turn that off.
    if from_file {
        no_twitter = true;
    }
    Ok(IncomingCreate {
        message,
        no_twitter,
        from_file,
    })
}

/// Text is shown as itself. Anything else is described by its size, because
/// the invoice page also prints the SHA-256 of the raw bytes.
fn message_for_page(message: &[u8]) -> (String, bool) {
    match std::str::from_utf8(message) {
        Ok(text) => (text.to_owned(), false),
        Err(_) => (format!("{} bytes", message.len()), true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn accepts_json_api_requests() {
        let request = Request::builder()
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"message":"hello","noTwitter":true}"#))
            .unwrap();
        let parsed = parse_create_request(request).await.unwrap();
        assert_eq!(parsed.message, b"hello");
        assert!(parsed.no_twitter);
        assert!(!parsed.from_file);
    }

    #[tokio::test]
    async fn accepts_form_api_requests() {
        let request = Request::builder()
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from("message=hello+world&noTwitter=false"))
            .unwrap();
        let parsed = parse_create_request(request).await.unwrap();
        assert_eq!(parsed.message, b"hello world");
        assert!(!parsed.no_twitter);
        assert!(!parsed.from_file);
    }

    #[tokio::test]
    async fn accepts_an_uploaded_file() {
        let body = b"\
--bound\r\n\
Content-Disposition: form-data; name=\"file\"; filename=\"note.bin\"\r\n\
Content-Type: application/octet-stream\r\n\
\r\n\
\xff\x00hello\r\n\
--bound\r\n\
Content-Disposition: form-data; name=\"noTwitter\"\r\n\
\r\n\
false\r\n\
--bound--\r\n";
        let request = Request::builder()
            .header(header::CONTENT_TYPE, "multipart/form-data; boundary=bound")
            .body(Body::from(body.as_slice()))
            .unwrap();
        let parsed = parse_create_request(request).await.unwrap();
        assert_eq!(parsed.message, b"\xff\x00hello");
        assert!(parsed.no_twitter);
        assert!(parsed.from_file);
    }

    #[tokio::test]
    async fn keeps_typed_text_when_the_file_field_is_empty() {
        let body = "\
--bound\r\n\
Content-Disposition: form-data; name=\"message\"\r\n\
\r\n\
hello\r\n\
--bound\r\n\
Content-Disposition: form-data; name=\"file\"; filename=\"\"\r\n\
\r\n\
\r\n\
--bound--\r\n";
        let request = Request::builder()
            .header(header::CONTENT_TYPE, "multipart/form-data; boundary=bound")
            .body(Body::from(body))
            .unwrap();
        let parsed = parse_create_request(request).await.unwrap();
        assert_eq!(parsed.message, b"hello");
        assert!(!parsed.no_twitter);
        assert!(!parsed.from_file);
    }

    #[test]
    fn home_page_offers_a_file_upload() {
        let typed = IndexTemplate {
            onion_url: "http://onion.example",
            recent: &[],
            recent_page: RECENT_PAGE,
            error: "",
            message: "kept",
            file_mode: false,
        }
        .render()
        .unwrap();
        assert!(typed.contains("name=\"file\""));
        assert!(typed.contains("multipart/form-data"));
        assert!(typed.contains("A file is not posted to Twitter or Nostr"));
        assert!(!typed.contains("id=\"noTwitterFile\""));
        assert!(typed.contains("id=\"source-text\" checked"));
        assert!(typed.contains("kept</textarea>"));
        assert!(!typed.contains("id=\"source-file\" checked"));

        let uploaded = IndexTemplate {
            onion_url: "http://onion.example",
            recent: &[],
            recent_page: RECENT_PAGE,
            error: "invalid request: message is too long",
            message: "",
            file_mode: true,
        }
        .render()
        .unwrap();
        assert!(uploaded.contains("id=\"source-file\" checked"));
        assert!(uploaded.contains("message is too long"));

        let refused = IndexTemplate {
            onion_url: "http://onion.example",
            recent: &[],
            recent_page: RECENT_PAGE,
            error: "This file cannot be published.",
            message: "",
            file_mode: true,
        }
        .render()
        .unwrap();
        assert!(refused.contains("This file cannot be published."));
        assert!(refused.contains("id=\"source-file\" checked"));
        assert!(refused.contains("></textarea>"));
    }

    #[test]
    fn invoice_page_describes_a_binary_payload() {
        let page = InvoiceTemplate {
            onion_url: "http://onion.example",
            message: "2 bytes",
            binary: true,
            message_hash: "abc".to_owned(),
            invoice: "lnbc1",
            payment_hash: "ff",
            lightning_uri: "lightning:lnbc1".to_owned(),
            unified: None,
        }
        .render()
        .unwrap();
        assert!(page.contains(">File</dt>"));
        assert!(page.contains("2 bytes"));
        assert!(page.contains("The SHA256 is of these bytes."));
        assert!(page.contains(">abc</dd>"));
    }

    #[test]
    fn describes_a_binary_payload_by_its_size() {
        let (text, binary) = message_for_page(b"hello");
        assert_eq!(text, "hello");
        assert!(!binary);
        let (text, binary) = message_for_page(&[0xff, 0x00]);
        assert_eq!(text, "2 bytes");
        assert!(binary);
        assert_ne!(
            hex::encode(Sha256::digest(text.as_bytes())),
            hex::encode(Sha256::digest([0xff, 0x00]))
        );
    }

    #[test]
    fn previews_text_and_binary_messages() {
        let text = RecentTile::new(
            RecentRequest {
                id: 7,
                txid: "ab".repeat(32),
                created_at: 1_000,
                message: "é".repeat(RECENT_PREVIEW_CHARS + 10).into_bytes(),
            },
            1_000,
        );
        assert_eq!(
            text.message
                .as_deref()
                .map(|message| message.chars().count()),
            Some(RECENT_PREVIEW_CHARS)
        );
        assert_eq!(text.bytes, (RECENT_PREVIEW_CHARS + 10) * 2);
        assert_eq!(text.hex, None);
        assert_eq!(text.short_txid(), "abababab");

        let binary = RecentTile::new(
            RecentRequest {
                id: 8,
                txid: "cd".repeat(32),
                created_at: 1_000,
                message: vec![0xff; RECENT_PREVIEW_HEX_BYTES + 1],
            },
            1_000,
        );
        assert_eq!(binary.message, None);
        assert_eq!(binary.hex, Some("ff".repeat(RECENT_PREVIEW_HEX_BYTES)));
    }

    #[test]
    fn describes_message_age() {
        assert_eq!(time_ago(1_000, 1_000), "1 min ago");
        assert_eq!(time_ago(10_000, 10_000 - 59 * 60), "59 min ago");
        assert_eq!(time_ago(10_000, 10_000 - 3_600), "1 hour ago");
        assert_eq!(time_ago(100_000, 100_000 - 7_200), "2 hours ago");
        assert_eq!(time_ago(1_000_000, 1_000_000 - 3 * 86_400), "3 days ago");
        // A clock that moved backwards still reads as just now.
        assert_eq!(time_ago(1_000, 2_000), "1 min ago");
    }

    #[test]
    fn defaults_qr_dimensions_like_the_scala_service() {
        assert_eq!(qr_dimension(None), 300);
        assert_eq!(qr_dimension(Some("abc")), 300);
        assert_eq!(qr_dimension(Some(" 450 ")), 450);
    }

    #[test]
    fn compares_wallet_keys_in_constant_time() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"Secret"));
        assert!(!constant_time_eq(b"secret", b"secre"));
    }

    #[test]
    fn parses_lnurl_metadata_hashes() {
        let hash = "ab".repeat(32);
        assert_eq!(parse_metadata_hash(&hash).unwrap(), [0xab; 32]);
        assert!(parse_metadata_hash("abcd").is_err());
        assert!(parse_metadata_hash("not hex").is_err());
    }

    #[test]
    fn extracts_invoice_identifiers_from_qr_payloads() {
        let invoice = |value: &str| Some(QrPayment::Invoice(value.to_owned()));
        assert_eq!(qr_payment_identifier("lnbc10u1abc"), invoice("lnbc10u1abc"));
        assert_eq!(
            qr_payment_identifier("lightning:LNBCRT1TEST"),
            invoice("lnbcrt1test")
        );
        assert_eq!(
            qr_payment_identifier("bitcoin:bcrt1qtest?amount=0.00001234&lightning=lnbcrt1invoice"),
            invoice("lnbcrt1invoice")
        );
        assert_eq!(
            qr_payment_identifier("bitcoin:1BoatSLRHtKNngkdXEeobR76b53LETtpyT?amount=0.001"),
            Some(QrPayment::Address(
                "1BoatSLRHtKNngkdXEeobR76b53LETtpyT".to_owned()
            ))
        );
        assert_eq!(qr_payment_identifier("bitcoin:?amount=1"), None);
        assert_eq!(
            qr_payment_identifier("LNO1QGSQVGNWGCG35Z6EE2H3YCZRADDM72XRFUA9UVE"),
            Some(QrPayment::Offer(
                "lno1qgsqvgnwgcg35z6ee2h3yczraddm72xrfua9uve".to_owned()
            ))
        );
        assert_eq!(qr_payment_identifier("https://evil.example"), None);
    }

    #[test]
    fn formats_bip21_payment_strings() {
        assert_eq!(sats_to_btc(1_234).unwrap(), "0.00001234");
        assert_eq!(
            unified_payment_string("bcrt1qtest", 1_234, "lnbcrt1invoice"),
            "BITCOIN:BCRT1QTEST?AMOUNT=0.00001234&LIGHTNING=LNBCRT1INVOICE"
        );
    }
}
