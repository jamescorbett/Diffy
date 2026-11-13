//! Bounded literal search over supplied diff source, never paths or comments.
use gpui_kit::{
    SharedString,
    component::diff::{DiffFile, DiffLinePosition},
};
use regex::{Regex, RegexBuilder};
use std::sync::atomic::{AtomicBool, Ordering};

pub const MAX_RESULTS: usize = 1000;
pub const MAX_BYTES: usize = 128 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct Match {
    pub position: DiffLinePosition,
    pub snippet: SharedString,
}
#[derive(Default)]
pub struct Results {
    pub matches: Vec<Match>,
    pub truncated: bool,
    pub skipped: usize,
    pub error: Option<String>,
}
pub struct Matcher {
    pattern: Regex,
    pub results: Results,
    bytes: usize,
}
impl Matcher {
    pub fn new(query: &str, case_sensitive: bool) -> Result<Self, String> {
        if query.is_empty() || query.len() > 4096 {
            return Err("Enter between 1 and 4,096 bytes of search text.".into());
        }
        let pattern = RegexBuilder::new(&regex::escape(query))
            .case_insensitive(!case_sensitive)
            .build()
            .map_err(|_| "Couldn't prepare this search.")?;
        Ok(Self {
            pattern,
            results: Results::default(),
            bytes: 0,
        })
    }
    /// False means the caller should stop scanning (cancelled or bounded).
    pub fn file(&mut self, file: &DiffFile, cancel: &AtomicBool) -> bool {
        self.bytes = self.bytes.saturating_add(file.estimated_size_bytes());
        if self.bytes > MAX_BYTES {
            self.results.truncated = true;
            return false;
        }
        for (side, line, text) in file.search_lines() {
            if cancel.load(Ordering::Relaxed) {
                return false;
            }
            if let Some(found) = self.pattern.find(text) {
                if self.results.matches.len() == MAX_RESULTS {
                    self.results.truncated = true;
                    return false;
                }
                self.results.matches.push(Match {
                    position: DiffLinePosition::new(file.path().clone(), side, line),
                    snippet: snippet(text, found.start()).into(),
                });
            }
        }
        true
    }
}

fn snippet(text: &str, start: usize) -> String {
    let left = text[..start]
        .char_indices()
        .rev()
        .nth(48)
        .map_or(0, |(ix, _)| ix);
    // Keep snippets bounded even when the search term itself is very long.
    let right = text[start..]
        .char_indices()
        .nth(180)
        .map_or(text.len(), |(ix, _)| start + ix);
    format!(
        "{}{}{}",
        if left > 0 { "…" } else { "" },
        text[left..right].replace('\t', "  "),
        if right < text.len() { "…" } else { "" }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::component::diff::DiffSide;
    #[test]
    fn search_is_literal_unicode_aware_and_does_not_duplicate_context() {
        let file = DiffFile::parse("diff --git a/f.rs b/f.rs\n--- a/f.rs\n+++ b/f.rs\n@@ -7,2 +7,2 @@\n café a+b\n-old a+b\n+new a+b\n").unwrap().pop().unwrap();
        let mut matcher = Matcher::new("a+b", false).unwrap();
        matcher.file(&file, &AtomicBool::new(false));
        assert_eq!(matcher.results.matches.len(), 3);
        assert_eq!(matcher.results.matches[0].position.line(), 7);
        assert_eq!(
            matcher.results.matches[2].position.side(),
            DiffSide::Original
        );
        let mut matcher = Matcher::new("CAFÉ", false).unwrap();
        matcher.file(&file, &AtomicBool::new(false));
        assert_eq!(matcher.results.matches.len(), 1);
        let mut matcher = Matcher::new("CAFÉ", true).unwrap();
        matcher.file(&file, &AtomicBool::new(false));
        assert!(matcher.results.matches.is_empty());
    }
    #[test]
    fn results_are_bounded_cancellable_and_snippets_show_late_matches() {
        let file = DiffFile::unchanged("large.txt", &"match\n".repeat(MAX_RESULTS + 1));
        let mut matcher = Matcher::new("match", false).unwrap();
        assert!(!matcher.file(&file, &AtomicBool::new(false)));
        assert_eq!(matcher.results.matches.len(), MAX_RESULTS);
        assert!(matcher.results.truncated);
        let mut matcher = Matcher::new("match", false).unwrap();
        assert!(!matcher.file(&file, &AtomicBool::new(true)));
        assert!(matcher.results.matches.is_empty());
        let text = format!("{}needle{}", "é".repeat(5000), "界".repeat(5000));
        let found = text.find("needle").unwrap();
        let short = snippet(&text, found);
        assert!(short.contains("needle"));
        assert!(short.chars().count() < 240);
        assert!(Matcher::new("", false).is_err());
    }
}
