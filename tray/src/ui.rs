const MAX_DIALOG_LINE_CHARS: usize = 68;

pub(crate) fn dialog_text(value: impl AsRef<str>) -> String {
    value
        .as_ref()
        .split('\n')
        .map(|line| wrap_line(line, MAX_DIALOG_LINE_CHARS))
        .collect::<Vec<_>>()
        .join("\n")
}

fn wrap_line(line: &str, max_chars: usize) -> String {
    if line.chars().count() <= max_chars || max_chars == 0 {
        return line.to_string();
    }

    let mut wrapped = String::new();
    let mut line_len = 0;
    for word in line.split_whitespace() {
        let mut word = word;
        while !word.is_empty() {
            let available = max_chars.saturating_sub(line_len + usize::from(line_len > 0));
            if word.chars().count() <= available {
                if line_len > 0 {
                    wrapped.push(' ');
                }
                wrapped.push_str(word);
                line_len += usize::from(line_len > 0) + word.chars().count();
                break;
            }

            if line_len > 0 {
                wrapped.push('\n');
                line_len = 0;
                continue;
            }

            let split_at = word
                .char_indices()
                .nth(max_chars)
                .map(|(index, _)| index)
                .unwrap_or(word.len());
            wrapped.push_str(&word[..split_at]);
            word = &word[split_at..];
            if !word.is_empty() {
                wrapped.push('\n');
            }
            line_len = 0;
        }
    }
    wrapped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dialogs_wrap_prose_and_unbroken_paths() {
        let input = format!(
            "A normal sentence with several words.\n\n/{}",
            "very-long-directory/".repeat(10)
        );
        let output = dialog_text(input);
        assert!(output
            .lines()
            .all(|line| line.chars().count() <= MAX_DIALOG_LINE_CHARS));
        assert!(output.contains("\n\n"));
    }
}
