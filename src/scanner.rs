use anyhow::{Context, Result};
use std::collections::HashMap;
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
            if let Some(marker) = failed_marker(output_dir, &source_file) {
                let stem = source_file.file_stem().and_then(|s| s.to_str()).unwrap_or("video");
                tracing::warn!("[{stem}] permanently failed - delete {} to retry", marker.display());
                continue;
            }
            jobs.push(Job { encode_toml: encode_toml.clone(), source_file });
        }
    }

    Ok(drop_stem_collisions(jobs))
}

/// The stem names output, temp dir and archive, so two files sharing one both stop.
fn drop_stem_collisions(jobs: Vec<Job>) -> Vec<Job> {
    let mut seen: HashMap<&str, usize> = HashMap::new();
    for job in &jobs {
        *seen.entry(job.stem()).or_insert(0) += 1;
    }

    let colliding: Vec<&str> = seen
        .iter()
        .filter(|&(_, &n)| n > 1)
        .map(|(&stem, _)| stem)
        .collect();

    for stem in &colliding {
        let paths: Vec<String> = jobs
            .iter()
            .filter(|j| j.stem() == *stem)
            .map(|j| j.source_file.display().to_string())
            .collect();
        tracing::error!(
            "[{stem}] skipping {} files that share this name - they would overwrite each \
             other's output: {}",
            paths.len(),
            paths.join(", ")
        );
    }

    if colliding.is_empty() {
        return jobs;
    }
    let colliding: Vec<String> = colliding.into_iter().map(str::to_owned).collect();
    jobs.into_iter()
        .filter(|j| !colliding.iter().any(|s| s == j.stem()))
        .collect()
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
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                tracing::warn!("skipping file with non-UTF8 name: {}", path.display());
                continue;
            };
            // The stem reaches ffmpeg's concat list, whose parser cannot escape one.
            if stem.contains(['\n', '\r']) {
                tracing::warn!("skipping file with a line break in its name: {}", path.display());
                continue;
            }
            files.push(path);
        }
    }
    files.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
    Ok(files)
}

/// avxs never produces an empty output, so one is a leftover, not "already done".
fn output_exists(output_dir: &Path, source_file: &Path) -> bool {
    let stem = source_file.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    let path = output_dir.join(format!("{stem}.mkv"));
    match std::fs::metadata(&path) {
        Ok(m) if m.len() > 0 => true,
        Ok(_) => {
            tracing::warn!("[{stem}] ignoring empty output file {}", path.display());
            false
        }
        Err(_) => false,
    }
}

/// The marker path when this exact source is locked out; a stem twin does not block.
fn failed_marker(output_dir: &Path, source_file: &Path) -> Option<PathBuf> {
    let stem = source_file.file_stem().and_then(|s| s.to_str())?;
    let temp = TempDir::for_video(output_dir, stem);
    if !temp.failed_path.exists() {
        return None;
    }
    match temp.recorded_source() {
        Some(prev) if prev != source_file.display().to_string() => None,
        _ => Some(temp.failed_path),
    }
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

    #[test]
    fn scan_drops_files_that_share_a_stem() {
        let (_tmp, input, output) = make_dirs();
        for p in ["a", "b"] {
            let profile = input.join(p);
            fs::create_dir_all(&profile).unwrap();
            fs::write(profile.join("encode.toml"), b"encoder = \"svt-av1\"\n").unwrap();
            fs::write(profile.join("film.mkv"), b"fake").unwrap();
        }
        assert_eq!(scan(&input, &output).unwrap().len(), 0);
    }

    #[test]
    fn scan_drops_same_stem_with_different_extensions() {
        let (_tmp, input, output) = make_dirs();
        let profile = input.join("p");
        fs::create_dir_all(&profile).unwrap();
        fs::write(profile.join("encode.toml"), b"encoder = \"svt-av1\"\n").unwrap();
        fs::write(profile.join("film.mkv"), b"fake").unwrap();
        fs::write(profile.join("film.mp4"), b"fake").unwrap();
        assert_eq!(scan(&input, &output).unwrap().len(), 0);
    }

    #[test]
    fn scan_keeps_distinct_stems_next_to_a_collision() {
        let (_tmp, input, output) = make_dirs();
        for p in ["a", "b"] {
            let profile = input.join(p);
            fs::create_dir_all(&profile).unwrap();
            fs::write(profile.join("encode.toml"), b"encoder = \"svt-av1\"\n").unwrap();
            fs::write(profile.join("film.mkv"), b"fake").unwrap();
        }
        fs::write(input.join("a").join("other.mkv"), b"fake").unwrap();
        let jobs = scan(&input, &output).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].stem(), "other");
    }

    #[test]
    fn failed_marker_only_blocks_the_file_it_was_written_for() {
        let (_tmp, input, output) = make_dirs();
        let profile = input.join("p");
        fs::create_dir_all(&profile).unwrap();
        fs::write(profile.join("encode.toml"), b"encoder = \"svt-av1\"\n").unwrap();
        fs::write(profile.join("film.mkv"), b"fake").unwrap();

        let temp = crate::resume::TempDir::for_video(&output, "film");
        temp.create_dirs().unwrap();
        fs::write(&temp.failed_path, b"boom").unwrap();

        // Marker written for this exact file: blocked.
        fs::write(&temp.source_id_path, profile.join("film.mkv").display().to_string()).unwrap();
        assert_eq!(scan(&input, &output).unwrap().len(), 0);

        // Marker left over from a different file that had the same name: not blocked.
        fs::write(&temp.source_id_path, "/somewhere/else/film.mkv").unwrap();
        assert_eq!(scan(&input, &output).unwrap().len(), 1);
    }
}
