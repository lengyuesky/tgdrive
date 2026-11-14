//! 两端共用的有界 NFO 读取；资料文本不执行外部实体或网络引用。

pub(crate) const MAX_BYTES: usize = 1024 * 1024;

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
}
