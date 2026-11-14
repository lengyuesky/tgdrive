//! 只读预览：在用户确认前不创建目录、不移动视频、不写刮削产物。

use super::{
    backend::{self, Backend},
    data,
    types::*,
};
use crate::{
    api::AppState,
    apps::InstalledApp,
    error::{AppError, AppResult},
};
use std::collections::HashSet;
use tokio::io::AsyncReadExt as _;

async fn read(state: &AppState, file: &Snapshot, limit: usize) -> AppResult<Vec<u8>> {
    if file.is_dir || file.size <= 0 || file.size > limit as i64 {
        return Err(AppError::BadRequest(
            "既有资料超过预算或不是有效文件，请手动处理".into(),
        ));
    }
    let node = state.vfs.db.node(file.id).await?;
    if node.content_version() != file.content_version {
        return Err(AppError::Conflict("既有资料内容已改变，请重新预览".into()));
    }
    let reader = state
        .vfs
        .open_reader(&node, 0)
        .await?
        .with_read_limit(limit as u64);
    let mut bytes = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        reader.take(limit as u64 + 1).read_to_end(&mut bytes),
    )
    .await
    .map_err(|_| AppError::BadRequest("既有资料读取超时，请重试预览".into()))??;
    if bytes.len() > limit {
        return Err(AppError::BadRequest("既有资料超过读取预算".into()));
    }
    Ok(bytes)
}

pub(super) fn poster_names(base: &str) -> Vec<String> {
    crate::apps::nfo::poster_names(base)
}
fn find(files: &[Snapshot], name: &str) -> Option<Snapshot> {
    files
        .iter()
        .find(|f| f.name.eq_ignore_ascii_case(name))
        .cloned()
}
fn output(
    files: &[Snapshot],
    name: &str,
    enabled: bool,
    refresh: bool,
    public_name: Option<&str>,
) -> Artifact {
    let private = find(files, name);
    let existing = private
        .clone()
        .or_else(|| public_name.and_then(|n| find(files, n)));
    let action = if !enabled || !refresh && existing.is_some() {
        "keep"
    } else if private.is_some() {
        "replace"
    } else {
        "create"
    };
    Artifact {
        action: action.into(),
        name: if action != "create" {
            existing
                .as_ref()
                .map(|f| f.name.clone())
                .unwrap_or_else(|| name.into())
        } else {
            name.into()
        },
        existing: if action == "create" { None } else { existing },
    }
}

pub(super) async fn build(
    state: &AppState,
    app: &InstalledApp,
    batch: &Batch,
    input: &Prepare,
    backend: &Backend,
) -> AppResult<Plan> {
    if input.files.is_empty() || input.files.len() > 128 || input.shared_ids.len() > 128 {
        return Err(AppError::BadRequest(
            "请选定一组同基本名的视频及不超过 128 个配套文件".into(),
        ));
    }
    let requested = backend::valid_number(&input.number)?;
    let selected = if let Some(candidate) = &input.candidate {
        let saved: Option<String> = sqlx::query_scalar(
            "SELECT movie FROM app_scraper_sources WHERE batch_id=? AND provider=? AND source_id=?",
        )
        .bind(&batch.id)
        .bind(&candidate.provider)
        .bind(&candidate.id)
        .fetch_optional(&state.vfs.db.pool)
        .await?;
        let movie = if let Some(saved) = saved {
            data::decode(&saved)?
        } else {
            backend.movie(candidate).await?
        };
        if backend::normalized_number(&movie.number) != backend::normalized_number(&requested) {
            return Err(AppError::BadRequest(
                "选定候选的番号与本组不一致，请先修正识别番号".into(),
            ));
        }
        Some(movie)
    } else {
        None
    };
    let number = selected
        .as_ref()
        .map(|m| backend::valid_number(&m.number))
        .transpose()?
        .unwrap_or(requested);
    let mut tx = state.vfs.db.pool.begin().await?;
    data::authorize(&mut tx, &app.revision, &[&batch.root_path]).await?;
    let root = Directory {
        id: batch.root_id,
        path: batch.root_path.clone(),
    };
    if !data::node(&mut tx, root.id).await?.is_dir
        || data::path(&mut tx, root.id).await? != root.path
    {
        return Err(AppError::Conflict("刮削目录已改变，请重新选择".into()));
    }
    let mut videos = Vec::new();
    let mut ids = HashSet::new();
    for file in &input.files {
        let node = data::node(&mut tx, file.id).await?;
        let path = data::path(&mut tx, file.id).await?;
        if !is_video(&node)
            || node.content_version() != file.content_version
            || path != file.path
            || !ids.insert(file.id)
            || !crate::apps::scope::in_scope(std::slice::from_ref(&root.path), &path)
        {
            return Err(AppError::Conflict(
                "所选视频身份、位置或范围已变化，请重新扫描".into(),
            ));
        }
        videos.push(Snapshot::from_node(&node, path));
    }
    let parent = videos[0].parent_id.ok_or(AppError::NotFound)?;
    let base = stem(&videos[0].name).to_string();
    if videos
        .iter()
        .any(|f| f.parent_id != Some(parent) || !stem(&f.name).eq_ignore_ascii_case(&base))
    {
        return Err(AppError::BadRequest(
            "每次预览需为同目录、同基本名的一组视频".into(),
        ));
    }
    let folder = Directory {
        id: parent,
        path: data::path(&mut tx, parent).await?,
    };
    let siblings = data::children(&mut tx, parent).await?;
    if siblings
        .iter()
        .any(|n| is_video(n) && stem(&n.name).eq_ignore_ascii_case(&base) && !ids.contains(&n.id))
    {
        return Err(AppError::BadRequest(
            "同基本名、不同后缀的视频共用配套文件，请一起勾选".into(),
        ));
    }
    let parent_name = data::node(&mut tx, parent).await?.name;
    let aliases: Vec<_> = siblings
        .iter()
        .filter(|n| {
            n.is_dir && backend::normalized_number(&n.name) == backend::normalized_number(&number)
        })
        .collect();
    if aliases.len() > 1 {
        return Err(AppError::BadRequest(
            "同位置存在多个同番号目录，请先手动确定目标目录".into(),
        ));
    }
    let target_path =
        if backend::normalized_number(&parent_name) == backend::normalized_number(&number) {
            folder.path.clone()
        } else if let Some(dir) = aliases.first() {
            joined(&folder.path, &dir.name)
        } else {
            joined(&folder.path, &number)
        };
    data::authorize(&mut tx, &app.revision, &[&folder.path, &target_path]).await?;
    let target = if target_path == folder.path {
        Some(data::node(&mut tx, parent).await?)
    } else {
        siblings
            .iter()
            .find(|n| joined(&folder.path, &n.name) == target_path)
            .cloned()
    };
    if target.as_ref().is_some_and(|n| !n.is_dir) {
        return Err(AppError::BadRequest(
            "目标番号名称已被文件占用，请手动处理".into(),
        ));
    }
    let extra_names: Vec<String> = input
        .shared_ids
        .iter()
        .filter_map(|id| {
            siblings
                .iter()
                .find(|file| file.id == *id)
                .map(|file| file.name.clone())
        })
        .collect();
    let watched = data::watched(&mut tx, &folder, &base, &extra_names).await?;
    let mut moving = if target_path == folder.path {
        Vec::new()
    } else {
        videos.clone()
    };
    if target_path != folder.path {
        moving.extend(
            siblings
                .iter()
                .filter(|n| {
                    !n.is_dir
                        && !is_video(n)
                        && private_related(&n.name, &base)
                        && !other_video_uses(
                            &n.name,
                            &base,
                            siblings
                                .iter()
                                .filter(|v| is_video(v))
                                .map(|v| v.name.as_str()),
                        )
                })
                .map(|n| Snapshot::from_node(n, joined(&folder.path, &n.name))),
        );
        for id in &input.shared_ids {
            let file = siblings
                .iter()
                .find(|n| n.id == *id)
                .ok_or_else(|| AppError::Conflict("所选共用文件已改变，请重新预览".into()))?;
            if file.is_dir
                || ![
                    "nfo", "jpg", "jpeg", "png", "webp", "srt", "vtt", "ass", "ssa",
                ]
                .contains(&extension(&file.name).as_str())
            {
                return Err(AppError::BadRequest(
                    "同行共用文件只支持 NFO、图片或文本字幕".into(),
                ));
            }
            if !moving.iter().any(|f| f.id == *id) {
                moving.push(Snapshot::from_node(file, joined(&folder.path, &file.name)));
            }
        }
    }
    let target_watched = if let Some(dir) = &target {
        data::watched(
            &mut tx,
            &Directory {
                id: dir.id,
                path: target_path.clone(),
            },
            &base,
            &extra_names,
        )
        .await?
    } else {
        Vec::new()
    };
    if target_path != folder.path
        && moving
            .iter()
            .any(|f| target_watched.iter().any(|t| t.name == f.name))
    {
        return Err(AppError::Conflict(
            "目标已有同名视频或配套文件，本组不会移动或覆盖".into(),
        ));
    }
    let mut final_files = if target_path == folder.path {
        watched.clone()
    } else {
        target_watched.clone()
    };
    final_files.extend(
        moving
            .iter()
            .filter(|f| !f.is_dir && !is_video_snapshot(f))
            .cloned(),
    );
    let nfo_name = format!("{base}.nfo");
    let mut nfo = output(
        &final_files,
        &nfo_name,
        input.update_nfo,
        input.refresh,
        Some("movie.nfo"),
    );
    let names = poster_names(&base);
    let existing_poster = names.iter().find_map(|name| find(&final_files, name));
    let target_videos: Vec<_> = target_watched
        .iter()
        .chain(moving.iter())
        .filter(|file| !file.is_dir && video_name(&file.name))
        .map(|file| file.name.as_str())
        .collect();
    if input.refresh
        && input.update_poster
        && existing_poster.as_ref().is_some_and(|file| {
            private_related(&file.name, &base)
                && other_video_uses(&file.name, &base, target_videos.iter().copied())
        })
    {
        return Err(AppError::BadRequest(
            "优先海报同时关联另一视频，请手动确定资料归属，或先只更新 NFO".into(),
        ));
    }
    let private_jpeg = existing_poster
        .as_ref()
        .filter(|f| {
            private_related(&f.name, &base)
                && ["jpg", "jpeg"].contains(&extension(&f.name).as_str())
        })
        .cloned();
    let poster_name = private_jpeg
        .as_ref()
        .map(|f| f.name.clone())
        .unwrap_or_else(|| format!("{base}.jpg"));
    let mut poster = if !input.update_poster || !input.refresh && existing_poster.is_some() {
        Artifact {
            action: "keep".into(),
            name: existing_poster
                .as_ref()
                .map(|f| f.name.clone())
                .unwrap_or_else(|| poster_name.clone()),
            existing: existing_poster.clone(),
        }
    } else {
        Artifact {
            action: if private_jpeg.is_some() {
                "replace"
            } else {
                "create"
            }
            .into(),
            name: poster_name,
            existing: private_jpeg,
        }
    };
    for artifact in [&nfo, &poster] {
        if artifact.action != "keep"
            && other_video_uses(&artifact.name, &base, target_videos.iter().copied())
        {
            return Err(AppError::BadRequest(
                "拟写入的资料名会影响另一视频，请手动确定资料归属后重新预览".into(),
            ));
        }
    }
    // 共用文件留在原目录时，默认补缺不能静默改变移动后的视频资料。
    if target_path != folder.path
        && !input.refresh
        && (nfo.action == "create"
            && [nfo_name.as_str(), "movie.nfo"]
                .iter()
                .any(|name| find(&watched, name).is_some())
            || poster.action == "create" && names.iter().any(|n| find(&watched, n).is_some()))
    {
        return Err(AppError::BadRequest(
            "原目录有共用资料，请确认携带共用文件，或主动重新刮削生成所选视频的专属资料".into(),
        ));
    }
    tx.commit().await?;
    let mut existing_number = false;
    if !input.refresh {
        if let Some(file) = &nfo.existing {
            let bytes = read(state, file, crate::apps::nfo::MAX_BYTES).await?;
            let xml = std::str::from_utf8(&bytes).map_err(|_| {
                AppError::BadRequest("既有 NFO 不是 UTF-8，请主动重新刮削或手动处理".into())
            })?;
            if crate::apps::nfo::document(xml).is_none() {
                return Err(AppError::BadRequest(
                    "既有 NFO 无法解析，请主动重新刮削或手动处理".into(),
                ));
            }
            let supplied = crate::apps::nfo::number(xml);
            existing_number = supplied.as_ref().is_some_and(|n| {
                backend::normalized_number(n) == backend::normalized_number(&number)
            });
            if supplied.as_ref().is_some_and(|n| {
                backend::normalized_number(n) != backend::normalized_number(&number)
            }) || file.name.eq_ignore_ascii_case("movie.nfo")
                && supplied.is_none()
                && !input.shared_ids.contains(&file.id)
            {
                return Err(AppError::BadRequest(
                    "既有 NFO 归属不能确定，请明确选择共用文件或主动重新刮削".into(),
                ));
            }
        }
        if let Some(file) = &poster.existing {
            let bytes = read(state, file, backend::POSTER_LIMIT).await?;
            tokio::task::spawn_blocking(move || backend::encode_poster(&bytes, true))
                .await
                .map_err(|e| AppError::Internal(e.into()))??;
        }
    }
    let needs_candidate = (nfo.action != "keep"
        || poster.action != "keep"
        || target_path != folder.path && !existing_number)
        && input.candidate.is_none();
    let movie = if nfo.action != "keep" || poster.action != "keep" {
        if let Some(movie) = selected {
            if nfo.action != "keep" {
                movie.nfo()?;
            }
            Some(movie)
        } else {
            None
        }
    } else {
        None
    };
    // 原目录里的同行文件尚未移动，预览用原路径读取；移动事务再重写提交位置。
    for artifact in [&mut nfo, &mut poster] {
        if let Some(file) = &mut artifact.existing
            && let Some(original) = moving.iter().find(|m| m.id == file.id)
        {
            *file = original.clone();
        }
    }
    Ok(Plan {
        root,
        folder,
        target_path,
        target_id: target.map(|n| n.id),
        number,
        stem: base,
        videos,
        moving,
        watched,
        target_watched,
        extra_names,
        nfo,
        poster,
        candidate: input.candidate.clone(),
        movie,
        needs_candidate,
    })
}
fn is_video_snapshot(file: &Snapshot) -> bool {
    [
        "mp4", "mkv", "m4v", "mov", "webm", "ogv", "avi", "wmv", "flv", "ts", "mts", "m2ts", "3gp",
    ]
    .contains(&extension(&file.name).as_str())
}
