//! Screen a file before it is stored, and a public message before it is posted.
//!
//! A payload that is not plain text is stored only when the decision
//! service chooses `allow` with confidence at or above the configured
//! floor. Plain text is always stored. Twitter and Nostr hear about it
//! only when a separate decision allows the post. Files the service
//! refuses are remembered by hash, so a resubmission is refused without
//! asking again.

use std::{
    collections::BTreeMap,
    io::Cursor,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base64::{Engine, engine::general_purpose::STANDARD};
use image::{ImageEncoder, ImageReader, Limits, imageops::FilterType};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::{sync::Semaphore, time::Instant};
use url::Url;

use crate::{
    AppError, AppResult,
    config::ModerationConfig,
    repository::{RefusedFile, Repository},
};

const PUBLISH_INSTRUCTIONS: &str = "Can this be published on a public permanent ledger?";
const PUBLISH_ALLOW: &str = "Content that is legal to publish.";
const PUBLISH_REFUSE: &str = "Sexual content involving anyone under 18, or other content that is illegal to publish or possess.";
const POST_INSTRUCTIONS: &str = "Can this be posted from a public social account?";
const POST_ALLOW: &str = "Ordinary speech, including rude or political speech.";
const POST_REFUSE: &str = "A slur, sexual content involving anyone under 18, or other content that would get a public social account banned.";
const IMAGE_NOTE: &str = "The attached image is the file to publish.";
const PDF_NO_TEXT: &str = "This PDF has no readable text layer. The attached images are its pages.";
/// A text layer is cut to this length before it is sent.
const MAX_STATE_BYTES: usize = 200_000;
/// Longest side of a rendered PDF page, in pixels.
const TARGET_LONG_SIDE: f32 = 1024.0;
/// Stay under the renderer's `u16` pixel limit.
const MAX_PIXEL: f32 = 2048.0;
/// Files prepared at once. A render that will not end holds its slot, so
/// this bounds the threads and memory such files can take.
const MAX_RENDERS: usize = 4;
/// How far into a payload a PDF reader looks for the `%PDF-` header.
const PDF_HEADER_WINDOW: usize = 1024;
/// An image whose difference hash is this many bits or fewer from a
/// refused image is a near copy of it.
const MAX_DHASH_DISTANCE: u32 = 4;
/// A hash with fewer set or clear bits than this comes from a flat or
/// plain gradient image, and would match many unrelated images.
const MIN_DHASH_BITS: u32 = 8;
/// Decoding limits for hashing, so a small file cannot claim a huge canvas.
const MAX_HASH_SIDE: u32 = 16_384;
const MAX_HASH_ALLOC: u64 = 256 * 1024 * 1024;

#[derive(Clone, Default)]
pub struct Moderator {
    inner: Mode,
}

#[derive(Clone, Default)]
enum Mode {
    Skip,
    #[default]
    Closed,
    Live(Box<LiveModerator>),
}

#[derive(Clone)]
struct LiveModerator {
    client: reqwest::Client,
    url: Url,
    api_key: String,
    floor: f64,
    max_pdf_pages: usize,
    /// Covers the whole check of one file: the wait for a render slot, the
    /// render, and the request.
    timeout: Duration,
    /// Render slots. A slot stays taken until its render ends, even after
    /// the wait for it has given up.
    renders: Arc<Semaphore>,
    /// Where refused files are remembered. Tests may run without it.
    refusals: Option<Repository>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FileKind {
    Text,
    Image(&'static str),
    Pdf,
    Unsupported,
}

#[derive(Debug)]
struct Submission {
    state: String,
    files: Vec<String>,
    kind: &'static str,
    /// Difference hash of an image, when it could be decoded.
    dhash: Option<u64>,
}

impl Moderator {
    pub async fn connect(config: &ModerationConfig, refusals: Repository) -> AppResult<Self> {
        if config.allow_unscreened {
            tracing::warn!("files and social posts are accepted without a content check");
            return Ok(Self { inner: Mode::Skip });
        }
        let (Some(url), Some(path)) = (&config.url, &config.api_key_file) else {
            tracing::warn!(
                "file uploads are refused and social posts are sent until moderation is configured"
            );
            return Ok(Self {
                inner: Mode::Closed,
            });
        };
        let api_key = tokio::fs::read_to_string(path)
            .await
            .map(|value| value.trim().to_owned())
            .map_err(|error| {
                AppError::Config(format!(
                    "could not read moderation api key {}: {error}",
                    path.display()
                ))
            })?;
        if api_key.is_empty() {
            return Err(AppError::Config(
                "moderation api key file is empty".to_owned(),
            ));
        }
        let timeout = Duration::from_secs(config.timeout_seconds);
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .no_proxy()
            .build()
            .map_err(|error| {
                AppError::Config(format!("could not build the moderation client: {error}"))
            })?;
        Ok(Self {
            inner: Mode::Live(Box::new(LiveModerator {
                client,
                url: url.clone(),
                api_key,
                floor: config.confidence_floor,
                max_pdf_pages: config.max_pdf_pages,
                timeout,
                renders: Arc::new(Semaphore::new(MAX_RENDERS)),
                refusals: Some(refusals),
            })),
        })
    }

    /// Screen a payload before it is stored.
    ///
    /// The check is best effort. Plain text, and any file the decision
    /// service cannot be shown, is stored without a check. Images and PDFs
    /// must pass it, whichever form field they came from.
    pub async fn screen(&self, bytes: &[u8]) -> AppResult<()> {
        let kind = classify(bytes);
        if kind == FileKind::Text {
            return Ok(());
        }
        if kind == FileKind::Unsupported {
            tracing::info!("file screen stored a file type it cannot check");
            return Ok(());
        }
        match &self.inner {
            Mode::Skip => Ok(()),
            Mode::Closed => {
                tracing::warn!("file upload refused because screening is not configured");
                Err(AppError::FileCheckFailed)
            }
            Mode::Live(live) => live.screen(bytes, kind).await,
        }
    }

    /// Whether Twitter and Nostr may announce `text`.
    ///
    /// An unconfigured check allows the post. A configured check allows it
    /// only on an allow at or above the floor. The transaction is
    /// unaffected either way.
    pub async fn check_post(&self, text: &str) -> Verdict {
        if text.trim().is_empty() {
            return Verdict::Allow;
        }
        match &self.inner {
            Mode::Skip | Mode::Closed => Verdict::Allow,
            Mode::Live(live) => live.check_post(text).await,
        }
    }
}

impl LiveModerator {
    async fn screen(&self, bytes: &[u8], kind: FileKind) -> AppResult<()> {
        let sha256 = hex::encode(Sha256::digest(bytes));
        if self.was_refused(&sha256).await {
            tracing::info!(%sha256, "file screen refused a file it refused before");
            return Err(AppError::Unpublishable);
        }
        let deadline = Instant::now() + self.timeout;
        // Every slot taken is a passing state, so the visitor may try again.
        let slot = tokio::time::timeout_at(deadline, self.renders.clone().acquire_owned())
            .await
            .map_err(|_| {
                tracing::warn!("file screen is busy");
                AppError::FileCheckFailed
            })?
            .map_err(|_| AppError::FileCheckFailed)?;
        let owned = bytes.to_vec();
        let max_pages = self.max_pdf_pages;
        // The render keeps running after this wait ends, and keeps its slot.
        // A file too slow to prepare is stored without a check.
        let prepared = tokio::time::timeout_at(
            deadline,
            tokio::task::spawn_blocking(move || {
                let _slot = slot;
                prepare(&owned, kind, max_pages)
            }),
        )
        .await;
        let Ok(Ok(Some(submission))) = prepared else {
            tracing::info!("file screen stored a file it could not prepare");
            return Ok(());
        };
        if let Some(dhash) = submission.dhash
            && self.near_refused_image(dhash).await
        {
            tracing::info!(%sha256, "file screen refused a near copy of a refused image");
            return Err(AppError::Unpublishable);
        }
        tracing::info!(
            kind = submission.kind,
            files = submission.files.len(),
            "file screen asking for a decision"
        );
        let response = self
            .client
            .post(self.url.clone())
            .bearer_auth(&self.api_key)
            .timeout(deadline.saturating_duration_since(Instant::now()))
            .json(&request_body(
                &submission.state,
                &submission.files,
                &PUBLISH,
            ))
            .send()
            .await
            .map_err(|error| {
                tracing::warn!(error = %error, "file screen request failed");
                AppError::FileCheckFailed
            })?;
        let status = response.status();
        if !status.is_success() {
            tracing::warn!(%status, "file screen request was rejected");
            return Err(AppError::FileCheckFailed);
        }
        let body = response.json().await.map_err(|_| {
            tracing::warn!("file screen returned a decision that could not be read");
            AppError::FileCheckFailed
        })?;
        match interpret_decision(&body, "publish", self.floor, "file screen") {
            Verdict::Allow => Ok(()),
            Verdict::Refuse => {
                self.remember(&sha256, &submission, bytes.len()).await;
                Err(AppError::Unpublishable)
            }
            // An unsure allow is not remembered, so a lower floor can let
            // the same file through later.
            Verdict::Unsure => Err(AppError::Unpublishable),
            Verdict::Unusable => Err(AppError::FileCheckFailed),
        }
    }

    /// Memory lookups are best effort. A database error skips them.
    async fn was_refused(&self, sha256: &str) -> bool {
        let Some(refusals) = &self.refusals else {
            return false;
        };
        refusals
            .file_was_refused(sha256)
            .await
            .unwrap_or_else(|error| {
                tracing::warn!(%error, "could not look up refused files");
                false
            })
    }

    async fn near_refused_image(&self, dhash: u64) -> bool {
        let Some(refusals) = &self.refusals else {
            return false;
        };
        match refusals.refused_image_hashes().await {
            Ok(hashes) => hashes
                .iter()
                .any(|refused| (refused ^ dhash).count_ones() <= MAX_DHASH_DISTANCE),
            Err(error) => {
                tracing::warn!(%error, "could not look up refused images");
                false
            }
        }
    }

    async fn remember(&self, sha256: &str, submission: &Submission, size: usize) {
        let Some(refusals) = &self.refusals else {
            return;
        };
        let refused_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| {
                i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
            });
        let file = RefusedFile {
            sha256,
            dhash: submission.dhash,
            size: i64::try_from(size).unwrap_or(i64::MAX),
            kind: submission.kind,
            refused_at,
        };
        if let Err(error) = refusals.record_refused_file(&file).await {
            tracing::warn!(%error, "could not remember a refused file");
        }
    }

    async fn check_post(&self, text: &str) -> Verdict {
        let response = match self
            .client
            .post(self.url.clone())
            .bearer_auth(&self.api_key)
            .json(&request_body(text, &[], &POST))
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) => {
                tracing::warn!(error = %error, "social post check failed");
                return Verdict::Unusable;
            }
        };
        let status = response.status();
        if !status.is_success() {
            tracing::warn!(%status, "social post check was rejected");
            return Verdict::Unusable;
        }
        let body = match response.json().await {
            Ok(body) => body,
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "social post check returned a decision that could not be read"
                );
                return Verdict::Unusable;
            }
        };
        interpret_decision(&body, POST.name, self.floor, "social post check")
    }
}

struct Question {
    name: &'static str,
    instructions: &'static str,
    allow: &'static str,
    refuse: &'static str,
}

const PUBLISH: Question = Question {
    name: "publish",
    instructions: PUBLISH_INSTRUCTIONS,
    allow: PUBLISH_ALLOW,
    refuse: PUBLISH_REFUSE,
};

const POST: Question = Question {
    name: "post",
    instructions: POST_INSTRUCTIONS,
    allow: POST_ALLOW,
    refuse: POST_REFUSE,
};

/// The outcome of a content check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    Refuse,
    /// An allow below the confidence floor.
    Unsure,
    /// The check did not return a usable decision.
    Unusable,
}

fn request_body(state: &str, files: &[String], question: &Question) -> serde_json::Value {
    let mut body = serde_json::json!({
        "state": state,
        "questions": {}
    });
    body["questions"][question.name] = serde_json::json!({
        "type": "choice",
        "instructions": question.instructions,
        "criteria": {
            "allow": question.allow,
            "refuse": question.refuse
        }
    });
    if !files.is_empty() {
        body["files"] = serde_json::json!(files);
    }
    body
}

#[derive(Debug, Deserialize)]
struct DecisionBody {
    #[serde(default)]
    answers: BTreeMap<String, AnswerBody>,
}

#[derive(Debug, Deserialize)]
struct AnswerBody {
    choice: Option<String>,
    confidence: Option<f64>,
}

fn interpret_decision(body: &DecisionBody, question: &str, floor: f64, check: &str) -> Verdict {
    let Some(answer) = body.answers.get(question) else {
        tracing::warn!(check, "content check returned no decision");
        return Verdict::Unusable;
    };
    let (Some(choice), Some(confidence)) = (&answer.choice, answer.confidence) else {
        tracing::warn!(
            check,
            "content check returned a decision without a choice or confidence"
        );
        return Verdict::Unusable;
    };
    if !confidence.is_finite() || !(0.0..=1.0).contains(&confidence) {
        tracing::warn!(check, "content check returned a confidence outside 0 to 1");
        return Verdict::Unusable;
    }
    if choice == "refuse" {
        tracing::info!(check, confidence, "content check refused");
        return Verdict::Refuse;
    }
    if choice == "allow" && confidence >= floor {
        tracing::info!(check, confidence, "content check allowed");
        return Verdict::Allow;
    }
    if choice == "allow" {
        tracing::info!(
            check,
            confidence,
            floor,
            "content check confidence was below the floor"
        );
        return Verdict::Unsure;
    }
    let shown: String = choice.chars().take(32).collect();
    tracing::warn!(check, choice = %shown, "content check returned an unusable choice");
    Verdict::Unusable
}

/// What the decision service is shown, or `None` when it cannot be shown
/// anything.
fn prepare(bytes: &[u8], kind: FileKind, max_pages: usize) -> Option<Submission> {
    match kind {
        // Neither reaches this point. Both are stored without a check.
        FileKind::Text | FileKind::Unsupported => None,
        FileKind::Image(mime) => Some(Submission {
            state: IMAGE_NOTE.to_owned(),
            files: vec![data_url(mime, bytes)],
            kind: "image",
            dhash: dhash(bytes),
        }),
        FileKind::Pdf => prepare_pdf(bytes, max_pages),
    }
}

/// Difference hash of an image: each bit says whether a pixel of a 9 by 8
/// grey thumbnail is darker than its right neighbour. A near copy, such as
/// the same picture rescaled or saved again, differs in only a few bits.
fn dhash(bytes: &[u8]) -> Option<u64> {
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_HASH_SIDE);
    limits.max_image_height = Some(MAX_HASH_SIDE);
    limits.max_alloc = Some(MAX_HASH_ALLOC);
    reader.limits(limits);
    let thumbnail = reader
        .decode()
        .ok()?
        .resize_exact(9, 8, FilterType::Triangle)
        .to_luma8();
    let mut hash = 0_u64;
    for y in 0..8 {
        for x in 0..8 {
            hash <<= 1;
            if thumbnail.get_pixel(x, y)[0] < thumbnail.get_pixel(x + 1, y)[0] {
                hash |= 1;
            }
        }
    }
    let ones = hash.count_ones();
    (MIN_DHASH_BITS..=64 - MIN_DHASH_BITS)
        .contains(&ones)
        .then_some(hash)
}

fn classify(bytes: &[u8]) -> FileKind {
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        FileKind::Image("image/jpeg")
    } else if bytes.starts_with(&[0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n']) {
        FileKind::Image("image/png")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        FileKind::Image("image/gif")
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        FileKind::Image("image/webp")
    } else if is_pdf(bytes) {
        FileKind::Pdf
    } else if std::str::from_utf8(bytes).is_ok() {
        FileKind::Text
    } else {
        FileKind::Unsupported
    }
}

/// PDF readers look for the header anywhere in the first kilobyte, so a
/// text prefix must not hide a PDF from the check.
fn is_pdf(bytes: &[u8]) -> bool {
    bytes[..bytes.len().min(PDF_HEADER_WINDOW)]
        .windows(5)
        .any(|window| window == b"%PDF-")
}

/// Shows the service the text layer and the first `max_pages` pages that
/// render.
fn prepare_pdf(bytes: &[u8], max_pages: usize) -> Option<Submission> {
    let text = pdf_text(bytes);
    let files = hayro::hayro_syntax::Pdf::new(bytes.to_vec())
        .map(|pdf| render_pages(&pdf, max_pages))
        .unwrap_or_default();
    if text.is_none() && files.is_empty() {
        return None;
    }
    Some(Submission {
        state: text.unwrap_or_else(|| PDF_NO_TEXT.to_owned()),
        files,
        kind: "pdf",
        dhash: None,
    })
}

/// The text layer, cut to `MAX_STATE_BYTES`.
fn pdf_text(bytes: &[u8]) -> Option<String> {
    let extracted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        pdf_extract::extract_text_from_mem(bytes)
    }));
    let Ok(Ok(text)) = extracted else {
        tracing::info!("file screen could not read the pdf text layer");
        return None;
    };
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed[..trimmed.floor_char_boundary(MAX_STATE_BYTES)].to_owned())
}

fn render_pages(pdf: &hayro::hayro_syntax::Pdf, max_pages: usize) -> Vec<String> {
    let pages = pdf.pages();
    if pages.len() > max_pages {
        tracing::info!(
            pages = pages.len(),
            max_pages,
            "file screen checks only the first pages of a pdf"
        );
    }
    let cache = hayro::RenderCache::new();
    let interpreter = hayro::hayro_interpret::InterpreterSettings::default();
    let render_settings = hayro::RenderSettings::default();
    let mut files = Vec::new();
    for page in pages.iter().take(max_pages) {
        let rendered = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            render_page(page, &cache, &interpreter, render_settings)
        }));
        if let Ok(Some(png)) = rendered {
            files.push(data_url("image/png", &png));
        } else {
            tracing::info!("file screen skipped a pdf page it could not render");
        }
    }
    files
}

fn render_page<'a>(
    page: &'a hayro::hayro_syntax::page::Page<'a>,
    cache: &hayro::RenderCache<'a>,
    interpreter: &hayro::hayro_interpret::InterpreterSettings,
    render_settings: hayro::RenderSettings,
) -> Option<Vec<u8>> {
    let (width, height) = page.render_dimensions();
    let scale = page_scale(width, height)?;
    let pixmap = hayro::render(
        page,
        cache,
        interpreter,
        &render_settings,
        &hayro::PixmapSettings {
            x_scale: scale,
            y_scale: scale,
            bg_color: hayro::vello_cpu::color::palette::css::WHITE,
        },
    );
    if pixmap.width() == 0 || pixmap.height() == 0 {
        return None;
    }
    let pixels = straight_rgba(pixmap.data_as_u8_slice())?;
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png)
        .write_image(
            &pixels,
            u32::from(pixmap.width()),
            u32::from(pixmap.height()),
            image::ExtendedColorType::Rgba8,
        )
        .ok()?;
    Some(png)
}

fn page_scale(width: f32, height: f32) -> Option<f32> {
    if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
        return None;
    }
    let scale = (TARGET_LONG_SIDE / width.max(height))
        .min(MAX_PIXEL / width)
        .min(MAX_PIXEL / height);
    let px_w = width * scale;
    let px_h = height * scale;
    if !px_w.is_finite()
        || !px_h.is_finite()
        || px_w < 1.0
        || px_h < 1.0
        || px_w > f32::from(u16::MAX)
        || px_h > f32::from(u16::MAX)
    {
        return None;
    }
    Some(scale)
}

fn straight_rgba(premultiplied: &[u8]) -> Option<Vec<u8>> {
    let (pixels, rest) = premultiplied.as_chunks::<4>();
    if !rest.is_empty() {
        return None;
    }
    let mut straight = Vec::with_capacity(premultiplied.len());
    for pixel in pixels {
        let alpha = u32::from(pixel[3]);
        if alpha == 0 || alpha == 255 {
            straight.extend_from_slice(pixel);
            continue;
        }
        for channel in &pixel[..3] {
            let value = (u32::from(*channel) * 255 + alpha / 2) / alpha;
            straight.push(u8::try_from(value).unwrap_or(255));
        }
        straight.push(pixel[3]);
    }
    Some(straight)
}

fn data_url(mime: &str, bytes: &[u8]) -> String {
    format!("data:{mime};base64,{}", STANDARD.encode(bytes))
}

#[cfg(test)]
mod tests {
    use std::{
        net::SocketAddr,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use axum::{
        Json, Router,
        extract::State,
        http::{HeaderMap, StatusCode, header},
        routing::post,
    };
    use lopdf::{Document, Object, Stream, dictionary};
    use serde_json::json;

    use super::*;

    /// A payload the check treats as an image.
    const IMAGE: &[u8] = b"GIF89ahello";

    fn prepare_file(bytes: &[u8], max_pages: usize) -> Option<Submission> {
        prepare(bytes, classify(bytes), max_pages)
    }

    fn answer(choice: Option<&str>, confidence: Option<f64>) -> DecisionBody {
        let mut answers = BTreeMap::new();
        answers.insert(
            "publish".to_owned(),
            AnswerBody {
                choice: choice.map(str::to_owned),
                confidence,
            },
        );
        DecisionBody { answers }
    }

    fn live(url: &str, floor: f64, pages: usize, timeout: Duration) -> Moderator {
        live_with_slots(url, floor, pages, timeout, MAX_RENDERS)
    }

    fn live_with_slots(
        url: &str,
        floor: f64,
        pages: usize,
        timeout: Duration,
        slots: usize,
    ) -> Moderator {
        // These tests speak HTTP to a local server. reqwest 0.13 still loads
        // the platform CA store while building a client, and the Nix sandbox
        // has none, so the package check fails unless that load is skipped.
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .no_proxy()
            .tls_certs_only(std::iter::empty())
            .build()
            .unwrap();
        Moderator {
            inner: Mode::Live(Box::new(LiveModerator {
                client,
                url: Url::parse(url).unwrap(),
                api_key: "test-key".to_owned(),
                floor,
                max_pdf_pages: pages,
                timeout,
                renders: Arc::new(Semaphore::new(slots)),
                refusals: None,
            })),
        }
    }

    async fn serve(app: Router) -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        for _ in 0..50 {
            if tokio::net::TcpStream::connect(address).await.is_ok() {
                return address;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        address
    }

    async fn record_and_allow(
        State(seen): State<Arc<Mutex<Option<serde_json::Value>>>>,
        headers: HeaderMap,
        Json(body): Json<serde_json::Value>,
    ) -> Json<serde_json::Value> {
        assert_eq!(
            headers
                .get(header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok()),
            Some("Bearer test-key")
        );
        *seen.lock().unwrap() = Some(body.clone());
        let mut answers = serde_json::Map::new();
        if let Some(questions) = body["questions"].as_object() {
            for name in questions.keys() {
                answers.insert(
                    name.clone(),
                    json!({ "choice": "allow", "confidence": 0.95 }),
                );
            }
        }
        Json(json!({ "answers": answers }))
    }

    fn pdf_with_pages(texts: &[&str]) -> Vec<u8> {
        let mut doc = Document::with_version("1.5");
        let parent_id = doc.new_object_id();
        let font_id = doc.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Courier",
        });
        let resources_id = doc.add_object(dictionary! {
            "Font" => dictionary! {
                "F1" => font_id,
            },
        });
        let mut kids = Vec::new();
        for text in texts {
            let content = lopdf::content::Content {
                operations: vec![
                    lopdf::content::Operation::new("BT", vec![]),
                    lopdf::content::Operation::new("Tf", vec!["F1".into(), 24.into()]),
                    lopdf::content::Operation::new("Td", vec![72.into(), 200.into()]),
                    lopdf::content::Operation::new("Tj", vec![Object::string_literal(*text)]),
                    lopdf::content::Operation::new("ET", vec![]),
                ],
            };
            let content_id = doc.add_object(Stream::new(dictionary! {}, content.encode().unwrap()));
            let page_id = doc.add_object(dictionary! {
                "Type" => "Page",
                "Parent" => parent_id,
                "Contents" => content_id,
                "Resources" => resources_id,
                "MediaBox" => vec![0.into(), 0.into(), 300.into(), 300.into()],
            });
            kids.push(page_id.into());
        }
        let count = i32::try_from(kids.len()).unwrap();
        doc.objects.insert(
            parent_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => kids,
                "Count" => count,
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => parent_id,
        });
        doc.trailer.set("Root", catalog_id);
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();
        bytes
    }

    fn embedded_pdf() -> Vec<u8> {
        let mut doc = Document::load_mem(&pdf_with_pages(&["Hello"])).unwrap();
        let file_id = doc.add_object(Stream::new(
            dictionary! { "Type" => "EmbeddedFile" },
            b"hidden".to_vec(),
        ));
        doc.add_object(dictionary! {
            "Type" => "Filespec",
            "F" => "hidden.txt",
            "EF" => dictionary! { "F" => file_id },
        });
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();
        bytes
    }

    #[test]
    fn classifies_by_magic_before_text() {
        assert_eq!(classify(b"GIF89a"), FileKind::Image("image/gif"));
        assert_eq!(classify(b"\x89PNG\r\n\x1a\n"), FileKind::Image("image/png"));
        assert_eq!(classify(&[0xff, 0xd8, 0xff]), FileKind::Image("image/jpeg"));
        assert_eq!(
            classify(b"RIFF\x00\x00\x00\x00WEBP"),
            FileKind::Image("image/webp")
        );
        assert_eq!(classify(b"%PDF-1.7\nhello"), FileKind::Pdf);
        assert_eq!(classify(b"hello\n%PDF-1.7\n"), FileKind::Pdf);
        assert_eq!(classify(b"hello"), FileKind::Text);
        assert_eq!(classify(b"\xff\x00"), FileKind::Unsupported);
    }

    fn verdict(choice: Option<&str>, confidence: Option<f64>) -> Verdict {
        interpret_decision(&answer(choice, confidence), "publish", 0.9, "file screen")
    }

    #[test]
    fn allows_only_a_confident_allow() {
        assert_eq!(verdict(Some("allow"), Some(0.9)), Verdict::Allow);
        assert_eq!(verdict(Some("allow"), Some(0.89)), Verdict::Unsure);
        assert_eq!(verdict(Some("refuse"), Some(0.99)), Verdict::Refuse);
        assert_eq!(verdict(Some("allow"), None), Verdict::Unusable);
        assert_eq!(verdict(Some("maybe"), Some(1.0)), Verdict::Unusable);
        assert_eq!(verdict(Some("allow"), Some(1.1)), Verdict::Unusable);
        assert_eq!(verdict(Some("allow"), Some(f64::NAN)), Verdict::Unusable);
        assert_eq!(
            interpret_decision(
                &DecisionBody {
                    answers: BTreeMap::new()
                },
                "publish",
                0.9,
                "file screen"
            ),
            Verdict::Unusable
        );
    }

    #[test]
    fn names_the_two_file_outcomes() {
        assert_eq!(
            AppError::Unpublishable.to_string(),
            "This file cannot be published."
        );
        assert_eq!(
            AppError::FileCheckFailed.to_string(),
            "This file could not be checked. Try again."
        );
    }

    #[tokio::test]
    async fn skips_the_check_when_unscreened_files_are_allowed() {
        let moderator = Moderator { inner: Mode::Skip };
        moderator.screen(IMAGE).await.unwrap();
    }

    #[tokio::test]
    async fn refuses_a_file_when_the_check_is_not_configured() {
        let moderator = Moderator {
            inner: Mode::Closed,
        };
        let error = moderator.screen(IMAGE).await.unwrap_err();
        assert!(matches!(error, AppError::FileCheckFailed));
        assert_eq!(moderator.check_post("hello").await, Verdict::Allow);
    }

    #[tokio::test]
    async fn stores_plain_text_without_asking() {
        let seen = Arc::new(Mutex::new(None));
        let app = Router::new()
            .route("/v1/systemone", post(record_and_allow))
            .with_state(seen.clone());
        let address = serve(app).await;
        let moderator = live(
            &format!("http://{address}/v1/systemone"),
            0.9,
            8,
            Duration::from_secs(2),
        );
        moderator.screen(b"hello").await.unwrap();
        assert!(seen.lock().unwrap().is_none());
        Moderator {
            inner: Mode::Closed,
        }
        .screen(b"hello")
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn sends_a_gif_as_an_image() {
        let seen = Arc::new(Mutex::new(None));
        let app = Router::new()
            .route("/v1/systemone", post(record_and_allow))
            .with_state(seen.clone());
        let address = serve(app).await;
        let moderator = live(
            &format!("http://{address}/v1/systemone"),
            0.9,
            8,
            Duration::from_secs(2),
        );
        moderator.screen(IMAGE).await.unwrap();
        let body = seen.lock().unwrap().clone().unwrap();
        let file = body["files"][0].as_str().unwrap();
        assert!(file.starts_with("data:image/gif;base64,"));
        assert_eq!(body["state"], IMAGE_NOTE);
        assert_eq!(body["questions"]["publish"]["type"], "choice");
        assert_eq!(
            body["questions"]["publish"]["instructions"],
            PUBLISH_INSTRUCTIONS
        );
        assert_eq!(
            body["questions"]["publish"]["criteria"]["allow"],
            PUBLISH_ALLOW
        );
        assert_eq!(
            body["questions"]["publish"]["criteria"]["refuse"],
            PUBLISH_REFUSE
        );
    }

    #[tokio::test]
    async fn stores_an_unsupported_file_without_asking() {
        let seen = Arc::new(Mutex::new(None));
        let app = Router::new()
            .route("/v1/systemone", post(record_and_allow))
            .with_state(seen.clone());
        let address = serve(app).await;
        let moderator = live(
            &format!("http://{address}/v1/systemone"),
            0.9,
            8,
            Duration::from_secs(2),
        );
        moderator.screen(b"\xff\x00hello").await.unwrap();
        assert!(seen.lock().unwrap().is_none());
        Moderator {
            inner: Mode::Closed,
        }
        .screen(b"\xff\x00hello")
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn refuses_when_the_model_refuses_or_is_unsure() {
        let address = serve(Router::new().route(
            "/v1/systemone",
            post(|| async {
                Json(json!({
                    "answers": { "publish": { "choice": "refuse", "confidence": 0.99 } }
                }))
            }),
        ))
        .await;
        let moderator = live(
            &format!("http://{address}/v1/systemone"),
            0.9,
            8,
            Duration::from_secs(2),
        );
        let error = moderator.screen(IMAGE).await.unwrap_err();
        assert!(matches!(error, AppError::Unpublishable));

        let address = serve(Router::new().route(
            "/v1/systemone",
            post(|| async {
                Json(json!({
                    "answers": { "publish": { "choice": "allow", "confidence": 0.2 } }
                }))
            }),
        ))
        .await;
        let moderator = live(
            &format!("http://{address}/v1/systemone"),
            0.9,
            8,
            Duration::from_secs(2),
        );
        let error = moderator.screen(IMAGE).await.unwrap_err();
        assert!(matches!(error, AppError::Unpublishable));
    }

    #[tokio::test]
    async fn does_not_publish_when_the_check_cannot_answer() {
        let address = serve(Router::new().route(
            "/v1/systemone",
            post(|| async { StatusCode::NOT_IMPLEMENTED }),
        ))
        .await;
        let moderator = live(
            &format!("http://{address}/v1/systemone"),
            0.9,
            8,
            Duration::from_secs(2),
        );
        let error = moderator.screen(IMAGE).await.unwrap_err();
        assert!(matches!(error, AppError::FileCheckFailed));

        let address = serve(Router::new().route(
            "/v1/systemone",
            post(|| async {
                tokio::time::sleep(Duration::from_secs(2)).await;
                StatusCode::OK
            }),
        ))
        .await;
        let moderator = live(
            &format!("http://{address}/v1/systemone"),
            0.9,
            8,
            Duration::from_millis(200),
        );
        let error = moderator.screen(IMAGE).await.unwrap_err();
        assert!(matches!(error, AppError::FileCheckFailed));
    }

    #[tokio::test]
    async fn asks_for_a_retry_when_every_render_slot_is_taken() {
        let moderator = live_with_slots(
            "http://127.0.0.1:9/v1/systemone",
            0.9,
            8,
            Duration::from_millis(100),
            0,
        );
        let error = moderator.screen(IMAGE).await.unwrap_err();
        assert!(matches!(error, AppError::FileCheckFailed));
    }

    #[test]
    fn screens_a_pdf_with_an_embedded_file() {
        let prepared = prepare_file(&embedded_pdf(), 8).unwrap();
        assert_eq!(prepared.files.len(), 1);
    }

    #[test]
    fn screens_the_first_pages_of_a_long_pdf() {
        let prepared = prepare_file(&pdf_with_pages(&["One", "Two"]), 1).unwrap();
        assert_eq!(prepared.files.len(), 1);
        assert!(prepared.state.contains("Two"));
    }

    #[test]
    fn stores_a_pdf_that_does_not_parse_without_asking() {
        assert!(prepare_file(b"%PDF-1.7 not a pdf", 8).is_none());
    }

    #[test]
    fn cuts_a_long_text_layer() {
        let long = "x".repeat(MAX_STATE_BYTES + 10);
        let prepared = prepare_file(&pdf_with_pages(&[long.as_str()]), 8).unwrap();
        assert!(prepared.state.starts_with("xxx"));
        assert!(prepared.state.len() <= MAX_STATE_BYTES);
    }

    #[test]
    fn renders_each_pdf_page_and_keeps_the_text() {
        let prepared = prepare_file(&pdf_with_pages(&["Alpha", "Beta"]), 8).unwrap();
        assert_eq!(prepared.files.len(), 2);
        assert!(
            prepared
                .files
                .iter()
                .all(|file| file.starts_with("data:image/png;base64,"))
        );
        assert!(prepared.state.contains("Alpha"));
        assert!(prepared.state.contains("Beta"));
        let png = STANDARD
            .decode(prepared.files[0].trim_start_matches("data:image/png;base64,"))
            .unwrap();
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
    }

    #[tokio::test]
    async fn sends_post_text_and_posts_on_allow() {
        let seen = Arc::new(Mutex::new(None));
        let app = Router::new()
            .route("/v1/systemone", post(record_and_allow))
            .with_state(seen.clone());
        let address = serve(app).await;
        let moderator = live(
            &format!("http://{address}/v1/systemone"),
            0.9,
            8,
            Duration::from_secs(2),
        );
        assert_eq!(moderator.check_post("hello").await, Verdict::Allow);
        let body = seen.lock().unwrap().clone().unwrap();
        assert_eq!(body["state"], "hello");
        assert!(body.get("files").is_none());
        assert_eq!(body["questions"]["post"]["instructions"], POST_INSTRUCTIONS);
        assert_eq!(body["questions"]["post"]["criteria"]["allow"], POST_ALLOW);
        assert_eq!(body["questions"]["post"]["criteria"]["refuse"], POST_REFUSE);
    }

    #[tokio::test]
    async fn skips_a_blank_post_without_asking() {
        let app = Router::new().route(
            "/v1/systemone",
            post(|| async { (StatusCode::INTERNAL_SERVER_ERROR, "nope") }),
        );
        let address = serve(app).await;
        let moderator = live(
            &format!("http://{address}/v1/systemone"),
            0.9,
            8,
            Duration::from_secs(2),
        );
        assert_eq!(moderator.check_post("  ").await, Verdict::Allow);
    }

    #[tokio::test]
    async fn skips_the_post_when_the_model_refuses_or_cannot_answer() {
        let refuse = Router::new().route(
            "/v1/systemone",
            post(|| async {
                Json(json!({
                    "answers": { "post": { "choice": "refuse", "confidence": 0.99 } }
                }))
            }),
        );
        let address = serve(refuse).await;
        let moderator = live(
            &format!("http://{address}/v1/systemone"),
            0.9,
            8,
            Duration::from_secs(2),
        );
        assert_eq!(moderator.check_post("hello").await, Verdict::Refuse);

        let broken = Router::new().route(
            "/v1/systemone",
            post(|| async { StatusCode::NOT_IMPLEMENTED }),
        );
        let address = serve(broken).await;
        let moderator = live(
            &format!("http://{address}/v1/systemone"),
            0.9,
            8,
            Duration::from_secs(2),
        );
        assert_eq!(moderator.check_post("hello").await, Verdict::Unusable);
    }

    /// A smooth test picture. `phase` shifts the pattern, which gives an
    /// unrelated picture.
    fn picture(phase: f32) -> image::RgbImage {
        image::RgbImage::from_fn(64, 64, |x, y| {
            #[allow(clippy::cast_precision_loss)]
            let (u, v) = (x as f32 / 64.0, y as f32 / 64.0);
            let wave = (std::f32::consts::TAU * (u * 1.5 + phase)).sin()
                * (std::f32::consts::TAU * (v + phase / 2.0)).cos();
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let value = (127.0 + 120.0 * wave) as u8;
            image::Rgb([value, 255 - value, 90])
        })
    }

    /// The same picture shrunk and saved again as a JPEG.
    fn near_copy(image: &image::RgbImage) -> Vec<u8> {
        let small = image::imageops::resize(image, 48, 48, FilterType::Triangle);
        let mut bytes = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, 70)
            .write_image(small.as_raw(), 48, 48, image::ExtendedColorType::Rgb8)
            .unwrap();
        bytes
    }

    fn png(image: &image::RgbImage) -> Vec<u8> {
        let mut bytes = Vec::new();
        image::codecs::png::PngEncoder::new(&mut bytes)
            .write_image(
                image.as_raw(),
                image.width(),
                image.height(),
                image::ExtendedColorType::Rgb8,
            )
            .unwrap();
        bytes
    }

    #[test]
    fn hashes_near_copies_close_and_other_images_apart() {
        let original = dhash(&png(&picture(0.0))).unwrap();
        let copy = dhash(&near_copy(&picture(0.0))).unwrap();
        let other = dhash(&png(&picture(0.4))).unwrap();
        assert!((original ^ copy).count_ones() <= MAX_DHASH_DISTANCE);
        assert!((original ^ other).count_ones() > MAX_DHASH_DISTANCE);
    }

    #[test]
    fn does_not_hash_a_flat_image() {
        let flat = image::RgbImage::from_pixel(32, 32, image::Rgb([200, 10, 10]));
        assert!(dhash(&png(&flat)).is_none());
        assert!(dhash(b"\x89PNG\r\n\x1a\nbroken").is_none());
    }

    async fn remembering(url: &str) -> (Moderator, tempfile::TempDir) {
        let directory = tempfile::tempdir().unwrap();
        let database = crate::Database::connect(&crate::config::DatabaseConfig {
            path: directory.path().join("test.sqlite"),
            max_connections: 1,
            busy_timeout_seconds: 1,
        })
        .await
        .unwrap();
        database.migrate().await.unwrap();
        let mut moderator = live(url, 0.9, 8, Duration::from_secs(2));
        let Mode::Live(live) = &mut moderator.inner else {
            unreachable!();
        };
        live.refusals = Some(Repository::new(database));
        (moderator, directory)
    }

    /// A decision service that answers every check with `choice` and counts
    /// the calls.
    async fn counting(choice: &'static str, confidence: f64) -> (String, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let app = Router::new()
            .route(
                "/v1/systemone",
                post(move |State(calls): State<Arc<AtomicUsize>>| async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Json(json!({
                        "answers": { "publish": { "choice": choice, "confidence": confidence } }
                    }))
                }),
            )
            .with_state(calls.clone());
        let address = serve(app).await;
        (format!("http://{address}/v1/systemone"), calls)
    }

    #[tokio::test]
    async fn refuses_a_refused_file_again_without_asking() {
        let (url, calls) = counting("refuse", 0.99).await;
        let (moderator, _directory) = remembering(&url).await;
        let file = png(&picture(0.0));
        for _ in 0..2 {
            let error = moderator.screen(&file).await.unwrap_err();
            assert!(matches!(error, AppError::Unpublishable));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn refuses_a_near_copy_of_a_refused_image_without_asking() {
        let (url, calls) = counting("refuse", 0.99).await;
        let (moderator, _directory) = remembering(&url).await;
        moderator.screen(&png(&picture(0.0))).await.unwrap_err();
        let error = moderator
            .screen(&near_copy(&picture(0.0)))
            .await
            .unwrap_err();
        assert!(matches!(error, AppError::Unpublishable));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        moderator.screen(&png(&picture(0.4))).await.unwrap_err();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn does_not_remember_an_unsure_allow() {
        let (url, calls) = counting("allow", 0.2).await;
        let (moderator, _directory) = remembering(&url).await;
        let file = png(&picture(0.0));
        for _ in 0..2 {
            let error = moderator.screen(&file).await.unwrap_err();
            assert!(matches!(error, AppError::Unpublishable));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
}
