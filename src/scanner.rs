use anyhow::{Context, Result};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use crate::resume::TempDir;

#[derive(Debug)]
pub struct Job {
    pub encode_toml: PathBuf,
    pub source_file: PathBuf,
}

impl Job {
    /// Always UTF-8: `find_video_files` filters non-UTF8 names before Jobs are constructed.
    pub fn stem(&self) -> &str {
        self.source_file
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("video")
    }
}

pub fn scan(input_dir: &Path, output_dir: &Path) -> Result<Vec<Job>> {
    let mut jobs = Vec::new();

    let mut profile_dirs: Vec<PathBuf> = std::fs::read_dir(input_dir)
        .with_context(|| format!("read {}", input_dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    profile_dirs.sort();

    for profile_dir in profile_dirs {
        if !profile_dir.is_dir() || profile_dir.file_name() == Some(OsStr::new("processed")) {
            continue;
        }

        let encode_toml = profile_dir.join("encode.toml");
        if !encode_toml.exists() {
            continue;
        }

        for source_file in find_video_files(&profile_dir)? {
            if output_exists(output_dir, &source_file) {
                tracing::debug!(file = %source_file.display(), "skip: output exists");
                continue;
            }
            if has_failed_marker(output_dir, &source_file) {
                let stem = source_file.file_stem().and_then(|s| s.to_str()).unwrap_or("video");
                tracing::warn!("[{stem}] permanently failed - delete .avxs_{stem}/.failed to retry");
                continue;
            }
            jobs.push(Job { encode_toml: encode_toml.clone(), source_file });
        }
    }

    Ok(jobs)
}

fn find_video_files(dir: &Path) -> Result<Vec<PathBuf>> {
    const EXTENSIONS: &[&str] = &["mkv", "mp4", "mov", "avi", "ts", "m2ts", "flv", "webm", "m4v"];

    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
        let entry = entry.context("directory entry")?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let ext = path.extension().and_then(|e| e.to_str()).map(|e| e.to_lowercase());
        if let Some(ext) = ext
            && EXTENSIONS.contains(&ext.as_str())
        {
            // Skip non-UTF8 stems: they'd collide on the fallback name and break temp-dir layout.
            if path.file_stem().and_then(|s| s.to_str()).is_none() {
                tracing::warn!("skipping file with non-UTF8 name: {}", path.display());
                continue;
            }
            files.push(path);
        }
    }
    files.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
    Ok(files)
}

fn output_exists(output_dir: &Path, source_file: &Path) -> bool {
    let stem = source_file.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    output_dir.join(format!("{stem}.mkv")).exists()
}

fn has_failed_marker(output_dir: &Path, source_file: &Path) -> bool {
    let stem = match source_file.file_stem().and_then(|s| s.to_str()) {
        Some(s) => s,
        None => return false,
    };
    TempDir::for_video(output_dir, stem).failed_path.exists()
}

pub fn ensure_processed_dir(input_dir: &Path) -> Result<PathBuf> {
    let processed = input_dir.join("processed");
    if !processed.exists() {
        std::fs::create_dir_all(&processed)
            .with_context(|| format!("create {}", processed.display()))?;
    }
    Ok(processed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn make_dirs() -> (TempDir, PathBuf, PathBuf) {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("input");
        let output = tmp.path().join("output");
        fs::create_dir_all(&input).unwrap();
        fs::create_dir_all(&output).unwrap();
        (tmp, input, output)
    }

    #[test]
    fn scan_finds_profile_with_video() {
        let (_tmp, input, output) = make_dirs();
        let profile = input.join("test-profile");
        fs::create_dir_all(&profile).unwrap();
        fs::write(profile.join("encode.toml"), b"encoder = \"svt-av1\"\n").unwrap();
        fs::write(profile.join("film.mkv"), b"fake").unwrap();

        let jobs = scan(&input, &output).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].source_file.file_name().unwrap(), "film.mkv");
    }

    #[test]
    fn scan_skips_processed_dir() {
        let (_tmp, input, output) = make_dirs();
        let processed = input.join("processed");
        fs::create_dir_all(&processed).unwrap();
        fs::write(processed.join("encode.toml"), b"encoder = \"svt-av1\"\n").unwrap();
        fs::write(processed.join("film.mkv"), b"fake").unwrap();

        let jobs = scan(&input, &output).unwrap();
        assert_eq!(jobs.len(), 0);
    }

    #[test]
    fn scan_skips_existing_output() {
        let (_tmp, input, output) = make_dirs();
        let profile = input.join("p");
        fs::create_dir_all(&profile).unwrap();
        fs::write(profile.join("encode.toml"), b"encoder = \"svt-av1\"\n").unwrap();
        fs::write(profile.join("film.mkv"), b"fake").unwrap();
        fs::write(output.join("film.mkv"), b"done").unwrap();

        let jobs = scan(&input, &output).unwrap();
        assert_eq!(jobs.len(), 0);
    }

    #[test]
    fn scan_skips_dir_without_toml() {
        let (_tmp, input, output) = make_dirs();
        let profile = input.join("no-toml");
        fs::create_dir_all(&profile).unwrap();
        fs::write(profile.join("film.mkv"), b"fake").unwrap();

        let jobs = scan(&input, &output).unwrap();
        assert_eq!(jobs.len(), 0);
    }
}
