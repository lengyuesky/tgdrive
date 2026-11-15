//! 人物补充有整片期限；成功图片暂存在宿主，随后逐张经过资料上传保护提交。

use super::{Backend, backend, data, types::*};
use crate::{
    api::AppState,
    apps::nfo,
    error::{AppError, AppResult},
};
use futures_util::{StreamExt, stream};
use std::{collections::HashMap, time::Duration};

const IMAGE_BYTES_LIMIT: usize = 8 * 1024 * 1024;

struct Enriched {
    result: ActorResult,
    image: Option<Vec<u8>>,
}

async fn one(backend: &Backend, name: String, order: usize) -> Enriched {
    let mut result = ActorResult {
        data: nfo::Actor {
            name: name.clone(),
            order: Some(order as u32),
            ..Default::default()
        },
        state: "unmatched".into(),
        message: "没有唯一精确同名的人物资料".into(),
        image_index: None,
    };
    if name.len() > 512 {
        result.state = "failed".into();
        result.message = "姓名超过人物查询长度预算".into();
        return Enriched {
            result,
            image: None,
        };
    }
    let person = match backend.person(&name).await {
        Ok(Some(person)) => person,
        Ok(None) => {
            return Enriched {
                result,
                image: None,
            };
        }
        Err(error) => {
            result.state = "failed".into();
            result.message = error.public_parts().0;
            return Enriched {
                result,
                image: None,
            };
        }
    };
    result.data.provider = person.provider.clone();
    result.data.id = person.id.clone();
    result.data.biography = person.summary.trim().to_owned();
    match backend.person_image(&person).await {
        Ok(Some(image)) => {
            result.state = "matched".into();
            result.message.clear();
            Enriched {
                result,
                image: Some(image),
            }
        }
        Ok(None) => {
            result.state = "no_image".into();
            result.message = "人物来源未提供可用头像".into();
            Enriched {
                result,
                image: None,
            }
        }
        Err(error) => {
            result.state = "failed".into();
            result.message = error.public_parts().0;
            Enriched {
                result,
                image: None,
            }
        }
    }
}

async fn enrich(backend: &Backend, names: Vec<String>, budget: Duration) -> Vec<Enriched> {
    let mut results: Vec<_> = names
        .iter()
        .enumerate()
        .map(|(i, name)| Enriched {
            result: ActorResult {
                data: nfo::Actor {
                    name: name.clone(),
                    order: Some(i as u32),
                    ..Default::default()
                },
                state: "timeout".into(),
                message: "人物补充已达到本片查询等待上限".into(),
                image_index: None,
            },
            image: None,
        })
        .collect();
    let mut requests = stream::iter(
        names
            .into_iter()
            .enumerate()
            .map(|(i, name)| async move { (i, one(backend, name, i).await) }),
    )
    .buffer_unordered(3);
    let deadline = tokio::time::Instant::now() + budget;
    let mut image_bytes = 0usize;
    let mut biography_bytes = 0usize;
    loop {
        let next = tokio::select! {
            biased;
            _ = tokio::time::sleep_until(deadline) => break,
            result = requests.next() => result,
        };
        let Some((index, mut result)) = next else {
            break;
        };
        if result
            .image
            .as_ref()
            .is_some_and(|image| image_bytes.saturating_add(image.len()) > IMAGE_BYTES_LIMIT)
        {
            result.image = None;
            result.result.state = "failed".into();
            result.result.message = "本片头像资料超过 8 MiB 暂存预算".into();
        }
        if biography_bytes.saturating_add(result.result.data.biography.len()) > nfo::MAX_BYTES {
            result.result.data.biography.clear();
            result.result.message = "人物简介超过本片资料预算，已保留姓名和头像".into();
        }
        image_bytes += result.image.as_ref().map(Vec::len).unwrap_or(0);
        biography_bytes += result.result.data.biography.len();
        results[index] = result;
    }
    results
}

fn planned_actors(result: &ResultRecord, plan: &Plan) -> Vec<nfo::Actor> {
    result
        .actors
        .iter()
        .map(|actor| {
            let mut data = actor.data.clone();
            if let Some(index) = actor.image_index {
                data.thumb = plan.actor_images[index].name.clone();
            }
            data
        })
        .collect()
}

pub(super) async fn prepare(
    state: &AppState,
    item: &Item,
    mut plan: Plan,
    backend: &Backend,
) -> AppResult<()> {
    let candidate = plan
        .candidate
        .as_ref()
        .ok_or_else(|| AppError::BadRequest("缺少影片资料".into()))?;
    let saved: String = sqlx::query_scalar(
        "SELECT movie FROM app_scraper_sources WHERE batch_id=? AND provider=? AND source_id=?",
    )
    .bind(&item.batch_id)
    .bind(&candidate.provider)
    .bind(&candidate.id)
    .fetch_one(&state.vfs.db.pool)
    .await?;
    let movie: backend::Movie = data::decode(&saved)?;
    let enriched = enrich(backend, movie.actor_names(), Duration::from_secs(40)).await;
    let mut result: ResultRecord = data::decode(&item.result)?;
    let mut images = HashMap::new();
    result.actors.clear();
    result.actor_images.clear();
    plan.actor_images.clear();
    for mut enriched in enriched {
        if let Some(bytes) = enriched.image {
            let index = plan.actor_images.len();
            enriched.result.image_index = Some(index);
            plan.actor_images.push(Artifact {
                action: "create".into(),
                name: nfo::actor_image_name(&plan.stem, &item.id, index),
                existing: None,
            });
            result.actor_images.push(Step {
                state: "pending".into(),
                ..Default::default()
            });
            images.insert((item.id.clone(), index), bytes);
        }
        result.actors.push(enriched.result);
    }
    if movie
        .nfo_with_actors(&planned_actors(&result, &plan))
        .is_err()
    {
        for actor in &mut result.actors {
            actor.data.biography.clear();
            actor.message = "人物简介无法完整写入 NFO，已保留姓名和可用头像".into();
        }
        if movie
            .nfo_with_actors(&planned_actors(&result, &plan))
            .is_err()
        {
            images.clear();
            plan.actor_images.clear();
            result.actor_images.clear();
            for actor in &mut result.actors {
                actor.data.provider.clear();
                actor.data.id.clear();
                actor.image_index = None;
                actor.state = "failed".into();
                actor.message = "人物附加资料超过 NFO 预算，已完整保留演员姓名".into();
            }
            // 预览验证过完整影片资料；可选人物字段不能使影片失败。
            movie.nfo_with_actors(
                &result
                    .actors
                    .iter()
                    .map(|a| a.data.clone())
                    .collect::<Vec<_>>(),
            )?;
        }
    }
    result.actors_state = "done".into();
    result.summarize_actors();
    let mut tx = state.vfs.db.pool.begin_with("BEGIN IMMEDIATE").await?;
    let batch = data::active(&mut tx, &item.batch_id).await?;
    data::check_plan(&mut tx, &batch, &plan).await?;
    sqlx::query("UPDATE app_scraper_items SET plan=?,result=? WHERE id=? AND status='processing'")
        .bind(data::encode(&plan)?)
        .bind(data::encode(&result)?)
        .bind(&item.id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    state
        .vfs
        .db
        .scraper
        .actor_images
        .lock()
        .unwrap()
        .extend(images);
    Ok(())
}

pub(super) fn nfo_actors(result: &ResultRecord) -> Vec<nfo::Actor> {
    result
        .actors
        .iter()
        .map(|a| {
            let mut data = a.data.clone();
            if !a.image_index.is_some_and(|i| {
                result
                    .actor_images
                    .get(i)
                    .is_some_and(|s| s.state == "done")
            }) {
                data.thumb.clear();
            }
            data
        })
        .collect()
}

pub(super) async fn missing_image(
    state: &AppState,
    item: &Item,
    index: usize,
    message: String,
) -> AppResult<()> {
    let mut tx = state.vfs.db.pool.begin_with("BEGIN IMMEDIATE").await?;
    data::active(&mut tx, &item.batch_id).await?;
    let raw: String = sqlx::query_scalar("SELECT result FROM app_scraper_items WHERE id=?")
        .bind(&item.id)
        .fetch_one(&mut *tx)
        .await?;
    let mut result: ResultRecord = data::decode(&raw)?;
    let step = result
        .actor_images
        .get_mut(index)
        .ok_or_else(|| AppError::BadRequest("人物头像步骤已失效".into()))?;
    if step.state == "done" {
        tx.commit().await?;
        state
            .vfs
            .db
            .scraper
            .actor_images
            .lock()
            .unwrap()
            .remove(&(item.id.clone(), index));
        return Ok(());
    }
    step.state = "failed".into();
    step.file = None;
    for actor in result
        .actors
        .iter_mut()
        .filter(|a| a.image_index == Some(index))
    {
        actor.state = "failed".into();
        actor.message = message.clone();
        actor.data.thumb.clear();
    }
    result.summarize_actors();
    sqlx::query("UPDATE app_scraper_items SET result=? WHERE id=?")
        .bind(data::encode(&result)?)
        .bind(&item.id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    state
        .vfs
        .db
        .scraper
        .actor_images
        .lock()
        .unwrap()
        .remove(&(item.id.clone(), index));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn total_budget_keeps_all_names_and_bounds_concurrent_queries() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let active_requests = active.clone();
        let peak_requests = peak.clone();
        let router = axum::Router::new().route(
            "/v1/actors/search",
            axum::routing::get(move || {
                let active = active_requests.clone();
                let peak = peak_requests.clone();
                async move {
                    let count = active.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(count, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    active.fetch_sub(1, Ordering::SeqCst);
                    axum::Json(serde_json::json!({"data":[]}))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend = Backend::synthetic(&format!("http://{}/", listener.local_addr().unwrap()));
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let names: Vec<_> = (0..7).map(|i| format!("合成演员{i}")).collect();
        let started = std::time::Instant::now();
        let results = enrich(&backend, names.clone(), Duration::from_millis(80)).await;
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(
            results
                .iter()
                .map(|a| &a.result.data.name)
                .collect::<Vec<_>>(),
            names.iter().collect::<Vec<_>>()
        );
        assert!(
            results
                .iter()
                .all(|a| a.result.state == "timeout" && a.image.is_none())
        );
        assert!(peak.load(Ordering::SeqCst) > 0 && peak.load(Ordering::SeqCst) <= 3);
        server.abort();
    }
}
