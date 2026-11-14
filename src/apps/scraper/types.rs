//! 手动刮削的预览和步骤回执；文件身份、内容及位置分别保存。

use super::backend::{Candidate, Movie};
use crate::db::Node;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct Snapshot {
    pub id: i64,
    pub name: String,
    pub path: String,
    pub parent_id: Option<i64>,
    pub content_version: String,
    pub size: i64,
    pub is_dir: bool,
}
impl Snapshot {
    pub fn from_node(node: &Node, path: String) -> Self {
        Self {
            id: node.id,
            name: node.name.clone(),
            path,
            parent_id: node.parent_id,
            content_version: node.content_version(),
            size: node.size,
            is_dir: node.is_dir,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct Directory {
    pub id: i64,
    pub path: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct Artifact {
    pub action: String,
    pub name: String,
    pub existing: Option<Snapshot>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct Plan {
    pub root: Directory,
    pub folder: Directory,
    pub target_path: String,
    pub target_id: Option<i64>,
    pub number: String,
    pub stem: String,
    pub videos: Vec<Snapshot>,
    pub moving: Vec<Snapshot>,
    pub watched: Vec<Snapshot>,
    pub target_watched: Vec<Snapshot>,
    #[serde(default)]
    pub extra_names: Vec<String>,
    pub nfo: Artifact,
    pub poster: Artifact,
    pub candidate: Option<Candidate>,
    /// 同作品资料在批次来源表中只保存一份；预览计算期间暂存。
    #[serde(skip_serializing, default)]
    pub movie: Option<Movie>,
    pub needs_candidate: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct Step {
    pub state: String,
    pub file: Option<Snapshot>,
    pub upload_id: Option<String>,
    pub sha256: Option<String>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct ResultRecord {
    pub organize: Step,
    pub nfo: Step,
    pub poster: Step,
    pub reason: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct WriteGuard {
    pub root: Directory,
    pub folder: Directory,
    pub watched: Vec<Snapshot>,
    #[serde(default)]
    pub extra_names: Vec<String>,
    pub stem: String,
    pub expected: Option<Snapshot>,
    pub name: String,
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub(super) struct Batch {
    pub id: String,
    pub revision: String,
    pub config_key: String,
    pub root_id: i64,
    pub root_path: String,
    pub owner: String,
    pub generation: String,
    pub status: String,
    pub read_only: bool,
    pub expires_at: i64,
    pub message: String,
    pub created_at: i64,
    pub updated_at: i64,
}
#[derive(Clone, Debug, sqlx::FromRow)]
pub(super) struct Item {
    pub id: String,
    pub batch_id: String,
    pub plan: String,
    pub result: String,
}

pub(super) struct UploadContent<'a> {
    pub size: i64,
    pub sha256: &'a str,
}

#[derive(sqlx::FromRow)]
pub(super) struct RecordRow {
    pub id: String,
    pub position: i64,
    pub files: String,
    pub result: String,
    pub status: String,
    pub number: Option<String>,
    pub target_path: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Reference {
    pub id: i64,
    pub content_version: String,
    pub path: String,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Handle {
    pub id: String,
    pub owner: String,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Prepare {
    pub batch: Handle,
    pub files: Vec<Reference>,
    pub number: String,
    pub candidate: Option<Candidate>,
    #[serde(default)]
    pub refresh: bool,
    #[serde(default = "yes")]
    pub update_nfo: bool,
    #[serde(default = "yes")]
    pub update_poster: bool,
    #[serde(default)]
    pub shared_ids: Vec<i64>,
}
fn yes() -> bool {
    true
}

pub(super) fn stem(name: &str) -> &str {
    name.rsplit_once('.').map(|(s, _)| s).unwrap_or(name)
}
pub(super) fn extension(name: &str) -> String {
    name.rsplit_once('.')
        .map(|(_, s)| s.to_ascii_lowercase())
        .unwrap_or_default()
}
pub(super) fn is_video(node: &Node) -> bool {
    !node.is_dir && video_name(&node.name)
}
pub(super) fn video_name(name: &str) -> bool {
    [
        "mp4", "mkv", "m4v", "mov", "webm", "ogv", "avi", "wmv", "flv", "ts", "mts", "m2ts", "3gp",
    ]
    .contains(&extension(name).as_str())
}
pub(super) fn related(name: &str, base: &str) -> bool {
    let value = stem(name).to_ascii_lowercase();
    let base = base.to_ascii_lowercase();
    private_related(name, &base)
        || common_artifact(name)
        || video_name(name)
            && (value == base
                || value == format!("{base}-poster")
                || base == format!("{value}-poster")
                || value.starts_with(&format!("{base}."))
                || base.starts_with(&format!("{value}."))
                || ["movie", "poster", "folder", "cover"].contains(&base.as_str()))
}
pub(super) fn common_artifact(name: &str) -> bool {
    [
        "movie.nfo",
        "poster.jpg",
        "poster.jpeg",
        "poster.png",
        "poster.webp",
        "folder.jpg",
        "folder.jpeg",
        "folder.png",
        "folder.webp",
        "cover.jpg",
        "cover.jpeg",
        "cover.png",
        "cover.webp",
    ]
    .iter()
    .any(|n| n.eq_ignore_ascii_case(name))
}
pub(super) fn private_related(name: &str, base: &str) -> bool {
    let value = stem(name).to_ascii_lowercase();
    let base = base.to_ascii_lowercase();
    let ext = extension(name);
    ["srt", "vtt", "ass", "ssa"].contains(&ext.as_str())
        && (value == base || value.starts_with(&format!("{base}.")))
        || ext == "nfo" && value == base
        || ["jpg", "jpeg", "png", "webp"].contains(&ext.as_str())
            && (value == base || value == format!("{base}-poster"))
}
pub(super) fn other_video_uses<'a>(
    name: &str,
    base: &str,
    mut videos: impl Iterator<Item = &'a str>,
) -> bool {
    videos.any(|video| {
        !stem(video).eq_ignore_ascii_case(base)
            && (common_artifact(name) || private_related(name, stem(video)))
    })
}
pub(super) fn joined(parent: &str, name: &str) -> String {
    format!("{}/{}", parent.trim_end_matches('/'), name)
}
