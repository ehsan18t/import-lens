#[derive(Debug)]
pub(crate) struct Replacement {
    start: usize,
    end: usize,
    value: String,
}

impl Replacement {
    pub(crate) fn remove(start: usize, end: usize) -> Self {
        Self {
            start,
            end,
            value: String::new(),
        }
    }

    pub(crate) fn replace(start: usize, end: usize, value: String) -> Self {
        Self { start, end, value }
    }
}

pub(crate) fn span_overlaps_replacements(
    start: usize,
    end: usize,
    replacements: &[Replacement],
) -> bool {
    replacements
        .iter()
        .any(|replacement| start < replacement.end && end > replacement.start)
}

pub(crate) fn apply_replacements(
    source: &str,
    mut replacements: Vec<Replacement>,
) -> Result<String, String> {
    replacements.sort_by(|a, b| {
        b.start
            .cmp(&a.start)
            .then_with(|| b.end.cmp(&a.end))
            .then_with(|| a.value.len().cmp(&b.value.len()))
    });

    let source_len = source.len();
    let mut valid_replacements = Vec::new();
    let mut last_start = source_len;

    for replacement in replacements {
        if replacement.start > replacement.end || replacement.end > source_len {
            return Err(format!(
                "invalid replacement span {}..{}",
                replacement.start, replacement.end
            ));
        }
        if replacement.end > last_start {
            continue;
        }
        last_start = replacement.start;
        valid_replacements.push(replacement);
    }

    valid_replacements.reverse();

    let mut output = String::with_capacity(source.len());
    let mut last_end = 0;

    for replacement in valid_replacements {
        output.push_str(&source[last_end..replacement.start]);
        output.push_str(&replacement.value);
        last_end = replacement.end;
    }
    output.push_str(&source[last_end..]);

    Ok(output)
}
