use std::collections::HashSet;
use std::path::Path;

const MAX_ATTACHMENT_FILENAME_CHARS: usize = 128;

pub(crate) fn sanitize_attachment_filename(raw_name: &str, fallback_index: usize) -> String {
    let leaf = Path::new(raw_name)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(raw_name);

    let mut sanitized = String::new();
    let mut previous_was_separator = false;
    for ch in leaf.chars() {
        let safe = match ch {
            '/' | '\\' | ':' | '"' | '\'' | '<' | '>' | '|' | '?' | '*' | '\0' | '\r' | '\n' => {
                false
            }
            _ if ch.is_control() => false,
            _ => true,
        };

        if safe {
            sanitized.push(ch);
            previous_was_separator = false;
        } else if !previous_was_separator {
            sanitized.push('_');
            previous_was_separator = true;
        }
    }

    let trimmed = sanitized.trim_matches(|ch| matches!(ch, '.' | ' ' | '_'));
    let bounded: String = trimmed
        .chars()
        .take(MAX_ATTACHMENT_FILENAME_CHARS)
        .collect();
    if bounded.is_empty() {
        format!("attachment-{}", fallback_index.max(1))
    } else {
        bounded
    }
}

pub(crate) fn unique_attachment_filename(
    raw_name: &str,
    fallback_index: usize,
    used_names: &mut HashSet<String>,
) -> String {
    let sanitized = sanitize_attachment_filename(raw_name, fallback_index);
    if used_names.insert(sanitized.clone()) {
        return sanitized;
    }

    let (stem, extension) = split_extension(&sanitized);
    for suffix in 2..=999 {
        let candidate = match extension {
            Some(ext) => format!("{stem}-{suffix}.{ext}"),
            None => format!("{stem}-{suffix}"),
        };
        if used_names.insert(candidate.clone()) {
            return candidate;
        }
    }

    let candidate = format!("attachment-{}", used_names.len() + 1);
    used_names.insert(candidate.clone());
    candidate
}

fn split_extension(filename: &str) -> (&str, Option<&str>) {
    match filename.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && !ext.is_empty() => (stem, Some(ext)),
        _ => (filename, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_path_traversal_to_leaf_name() {
        assert_eq!(
            sanitize_attachment_filename("../../Library/LaunchAgents/evil.plist", 1),
            "evil.plist"
        );
    }

    #[test]
    fn replaces_windows_separators_and_header_breaks() {
        assert_eq!(
            sanitize_attachment_filename("..\\secret\r\nContent-Length: 0.pdf", 1),
            "secret_Content-Length_ 0.pdf"
        );
    }

    #[test]
    fn falls_back_for_empty_or_parent_names() {
        assert_eq!(sanitize_attachment_filename("..", 3), "attachment-3");
        assert_eq!(sanitize_attachment_filename("\r\n", 4), "attachment-4");
    }

    #[test]
    fn deduplicates_names_without_changing_extension() {
        let mut used = HashSet::new();
        assert_eq!(
            unique_attachment_filename("quote.pdf", 1, &mut used),
            "quote.pdf"
        );
        assert_eq!(
            unique_attachment_filename("quote.pdf", 2, &mut used),
            "quote-2.pdf"
        );
    }
}
