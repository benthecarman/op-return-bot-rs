//! Check that a PDF stores nothing its pages do not show.
//!
//! The decision service sees the rendered pages and the text layer, but
//! the whole file is written on chain. These checks refuse a PDF with data
//! around the document, bytes outside any object, objects nothing refers
//! to, images no page draws, hidden layers, or an image file tucked inside
//! another object. Data hidden inside a font program, or an image drawn
//! under another shape, is not caught.

use std::collections::HashSet;

use lopdf::{
    Dictionary, Document, Object, ObjectId, Stream,
    content::Content,
    xref::{XrefEntry, XrefType},
};

use super::raster::{self, Refusal};

/// Bytes outside every object, beyond the cross-reference table, that a
/// PDF may have. The header, trailer, and line breaks fit in this.
const MAX_LOOSE_BYTES: usize = 1024;
/// Size of one entry in a cross-reference table.
const XREF_ENTRY_BYTES: usize = 20;
const MAX_DEPTH: usize = 32;

pub(super) fn check(bytes: &[u8], document: &Document) -> Result<(), Refusal> {
    surroundings(bytes)?;
    if document.encryption_state.is_some() || document.trailer.has(b"Encrypt") {
        return Err("pdf is encrypted");
    }
    let catalog = document
        .trailer
        .get_deref(b"Root", document)
        .and_then(Object::as_dict)
        .map_err(|_| "pdf has no catalog")?;
    if catalog.has(b"OCProperties") {
        return Err("pdf has optional layers");
    }
    loose_bytes(bytes, document)?;
    object_streams(document)?;
    reachable(document)?;
    let drawn = drawn_images(document)?;
    for (id, object) in &document.objects {
        if let Object::Stream(stream) = object
            && stream.dict.get(b"Subtype").and_then(Object::as_name).ok() == Some(b"Image")
            && !drawn.contains(id)
        {
            return Err("pdf has an image no page draws");
        }
    }
    signatures(document, &drawn)
}

fn surroundings(bytes: &[u8]) -> Result<(), Refusal> {
    if !bytes.starts_with(b"%PDF-") {
        return Err("pdf has data before its header");
    }
    let end = bytes
        .windows(5)
        .rposition(|window| window == b"%%EOF")
        .ok_or("pdf has no end marker")?
        + 5;
    if !bytes[end..].iter().all(u8::is_ascii_whitespace) {
        return Err("pdf has data after its end");
    }
    Ok(())
}

/// Every byte must belong to an object the cross-reference table points
/// at, apart from the table itself and a little framing. Old revisions
/// and bytes slipped between objects fail this.
fn loose_bytes(bytes: &[u8], document: &Document) -> Result<(), Refusal> {
    let table = &document.reference_table;
    let mut spans = Vec::new();
    for (&number, entry) in &table.entries {
        let XrefEntry::Normal { offset, generation } = *entry else {
            continue;
        };
        let Some(object) = document.objects.get(&(number, generation)) else {
            continue;
        };
        let start = usize::try_from(offset).map_err(|_| "pdf object offset is too large")?;
        let body_end = match object {
            Object::Stream(stream) => stream
                .start_position
                .map_or(start, |position| position + stream.content.len()),
            _ => start,
        };
        let Some(end) = find(bytes, b"endobj", body_end) else {
            continue;
        };
        spans.push((start, end + b"endobj".len()));
    }
    spans.sort_unstable();
    let mut covered = 0;
    let mut reached = 0;
    for (start, end) in spans {
        let start = start.max(reached);
        if end > start {
            covered += end - start;
            reached = end;
        }
    }
    let mut allowed = MAX_LOOSE_BYTES;
    if matches!(table.cross_reference_type, XrefType::CrossReferenceTable) {
        let entries = usize::try_from(table.size).map_err(|_| "pdf has too many objects")?;
        allowed = allowed.saturating_add(entries.saturating_mul(XREF_ENTRY_BYTES));
    }
    if bytes.len().saturating_sub(covered) > allowed {
        return Err("pdf has bytes outside its objects");
    }
    Ok(())
}

fn find(bytes: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    bytes
        .get(from..)?
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|position| from + position)
}

/// An object stream must hold only the objects the table says it holds.
fn object_streams(document: &Document) -> Result<(), Refusal> {
    for (id, object) in &document.objects {
        let Object::Stream(stream) = object else {
            continue;
        };
        if !stream.dict.has_type(b"ObjStm") {
            continue;
        }
        let count = stream
            .dict
            .get(b"N")
            .and_then(Object::as_i64)
            .map_err(|_| "pdf object stream has no count")?;
        let listed = document
            .reference_table
            .entries
            .values()
            .filter(|entry| {
                matches!(entry, XrefEntry::Compressed { container, .. } if *container == id.0)
            })
            .count();
        if usize::try_from(count).ok() != Some(listed) {
            return Err("pdf object stream holds objects the table skips");
        }
    }
    Ok(())
}

fn reachable(document: &Document) -> Result<(), Refusal> {
    let mut seen = HashSet::new();
    let mut pending: Vec<ObjectId> = [b"Root".as_slice(), b"Info"]
        .iter()
        .filter_map(|key| {
            document
                .trailer
                .get(key)
                .and_then(Object::as_reference)
                .ok()
        })
        .collect();
    while let Some(id) = pending.pop() {
        if !seen.insert(id) {
            continue;
        }
        if let Some(object) = document.objects.get(&id) {
            references(object, &mut pending, 0)?;
        }
    }
    let linearized = document
        .objects
        .values()
        .any(|object| object.as_dict().is_ok_and(|dict| dict.has(b"Linearized")));
    for (id, object) in &document.objects {
        if !seen.contains(id) && !is_structural(object, linearized) {
            return Err("pdf has an object nothing refers to");
        }
    }
    Ok(())
}

fn references(object: &Object, out: &mut Vec<ObjectId>, depth: usize) -> Result<(), Refusal> {
    if depth > MAX_DEPTH {
        return Err("pdf objects nest too deeply");
    }
    match object {
        Object::Reference(id) => out.push(*id),
        Object::Array(items) => {
            for item in items {
                references(item, out, depth + 1)?;
            }
        }
        Object::Dictionary(dict) => dict_references(dict, out, depth)?,
        Object::Stream(stream) => dict_references(&stream.dict, out, depth)?,
        _ => {}
    }
    Ok(())
}

fn dict_references(
    dict: &Dictionary,
    out: &mut Vec<ObjectId>,
    depth: usize,
) -> Result<(), Refusal> {
    for (_, value) in dict {
        references(value, out, depth + 1)?;
    }
    Ok(())
}

/// Object streams, cross-reference streams, and the linearization
/// dictionary and its hint stream are found through the table or by
/// offset, not through references. A stream length can be its own object,
/// which the reader folds into the stream. A bare number or an empty
/// container hides nothing.
fn is_structural(object: &Object, linearized: bool) -> bool {
    match object {
        Object::Stream(stream) => {
            stream.dict.has_type(b"ObjStm")
                || stream.dict.has_type(b"XRef")
                || (linearized && stream.dict.has(b"S") && stream.content.len() <= MAX_LOOSE_BYTES)
        }
        Object::Dictionary(dict) => dict.has(b"Linearized") || dict.is_empty(),
        Object::Array(items) => items.is_empty(),
        Object::Integer(_) | Object::Real(_) | Object::Null | Object::Boolean(_) => true,
        _ => false,
    }
}

/// Images that a page draws, directly or through a form, with their masks.
fn drawn_images(document: &Document) -> Result<HashSet<ObjectId>, Refusal> {
    let mut drawn = HashSet::new();
    let mut forms = HashSet::new();
    for page_id in document.get_pages().into_values() {
        let page = document
            .get_dictionary(page_id)
            .map_err(|_| "pdf page cannot be read")?;
        // A viewer shows the thumbnail, but it is not part of the render.
        if page.has(b"Thumb") {
            return Err("pdf has a page thumbnail");
        }
        let content = document
            .get_page_content(page_id)
            .map_err(|_| "pdf page content cannot be read")?;
        let resources = inherited_resources(document, page);
        draw(document, &content, resources, &mut drawn, &mut forms, 0)?;
    }
    let masks: Vec<ObjectId> = drawn
        .iter()
        .filter_map(|id| document.get_object(*id).and_then(Object::as_stream).ok())
        .flat_map(|stream| {
            [b"SMask".as_slice(), b"Mask"]
                .into_iter()
                .filter_map(|key| stream.dict.get(key).and_then(Object::as_reference).ok())
        })
        .collect();
    drawn.extend(masks);
    Ok(drawn)
}

fn draw(
    document: &Document,
    content: &[u8],
    resources: Option<&Dictionary>,
    drawn: &mut HashSet<ObjectId>,
    forms: &mut HashSet<ObjectId>,
    depth: usize,
) -> Result<(), Refusal> {
    if depth > MAX_DEPTH {
        return Err("pdf forms nest too deeply");
    }
    let content = Content::decode(content).map_err(|_| "pdf page content cannot be read")?;
    for operation in content.operations {
        if operation.operator != "Do" {
            continue;
        }
        let Some(name) = operation
            .operands
            .first()
            .and_then(|name| name.as_name().ok())
        else {
            continue;
        };
        let Some(id) = xobject(document, resources, name) else {
            continue;
        };
        let Ok(stream) = document.get_object(id).and_then(Object::as_stream) else {
            continue;
        };
        match stream.dict.get(b"Subtype").and_then(Object::as_name) {
            Ok(b"Image") => {
                drawn.insert(id);
            }
            Ok(b"Form") if forms.insert(id) => {
                let inner = stream
                    .dict
                    .get_deref(b"Resources", document)
                    .and_then(Object::as_dict)
                    .ok()
                    .or(resources);
                let content = stream
                    .decompressed_content()
                    .map_err(|_| "pdf form content cannot be read")?;
                draw(document, &content, inner, drawn, forms, depth + 1)?;
            }
            _ => {}
        }
    }
    Ok(())
}

/// A page uses the nearest `Resources` on itself or its ancestors.
fn inherited_resources<'a>(document: &'a Document, page: &'a Dictionary) -> Option<&'a Dictionary> {
    let mut node = page;
    for _ in 0..MAX_DEPTH {
        if let Ok(resources) = node
            .get_deref(b"Resources", document)
            .and_then(Object::as_dict)
        {
            return Some(resources);
        }
        node = node
            .get_deref(b"Parent", document)
            .and_then(Object::as_dict)
            .ok()?;
    }
    None
}

fn xobject(document: &Document, resources: Option<&Dictionary>, name: &[u8]) -> Option<ObjectId> {
    resources?
        .get_deref(b"XObject", document)
        .and_then(Object::as_dict)
        .ok()?
        .get(name)
        .and_then(Object::as_reference)
        .ok()
}

/// Starts of image and PDF files. One of these inside a string, a font, or
/// any other stream is a file stored where no page shows it.
const SIGNATURES: [&[u8]; 5] = [
    b"\x89PNG\r\n\x1a\n",
    b"GIF87a",
    b"GIF89a",
    b"WEBPVP8",
    b"%PDF-",
];

fn has_signature(bytes: &[u8]) -> bool {
    let jpeg = bytes.windows(4).any(|window| {
        window[..3] == [0xff, 0xd8, 0xff]
            && matches!(window[3], 0xc0..=0xc2 | 0xc4 | 0xdb | 0xe0..=0xef | 0xfe)
    });
    jpeg || SIGNATURES.iter().any(|signature| {
        bytes
            .windows(signature.len())
            .any(|window| window == *signature)
    })
}

fn signatures(document: &Document, drawn: &HashSet<ObjectId>) -> Result<(), Refusal> {
    for (id, object) in &document.objects {
        strings(object, 0)?;
        let Object::Stream(stream) = object else {
            continue;
        };
        if drawn.contains(id) {
            // The model sees what a drawn image draws. A JPEG still must not
            // carry a thumbnail or trailing data.
            if is_jpeg(stream)? {
                raster::check("image/jpeg", &stream.content)?;
            }
            continue;
        }
        if has_signature(&stream.content) {
            return Err("pdf hides a file inside another object");
        }
        if stream.dict.has(b"Filter")
            && let Ok(decoded) = stream.decompressed_content()
            && has_signature(&decoded)
        {
            return Err("pdf hides a file inside another object");
        }
    }
    Ok(())
}

fn is_jpeg(stream: &Stream) -> Result<bool, Refusal> {
    let filters = stream.filters().unwrap_or_default();
    if !filters.contains(&b"DCTDecode".as_slice()) {
        return Ok(false);
    }
    if filters.len() != 1 {
        return Err("pdf image wraps a JPEG in another filter");
    }
    Ok(true)
}

fn strings(object: &Object, depth: usize) -> Result<(), Refusal> {
    if depth > MAX_DEPTH {
        return Err("pdf objects nest too deeply");
    }
    match object {
        Object::String(bytes, _) if has_signature(bytes) => Err("pdf hides a file inside a string"),
        Object::Array(items) => items.iter().try_for_each(|item| strings(item, depth + 1)),
        Object::Dictionary(dict) => dict
            .iter()
            .try_for_each(|(_, value)| strings(value, depth + 1)),
        Object::Stream(stream) => stream
            .dict
            .iter()
            .try_for_each(|(_, value)| strings(value, depth + 1)),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use lopdf::{content::Operation, dictionary};

    use super::*;
    use crate::moderation::tests::pdf_with_pages;

    /// A JPEG shape with a JFIF header, tables, a frame, and a scan.
    const JPEG: &[u8] = b"\xff\xd8\
\xff\xe0\x00\x10JFIF\x00\x01\x01\x00\x00\x01\x00\x01\x00\x00\
\xff\xdb\x00\x04\x00\x01\
\xff\xc0\x00\x05\x08\x00\x01\
\xff\xda\x00\x03\x01\x12\x34\
\xff\xd9";

    fn load(bytes: &[u8]) -> Document {
        Document::load_mem(bytes).unwrap()
    }

    fn save(document: &mut Document) -> Vec<u8> {
        let mut bytes = Vec::new();
        document.save_to(&mut bytes).unwrap();
        bytes
    }

    fn first_page(document: &Document) -> ObjectId {
        *document.get_pages().values().next().unwrap()
    }

    /// Add an image to the first page's resources, and draw it when `draw`
    /// is set.
    fn with_image(content: &[u8], draw: bool) -> Vec<u8> {
        let mut document = load(&pdf_with_pages(&["Hello"]));
        let image = document.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Image",
                "Width" => 1,
                "Height" => 1,
                "ColorSpace" => "DeviceGray",
                "BitsPerComponent" => 8,
                "Filter" => "DCTDecode",
            },
            content.to_vec(),
        ));
        let page_id = first_page(&document);
        let resources_id = document
            .get_dictionary(page_id)
            .unwrap()
            .get(b"Resources")
            .and_then(Object::as_reference)
            .unwrap();
        document
            .get_dictionary_mut(resources_id)
            .unwrap()
            .set("XObject", dictionary! { "Im1" => image });
        if draw {
            let content = Content {
                operations: vec![
                    Operation::new("q", vec![]),
                    Operation::new(
                        "cm",
                        vec![10.into(), 0.into(), 0.into(), 10.into(), 0.into(), 0.into()],
                    ),
                    Operation::new("Do", vec![Object::Name(b"Im1".to_vec())]),
                    Operation::new("Q", vec![]),
                ],
            };
            let content_id =
                document.add_object(Stream::new(dictionary! {}, content.encode().unwrap()));
            let page = document.get_dictionary_mut(page_id).unwrap();
            let first = page.get(b"Contents").unwrap().clone();
            page.set("Contents", vec![first, content_id.into()]);
        }
        save(&mut document)
    }

    fn refusal(bytes: &[u8]) -> Refusal {
        check(bytes, &load(bytes)).unwrap_err()
    }

    #[test]
    fn accepts_a_plain_pdf() {
        let bytes = pdf_with_pages(&["Alpha", "Beta"]);
        check(&bytes, &load(&bytes)).unwrap();
    }

    #[test]
    fn accepts_a_drawn_image() {
        let bytes = with_image(JPEG, true);
        check(&bytes, &load(&bytes)).unwrap();
    }

    #[test]
    fn refuses_an_image_no_page_draws() {
        assert_eq!(
            refusal(&with_image(JPEG, false)),
            "pdf has an image no page draws"
        );
    }

    #[test]
    fn refuses_a_drawn_jpeg_with_hidden_data() {
        let mut jpeg = JPEG.to_vec();
        jpeg.extend_from_slice(b"hidden");
        assert_eq!(
            refusal(&with_image(&jpeg, true)),
            "jpeg has data after its end"
        );
    }

    #[test]
    fn refuses_data_around_the_document() {
        let mut trailing = pdf_with_pages(&["Hello"]);
        trailing.extend_from_slice(b"\xff\xd8\xff\xe0 hidden");
        assert_eq!(refusal(&trailing), "pdf has data after its end");
        let mut leading = b"hello\n".to_vec();
        leading.extend_from_slice(&pdf_with_pages(&["Hello"]));
        assert_eq!(refusal(&leading), "pdf has data before its header");
    }

    #[test]
    fn refuses_bytes_between_objects() {
        let bytes = pdf_with_pages(&["Hello"]);
        let marker = find(&bytes, b"startxref", 0).unwrap();
        let xref: usize = std::str::from_utf8(&bytes[marker + 9..bytes.len() - 5])
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let junk = vec![b'%'; MAX_LOOSE_BYTES * 2];
        let mut spliced = bytes[..xref].to_vec();
        spliced.extend_from_slice(&junk);
        spliced.extend_from_slice(&bytes[xref..marker]);
        spliced.extend_from_slice(format!("startxref\n{}\n%%EOF", xref + junk.len()).as_bytes());
        assert_eq!(refusal(&spliced), "pdf has bytes outside its objects");
    }

    #[test]
    fn refuses_an_object_nothing_refers_to() {
        let mut document = load(&pdf_with_pages(&["Hello"]));
        document.add_object(Stream::new(dictionary! {}, b"loose".to_vec()));
        assert_eq!(
            refusal(&save(&mut document)),
            "pdf has an object nothing refers to"
        );
    }

    #[test]
    fn refuses_a_file_inside_another_object() {
        let mut document = load(&pdf_with_pages(&["Hello"]));
        let hidden = document.add_object(Stream::new(dictionary! {}, JPEG.to_vec()));
        let catalog = document
            .trailer
            .get(b"Root")
            .and_then(Object::as_reference)
            .unwrap();
        document
            .get_dictionary_mut(catalog)
            .unwrap()
            .set("Junk", hidden);
        assert_eq!(
            refusal(&save(&mut document)),
            "pdf hides a file inside another object"
        );

        let mut document = load(&pdf_with_pages(&["Hello"]));
        let catalog = document
            .trailer
            .get(b"Root")
            .and_then(Object::as_reference)
            .unwrap();
        document
            .get_dictionary_mut(catalog)
            .unwrap()
            .set("Junk", Object::string_literal(JPEG.to_vec()));
        assert_eq!(
            refusal(&save(&mut document)),
            "pdf hides a file inside a string"
        );
    }

    #[test]
    fn refuses_a_page_thumbnail() {
        let mut document = load(&pdf_with_pages(&["Hello"]));
        let thumb = document.add_object(Stream::new(
            dictionary! { "Width" => 1, "Height" => 1, "BitsPerComponent" => 8 },
            vec![0],
        ));
        let page_id = first_page(&document);
        document
            .get_dictionary_mut(page_id)
            .unwrap()
            .set("Thumb", thumb);
        assert_eq!(refusal(&save(&mut document)), "pdf has a page thumbnail");
    }

    #[test]
    fn refuses_optional_layers() {
        let mut document = load(&pdf_with_pages(&["Hello"]));
        let catalog = document
            .trailer
            .get(b"Root")
            .and_then(Object::as_reference)
            .unwrap();
        document
            .get_dictionary_mut(catalog)
            .unwrap()
            .set("OCProperties", dictionary! {});
        assert_eq!(refusal(&save(&mut document)), "pdf has optional layers");
    }
}
