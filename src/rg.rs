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
}

#[derive(Clone, Debug, Serialize)]
pub struct RgOutput {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
    pub matches: usize,
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
    let matches = rg_matches(options).await?;
    let stdout = matches
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
        code: if matches.is_empty() { 1 } else { 0 },
        stdout: if stdout.is_empty() {
            stdout
        } else {
            format!("{stdout}\n")
        },
        stderr: String::new(),
        matches: matches.len(),
    })
}

pub async fn rg_matches(options: RgOptions) -> Result<Vec<RgMatch>> {
    tokio::task::spawn_blocking(move || search_sync(options)).await?
}

fn search_sync(options: RgOptions) -> Result<Vec<RgMatch>> {
    if options.pattern.is_empty() {
        return Err(anyhow!("rg pattern must not be empty"));
    }

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

    for entry in walk_paths(&start) {
        let path = entry?;
        if matches.len() >= max_count {
            break;
        }
        if !path.is_file() {
            continue;
        }
        if !glob_matches(&root, &path, globset.as_ref()) {
            continue;
        }

        let _ = search_file(&matcher, &path, &mut matches, max_count);
    }

    Ok(matches)
}

fn search_file(
    matcher: &grep_regex::RegexMatcher,
    path: &Path,
    matches: &mut Vec<RgMatch>,
    max_count: usize,
) -> Result<()> {
    let mut searcher = SearcherBuilder::new().line_number(true).build();
    let path_text = path.to_string_lossy().into_owned();
    let mod_time = mtime_ms(path);
    searcher.search_path(
        matcher,
        path,
        UTF8(|line_number, line| {
            if matches.len() >= max_count {
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
            Ok(matches.len() < max_count)
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

fn walk_paths(start: &Path) -> Vec<Result<PathBuf, ignore::Error>> {
    if start.is_file() {
        return vec![Ok(start.to_path_buf())];
    }
    WalkBuilder::new(start)
        .hidden(false)
        .build()
        .map(|entry| entry.map(|entry| entry.into_path()))
        .collect::<Vec<_>>()
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
}
