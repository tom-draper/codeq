use super::{text::Source, *};

pub(super) fn extract(language: &str, src: &[u8]) -> Result<Extracted> {
    if src.len() > 1_048_576 {
        return Ok(((Vec::new(), Vec::new(), Vec::new()), None, Vec::new()));
    }
    let text = std::str::from_utf8(src)?;
    let source = Source::new(text);
    if language == "xml" {
        return Ok(xml(&source));
    }
    let mut parser = Parser::new();
    parser.set_language(&tree_sitter_html::LANGUAGE.into())?;
    let tree = parser
        .parse(src, None)
        .context("HTML parser returned no tree")?;
    let mut facts = jinja_facts(src).0;
    let mut spans = Vec::new();
    let mut stack = vec![(tree.root_node(), 0)];
    while let Some((node, depth)) = stack.pop() {
        if depth > 64 {
            continue;
        }
        let mut cursor = node.walk();
        if node.kind() == "comment" {
            spans.push((node.start_byte(), node.end_byte(), Span::Comment));
            continue;
        }
        if node.kind() == "attribute_value" {
            spans.push((node.start_byte(), node.end_byte(), Span::Str));
        }
        if matches!(node.kind(), "element" | "script_element" | "style_element") {
            let tag = node
                .named_children(&mut cursor)
                .find(|n| matches!(n.kind(), "start_tag" | "self_closing_tag"));
            if let Some(tag) = tag {
                let mut tc = tag.walk();
                let tag_name = tag
                    .named_children(&mut tc)
                    .find(|n| n.kind() == "tag_name")
                    .and_then(|n| n.utf8_text(src).ok())
                    .unwrap_or("");
                let mut name = tag_name.to_string();
                let mut has_id = false;
                for attr in tag
                    .named_children(&mut tc)
                    .filter(|n| n.kind() == "attribute")
                {
                    let mut ac = attr.walk();
                    let key = attr
                        .named_children(&mut ac)
                        .find(|n| n.kind() == "attribute_name")
                        .and_then(|n| n.utf8_text(src).ok())
                        .unwrap_or("");
                    let value = attr
                        .named_children(&mut ac)
                        .find(|n| matches!(n.kind(), "quoted_attribute_value" | "attribute_value"));
                    if let Some(value) = value {
                        let raw = value.utf8_text(src).unwrap_or("").trim_matches(['\'', '"']);
                        let decoded = html_escape::decode_html_entities(raw);
                        let raw = decoded.as_ref();
                        if key.eq_ignore_ascii_case("id") {
                            name = raw.into();
                            has_id = true;
                        } else if !has_id && key.eq_ignore_ascii_case("name") {
                            name = raw.into();
                        }
                        if matches!(key.to_ascii_lowercase().as_str(), "href" | "src") {
                            if let Some(f) = source.import(
                                raw,
                                value.start_byte(),
                                value.end_byte(),
                                "tree-sitter/html",
                            ) {
                                facts.push(f);
                            }
                        }
                    }
                }
                if !name.is_empty() {
                    let mut f = source.fact(
                        "element",
                        &name,
                        node.start_byte(),
                        node.end_byte(),
                        "tree-sitter/html",
                    );
                    f.signature = cap(&collapse_ws(tag.utf8_text(src).unwrap_or("")), MAX_SIG);
                    facts.push(f);
                }
            }
        }
        stack.extend(node.named_children(&mut cursor).map(|n| (n, depth + 1)));
        if facts.len() > 10_000 {
            facts.clear();
            break;
        }
    }
    facts.sort_by_key(|f| (f.start_byte, f.end_byte));
    Ok((
        (facts, spans, Vec::new()),
        Some(parse_health(tree.root_node())),
        Vec::new(),
    ))
}

fn xml(source: &Source<'_>) -> Extracted {
    use quick_xml::{events::Event, Reader};
    struct Open {
        name: String,
        start: usize,
        signature: String,
        children: HashMap<String, usize>,
    }
    let mut reader = Reader::from_str(source.text);
    let mut stack = Vec::<Open>::new();
    let mut roots = HashMap::<String, usize>::new();
    let mut facts = Vec::new();
    let mut spans = Vec::new();
    let mut errors = 0;
    let mut skipped_depth = 0;
    loop {
        let start = reader.buffer_position() as usize;
        let event = reader.read_event();
        let end = reader.buffer_position() as usize;
        if skipped_depth > 0 {
            match event {
                Ok(Event::Start(_)) => skipped_depth += 1,
                Ok(Event::End(_)) => skipped_depth -= 1,
                Ok(Event::Eof) => {
                    errors += 1;
                    break;
                }
                Err(_) => {
                    errors += 1;
                    break;
                }
                _ => {}
            }
            continue;
        }
        match event {
            Ok(Event::Start(ref tag)) | Ok(Event::Empty(ref tag)) => {
                let empty = matches!(event, Ok(Event::Empty(_)));
                if stack.len() >= 64 {
                    if !empty {
                        skipped_depth = 1;
                    }
                    continue;
                }
                let tag_name = String::from_utf8_lossy(tag.name().as_ref()).into_owned();
                let counts = stack
                    .last_mut()
                    .map(|o| &mut o.children)
                    .unwrap_or(&mut roots);
                let count = counts.entry(tag_name.clone()).or_default();
                *count += 1;
                let count = *count;
                let mut name = format!(
                    "{}/{}[{}]",
                    stack.last().map(|o| o.name.as_str()).unwrap_or(""),
                    tag_name,
                    count
                );
                for attr in tag.attributes() {
                    let Ok(attr) = attr else {
                        errors += 1;
                        continue;
                    };
                    let Ok(value) = attr.decode_and_unescape_value(reader.decoder()) else {
                        errors += 1;
                        continue;
                    };
                    match attr.key.as_ref() {
                        b"id" | b"xml:id" => name = value.into_owned(),
                        b"href" | b"src" | b"xlink:href" => {
                            if let Some(f) = source.import(&value, start, end, "xml") {
                                facts.push(f);
                            }
                        }
                        _ => {}
                    }
                }
                let open = Open {
                    name,
                    start,
                    signature: cap(&collapse_ws(&source.text[start..end]), MAX_SIG),
                    children: HashMap::new(),
                };
                if empty {
                    if stack.len() < 64 {
                        let mut f = source.fact("element", &open.name, start, end, "xml");
                        f.signature = open.signature;
                        facts.push(f);
                    }
                } else {
                    stack.push(open);
                }
            }
            Ok(Event::End(_)) => {
                if let Some(open) = stack.pop() {
                    if stack.len() < 64 {
                        let mut f = source.fact("element", &open.name, open.start, end, "xml");
                        f.signature = open.signature;
                        facts.push(f);
                    }
                }
            }
            Ok(Event::Comment(_)) => spans.push((start, end, Span::Comment)),
            Ok(Event::CData(_)) | Ok(Event::Text(_)) => spans.push((start, end, Span::Str)),
            Ok(Event::Eof) => break,
            Err(_) => {
                errors += 1;
                break;
            }
            _ => {}
        }
        if facts.len() > 10_000 {
            facts.clear();
            break;
        }
    }
    if !stack.is_empty() {
        errors += 1;
    }
    facts.sort_by_key(|f| (f.start_byte, f.end_byte));
    (
        (facts, spans, Vec::new()),
        Some(ParseHealth {
            errors,
            recovered: errors > 0,
        }),
        Vec::new(),
    )
}
