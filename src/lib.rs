//! Svelte component parser plugin — full-parse mode.
//!
//! Handles `.svelte` single-file component files.
//!
//! Svelte SFCs consist of optional `<script>`, `<script context="module">`,
//! and `<style>` blocks; everything else is template markup.
//!
//! Semantic nodes produced:
//!   svelte_component     — root; label = filename stem
//!   script_block         — `<script ...>` block (label includes "module" if context=module)
//!   style_block          — `<style ...>` block (label includes "global" if global)
//!   template_body        — the remaining markup outside of script/style blocks

use intentumdiff_plugin_sdk::tree::{SemanticNode, SemanticNodeBuilder};

wit_bindgen::generate!({
    path: "wit/plugin.wit",
    world: "parser-plugin",
});

use crate::exports::intentumdiff::plugin::parser::ExamplePair;
use crate::exports::intentumdiff::plugin::parser::Guest;
use crate::exports::intentumdiff::plugin::parser::LanguageInfoRecord;
use crate::exports::intentumdiff::plugin::parser::ParserMode;

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
    node_type: &'static str,
    label: String,
    start_line: u32,
    end_line: u32,
    content_start_line: u32,
    content: String,
    content_hash: String,
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

fn extract_blocks(source: &str) -> Vec<SfcBlock> {
    const SFC_TAGS: &[&str] = &["script", "style"];
    let mut blocks: Vec<SfcBlock> = Vec::new();
    let lines: Vec<&str> = source.lines().collect();
    let mut template_lines: Vec<u32> = Vec::new();

    let mut i = 0;
    while i < lines.len() {
        let line = lines[i].trim_start();
        if let Some((tag, attrs)) = parse_tag_open(line) {
            if SFC_TAGS.contains(&tag.as_str()) {
                let start_line = i as u32;
                let closing = format!("</{}", tag);
                let opening = format!("<{}", tag);
                let mut depth: i32 = 1;
                let mut j = i + 1;
                let content_start = i + 1;
                while i < lines.len() && !lines[i].contains('>') {
                    i += 1;
                }
                while j < lines.len() {
                    let l = lines[j];
                    let open_count = l.matches(&opening).count() as i32;
                    let close_count = l.matches(closing.as_str()).count() as i32;
                    depth += open_count - close_count;
                    if depth <= 0 {
                        let end_line = j as u32;
                        let content: String = lines[content_start.min(j)..j].join("\n");
                        let hash = content_hash(&content);
                        let (node_type, label) = match tag.as_str() {
                            "script" => {
                                let is_module = attr_contains(&attrs, "context=\"module\"")
                                    || attr_contains(&attrs, "context='module'");
                                let mut parts = vec!["script".to_string()];
                                if is_module {
                                    parts.push("module".to_string());
                                }
                                if let Some(lang) = attr_lang(&attrs) {
                                    parts.push(lang);
                                }
                                ("script_block", parts.join(":"))
                            }
                            "style" => {
                                let is_global = attr_contains(&attrs, "global");
                                let mut parts = vec!["style".to_string()];
                                if is_global {
                                    parts.push("global".to_string());
                                }
                                if let Some(lang) = attr_lang(&attrs) {
                                    parts.push(lang);
                                }
                                ("style_block", parts.join(":"))
                            }
                            _ => ("custom_block", tag.clone()),
                        };
                        blocks.push(SfcBlock {
                            node_type,
                            label,
                            start_line,
                            end_line,
                            content_start_line: start_line + 1,
                            content,
                            content_hash: hash,
                        });
                        i = j;
                        break;
                    }
                    j += 1;
                }
                if depth > 0 {
                    let end_line = lines.len().saturating_sub(1) as u32;
                    let content: String = lines[content_start..].join("\n");
                    let label = match tag.as_str() {
                        "script" => "script".to_string(),
                        "style" => "style".to_string(),
                        _ => tag.clone(),
                    };
                    let node_type: &'static str = match tag.as_str() {
                        "script" => "script_block",
                        "style" => "style_block",
                        _ => "custom_block",
                    };
                    blocks.push(SfcBlock {
                        node_type,
                        label,
                        start_line,
                        end_line,
                        content_start_line: start_line + 1,
                        content: content.clone(),
                        content_hash: content_hash(&content),
                    });
                    i = lines.len();
                }
            } else {
                template_lines.push(i as u32);
            }
        } else {
            template_lines.push(i as u32);
        }
        i += 1;
    }

    // Add template_body node for remaining markup if any
    if !template_lines.is_empty() {
        let tstart = *template_lines.first().unwrap();
        let tend = *template_lines.last().unwrap();
        let template_content: String = template_lines
            .iter()
            .map(|&l| lines[l as usize])
            .collect::<Vec<_>>()
            .join("\n");
        blocks.push(SfcBlock {
            node_type: "template_body",
            label: "template".to_string(),
            start_line: tstart,
            end_line: tend,
            content_start_line: tstart,
            content: template_content.clone(),
            content_hash: content_hash(&template_content),
        });
    }

    blocks
}

fn make_leaf(id: &str, node_type: &str, label: impl Into<String>, line: u32) -> SemanticNode {
    SemanticNodeBuilder::new(id, node_type, label.into(), line, 0, line, 0, "").build()
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

fn parse_attr_labels(attrs: &str) -> Vec<String> {
    attrs
        .split_whitespace()
        .filter_map(|part| {
            let clean = part
                .trim()
                .trim_end_matches('>')
                .trim_end_matches('/')
                .trim();
            if clean.is_empty() {
                return None;
            }
            let (name, value) = clean.split_once('=').unwrap_or((clean, ""));
            let name = name.trim();
            if name.is_empty() {
                return None;
            }
            if value.is_empty() {
                Some(name.to_string())
            } else {
                let value = value
                    .trim()
                    .trim_matches('"')
                    .trim_matches('\'')
                    .trim_matches('{')
                    .trim_matches('}');
                Some(format!("{}={}", name, value))
            }
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

fn extract_brace_labels(line: &str) -> Vec<String> {
    let mut labels = Vec::new();
    let mut rest = line;
    while let Some(start) = rest.find('{') {
        if rest[start..].starts_with("{{") {
            rest = &rest[start + 2..];
            continue;
        }
        let after = &rest[start + 1..];
        if let Some(end) = after.find('}') {
            let label = after[..end].trim();
            if !label.is_empty() && !label.starts_with('#') && !label.starts_with('/') {
                labels.push(label.to_string());
            }
            rest = &after[end + 1..];
        } else {
            break;
        }
    }
    labels
}

fn extract_template_children(content: &str, start_line: u32, id_prefix: &str) -> Vec<SemanticNode> {
    let mut children = Vec::new();
    for (line_idx, line) in content.lines().enumerate() {
        let absolute_line = start_line + line_idx as u32;
        let mut rest = line;
        while let Some(open_idx) = rest.find('<') {
            let after_open = &rest[open_idx + 1..];
            if let Some(close_idx) = after_open.find('>') {
                let segment = &after_open[..close_idx];
                if let Some((tag, attrs)) = parse_tag_segment(segment) {
                    let child_id = format!("{}.{}", id_prefix, children.len());
                    let attr_children: Vec<SemanticNode> = parse_attr_labels(&attrs)
                        .into_iter()
                        .enumerate()
                        .map(|(i, label)| {
                            make_leaf(
                                &format!("{}.{}", child_id, i),
                                "attribute",
                                label,
                                absolute_line,
                            )
                        })
                        .collect();
                    children.push(
                        SemanticNodeBuilder::new(
                            &child_id,
                            "element",
                            tag,
                            absolute_line,
                            0,
                            absolute_line,
                            0,
                            "",
                        )
                        .children(attr_children)
                        .build(),
                    );
                }
                rest = &after_open[close_idx + 1..];
            } else {
                break;
            }
        }
        for label in extract_brace_labels(line) {
            let child_id = format!("{}.{}", id_prefix, children.len());
            children.push(make_leaf(&child_id, "interpolation", label, absolute_line));
        }
    }
    children
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

fn extract_script_children(content: &str, start_line: u32, id_prefix: &str) -> Vec<SemanticNode> {
    let mut children = Vec::new();
    for (line_idx, raw_line) in content.lines().enumerate() {
        let line = normalize_statement(raw_line);
        if line.is_empty() || line == "{" || line == "}" {
            continue;
        }
        let absolute_line = start_line + line_idx as u32;
        let child_id = format!("{}.{}", id_prefix, children.len());
        let node = if line.starts_with("import ") {
            Some(make_leaf(
                &child_id,
                "import_statement",
                line,
                absolute_line,
            ))
        } else if let Some(name) = declaration_name(&line) {
            let mut decl = make_leaf(
                &child_id,
                "variable_declaration",
                name.to_string(),
                absolute_line,
            );
            // #46: the RHS is review content — a value edit (let count = 0 -> 1)
            // hashed style-only with name-only declarations.
            if let Some((_, rhs)) = line.split_once('=') {
                let rhs = rhs.trim().trim_end_matches(';').trim();
                if !rhs.is_empty() {
                    decl.children = vec![make_leaf(
                        &format!("{child_id}.0"),
                        "declaration_value",
                        rhs.to_string(),
                        absolute_line,
                    )];
                }
            }
            Some(decl)
        } else if let Some(name) = function_name(&line) {
            Some(make_leaf(
                &child_id,
                "function_declaration",
                name.to_string(),
                absolute_line,
            ))
        } else {
            assignment_label(&line)
                .map(|label| make_leaf(&child_id, "assignment_statement", label, absolute_line))
        };
        if let Some(node) = node {
            children.push(node);
        }
    }
    children
}

fn extract_style_children(content: &str, start_line: u32, id_prefix: &str) -> Vec<SemanticNode> {
    let mut children = Vec::new();
    for (line_idx, raw_line) in content.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() || line == "}" {
            continue;
        }
        let absolute_line = start_line + line_idx as u32;
        let child_id = format!("{}.{}", id_prefix, children.len());
        if let Some(selector) = line.strip_suffix('{') {
            let selector = selector.trim();
            if !selector.is_empty() {
                children.push(make_leaf(&child_id, "style_rule", selector, absolute_line));
            }
        } else if let Some((property, _)) = line.split_once(':') {
            let property = property.trim();
            if !property.is_empty() {
                children.push(make_leaf(
                    &child_id,
                    "style_declaration",
                    property,
                    absolute_line,
                ));
            }
        }
    }
    children
}

fn process_impl(source: &str, filename: &str) -> String {
    let stem = filename
        .rsplit(['/', '\\'])
        .next()
        .and_then(|f| f.rsplit('.').nth(1))
        .unwrap_or("component");

    let mut blocks = extract_blocks(source);
    // Sort by start_line for deterministic output
    blocks.sort_by_key(|b| b.start_line);

    let end_line = source.lines().count().saturating_sub(1) as u32;

    let children: Vec<SemanticNode> = blocks
        .iter()
        .enumerate()
        .map(|(i, b)| {
            let id = format!("0.{}", i);
            let block_children = match b.node_type {
                "script_block" => extract_script_children(&b.content, b.content_start_line, &id),
                "style_block" => extract_style_children(&b.content, b.content_start_line, &id),
                "template_body" => extract_template_children(&b.content, b.content_start_line, &id),
                _ => Vec::new(),
            };
            SemanticNodeBuilder::new(
                &id,
                b.node_type,
                b.label.clone(),
                b.start_line,
                0,
                b.end_line,
                0,
                b.content_hash.clone(),
            )
            .children(block_children)
            .build()
        })
        .collect();

    let root_hash: String = {
        let combined: String = blocks
            .iter()
            .map(|b| b.content_hash.as_str())
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
        end_line,
        0,
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
    use crate::exports::intentumdiff::plugin::parser::Guest;

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
