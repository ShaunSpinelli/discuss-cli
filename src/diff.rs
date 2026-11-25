use std::process::Command;

use serde::Serialize;

use crate::error::{DiscussError, Result};

pub const DEFAULT_DIFF_SIZE_LIMIT_BYTES: usize = 5 * 1024 * 1024;

#[derive(Debug)]
pub struct DiffOutput {
    pub git_args: Vec<String>,
    pub files: Vec<DiffFile>,
}

#[derive(Debug, Clone)]
pub struct DiffFile {
    pub path: String,
    pub content: String,
}

/// Structured model of a diff file's content, shipped to the browser as JSON
/// so the client can render unified and side-by-side views from one source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffData {
    pub sections: Vec<DiffSection>,
}

/// One `diff --git` block. A `discuss diff` session produces exactly one per
/// file; a `.patch` passed as a positional file may contain several.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffSection {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lang: Option<&'static str>,
    /// Stats and header-derived notes ("+3 −1", "new file", "renamed to `x`").
    pub notes: Vec<String>,
    /// New files have no old side; the browser renders them one-sided
    /// (unified) and skips context gaps, since the lone hunk is the file.
    pub new_file: bool,
    /// Deleted files have no new side, so context expansion is disabled.
    pub deleted: bool,
    /// New-side blob id from the `index <old>..<new>` header line (full ids
    /// thanks to `--full-index`). `None` when the new side is not in the
    /// object database (working-tree diffs, /dev/null).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_blob: Option<String>,
    pub hunks: Vec<Hunk>,
    /// Spec/test file. Multi-section diff views hide these behind a toggle.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub test: bool,
    /// Non-git content (plain unified diff without `diff --git` headers)
    /// renders through the legacy fenced-block path instead of `hunks`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Hunk {
    /// The full `@@ -a,b +c,d @@ ctx` line. Thread `lineRange` coordinates are
    /// hunk-local with line 1 = this header, so `1 + lines.len()` always
    /// equals the raw hunk's line count.
    pub header: String,
    pub old_start: u32,
    pub old_count: u32,
    pub new_start: u32,
    pub new_count: u32,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffLine {
    pub kind: DiffLineKind,
    /// Old-side file line number (context and del lines).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old: Option<u32>,
    /// New-side file line number (context and add lines).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new: Option<u32>,
    /// Line content with the leading `+`/`-`/` `/`\` marker column stripped.
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DiffLineKind {
    Context,
    Add,
    Del,
    /// `\ No newline at end of file` rows — no line numbers, but they occupy
    /// a hunk-local line index so lineRange coordinates stay faithful.
    Meta,
}

fn git_diff_args(unstaged: bool, extra: &[String]) -> Vec<String> {
    // --full-index puts complete blob ids on `index <old>..<new>` lines so
    // context expansion can fetch the new side via `git cat-file blob`.
    let mut git_args: Vec<String> = vec![
        "diff".into(),
        "--no-color".into(),
        "--no-ext-diff".into(),
        "--full-index".into(),
    ];
    if extra.is_empty() && !unstaged {
        git_args.push("--cached".into());
    }
    if !extra.is_empty() {
        git_args.extend(extra.iter().cloned());
    }
    git_args
}

pub fn run_git_diff(unstaged: bool, extra: &[String], limit_bytes: usize) -> Result<DiffOutput> {
    let git_args = git_diff_args(unstaged, extra);

    let output = Command::new("git")
        .args(&git_args)
        .output()
        .map_err(|source| DiscussError::DiffError {
            message: format!("failed to spawn `git`: {source}"),
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let message = if stderr.is_empty() {
            format!("git exited with status {}", output.status)
        } else {
            stderr
        };
        return Err(DiscussError::DiffError { message });
    }

    if output.stdout.len() > limit_bytes {
        return Err(DiscussError::DiffError {
            message: format!(
                "diff is {} bytes, which exceeds the {} byte limit. Narrow the range (e.g. fewer commits, a path filter, or smaller -U context), or raise the cap via --max-diff-bytes / max_diff_bytes in discuss.config.toml / DISCUSS_MAX_DIFF_BYTES.",
                output.stdout.len(),
                limit_bytes
            ),
        });
    }

    let stdout = String::from_utf8(output.stdout).map_err(|source| DiscussError::DiffError {
        message: format!("git diff produced non-UTF-8 output: {source}"),
    })?;

    Ok(DiffOutput {
        git_args,
        files: split_into_files(&stdout),
    })
}

pub fn split_into_files(unified: &str) -> Vec<DiffFile> {
    let mut starts: Vec<usize> = Vec::new();
    let mut offset = 0;
    for line in unified.split_inclusive('\n') {
        if line.starts_with("diff --git ") {
            starts.push(offset);
        }
        offset += line.len();
    }

    starts
        .iter()
        .enumerate()
        .map(|(i, &start)| {
            let end = starts.get(i + 1).copied().unwrap_or(unified.len());
            let block = &unified[start..end];
            let path = parse_path_from_block(block).unwrap_or_else(|| format!("diff-{}", i + 1));
            DiffFile {
                path,
                content: block.to_string(),
            }
        })
        .collect()
}

fn parse_path_from_block(block: &str) -> Option<String> {
    let header = block.lines().next()?;
    let after = header.strip_prefix("diff --git ")?;
    let mut tokens = after.split_whitespace();
    let _a = tokens.next()?;
    let b = tokens.next()?;
    let stripped = b.strip_prefix("b/").unwrap_or(b);
    Some(stripped.to_string())
}

/// Splits a single-file diff block into its header (everything before the
/// first `@@`) and one string per hunk (each starting at its `@@` line).
fn split_header_and_hunks(content: &str) -> (String, Vec<&str>) {
    let mut hunk_starts: Vec<usize> = Vec::new();
    let mut offset = 0;
    for line in content.split_inclusive('\n') {
        if line.starts_with("@@") {
            hunk_starts.push(offset);
        }
        offset += line.len();
    }

    let Some(&first) = hunk_starts.first() else {
        return (content.to_string(), Vec::new());
    };

    let header = content[..first].to_string();
    let hunks = hunk_starts
        .iter()
        .enumerate()
        .map(|(i, &start)| {
            let end = hunk_starts.get(i + 1).copied().unwrap_or(content.len());
            &content[start..end]
        })
        .collect();

    (header, hunks)
}

/// Parses raw diff content into the structured model the browser renders.
///
/// `content` may contain one or more `diff --git` sections. Content without
/// any git headers (e.g. a plain unified diff) becomes a single section with
/// `raw` set, which the client renders through the legacy fenced-block path.
pub fn parse_diff(fallback_path: &str, content: &str) -> DiffData {
    let files = split_into_files(content);

    if files.is_empty() {
        return DiffData {
            sections: vec![DiffSection {
                path: fallback_path.to_string(),
                lang: language_for_path(fallback_path),
                notes: Vec::new(),
                new_file: false,
                deleted: false,
                new_blob: None,
                hunks: Vec::new(),
                test: false,
                raw: Some(content.to_string()),
            }],
        };
    }

    DiffData {
        sections: files.iter().map(parse_section).collect(),
    }
}

fn parse_section(file: &DiffFile) -> DiffSection {
    let (header, raw_hunks) = split_header_and_hunks(&file.content);
    let hunks: Vec<Hunk> = raw_hunks
        .iter()
        .filter_map(|hunk| parse_hunk(hunk))
        .collect();

    let mut additions = 0usize;
    let mut deletions = 0usize;
    for hunk in &hunks {
        for line in &hunk.lines {
            match line.kind {
                DiffLineKind::Add => additions += 1,
                DiffLineKind::Del => deletions += 1,
                _ => {}
            }
        }
    }

    let mut notes: Vec<String> = Vec::new();
    if !hunks.is_empty() {
        notes.push(format!("+{additions} −{deletions}"));
    }
    let mut new_file = false;
    let mut deleted = false;
    for line in header.lines() {
        if line.starts_with("new file mode") {
            notes.push("new file".to_string());
            new_file = true;
        } else if line.starts_with("deleted file mode") {
            notes.push("deleted file".to_string());
            deleted = true;
        } else if let Some(to) = line.strip_prefix("rename to ") {
            notes.push(format!("renamed to `{to}`"));
        } else if line.starts_with("old mode") || line.starts_with("new mode") {
            notes.push(line.to_string());
        } else if line.starts_with("Binary files") {
            notes.push("binary file changed".to_string());
        }
    }

    DiffSection {
        path: file.path.clone(),
        lang: language_for_path(&file.path),
        notes,
        new_file,
        deleted,
        new_blob: if deleted {
            None
        } else {
            parse_new_blob_hash(&header)
        },
        hunks,
        test: crate::pr::is_test_path(&file.path),
        raw: None,
    }
}

fn parse_hunk(raw: &str) -> Option<Hunk> {
    let mut lines = raw.lines();
    let header = lines.next()?;
    let (old_start, old_count, new_start, new_count) = parse_hunk_header(header)?;

    let mut old = old_start;
    let mut new = new_start;
    let parsed = lines
        .map(|line| {
            let mut chars = line.chars();
            let marker = chars.next();
            let text: String = chars.collect();
            match marker {
                Some('+') => {
                    let n = new;
                    new += 1;
                    DiffLine {
                        kind: DiffLineKind::Add,
                        old: None,
                        new: Some(n),
                        text,
                    }
                }
                Some('-') => {
                    let o = old;
                    old += 1;
                    DiffLine {
                        kind: DiffLineKind::Del,
                        old: Some(o),
                        new: None,
                        text,
                    }
                }
                Some('\\') => DiffLine {
                    kind: DiffLineKind::Meta,
                    old: None,
                    new: None,
                    text,
                },
                // ' ' context lines; a fully empty line is an empty context
                // line whose marker column was trimmed away.
                _ => {
                    let (o, n) = (old, new);
                    old += 1;
                    new += 1;
                    DiffLine {
                        kind: DiffLineKind::Context,
                        old: Some(o),
                        new: Some(n),
                        text,
                    }
                }
            }
        })
        .collect();

    Some(Hunk {
        header: header.to_string(),
        old_start,
        old_count,
        new_start,
        new_count,
        lines: parsed,
    })
}

/// Parses `@@ -a[,b] +c[,d] @@ ...` into (old_start, old_count, new_start,
/// new_count). Omitted counts default to 1 per the unified diff format.
fn parse_hunk_header(line: &str) -> Option<(u32, u32, u32, u32)> {
    fn parse_span(span: &str) -> Option<(u32, u32)> {
        match span.split_once(',') {
            Some((start, count)) => Some((start.parse().ok()?, count.parse().ok()?)),
            None => Some((span.parse().ok()?, 1)),
        }
    }

    let rest = line.strip_prefix("@@ -")?;
    let (old_span, rest) = rest.split_once(" +")?;
    let (new_span, _) = rest.split_once(" @@")?;
    let (old_start, old_count) = parse_span(old_span)?;
    let (new_start, new_count) = parse_span(new_span)?;
    Some((old_start, old_count, new_start, new_count))
}

/// New-side blob id from a section header's `index <old>..<new> [<mode>]`
/// line. `None` for all-zero ids (new side not in the object database, e.g.
/// working-tree diffs) or when no index line exists.
fn parse_new_blob_hash(header: &str) -> Option<String> {
    let line = header.lines().find(|line| line.starts_with("index "))?;
    let range = line.strip_prefix("index ")?.split_whitespace().next()?;
    let (_, new) = range.split_once("..")?;
    if new.is_empty() || new.chars().all(|c| c == '0') {
        return None;
    }
    Some(new.to_string())
}

/// Full new-side file content for a diff section, used by context expansion.
///
/// The blob id from the `index` line (complete thanks to `--full-index`) is
/// tried first: for staged and range diffs it resolves via `git cat-file
/// blob` — content-addressed, so immutable for the whole session. Unstaged
/// diffs emit a *computed* new-side hash whose object is not in the object
/// database, so a cat-file failure falls back to reading the working-tree
/// file; mid-review drift is accepted there because expanded rows are
/// view-only.
pub fn read_new_side(path: &str, new_blob: Option<&str>) -> Result<String> {
    read_new_side_in(None, path, new_blob)
}

fn read_new_side_in(
    dir: Option<&std::path::Path>,
    path: &str,
    new_blob: Option<&str>,
) -> Result<String> {
    let run = |args: &[&str]| -> Result<Vec<u8>> {
        let mut cmd = Command::new("git");
        if let Some(dir) = dir {
            cmd.current_dir(dir);
        }
        let output = cmd
            .args(args)
            .output()
            .map_err(|source| DiscussError::DiffError {
                message: format!("failed to spawn `git`: {source}"),
            })?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Err(DiscussError::DiffError {
                message: if stderr.is_empty() {
                    format!("git exited with status {}", output.status)
                } else {
                    stderr
                },
            });
        }
        Ok(output.stdout)
    };

    if let Some(hash) = new_blob
        && let Ok(bytes) = run(&["cat-file", "blob", hash])
    {
        return String::from_utf8(bytes).map_err(|source| DiscussError::DiffError {
            message: format!("blob content is not UTF-8: {source}"),
        });
    }

    // git diff paths are toplevel-relative even when discuss was launched
    // from a subdirectory.
    let toplevel = run(&["rev-parse", "--show-toplevel"])?;
    let root = String::from_utf8(toplevel).map_err(|source| DiscussError::DiffError {
        message: format!("git produced non-UTF-8 output: {source}"),
    })?;
    let file_path = std::path::Path::new(root.trim()).join(path);
    std::fs::read_to_string(&file_path).map_err(|source| DiscussError::DiffError {
        message: format!("failed to read {}: {source}", file_path.display()),
    })
}

fn language_for_path(path: &str) -> Option<&'static str> {
    let extension = path.rsplit('.').next()?.to_ascii_lowercase();

    Some(match extension.as_str() {
        "rs" => "rust",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "tsx",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "jsx",
        "py" => "python",
        "rb" => "ruby",
        "go" => "go",
        "java" => "java",
        "kt" | "kts" => "kotlin",
        "swift" => "swift",
        "c" | "h" => "c",
        "cpp" | "cc" | "cxx" | "hpp" | "hh" => "cpp",
        "cs" => "csharp",
        "php" => "php",
        "md" | "markdown" => "markdown",
        "json" => "json",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        "sh" | "bash" | "zsh" => "bash",
        "html" | "htm" | "xml" | "svg" => "markup",
        "css" => "css",
        "scss" => "scss",
        "sql" => "sql",
        "lua" => "lua",
        "ex" | "exs" => "elixir",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
diff --git a/foo.rs b/foo.rs
index 1234..5678 100644
--- a/foo.rs
+++ b/foo.rs
@@ -1 +1 @@
-old
+new
diff --git a/bar.md b/bar.md
new file mode 100644
index 0000..89ab
--- /dev/null
+++ b/bar.md
@@ -0,0 +1 @@
+hello
";

    #[test]
    fn split_into_files_returns_one_per_diff_header() {
        let files = split_into_files(SAMPLE);

        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path, "foo.rs");
        assert!(files[0].content.contains("@@ -1 +1 @@"));
        assert!(files[0].content.contains("+new"));
        assert_eq!(files[1].path, "bar.md");
        assert!(files[1].content.contains("+hello"));
    }

    #[test]
    fn split_into_files_returns_empty_when_no_diff_headers() {
        assert!(split_into_files("").is_empty());
        assert!(split_into_files("not a diff\n").is_empty());
    }

    #[test]
    fn split_into_files_ignores_diff_git_text_inside_hunk_lines() {
        let sample = "\
diff --git a/doc.md b/doc.md
index 1111..2222 100644
--- a/doc.md
+++ b/doc.md
@@ -1 +1 @@
-run `diff --git a/x b/x` to inspect
+diff --git looks like a header but is hunk content
";

        // "+diff --git ..." starts with '+', not at column 0, so only the
        // real header at column 0 counts.
        let files = split_into_files(sample);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "doc.md");
    }

    #[test]
    fn parse_path_from_block_handles_renames_with_b_prefix() {
        let block =
            "diff --git a/old.md b/new/path.md\nrename from old.md\nrename to new/path.md\n";

        assert_eq!(parse_path_from_block(block).as_deref(), Some("new/path.md"));
    }

    #[test]
    fn default_diff_size_limit_is_five_megabytes() {
        assert_eq!(DEFAULT_DIFF_SIZE_LIMIT_BYTES, 5 * 1024 * 1024);
    }

    #[test]
    fn parse_diff_handles_renames_multiple_hunks_and_unknown_extensions() {
        let rename = "\
diff --git a/old.md b/new.md
similarity index 100%
rename from old.md
rename to new.md
";
        let section = parse_section(&split_into_files(rename)[0]);
        assert!(section.notes.contains(&"renamed to `new.md`".to_string()));
        assert!(section.hunks.is_empty());

        let multi = "\
diff --git a/Makefile b/Makefile
index 1111..2222 100644
--- a/Makefile
+++ b/Makefile
@@ -1,2 +1,2 @@
-first:
+first: build
 keep
@@ -10,2 +10,2 @@
 keep
-second:
+second: test
";
        let section = parse_section(&split_into_files(multi)[0]);
        assert_eq!(section.hunks.len(), 2);
        assert!(section.notes.contains(&"+2 −2".to_string()));
        // Unknown extension → no language; the client renders plain diff.
        assert_eq!(section.lang, None);
    }

    #[test]
    fn git_diff_args_includes_full_index_and_defaults_to_cached() {
        let args = git_diff_args(false, &[]);
        assert!(args.contains(&"--full-index".to_string()));
        assert!(args.contains(&"--cached".to_string()));

        let unstaged = git_diff_args(true, &[]);
        assert!(unstaged.contains(&"--full-index".to_string()));
        assert!(!unstaged.contains(&"--cached".to_string()));

        let range = git_diff_args(false, &["HEAD~2..HEAD".to_string()]);
        assert!(!range.contains(&"--cached".to_string()));
        assert!(range.contains(&"HEAD~2..HEAD".to_string()));
    }

    #[test]
    fn parse_hunk_header_handles_omitted_counts_and_context() {
        assert_eq!(parse_hunk_header("@@ -1 +1 @@"), Some((1, 1, 1, 1)));
        assert_eq!(
            parse_hunk_header("@@ -10,2 +10,3 @@ fn main()"),
            Some((10, 2, 10, 3))
        );
        assert_eq!(parse_hunk_header("@@ -0,0 +1 @@"), Some((0, 0, 1, 1)));
        assert_eq!(parse_hunk_header("not a header"), None);
    }

    #[test]
    fn parse_diff_assigns_old_and_new_line_numbers() {
        let sample = "\
diff --git a/multi.py b/multi.py
index 1111..2222 100644
--- a/multi.py
+++ b/multi.py
@@ -10,3 +10,3 @@
 keep
-second = 1
+second = 2
";

        let data = parse_diff("multi.py", sample);
        assert_eq!(data.sections.len(), 1);
        let hunk = &data.sections[0].hunks[0];
        assert_eq!(hunk.header, "@@ -10,3 +10,3 @@");
        assert_eq!((hunk.old_start, hunk.new_start), (10, 10));

        let [ctx, del, add] = &hunk.lines[..] else {
            panic!("expected 3 lines, got {:?}", hunk.lines);
        };
        assert_eq!(
            (ctx.kind, ctx.old, ctx.new, ctx.text.as_str()),
            (DiffLineKind::Context, Some(10), Some(10), "keep")
        );
        assert_eq!(
            (del.kind, del.old, del.new, del.text.as_str()),
            (DiffLineKind::Del, Some(11), None, "second = 1")
        );
        assert_eq!(
            (add.kind, add.old, add.new, add.text.as_str()),
            (DiffLineKind::Add, None, Some(11), "second = 2")
        );
    }

    #[test]
    fn parse_diff_handles_deletion_only_hunks_and_meta_rows() {
        let sample = "\
diff --git a/gone.txt b/gone.txt
index 1111..2222 100644
--- a/gone.txt
+++ b/gone.txt
@@ -5,2 +4,0 @@
-first
-second
\\ No newline at end of file
";

        let data = parse_diff("gone.txt", sample);
        let hunk = &data.sections[0].hunks[0];
        assert_eq!(hunk.lines.len(), 3);
        assert_eq!(hunk.lines[0].old, Some(5));
        assert_eq!(hunk.lines[1].old, Some(6));
        assert_eq!(hunk.lines[2].kind, DiffLineKind::Meta);
        assert_eq!(hunk.lines[2].old, None);
        assert_eq!(hunk.lines[2].new, None);
        assert_eq!(hunk.lines[2].text, " No newline at end of file");
    }

    #[test]
    fn parse_diff_preserves_hunk_local_line_count_invariant() {
        // Thread lineRange coordinates are hunk-local with line 1 = the @@
        // header, so 1 + lines.len() must equal every raw hunk's line count.
        for content in [
            SAMPLE.to_string(),
            "diff --git a/gone.txt b/gone.txt\nindex 1111..2222 100644\n--- a/gone.txt\n+++ b/gone.txt\n@@ -5,2 +4,0 @@\n-first\n-second\n\\ No newline at end of file\n".to_string(),
        ] {
            for file in split_into_files(&content) {
                let (_, raw_hunks) = split_header_and_hunks(&file.content);
                let section = parse_section(&file);
                assert_eq!(section.hunks.len(), raw_hunks.len());
                for (hunk, raw) in section.hunks.iter().zip(raw_hunks) {
                    assert_eq!(
                        1 + hunk.lines.len(),
                        raw.lines().count(),
                        "hunk-local line count mismatch for {raw:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn parse_diff_collects_notes_and_stats() {
        let files = split_into_files(SAMPLE);
        let foo = parse_section(&files[0]);
        assert_eq!(foo.notes, vec!["+1 −1".to_string()]);
        assert_eq!(foo.lang, Some("rust"));
        assert!(!foo.new_file);
        assert!(!foo.deleted);

        let bar = parse_section(&files[1]);
        assert!(bar.notes.contains(&"new file".to_string()));
        assert!(bar.new_file);

        let deleted = "\
diff --git a/old.rs b/old.rs
deleted file mode 100644
index 1234..0000
--- a/old.rs
+++ /dev/null
@@ -1 +0,0 @@
-gone
";
        let section = parse_section(&split_into_files(deleted)[0]);
        assert!(section.deleted);
        assert!(!section.new_file);
        assert!(section.notes.contains(&"deleted file".to_string()));
        assert_eq!(section.new_blob, None);

        let binary = "\
diff --git a/img.png b/img.png
index 1111..2222 100644
Binary files a/img.png and b/img.png differ
";
        let section = parse_section(&split_into_files(binary)[0]);
        assert!(section.notes.contains(&"binary file changed".to_string()));
        assert!(section.hunks.is_empty());
    }

    #[test]
    fn parse_new_blob_hash_reads_full_index_lines() {
        let header = "diff --git a/foo.rs b/foo.rs\nindex 0123456789abcdef0123456789abcdef01234567..fedcba9876543210fedcba9876543210fedcba98 100644\n--- a/foo.rs\n+++ b/foo.rs\n";
        assert_eq!(
            parse_new_blob_hash(header).as_deref(),
            Some("fedcba9876543210fedcba9876543210fedcba98")
        );

        // All-zero new side (working-tree diff) has no blob to fetch.
        let zero = "diff --git a/foo.rs b/foo.rs\nindex 0123456789abcdef0123456789abcdef01234567..0000000000000000000000000000000000000000 100644\n";
        assert_eq!(parse_new_blob_hash(zero), None);
        assert_eq!(parse_new_blob_hash("diff --git a/foo.rs b/foo.rs\n"), None);
    }

    #[test]
    fn parse_diff_wraps_non_git_content_as_raw_section() {
        let data = parse_diff("change.patch", "@@ -1 +1 @@\n-a\n+b\n");

        assert_eq!(data.sections.len(), 1);
        let section = &data.sections[0];
        assert_eq!(section.path, "change.patch");
        assert!(section.hunks.is_empty());
        assert_eq!(section.raw.as_deref(), Some("@@ -1 +1 @@\n-a\n+b\n"));
    }

    // Scripted scratch repo for read_new_side tests. Uses current_dir on
    // every git call (never the process cwd) so parallel tests stay safe.
    fn scratch_repo() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "discuss-diff-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .current_dir(&dir)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap()
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        dir
    }

    fn git_in(dir: &std::path::Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    #[test]
    fn read_new_side_reads_blob_for_staged_diff() {
        let dir = scratch_repo();
        std::fs::write(dir.join("a.txt"), "one\ntwo\n").unwrap();
        git_in(&dir, &["add", "a.txt"]);
        git_in(&dir, &["commit", "-qm", "init"]);
        std::fs::write(dir.join("a.txt"), "one\nTWO\nthree\n").unwrap();
        git_in(&dir, &["add", "a.txt"]);

        let diff = git_in(&dir, &["diff", "--cached", "--full-index"]);
        let section = parse_section(&split_into_files(&diff)[0]);
        let blob = section
            .new_blob
            .as_deref()
            .expect("staged diff has new blob");

        let content = read_new_side_in(Some(&dir), "a.txt", Some(blob)).unwrap();
        assert_eq!(content, "one\nTWO\nthree\n");

        // Blob content is a snapshot: changing the working tree afterwards
        // must not affect what expansion serves.
        std::fs::write(dir.join("a.txt"), "drifted\n").unwrap();
        let again = read_new_side_in(Some(&dir), "a.txt", Some(blob)).unwrap();
        assert_eq!(again, "one\nTWO\nthree\n");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_new_side_falls_back_to_working_tree_for_unstaged_diffs() {
        let dir = scratch_repo();
        std::fs::write(dir.join("b.txt"), "alpha\n").unwrap();
        git_in(&dir, &["add", "b.txt"]);
        git_in(&dir, &["commit", "-qm", "init"]);
        std::fs::write(dir.join("b.txt"), "alpha\nbeta\n").unwrap();

        // Unstaged diffs emit a *computed* new-side hash whose object is not
        // in the object database — cat-file fails, and read_new_side must
        // fall back to the working-tree file.
        let diff = git_in(&dir, &["diff", "--full-index"]);
        let section = parse_section(&split_into_files(&diff)[0]);
        let blob = section.new_blob.as_deref().expect("computed hash present");

        let content = read_new_side_in(Some(&dir), "b.txt", Some(blob)).unwrap();
        assert_eq!(content, "alpha\nbeta\n");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_new_side_errors_when_blob_and_file_are_both_missing() {
        let dir = scratch_repo();
        let missing = "1111111111111111111111111111111111111111";
        assert!(read_new_side_in(Some(&dir), "x.txt", Some(missing)).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn diff_data_serializes_camel_case() {
        let files = split_into_files(SAMPLE);
        let data = parse_diff("foo.rs", &files[0].content);
        let json = serde_json::to_value(&data).expect("serializes");

        let section = &json["sections"][0];
        assert_eq!(section["path"], "foo.rs");
        assert_eq!(section["lang"], "rust");
        assert_eq!(section["newFile"], false);
        assert_eq!(section["deleted"], false);
        let hunk = &section["hunks"][0];
        assert_eq!(hunk["oldStart"], 1);
        assert_eq!(hunk["newCount"], 1);
        assert_eq!(hunk["lines"][0]["kind"], "del");
        assert_eq!(hunk["lines"][1]["kind"], "add");
        assert_eq!(hunk["lines"][1]["new"], 1);
        // Absent sides are omitted, not null.
        assert!(hunk["lines"][0].get("new").is_none());
        assert!(section.get("raw").is_none());
    }
}
