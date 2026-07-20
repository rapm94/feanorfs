const MAX_MENU_LABEL_CHARS: usize = 52;
const MAX_DIALOG_LINE_CHARS: usize = 68;

pub(crate) fn menu_label(value: impl AsRef<str>) -> String {
    truncate_middle(value.as_ref(), MAX_MENU_LABEL_CHARS)
}

pub(crate) fn menu_label_with_suffix(value: &str, suffix: &str) -> String {
    let suffix_chars = suffix.chars().count();
    let body_chars = MAX_MENU_LABEL_CHARS.saturating_sub(suffix_chars);
    format!("{}{}", truncate_middle(value, body_chars), suffix)
}

pub(crate) fn dialog_text(value: impl AsRef<str>) -> String {
    value
        .as_ref()
        .split('\n')
        .map(|line| wrap_line(line, MAX_DIALOG_LINE_CHARS))
        .collect::<Vec<_>>()
        .join("\n")
}

fn truncate_middle(value: &str, max_chars: usize) -> String {
    let chars = value.chars().collect::<Vec<_>>();
    if chars.len() <= max_chars {
        return value.to_string();
    }
    if max_chars == 0 {
        return String::new();
    }
    if max_chars == 1 {
        return "…".into();
    }
    let visible = max_chars - 1;
    let left = visible / 2;
    let right = visible - left;
    chars[..left]
        .iter()
        .chain(std::iter::once(&'…'))
        .chain(chars[chars.len() - right..].iter())
        .collect()
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
    fn menu_labels_keep_both_ends_with_a_hard_bound() {
        let input = "project — ~/a/very/long/path/that/keeps/growing/project";
        let output = menu_label(input);
        assert!(output.starts_with("project"));
        assert!(output.ends_with("project"));
        assert!(output.contains('…'));
        assert!(output.chars().count() <= MAX_MENU_LABEL_CHARS);
    }

    #[test]
    fn suffix_stays_visible_inside_the_menu_bound() {
        let output = menu_label_with_suffix(&"x".repeat(100), " — unavailable");
        assert!(output.ends_with(" — unavailable"));
        assert!(output.chars().count() <= MAX_MENU_LABEL_CHARS);
    }

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
