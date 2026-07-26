//! cmd+click link opening (egui_term patches P10/P20 hand us the matched
//! candidate texts, most complete first - P20 speculatively rejoins tokens
//! a TUI hard-wrapped across rows, so a joined guess arrives ahead of its
//! unjoined fallback). URLs open as-is; path-shaped tokens are resolved
//! against the pane's live cwd and only opened when they exist, which is
//! what filters both the regex's false positives (`and/or` in prose
//! resolves to nothing) and bad wrap-join guesses (`src/app.rsand` loses
//! to `src/app.rs`).
//!
//! P28 runs that same judgement on *hover*: `resolve_target` doubles as the
//! widget's link validator, so the cmd-held underline covers exactly the
//! candidate a click would open instead of P20's longest guess. The hover
//! path answers from the App's cached pane cwd (`tmux::SharedPanes`, already
//! refreshed once a second) rather than shelling out per frame.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::tmux::TmuxCtl;

/// The pane's link opener: the first candidate that resolves wins. Off the
/// render thread: resolving a relative path shells out to tmux for the
/// pane's cwd, and `open` itself can take tens of ms.
///
/// `pr_base` is the pane repo's GitHub web base when the PR detector is on
/// (config `pr_detector`) and the repo has known PRs: a `#123` candidate
/// (egui_term P24 emits one only for a known PR number) then opens
/// `<base>/pull/123`. PR-first is safe: a `#N` candidate can only coexist
/// with speculative path-join guesses, never a URL.
pub fn spawn_open(
    tmux: TmuxCtl,
    session: String,
    texts: Vec<String>,
    pr_base: Option<String>,
) {
    std::thread::spawn(move || {
        // One cwd fetch serves every candidate; URLs and PR tokens need none.
        let cwd = texts
            .iter()
            .any(|t| !is_url(t) && !is_pr_token(t))
            .then(|| tmux.pane_current_path(&session))
            .flatten();
        let home = std::env::var("HOME").ok();
        let target = texts.iter().find_map(|text| {
            resolve_target(text, cwd.as_deref(), home.as_deref(), pr_base.as_deref())
        });
        if let Some(target) = target {
            // Every non-URL target is an absolute path (resolve_path
            // guarantees it), so it can't be mistaken for an `open` flag.
            let _ = Command::new("/usr/bin/open").arg(&target).status();
        }
    });
}

/// What `open` would be handed for this candidate, or None when the
/// candidate means nothing: a known PR number first (egui_term P24 only
/// emits `#N` for one), then a URL as-is, then a path that exists.
///
/// muxterm patch P28: also the hover validator (`set_link_validator`, wired
/// in `app::create_pane`), so the underline shows exactly what a click will
/// open. Both callers going through this one function is what keeps them
/// from drifting.
pub fn resolve_target(
    text: &str,
    cwd: Option<&str>,
    home: Option<&str>,
    pr_base: Option<&str>,
) -> Option<String> {
    if let Some(url) = pr_base.and_then(|base| pr_url(text, base)) {
        return Some(url);
    }
    if is_url(text) {
        return Some(text.to_string());
    }
    resolve_path(text, cwd, home).map(|p| p.display().to_string())
}

/// The repo web base of a PR URL, as GitHub's API hands them out:
/// "https://github.com/owner/repo/pull/12" -> "https://github.com/owner/repo".
pub fn pr_base(url: &str) -> Option<&str> {
    url.find("/pull/").map(|i| &url[..i])
}

/// A bare `#<digits>` token - egui_term P24 emits one only for a PR number
/// the app registered, so its shape is the whole test.
fn is_pr_token(text: &str) -> bool {
    text.strip_prefix('#')
        .is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
}

/// "#123" -> "<base>/pull/123"; any other token shape is None.
fn pr_url(text: &str, base: &str) -> Option<String> {
    is_pr_token(text).then(|| format!("{base}/pull/{}", &text[1..]))
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
    // A bare-relative token can be prose glued to an absolute path - Claude
    // Code prints `[image/private/tmp/x.png`, which the grid regex matches
    // from `image` - so each bare form also tries from its first slash.
    // After the literal forms (a real relative path resolves against the
    // cwd first), and only for multi-segment tails: `usr/bin` in a tar
    // listing must not open `/bin`.
    let stripped: Vec<String> = out
        .iter()
        .filter(|c| !c.starts_with(['/', '~', '.']))
        .filter_map(|c| {
            let i = c.find('/')?;
            c[i + 1..].contains('/').then(|| c[i..].to_string())
        })
        .collect();
    for cand in stripped {
        if !out.iter().any(|c| *c == cand) {
            out.push(cand);
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
    fn pr_urls_build_from_the_badge_base() {
        let badge = "https://github.com/herval/muxterm/pull/12";
        let base = pr_base(badge).unwrap();
        assert_eq!(base, "https://github.com/herval/muxterm");
        assert_eq!(
            pr_url("#123", base).as_deref(),
            Some("https://github.com/herval/muxterm/pull/123")
        );
        // Only a bare #digits token qualifies.
        assert_eq!(pr_url("123", base), None);
        assert_eq!(pr_url("#", base), None);
        assert_eq!(pr_url("#12a", base), None);
        // No /pull/ segment, no base.
        assert_eq!(pr_base("https://github.com/herval/muxterm"), None);
    }

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
    fn glued_junk_strips_to_the_absolute_tail() {
        assert_eq!(
            candidates("image/private/tmp/x.png"),
            vec!["image/private/tmp/x.png", "/private/tmp/x.png"]
        );
        // A single-segment tail stays put: `usr/bin` must not try `/bin`.
        assert_eq!(candidates("usr/bin"), vec!["usr/bin"]);
    }

    #[test]
    fn glued_absolute_paths_resolve() {
        let dir = std::env::temp_dir().join("muxterm-links-glue-test");
        std::fs::create_dir_all(dir.join("a")).unwrap();
        std::fs::write(dir.join("a/x.png"), "x").unwrap();
        let glued = format!("image{}", dir.join("a/x.png").display());
        assert_eq!(resolve_path(&glued, None, None), Some(dir.join("a/x.png")));
        std::fs::remove_dir_all(&dir).unwrap();
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

    // muxterm patch P28: the one judgement hover and click share.
    #[test]
    fn resolve_target_covers_prs_urls_and_paths() {
        let dir = std::env::temp_dir().join("muxterm-links-target-test");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("app.rs"), "x").unwrap();
        let cwd = dir.to_str().unwrap();
        let base = Some("https://github.com/h/m");

        // A known PR wins, and only with a base to build the URL from.
        assert_eq!(
            resolve_target("#12", None, None, base).as_deref(),
            Some("https://github.com/h/m/pull/12")
        );
        assert_eq!(resolve_target("#12", None, None, None), None);
        // URLs open as-is, no cwd needed.
        assert_eq!(
            resolve_target("https://x.com/a", None, None, None).as_deref(),
            Some("https://x.com/a")
        );
        // Paths must exist - this is what filters P20's wrap-join guesses
        // and prose false positives, on hover as well as on click.
        assert_eq!(
            resolve_target("app.rs:9", Some(cwd), None, None),
            Some(dir.join("app.rs").display().to_string())
        );
        assert_eq!(resolve_target("and/or", Some(cwd), None, None), None);
        assert_eq!(resolve_target("gone.rs", Some(cwd), None, None), None);
        std::fs::remove_dir_all(&dir).unwrap();
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
