//! cmd+click link opening (egui_term patch P10 hands us the matched text).
//! URLs open as-is; path-shaped tokens are resolved against the pane's live
//! cwd and only opened when they exist, which is also what filters the
//! regex's false positives (`and/or` in prose resolves to nothing).

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::tmux::TmuxCtl;

/// The pane's link opener. Off the render thread: resolving a relative
/// path shells out to tmux for the pane's cwd, and `open` itself can take
/// tens of ms.
pub fn spawn_open(tmux: TmuxCtl, session: String, text: String) {
    std::thread::spawn(move || {
        let target = if is_url(&text) {
            Some(text)
        } else {
            let cwd = tmux.pane_current_path(&session);
            let home = std::env::var("HOME").ok();
            resolve_path(&text, cwd.as_deref(), home.as_deref())
                .map(|p| p.display().to_string())
        };
        if let Some(target) = target {
            // Every non-URL target is an absolute path (resolve_path
            // guarantees it), so it can't be mistaken for an `open` flag.
            let _ = Command::new("/usr/bin/open").arg(&target).status();
        }
    });
}

/// Mirrors the scheme list of egui_term's URL regex: text the widget
/// matched as a URL must be recognized as one here, or it would be
/// existence-checked as a path and silently dropped.
fn is_url(text: &str) -> bool {
    [
        "ipfs:", "ipns:", "magnet:", "mailto:", "gemini://", "gopher://",
        "https://", "http://", "news:", "file://", "git://", "ssh:", "ftp://",
    ]
    .iter()
    .any(|scheme| text.starts_with(scheme))
}

/// First existing filesystem path the matched token can mean, absolute.
fn resolve_path(
    text: &str,
    cwd: Option<&str>,
    home: Option<&str>,
) -> Option<PathBuf> {
    for candidate in candidates(text) {
        if let Some(path) = expand(&candidate, cwd, home) {
            if path.exists() {
                return Some(path);
            }
        }
    }
    None
}

/// What the token might denote, most literal first: as matched, with a
/// rustc/grep-style `:line[:col]` suffix stripped, and with sentence
/// punctuation trimmed ("see src/app.rs.").
fn candidates(text: &str) -> Vec<String> {
    // Punctuation first: a trailing dot would otherwise hide the
    // `:line:col` suffix from strip_line_col ("src/app.rs:12:34.").
    let trimmed = text.trim_end_matches(['.', ',']);
    let mut out: Vec<String> = Vec::new();
    for cand in
        [text, trimmed, strip_line_col(text), strip_line_col(trimmed)]
    {
        if !cand.is_empty() && !out.iter().any(|c| c == cand) {
            out.push(cand.to_string());
        }
    }
    out
}

/// Strip up to two trailing `:<digits>` groups (`src/app.rs:12:34`).
fn strip_line_col(text: &str) -> &str {
    let mut s = text;
    for _ in 0..2 {
        match s.rsplit_once(':') {
            Some((head, tail))
                if !tail.is_empty()
                    && tail.bytes().all(|b| b.is_ascii_digit()) =>
            {
                s = head;
            },
            _ => break,
        }
    }
    s
}

/// Absolute form of a candidate: `~` expands to home, relative paths hang
/// off the pane's cwd; None when the needed base is unknown.
fn expand(
    text: &str,
    cwd: Option<&str>,
    home: Option<&str>,
) -> Option<PathBuf> {
    if text == "~" {
        return home.map(PathBuf::from);
    }
    if let Some(rest) = text.strip_prefix("~/") {
        return home.map(|h| Path::new(h).join(rest));
    }
    let path = Path::new(text);
    if path.is_absolute() {
        return Some(path.to_path_buf());
    }
    cwd.map(|c| Path::new(c).join(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_are_recognized() {
        assert!(is_url("https://example.com/a?b=1"));
        assert!(is_url("mailto:someone@example.com"));
        assert!(is_url("file:///tmp/x"));
        assert!(!is_url("src/app.rs"));
        assert!(!is_url("/tmp/https://not-a-url"));
    }

    #[test]
    fn line_col_suffixes_strip() {
        assert_eq!(strip_line_col("src/app.rs:12:34"), "src/app.rs");
        assert_eq!(strip_line_col("src/app.rs:12"), "src/app.rs");
        assert_eq!(strip_line_col("src/app.rs"), "src/app.rs");
        // Not a line number: a colon-embedded name stays intact.
        assert_eq!(strip_line_col("a:b/c"), "a:b/c");
        // Only two groups strip; a third is part of the name.
        assert_eq!(strip_line_col("x:1:2:3"), "x:1");
    }

    #[test]
    fn candidates_are_ordered_and_deduped() {
        assert_eq!(
            candidates("src/app.rs:12:34."),
            vec!["src/app.rs:12:34.", "src/app.rs:12:34", "src/app.rs"]
        );
        assert_eq!(candidates("src/app.rs"), vec!["src/app.rs"]);
        assert_eq!(candidates("a/b."), vec!["a/b.", "a/b"]);
    }

    #[test]
    fn expansion_covers_home_cwd_and_absolute() {
        assert_eq!(
            expand("~/x", None, Some("/home/u")),
            Some(PathBuf::from("/home/u/x"))
        );
        assert_eq!(expand("~", None, Some("/home/u")), Some("/home/u".into()));
        assert_eq!(expand("~/x", None, None), None);
        assert_eq!(expand("/a/b", None, None), Some(PathBuf::from("/a/b")));
        assert_eq!(
            expand("src/app.rs", Some("/repo"), None),
            Some(PathBuf::from("/repo/src/app.rs"))
        );
        assert_eq!(expand("src/app.rs", None, None), None);
    }

    #[test]
    fn resolution_requires_existence() {
        let dir = std::env::temp_dir().join("muxterm-links-test");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/app.rs"), "x").unwrap();
        let cwd = dir.to_str().unwrap();

        assert_eq!(
            resolve_path("src/app.rs:42:7", Some(cwd), None),
            Some(dir.join("src/app.rs"))
        );
        assert_eq!(resolve_path("src/gone.rs", Some(cwd), None), None);
        assert_eq!(resolve_path("and/or", Some(cwd), None), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
