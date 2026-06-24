use crate::ipc::protocol::{SourcePosition, SourceRange};

pub fn position_at(source: &str, offset: usize) -> SourcePosition {
    let safe_offset = offset.min(source.len());
    let bytes = source.as_bytes();
    let mut index = 0;
    let mut line = 0_u32;
    let mut character = 0_u32;

    while index < safe_offset {
        match bytes[index] {
            b'\r' => {
                line += 1;
                character = 0;
                index += if index + 1 < safe_offset && bytes[index + 1] == b'\n' {
                    2
                } else {
                    1
                };
            }
            b'\n' => {
                line += 1;
                character = 0;
                index += 1;
            }
            _ => {
                let Some(char) = source[index..].chars().next() else {
                    break;
                };
                character += char.len_utf16() as u32;
                index += char.len_utf8();
            }
        }
    }

    SourcePosition { line, character }
}

pub fn range_from_offsets(source: &str, start: usize, end: usize) -> SourceRange {
    SourceRange {
        start: position_at(source, start),
        end: position_at(source, end),
    }
}
