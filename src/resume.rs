use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tokio::sync::Mutex;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SceneEntry {
    pub index: usize,
    pub start_frame: u64,
    pub end_frame: u64,
}

impl SceneEntry {
    pub fn frame_count(&self) -> u64 {
        self.end_frame - self.start_frame + 1
    }

    pub fn padded_index(&self) -> String {
        format!("{:05}", self.index + 1)
    }
}

pub fn read_scenes(path: &Path) -> Result<Vec<SceneEntry>> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("read scenes.json: {}", path.display()))?;
    serde_json::from_str(&raw)
        .with_context(|| format!("parse scenes.json: {}", path.display()))
}

pub fn write_scenes(path: &Path, scenes: &[SceneEntry]) -> Result<()> {
    let json = serde_json::to_string_pretty(scenes)?;
    std::fs::write(path, json)
        .with_context(|| format!("write scenes.json: {}", path.display()))
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ChunkInfo {
    pub frames: u64,
    pub size_bytes: u64,
}

#[derive(Debug, Deserialize, Serialize, Default)]
pub struct DoneState {
    pub chunks: HashMap<String, ChunkInfo>,
}

pub struct DoneFile {
    pub path: PathBuf,
    pub state: Mutex<DoneState>,
}

impl DoneFile {
    pub fn load_or_create(path: &Path) -> Result<Self> {
        let state = if path.exists() {
            let raw = std::fs::read_to_string(path)
                .with_context(|| format!("read done.json: {}", path.display()))?;
            serde_json::from_str(&raw)
                .with_context(|| format!("parse done.json: {}", path.display()))?
        } else {
            DoneState::default()
        };
        Ok(Self { path: path.to_owned(), state: Mutex::new(state) })
    }

    pub async fn is_done(&self, chunk_key: &str, chunk_path: &Path) -> bool {
        if !self.state.lock().await.chunks.contains_key(chunk_key) {
            return false;
        }
        matches!(std::fs::metadata(chunk_path), Ok(m) if m.len() > 0)
    }

    pub async fn mark_done(&self, chunk_key: &str, frames: u64, size_bytes: u64) -> Result<()> {
        let mut state = self.state.lock().await;
        state.chunks.insert(chunk_key.to_owned(), ChunkInfo { frames, size_bytes });
        let json = serde_json::to_string_pretty(&*state)?;
        std::fs::write(&self.path, json)
            .with_context(|| format!("write done.json: {}", self.path.display()))
    }
}

pub struct TempDir {
    pub path: PathBuf,
    pub index_path: PathBuf,
    pub scenes_path: PathBuf,
    pub done_path: PathBuf,
    pub failed_path: PathBuf,
    pub chunks_dir: PathBuf,
    pub crop_cache: PathBuf,
}

impl TempDir {
    pub fn for_video(output_dir: &Path, video_stem: &str) -> Self {
        let path = output_dir.join(format!(".avxs_{video_stem}"));
        let index_path  = path.join("frame-index.ffindex");
        let scenes_path = path.join("scenes.json");
        let done_path   = path.join("done.json");
        let failed_path = path.join(".failed");
        let chunks_dir  = path.join("chunks");
        let crop_cache  = path.join("crop.cache");
        Self { path, index_path, scenes_path, done_path, failed_path, chunks_dir, crop_cache }
    }

    pub fn create_dirs(&self) -> Result<()> {
        std::fs::create_dir_all(&self.chunks_dir)
            .with_context(|| format!("create {}", self.chunks_dir.display()))
    }

    pub fn chunk_path(&self, key: &str) -> PathBuf {
        self.chunks_dir.join(format!("{key}.ivf"))
    }
}
