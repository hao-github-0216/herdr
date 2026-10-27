//! Pure terminal-reference extraction. File access is deferred to file.preview.
use super::responses::encode_success;
use crate::api::schema::{FileReference, PaneLinkActivateParams, ResponseResult};
use crate::app::App;
use std::ops::Range;

fn position_suffix(value: &str) -> (&str, Option<u32>, Option<u32>) {
    let Some((head, last)) = value.rsplit_once(':') else {
        return (value, None, None);
    };
    let Ok(last) = last.parse::<u32>() else {
        return (value, None, None);
    };
    if last == 0 {
        return (value, None, None);
    }
    if let Some((path, line)) = head.rsplit_once(':') {
        if let Ok(line) = line.parse::<u32>() {
            if line > 0 {
                return (path, Some(line), Some(last));
            }
        }
    }
    (head, Some(last), None)
}

pub(crate) fn reference(value: &str) -> Option<FileReference> {
    if value.len() > 8192 || value.chars().any(char::is_control) {
        return None;
    }
    let value =
        value.trim_matches(|ch| matches!(ch, '`' | '\'' | '"' | '(' | ')' | '[' | ']' | ',' | ';'));
    let decoded;
    let value = if let Some(uri) = value.strip_prefix("file://") {
        let path = uri
            .strip_prefix("localhost")
            .filter(|path| path.starts_with('/'))
            .unwrap_or(uri);
        if !path.starts_with('/') {
            return None;
        }
        let mut bytes = Vec::new();
        let mut input = path.bytes();
        while let Some(ch) = input.next() {
            if ch == b'%' {
                let high = char::from(input.next()?).to_digit(16)?;
                let low = char::from(input.next()?).to_digit(16)?;
                bytes.push((high * 16 + low) as u8);
            } else {
                bytes.push(ch);
            }
        }
        decoded = String::from_utf8(bytes).ok()?;
        decoded.as_str()
    } else {
        value
    };
    if value.contains("://") || value.chars().any(char::is_control) {
        return None;
    }
    let (path, line, column) = position_suffix(value);
    if path.is_empty() || path.ends_with('/') || path.contains(':') {
        return None;
    }
    // A bare host:port or ordinary word is not evidence of a file. Recognize
    // common extensionless root names and dotfiles only with an explicit line.
    let positioned_filename = line.is_some()
        && path
            .chars()
            .all(|ch| ch.is_alphanumeric() || matches!(ch, '.' | '_' | '-'))
        && ((path.starts_with('.') && path.len() > 1)
            || matches!(
                path,
                "Makefile"
                    | "makefile"
                    | "Dockerfile"
                    | "Containerfile"
                    | "Justfile"
                    | "justfile"
                    | "LICENSE"
                    | "README"
                    | "CHANGELOG"
                    | "BUILD"
                    | "WORKSPACE"
            ));
    if !positioned_filename
        && !path.contains('/')
        && !path.rsplit_once('.').is_some_and(|(base, ext)| {
            !base.is_empty() && !ext.is_empty() && ext.chars().all(|ch| ch.is_ascii_alphanumeric())
        })
    {
        return None;
    }
    Some(FileReference {
        path: path.to_owned(),
        line,
        column,
    })
}

pub(crate) fn reference_range(text: &str, clicked: usize) -> Option<Range<usize>> {
    if clicked > text.len() || !text.is_char_boundary(clicked) {
        return None;
    }
    let start = text[..clicked.min(text.len())]
        .rfind(char::is_whitespace)
        .map_or(0, |index| index + 1);
    let end = text[clicked.min(text.len())..]
        .find(char::is_whitespace)
        .map_or(text.len(), |index| clicked + index);
    reference(text.get(start..end)?)?;
    Some(start..end)
}

fn link_range(text: &str, clicked: usize) -> Option<Range<usize>> {
    crate::app::actions::url_byte_range(text, clicked).or_else(|| reference_range(text, clicked))
}

impl App {
    pub(super) fn handle_file_link_resolve(
        &self,
        id: String,
        params: PaneLinkActivateParams,
    ) -> String {
        match self.read_checked_pane_link(&id, &params, "file resolution", |runtime, col, row| {
            runtime.link_regions_at(col, row, link_range)
        }) {
            Ok((_, regions)) => encode_success(id, ResponseResult::PaneLinkResolved { regions }),
            Err(error) => error,
        }
    }

    pub(super) fn handle_file_link_activate(
        &self,
        id: String,
        params: PaneLinkActivateParams,
    ) -> String {
        match self.read_checked_pane_link(&id, &params, "file activation", |runtime, col, row| {
            runtime
                .link_target_at(col, row)
                .and_then(|target| match target {
                    crate::ghostty::LinkTarget::Uri(uri) => reference(&uri),
                    crate::ghostty::LinkTarget::Text { text, clicked_byte } => {
                        let range = reference_range(&text, clicked_byte)?;
                        reference(&text[range])
                    }
                })
        }) {
            Ok((_, reference)) => encode_success(id, ResponseResult::FileReference { reference }),
            Err(error) => error,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn terminal_file_references_parse_position_osc8_and_reject_urls_hosts_controls() {
        let parsed = reference("src/main.rs:42:5").unwrap();
        assert_eq!(
            (parsed.path.as_str(), parsed.line, parsed.column),
            ("src/main.rs", Some(42), Some(5))
        );
        assert_eq!(
            reference("file:///repo/a%20b.rs:9").unwrap().path,
            "/repo/a b.rs"
        );
        for value in [
            "https://host/file.rs",
            "12:30:45",
            "file://otherhost/repo/a",
            "host:80",
            "a\u{1b}.rs",
        ] {
            assert!(reference(value).is_none(), "{value}");
        }
        assert_eq!(reference_range("see src/main.rs:5 now", 8), Some(4..17));
    }
    #[test]
    fn positioned_root_filenames_and_dotfiles_are_references_not_hosts_or_plain_words() {
        for value in [
            "Makefile:12",
            ".gitignore:3",
            ".env:2",
            "Dockerfile:7:4",
            "justfile:8",
        ] {
            let parsed = reference(value).unwrap();
            assert!(parsed.line.is_some());
            assert!(!parsed.path.contains(':'));
        }
        for value in [
            "ordinary",
            "ordinary:12",
            "host:80",
            "https://host/a:12",
            "12:30:45",
            ".env",
        ] {
            assert!(reference(value).is_none(), "{value}");
        }
    }
}
