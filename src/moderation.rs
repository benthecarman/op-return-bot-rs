//! Screen a file before it is stored, and a public message before it is posted.
//!
//! A payload that is not plain text is stored only when the decision
//! service chooses `allow` with confidence at or above the configured
//! floor. Plain text is always stored. Twitter and Nostr hear about it only when a separate decision
//! allows the post.

use std::{collections::BTreeMap, time::Duration};

use base64::{Engine, engine::general_purpose::STANDARD};
use image::ImageEncoder;
use serde::Deserialize;
use url::Url;

use crate::{AppError, AppResult, config::ModerationConfig};

const PUBLISH_INSTRUCTIONS: &str = "Can this be published on a public permanent ledger?";
const PUBLISH_ALLOW: &str = "Content that is legal to publish.";
const PUBLISH_REFUSE: &str = "Sexual content involving anyone under 18, or other content that is illegal to publish or possess.";
const POST_INSTRUCTIONS: &str = "Can this be posted from a public social account?";
const POST_ALLOW: &str = "Ordinary speech, including rude or political speech.";
const POST_REFUSE: &str = "A slur, sexual content involving anyone under 18, or other content that would get a public social account banned.";
const IMAGE_NOTE: &str = "The attached image is the file to publish.";
const PDF_NO_TEXT: &str = "This PDF has no text layer. The attached images are its pages.";
/// A text layer longer than this is not sent. The file is refused instead,
/// so the decision is not made on a clipped document.
const MAX_STATE_BYTES: usize = 200_000;
/// Longest side of a rendered PDF page, in pixels.
const TARGET_LONG_SIDE: f32 = 1024.0;
/// Stay under the renderer's `u16` pixel limit.
const MAX_PIXEL: f32 = 2048.0;
const MAX_PDF_DEPTH: usize = 32;
/// Lowercase markers of SVG markup and `data:` image URLs. The namespace
/// catches an SVG root that uses another prefix.
const DRAWN_TEXT_MARKERS: [&[u8]; 3] = [b"<svg", b"www.w3.org/2000/svg", b"data:image/"];
/// How far into a payload a PDF reader looks for the `%PDF-` header.
const PDF_HEADER_WINDOW: usize = 1024;

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

struct LiveModerator {
    client: reqwest::Client,
    url: Url,
    api_key: String,
    floor: f64,
    max_pdf_pages: usize,
    timeout: Duration,
}

impl Clone for LiveModerator {
    fn clone(&self) -> Self {
        Self {
            client: self.client.clone(),
            url: self.url.clone(),
            api_key: self.api_key.clone(),
            floor: self.floor,
            max_pdf_pages: self.max_pdf_pages,
            timeout: self.timeout,
        }
    }
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
}

impl Moderator {
    pub async fn connect(config: &ModerationConfig) -> AppResult<Self> {
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
            })),
        })
    }

    /// Screen a payload before it is stored.
    ///
    /// Plain text is stored without asking the decision service, unless it
    /// is markup that draws an image. Anything else must pass the decision
    /// service, whichever form field it came from.
    pub async fn screen(&self, bytes: &[u8]) -> AppResult<()> {
        let kind = classify(bytes);
        if kind == FileKind::Text {
            return refuse_drawn_text(bytes);
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
    /// An unconfigured check posts. A configured check posts only on an
    /// allow at or above the floor. A refusal, a low confidence, or a
    /// check that does not answer skips the posts. The transaction is
    /// unaffected.
    #[must_use]
    pub async fn allows_post(&self, text: &str) -> bool {
        if text.trim().is_empty() {
            return true;
        }
        match &self.inner {
            Mode::Skip | Mode::Closed => true,
            Mode::Live(live) => live.allows_post(text).await,
        }
    }
}

impl LiveModerator {
    async fn screen(&self, bytes: &[u8], kind: FileKind) -> AppResult<()> {
        let owned = bytes.to_vec();
        let max_pages = self.max_pdf_pages;
        // The render keeps running after this wait ends. The upload limit
        // bounds how often that happens, and the page cap bounds each render.
        let submission = tokio::time::timeout(
            self.timeout,
            tokio::task::spawn_blocking(move || prepare(&owned, kind, max_pages)),
        )
        .await
        .map_err(|_| {
            tracing::warn!("file screen preparation timed out");
            AppError::Unpublishable
        })?
        .map_err(|_| {
            tracing::error!("file screen preparation panicked");
            AppError::FileCheckFailed
        })??;
        tracing::info!(
            kind = submission.kind,
            files = submission.files.len(),
            "file screen asking for a decision"
        );
        let response = self
            .client
            .post(self.url.clone())
            .bearer_auth(&self.api_key)
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
            Verdict::Refuse => Err(AppError::Unpublishable),
            Verdict::Unusable => Err(AppError::FileCheckFailed),
        }
    }

    async fn allows_post(&self, text: &str) -> bool {
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
                return false;
            }
        };
        let status = response.status();
        if !status.is_success() {
            tracing::warn!(%status, "social post check was rejected");
            return false;
        }
        let body = match response.json().await {
            Ok(body) => body,
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "social post check returned a decision that could not be read"
                );
                return false;
            }
        };
        interpret_decision(&body, POST.name, self.floor, "social post check") == Verdict::Allow
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verdict {
    Allow,
    Refuse,
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
        return Verdict::Refuse;
    }
    let shown: String = choice.chars().take(32).collect();
    tracing::warn!(check, choice = %shown, "content check returned an unusable choice");
    Verdict::Unusable
}

fn prepare(bytes: &[u8], kind: FileKind, max_pages: usize) -> AppResult<Submission> {
    match kind {
        // Text never reaches this point. It is stored without a check.
        FileKind::Text | FileKind::Unsupported => {
            tracing::info!("file screen refused an unsupported file");
            Err(AppError::Unpublishable)
        }
        FileKind::Image(mime) => Ok(Submission {
            state: IMAGE_NOTE.to_owned(),
            files: vec![data_url(mime, bytes)],
            kind: "image",
        }),
        FileKind::Pdf => prepare_pdf(bytes, max_pages),
    }
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

/// Text that a browser draws as an image when the bytes are saved as a
/// file. The decision service would only read the markup, so this text is
/// refused instead of checked.
fn refuse_drawn_text(bytes: &[u8]) -> AppResult<()> {
    let lower = bytes.to_ascii_lowercase();
    let draws = DRAWN_TEXT_MARKERS
        .iter()
        .any(|marker| lower.windows(marker.len()).any(|window| window == *marker));
    if draws {
        tracing::info!("refused text that contains SVG or an inline image");
        return Err(AppError::InvalidRequest(
            "SVG and inline images cannot be published".to_owned(),
        ));
    }
    Ok(())
}

/// PDF readers look for the header anywhere in the first kilobyte, so a
/// text prefix must not hide a PDF from the check.
fn is_pdf(bytes: &[u8]) -> bool {
    bytes[..bytes.len().min(PDF_HEADER_WINDOW)]
        .windows(5)
        .any(|window| window == b"%PDF-")
}

fn prepare_pdf(bytes: &[u8], max_pages: usize) -> AppResult<Submission> {
    if embedded_file(bytes)? {
        tracing::info!("file screen refused a pdf with an embedded file");
        return Err(AppError::Unpublishable);
    }
    let state = pdf_state(bytes)?;
    let pdf = hayro::hayro_syntax::Pdf::new(bytes.to_vec()).map_err(|_| {
        tracing::info!("file screen refused a pdf it could not read");
        AppError::Unpublishable
    })?;
    let files = render_pages(&pdf, max_pages)?;
    Ok(Submission {
        state,
        files,
        kind: "pdf",
    })
}

fn embedded_file(bytes: &[u8]) -> AppResult<bool> {
    let document = lopdf::Document::load_mem(bytes).map_err(|_| {
        tracing::info!("file screen refused a pdf it could not read");
        AppError::Unpublishable
    })?;
    Ok(document
        .objects
        .values()
        .any(|object| object_has_embedded(object, 0)))
}

fn object_has_embedded(object: &lopdf::Object, depth: usize) -> bool {
    if depth > MAX_PDF_DEPTH {
        return true;
    }
    match object {
        lopdf::Object::Dictionary(dict) => dict_has_embedded(dict, depth),
        lopdf::Object::Stream(stream) => dict_has_embedded(&stream.dict, depth),
        lopdf::Object::Array(items) => items
            .iter()
            .any(|item| object_has_embedded(item, depth + 1)),
        _ => false,
    }
}

fn dict_has_embedded(dict: &lopdf::Dictionary, depth: usize) -> bool {
    if dict.has(b"EF") || dict.has(b"EmbeddedFiles") || dict.has(b"AF") {
        return true;
    }
    if let Ok(lopdf::Object::Name(name)) = dict.get(b"Type")
        && name.as_slice() == b"EmbeddedFile"
    {
        return true;
    }
    dict.as_hashmap()
        .values()
        .any(|value| object_has_embedded(value, depth + 1))
}

fn pdf_state(bytes: &[u8]) -> AppResult<String> {
    let extracted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        pdf_extract::extract_text_from_mem(bytes)
    }));
    let Ok(Ok(text)) = extracted else {
        tracing::info!("file screen could not read the pdf text layer");
        return Ok(PDF_NO_TEXT.to_owned());
    };
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(PDF_NO_TEXT.to_owned());
    }
    if trimmed.len() > MAX_STATE_BYTES {
        tracing::info!("file screen refused a pdf whose text layer is too long");
        return Err(AppError::Unpublishable);
    }
    Ok(trimmed.to_owned())
}

fn render_pages(pdf: &hayro::hayro_syntax::Pdf, max_pages: usize) -> AppResult<Vec<String>> {
    let pages = pdf.pages();
    if pages.is_empty() {
        tracing::info!("file screen refused a pdf with no pages");
        return Err(AppError::Unpublishable);
    }
    if pages.len() > max_pages {
        tracing::info!(
            pages = pages.len(),
            max_pages,
            "file screen refused a pdf with too many pages"
        );
        return Err(AppError::Unpublishable);
    }
    let cache = hayro::RenderCache::new();
    let interpreter = hayro::hayro_interpret::InterpreterSettings::default();
    let render_settings = hayro::RenderSettings::default();
    let mut files = Vec::with_capacity(pages.len());
    for page in pages.iter() {
        let rendered = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            render_page(page, &cache, &interpreter, render_settings)
        }));
        if let Ok(Some(png)) = rendered {
            files.push(data_url("image/png", &png));
        } else {
            tracing::info!("file screen refused a pdf page it could not render");
            return Err(AppError::Unpublishable);
        }
    }
    Ok(files)
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
        sync::{Arc, Mutex},
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

    fn prepare_file(bytes: &[u8], max_pages: usize) -> AppResult<Submission> {
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
        assert_eq!(verdict(Some("allow"), Some(0.89)), Verdict::Refuse);
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
        moderator.screen(&[0xff, 0x00]).await.unwrap();
    }

    #[tokio::test]
    async fn refuses_a_file_when_the_check_is_not_configured() {
        let moderator = Moderator {
            inner: Mode::Closed,
        };
        let error = moderator.screen(IMAGE).await.unwrap_err();
        assert!(matches!(error, AppError::FileCheckFailed));
        assert!(moderator.allows_post("hello").await);
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
    async fn refuses_text_that_draws_an_image() {
        let moderator = Moderator {
            inner: Mode::Closed,
        };
        for text in [
            &b"<SVG viewBox='0 0 1 1'><rect/></SVG>"[..],
            b"<x:svg xmlns:x=\"http://www.w3.org/2000/svg\"/>",
            b"<img src=\"DATA:image/png;base64,iVBORw0KGgo=\">",
        ] {
            let error = moderator.screen(text).await.unwrap_err();
            assert!(matches!(error, AppError::InvalidRequest(_)));
        }
        moderator
            .screen(b"an svg is an image format")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn screens_binary_bytes_from_any_field() {
        let moderator = Moderator {
            inner: Mode::Closed,
        };
        let error = moderator.screen(b"\xff\x00hello").await.unwrap_err();
        assert!(matches!(error, AppError::FileCheckFailed));
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
    async fn refuses_an_unsupported_file_without_asking() {
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
        let error = moderator.screen(b"\xff\x00hello").await.unwrap_err();
        assert!(matches!(error, AppError::Unpublishable));
        assert!(seen.lock().unwrap().is_none());
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

    #[test]
    fn refuses_a_pdf_with_an_embedded_file() {
        let error = prepare_file(&embedded_pdf(), 8).unwrap_err();
        assert!(matches!(error, AppError::Unpublishable));
    }

    #[test]
    fn refuses_a_pdf_with_too_many_pages() {
        let error = prepare_file(&pdf_with_pages(&["One", "Two"]), 1).unwrap_err();
        assert!(matches!(error, AppError::Unpublishable));
    }

    #[test]
    fn refuses_a_pdf_that_does_not_parse() {
        let error = prepare_file(b"%PDF-1.7 not a pdf", 8).unwrap_err();
        assert!(matches!(error, AppError::Unpublishable));
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
        assert!(moderator.allows_post("hello").await);
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
        assert!(moderator.allows_post("  ").await);
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
        assert!(!moderator.allows_post("hello").await);

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
        assert!(!moderator.allows_post("hello").await);
    }
}
