use crate::ipc::protocol::{SourcePosition, SourceRange};

pub struct LineIndex {
    line_starts: Vec<usize>,
}

impl LineIndex {
    pub fn new(source: &str) -> Self {
        let bytes = source.as_bytes();
        let mut line_starts = vec![0];

        let mut index = 0;
        while index < bytes.len() {
            match bytes[index] {
                b'\n' => line_starts.push(index + 1),
                b'\r' => {
                    if bytes.get(index + 1) == Some(&b'\n') {
                        line_starts.push(index + 2);
                        index += 2;
                        continue;
                    }
                    line_starts.push(index + 1);
                }
                _ => {}
            }
            index += 1;
        }

        Self { line_starts }
    }

    // `offset` must sit on a char boundary of `source`; callers pass oxc span
    // boundaries or scanner offsets, which always do. Characters count UTF-16
    // code units to match VS Code position semantics.
    pub fn position_at(&self, source: &str, offset: usize) -> SourcePosition {
        let safe_offset = offset.min(source.len());
        let line = self
            .line_starts
            .partition_point(|start| *start <= safe_offset)
            - 1;
        let line_start = self.line_starts[line];
        let character = source[line_start..safe_offset]
            .chars()
            .map(|char| char.len_utf16() as u32)
            .sum();

        SourcePosition {
            line: line as u32,
            character,
        }
    }

    pub fn range_from_offsets(&self, source: &str, start: usize, end: usize) -> SourceRange {
        SourceRange {
            start: self.position_at(source, start),
            end: self.position_at(source, end),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::LineIndex;

    #[test]
    fn line_index_handles_lf() {
        let source = "ab\ncd";
        let index = LineIndex::new(source);
        let positions = (0..=source.len())
            .map(|offset| {
                let position = index.position_at(source, offset);
                (position.line, position.character)
            })
            .collect::<Vec<_>>();
        assert_eq!(
            positions,
            vec![(0, 0), (0, 1), (0, 2), (1, 0), (1, 1), (1, 2)]
        );
    }

    #[test]
    fn line_index_handles_crlf() {
        let source = "ab\r\ncd";
        let index = LineIndex::new(source);
        assert_eq!(index.position_at(source, 2).line, 0);
        assert_eq!(index.position_at(source, 4).line, 1);
        assert_eq!(index.position_at(source, 4).character, 0);
    }

    #[test]
    fn line_index_handles_lone_cr() {
        let source = "a\rb";
        let index = LineIndex::new(source);
        assert_eq!(index.position_at(source, 2).line, 1);
        assert_eq!(index.position_at(source, 2).character, 0);
    }

    #[test]
    fn line_index_counts_utf16_columns() {
        let source = "const s = '\u{1D11E}x';";
        let index = LineIndex::new(source);
        let x_offset = source.find('x').expect("x exists");
        assert_eq!(
            index.position_at(source, x_offset).character,
            "const s = '".len() as u32 + 2
        );
    }

    #[test]
    fn line_index_clamps_out_of_range_offsets() {
        let index = LineIndex::new("ab");
        assert_eq!(index.position_at("ab", 99).character, 2);
    }
}
