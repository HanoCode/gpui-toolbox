//! 极简 XML 读取：只够解析 S3 的响应。
//!
//! 不引 `quick-xml`：这里要的只有「按标签取文本 / 取块」两件事，S3 的响应形状
//! 是固定的（`ListBucketResult` / `ListAllMyBucketsResult` / `Error`），上一套
//! 通用解析器（连带 `memchr` 与两遍扫描）只为读十几个字段，不划算。
//!
//! 明确**不支持**：命名空间前缀、DTD、CDATA、属性值里的 `>`。
//! S3 及其兼容实现（MinIO / Ceph / 阿里云 OSS 的 S3 接口）都不用这些；
//! 真遇到了，[`text`] 返回 `None`，调用方按「字段缺失」处理，不会误读。

/// 取 `<tag>` 的文本内容（第一个）。自动解实体并 trim。
pub fn text(xml: &str, tag: &str) -> Option<String> {
    blocks(xml, tag)
        .into_iter()
        .next()
        .map(|inner| unescape(inner.trim()))
}

/// 取所有 `<tag>...</tag>` 的内部片段。
pub fn blocks<'a>(xml: &'a str, tag: &str) -> Vec<&'a str> {
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find(&open) {
        let after = &rest[start + open.len()..];
        let Some(gt) = after.find('>') else { break };
        let tail = &after[..gt];
        // `<Key` 也会命中 `<KeyCount` / `<KeyTooLong>`：标签名后面必须是
        // `>` / `/` / 空白（属性）三者之一，否则不是同一个标签。
        let same_tag = tail.is_empty()
            || tail.starts_with('/')
            || tail.starts_with(|c: char| c.is_ascii_whitespace());
        if !same_tag {
            rest = &rest[start + open.len()..];
            continue;
        }
        let body_start = start + open.len() + gt + 1;
        if tail.ends_with('/') {
            // 自闭合：没有内容，也算一个空块。
            out.push("");
            rest = &rest[body_start..];
            continue;
        }
        let Some(end) = rest[body_start..].find(&close) else {
            break;
        };
        out.push(&rest[body_start..body_start + end]);
        rest = &rest[body_start + end + close.len()..];
    }
    out
}

/// 解五个预定义实体 + 数字实体（S3 的 key 里 `&` `<` `>` 都会被转义）。
fn unescape(raw: &str) -> String {
    if !raw.contains('&') {
        return raw.to_string();
    }
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let tail = &rest[amp..];
        let Some(semi) = tail.find(';') else {
            out.push_str(tail);
            return out;
        };
        let entity = &tail[1..semi];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => entity
                .strip_prefix("#x")
                .or_else(|| entity.strip_prefix("#X"))
                .and_then(|hex| u32::from_str_radix(hex, 16).ok())
                .or_else(|| entity.strip_prefix('#').and_then(|dec| dec.parse().ok()))
                .and_then(char::from_u32),
        };
        match decoded {
            Some(ch) => {
                out.push(ch);
                rest = &tail[semi + 1..];
            }
            // 不认识的实体原样留着（宁可显示 `&nbsp;` 也不要吞字符）。
            None => {
                out.push_str(&tail[..=semi]);
                rest = &tail[semi + 1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Name>demo</Name>
  <Prefix>a/</Prefix>
  <KeyCount>2</KeyCount>
  <IsTruncated>false</IsTruncated>
  <Contents>
    <Key>a/1.txt</Key>
    <LastModified>2026-10-01T12:00:00.000Z</LastModified>
    <ETag>&quot;abc&quot;</ETag>
    <Size>12</Size>
    <StorageClass>STANDARD</StorageClass>
  </Contents>
  <Contents>
    <Key>a/&amp;2.txt</Key>
    <Size>0</Size>
  </Contents>
  <CommonPrefixes>
    <Prefix>a/sub/</Prefix>
  </CommonPrefixes>
</ListBucketResult>"#;

    #[test]
    fn blocks_do_not_confuse_key_with_keycount() {
        // 只看 `<Key ...>`，不能被 `<KeyCount>` 骗到（这是手写解析最典型的坑）。
        let contents = blocks(LIST, "Contents");
        assert_eq!(contents.len(), 2);
        let keys: Vec<Option<String>> = contents.iter().map(|block| text(block, "Key")).collect();
        assert_eq!(keys[0].as_deref(), Some("a/1.txt"));
        // 实体还原：`&amp;` → `&`，`&quot;` → `"`。
        assert_eq!(keys[1].as_deref(), Some("a/&2.txt"));
        assert_eq!(text(contents[0], "ETag").as_deref(), Some("\"abc\""));
        assert_eq!(text(LIST, "KeyCount").as_deref(), Some("2"));
    }

    #[test]
    fn empty_and_missing_tags_are_told_apart() {
        assert_eq!(text("<A></A>", "A").as_deref(), Some(""));
        assert_eq!(text("<A/>", "A").as_deref(), Some(""));
        assert_eq!(text("<A>1</A>", "B"), None);
    }

    #[test]
    fn error_body_fields_are_readable() {
        let body = r#"<Error><Code>NoSuchBucket</Code><Message>The specified bucket does not exist</Message><RequestId>X</RequestId></Error>"#;
        assert_eq!(text(body, "Code").as_deref(), Some("NoSuchBucket"));
        assert_eq!(
            text(body, "Message").as_deref(),
            Some("The specified bucket does not exist")
        );
    }

    #[test]
    fn numeric_entities_and_unknown_entities() {
        assert_eq!(unescape("&#65;&#x42;"), "AB");
        // 不认识的实体原样保留，不吞字符。
        assert_eq!(unescape("a&nbsp;b"), "a&nbsp;b");
        // 没有分号的裸 `&` 也原样保留。
        assert_eq!(unescape("a&b"), "a&b");
    }
}
