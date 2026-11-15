//! 两端共用的有界 NFO 读取；资料文本不执行外部实体或网络引用。

pub(crate) const MAX_BYTES: usize = 1024 * 1024;

pub(crate) fn actor_image_prefix(base: &str) -> String {
    use sha2::{Digest, Sha256};
    let hash = crate::engine::upload::to_hex(&Sha256::digest(base.to_ascii_lowercase().as_bytes()));
    format!("tgdrive-actor-{}-", &hash[..32])
}

pub(crate) fn actor_image_name(base: &str, item: &str, index: usize) -> String {
    use sha2::{Digest, Sha256};
    let item = crate::engine::upload::to_hex(&Sha256::digest(item.as_bytes()));
    format!("{}{}-{index}.jpg", actor_image_prefix(base), &item[..32])
}

pub(crate) fn is_actor_image(name: &str, base: &str) -> bool {
    let name = name.to_ascii_lowercase();
    let prefix = actor_image_prefix(base);
    name.strip_prefix(&prefix)
        .and_then(|s| s.strip_suffix(".jpg"))
        .and_then(|s| s.split_once('-'))
        .is_some_and(|(item, index)| {
            item.len() == 32
                && item.bytes().all(|b| b.is_ascii_hexdigit())
                && !index.is_empty()
                && index.bytes().all(|b| b.is_ascii_digit())
        })
}

#[derive(Clone, Debug, Default, serde::Deserialize, serde::Serialize)]
#[serde(default)]
pub(crate) struct Actor {
    pub name: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order: Option<u32>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub thumb: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub biography: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub provider: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub id: String,
}

fn node_text(node: roxmltree::Node<'_, '_>, tag: &str) -> Option<String> {
    let child = node.children().find(|n| n.has_tag_name(tag))?;
    let value: String = child
        .children()
        .filter(|n| n.is_text())
        .filter_map(|n| n.text())
        .collect();
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

/// 保留资料的原始相对顺序；人物归并和关系去重由消费端按身份处理。
pub(crate) fn actors(xml: &str) -> Vec<Actor> {
    let Some(doc) = document(xml) else {
        return Vec::new();
    };
    doc.root_element()
        .children()
        .filter(|n| n.has_tag_name("actor"))
        .filter_map(|node| {
            let name = node_text(node, "name")?;
            let identity = node
                .children()
                .filter(|n| n.has_tag_name("uniqueid"))
                .filter_map(|n| {
                    let kind = n.attribute("type")?.trim();
                    let provider = if kind.eq_ignore_ascii_case("metatube") {
                        n.attribute("provider")?.trim()
                    } else {
                        kind
                    };
                    let id = n.text()?.trim();
                    (!provider.is_empty() && !id.is_empty())
                        .then(|| (n.attribute("default") == Some("true"), provider, id))
                })
                .max_by_key(|(preferred, _, _)| *preferred);
            Some(Actor {
                name,
                role: node_text(node, "role").unwrap_or_default(),
                order: node_text(node, "order").and_then(|s| s.parse().ok()),
                thumb: node_text(node, "thumb").unwrap_or_default(),
                biography: node_text(node, "biography")
                    .or_else(|| node_text(node, "description"))
                    .unwrap_or_default(),
                provider: identity
                    .map(|(_, provider, _)| provider.to_owned())
                    .unwrap_or_default(),
                id: identity.map(|(_, _, id)| id.to_owned()).unwrap_or_default(),
            })
        })
        .collect()
}

/// 本地头像只在资料所在目录及其后代寻找，不能借 NFO 读取网络或越过配套目录。
pub(crate) fn local_actor_path(directory: &str, reference: &str) -> Option<String> {
    let reference = reference.trim();
    if reference.is_empty()
        || reference.starts_with('/')
        || reference
            .chars()
            .any(|c| c.is_control() || matches!(c, '\\' | ':' | '?' | '#'))
    {
        return None;
    }
    let mut components: Vec<_> = directory.split('/').filter(|s| !s.is_empty()).collect();
    let boundary = components.len();
    for part in reference.split('/') {
        match part {
            "" | "." => {}
            ".." if components.len() > boundary => {
                components.pop();
            }
            ".." => return None,
            name => components.push(name),
        }
    }
    (components.len() > boundary).then(|| format!("/{}", components.join("/")))
}

pub(crate) fn poster_names(base: &str) -> Vec<String> {
    let mut names = vec![format!("{base}-poster.jpg"), format!("{base}-poster.jpeg")];
    for extension in ["jpg", "jpeg", "png", "webp"] {
        names.push(format!("{base}.{extension}"));
    }
    for common in ["poster", "folder", "cover"] {
        for extension in ["jpg", "jpeg", "png", "webp"] {
            names.push(format!("{common}.{extension}"));
        }
    }
    names
}

pub(crate) fn document(text: &str) -> Option<roxmltree::Document<'_>> {
    if text.len() > MAX_BYTES || text.contains("<!DOCTYPE") || text.contains("<!ENTITY") {
        return None;
    }
    let doc = roxmltree::Document::parse(text).ok()?;
    if !matches!(
        doc.root_element().tag_name().name(),
        "movie" | "episodedetails" | "tvshow"
    ) {
        return None;
    }
    Some(doc)
}

pub(crate) fn text(xml: &str, tag: &str) -> Option<String> {
    let doc = document(xml)?;
    let node = doc
        .root_element()
        .children()
        .find(|n| n.is_element() && n.tag_name().name() == tag)?;
    let value: String = node
        .children()
        .filter(|n| n.is_text())
        .filter_map(|n| n.text())
        .collect();
    let value = value.trim().to_string();
    (!value.is_empty()).then_some(value)
}

pub(crate) fn number(xml: &str) -> Option<String> {
    let doc = document(xml)?;
    let root = doc.root_element();
    root.children()
        .find(|n| n.has_tag_name("num"))
        .or_else(|| {
            root.children()
                .find(|n| n.has_tag_name("uniqueid") && n.attribute("type") == Some("number"))
        })
        .and_then(|n| n.text())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_long_unicode_and_decodes_xml_once() {
        let plot = "长简介与原始文字。".repeat(2500);
        let xml =
            format!("<movie><title>甲 &amp; 乙</title><plot><![CDATA[{plot}]]></plot></movie>");
        assert_eq!(text(&xml, "title").as_deref(), Some("甲 & 乙"));
        assert_eq!(text(&xml, "plot").as_deref(), Some(plot.as_str()));
        assert_eq!(
            text("<movie><title>&amp;lt;</title></movie>", "title").as_deref(),
            Some("&lt;")
        );
    }

    #[test]
    fn rejects_entities_malformed_xml_and_oversize() {
        assert!(document("<!DOCTYPE movie [<!ENTITY x SYSTEM 'file:///etc/passwd'>]><movie><title>&x;</title></movie>").is_none());
        assert!(document("<movie><title>缺少闭合</movie>").is_none());
        assert!(
            document(&format!(
                "<movie><plot>{}</plot></movie>",
                "a".repeat(MAX_BYTES)
            ))
            .is_none()
        );
    }

    #[test]
    fn reads_nested_actor_fields_without_inventing_missing_data() {
        let bio = "人物简介与原始文字。".repeat(3000);
        let xml = format!(
            "<movie><name>不是演员</name><actor><name>  演员甲 &amp; 乙  </name><role>角色 &amp; 名称</role><order>2</order><thumb>actors/肖像.jpg</thumb><biography><![CDATA[{bio}]]></biography><uniqueid type='metatube' provider='Gfriends'>演员甲 &amp; 乙</uniqueid></actor><actor><name> </name></actor><actor><name>演员丙</name><order>-1</order></actor></movie>"
        );
        let actors = actors(&xml);
        assert_eq!(actors.len(), 2);
        assert_eq!(actors[0].name, "演员甲 & 乙");
        assert_eq!(actors[0].role, "角色 & 名称");
        assert_eq!(actors[0].order, Some(2));
        assert_eq!(actors[0].biography, bio);
        assert_eq!(actors[0].provider, "Gfriends");
        assert_eq!(actors[1].order, None);
        assert!(actors[1].thumb.is_empty());
    }

    #[test]
    fn actor_paths_stay_in_the_metadata_directory() {
        assert_eq!(
            local_actor_path("/影视/甲", "演员.jpg").as_deref(),
            Some("/影视/甲/演员.jpg")
        );
        assert_eq!(
            local_actor_path("/影视/甲", "actors/../演员.jpg").as_deref(),
            Some("/影视/甲/演员.jpg")
        );
        for path in [
            "https://example.invalid/头像.jpg",
            "//example.invalid/a.jpg",
            "../私有/a.jpg",
            "actors/../../a.jpg",
            "/私有/a.jpg",
            "a.jpg?token=secret",
            "a\\b.jpg",
        ] {
            assert!(local_actor_path("/影视/甲", path).is_none(), "{path}");
        }
    }

    #[test]
    fn generated_portraits_keep_their_video_group_when_item_tokens_are_not_hex() {
        let name = actor_image_name("ABC-123-4K", "sDjO4nZV1eE6rX41jrqyMNY9CyutTY91", 0);
        assert!(is_actor_image(&name, "abc-123-4k"));
        assert!(!is_actor_image(&name, "ABC-123"));
        assert_ne!(
            name,
            actor_image_name("ABC-123-4K", "another-batch-item", 0)
        );
    }
}
