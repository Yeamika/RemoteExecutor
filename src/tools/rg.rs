use super::soft_timeout::SoftTimeout;
use anyhow::{anyhow, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use grep_matcher::Matcher;
use grep_regex::RegexMatcherBuilder;
use grep_searcher::{sinks::UTF8, SearcherBuilder};
use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct RgExecutor {
    root: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RgOptions {
    pub pattern: String,
    #[serde(default)]
    pub root: Option<PathBuf>,
    #[serde(default)]
    pub path: Option<PathBuf>,
    #[serde(default)]
    pub globs: Vec<String>,
    #[serde(default = "default_case_sensitive")]
    pub case_sensitive: bool,
    #[serde(default)]
    pub max_count: Option<usize>,
    #[serde(default)]
    pub timeout: Option<i64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct RgOutput {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
    pub matches: usize,
    #[serde(rename = "filesWalked")]
    pub files_walked: usize,
    #[serde(rename = "timedOut")]
    pub timed_out: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct RgMatch {
    pub path: String,
    pub line_number: u64,
    pub column: usize,
    pub line: String,
    pub mod_time: u128,
}

impl RgExecutor {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &PathBuf {
        &self.root
    }

    pub async fn search(&self, mut options: RgOptions) -> Result<RgOutput> {
        if options.root.is_none() {
            options.root = Some(self.root.clone());
        }
        rg_search(options).await
    }
}

pub async fn rg_search(options: RgOptions) -> Result<RgOutput> {
    let result = rg_search_result(options).await?;
    let stdout = result
        .matches
        .iter()
        .map(|hit| {
            format!(
                "{}:{}:{}:{}",
                hit.path, hit.line_number, hit.column, hit.line
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    Ok(RgOutput {
        code: if result.matches.is_empty() { 1 } else { 0 },
        stdout: if stdout.is_empty() {
            stdout
        } else {
            format!("{stdout}\n")
        },
        stderr: String::new(),
        matches: result.matches.len(),
        files_walked: result.files_walked,
        timed_out: result.timed_out,
    })
}

pub async fn rg_matches(options: RgOptions) -> Result<Vec<RgMatch>> {
    Ok(rg_search_result(options).await?.matches)
}

async fn rg_search_result(options: RgOptions) -> Result<RgSearchResult> {
    tokio::task::spawn_blocking(move || search_sync(options)).await?
}

#[derive(Debug)]
struct RgSearchResult {
    matches: Vec<RgMatch>,
    files_walked: usize,
    timed_out: bool,
}

fn search_sync(options: RgOptions) -> Result<RgSearchResult> {
    if options.pattern.is_empty() {
        return Err(anyhow!("rg pattern must not be empty"));
    }

    let mut deadline = SoftTimeout::from_millis(options.timeout)?;
    let root = options.root.unwrap_or(std::env::current_dir()?);
    let start = path_under(
        &root,
        options.path.as_deref().unwrap_or_else(|| Path::new(".")),
    );
    let matcher = RegexMatcherBuilder::new()
        .case_insensitive(!options.case_sensitive)
        .build(&options.pattern)?;
    let globset = build_globset(&options.globs)?;
    let max_count = options.max_count.unwrap_or(usize::MAX);
    let mut matches = Vec::new();
    let mut files_walked = 0usize;

    for entry in WalkBuilder::new(&start).hidden(false).build() {
        if deadline.expired() {
            break;
        }
        let path = entry?.into_path();
        if matches.len() >= max_count {
            break;
        }
        if !path.is_file() {
            continue;
        }
        files_walked += 1;
        if !glob_matches(&root, &path, globset.as_ref()) {
            continue;
        }

        let _ = search_file(&matcher, &path, &mut matches, max_count, &mut deadline);
    }

    Ok(RgSearchResult {
        matches,
        files_walked,
        timed_out: deadline.timed_out(),
    })
}

fn search_file(
    matcher: &grep_regex::RegexMatcher,
    path: &Path,
    matches: &mut Vec<RgMatch>,
    max_count: usize,
    deadline: &mut SoftTimeout,
) -> Result<()> {
    if deadline.expired() {
        return Ok(());
    }

    let mut searcher = SearcherBuilder::new().line_number(true).build();
    let path_text = path.to_string_lossy().into_owned();
    let mod_time = mtime_ms(path);
    searcher.search_path(
        matcher,
        path,
        UTF8(|line_number, line| {
            if matches.len() >= max_count || deadline.expired() {
                return Ok(false);
            }
            let column = matcher
                .find(line.as_bytes())
                .ok()
                .flatten()
                .map(|mat| mat.start() + 1)
                .unwrap_or(1);
            matches.push(RgMatch {
                path: path_text.clone(),
                line_number,
                column,
                line: line.trim_end_matches('\n').to_string(),
                mod_time,
            });
            Ok(matches.len() < max_count && !deadline.expired())
        }),
    )?;
    Ok(())
}

fn mtime_ms(path: &Path) -> u128 {
    fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}

fn build_globset(globs: &[String]) -> Result<Option<GlobSet>> {
    if globs.is_empty() {
        return Ok(None);
    }
    let mut builder = GlobSetBuilder::new();
    for glob in globs {
        builder.add(Glob::new(glob)?);
    }
    Ok(Some(builder.build()?))
}

fn glob_matches(root: &Path, path: &Path, globset: Option<&GlobSet>) -> bool {
    let Some(globset) = globset else {
        return true;
    };
    let relative = path.strip_prefix(root).unwrap_or(path);
    globset.is_match(relative) || globset.is_match(path)
}

fn path_under(root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

fn default_case_sensitive() -> bool {
    true
}

impl RgOptions {
    pub fn new(pattern: impl Into<String>) -> Self {
        Self {
            pattern: pattern.into(),
            root: None,
            path: None,
            globs: Vec::new(),
            case_sensitive: true,
            max_count: None,
            timeout: None,
        }
    }

    pub fn root(mut self, root: impl Into<PathBuf>) -> Self {
        self.root = Some(root.into());
        self
    }

    pub fn path(mut self, path: impl Into<PathBuf>) -> Self {
        self.path = Some(path.into());
        self
    }

    pub fn glob(mut self, glob: impl Into<String>) -> Self {
        self.globs.push(glob.into());
        self
    }

    pub fn ignore_case(mut self) -> Self {
        self.case_sensitive = false;
        self
    }

    pub fn max_count(mut self, max_count: usize) -> Self {
        self.max_count = Some(max_count);
        self
    }

    pub fn timeout(mut self, timeout: i64) -> Self {
        self.timeout = Some(timeout);
        self
    }
}
