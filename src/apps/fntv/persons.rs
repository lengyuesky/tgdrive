//! 人物仅从当前可读作品的 NFO 汇总；身份与作品关系、图片授权分别处理。

use super::{
    Context, authorized, guid,
    index::{Actor, Entry, Source},
    probe, protocol,
};
use crate::{
    api::AppState,
    db::Node,
    error::{AppError, AppResult},
};
use axum::{
    body::{Body, to_bytes},
    http::{Method, header},
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};

fn identity(actor: &Actor) -> String {
    let data = &actor.data;
    let provider = data.provider.trim();
    let id = data.id.trim();
    if !provider.is_empty()
        && !id.is_empty()
        && !provider.eq_ignore_ascii_case("gfriends")
        && id != data.name.trim()
    {
        guid(&["fntv-person-id-1", &provider.to_ascii_lowercase(), id])
    } else {
        guid(&["fntv-person-name-1", data.name.trim()])
    }
}

type Authorized = HashMap<(i64, String), Option<(Node, String)>>;

async fn source(
    state: &AppState,
    context: &Context,
    value: &Source,
    cache: &mut Authorized,
) -> AppResult<Option<(Node, String)>> {
    let key = (value.id, value.version.clone());
    if let Some(found) = cache.get(&key) {
        return Ok(found.clone());
    }
    let found = authorized(state, context, value.id, &value.version)
        .await
        .ok();
    context.lease.check()?;
    cache.insert(key, found.clone());
    Ok(found)
}

/// 索引不是读权限；NFO 被移动、改写、撤权后，其旧人物资料不得继续参与计数。
pub(super) async fn authorize(
    state: &AppState,
    context: &Context,
    entries: &mut [Entry],
) -> AppResult<()> {
    let mut cache = Authorized::new();
    let mut parents = HashMap::<i64, Node>::new();
    for entry in entries {
        let Some((video, _)) = source(
            state,
            context,
            &Source {
                id: entry.node_id,
                version: entry.version.clone(),
                name: entry.filename.clone(),
                size: entry.size,
            },
            &mut cache,
        )
        .await?
        else {
            entry.actors.clear();
            entry.show_actors.clear();
            continue;
        };
        let parent = video.parent_id.unwrap_or_default();
        if entry.kind == "Episode" && !parents.contains_key(&parent) {
            parents.insert(parent, state.vfs.db.node(parent).await?);
        }
        let show_parent = if entry.kind == "Episode" {
            parents.get(&parent).map(|n| {
                if guid(&["show", &parent.to_string()]) == entry.show_guid {
                    parent
                } else {
                    n.parent_id.unwrap_or(parent)
                }
            })
        } else {
            None
        };
        for (actors, is_show) in [(&mut entry.actors, false), (&mut entry.show_actors, true)] {
            let mut kept = Vec::with_capacity(actors.len());
            for mut actor in std::mem::take(actors) {
                let Some((metadata, path)) =
                    source(state, context, &actor.metadata, &mut cache).await?
                else {
                    continue;
                };
                let associated = if is_show {
                    metadata.name.eq_ignore_ascii_case("tvshow.nfo")
                        && metadata.parent_id == show_parent
                        && show_parent
                            .is_some_and(|id| guid(&["show", &id.to_string()]) == entry.show_guid)
                } else {
                    let base = video
                        .name
                        .rsplit_once('.')
                        .map(|(base, _)| base)
                        .unwrap_or(&video.name);
                    metadata.parent_id == Some(parent)
                        && (metadata.name.eq_ignore_ascii_case(&format!("{base}.nfo"))
                            || metadata
                                .name
                                .eq_ignore_ascii_case(if entry.kind == "Movie" {
                                    "movie.nfo"
                                } else {
                                    "episodedetails.nfo"
                                }))
                };
                if !associated {
                    continue;
                }
                if let Some(portrait) = &actor.portrait {
                    let actual = source(state, context, portrait, &mut cache).await?;
                    let directory = path.rsplit_once('/').map(|(p, _)| p).unwrap_or("/");
                    let expected = crate::apps::nfo::local_actor_path(directory, &actor.data.thumb);
                    if actual.is_none_or(|(node, path)| {
                        node.size <= 0
                            || node.size > 2 * 1024 * 1024
                            || expected.as_deref() != Some(path.as_str())
                    }) {
                        actor.portrait = None;
                    }
                }
                kept.push(actor);
            }
            *actors = kept;
        }
    }
    context.lease.check()
}

struct Person<'a> {
    id: String,
    records: Vec<&'a Actor>,
}
impl<'a> Person<'a> {
    fn latest(&self) -> &'a Actor {
        self.records[0]
    }
    fn biography(&self) -> &str {
        self.records
            .iter()
            .find_map(|actor| {
                (!actor.data.biography.is_empty()).then_some(actor.data.biography.as_str())
            })
            .unwrap_or("")
    }
    fn profile(&self) -> String {
        let keys: Vec<_> = self
            .records
            .iter()
            .filter_map(|a| a.portrait.as_ref())
            .map(|s| s.guid("person-image"))
            .collect();
        if keys.is_empty() {
            String::new()
        } else {
            format!(
                "/tgdrive-person/{}",
                guid(&["fntv-person-profile-1", &self.id, &keys.join(":")])
            )
        }
    }
}

fn people(entries: &[Entry]) -> BTreeMap<String, Person<'_>> {
    let mut people: BTreeMap<String, Person<'_>> = BTreeMap::new();
    let mut seen = HashSet::new();
    for actor in entries
        .iter()
        .flat_map(|e| e.actors.iter().chain(e.show_actors.iter()))
    {
        let id = identity(actor);
        if seen.insert((
            id.clone(),
            actor.metadata.id,
            actor.metadata.version.clone(),
            actor.data.name.clone(),
            guid(&[&actor.data.thumb, &actor.data.biography]),
        )) {
            people
                .entry(id.clone())
                .or_insert_with(|| Person {
                    id,
                    records: Vec::new(),
                })
                .records
                .push(actor);
        }
    }
    for person in people.values_mut() {
        person.records.sort_by(|a, b| {
            b.updated_at
                .cmp(&a.updated_at)
                .then_with(|| b.metadata.id.cmp(&a.metadata.id))
        });
    }
    people
}

fn effective(entry: &Entry) -> &[Actor] {
    if entry.actors.is_empty() {
        &entry.show_actors
    } else {
        &entry.actors
    }
}

fn item_cast<'a>(entries: &'a [Entry], reference: &str) -> Option<Vec<&'a Actor>> {
    if let Some(entry) = protocol::entry_for(entries, reference) {
        return Some(effective(entry).iter().collect());
    }
    let mut selected: Vec<_> = entries
        .iter()
        .filter(|e| {
            e.kind == "Episode"
                && (e.show_guid == reference || protocol::season_guid(e) == reference)
        })
        .collect();
    if selected.is_empty() {
        return None;
    }
    selected.sort_by(|a, b| {
        a.season
            .cmp(&b.season)
            .then(a.episode.cmp(&b.episode))
            .then(a.guid.cmp(&b.guid))
    });
    if let Some(entry) = selected.iter().find(|e| !e.show_actors.is_empty()) {
        return Some(entry.show_actors.iter().collect());
    }
    Some(
        selected
            .into_iter()
            .flat_map(|entry| effective(entry).iter())
            .collect(),
    )
}

pub(super) fn cast(entries: &[Entry], reference: &str, value: &Value) -> AppResult<Value> {
    let mut actors = item_cast(entries, reference).ok_or(AppError::NotFound)?;
    actors.sort_by_key(|a| a.data.order.unwrap_or(u32::MAX));
    let unranked = actors
        .iter()
        .filter_map(|a| a.data.order)
        .max()
        .map(|n| u64::from(n) + 1)
        .unwrap_or(0);
    let people = people(entries);
    let mut list: Vec<Value> = Vec::new();
    let mut positions = HashMap::<String, usize>::new();
    for actor in actors {
        let id = identity(actor);
        if let Some(index) = positions.get(&id).copied() {
            let row: &mut Value = &mut list[index];
            let role = actor.data.role.trim();
            let current = row["role"].as_str().unwrap_or_default();
            if !role.is_empty() && !current.split(" / ").any(|s| s == role) {
                row["role"] = json!(if current.is_empty() {
                    role.to_owned()
                } else {
                    format!("{current} / {role}")
                });
            }
            continue;
        }
        let person = &people[&id];
        let data = &person.latest().data;
        let numeric = |provider: &str| {
            if data.provider.eq_ignore_ascii_case(provider) {
                data.id.parse::<u64>().unwrap_or(0)
            } else {
                0
            }
        };
        let row = json!({"item_guid":reference,"person_guid":id,"name":actor.data.name,"original_name":data.name,
            "role":actor.data.role,"order":actor.data.order.map(u64::from).unwrap_or(unranked + list.len() as u64),
            "job":"Actor","department":"Acting","known_for_department":"Acting","profile_path":person.profile(),
            "biography":person.biography(),"also_know_as":"","trim_id":"","imdb_id":if data.provider.eq_ignore_ascii_case("imdb"){data.id.as_str()}else{""},
            "tmdb_id":numeric("tmdb"),"douban_id":numeric("douban"),"lan":"","gender":0});
        positions.insert(id, list.len());
        list.push(row);
    }
    let total = list.len();
    let page = value.get("page").and_then(Value::as_u64).unwrap_or(1);
    let size = value
        .get("page_size")
        .and_then(Value::as_u64)
        .unwrap_or(total.max(1) as u64);
    if page == 0 || page > 10000 || size == 0 || size > 100000 {
        return Err(AppError::BadRequest("演员列表分页无效".into()));
    }
    Ok(
        json!({"list":list.into_iter().skip(((page-1)*size) as usize).take(size as usize).collect::<Vec<_>>(),"total":total}),
    )
}

async fn work_rows(
    state: &AppState,
    context: &Context,
    entries: &[Entry],
    id: &str,
) -> AppResult<Vec<Value>> {
    if !people(entries).contains_key(id) {
        return Err(AppError::NotFound);
    }
    let matches = |entry: &Entry| {
        entry
            .actors
            .iter()
            .chain(entry.show_actors.iter())
            .any(|a| identity(a) == id)
    };
    let mut rows = Vec::new();
    let mut shows = HashSet::new();
    for entry in entries.iter().filter(|e| matches(e)) {
        if entry.kind == "Episode" {
            shows.insert(entry.show_guid.as_str());
        } else {
            let info = probe::cached(state, entry).await?;
            rows.push(protocol::item(state, context, entry, info.as_ref()).await?);
        }
    }
    rows.extend(
        protocol::group_items(state, context, entries, "")
            .await?
            .into_iter()
            .filter(|row| {
                row["type"] == "TV" && row["guid"].as_str().is_some_and(|id| shows.contains(id))
            }),
    );
    Ok(rows)
}

pub(super) async fn works(
    state: &AppState,
    context: &Context,
    entries: &[Entry],
    value: &Value,
) -> AppResult<Response> {
    let id = value["person_guid"].as_str().unwrap_or_default();
    let mut rows = work_rows(state, context, entries, id).await?;
    if value["job"]
        .as_str()
        .is_some_and(|job| !job.is_empty() && !job.eq_ignore_ascii_case("actor"))
    {
        rows.clear();
    }
    let mut query = value.clone();
    if !matches!(
        query["sort_column"].as_str(),
        Some("create_time" | "sort_title" | "title")
    ) {
        // 客户端使用未支持的排序名时仍采用人物页约定的最近入库顺序。
        query["sort_column"] = json!("create_time");
        query["sort_type"] = json!("DESC");
    }
    protocol::page_rows_with_limit(context, &query, rows, 512)
}

pub(super) async fn detail(
    state: &AppState,
    context: &Context,
    entries: &[Entry],
    id: &str,
) -> AppResult<Value> {
    let people = people(entries);
    let person = people.get(id).ok_or(AppError::NotFound)?;
    let mut works = work_rows(state, context, entries, id).await?;
    works.sort_by(|a, b| {
        b["create_time"]
            .as_i64()
            .cmp(&a["create_time"].as_i64())
            .then(a["title"].as_str().cmp(&b["title"].as_str()))
            .then(a["guid"].as_str().cmp(&b["guid"].as_str()))
    });
    let total = works.len();
    Ok(
        json!({"guid":id,"name":person.latest().data.name,"original_name":person.latest().data.name,
        "profile":person.profile(),"biography":person.biography(),"is_favorite":0,
        "actor_item_list":works.into_iter().take(50).collect::<Vec<_>>(),"actor_item_total":total,"director_item_list":[]}),
    )
}

pub(super) async fn image(
    state: &AppState,
    context: &Context,
    entries: &[Entry],
    reference: &str,
    method: Method,
) -> AppResult<Response> {
    let catalog = people(entries);
    let expected = format!("/tgdrive-person/{reference}");
    let person = catalog
        .values()
        .find(|p| p.profile() == expected)
        .ok_or(AppError::NotFound)?;
    for actor in &person.records {
        let Some(portrait) = &actor.portrait else {
            continue;
        };
        let (node, _) = authorized(state, context, portrait.id, &portrait.version).await?;
        let body = protocol::byte_stream(state, context, &node, 0, node.size as u64).await?;
        let read = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            to_bytes(body, 2 * 1024 * 1024),
        )
        .await;
        context.lease.check()?;
        let Ok(Ok(bytes)) = read else {
            continue;
        };
        if let Ok((mime, bytes)) = super::covers::validate_image(bytes.to_vec()).await {
            let owner = entries.iter().find(|entry| {
                entry
                    .actors
                    .iter()
                    .chain(entry.show_actors.iter())
                    .any(|record| {
                        record.metadata.id == actor.metadata.id
                            && identity(record) == person.id
                            && record.portrait.as_ref().is_some_and(|p| {
                                p.id == portrait.id && p.version == portrait.version
                            })
                    })
            });
            let Some(owner) = owner else {
                continue;
            };
            let mut current = vec![owner.clone()];
            authorize(state, context, &mut current).await?;
            if !people(&current).get(&person.id).is_some_and(|person| {
                person.records.iter().any(|record| {
                    record
                        .portrait
                        .as_ref()
                        .is_some_and(|p| p.id == portrait.id && p.version == portrait.version)
                })
            }) {
                continue;
            }
            let size = bytes.len();
            return Ok((
                [
                    (header::CONTENT_TYPE, mime),
                    (header::CONTENT_LENGTH, size.to_string()),
                ],
                if method == Method::HEAD {
                    Body::empty()
                } else {
                    Body::from(bytes)
                },
            )
                .into_response());
        }
    }
    Err(AppError::NotFound)
}
