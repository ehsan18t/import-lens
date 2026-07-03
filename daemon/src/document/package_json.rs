use super::positions::LineIndex;
use crate::ipc::protocol::SourceRange;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageJsonDependencyEntry {
    pub name: String,
    pub version: String,
    pub section: String,
    pub range: SourceRange,
    pub name_range: SourceRange,
    pub value_range: SourceRange,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageJsonDependencySection {
    pub section: String,
    pub range: SourceRange,
    pub object_range: SourceRange,
}

const DEPENDENCY_SECTION_NAMES: [&str; 4] = [
    "dependencies",
    "devDependencies",
    "peerDependencies",
    "optionalDependencies",
];

#[derive(Debug, Clone, PartialEq, Eq)]
struct JsonStringToken {
    value: String,
    start: usize,
    end: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DependencySectionObject {
    section: String,
    key_start: usize,
    key_end: usize,
    object_start: usize,
    object_end: usize,
}

pub fn package_json_dependency_entries(source: &str) -> Vec<PackageJsonDependencyEntry> {
    let Ok(parsed) = serde_json::from_str::<Value>(source) else {
        return Vec::new();
    };
    let Some(root) = parsed.as_object() else {
        return Vec::new();
    };

    let line_index = LineIndex::new(source);
    let mut entries = Vec::new();
    for section in dependency_section_objects(source) {
        let Some(dependencies) = root.get(&section.section).and_then(Value::as_object) else {
            continue;
        };

        if dependencies.is_empty() {
            continue;
        }

        entries.extend(dependency_entries_for_section(source, &line_index, &section));
    }

    entries.sort_by(|left, right| {
        left.range
            .start
            .line
            .cmp(&right.range.start.line)
            .then_with(|| left.name.cmp(&right.name))
    });
    entries
}

pub fn package_json_dependency_sections(source: &str) -> Vec<PackageJsonDependencySection> {
    if serde_json::from_str::<Value>(source).is_err() {
        return Vec::new();
    }

    let line_index = LineIndex::new(source);
    dependency_section_objects(source)
        .into_iter()
        .map(|section| PackageJsonDependencySection {
            section: section.section,
            range: line_index.range_from_offsets(source, section.key_start, section.key_end),
            object_range: line_index.range_from_offsets(
                source,
                section.object_start,
                section.object_end,
            ),
        })
        .collect()
}

fn dependency_entries_for_section(
    source: &str,
    line_index: &LineIndex,
    section: &DependencySectionObject,
) -> Vec<PackageJsonDependencyEntry> {
    let mut entries = Vec::new();
    let mut depth = 0_i32;
    let mut current = section.object_start;

    while current < section.object_end {
        let byte = source.as_bytes()[current];

        if byte == b'"' {
            let Some(name_token) = read_json_string(source, current) else {
                return entries;
            };

            let colon_offset = skip_whitespace(source, name_token.end);
            let value_start = skip_whitespace(source, colon_offset + 1);
            let value_token = if source.as_bytes().get(colon_offset) == Some(&b':') {
                read_json_string(source, value_start)
            } else {
                None
            };

            if depth == 1
                && let Some(value_token) = value_token
            {
                let name_range =
                    line_index.range_from_offsets(source, name_token.start, name_token.end);
                entries.push(PackageJsonDependencyEntry {
                    name: name_token.value,
                    version: value_token.value,
                    section: section.section.clone(),
                    range: name_range,
                    name_range,
                    value_range: line_index.range_from_offsets(
                        source,
                        value_token.start,
                        value_token.end,
                    ),
                });
            }

            current = name_token.end;
            continue;
        }

        if byte == b'{' || byte == b'[' {
            depth += 1;
        } else if byte == b'}' || byte == b']' {
            depth -= 1;
        }

        current += 1;
    }

    entries
}

fn dependency_section_objects(source: &str) -> Vec<DependencySectionObject> {
    let mut sections = Vec::new();
    let mut depth = 0_i32;
    let mut current = 0_usize;

    while current < source.len() {
        let byte = source.as_bytes()[current];

        if byte == b'"' {
            let Some(token) = read_json_string(source, current) else {
                return sections;
            };

            let colon_offset = skip_whitespace(source, token.end);
            if depth == 1
                && source.as_bytes().get(colon_offset) == Some(&b':')
                && is_dependency_section_name(&token.value)
            {
                let object_start = skip_whitespace(source, colon_offset + 1);
                let object_end = if source.as_bytes().get(object_start) == Some(&b'{') {
                    matching_object_end(source, object_start)
                } else {
                    None
                };

                if let Some(object_end) = object_end {
                    sections.push(DependencySectionObject {
                        section: token.value,
                        key_start: token.start,
                        key_end: token.end,
                        object_start,
                        object_end,
                    });
                }
            }

            current = token.end;
            continue;
        }

        if byte == b'{' || byte == b'[' {
            depth += 1;
        } else if byte == b'}' || byte == b']' {
            depth -= 1;
        }

        current += 1;
    }

    sections
}

fn read_json_string(source: &str, start: usize) -> Option<JsonStringToken> {
    if source.as_bytes().get(start) != Some(&b'"') {
        return None;
    }

    let mut current = start + 1;
    while current < source.len() {
        match source.as_bytes()[current] {
            b'\\' => current += 2,
            b'"' => {
                let end = current + 1;
                let value = serde_json::from_str::<String>(&source[start..end]).ok()?;
                return Some(JsonStringToken { value, start, end });
            }
            _ => current += 1,
        }
    }

    None
}

fn matching_object_end(source: &str, object_start: usize) -> Option<usize> {
    let mut depth = 0_i32;
    let mut current = object_start;

    while current < source.len() {
        match source.as_bytes()[current] {
            b'"' => {
                let token = read_json_string(source, current)?;
                current = token.end;
                continue;
            }
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(current + 1);
                }
            }
            _ => {}
        }

        current += 1;
    }

    None
}

fn skip_whitespace(source: &str, mut offset: usize) -> usize {
    while source
        .as_bytes()
        .get(offset)
        .is_some_and(|byte| matches!(byte, b' ' | b'\n' | b'\r' | b'\t'))
    {
        offset += 1;
    }

    offset
}

fn is_dependency_section_name(value: &str) -> bool {
    DEPENDENCY_SECTION_NAMES.contains(&value)
}
