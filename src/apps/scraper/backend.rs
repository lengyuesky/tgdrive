//! MetaTube 固定接口适配；不使用详情里的任意上游 URL，也不回显 Token。

use crate::{
    api::AppState,
    error::{AppError, AppResult},
};
use base64::Engine as _;
use futures_util::StreamExt as _;
use image::ImageEncoder as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeMap, io::Cursor, time::Duration};

pub(super) const DEFAULT_URL: &str = "https://metatube-server.hf.space/";
pub(super) const POSTER_LIMIT: usize = 2 * 1024 * 1024;
pub(super) const ACTOR_IMAGE_LIMIT: usize = 128 * 1024;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct Candidate {
    pub provider: String,
    pub id: String,
    pub number: String,
    pub title: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct Movie {
    pub provider: String,
    pub id: String,
    pub number: String,
    pub title: String,
    pub summary: Option<String>,
    pub actors: Option<Vec<String>>,
    pub director: Option<String>,
    pub maker: Option<String>,
    pub label: Option<String>,
    pub series: Option<String>,
    pub genres: Option<Vec<String>>,
    pub score: Option<f64>,
    pub release_date: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub(super) struct Person {
    pub name: String,
    pub provider: String,
    pub id: String,
    pub summary: String,
}

pub(super) fn exact_person(candidates: Vec<Person>, name: &str) -> Option<Person> {
    let mut exact: BTreeMap<(String, String), Person> = BTreeMap::new();
    for person in candidates {
        if person.name.trim() == name.trim()
            && identity(&Candidate {
                provider: person.provider.clone(),
                id: person.id.clone(),
                ..Default::default()
            })
            .is_ok()
        {
            exact
                .entry((person.provider.clone(), person.id.clone()))
                .or_insert(person);
        }
    }
    (exact.len() == 1).then(|| exact.into_values().next().unwrap())
}

pub(super) fn base_url(raw: &str) -> AppResult<reqwest::Url> {
    let mut url = reqwest::Url::parse(raw.trim())
        .map_err(|_| AppError::BadRequest("MetaTube 地址格式无效".into()))?;
    if raw.len() > 2048
        || !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(AppError::BadRequest(
            "MetaTube 地址需为不含账号、查询参数或片段的 HTTP/HTTPS 地址".into(),
        ));
    }
    if !url.path().ends_with('/') {
        url.set_path(&format!("{}/", url.path()));
    }
    Ok(url)
}

pub(super) fn normalized_number(value: &str) -> String {
    value
        .bytes()
        .filter(u8::is_ascii_alphanumeric)
        .map(|b| char::from(b.to_ascii_uppercase()))
        .collect()
}

pub(super) fn valid_number(value: &str) -> AppResult<String> {
    let value = value.trim().to_ascii_uppercase();
    if value.is_empty()
        || value.len() > 100
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
        || !value.bytes().any(|b| b.is_ascii_digit())
        || value.starts_with(['-', '_'])
        || value.ends_with(['-', '_'])
    {
        return Err(AppError::BadRequest(
            "请填写只含英文字母、数字、短横线或下划线的作品番号".into(),
        ));
    }
    Ok(value)
}

pub(super) struct Backend {
    client: reqwest::Client,
    base: reqwest::Url,
    token: Option<String>,
}

impl Backend {
    #[cfg(test)]
    pub(super) fn synthetic(url: &str) -> Self {
        Self {
            client: reqwest::Client::builder().no_proxy().build().unwrap(),
            base: base_url(url).unwrap(),
            token: None,
        }
    }
    pub fn new(state: &AppState, url: &str, token: Option<String>) -> AppResult<Self> {
        let client = state
            .rt
            .current()
            .proxy
            .http_builder()?
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(40))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| AppError::ScraperBackend("无法创建后端连接".into()))?;
        Ok(Self {
            client,
            base: base_url(url)?,
            token,
        })
    }

    async fn get(
        &self,
        parts: &[&str],
        query: &[(&str, &str)],
        limit: usize,
    ) -> AppResult<Option<Vec<u8>>> {
        let mut url = self.base.clone();
        {
            let mut path = url
                .path_segments_mut()
                .map_err(|_| AppError::BadRequest("后端地址无效".into()))?;
            path.pop_if_empty();
            for part in parts {
                path.push(part);
            }
        }
        let mut request = self.client.get(url).query(query);
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        let response = request
            .send()
            .await
            .map_err(|_| AppError::ScraperBackend("连接失败或请求超时".into()))?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !response.status().is_success() {
            return Err(AppError::ScraperBackend(format!(
                "接口返回 HTTP {}，请检查连接或资料源",
                response.status().as_u16()
            )));
        }
        if response
            .content_length()
            .is_some_and(|len| len > limit as u64)
        {
            return Err(AppError::BadRequest("后端资料超过读取预算".into()));
        }
        let mut bytes = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(part) = stream.next().await {
            let part = part.map_err(|_| AppError::ScraperBackend("响应读取中断".into()))?;
            if bytes.len().saturating_add(part.len()) > limit {
                return Err(AppError::BadRequest("后端资料超过读取预算".into()));
            }
            bytes.extend_from_slice(&part);
        }
        Ok(Some(bytes))
    }

    async fn data(&self, parts: &[&str], query: &[(&str, &str)]) -> AppResult<Option<Value>> {
        let Some(bytes) = self.get(parts, query, 2 * 1024 * 1024).await? else {
            return Ok(None);
        };
        let value: Value = serde_json::from_slice(&bytes)
            .map_err(|_| AppError::ScraperBackend("响应不是有效的 JSON".into()))?;
        value
            .get("data")
            .cloned()
            .map(Some)
            .ok_or_else(|| AppError::ScraperBackend("响应缺少 data 字段".into()))
    }

    pub async fn providers(&self) -> AppResult<Vec<String>> {
        let value = self
            .data(&["v1", "providers"], &[])
            .await?
            .ok_or(AppError::NotFound)?;
        let providers: BTreeMap<String, String> =
            serde_json::from_value(value["movie_providers"].clone())
                .map_err(|_| AppError::ScraperBackend("资料源列表格式无效".into()))?;
        Ok(providers.into_keys().collect())
    }

    pub async fn search(&self, number: &str) -> AppResult<Vec<Candidate>> {
        let number = valid_number(number)?;
        let Some(value) = self
            .data(&["v1", "movies", "search"], &[("q", &number)])
            .await?
        else {
            return Ok(Vec::new());
        };
        let results: Vec<Candidate> = serde_json::from_value(value)
            .map_err(|_| AppError::ScraperBackend("搜索结果格式无效".into()))?;
        Ok(results
            .into_iter()
            .filter(|c| !c.provider.is_empty() && !c.id.is_empty())
            .take(100)
            .collect())
    }

    pub async fn movie(&self, candidate: &Candidate) -> AppResult<Movie> {
        identity(candidate)?;
        let value = self
            .data(
                &["v1", "movies", &candidate.provider, &candidate.id],
                &[("lazy", "true")],
            )
            .await?
            .ok_or(AppError::NotFound)?;
        let movie: Movie = serde_json::from_value(value)
            .map_err(|_| AppError::ScraperBackend("影片详情格式无效".into()))?;
        if movie.provider != candidate.provider
            || movie.id != candidate.id
            || movie.title.trim().is_empty()
        {
            return Err(AppError::BadRequest(
                "影片详情与选定候选不一致或缺少标题".into(),
            ));
        }
        Ok(movie)
    }

    pub async fn person(&self, name: &str) -> AppResult<Option<Person>> {
        let Some(value) = self
            .data(
                &["v1", "actors", "search"],
                &[("q", name), ("fallback", "false")],
            )
            .await?
        else {
            return Ok(None);
        };
        let candidates = serde_json::from_value(value)
            .map_err(|_| AppError::ScraperBackend("人物搜索格式无效".into()))?;
        let Some(candidate) = exact_person(candidates, name) else {
            return Ok(None);
        };
        let Some(value) = self
            .data(
                &["v1", "actors", &candidate.provider, &candidate.id],
                &[("lazy", "true")],
            )
            .await?
        else {
            return Ok(None);
        };
        let person: Person = serde_json::from_value(value)
            .map_err(|_| AppError::ScraperBackend("人物详情格式无效".into()))?;
        if person.provider != candidate.provider
            || person.id != candidate.id
            || person.name.trim() != name.trim()
        {
            return Ok(None);
        }
        Ok(Some(person))
    }

    pub async fn person_image(&self, person: &Person) -> AppResult<Option<Vec<u8>>> {
        let Some(bytes) = self
            .get(
                &["v1", "images", "primary", &person.provider, &person.id],
                &[],
                8 * 1024 * 1024,
            )
            .await?
        else {
            return Ok(None);
        };
        tokio::task::spawn_blocking(move || encode_poster(&bytes, true))
            .await
            .map_err(|e| AppError::Internal(e.into()))?
            .map(Some)
    }

    pub async fn poster(&self, candidate: &Candidate, preview: bool) -> AppResult<Vec<u8>> {
        identity(candidate)?;
        let bytes = self
            .get(
                &[
                    "v1",
                    "images",
                    "primary",
                    &candidate.provider,
                    &candidate.id,
                ],
                &[],
                8 * 1024 * 1024,
            )
            .await?
            .ok_or(AppError::NotFound)?;
        tokio::task::spawn_blocking(move || encode_poster(&bytes, preview))
            .await
            .map_err(|e| AppError::Internal(e.into()))?
    }
}

fn identity(candidate: &Candidate) -> AppResult<()> {
    if [&candidate.provider, &candidate.id].iter().any(|s| {
        s.is_empty()
            || s.len() > 200
            || s.as_str() == "."
            || s.as_str() == ".."
            || s.chars()
                .any(|c| c.is_control() || matches!(c, '/' | '\\' | '?' | '#'))
    }) {
        return Err(AppError::BadRequest("资料来源或条目 ID 无效".into()));
    }
    Ok(())
}

pub(super) fn encode_poster(bytes: &[u8], preview: bool) -> AppResult<Vec<u8>> {
    let bad = || AppError::BadRequest("海报无法解码或超过像素预算".into());
    let reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| bad())?;
    let (width, height) = reader.into_dimensions().map_err(|_| bad())?;
    if width == 0 || height == 0 || u64::from(width) * u64::from(height) > 16_000_000 {
        return Err(bad());
    }
    let image = image::load_from_memory(bytes).map_err(|_| bad())?;
    let mut unit = (width / 2)
        .min(height / 3)
        .max(1)
        .min(if preview { 120 } else { 600 });
    let limit = if preview { 128 * 1024 } else { POSTER_LIMIT };
    loop {
        let rgb = image
            .resize_to_fill(unit * 2, unit * 3, image::imageops::FilterType::Lanczos3)
            .to_rgb8();
        for quality in [90, 80, 65, 50] {
            let mut output = Vec::new();
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut output, quality)
                .write_image(
                    rgb.as_raw(),
                    rgb.width(),
                    rgb.height(),
                    image::ExtendedColorType::Rgb8,
                )
                .map_err(|_| bad())?;
            if output.len() <= limit {
                return Ok(output);
            }
        }
        if unit == 1 {
            return Err(bad());
        }
        unit = (unit / 2).max(1);
    }
}

pub(super) fn poster_value(bytes: Vec<u8>) -> Value {
    json!({"data":format!("data:image/jpeg;base64,{}", base64::engine::general_purpose::STANDARD.encode(bytes))})
}

fn escaped(value: &str) -> AppResult<String> {
    if value
        .chars()
        .any(|c| matches!(c as u32, 0..=8 | 11..=12 | 14..=31 | 0xfffe | 0xffff))
    {
        return Err(AppError::BadRequest(
            "资料包含 XML 无法表示的字符，请选择其他来源".into(),
        ));
    }
    Ok(value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;"))
}

fn strip_number_prefix<'a>(title: &'a str, number: &str) -> Option<&'a str> {
    let first = title.chars().next()?;
    let closing = match first {
        '[' => Some(']'),
        '(' => Some(')'),
        '{' => Some('}'),
        '【' => Some('】'),
        '（' => Some('）'),
        '「' => Some('」'),
        '『' => Some('』'),
        _ => None,
    };
    let (prefix, rest) = if let Some(closing) = closing {
        let inner = &title[first.len_utf8()..];
        let end = inner.find(closing)?;
        (&inner[..end], &inner[end + closing.len_utf8()..])
    } else {
        let end = title
            .find(|c: char| !c.is_ascii_alphanumeric() && !matches!(c, '-' | '_'))
            .unwrap_or(title.len());
        (&title[..end], &title[end..])
    };
    // 只移除完整的同番号前缀，保留其他番号和片名正文。
    let prefix = valid_number(prefix).ok()?;
    (normalized_number(&prefix) == normalized_number(number)).then_some(rest)
}

impl Movie {
    fn display_title(&self) -> AppResult<String> {
        let number = valid_number(&self.number)?;
        let mut title = self.title.trim();
        while let Some(rest) = strip_number_prefix(title, &number) {
            title = rest.trim_start();
        }
        Ok(if title.is_empty() {
            number
        } else {
            format!("{number} {title}")
        })
    }

    pub fn nfo(&self) -> AppResult<Vec<u8>> {
        self.nfo_with_actors(
            &self
                .actor_names()
                .into_iter()
                .enumerate()
                .map(|(i, name)| crate::apps::nfo::Actor {
                    name,
                    order: Some(i as u32),
                    ..Default::default()
                })
                .collect::<Vec<_>>(),
        )
    }

    pub fn actor_names(&self) -> Vec<String> {
        let mut seen = std::collections::HashSet::new();
        self.actors
            .iter()
            .flatten()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .filter(|s| seen.insert((*s).to_owned()))
            .map(str::to_owned)
            .collect()
    }

    pub fn nfo_with_actors(&self, actors: &[crate::apps::nfo::Actor]) -> AppResult<Vec<u8>> {
        let title = self.display_title()?;
        let mut xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<movie>\n");
        let mut tag = |key: &str, value: &str| -> AppResult<()> {
            if !value.is_empty() {
                xml.push_str(&format!("  <{key}>{}</{key}>\n", escaped(value)?));
            }
            Ok(())
        };
        tag("num", &self.number)?;
        tag("title", &title)?;
        tag("originaltitle", &self.title)?;
        for (key, value) in [
            ("plot", &self.summary),
            ("director", &self.director),
            ("studio", &self.maker),
            ("label", &self.label),
        ] {
            if let Some(value) = value {
                tag(key, value)?;
            }
        }
        if let Some(date) = &self.release_date {
            tag("releasedate", date)?;
            if let Some(year) = date
                .get(..4)
                .filter(|s| s.bytes().all(|b| b.is_ascii_digit()))
            {
                tag("year", year)?;
            }
            if let Some(day) = date.get(..10) {
                tag("premiered", day)?;
            }
        }
        for genre in self.genres.iter().flatten() {
            tag("genre", genre)?;
            tag("tag", genre)?;
        }
        for actor in actors {
            xml.push_str(&format!("  <actor><name>{}</name>", escaped(&actor.name)?));
            for (key, value) in [
                ("role", &actor.role),
                ("thumb", &actor.thumb),
                ("biography", &actor.biography),
            ] {
                if !value.is_empty() {
                    xml.push_str(&format!("<{key}>{}</{key}>", escaped(value)?));
                }
            }
            if let Some(order) = actor.order {
                xml.push_str(&format!("<order>{order}</order>"));
            }
            if !actor.provider.is_empty() && !actor.id.is_empty() {
                xml.push_str(&format!(
                    "<uniqueid type=\"metatube\" provider=\"{}\">{}</uniqueid>",
                    escaped(&actor.provider)?,
                    escaped(&actor.id)?
                ));
            }
            xml.push_str("</actor>\n");
        }
        if let Some(series) = &self.series
            && !series.is_empty()
        {
            xml.push_str(&format!("  <set><name>{}</name></set>\n", escaped(series)?));
        }
        xml.push_str(&format!("  <uniqueid type=\"number\" default=\"true\">{}</uniqueid>\n  <metatube provider=\"{}\" id=\"{}\"/>\n", escaped(&self.number)?, escaped(&self.provider)?, escaped(&self.id)?));
        if let Some(score) = self.score.filter(|s| s.is_finite()) {
            xml.push_str(&format!(
                "  <metatube_score provider=\"{}\">{score}</metatube_score>\n",
                escaped(&self.provider)?
            ));
        }
        xml.push_str("</movie>\n");
        if xml.len() > super::super::nfo::MAX_BYTES {
            return Err(AppError::BadRequest(
                "完整 NFO 超过 1 MiB，请选择其他来源或手动处理".into(),
            ));
        }
        Ok(xml.into_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn person_matching_requires_one_exact_source_identity() {
        let person = |id: &str, name: &str| Person {
            provider: "Fixture".into(),
            id: id.into(),
            name: name.into(),
            ..Default::default()
        };
        assert!(exact_person(vec![person("one", "甲"), person("two", "甲")], "甲").is_none());
        assert!(exact_person(vec![person("one", "甲乙")], "甲").is_none());
        assert_eq!(
            exact_person(
                vec![
                    person("one", " 甲 "),
                    person("one", "甲"),
                    person("near", "甲乙")
                ],
                "甲"
            )
            .unwrap()
            .id,
            "one"
        );
        assert!(exact_person(vec![person("../escape", "甲")], "甲").is_none());
    }
    #[test]
    fn writes_complete_fields_and_keeps_unknown_score_scale() {
        let movie = Movie {
            provider: "测试源".into(),
            id: "来源条目".into(),
            number: "ABC-123".into(),
            title: "甲 & 乙".into(),
            summary: Some("长简介。".repeat(4000)),
            score: Some(5.0),
            actors: Some(vec!["演员甲".into()]),
            label: Some("发行品牌".into()),
            maker: Some("厂商".into()),
            series: Some("电影系列".into()),
            genres: Some(vec!["分类".into()]),
            ..Default::default()
        };
        let xml = String::from_utf8(movie.nfo().unwrap()).unwrap();
        assert_eq!(
            crate::apps::nfo::text(&xml, "title").as_deref(),
            Some("ABC-123 甲 & 乙")
        );
        assert_eq!(
            crate::apps::nfo::text(&xml, "originaltitle").as_deref(),
            Some("甲 & 乙")
        );
        assert_eq!(crate::apps::nfo::text(&xml, "plot"), movie.summary);
        assert!(xml.contains("<metatube_score") && !xml.contains("<rating>"));
        assert!(xml.contains("<label>发行品牌</label>") && xml.contains("<studio>厂商</studio>"));
        let actors = crate::apps::nfo::actors(&xml);
        assert_eq!(actors.len(), 1);
        assert_eq!(actors[0].name, "演员甲");
        assert_eq!(actors[0].order, Some(0));
        assert!(xml.contains("<set><name>电影系列</name></set>"));
    }

    #[test]
    fn nfo_titles_prefix_the_number_once_and_preserve_the_original_title() {
        for (number, original, expected) in [
            ("ABC-123", "影片名称", "ABC-123 影片名称"),
            ("ABC-123", "  影片名称  ", "ABC-123 影片名称"),
            ("ABC-123", "ABC-123 影片名称", "ABC-123 影片名称"),
            ("ABC-123", "abc-123 影片名称", "ABC-123 影片名称"),
            ("ABC-123", "abc123 影片名称", "ABC-123 影片名称"),
            ("ABC-123", "ABC_123 影片名称", "ABC-123 影片名称"),
            ("ABC-123", "[abc-123] 影片名称", "ABC-123 影片名称"),
            ("ABC-123", "[ ABC-123 ]影片名称", "ABC-123 影片名称"),
            ("ABC-123", "(ABC-123)影片名称", "ABC-123 影片名称"),
            ("ABC-123", "{abc-123} 影片名称", "ABC-123 影片名称"),
            ("ABC-123", "【abc-123】 影片名称", "ABC-123 影片名称"),
            ("ABC-123", "（ABC-123） 影片名称", "ABC-123 影片名称"),
            ("ABC-123", "「abc-123」 影片名称", "ABC-123 影片名称"),
            ("ABC-123", "『ABC-123』 影片名称", "ABC-123 影片名称"),
            ("ABC-123", "ABC-123影片名称", "ABC-123 影片名称"),
            (
                "ABC-123",
                "ABC-123 [abc-123] ABC_123 影片名称",
                "ABC-123 影片名称",
            ),
            ("ABC-123", "ABC-123", "ABC-123"),
            ("ABC-123", "[abc-123]", "ABC-123"),
            ("ABC-123", "[abc-123] ABC-123", "ABC-123"),
            (
                "ABC-123",
                "讲述 ABC-123 的影片",
                "ABC-123 讲述 ABC-123 的影片",
            ),
            ("ABC-123", "DEF-456 影片名称", "ABC-123 DEF-456 影片名称"),
            ("ABC-123", "ABC-1234 影片名称", "ABC-123 ABC-1234 影片名称"),
            (
                "ABC-123",
                "ABC-123-CD1 影片名称",
                "ABC-123 ABC-123-CD1 影片名称",
            ),
            (
                "ABC-123",
                "[ABC-123 影片名称] 后续",
                "ABC-123 [ABC-123 影片名称] 后续",
            ),
            ("abc_123", "[ABC-123] 影片名称", "ABC_123 影片名称"),
            (
                "FC2-PPV-1234567",
                "[fc2_ppv_1234567] 影片名称",
                "FC2-PPV-1234567 影片名称",
            ),
            ("123456-001", "123456-001 影片名称", "123456-001 影片名称"),
        ] {
            let movie = Movie {
                number: number.into(),
                title: original.into(),
                ..Default::default()
            };
            let xml = String::from_utf8(movie.nfo().unwrap()).unwrap();
            assert_eq!(
                crate::apps::nfo::text(&xml, "title").as_deref(),
                Some(expected),
                "来源标题：{original}"
            );
            let document = crate::apps::nfo::document(&xml).unwrap();
            let raw_title = document
                .root_element()
                .children()
                .find(|node| node.has_tag_name("originaltitle"))
                .and_then(|node| node.text());
            assert_eq!(raw_title, Some(original), "来源标题：{original}");
            assert_eq!(crate::apps::nfo::number(&xml).as_deref(), Some(number));
        }
    }

    #[test]
    fn poster_is_exact_ratio_and_bounded_jpeg() {
        let image = image::RgbImage::from_pixel(358, 538, image::Rgb([20, 130, 210]));
        let mut input = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut input)
            .encode_image(&image)
            .unwrap();
        let bytes = encode_poster(&input, false).unwrap();
        let decoded = image::load_from_memory(&bytes).unwrap();
        assert_eq!(decoded.width() * 3, decoded.height() * 2);
        assert!(bytes.len() <= POSTER_LIMIT && bytes.starts_with(&[0xff, 0xd8]));
        assert!(encode_poster(b"<html>error</html>", false).is_err());
    }
}
