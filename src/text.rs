use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const MAX_CHAT_BYTES: usize = 4_096;
const MAX_CHAT_GRAPHEMES: usize = 1_024;
const MAX_NAME_BYTES: usize = 256;
const MAX_NAME_GRAPHEMES: usize = 64;
const MAX_ZERO_WIDTH_CHARS_PER_GRAPHEME: usize = 8;
const TRUNCATION_MARKER: &str = "…";

pub(crate) struct DisplayTextSanitizer {
    remaining_bytes: usize,
    remaining_graphemes: usize,
    exhausted: bool,
}

impl DisplayTextSanitizer {
    pub(crate) const fn chat_message() -> Self {
        Self::new(MAX_CHAT_BYTES, MAX_CHAT_GRAPHEMES)
    }

    const fn display_name() -> Self {
        Self::new(MAX_NAME_BYTES, MAX_NAME_GRAPHEMES)
    }

    const fn new(max_bytes: usize, max_graphemes: usize) -> Self {
        Self {
            remaining_bytes: max_bytes,
            remaining_graphemes: max_graphemes,
            exhausted: false,
        }
    }

    pub(crate) fn sanitize(&mut self, input: &str) -> String {
        if self.exhausted {
            return String::new();
        }

        let cleaned = clean_controls(input);
        let mut output = String::with_capacity(cleaned.len().min(self.remaining_bytes));
        for grapheme in cleaned.graphemes(true) {
            let grapheme = bound_zero_width_chars(grapheme);
            if grapheme.is_empty() || grapheme.width() == 0 {
                continue;
            }
            if self.remaining_graphemes <= 1
                || grapheme.len() > self.remaining_bytes.saturating_sub(TRUNCATION_MARKER.len())
            {
                self.append_truncation_marker(&mut output);
                break;
            }
            self.remaining_bytes -= grapheme.len();
            self.remaining_graphemes -= 1;
            output.push_str(&grapheme);
        }
        output
    }

    fn append_truncation_marker(&mut self, output: &mut String) {
        if TRUNCATION_MARKER.len() <= self.remaining_bytes && self.remaining_graphemes > 0 {
            output.push_str(TRUNCATION_MARKER);
            self.remaining_bytes -= TRUNCATION_MARKER.len();
            self.remaining_graphemes -= 1;
        }
        self.exhausted = true;
    }
}

pub(crate) fn sanitize_display_name(input: &str) -> String {
    DisplayTextSanitizer::display_name().sanitize(input)
}

fn clean_controls(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    for character in input.chars() {
        match character {
            '\n' | '\r' | '\t' => output.push(' '),
            character if is_unsafe_format(character) || character.is_control() => {}
            character => output.push(character),
        }
    }
    output
}

fn is_unsafe_format(character: char) -> bool {
    matches!(
        character,
        '\u{061C}'
            | '\u{200B}'
            | '\u{200E}'
            | '\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'
            | '\u{2066}'..='\u{2069}'
            | '\u{FEFF}'
    )
}

fn bound_zero_width_chars(grapheme: &str) -> String {
    let mut zero_width_chars = 0;
    grapheme
        .chars()
        .filter(|character| {
            if character.width() == Some(0) {
                zero_width_chars += 1;
                zero_width_chars <= MAX_ZERO_WIDTH_CHARS_PER_GRAPHEME
            } else {
                true
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_terminal_and_bidi_controls() {
        let mut sanitizer = DisplayTextSanitizer::chat_message();
        assert_eq!(
            sanitizer.sanitize("hello\x1b[31m\u{202E}world\u{200B}\r\nnext"),
            "hello[31mworld  next"
        );
    }

    #[test]
    fn bounds_zalgo_marks_without_removing_the_base_character() {
        let input = format!("a{}b", "\u{0301}".repeat(100));
        let mut sanitizer = DisplayTextSanitizer::chat_message();
        let output = sanitizer.sanitize(&input);

        assert_eq!(
            output
                .chars()
                .filter(|character| *character == '\u{0301}')
                .count(),
            8
        );
        assert!(output.starts_with('a'));
        assert!(output.ends_with('b'));
    }

    #[test]
    fn preserves_normal_joined_emoji() {
        let mut sanitizer = DisplayTextSanitizer::chat_message();
        assert_eq!(sanitizer.sanitize("family 👨‍👩‍👧‍👦"), "family 👨‍👩‍👧‍👦");
    }

    #[test]
    fn skips_orphaned_zero_width_graphemes() {
        let mut sanitizer = DisplayTextSanitizer::chat_message();
        assert_eq!(sanitizer.sanitize("\u{0301}\u{0302}hello"), "hello");
    }

    #[test]
    fn truncates_oversized_messages_once_across_fragments() {
        let mut sanitizer = DisplayTextSanitizer::chat_message();
        let first = sanitizer.sanitize(&"a".repeat(MAX_CHAT_BYTES + 10));
        let second = sanitizer.sanitize("later");

        assert!(first.ends_with(TRUNCATION_MARKER));
        assert!(first.len() <= MAX_CHAT_BYTES);
        assert!(second.is_empty());
    }

    #[test]
    fn bounds_display_names_separately() {
        let output = sanitize_display_name(&"a".repeat(MAX_NAME_GRAPHEMES + 10));
        assert!(output.ends_with(TRUNCATION_MARKER));
        assert_eq!(output.graphemes(true).count(), MAX_NAME_GRAPHEMES);
    }
}
