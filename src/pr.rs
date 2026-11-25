//! `discuss pr <url>`: review a GitHub pull request — its description as a
//! markdown file plus one diff file per changed path. All GitHub access goes
//! through the `gh` CLI so auth is whatever the user already has set up.

use std::collections::HashMap;
use std::process::Command;
use std::sync::{Arc, Mutex};

use serde::Deserialize;

use crate::error::{DiscussError, Result};

#[derive(Debug)]
pub struct PrOutput {
    pub number: u64,
    pub url: String,
    /// `owner/name`, parsed from the PR URL.
    pub repo: String,
    pub head_sha: String,
    pub description: String,
    /// Proxy for the description's GitHub attachments, which need auth.
    pub assets: PrAssets,
    /// The whole PR diff, reviewed as one scrolling document.
    pub diff: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PrView {
    number: u64,
    title: String,
    body: String,
    url: String,
    author: PrAuthor,
    base_ref_name: String,
    head_ref_name: String,
    head_ref_oid: String,
    additions: u64,
    deletions: u64,
    changed_files: u64,
}

#[derive(Debug, Deserialize)]
struct PrAuthor {
    login: String,
}

const VIEW_FIELDS: &str = "number,title,body,url,author,baseRefName,headRefName,headRefOid,additions,deletions,changedFiles";

pub fn fetch_pr(pr: &str, limit_bytes: usize) -> Result<PrOutput> {
    let view_json = run_gh(&["pr", "view", pr, "--json", VIEW_FIELDS])?;
    let view: PrView =
        serde_json::from_slice(&view_json).map_err(|source| DiscussError::DiffError {
            message: format!("could not parse `gh pr view` output: {source}"),
        })?;

    // Diff the resolved URL, not the raw argument, so a bare PR number and
    // the description always refer to the same PR.
    let diff = run_gh(&["pr", "diff", &view.url])?;
    if diff.iter().all(u8::is_ascii_whitespace) {
        return Err(DiscussError::DiffError {
            message: "PR has no changes to review".to_string(),
        });
    }
    if diff.len() > limit_bytes {
        return Err(DiscussError::DiffError {
            message: format!(
                "PR diff is {} bytes, which exceeds the {} byte limit. Raise the cap via --max-diff-bytes / max_diff_bytes in discuss.config.toml / DISCUSS_MAX_DIFF_BYTES.",
                diff.len(),
                limit_bytes
            ),
        });
    }
    let diff = String::from_utf8(diff).map_err(|source| DiscussError::DiffError {
        message: format!("`gh pr diff` produced non-UTF-8 output: {source}"),
    })?;

    let repo = repo_from_url(&view.url).ok_or_else(|| DiscussError::DiffError {
        message: format!("could not read owner/repo from PR url {}", view.url),
    })?;

    let assets = PrAssets::new(repo.clone(), view.number);
    let extensions = assets.refresh().unwrap_or_else(|error| {
        tracing::warn!(%error, "could not read PR attachments; they will not render");
        HashMap::new()
    });
    let description = rewrite_attachments(&description_markdown(&view), &extensions);

    Ok(PrOutput {
        number: view.number,
        url: view.url.clone(),
        repo,
        head_sha: view.head_ref_oid.clone(),
        description,
        assets,
        diff,
    })
}

fn description_markdown(view: &PrView) -> String {
    let body = view.body.trim();
    let body = if body.is_empty() {
        "_No description provided._"
    } else {
        body
    };
    format!(
        "# #{number} {title}\n\n\
**@{author}** wants to merge `{head}` into `{base}` · {changed} files · +{add} −{del} · [view on GitHub]({url})\n\n\
---\n\n{body}\n",
        number = view.number,
        title = view.title,
        author = view.author.login,
        head = view.head_ref_name,
        base = view.base_ref_name,
        changed = view.changed_files,
        add = view.additions,
        del = view.deletions,
        url = view.url,
    )
}

const ATTACHMENT_PREFIX: &str = "https://github.com/user-attachments/assets/";
const SIGNED_PREFIX: &str = "https://private-user-images.githubusercontent.com/";
/// Local route that serves an attachment; see `PrAssets::get`.
pub const ASSET_ROUTE_PREFIX: &str = "/api/pr-assets/";

/// PR state the server needs during the session.
#[derive(Debug, Clone)]
pub struct PrSession {
    /// `owner/name`.
    pub repo: String,
    /// Context expansion reads files at this commit.
    pub head_sha: String,
    /// The description file; it renders with raw HTML allowed.
    pub description_file: crate::state::FileId,
    pub assets: PrAssets,
}

/// A downloaded attachment: content type and bytes.
pub type Asset = (String, Arc<Vec<u8>>);

/// GitHub attachments (`github.com/user-attachments/assets/<id>`) only load
/// for a signed-in browser. The API's rendered body carries short-lived
/// signed URLs for them, so the server fetches through those and caches the
/// bytes for the session.
#[derive(Debug, Clone)]
pub struct PrAssets {
    repo: String,
    number: u64,
    /// Asset id → current signed URL. Refreshed when a URL has expired.
    signed: Arc<Mutex<HashMap<String, String>>>,
    /// Asset id → (content type, bytes).
    cache: Arc<Mutex<HashMap<String, Asset>>>,
}

impl PrAssets {
    fn new(repo: String, number: u64) -> Self {
        Self {
            repo,
            number,
            signed: Arc::default(),
            cache: Arc::default(),
        }
    }

    /// Re-reads the signed URLs from the PR's rendered body. Returns asset
    /// id → file extension (`png`, `mp4`, …).
    fn refresh(&self) -> Result<HashMap<String, String>> {
        let endpoint = format!("repos/{}/pulls/{}", self.repo, self.number);
        let html = run_gh(&[
            "api",
            "-H",
            "Accept: application/vnd.github.html+json",
            "--jq",
            ".body_html",
            &endpoint,
        ])?;
        let signed = signed_urls(&String::from_utf8_lossy(&html));
        let extensions = signed
            .iter()
            .map(|(id, url)| (id.clone(), url_extension(url).unwrap_or_default()))
            .collect();
        if let Ok(mut current) = self.signed.lock() {
            *current = signed;
        }
        Ok(extensions)
    }

    /// Bytes for an attachment, fetched once and cached. A failed fetch
    /// refreshes the signed URLs (they expire after a few minutes) and
    /// retries once.
    pub fn get(&self, id: &str) -> Result<Asset> {
        if let Some(hit) = self.cache.lock().ok().and_then(|c| c.get(id).cloned()) {
            return Ok(hit);
        }
        let url_for = |id: &str| self.signed.lock().ok().and_then(|s| s.get(id).cloned());
        let fetched = match url_for(id).map(|url| download(&url)) {
            Some(Ok(found)) => found,
            _ => {
                self.refresh()?;
                let url = url_for(id).ok_or_else(|| DiscussError::DiffError {
                    message: format!("unknown PR attachment {id}"),
                })?;
                download(&url)?
            }
        };
        let entry = (fetched.0, Arc::new(fetched.1));
        if let Ok(mut cache) = self.cache.lock() {
            cache.insert(id.to_string(), entry.clone());
        }
        Ok(entry)
    }
}

fn download(url: &str) -> Result<(String, Vec<u8>)> {
    let fail = |message: String| DiscussError::DiffError { message };
    let response = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .and_then(|client| client.get(url).send())
        .and_then(|response| response.error_for_status())
        .map_err(|error| fail(format!("attachment download failed: {error}")))?;
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();
    let bytes = response
        .bytes()
        .map_err(|error| fail(format!("attachment download failed: {error}")))?;
    Ok((content_type, bytes.to_vec()))
}

/// Asset id → signed URL, from the API's rendered PR body. Signed file names
/// are `<n>-<asset id>.<ext>`.
fn signed_urls(html: &str) -> HashMap<String, String> {
    html.match_indices(SIGNED_PREFIX)
        .filter_map(|(start, _)| {
            let rest = &html[start..];
            let end = rest.find(['"', '\'', ' ', '>']).unwrap_or(rest.len());
            let url = rest[..end].replace("&amp;", "&");
            let path = url.split('?').next()?;
            let stem = path.rsplit('/').next()?.split('.').next()?;
            let id = stem.get(stem.len().checked_sub(36)?..)?;
            Some((id.to_string(), url))
        })
        .collect()
}

fn url_extension(url: &str) -> Option<String> {
    let path = url.split('?').next()?;
    let name = path.rsplit('/').next()?;
    let (_, ext) = name.rsplit_once('.')?;
    Some(ext.to_ascii_lowercase())
}

fn is_video_extension(ext: &str) -> bool {
    matches!(ext, "mp4" | "mov" | "webm" | "m4v")
}

/// Points attachment URLs at the local proxy route. A URL alone on its line
/// (how GitHub embeds attachments) becomes an `<img>` or `<video>` tag.
/// Unknown ids keep their GitHub URL.
fn rewrite_attachments(markdown: &str, extensions: &HashMap<String, String>) -> String {
    let local = |id: &str| -> Option<String> {
        let ext = extensions.get(id)?;
        Some(format!("{ASSET_ROUTE_PREFIX}{id}.{ext}"))
    };
    let mut out = String::with_capacity(markdown.len());
    for line in markdown.split_inclusive('\n') {
        let trimmed = line.trim();
        if let Some(id) = trimmed.strip_prefix(ATTACHMENT_PREFIX)
            && let Some(path) = local(id)
        {
            if is_video_extension(extensions.get(id).map_or("", String::as_str)) {
                out.push_str(&format!(
                    "<video src=\"{path}\" controls muted playsinline style=\"max-width:100%\"></video>\n"
                ));
            } else {
                out.push_str(&format!("![]({path})\n"));
            }
            continue;
        }
        let mut rest = line;
        while let Some(pos) = rest.find(ATTACHMENT_PREFIX) {
            out.push_str(&rest[..pos]);
            let after = &rest[pos + ATTACHMENT_PREFIX.len()..];
            let id_len = after
                .find(|c: char| !(c.is_ascii_hexdigit() || c == '-'))
                .unwrap_or(after.len());
            match local(&after[..id_len]) {
                Some(path) => out.push_str(&path),
                None => out.push_str(&rest[pos..pos + ATTACHMENT_PREFIX.len() + id_len]),
            }
            rest = &after[id_len..];
        }
        out.push_str(rest);
    }
    out
}

/// `https://github.com/<owner>/<repo>/pull/<n>` → `<owner>/<repo>`.
fn repo_from_url(url: &str) -> Option<String> {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let mut parts = rest.split('/');
    let _host = parts.next()?;
    let owner = parts.next().filter(|s| !s.is_empty())?;
    let name = parts.next().filter(|s| !s.is_empty())?;
    Some(format!("{owner}/{name}"))
}

/// Spec/test files are hidden behind a toggle in multi-file diff views.
pub fn is_test_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let mut segments: Vec<&str> = lower.split('/').collect();
    let file_name = segments.pop().unwrap_or("");

    let in_test_dir = segments.iter().any(|segment| {
        matches!(
            *segment,
            "test" | "tests" | "__tests__" | "spec" | "specs" | "__mocks__" | "cypress" | "e2e"
        )
    });

    in_test_dir
        || [".spec.", ".test.", ".cy.", "_spec.", "_test."]
            .iter()
            .any(|marker| file_name.contains(marker))
        || file_name.starts_with("test_")
}

/// Full head-side content of `path`, for context expansion. Fetched from
/// GitHub at the PR's head commit so it works without a local checkout.
pub fn read_head_file(repo: &str, head_sha: &str, path: &str) -> Result<String> {
    let endpoint = format!("repos/{repo}/contents/{path}?ref={head_sha}");
    let bytes = run_gh(&["api", "-H", "Accept: application/vnd.github.raw", &endpoint])?;
    String::from_utf8(bytes).map_err(|source| DiscussError::DiffError {
        message: format!("{path} is not UTF-8: {source}"),
    })
}

fn run_gh(args: &[&str]) -> Result<Vec<u8>> {
    let output = Command::new("gh").args(args).output().map_err(|source| {
        DiscussError::DiffError {
            message: format!(
                "failed to spawn `gh` ({source}); install the GitHub CLI and run `gh auth login`"
            ),
        }
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(DiscussError::DiffError {
            message: if stderr.is_empty() {
                format!("gh exited with status {}", output.status)
            } else {
                stderr
            },
        });
    }
    Ok(output.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_spec_and_test_files() {
        for path in [
            "apps/x/src/tests/PrimaryLanguageSeed.spec.ts",
            "apps/x/src/profile/test/profile.service.ts",
            "apps/lambdas/foo/src/index.spec.ts",
            "src/components/Button.test.tsx",
            "cypress/e2e/login.cy.ts",
            "pkg/server_test.go",
            "tests/test_api.py",
            "src/__tests__/thing.js",
        ] {
            assert!(is_test_path(path), "{path} should be a test file");
        }
    }

    #[test]
    fn keeps_non_test_files() {
        for path in [
            "apps/x/src/profile/profile.service.ts",
            "apps/x/prisma/schema.prisma",
            "src/testing-utils.ts",
            "src/latest.ts",
            "docs/contest.md",
        ] {
            assert!(!is_test_path(path), "{path} should not be a test file");
        }
    }

    const VIDEO_ID: &str = "48cb9aa7-b30b-410f-80b8-e855ad35a909";
    const IMAGE_ID: &str = "0f3679f2-5459-4566-973f-7c8d8593ef65";

    #[test]
    fn signed_urls_maps_asset_ids_and_unescapes_queries() {
        let html = format!(
            r#"<video src="{SIGNED_PREFIX}1/716489879-{VIDEO_ID}.mp4?jwt=a&amp;b=c" controls>
<img src="{SIGNED_PREFIX}1/716489901-{IMAGE_ID}.png?jwt=x" alt="x">"#
        );
        let urls = signed_urls(&html);
        assert_eq!(
            urls.get(VIDEO_ID).map(String::as_str),
            Some(format!("{SIGNED_PREFIX}1/716489879-{VIDEO_ID}.mp4?jwt=a&b=c").as_str())
        );
        assert_eq!(url_extension(&urls[IMAGE_ID]).as_deref(), Some("png"));
    }

    #[test]
    fn rewrite_attachments_points_images_and_videos_at_proxy() {
        let extensions = HashMap::from([
            (VIDEO_ID.to_string(), "mp4".to_string()),
            (IMAGE_ID.to_string(), "png".to_string()),
        ]);
        let markdown = format!(
            "Walkthrough\n\n{ATTACHMENT_PREFIX}{VIDEO_ID}\n\n![Before]({ATTACHMENT_PREFIX}{IMAGE_ID})\n\n![Gone]({ATTACHMENT_PREFIX}ffff)\n"
        );
        let out = rewrite_attachments(&markdown, &extensions);
        assert!(out.contains(&format!(
            "<video src=\"/api/pr-assets/{VIDEO_ID}.mp4\" controls"
        )));
        assert!(out.contains(&format!("![Before](/api/pr-assets/{IMAGE_ID}.png)")));
        assert!(out.contains(&format!("![Gone]({ATTACHMENT_PREFIX}ffff)")));
    }

    #[test]
    fn repo_from_url_reads_owner_and_name() {
        assert_eq!(
            repo_from_url("https://github.com/FutureFit-ai/ffai-pathways/pull/4430").as_deref(),
            Some("FutureFit-ai/ffai-pathways")
        );
        assert_eq!(repo_from_url("https://github.com/"), None);
    }

    #[test]
    fn description_includes_title_meta_and_body() {
        let view = PrView {
            number: 7,
            title: "Add thing".into(),
            body: "Does the thing.".into(),
            url: "https://github.com/o/r/pull/7".into(),
            author: PrAuthor {
                login: "dev".into(),
            },
            base_ref_name: "main".into(),
            head_ref_name: "feat/thing".into(),
            head_ref_oid: "abc".into(),
            additions: 3,
            deletions: 1,
            changed_files: 2,
        };
        let md = description_markdown(&view);
        assert!(md.starts_with("# #7 Add thing\n"));
        assert!(md.contains("`feat/thing` into `main`"));
        assert!(md.contains("Does the thing."));

        let empty = description_markdown(&PrView {
            body: "  ".into(),
            ..view
        });
        assert!(empty.contains("_No description provided._"));
    }
}
