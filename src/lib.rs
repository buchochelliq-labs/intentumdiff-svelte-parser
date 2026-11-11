//! Svelte component parser plugin — full-parse mode.
//!
//! Handles `.svelte` single-file component files.
//!
//! Svelte SFCs consist of optional top-level `<script>`, `<script module>`
//! (Svelte 4: `context="module"`) and `<style>` blocks; everything else,
//! including a `<script>` nested in `<svelte:head>`, is template markup.
//!
//! Semantic nodes produced:
//!   svelte_component     — root; label = filename stem
//!   script_block         — top-level `<script ...>` (label includes "module" for module context)
//!   style_block          — top-level `<style ...>` (label includes "global" if global)
//!   attribute            — other block/element attributes (`src=./a.js`)
//!   template_body        — the remaining markup outside of script/style blocks
//!   element / interpolation / block_tag / text — template structure and payload
//!   variable_declaration / assignment_statement / expression_statement / ... — script

use intentumdiff_plugin_sdk::tree::{SemanticNode, SemanticNodeBuilder};

wit_bindgen::generate!({
    path: "wit/plugin.wit",
    world: "parser-plugin",
});

use crate::exports::intentdiff::plugin::parser::ExamplePair;
use crate::exports::intentdiff::plugin::parser::Guest;
use crate::exports::intentdiff::plugin::parser::LanguageInfoRecord;
use crate::exports::intentdiff::plugin::parser::ParserMode;

const PLUGIN_METADATA: &str = include_str!("../plugin_metadata.info");

fn language_info_for(ids: Vec<String>) -> Vec<LanguageInfoRecord> {
    let metadata = intentumdiff_plugin_sdk::metadata::parse_plugin_metadata(PLUGIN_METADATA);
    ids.into_iter()
        .map(|language_id| {
            let info = metadata.language_or_default(&language_id);
            LanguageInfoRecord {
                language_id: info.language_id,
                language_name: info.language_name,
                language_short_name: info.language_short_name,
                monaco_language: info.monaco_language,
                default_filename: info.default_filename,
                language_file_extensions: info.language_file_extensions,
                author: metadata.author().to_string(),
                plugin_version: metadata.plugin_version().to_string(),
                last_updated: metadata.last_updated().to_string(),
            }
        })
        .collect()
}
struct SvelteParser;

// ---------------------------------------------------------------------------
// Block extraction (shared with vue-parser approach)
// ---------------------------------------------------------------------------

struct SfcBlock {
    unclosed: bool,
    node_type: &'static str,
    label: String,
    start_line: u32,
    start_col: u32,
    end_line: u32,
    end_col: u32,
    content_start_line: u32,
    content_start_col: u32,
    content: String,
    content_hash: String,
    /// Raw attribute text of the opening tag (script/style blocks only).
    attrs: String,
    /// Byte offset in the source where `attrs` search starts (after the tag name).
    attrs_offset: usize,
}

fn content_hash(s: &str) -> String {
    let mut h: u64 = 5381;
    for b in s.bytes() {
        h = h.wrapping_mul(33).wrapping_add(b as u64);
    }
    format!("{:016x}", h)
}

fn parse_tag_open(line: &str) -> Option<(String, String)> {
    let rest = line.trim_start().strip_prefix('<')?;
    let tag_end = rest
        .find(|c: char| c.is_whitespace() || c == '>' || c == '/')
        .unwrap_or(rest.len());
    let tag = &rest[..tag_end];
    if tag.is_empty() || tag.starts_with('/') || tag.starts_with('!') {
        return None;
    }
    let attrs = rest[tag_end..].trim().trim_end_matches('>').to_string();
    Some((tag.to_lowercase(), attrs))
}

fn attr_contains(attrs: &str, keyword: &str) -> bool {
    attrs.to_lowercase().contains(keyword)
}

fn attr_lang(attrs: &str) -> Option<String> {
    for part in attrs.split_whitespace() {
        let p = part.to_lowercase();
        if let Some(rest) = p.strip_prefix("lang=") {
            return Some(rest.trim_matches('"').trim_matches('\'').to_string());
        }
    }
    None
}

/// Byte offset just past the `>` closing the tag at `start`. Quoted attribute
/// values and `{expression}` attribute values (`on:click={() => a > b}`) are
/// skipped, so a `>` inside them does not end the tag.
fn tag_end(source: &str, start: usize) -> Option<usize> {
    let mut quote = None;
    let mut index = start;
    while let Some(c) = source[index..].chars().next() {
        match (quote, c) {
            (Some(q), c) if q == c => quote = None,
            (None, '\'' | '"') => quote = Some(c),
            (None, '{') => {
                index = expression_end(source, index)?;
                continue;
            }
            (None, '>') => return Some(index + 1),
            _ => {}
        }
        index += c.len_utf8();
    }
    None
}

/// Byte offset just past the `}` matching the `{` at `start`, skipping nested
/// braces and JavaScript string/template literals; `None` if it never closes.
fn expression_end(source: &str, start: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut depth = 0usize;
    let mut quote: Option<u8> = None;
    let mut i = start;
    while i < bytes.len() {
        let b = bytes[i];
        match quote {
            Some(_) if b == b'\\' => i += 1,
            Some(q) if b == q => quote = None,
            Some(_) => {}
            None if matches!(b, b'"' | b'\'' | b'`') => quote = Some(b),
            None if b == b'{' => depth += 1,
            None if b == b'}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            None => {}
        }
        i += 1;
    }
    None
}

/// Byte span `(start, end)` of the first `</tag>` at or after `from`.
fn find_closing_tag(source: &str, lower: &str, from: usize, tag: &str) -> Option<(usize, usize)> {
    let closing = format!("</{tag}");
    let mut close_search = from;
    while let Some(relative) = lower[close_search..].find(&closing) {
        let close_start = close_search + relative;
        let boundary = close_start + closing.len();
        if source[boundary..].starts_with('>') || source[boundary..].starts_with(char::is_whitespace)
        {
            return tag_end(source, close_start).map(|close_end| (close_start, close_end));
        }
        close_search = boundary;
    }
    None
}

/// Element names that can hold children (`div`, `svelte:head`, `My.Component`).
fn is_element_name(tag: &str) -> bool {
    let mut chars = tag.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '.' | '-' | '_'))
}

/// HTML void elements, plus elements whose end tag may be omitted; neither is
/// tracked as an open parent so an implicit close cannot hide a top-level block.
fn is_void_or_optional_close(tag: &str) -> bool {
    matches!(
        tag,
        "area" | "base" | "br" | "col" | "embed" | "hr" | "img" | "input" | "link" | "meta"
            | "param" | "source" | "track" | "wbr" | "p" | "li" | "dt" | "dd" | "option"
            | "optgroup" | "tr" | "td" | "th" | "thead" | "tbody" | "tfoot" | "rp" | "rt"
            | "colgroup" | "caption" | "html" | "head" | "body"
    )
}

/// `<script module>` (Svelte 5) or `<script context="module">` (Svelte 4).
fn is_module_script(attrs: &str) -> bool {
    split_attrs(attrs).iter().any(|(raw, _, _)| {
        let (name, value) = attr_name_value(raw);
        let name = name.to_ascii_lowercase();
        (name == "module" && value.is_empty())
            || (name == "context" && value.eq_ignore_ascii_case("module"))
    })
}

/// Attributes already encoded in a block label (`lang`, module context, `global`).
fn is_block_label_attr(tag: &str, name: &str, value: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name == "lang"
        || (tag == "script" && name == "module" && value.is_empty())
        || (tag == "script" && name == "context" && value.eq_ignore_ascii_case("module"))
        || (tag == "style" && name == "global" && value.is_empty())
}

/// Quote- and brace-aware attribute split: `(raw, start, end)` byte spans in `attrs`.
fn split_attrs(attrs: &str) -> Vec<(String, usize, usize)> {
    let mut out = Vec::new();
    let mut start: Option<usize> = None;
    let mut quote: Option<char> = None;
    let mut braces = 0usize;
    for (index, c) in attrs.char_indices() {
        match (quote, c) {
            (Some(q), c) if q == c => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') if start.is_some() => quote = Some(c),
            (None, '{') => {
                braces += 1;
                start.get_or_insert(index);
            }
            (None, '}') => braces = braces.saturating_sub(1),
            (None, c) if c.is_whitespace() && braces == 0 => {
                if let Some(s) = start.take() {
                    out.push((attrs[s..index].to_string(), s, index));
                }
            }
            (None, _) => {
                start.get_or_insert(index);
            }
        }
    }
    if let Some(s) = start {
        out.push((attrs[s..].to_string(), s, attrs.len()));
    }
    out
}

/// `name="value"` -> (`name`, `value`), stripping quotes; a `{expr}` value keeps
/// its expression, normalized so quote spelling and spacing are not semantic.
fn attr_name_value(raw: &str) -> (String, String) {
    let clean = raw.trim().trim_end_matches('>').trim_end_matches('/').trim();
    let (name, value) = clean.split_once('=').unwrap_or((clean, ""));
    let value = value.trim();
    let value = if value.len() >= 2
        && ((value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\'')))
    {
        value[1..value.len() - 1].to_string()
    } else {
        value.to_string()
    };
    let value = match value.strip_prefix('{').and_then(|v| v.strip_suffix('}')) {
        Some(expr) => normalize_js(expr, &mut false),
        None => value,
    };
    (name.trim().to_string(), value)
}

/// Canonical JavaScript expression text for labels: comments dropped, `'x'` and
/// `"x"` spelled the same, layout whitespace removed where it cannot change
/// tokenization. Template literals are kept verbatim. `in_comment` carries an
/// open `/* */` comment across lines.
fn normalize_js(text: &str, in_comment: &mut bool) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::new();
    let mut pending_space = false;
    let mut i = 0;
    let push_token = |out: &mut String, pending_space: &mut bool, c: char| {
        if *pending_space {
            if let Some(last) = out.chars().last() {
                let word = |ch: char| ch.is_alphanumeric() || ch == '_' || ch == '$';
                if (word(last) && word(c)) || (last == c && matches!(c, '+' | '-')) {
                    out.push(' ');
                }
            }
            *pending_space = false;
        }
        out.push(c);
    };
    while i < chars.len() {
        let c = chars[i];
        if *in_comment {
            if c == '*' && chars.get(i + 1) == Some(&'/') {
                *in_comment = false;
                pending_space = true;
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        match c {
            '/' if chars.get(i + 1) == Some(&'/') => break,
            '/' if chars.get(i + 1) == Some(&'*') => {
                *in_comment = true;
                i += 2;
            }
            '\'' | '"' => {
                let mut value = String::new();
                i += 1;
                while i < chars.len() && chars[i] != c {
                    if chars[i] == '\\' && i + 1 < chars.len() {
                        let next = chars[i + 1];
                        if next == '\'' || next == '"' {
                            value.push(next);
                        } else {
                            value.push('\\');
                            value.push(next);
                        }
                        i += 2;
                    } else {
                        value.push(chars[i]);
                        i += 1;
                    }
                }
                i += 1;
                push_token(&mut out, &mut pending_space, '"');
                for ch in value.chars() {
                    if ch == '"' {
                        out.push('\\');
                    }
                    out.push(ch);
                }
                out.push('"');
            }
            // A regular-expression literal: whitespace inside it is pattern
            // payload (`/a +b/` is not `/a+b/`), so copy it verbatim.
            '/' if regex_allowed(&out) => {
                push_token(&mut out, &mut pending_space, '/');
                i += 1;
                let mut in_class = false;
                while i < chars.len() {
                    let ch = chars[i];
                    out.push(ch);
                    i += 1;
                    match ch {
                        '\\' if i < chars.len() => {
                            out.push(chars[i]);
                            i += 1;
                        }
                        '[' => in_class = true,
                        ']' => in_class = false,
                        '/' if !in_class => break,
                        _ => {}
                    }
                }
                while i < chars.len() && chars[i].is_ascii_alphabetic() {
                    out.push(chars[i]);
                    i += 1;
                }
            }
            '`' => {
                push_token(&mut out, &mut pending_space, '`');
                i += 1;
                while i < chars.len() && chars[i] != '`' {
                    if chars[i] == '\\' && i + 1 < chars.len() {
                        out.push(chars[i]);
                        i += 1;
                    }
                    out.push(chars[i]);
                    i += 1;
                }
                if i < chars.len() {
                    out.push('`');
                }
                i += 1;
            }
            c if c.is_whitespace() => {
                pending_space = true;
                i += 1;
            }
            c => {
                push_token(&mut out, &mut pending_space, c);
                i += 1;
            }
        }
    }
    out.trim_end_matches([';', ',']).trim().to_string()
}

/// Whether a `/` after the canonical text `out` starts a regular-expression
/// literal rather than a division: at the start, after an operator or opening
/// punctuator, or after a keyword such as `return`/`typeof`.
fn regex_allowed(out: &str) -> bool {
    let Some(last) = out.chars().last() else {
        return true;
    };
    if "(,=:[!&|?{};+-*%<>~^".contains(last) {
        return true;
    }
    let word: String = out
        .chars()
        .rev()
        .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '$')
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    matches!(
        word.as_str(),
        "return" | "typeof" | "instanceof" | "in" | "of" | "new" | "delete" | "void"
            | "throw" | "case" | "do" | "else" | "yield" | "await"
    )
}

// Source spans are 0-based UTF-8 byte columns and have exclusive ends.
fn source_position(source: &str, offset: usize) -> (u32, u32) {
    let prefix = &source[..offset];
    let line = prefix.as_bytes().iter().filter(|&&b| b == b'\n').count() as u32;
    let line_start = prefix.rfind('\n').map_or(0, |index| index + 1);
    (line, (offset - line_start) as u32)
}

fn extract_blocks(source: &str) -> Vec<SfcBlock> {
    let mut blocks = Vec::new();
    let lower = source.to_ascii_lowercase();
    let mut template = source.as_bytes().to_vec();
    let mut cursor = 0;
    let mut open_elements: Vec<String> = Vec::new();
    while let Some(relative) = source[cursor..].find(['<', '{']) {
        let start = cursor + relative;
        // A template expression (`{a < b}`) is JavaScript, not markup: skip it
        // whole so a `<` inside it is never read as a tag. An unclosed `{` is text.
        if source[start..].starts_with('{') {
            cursor = expression_end(source, start).unwrap_or(start + 1);
            continue;
        }
        // A commented example is template trivia, not an executable script block.
        if source[start..].starts_with("<!--") {
            cursor = source[start + 4..]
                .find("-->")
                .map_or(source.len(), |n| start + 4 + n + 3);
            continue;
        }
        let Some(open_end) = tag_end(source, start) else {
            break;
        };
        cursor = open_end;
        if let Some(name) = source[start + 1..open_end].strip_prefix('/') {
            // Closing tag: pop back to the matching open element (tolerates
            // implicitly closed children); unknown closers are ignored.
            let name = name
                .trim_end_matches('>')
                .split(|c: char| c.is_whitespace())
                .next()
                .unwrap_or("")
                .to_ascii_lowercase();
            if let Some(index) = open_elements.iter().rposition(|open| *open == name) {
                open_elements.truncate(index);
            }
            continue;
        }
        let Some((tag, attrs)) = parse_tag_open(&source[start..open_end]) else {
            continue;
        };
        let nested = !open_elements.is_empty();
        if !matches!(tag.as_str(), "script" | "style") || nested {
            // Only top-level <script>/<style> are component blocks. A <script>
            // inside <svelte:head> (or any element) is DOM markup: keep it in the
            // template, and skip its raw body so `<` in code is not read as a tag.
            if matches!(tag.as_str(), "script" | "style") {
                if let Some(close) = find_closing_tag(source, &lower, open_end, &tag) {
                    cursor = close.1;
                }
                continue;
            }
            let self_closing = source[start..open_end].trim_end_matches('>').ends_with('/');
            if is_element_name(&tag) && !self_closing && !is_void_or_optional_close(&tag) {
                open_elements.push(tag);
            }
            continue;
        }
        let closing_span = find_closing_tag(source, &lower, open_end, &tag);
        let (content_end, end) = closing_span.unwrap_or((source.len(), source.len()));
        let content = source[open_end..content_end].to_string();
        let mut parts = vec![tag.clone()];
        // Svelte 4 `context="module"` and Svelte 5 bare `module` both run the
        // script once per module instead of once per component instance.
        if tag == "script" && is_module_script(&attrs) {
            parts.push("module".into());
        }
        if tag == "style" && attr_contains(&attrs, "global") {
            parts.push("global".into());
        }
        if let Some(lang) = attr_lang(&attrs) {
            parts.push(lang);
        }
        let (start_line, start_col) = source_position(source, start);
        let (end_line, end_col) = source_position(source, end);
        let (content_start_line, content_start_col) = source_position(source, open_end);
        blocks.push(SfcBlock {
            unclosed: closing_span.is_none(),
            node_type: if tag == "script" {
                "script_block"
            } else {
                "style_block"
            },
            label: parts.join(":"),
            start_line,
            start_col,
            end_line,
            end_col,
            content_start_line,
            content_start_col,
            content_hash: content_hash(&content),
            content,
            attrs,
            attrs_offset: start + 1 + tag.len(),
        });
        // Retain template source on either side, including on the same line.
        // Keep newline positions so child locations still refer to the original source.
        for byte in &mut template[start..end] {
            if *byte != b'\n' {
                *byte = b' ';
            }
        }
        cursor = end;
    }
    let template = String::from_utf8(template).expect("source UTF-8 with blocks replaced by ASCII");
    let lines: Vec<_> = template.lines().collect();
    if let (Some(start), Some(end)) = (
        lines.iter().position(|l| !l.trim().is_empty()),
        lines.iter().rposition(|l| !l.trim().is_empty()),
    ) {
        let content = lines[start..=end].join("\n");
        blocks.push(SfcBlock {
            unclosed: false,
            node_type: "template_body",
            label: "template".into(),
            start_line: start as u32,
            start_col: 0,
            end_line: end as u32,
            end_col: lines[end].len() as u32,
            content_start_line: start as u32,
            content_start_col: 0,
            content_hash: content_hash(&content),
            content,
            attrs: String::new(),
            attrs_offset: 0,
        });
    }
    blocks
}

fn make_leaf(
    id: &str,
    node_type: &str,
    label: impl Into<String>,
    line: u32,
    start_col: u32,
    end_col: u32,
) -> SemanticNode {
    SemanticNodeBuilder::new(id, node_type, label.into(), line, start_col, line, end_col, "").build()
}

fn line_column(content_line: usize, first_line_column: u32, local_byte: usize) -> u32 {
    if content_line == 0 {
        first_line_column + local_byte as u32
    } else {
        local_byte as u32
    }
}

fn is_identifier_like(value: &str) -> bool {
    let mut chars = value.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' || c == '$' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$' || c == '-')
}

fn normalize_statement(line: &str) -> String {
    line.trim()
        .trim_end_matches(',')
        .trim_end_matches(';')
        .trim()
        .to_string()
}

fn parse_attr_labels(attrs: &str) -> Vec<(String, usize, usize)> {
    split_attrs(attrs)
        .into_iter()
        .filter_map(|(raw, start, end)| {
            let (name, value) = attr_name_value(&raw);
            if name.is_empty() {
                return None;
            }
            let label = if value.is_empty() && !raw.contains('=') {
                name
            } else {
                format!("{name}={value}")
            };
            Some((label, start, end))
        })
        .collect()
}

fn parse_tag_segment(segment: &str) -> Option<(String, String)> {
    let trimmed = segment.trim();
    if trimmed.is_empty() || trimmed.starts_with('/') || trimmed.starts_with('!') {
        return None;
    }
    let tag_end = trimmed
        .find(|c: char| c.is_whitespace() || c == '>' || c == '/')
        .unwrap_or(trimmed.len());
    let tag = trimmed[..tag_end].trim();
    if tag.is_empty() {
        return None;
    }
    let attrs = trimmed[tag_end..]
        .trim()
        .trim_end_matches('/')
        .trim()
        .to_string();
    Some((tag.to_lowercase(), attrs))
}

fn extract_template_children(
    content: &str,
    start_line: u32,
    first_line_column: u32,
    id_prefix: &str,
) -> Vec<SemanticNode> {
    let mut children = Vec::new();
    for (line_idx, line) in content.lines().enumerate() {
        let absolute_line = start_line + line_idx as u32;
        let mut rest = line;
        let mut consumed = 0;
        while let Some(open_idx) = rest.find('<') {
            let after_open = &rest[open_idx + 1..];
            if let Some(close_idx) = after_open.find('>') {
                let segment = &after_open[..close_idx];
                if let Some((tag, attrs)) = parse_tag_segment(segment) {
                    let child_id = format!("{}.{}", id_prefix, children.len());
                    let local_start = consumed + open_idx;
                    let local_end = local_start + 2 + close_idx;
                    let attr_start = segment.find(&attrs).unwrap_or(segment.len());
                    let attr_children = parse_attr_labels(&attrs)
                        .into_iter()
                        .enumerate()
                        .map(|(i, (label, a, b))| {
                            make_leaf(
                                &format!("{}.{}", child_id, i),
                                "attribute",
                                label,
                                absolute_line,
                                line_column(line_idx, first_line_column, local_start + 1 + attr_start + a),
                                line_column(line_idx, first_line_column, local_start + 1 + attr_start + b),
                            )
                        })
                        .collect();
                    children.push(
                        SemanticNodeBuilder::new(
                            &child_id,
                            "element",
                            tag,
                            absolute_line,
                            line_column(line_idx, first_line_column, local_start),
                            absolute_line,
                            line_column(line_idx, first_line_column, local_end),
                            "",
                        )
                        .children(attr_children)
                        .build(),
                    );
                }
                consumed += open_idx + close_idx + 2;
                rest = &after_open[close_idx + 1..];
            } else {
                break;
            }
        }
        let mut cursor = 0;
        while let Some(relative) = line[cursor..].find('{') {
            let brace = cursor + relative;
            if line[brace..].starts_with("{{") {
                cursor = brace + 2;
                continue;
            }
            let after = brace + 1;
            let Some(close_rel) = line[after..].find('}') else { break };
            let close = after + close_rel;
            let raw_label = &line[after..close];
            let label = raw_label.trim();
            if !label.is_empty() && !label.starts_with('/') {
                let leading = raw_label.len() - raw_label.trim_start().len();
                let start = after + leading;
                let child_id = format!("{}.{}", id_prefix, children.len());
                // `{#if cond}` / `{#each items as item}` carry control-flow
                // conditions; keep them instead of dropping the payload.
                let node_type = if label.starts_with('#') { "block_tag" } else { "interpolation" };
                children.push(make_leaf(
                    &child_id, node_type, normalize_js(label, &mut false), absolute_line,
                    line_column(line_idx, first_line_column, start),
                    line_column(line_idx, first_line_column, start + label.len()),
                ));
            }
            cursor = close + 1;
        }
    }
    extract_template_text(content, start_line, first_line_column, id_prefix, &mut children);
    children
}

/// Static template text (DOM text content) between tags, outside `{...}` and
/// `<!-- -->`. Whitespace runs collapse to one space, so reflowing markup stays
/// layout-only while any wording, number or symbol edit is a semantic payload.
fn extract_template_text(
    content: &str,
    start_line: u32,
    first_line_column: u32,
    id_prefix: &str,
    children: &mut Vec<SemanticNode>,
) {
    let position = |offset: usize| -> (u32, u32) {
        let (line, col) = source_position(content, offset);
        let col = if line == 0 { first_line_column + col } else { col };
        (start_line + line, col)
    };
    let flush = |children: &mut Vec<SemanticNode>, from: usize, to: usize| {
        let raw = &content[from..to];
        let label = raw.split_whitespace().collect::<Vec<_>>().join(" ");
        if label.is_empty() {
            return;
        }
        let lead = raw.len() - raw.trim_start().len();
        let trail = raw.trim_end().len();
        let (sl, sc) = position(from + lead);
        let (el, ec) = position(from + trail);
        let id = format!("{}.{}", id_prefix, children.len());
        children.push(SemanticNodeBuilder::new(&id, "text", label, sl, sc, el, ec, "").build());
    };
    let bytes = content.as_bytes();
    let mut text_start = 0;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'<' if content[i..].starts_with("<!--") => {
                flush(children, text_start, i);
                i = content[i + 4..].find("-->").map_or(content.len(), |n| i + 4 + n + 3);
                text_start = i;
            }
            b'<' if bytes
                .get(i + 1)
                .is_some_and(|b| b.is_ascii_alphabetic() || *b == b'/' || *b == b'!') =>
            {
                flush(children, text_start, i);
                i = tag_end(content, i).unwrap_or(content.len());
                text_start = i;
            }
            b'{' => {
                flush(children, text_start, i);
                let mut depth = 0usize;
                let mut quote: Option<u8> = None;
                while i < bytes.len() {
                    let b = bytes[i];
                    match quote {
                        Some(q) if b == q => quote = None,
                        Some(_) if b == b'\\' => i += 1,
                        Some(_) => {}
                        None if matches!(b, b'"' | b'\'' | b'`') => quote = Some(b),
                        None if b == b'{' => depth += 1,
                        None if b == b'}' => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        None => {}
                    }
                    i += 1;
                }
                i = (i + 1).min(bytes.len());
                text_start = i;
            }
            _ => i += 1,
        }
    }
    flush(children, text_start, bytes.len());
}

fn declaration_name(line: &str) -> Option<&str> {
    for keyword in ["const ", "let ", "var "] {
        if let Some(rest) = line.strip_prefix(keyword) {
            let name = rest
                .split(|c: char| c.is_whitespace() || c == '=' || c == ':' || c == ';')
                .next()
                .unwrap_or("");
            if is_identifier_like(name) {
                return Some(name);
            }
        }
    }
    None
}

fn function_name(line: &str) -> Option<&str> {
    if let Some(rest) = line.strip_prefix("function ") {
        let name = rest
            .split(|c: char| c.is_whitespace() || c == '(')
            .next()
            .unwrap_or("");
        if is_identifier_like(name) {
            return Some(name);
        }
    }
    None
}

fn assignment_label(line: &str) -> Option<String> {
    for op in ["+=", "-=", "*=", "/=", "="] {
        if let Some((left, _)) = line.split_once(op) {
            let label = left.trim();
            if !label.is_empty()
                && !label.starts_with("return")
                && !label.starts_with("const ")
                && !label.starts_with("let ")
                && !label.starts_with("var ")
            {
                return Some(label.to_string());
            }
        }
    }
    None
}

/// `raw` without a trailing `// comment` (quote-aware).
fn strip_line_comment(raw: &str) -> &str {
    let mut quote = None;
    let mut escaped = false;
    for (index, c) in raw.char_indices() {
        match quote {
            Some(_) if escaped => escaped = false,
            Some(_) if c == '\\' => escaped = true,
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if matches!(c, '\'' | '"' | '`') => quote = Some(c),
            None if raw[index..].starts_with("//") => return &raw[..index],
            None => {}
        }
    }
    raw
}

/// `raw` with every `/* ... */` comment byte replaced by a space, so byte
/// offsets still index the source line. Quote-aware; a `//` comment is left
/// for `strip_line_comment`. `in_comment` carries an open comment across lines.
fn blank_block_comments(raw: &str, in_comment: &mut bool) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut index = 0;
    while let Some(c) = raw[index..].chars().next() {
        let rest = &raw[index..];
        if *in_comment {
            if rest.starts_with("*/") {
                *in_comment = false;
                out.push_str("  ");
                index += 2;
            } else {
                out.extend(std::iter::repeat_n(' ', c.len_utf8()));
                index += c.len_utf8();
            }
            continue;
        }
        match quote {
            Some(_) if escaped => escaped = false,
            Some(_) if c == '\\' => escaped = true,
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if matches!(c, '\'' | '"' | '`') => quote = Some(c),
            None if rest.starts_with("//") => {
                out.push_str(rest);
                break;
            }
            None if rest.starts_with("/*") => {
                *in_comment = true;
                out.push_str("  ");
                index += 2;
                continue;
            }
            None => {}
        }
        out.push(c);
        index += c.len_utf8();
    }
    out
}

fn extract_script_children(
    content: &str,
    start_line: u32,
    first_line_column: u32,
    id_prefix: &str,
) -> Vec<SemanticNode> {
    let mut children = Vec::new();
    let mut in_comment = false;
    for (line_idx, raw_line) in content.lines().enumerate() {
        // Block comments blanked to spaces (byte offsets kept), so a statement
        // sharing its line with `/* ... */` is classified like any other.
        let blanked = blank_block_comments(raw_line, &mut in_comment);
        // Comment- and quote-insensitive view of the line; comment-only and
        // brace-only lines carry no executable payload.
        let semantic = normalize_js(&blanked, &mut false);
        if semantic.chars().all(|c| matches!(c, '{' | '}' | '(' | ')' | '[' | ']' | ';' | ',')) {
            continue;
        }
        let code = strip_line_comment(&blanked);
        let line = normalize_statement(code);
        if line.is_empty() {
            continue;
        }
        let absolute_line = start_line + line_idx as u32;
        let leading = code.find(&line).unwrap_or(0);
        let start_col = line_column(line_idx, first_line_column, leading);
        let end_col = start_col + line.len() as u32;
        let child_id = format!("{}.{}", id_prefix, children.len());
        let node = if line.starts_with("import ") {
            Some(make_leaf(
                &child_id, "import_statement", normalize_js(&line, &mut false),
                absolute_line, start_col, end_col,
            ))
        } else if let Some(name) = declaration_name(&line) {
            let mut decl = make_leaf(
                &child_id, "variable_declaration", name.to_string(),
                absolute_line, start_col, end_col,
            );
            // Retain the RHS in Rust semantics; never turn a numeric change into style-only.
            if let Some(equals) = line.find('=') {
                let remainder = &line[equals + 1..];
                let rhs = remainder.trim();
                if !rhs.is_empty() {
                    let rhs_start = start_col + (equals + 1 + remainder.len() - remainder.trim_start().len()) as u32;
                    decl.children = vec![make_leaf(
                        &format!("{child_id}.0"), "declaration_value", normalize_js(rhs, &mut false),
                        absolute_line, rhs_start, rhs_start + rhs.len() as u32,
                    )];
                }
            }
            Some(decl)
        } else if let Some(name) = function_name(&line) {
            Some(make_leaf(
                &child_id, "function_declaration", name.to_string(),
                absolute_line, start_col, end_col,
            ))
        } else if let Some(label) = assignment_label(&line) {
            let mut assignment = make_leaf(
                &child_id, "assignment_statement", label,
                absolute_line, start_col, end_col,
            );
            if let Some((index, op)) = ["+=", "-=", "*=", "/=", "="]
                .into_iter()
                .find_map(|op| line.find(op).map(|index| (index, op)))
            {
                let remainder = &line[index + op.len()..];
                let rhs = remainder.trim();
                if !rhs.is_empty() {
                    let rhs_start = start_col + (index + op.len() + remainder.len() - remainder.trim_start().len()) as u32;
                    assignment.children = vec![make_leaf(
                        &format!("{child_id}.0"), "assignment_value", normalize_js(rhs, &mut false),
                        absolute_line, rhs_start, rhs_start + rhs.len() as u32,
                    )];
                }
            }
            Some(assignment)
        } else {
            // Calls and other statements (`console.log(1)`, `export let x`) are
            // executable payload: retain them rather than certify edits as style.
            Some(make_leaf(&child_id, "expression_statement", semantic, absolute_line, start_col, end_col))
        };
        if let Some(node) = node {
            children.push(node);
        }
    }
    children
}

fn extract_style_children(
    content: &str,
    start_line: u32,
    first_line_column: u32,
    id_prefix: &str,
) -> Vec<SemanticNode> {
    let mut children = Vec::new();
    for (line_idx, raw_line) in content.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() || line == "}" {
            continue;
        }
        let absolute_line = start_line + line_idx as u32;
        let leading = raw_line.find(line).expect("trimmed style statement occurs in input line");
        let start_col = line_column(line_idx, first_line_column, leading);
        let end_col = start_col + line.len() as u32;
        let child_id = format!("{}.{}", id_prefix, children.len());
        if let Some(selector) = line.strip_suffix('{') {
            let selector = selector.trim();
            if !selector.is_empty() {
                children.push(make_leaf(
                    &child_id, "style_rule", selector,
                    absolute_line, start_col, end_col,
                ));
            }
        } else if let Some((property, value)) = line.split_once(':') {
            let property = property.trim();
            if !property.is_empty() {
                let mut decl = make_leaf(
                    &child_id, "style_declaration", property,
                    absolute_line, start_col, end_col,
                );
                let value = value.trim().trim_end_matches(';').trim_end_matches('}').trim();
                if !value.is_empty() {
                    let raw_value = &line[line.find(':').unwrap() + 1..];
                    let local = line.find(':').unwrap() + 1 + raw_value.len() - raw_value.trim_start().len();
                    let value_col = start_col + local as u32;
                    decl.children = vec![make_leaf(
                        &format!("{child_id}.0"), "style_value", value,
                        absolute_line, value_col, value_col + value.len() as u32,
                    )];
                }
                children.push(decl);
            }
        }
    }
    children
}

/// Opening-tag attributes of a script/style block that its label does not
/// already encode (`src`, `type`, `generics`, `nonce`, ...): an edit to any of
/// them changes what the component loads or runs.
fn block_attribute_children(source: &str, block: &SfcBlock, id: &str) -> Vec<SemanticNode> {
    let tag = if block.node_type == "script_block" { "script" } else { "style" };
    let Some(raw_attrs) = source.get(block.attrs_offset..) else {
        return Vec::new();
    };
    let lead = raw_attrs.find(block.attrs.as_str()).unwrap_or(0);
    let base = block.attrs_offset + lead;
    split_attrs(&block.attrs)
        .into_iter()
        .filter_map(|(raw, a, b)| {
            let (name, value) = attr_name_value(&raw);
            if name.is_empty() || is_block_label_attr(tag, &name, &value) {
                return None;
            }
            let label = if value.is_empty() && !raw.contains('=') { name } else { format!("{name}={value}") };
            Some((label, a, b))
        })
        .enumerate()
        .map(|(i, (label, a, b))| {
            let (sl, sc) = source_position(source, base + a);
            let (el, ec) = source_position(source, base + b);
            SemanticNodeBuilder::new(&format!("{id}.a{i}"), "attribute", label, sl, sc, el, ec, "").build()
        })
        .collect()
}

/// Deterministic `type\0label` pre-order serialization of `nodes` (no ids,
/// positions or raw text), used to hash a block by what it means.
fn project(nodes: &[SemanticNode], out: &mut String) {
    for node in nodes {
        out.push_str(&node.node_type);
        out.push('\0');
        out.push_str(&node.label);
        out.push('\u{1}');
        project(&node.children, out);
        out.push('\u{2}');
    }
}

fn process_impl(source: &str, filename: &str) -> String {
    let stem = filename
        .rsplit(['/', '\\'])
        .next()
        .and_then(|f| f.rsplit('.').nth(1))
        .unwrap_or("component");

    let mut blocks = extract_blocks(source);
    if blocks.iter().any(|block| block.unclosed) {
        return r#"{"error":"Unclosed script or style block"}"#.to_string();
    }
    // Sort by start_line for deterministic output
    blocks.sort_by_key(|b| b.start_line);

    let children: Vec<SemanticNode> = blocks
        .iter()
        .enumerate()
        .map(|(i, b)| {
            let id = format!("0.{}", i);
            let mut block_children = block_attribute_children(source, b, &id);
            block_children.extend(match b.node_type {
                "script_block" => extract_script_children(&b.content, b.content_start_line, b.content_start_col, &id),
                "style_block" => extract_style_children(&b.content, b.content_start_line, b.content_start_col, &id),
                "template_body" => extract_template_children(&b.content, b.content_start_line, b.content_start_col, &id),
                _ => Vec::new(),
            });
            // A script block hashes its semantic projection, not its raw text:
            // comments and layout are trivia there (the children drop them), so
            // a comment-only edit must not change the block or component hash.
            let hash = if b.node_type == "script_block" {
                let mut projection = String::new();
                project(&block_children, &mut projection);
                content_hash(&projection)
            } else {
                b.content_hash.clone()
            };
            SemanticNodeBuilder::new(
                &id,
                b.node_type,
                b.label.clone(),
                b.start_line,
                b.start_col,
                b.end_line,
                b.end_col,
                hash,
            )
            .children(block_children)
            .build()
        })
        .collect();

    let root_hash: String = {
        let combined: String = children
            .iter()
            .map(|b| b.structural_hash.as_str())
            .collect::<Vec<_>>()
            .join("|");
        let mut h: u64 = 5381;
        for b in combined.bytes() {
            h = h.wrapping_mul(33).wrapping_add(b as u64);
        }
        format!("{:016x}", h)
    };

    let root = SemanticNodeBuilder::new(
        "0",
        "svelte_component",
        stem.to_string(),
        0,
        0,
        source_position(source, source.len()).0,
        source_position(source, source.len()).1,
        root_hash,
    )
    .children(children)
    .build();

    match serde_json::to_string(&root) {
        Ok(s) => s,
        Err(e) => format!(r#"{{"error":"Serialisation error: {}"}}"#, e),
    }
}

impl Guest for SvelteParser {
    fn get_parser_mode() -> ParserMode {
        ParserMode::FullParse
    }
    fn grammar_id() -> String {
        "svelte".to_string()
    }
    fn detect_language(filename: String, _content: String) -> String {
        if filename.to_lowercase().ends_with(".svelte") {
            return "svelte".to_string();
        }
        String::new()
    }
    fn preprocess_source(source: String) -> String {
        source
    }
    fn example(_language: String) -> ExamplePair {
        ExamplePair {
            old: "<script>\n  let name = 'World';\n</script>\n\n<h1>Hello, {name}!</h1>\n".to_string(),
            new: "<script>\n  let name = 'World';\n  let count = 0;\n\n  function increment() {\n    count += 1;\n  }\n</script>\n\n<h1>Hello, {name}!</h1>\n<p>Count: {count}</p>\n<button on:click={increment}>Increment</button>\n".to_string(),
        }
    }
    fn process(input: String, _language: String, filename: String) -> String {
        process_impl(&input, &filename)
    }
    fn trivia_node_types() -> Vec<String> {
        vec![]
    }
    fn language_ids() -> Vec<String> {
        vec!["svelte".to_string()]
    }
    fn language_info() -> Vec<LanguageInfoRecord> {
        language_info_for(Self::language_ids())
    }
    fn priority() -> i32 {
        0
    }
}

export!(SvelteParser);

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exports::intentdiff::plugin::parser::Guest;

    #[test]
    fn grammar_id_nonempty() {
        assert!(!SvelteParser::grammar_id().is_empty());
    }

    #[test]
    fn language_ids_contain_grammar_id() {
        assert!(SvelteParser::language_ids().contains(&SvelteParser::grammar_id()));
    }

    #[test]
    fn detect_language_svelte() {
        assert_eq!(
            SvelteParser::detect_language("App.svelte".to_string(), "".to_string()),
            "svelte"
        );
    }

    #[test]
    fn detect_language_unknown() {
        assert_eq!(
            SvelteParser::detect_language("main.ts".to_string(), "".to_string()),
            ""
        );
    }

    #[test]
    fn empty_source_valid_json() {
        let out = process_impl("", "App.svelte");
        serde_json::from_str::<serde_json::Value>(&out).expect("valid JSON");
    }

    #[test]
    fn blocks_extracted() {
        let src = r#"<script lang="ts">
  let count = 0;
</script>
<h1>Hello {count}</h1>
<style>
  h1 { color: blue; }
</style>"#;
        let blocks = extract_blocks(src);
        let has_script = blocks.iter().any(|b| b.node_type == "script_block");
        let has_style = blocks.iter().any(|b| b.node_type == "style_block");
        let has_template = blocks.iter().any(|b| b.node_type == "template_body");
        assert!(has_script, "should have script_block");
        assert!(has_style, "should have style_block");
        assert!(has_template, "should have template_body");
    }

    #[test]
    fn example_extracts_component_children() {
        let example = SvelteParser::example("svelte".to_string());
        let out = process_impl(&example.new, "App.svelte");
        let v: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
        let labels = collect_labels(&v);
        assert!(labels.iter().any(|label| label == "count"));
        assert!(labels.iter().any(|label| label == "increment"));
        assert!(labels.iter().any(|label| label == "button"));
        assert!(labels.iter().any(|label| label == "on:click=increment"));
    }

    #[test]
    fn inline_script_content_and_trailing_template_are_retained() {
        let old = process_impl(
            "<script>let name = 'One';</script><h1>{name}</h1>",
            "App.svelte",
        );
        let new = process_impl(
            "<script>let name = 'Two';</script><h1>{name}</h1>",
            "App.svelte",
        );
        let old: serde_json::Value = serde_json::from_str(&old).unwrap();
        let new: serde_json::Value = serde_json::from_str(&new).unwrap();
        // String literals are labelled in canonical double-quoted spelling.
        assert!(collect_labels(&old).contains(&"\"One\"".to_string()));
        assert!(collect_labels(&new).contains(&"\"Two\"".to_string()));
        assert!(collect_labels(&new).contains(&"h1".to_string()));
        assert_ne!(old["structural_hash"], new["structural_hash"]);
    }

    #[test]
    fn block_content_keeps_opening_and_closing_lines_and_positions() {
        let source = "<h1>Olá</h1><script context=\"module\" lang=\"ts\">let a = 1;\nlet b = 2;</script><style global>h1 {color: red;}</style><p>{a}</p>";
        let blocks = extract_blocks(source);
        let script = blocks
            .iter()
            .find(|b| b.node_type == "script_block")
            .unwrap();
        assert_eq!(script.content, "let a = 1;\nlet b = 2;");
        assert_eq!(script.label, "script:module:ts");
        assert_eq!(
            (
                script.start_line,
                script.content_start_line,
                script.end_line
            ),
            (0, 0, 1)
        );
        let style = blocks
            .iter()
            .find(|b| b.node_type == "style_block")
            .unwrap();
        assert_eq!(style.content, "h1 {color: red;}");
        assert_eq!(style.label, "style:global");
        let template = blocks
            .iter()
            .find(|b| b.node_type == "template_body")
            .unwrap();
        assert!(template.content.contains("<h1>Olá</h1>"));
        assert!(template.content.contains("<p>{a}</p>"));
        assert!(!template.content.contains("let a"));
    }

    #[test]
    fn quoted_tag_end_and_commented_script_do_not_swallow_content() {
        let blocks = extract_blocks("<!-- <script>not code</script> -->\n<script data-note=\"a > b\">let name = 'One';</script>");
        let scripts: Vec<_> = blocks
            .iter()
            .filter(|b| b.node_type == "script_block")
            .collect();
        assert_eq!(scripts.len(), 1);
        assert_eq!(scripts[0].content, "let name = 'One';");
        assert_eq!(scripts[0].content_start_line, 1);
    }

    #[test]
    fn unclosed_unicode_blocks_preserve_content_and_positions() {
        for tag in ["script", "style"] {
            for content in ["é", "漢", "😀", "\né", "é\n"] {
                let source = format!("<{tag}>{content}");
                let blocks = extract_blocks(&source);
                assert_eq!(blocks.len(), 1);
                assert_eq!(blocks[0].content, content);
                assert_eq!(blocks[0].start_line, 0);
                assert_eq!(blocks[0].content_start_line, 0);
                assert_eq!(blocks[0].end_line, content.bytes().filter(|&b| b == b'\n').count() as u32);
                let output = process_impl(&source, "App.svelte");
                let value: serde_json::Value = serde_json::from_str(&output).expect("in-band JSON");
                assert_eq!(value["error"], "Unclosed script or style block");
            }
        }
    }


    #[test]
    fn numeric_literal_has_exact_source_byte_span_on_both_sides() {
        let old_source = "<script>\n  let count = 0;\n</script>\n<h1>{count}</h1>";
        let new_source = old_source.replacen("= 0;", "= 1;", 1);
        let old: serde_json::Value = serde_json::from_str(&process_impl(old_source, "App.svelte")).unwrap();
        let new: serde_json::Value = serde_json::from_str(&process_impl(&new_source, "App.svelte")).unwrap();
        fn value_span<'a>(tree: &'a serde_json::Value, label: &str) -> &'a serde_json::Value {
            fn find<'a>(node: &'a serde_json::Value, label: &str) -> Option<&'a serde_json::Value> {
                if node["node_type"] == "declaration_value" && node["label"] == label {
                    return Some(node);
                }
                node["children"].as_array()?.iter().find_map(|child| find(child, label))
            }
            find(tree, label).expect("Rust declaration value must be source-anchored")
        }
        for (tree, label) in [(&old, "0"), (&new, "1")] {
            let node = value_span(tree, label);
            assert_eq!(node["position"]["start_line"], 1);
            assert_eq!(node["position"]["end_line"], 1);
            assert_eq!(node["position"]["start_col"], 14);
            assert_eq!(node["position"]["end_col"], 15);
        }
        assert_ne!(old["structural_hash"], new["structural_hash"]);
    }

    #[test]
    fn inline_unicode_script_and_template_have_real_byte_columns() {
        let source = "<h1>é</h1><script>let count = 0;</script><p>{count}</p>";
        let result: serde_json::Value = serde_json::from_str(&process_impl(source, "App.svelte")).unwrap();
        fn find<'a>(node: &'a serde_json::Value, kind: &str, label: &str) -> Option<&'a serde_json::Value> {
            if node["node_type"] == kind && node["label"] == label {
                return Some(node);
            }
            node["children"].as_array()?.iter().find_map(|child| find(child, kind, label))
        }
        let value = find(&result, "declaration_value", "0").unwrap();
        let column = source.find("0;").unwrap() as u32;
        assert_eq!(value["position"]["start_col"], column);
        assert_eq!(value["position"]["end_col"], column + 1);
        let interpolation = find(&result, "interpolation", "count").unwrap();
        let col = source.rfind("{count}").unwrap() as u32 + 1;
        assert_eq!(interpolation["position"]["start_col"], col);
        assert_eq!(interpolation["position"]["end_col"], col + 5);
    }

    #[test]
    fn assignment_and_style_rhs_keep_source_anchored_semantics() {
        let source = "<script>\n  count += 2;\n</script>\n<style>\n  p {\n    color: red;\n  }\n</style>";
        let result: serde_json::Value = serde_json::from_str(&process_impl(source, "App.svelte")).unwrap();
        fn find<'a>(node: &'a serde_json::Value, kind: &str, label: &str) -> Option<&'a serde_json::Value> {
            if node["node_type"] == kind && node["label"] == label { return Some(node); }
            node["children"].as_array()?.iter().find_map(|child| find(child, kind, label))
        }
        let assignment = find(&result, "assignment_value", "2").unwrap();
        assert_eq!(assignment["position"]["start_line"], 1);
        assert_eq!(assignment["position"]["start_col"], 11);
        assert_eq!(assignment["position"]["end_col"], 12);
        let value = find(&result, "style_value", "red").unwrap();
        assert_eq!(value["position"]["start_line"], 5);
        assert_eq!(value["position"]["start_col"], 11);
        assert_eq!(value["position"]["end_col"], 14);
    }

    // -- 0.0.3 regressions: #7 (module context, nested head script) and #8 (static text) --

    fn tree(source: &str) -> serde_json::Value {
        serde_json::from_str(&process_impl(source, "assessment.svelte")).expect("tree JSON")
    }

    fn find_node<'a>(node: &'a serde_json::Value, kind: &str, label: &str) -> Option<&'a serde_json::Value> {
        if node["node_type"] == kind && node["label"] == label {
            return Some(node);
        }
        node["children"].as_array()?.iter().find_map(|child| find_node(child, kind, label))
    }

    fn span(node: &serde_json::Value) -> (u64, u64, u64, u64) {
        let p = &node["position"];
        (
            p["start_line"].as_u64().unwrap(),
            p["start_col"].as_u64().unwrap(),
            p["end_line"].as_u64().unwrap(),
            p["end_col"].as_u64().unwrap(),
        )
    }

    /// Semantic projection without ids/positions/hashes: what a diff compares.
    fn shape(node: &serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "t": node["node_type"],
            "l": node["label"],
            "c": node["children"].as_array().map(|c| c.iter().map(shape).collect::<Vec<_>>()).unwrap_or_default(),
        })
    }

    #[test]
    fn svelte5_module_flag_is_module_context() {
        // web-svelte-module_context (#7)
        let old = tree("<script>console.log(1);</script>\n<div>Hello</div>\n");
        let new = tree("<script module>console.log(1);</script>\n<div>Hello</div>\n");
        assert!(find_node(&old, "script_block", "script").is_some());
        assert!(find_node(&new, "script_block", "script:module").is_some());
        // The once-per-module side effect itself is retained with its source span.
        let stmt = find_node(&new, "expression_statement", "console.log(1)").expect("statement kept");
        assert_eq!(span(stmt), (0, 15, 0, 29));
        assert!(find_node(&new, "text", "Hello").is_some(), "template after inline script kept");
        assert_ne!(shape(&old), shape(&new));
        // Svelte 4 spelling is the same context, so the two spellings are equivalent.
        let legacy = tree("<script context=\"module\">console.log(1);</script>\n<div>Hello</div>\n");
        assert_eq!(shape(&legacy), shape(&new));
    }

    #[test]
    fn nested_head_script_is_template_markup_with_src() {
        // web-svelte-head_script_src (#7)
        let old_src = "<svelte:head>\n<script src=\"./safe.js\"></script>\n</svelte:head>\n<div>Hello</div>\n";
        let new_src = old_src.replace("./safe.js", "./danger.js");
        let old = tree(old_src);
        let new = tree(&new_src);
        for t in [&old, &new] {
            assert!(find_node(t, "script_block", "script").is_none(), "head script is not a component block");
            assert!(find_node(t, "element", "svelte:head").is_some());
            assert!(find_node(t, "element", "script").is_some());
        }
        let attr = find_node(&old, "attribute", "src=./safe.js").expect("old src");
        assert_eq!(span(attr), (1, 8, 1, 23));
        let attr = find_node(&new, "attribute", "src=./danger.js").expect("new src");
        assert_eq!(span(attr), (1, 8, 1, 25));
        assert_ne!(shape(&old), shape(&new));
    }

    #[test]
    fn top_level_script_after_head_is_still_a_block() {
        let t = tree("<svelte:head><title>x</title></svelte:head>\n<p>a\n<script>let a = 1;</script>\n");
        assert!(find_node(&t, "script_block", "script").is_some());
        assert!(find_node(&t, "declaration_value", "1").is_some());
    }

    #[test]
    fn block_attributes_are_retained_and_quote_insensitive() {
        let a = tree("<script src=\"./a.js\" lang=\"ts\"></script>\n");
        let b = tree("<script src='./b.js' lang='ts'></script>\n");
        let a2 = tree("<script src='./a.js'  lang=ts></script>\n");
        let attr = find_node(&a, "attribute", "src=./a.js").expect("block src attribute");
        assert_eq!(span(attr), (0, 8, 0, 20));
        assert!(find_node(&a, "script_block", "script:ts").is_some());
        assert_ne!(shape(&a), shape(&b));
        assert_eq!(shape(&a), shape(&a2));
    }

    #[test]
    fn static_template_text_is_retained() {
        // web-svelte-text (#8)
        let old = tree("<p>Ready</p>\n");
        let new = tree("<p>Failed</p>\n");
        assert_eq!(span(find_node(&old, "text", "Ready").expect("old text")), (0, 3, 0, 8));
        assert_eq!(span(find_node(&new, "text", "Failed").expect("new text")), (0, 3, 0, 9));
        assert_ne!(shape(&old), shape(&new));
    }

    #[test]
    fn unicode_template_text_keeps_payload_and_byte_columns() {
        // web-svelte-unicode (#8)
        let old_src = "<p>Café: 5 €</p>\n";
        let new_src = "<p>Café: 6 €</p>\n";
        let old = tree(old_src);
        let new = tree(new_src);
        let end = (old_src.find("</p>").unwrap()) as u64;
        assert_eq!(span(find_node(&old, "text", "Café: 5 €").expect("old")), (0, 3, 0, end));
        assert_eq!(span(find_node(&new, "text", "Café: 6 €").expect("new")), (0, 3, 0, end));
        assert_ne!(shape(&old), shape(&new));
    }

    #[test]
    fn text_layout_comments_and_interpolations_are_not_text_payload() {
        let a = tree("<p>Hello   world</p>\n<!-- note -->\n<h1>{title}</h1>\n");
        let b = tree("<p>\n  Hello\n  world\n</p>\n<!-- other note -->\n<h1>{ title }</h1>\n");
        assert_eq!(shape(&a), shape(&b));
        assert!(find_node(&a, "text", "title").is_none());
        assert!(find_node(&a, "interpolation", "title").is_some());
        // Multi-line text keeps an exact multi-line span.
        assert_eq!(span(find_node(&b, "text", "Hello world").unwrap()), (1, 2, 2, 7));
        // A comment containing tag-like text does not leak into text.
        let c = tree("<!-- <p>Ready</p> -->\n<p>Ready</p>\n");
        let texts = collect_labels(&c).into_iter().filter(|l| l == "Ready").count();
        assert_eq!(texts, 1);
    }

    #[test]
    fn block_tag_conditions_are_retained() {
        let a = tree("{#if count > 1}<p>many</p>{/if}\n");
        let b = tree("{#if count > 2}<p>many</p>{/if}\n");
        assert!(find_node(&a, "block_tag", "#if count>1").is_some());
        assert_ne!(shape(&a), shape(&b));
    }

    #[test]
    fn string_quote_spelling_is_not_semantic() {
        // web-svelte-preservation
        let old = tree("<script>const title = 'Ready';</script>\n<h1>{title}</h1>\n");
        let new = tree("<script>const title = \"Ready\";</script>\n<h1>{title}</h1>\n");
        assert_eq!(shape(&old), shape(&new));
        assert!(find_node(&new, "declaration_value", "\"Ready\"").is_some());
        // ...but the string value itself is.
        let changed = tree("<script>const title = \"Done\";</script>\n<h1>{title}</h1>\n");
        assert_ne!(shape(&new), shape(&changed));
    }

    #[test]
    fn script_comments_are_trivia_and_statements_are_payload() {
        let a = tree("<script>\n  // init\n  init(1);\n  /* a\n  b */\n  let x = 1; // one\n</script>\n");
        let b = tree("<script>\n  init( 1 );\n  let x = 1;\n</script>\n");
        assert_eq!(shape(&a), shape(&b));
        let c = tree("<script>\n  init(2);\n  let x = 1;\n</script>\n");
        assert_ne!(shape(&b), shape(&c));
        let commented = tree("<script>\n  // let x = 2;\n</script>\n");
        assert!(collect_labels(&commented).iter().all(|l| !l.contains("x")));
    }

    #[test]
    fn normalize_js_canonical_forms() {
        let n = |s: &str| normalize_js(s, &mut false);
        assert_eq!(n("'a\\'b'"), n("\"a'b\""));
        assert_eq!(n("a  +  b"), "a+b");
        assert_eq!(n("a + +b"), "a+ +b");
        assert_eq!(n("typeof  x"), "typeof x");
        assert_eq!(n("`a  ${b}`"), "`a  ${b}`");
        assert_eq!(n("f('x', \"y\");"), "f(\"x\",\"y\")");
    }

    #[test]
    fn malformed_inputs_never_panic() {
        for src in [
            "<script>let count =", "<p>{unclosed", "<p>é{", "{'é", "<svelte:head><script src=x>",
            "</div></svelte:head><script>a</script>", "<p a=\"é\" b='{x}'>é</p>", "<!-- é", "<", "{#if", "é<a",
        ] {
            let out = process_impl(src, "x.svelte");
            serde_json::from_str::<serde_json::Value>(&out).expect("valid JSON");
            assert_eq!(out, process_impl(src, "x.svelte"), "deterministic");
        }
    }

    #[test]
    fn issue10_lt_inside_template_expression_is_not_a_tag() {
        let old = tree("<div>{a < b}</div>\n<script>let n = 1;</script>\n");
        let new = tree("<div>{a < b}</div>\n<script>let n = 2;</script>\n");
        for t in [&old, &new] {
            assert!(find_node(t, "script_block", "script").is_some(), "top-level script is a block");
            assert!(find_node(t, "variable_declaration", "n").is_some());
            assert!(find_node(t, "element", "script").is_none());
            assert!(find_node(t, "element", "div").is_some());
        }
        assert!(find_node(&old, "declaration_value", "1").is_some());
        assert!(find_node(&new, "declaration_value", "2").is_some());
        assert_ne!(shape(&old), shape(&new));
        // Strings and nested braces inside the expression do not end it early.
        let t = tree("<p>{f('}', { k: a < b })}</p>\n<script>let n = 1;</script>\n");
        assert!(find_node(&t, "script_block", "script").is_some());
        // `=>` / `>` inside an attribute expression does not end the opening tag.
        let t = tree("<div on:click={() => a > b}>x</div>\n<script>let n = 1;</script>\n");
        assert!(find_node(&t, "script_block", "script").is_some());
        assert!(find_node(&t, "text", "x").is_some());
    }

    #[test]
    fn issue11_regex_literal_whitespace_is_semantic() {
        let old = tree("<script>const x = /a +b/;</script>\n");
        let new = tree("<script>const x = /a+b/;</script>\n");
        assert!(find_node(&old, "declaration_value", "/a +b/").is_some());
        assert!(find_node(&new, "declaration_value", "/a+b/").is_some());
        assert_ne!(shape(&old), shape(&new));
        assert_ne!(old["structural_hash"], new["structural_hash"]);
        // Spacing outside the literal stays layout.
        assert_eq!(
            shape(&tree("<script>x=/a b/;</script>\n")),
            shape(&tree("<script>x = /a b/;</script>\n"))
        );
        let n = |s: &str| normalize_js(s, &mut false);
        assert_eq!(n("f( /a  b/g , c )"), "f(/a  b/g,c)");
        assert_eq!(n("/[/ ]x/ . test( s )"), "/[/ ]x/.test(s)");
        assert_eq!(n("/a\\/ b/"), "/a\\/ b/");
        // Division is not a regex.
        assert_eq!(n("a / b / c"), "a/b/c");
        assert_eq!(n("(a) / 2"), "(a)/2");
        assert_eq!(n("return  /a b/"), n("return /a b/"));
        assert!(n("return /a b/").ends_with("/a b/"));
    }

    #[test]
    fn issue12_inline_block_comment_keeps_statement_classification() {
        let old = tree("<script>\nlet x = 1;\n</script>\n");
        let new = tree("<script>\nlet x = 1; /* note */\n</script>\n");
        assert!(find_node(&new, "variable_declaration", "x").is_some());
        assert!(find_node(&new, "expression_statement", "let x=1").is_none());
        assert_eq!(shape(&old), shape(&new));
        assert_eq!(old["structural_hash"], new["structural_hash"]);
        let value = find_node(&new, "declaration_value", "1").expect("value kept");
        assert_eq!(span(value), (1, 8, 1, 9));
        // The value on the commented line is still payload.
        let changed = tree("<script>\nlet x = 2; /* note */\n</script>\n");
        assert!(find_node(&changed, "declaration_value", "2").is_some());
        assert_ne!(shape(&new), shape(&changed));
        // Leading comment, comment closing a multi-line one, and assignment/import paths.
        let a = tree("<script>\n/* a */ count += 2;\n/* b\n c */ import x from 'y';\n</script>\n");
        assert!(find_node(&a, "assignment_value", "2").is_some());
        assert!(find_node(&a, "import_statement", "import x from\"y\"").is_some());
        let v = find_node(&a, "assignment_value", "2").unwrap();
        assert_eq!(span(v), (1, 17, 1, 18));
        // A `/*` inside a string or after `//` is not a comment.
        let s = tree("<script>\nlet p = '/*'; // x /* y\nlet q = 1;\n</script>\n");
        assert!(find_node(&s, "declaration_value", "\"/*\"").is_some());
        assert!(find_node(&s, "variable_declaration", "q").is_some());
    }

    fn collect_labels(value: &serde_json::Value) -> Vec<String> {
        let mut labels = Vec::new();
        if let Some(label) = value["label"].as_str() {
            labels.push(label.to_string());
        }
        if let Some(children) = value["children"].as_array() {
            for child in children {
                labels.extend(collect_labels(child));
            }
        }
        labels
    }
}
