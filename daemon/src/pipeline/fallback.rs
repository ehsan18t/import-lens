//! Diagnostic details for a source no build accepted. Nothing here produces a size: a size exists
//! if and only if a build succeeded (ADR-0006).

pub(crate) fn source_excerpt_detail(source: &str) -> String {
    const MAX_EXCERPT_CHARS: usize = 240;
    let excerpt = source
        .chars()
        .take(MAX_EXCERPT_CHARS)
        .collect::<String>()
        .replace('\n', "\\n")
        .replace('\r', "\\r");

    if source.chars().count() > MAX_EXCERPT_CHARS {
        format!("source_excerpt: {excerpt}...")
    } else {
        format!("source_excerpt: {excerpt}")
    }
}
