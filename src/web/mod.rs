use std::{io::Cursor, str::FromStr};

use askama::Template;
use axum::{
    Form, Json, Router,
    body::{Body, to_bytes},
    extract::{ConnectInfo, Path, Query, State},
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
    cors::{Any, CorsLayer},
    request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer},
    services::ServeDir,
    trace::TraceLayer,
};

use crate::{
    AppError, AppResult, AppState,
    domain::PaymentStatus,
    payment_service::{CreateRequest, CreatedPayment},
    pricing::STANDARD_OP_RETURN_BYTES,
    rate_limit,
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

fn qr_dimension(value: Option<&str>) -> u32 {
    value
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(DEFAULT_QR_SIZE)
}

#[derive(Deserialize)]
struct WalletNotifyEvent {
    txid: String,
    key: String,
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
}

#[derive(Template)]
#[template(path = "index.html")]
struct IndexTemplate<'a> {
    onion_url: &'a str,
    recent_txids: &'a [String],
    error: &'a str,
    message: &'a str,
}

#[derive(Template)]
#[template(path = "invoice.html")]
struct InvoiceTemplate<'a> {
    onion_url: &'a str,
    message: &'a str,
    message_hash: String,
    invoice: &'a str,
    payment_hash: &'a str,
    payment_string: String,
    qr_string: String,
    unified: bool,
}

#[derive(Template)]
#[template(path = "success.html")]
struct SuccessTemplate<'a> {
    onion_url: &'a str,
    txid: &'a str,
    warning: bool,
}

#[derive(Template)]
#[template(path = "pending.html")]
struct PendingTemplate<'a> {
    onion_url: &'a str,
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

pub fn router(state: AppState) -> Router {
    let request_id_header = http::HeaderName::from_static("x-request-id");
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

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
        render_index(&state, "", "").await?.into_response()
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

async fn render_index(state: &AppState, error: &str, message: &str) -> AppResult<Html<String>> {
    let recent = state.repository.recent_public_txids(5).await?;
    render(IndexTemplate {
        onion_url: state.config.server.onion_url.as_str(),
        recent_txids: &recent,
        error,
        message,
    })
}

async fn create_request(
    State(state): State<AppState>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    Form(form): Form<CreateForm>,
) -> Response {
    if let Err(error) = check_create_limit(&state, &headers, Some(peer)) {
        return error.into_response();
    }
    let input = form.into_request();
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
        Err(error) => match render_index(&state, &error.to_string(), &input.message_text()).await {
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
        "maxSendable": 100_000_000_000_u64,
        "minSendable": 1_000_u64,
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
        .filter(|amount| (1_000..=100_000_000_000).contains(amount))
        .ok_or_else(|| {
            AppError::InvalidRequest(
                "amount must be between 1000 and 100000000000 millisatoshis".to_owned(),
            )
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
        return Ok(render(PendingTemplate {
            onion_url: state.config.server.onion_url.as_str(),
        })?
        .into_response());
    }
    let payment_string = record.on_chain.as_ref().map_or_else(
        || invoice.bolt11.clone(),
        |on_chain| {
            unified_payment_string(
                &on_chain.address,
                on_chain.expected_amount_sats,
                &invoice.bolt11,
            )
        },
    );
    // The QR code of a Lightning-only request carries the lightning URI, as
    // before. The unified string already contains its scheme.
    let qr_string = if record.on_chain.is_some() {
        payment_string.clone()
    } else {
        format!("lightning:{}", invoice.bolt11)
    };
    let message = record.request.message_text();
    let page = InvoiceTemplate {
        onion_url: state.config.server.onion_url.as_str(),
        message: &message,
        message_hash: hex::encode(Sha256::digest(message.as_bytes())),
        invoice: &invoice.bolt11,
        payment_hash: &invoice.payment_hash,
        payment_string,
        qr_string,
        unified: record.on_chain.is_some(),
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
            warning: record.request.message.len() > STANDARD_OP_RETURN_BYTES,
        })?
        .into_response());
    }
    let invoice_paid = record.invoice.as_ref().is_some_and(|invoice| invoice.paid);
    let on_chain_txid = record
        .on_chain
        .as_ref()
        .and_then(|payment| payment.txid.as_deref());
    if record.request.payment_status(invoice_paid, on_chain_txid) == PaymentStatus::Pending {
        return Ok(render(PendingTemplate {
            onion_url: state.config.server.onion_url.as_str(),
        })?
        .into_response());
    }
    // Unpaid: show the home page with status 400, as the Scala service did.
    bad_request_index(&state).await
}

async fn bad_request_index(state: &AppState) -> AppResult<Response> {
    Ok((StatusCode::BAD_REQUEST, render_index(state, "", "").await?).into_response())
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

async fn qr(Query(query): Query<QrQuery>) -> AppResult<Response> {
    let width = qr_dimension(query.width.as_deref());
    let height = qr_dimension(query.height.as_deref());
    if width == 0 || height == 0 || width > 1_000 || height > 1_000 {
        return Err(AppError::InvalidRequest(
            "QR dimensions must be between 1 and 1000 pixels".to_owned(),
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
    Json(event): Json<WalletNotifyEvent>,
) -> AppResult<Response> {
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
    let expected_hash = Sha256::digest(expected);
    let supplied_hash = Sha256::digest(event.key.trim().as_bytes());
    if expected_hash != supplied_hash {
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

async fn parse_create_request(request: Request<Body>) -> AppResult<CreateForm> {
    let is_json = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("application/json"));
    let body = to_bytes(request.into_body(), 1_000_000)
        .await
        .map_err(|error| AppError::InvalidRequest(format!("could not read request: {error}")))?;
    if is_json {
        serde_json::from_slice(&body)
            .map_err(|error| AppError::InvalidRequest(format!("invalid JSON request: {error}")))
    } else {
        serde_urlencoded::from_bytes(&body)
            .map_err(|error| AppError::InvalidRequest(format!("invalid form request: {error}")))
    }
}

impl CreateForm {
    fn into_request(self) -> CreateRequest {
        CreateRequest {
            message: self.message.into_bytes(),
            no_twitter: self.no_twitter,
        }
    }
}

impl CreateRequest {
    fn message_text(&self) -> String {
        String::from_utf8_lossy(&self.message).into_owned()
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
        assert_eq!(parsed.message, "hello");
        assert!(parsed.no_twitter);
    }

    #[tokio::test]
    async fn accepts_form_api_requests() {
        let request = Request::builder()
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from("message=hello+world&noTwitter=false"))
            .unwrap();
        let parsed = parse_create_request(request).await.unwrap();
        assert_eq!(parsed.message, "hello world");
        assert!(!parsed.no_twitter);
    }

    #[test]
    fn defaults_qr_dimensions_like_the_scala_service() {
        assert_eq!(qr_dimension(None), 300);
        assert_eq!(qr_dimension(Some("abc")), 300);
        assert_eq!(qr_dimension(Some(" 450 ")), 450);
    }

    #[test]
    fn parses_lnurl_metadata_hashes() {
        let hash = "ab".repeat(32);
        assert_eq!(parse_metadata_hash(&hash).unwrap(), [0xab; 32]);
        assert!(parse_metadata_hash("abcd").is_err());
        assert!(parse_metadata_hash("not hex").is_err());
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
