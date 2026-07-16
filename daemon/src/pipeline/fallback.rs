//! What is left of the fallback sizers: nothing that produces a size.
//!
//! This module used to hold the two fabricators — `approximate_directory_size`, which measured the
//! package's bytes ON DISK (unminified, uncompressed, tests, source maps, unused files) and handed
//! that one number to all five size fields, and `estimate_minified_source`, which stripped comments
//! and collapsed whitespace so a source the minifier had rejected could still be given a plausible
//! byte count. Both are deleted: a size exists if and only if a build succeeded (ADR-0006).
//!
//! `source_excerpt_detail` survives because it produces a diagnostic DETAIL, not a size.

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
