//! 只扫描显式媒体库；索引展示信息不构成读取权限，进度按文件身份及版本保存。

use super::{Context, Library, authorized, data, guid, roots};
use crate::{
    api::AppState,
    db::{Node, apps::FileSearch},
    error::{AppError, AppResult},
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use tokio::io::AsyncReadExt;

const MAX_VIDEOS: usize = 3000;
const MAX_SCAN_FILES: usize = 20000;
const VIDEO_EXTS: &[&str] = &[
    "mp4", "mkv", "mov", "m4v", "webm", "avi", "ts", "m2ts", "mts", "flv", "wmv", "ogv", "3gp",
];

#[derive(Clone, Deserialize, Serialize)]
pub(super) struct Source {
    pub id: i64,
    pub version: String,
    pub name: String,
    pub size: i64,
}
impl Source {
    pub(super) fn from_node(n: &Node) -> Self {
        Self {
            id: n.id,
            version: n.content_version(),
            name: n.name.clone(),
            size: n.size,
        }
    }
    pub fn guid(&self, kind: &str) -> String {
        guid(&[kind, &self.id.to_string(), &self.version])
    }
}

#[derive(Clone, Deserialize, Serialize)]
pub(super) struct Actor {
    pub data: crate::apps::nfo::Actor,
    pub metadata: Source,
    pub updated_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub portrait: Option<Source>,
}

#[derive(Clone, Default, Deserialize, Serialize)]
pub(super) struct Thumbnail {
    pub target: Option<i64>,
    pub message: i64,
    pub file: String,
}

#[derive(Clone, Deserialize, Serialize)]
pub(super) struct Entry {
    pub node_id: i64,
    pub version: String,
    pub guid: String,
    pub media_guid: String,
    pub path: String,
    pub filename: String,
    pub size: i64,
    pub timestamp: i64,
    /// 加入 TGDrive 的时间，Unix 毫秒；旧索引从当前节点回填。
    #[serde(default)]
    pub added_at: i64,
    pub title: String,
    pub overview: String,
    pub year: String,
    pub kind: String,
    pub libraries: Vec<String>,
    pub season: u32,
    pub episode: u32,
    pub show_guid: String,
    pub show_title: String,
    pub metadata_version: String,
    pub poster: Option<Source>,
    #[serde(default)]
    pub posters: Vec<Source>,
    #[serde(default)]
    pub thumbnail: Option<Thumbnail>,
    #[serde(skip)]
    pub cover: Option<String>,
    pub subtitles: Vec<Source>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actors: Vec<Actor>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub show_actors: Vec<Actor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show_nfo: Option<Source>,
}

impl Entry {
    pub fn refresh_dates(&mut self, node: &Node) {
        self.added_at = node.created_at.saturating_mul(1000);
        self.timestamp = node.modified_at.saturating_mul(1000);
    }
}

pub(super) fn extension(name: &str) -> String {
    name.rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default()
}
fn stem(name: &str) -> &str {
    name.rsplit_once('.').map(|(s, _)| s).unwrap_or(name)
}

/// 只采用明确的 SxxExx 文件名，无法识别时仍保留独立视频。
pub(super) fn episode_name(name: &str) -> Option<(String, u32, u32)> {
    let bytes = name.as_bytes();
    for start in 0..bytes.len() {
        if !matches!(bytes[start], b'S' | b's')
            || (start > 0 && bytes[start - 1].is_ascii_alphanumeric())
        {
            continue;
        }
        let mut pos = start + 1;
        while pos < bytes.len() && bytes[pos].is_ascii_digit() {
            pos += 1;
        }
        if !(1..=2).contains(&(pos - start - 1))
            || !bytes.get(pos).is_some_and(|b| matches!(b, b'E' | b'e'))
        {
            continue;
        }
        let ep_start = pos + 1;
        pos = ep_start;
        while pos < bytes.len() && bytes[pos].is_ascii_digit() {
            pos += 1;
        }
        if !(1..=3).contains(&(pos - ep_start)) {
            continue;
        }
        let season = name[start + 1..ep_start - 1].parse().ok()?;
        let episode = name[ep_start..pos].parse().ok()?;
        if episode == 0 {
            continue;
        }
        let title = name[..start]
            .trim_matches(|c: char| c.is_whitespace() || matches!(c, '.' | '_' | '-'))
            .replace(['.', '_'], " ");
        return Some((title, season, episode));
    }
    None
}

fn xml_text(text: &str, tag: &str) -> Option<String> {
    crate::apps::nfo::text(text, tag)
}

async fn read_nfo(
    state: &AppState,
    context: &Context,
    source: &Source,
) -> AppResult<Option<String>> {
    if source.size > crate::apps::nfo::MAX_BYTES as i64 || source.size < 0 {
        return Ok(None);
    }
    let (node, _) = authorized(state, context, source.id, &source.version).await?;
    let reader = state
        .vfs
        .open_reader(&node, 0)
        .await?
        .with_read_limit(crate::apps::nfo::MAX_BYTES as u64);
    let mut bytes = Vec::new();
    let read = async {
        reader
            .take(crate::apps::nfo::MAX_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .await
    };
    tokio::select! {
        biased;
        _ = context.lease.cancelled.cancelled() => return Err(AppError::AppChanged),
        result = tokio::time::timeout(std::time::Duration::from_secs(5), read) => {
            if !matches!(result, Ok(Ok(_))) { return Ok(None); }
        }
    }
    if bytes.len() > crate::apps::nfo::MAX_BYTES {
        return Ok(None);
    }
    let Ok(text) = String::from_utf8(bytes) else {
        return Ok(None);
    };
    // XML 本身不执行外部实体；人物图片另行解析为受限的网盘本地引用。
    let upper = text.to_ascii_uppercase();
    if upper.contains("<!DOCTYPE") || upper.contains("<!ENTITY") {
        return Ok(None);
    }
    Ok(Some(text))
}

async fn scan_library(
    state: &AppState,
    context: &Context,
    library: &Library,
    root: &str,
) -> AppResult<Vec<(Node, String)>> {
    let mut result = Vec::new();
    let mut cursor = None;
    loop {
        context.lease.check()?;
        let query = FileSearch {
            under: root.into(),
            q: String::new(),
            kind: "file".into(),
            extensions: vec![],
            limit: 200,
            cursor,
        };
        let (rows, next) = state
            .vfs
            .db
            .app_search_page(library.root_id, root, &query)
            .await?;
        result.extend(rows);
        if result.len() > MAX_SCAN_FILES {
            return Err(AppError::BadRequest(
                "单个媒体库超过 20000 个文件的扫描预算，请选择更小目录".into(),
            ));
        }
        cursor = next;
        if cursor.is_none() {
            break;
        }
    }
    Ok(result)
}

pub(super) async fn entries(state: &AppState) -> AppResult<Vec<Entry>> {
    data::list(&state.vfs.db, "fntv:index:").await
}

pub(super) async fn refresh(
    state: &AppState,
    context: &Context,
    changed: Option<&[String]>,
) -> AppResult<()> {
    let _guard = state.vfs.db.fntv.index_lock.lock().await;
    context.lease.check()?;
    state
        .vfs
        .db
        .fntv
        .set_index_status(&context.lease, "scanning", "", 0);
    let result = refresh_inner(state, context, changed).await;
    if let Err(error) = &result {
        let (message, _) = error.public_parts();
        state
            .vfs
            .db
            .fntv
            .set_index_status(&context.lease, "failed", &message, 0);
    }
    result
}

async fn refresh_inner(
    state: &AppState,
    context: &Context,
    changed: Option<&[String]>,
) -> AppResult<()> {
    let roots = roots(state, context).await?;
    let old = entries(state).await?;
    let old_by_node: HashMap<_, _> = old.iter().map(|e| (e.node_id, e)).collect();
    let affected: HashSet<_> = roots
        .iter()
        .filter(|(_, root)| {
            changed.is_none_or(|paths| {
                paths.iter().any(|p| {
                    super::scope::in_scope(std::slice::from_ref(root), p)
                        || super::scope::in_scope(std::slice::from_ref(p), root)
                })
            })
        })
        .map(|(l, _)| l.id.clone())
        .collect();
    let live_libraries: HashSet<_> = roots.iter().map(|(l, _)| l.id.clone()).collect();
    let mut output: BTreeMap<i64, Entry> = BTreeMap::new();
    let mut staged_bytes = 0;
    for mut e in old.clone() {
        e.libraries
            .retain(|l| live_libraries.contains(l) && !affected.contains(l));
        if !e.libraries.is_empty() {
            stage_entry(&mut output, e, &mut staged_bytes)?;
        }
    }
    for (library, root) in roots.iter().filter(|(l, _)| affected.contains(&l.id)) {
        let files = scan_library(state, context, library, root).await?;
        let by_path: HashMap<_, _> = files.iter().map(|(n, p)| (p.as_str(), n)).collect();
        let mut show_metadata: HashMap<i64, Option<(Source, Vec<Actor>)>> = HashMap::new();
        let mut siblings: HashMap<i64, Vec<&Node>> = HashMap::new();
        for (n, _) in &files {
            siblings
                .entry(n.parent_id.unwrap_or(0))
                .or_default()
                .push(n);
        }
        for (node, path) in files
            .iter()
            .filter(|(n, _)| VIDEO_EXTS.contains(&extension(&n.name).as_str()))
        {
            if let Some(existing) = output.get_mut(&node.id) {
                if !existing.libraries.contains(&library.id) {
                    existing.libraries.push(library.id.clone());
                }
                continue;
            }
            if output.len() >= MAX_VIDEOS {
                return Err(AppError::BadRequest(
                    "媒体库合计超过 3000 条视频的首期索引上限".into(),
                ));
            }
            let related = siblings
                .get(&node.parent_id.unwrap_or(0))
                .cloned()
                .unwrap_or_default();
            let basename = stem(&node.name);
            let nfo = related
                .iter()
                .find(|n| n.name.eq_ignore_ascii_case(&format!("{basename}.nfo")))
                .or_else(|| {
                    related.iter().find(|n| {
                        n.name.eq_ignore_ascii_case(if library.kind == "movie" {
                            "movie.nfo"
                        } else {
                            "episodedetails.nfo"
                        })
                    })
                })
                .map(|n| Source::from_node(n));
            let posters: Vec<_> = crate::apps::nfo::poster_names(basename)
                .iter()
                .filter_map(|name| related.iter().find(|n| n.name.eq_ignore_ascii_case(name)))
                .filter(|n| n.size > 0 && n.size <= 2 * 1024 * 1024)
                .map(|n| Source::from_node(n))
                .collect();
            let poster = posters.first().cloned();
            let thumbnail = state.vfs.db.first_chunk(node.id).await?.and_then(|c| {
                c.tg_thumbnail_file_id.map(|file| Thumbnail {
                    target: c.storage_target_id,
                    message: c.tg_message_id,
                    file,
                })
            });
            let subtitles: Vec<_> = related
                .iter()
                .filter(|n| {
                    ["srt", "vtt", "ass", "ssa"].contains(&extension(&n.name).as_str())
                        && (stem(&n.name) == basename
                            || stem(&n.name).starts_with(&format!("{basename}.")))
                        && n.size >= 0
                        && n.size <= 512 * 1024
                })
                .take(32)
                .map(|n| Source::from_node(n))
                .collect();
            let metadata_version = guid(&[
                &library.kind,
                path,
                &node.name,
                &node.parent_id.unwrap_or(0).to_string(),
                &nfo.as_ref().map(|s| s.version.clone()).unwrap_or_default(),
                &posters
                    .iter()
                    .map(|s| s.guid("poster"))
                    .collect::<Vec<_>>()
                    .join(":"),
                &thumbnail
                    .as_ref()
                    .map(|t| {
                        super::guid(&[
                            &t.target.unwrap_or_default().to_string(),
                            &t.message.to_string(),
                            &t.file,
                        ])
                    })
                    .unwrap_or_default(),
                &subtitles
                    .iter()
                    .map(|s| s.version.as_str())
                    .collect::<Vec<_>>()
                    .join(":"),
            ]);
            let version = node.content_version();
            if let Some(old) = old_by_node.get(&node.id).filter(|e| {
                e.version == version
                    && e.metadata_version == metadata_version
                    && e.libraries.iter().any(|id| id == &library.id)
            }) {
                let mut entry = (*old).clone();
                entry.path = path.clone();
                entry.libraries = vec![library.id.clone()];
                entry.refresh_dates(node);
                refresh_actor_sources(&mut entry.actors, &files);
                refresh_actor_sources(&mut entry.show_actors, &files);
                if let Some(source) = &entry.show_nfo {
                    let current = files.iter().find(|(n, _)| n.id == source.id);
                    if current.is_none_or(|(n, _)| n.content_version() != source.version) {
                        // 剧级 NFO 的变化属于现有文件刷新，不是旧索引迁移。
                        entry.show_nfo = None;
                        entry.show_actors.clear();
                    }
                }
                if library.kind == "tv" && entry.kind == "Episode" {
                    fill_show_actors(
                        state,
                        context,
                        &mut entry,
                        &files,
                        &by_path,
                        &mut show_metadata,
                    )
                    .await?;
                }
                stage_entry(&mut output, entry, &mut staged_bytes)?;
                continue;
            }
            let mut entry = Entry {
                node_id: node.id,
                version: version.clone(),
                guid: guid(&["item", &node.id.to_string(), &version]),
                media_guid: guid(&["media", &node.id.to_string(), &version]),
                path: path.clone(),
                filename: node.name.clone(),
                size: node.size,
                timestamp: node.modified_at.saturating_mul(1000),
                added_at: node.created_at.saturating_mul(1000),
                title: basename.replace(['.', '_'], " "),
                overview: String::new(),
                year: String::new(),
                kind: if library.kind == "movie" {
                    "Movie"
                } else {
                    "Video"
                }
                .into(),
                libraries: vec![library.id.clone()],
                season: 0,
                episode: 0,
                show_guid: String::new(),
                show_title: String::new(),
                metadata_version,
                poster,
                posters,
                thumbnail,
                cover: None,
                subtitles,
                actors: Vec::new(),
                show_actors: Vec::new(),
                show_nfo: None,
            };
            let mut recognized = if library.kind == "tv" {
                episode_name(&node.name)
            } else {
                None
            };
            if let Some(source) = nfo
                && let Some(text) = read_nfo(state, context, &source).await?
            {
                entry.actors = actor_rows(&text, &source, &files, &by_path);
                if let Some(title) = xml_text(&text, "title").filter(|s| !s.is_empty()) {
                    entry.title = title;
                }
                if let Some(overview) = xml_text(&text, "plot") {
                    entry.overview = overview;
                }
                if let Some(year) = xml_text(&text, "year")
                    .filter(|y| y.len() == 4 && y.bytes().all(|b| b.is_ascii_digit()))
                {
                    entry.year = year;
                }
                if library.kind == "tv" {
                    let season = xml_text(&text, "season").and_then(|n| n.parse::<u32>().ok());
                    let episode = xml_text(&text, "episode")
                        .and_then(|n| n.parse::<u32>().ok())
                        .filter(|n| *n > 0);
                    if let (Some(s), Some(e)) = (season, episode) {
                        recognized = Some((xml_text(&text, "showtitle").unwrap_or_default(), s, e));
                    }
                }
            }
            if let Some((title, season, episode)) = recognized {
                let parent = state.vfs.db.node(node.parent_id.unwrap()).await?;
                let parent_name = parent.name.to_ascii_lowercase();
                let season_folder = parent_name
                    .strip_prefix("season")
                    .or_else(|| parent_name.strip_prefix('s'))
                    .map(str::trim)
                    .is_some_and(|suffix| {
                        !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit())
                    });
                let group_id = if season_folder {
                    parent.parent_id.unwrap_or(parent.id)
                } else {
                    parent.id
                };
                let group = state.vfs.db.node(group_id).await?;
                entry.kind = "Episode".into();
                entry.season = season;
                entry.episode = episode;
                entry.show_guid = guid(&["show", &group_id.to_string()]);
                entry.show_title = if title.is_empty() { group.name } else { title };
                fill_show_actors(
                    state,
                    context,
                    &mut entry,
                    &files,
                    &by_path,
                    &mut show_metadata,
                )
                .await?;
            }
            stage_entry(&mut output, entry, &mut staged_bytes)?;
        }
    }
    let rows: Vec<_> = output
        .values()
        .cloned()
        .map(|e| (format!("fntv:index:{}", e.node_id), e))
        .collect();
    data::publish_index(&state.vfs.db, context, &rows).await?;
    state
        .vfs
        .db
        .fntv
        .set_index_status(&context.lease, "ready", "", rows.len());
    crate::events::emit(
        &state.events,
        crate::events::HostEvent::app("app.storage", super::APP_ID, Some("fntv:index".into())),
    );
    super::covers::start_worker(state);
    state.vfs.db.fntv.cover_wake.notify_one();
    Ok(())
}

fn actor_rows(
    text: &str,
    source: &Source,
    files: &[(Node, String)],
    by_path: &HashMap<&str, &Node>,
) -> Vec<Actor> {
    let Some((metadata, path)) = files.iter().find(|(n, _)| n.id == source.id) else {
        return Vec::new();
    };
    let directory = path.rsplit_once('/').map(|(p, _)| p).unwrap_or("/");
    crate::apps::nfo::actors(text)
        .into_iter()
        .map(|data| {
            let portrait = crate::apps::nfo::local_actor_path(directory, &data.thumb)
                .and_then(|path| by_path.get(path.as_str()).copied())
                .filter(|n| {
                    !n.is_dir
                        && n.size > 0
                        && n.size <= 2 * 1024 * 1024
                        && ["jpg", "jpeg", "png", "webp"].contains(&extension(&n.name).as_str())
                })
                .map(Source::from_node);
            Actor {
                data,
                metadata: source.clone(),
                updated_at: metadata.modified_at.saturating_mul(1000),
                portrait,
            }
        })
        .collect()
}

fn refresh_actor_sources(actors: &mut [Actor], files: &[(Node, String)]) {
    for actor in actors {
        let directory = files
            .iter()
            .find(|(n, _)| n.id == actor.metadata.id)
            .and_then(|(_, path)| path.rsplit_once('/').map(|(dir, _)| dir));
        actor.portrait = directory
            .and_then(|dir| crate::apps::nfo::local_actor_path(dir, &actor.data.thumb))
            .and_then(|path| files.iter().find(|(_, p)| p == &path).map(|(n, _)| n))
            .filter(|node| {
                node.size > 0
                    && node.size <= 2 * 1024 * 1024
                    && ["jpg", "jpeg", "png", "webp"].contains(&extension(&node.name).as_str())
            })
            .map(Source::from_node);
    }
}

async fn fill_show_actors(
    state: &AppState,
    context: &Context,
    entry: &mut Entry,
    files: &[(Node, String)],
    by_path: &HashMap<&str, &Node>,
    cache: &mut HashMap<i64, Option<(Source, Vec<Actor>)>>,
) -> AppResult<()> {
    let parent = state
        .vfs
        .db
        .node(entry.node_id)
        .await?
        .parent_id
        .ok_or(AppError::NotFound)?;
    let mut group = parent;
    let parent_node = state.vfs.db.node(parent).await?;
    if guid(&["show", &parent.to_string()]) != entry.show_guid {
        group = parent_node.parent_id.unwrap_or(parent);
    }
    if let std::collections::hash_map::Entry::Vacant(slot) = cache.entry(group) {
        let nfo = files
            .iter()
            .find(|(n, _)| n.parent_id == Some(group) && n.name.eq_ignore_ascii_case("tvshow.nfo"));
        let value = if let Some((node, _)) = nfo {
            let source = Source::from_node(node);
            let actors = if let Some(text) = read_nfo(state, context, &source).await? {
                actor_rows(&text, &source, files, by_path)
            } else {
                Vec::new()
            };
            Some((source, actors))
        } else {
            None
        };
        slot.insert(value);
    }
    if let Some(Some((source, actors))) = cache.get(&group) {
        entry.show_nfo = Some(source.clone());
        entry.show_actors = actors.clone();
    } else {
        entry.show_nfo = None;
        entry.show_actors.clear();
    }
    Ok(())
}

/// 在持久化配额之前限制暂存内存，长 NFO 不会随数千个影片积累到无界大小。
fn stage_entry(
    output: &mut BTreeMap<i64, Entry>,
    entry: Entry,
    bytes: &mut usize,
) -> AppResult<()> {
    let size = serde_json::to_vec(&entry)
        .map_err(anyhow::Error::from)?
        .len();
    if size > data::INDEX_VALUE_LIMIT || bytes.saturating_add(size) > 8 * 1024 * 1024 {
        return Err(AppError::BadRequest(
            "影视索引资料超过单项或 8 MiB 总预算，请缩小媒体库".into(),
        ));
    }
    *bytes += size;
    output.insert(entry.node_id, entry);
    Ok(())
}

pub(super) fn start_worker(state: &AppState) {
    let lease = state.vfs.db.fntv.lease();
    {
        let mut inner = state.vfs.db.fntv.lock();
        if inner.worker_epoch == lease.epoch {
            return;
        }
        inner.worker_epoch = lease.epoch;
    }
    let state = state.clone();
    tokio::spawn(async move {
        let mut events = state.events.subscribe();
        let Ok(context) = super::context(&state, true).await else {
            return;
        };
        if context.lease.epoch != lease.epoch {
            return;
        }
        let (mut cursor, _) = state.events.file_changes(None);
        if refresh(&state, &context, None).await.is_err() && lease.cancelled.is_cancelled() {
            return;
        }
        loop {
            tokio::select! {
                biased;
                _=lease.cancelled.cancelled()=>break,
                _=state.restore.shutdown.cancelled()=>break,
                event=events.recv()=> {
                    if matches!(event,Ok(ref e) if e.kind!="files") { continue; }
                    let (next,paths)=state.events.file_changes(Some(&cursor)); cursor=next;
                    if paths.as_ref().is_some_and(Vec::is_empty) { continue; }
                    let Ok(context)=super::context(&state,true).await else { break; };
                    let _=refresh(&state,&context,paths.as_deref()).await;
                }
            }
        }
    });
}
