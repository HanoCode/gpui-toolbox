use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{DefiniteLength, Hsla, SharedString, TextAlign, px, relative};
use html5ever::tendril::TendrilSink;
use html5ever::{LocalName, ParseOpts, local_name, parse_document};
use markup5ever_rcdom::{Node, NodeData, RcDom};

use crate::text::document::ParsedDocument;
use crate::text::node::{
    self, BlockNode, BlockStyle, ImageNode, InlineNode, LinkMark, NodeContext, Paragraph, Table,
    TableRow, TextMark,
};

const BLOCK_ELEMENTS: [&str; 35] = [
    "html",
    "body",
    "head",
    "address",
    "article",
    "aside",
    "blockquote",
    "details",
    "summary",
    "dialog",
    "div",
    "dl",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "hr",
    "main",
    "nav",
    "ol",
    "p",
    "pre",
    "section",
    "table",
    "ul",
    "style",
    "script",
];

/// Parse HTML into AST Node.
pub(crate) fn parse(source: &str, cx: &mut NodeContext) -> Result<ParsedDocument, SharedString> {
    let opts = ParseOpts {
        ..Default::default()
    };

    let bytes = cleanup_html(&source);
    let mut cursor = std::io::Cursor::new(bytes);
    // Ref
    // https://github.com/servo/html5ever/blob/main/rcdom/examples/print-rcdom.rs
    let dom = parse_document(RcDom::default(), opts)
        .from_utf8()
        .read_from(&mut cursor)
        .map_err(|e| SharedString::from(format!("{:?}", e)))?;

    let mut paragraph = Paragraph::default();
    // NOTE: The outer paragraph is not used.
    let node: BlockNode =
        parse_node(&dom.document, &mut paragraph, cx, None).unwrap_or(BlockNode::Unknown);
    let node = node.compact();

    Ok(ParsedDocument {
        source: source.to_string().into(),
        blocks: Arc::new(vec![node]),
    })
}

fn cleanup_html(source: &str) -> Vec<u8> {
    let mut w = std::io::Cursor::new(vec![]);
    let mut r = std::io::Cursor::new(source);
    let mut minify = super::html5minify::Minifier::new(&mut w);
    minify.omit_doctype(true);
    if let Ok(()) = minify.minify(&mut r) {
        w.into_inner()
    } else {
        source.bytes().collect()
    }
}

fn attr_value(attrs: &RefCell<Vec<html5ever::Attribute>>, name: LocalName) -> Option<String> {
    attrs.borrow().iter().find_map(|attr| {
        if attr.name.local == name {
            Some(attr.value.to_string())
        } else {
            None
        }
    })
}

/// Get the highlight background color for a `<mark>` element.
///
/// Reads the `color` attribute first, then the `background-color` declaration
/// from the `style` attribute. Base accepts CSS hex plus common named colors.
fn mark_color(attrs: &RefCell<Vec<html5ever::Attribute>>) -> Option<Hsla> {
    let color_attr = attrs.borrow().iter().find_map(|attr| {
        if &*attr.name.local == "color" {
            Some(attr.value.to_string())
        } else {
            None
        }
    });

    if let Some(value) = color_attr
        && let Some(color) = parse_mark_color(value.trim())
    {
        return Some(color);
    }

    style_attrs(attrs)
        .get("background-color")
        .and_then(|v| parse_mark_color(v.trim()))
}

fn parse_mark_color(value: &str) -> Option<Hsla> {
    if value.starts_with('#') {
        return gpui::Rgba::try_from(value).ok().map(Into::into);
    }
    match value.to_ascii_lowercase().as_str() {
        "black" => Some(gpui::rgb(0x000000).into()),
        "white" => Some(gpui::rgb(0xffffff).into()),
        "blue" => Some(gpui::rgb(0x3b82f6).into()),
        "yellow" => Some(gpui::rgb(0xfacc15).into()),
        _ => None,
    }
}

/// Get style properties to HashMap
/// TODO: Use cssparser to parse style attribute.
fn style_attrs(attrs: &RefCell<Vec<html5ever::Attribute>>) -> HashMap<String, String> {
    let mut styles = HashMap::new();
    let Some(css_text) = attr_value(attrs, local_name!("style")) else {
        return styles;
    };

    for decl in css_text.split(';') {
        let mut parts = decl.splitn(2, ':');
        if let (Some(key), Some(value)) = (parts.next(), parts.next()) {
            styles.insert(
                key.trim().to_lowercase().to_string(),
                value.trim().to_string(),
            );
        }
    }

    styles
}

// ---- DUCK PATCH: inline `style` support ------------------------------------
//
// The document model has no CSS engine, but email is unreadable when every run
// renders in one size and one color: newsletter headings, footnote grays and
// button labels all rely on inline declarations. These helpers lift the three
// declarations that map cleanly onto `TextMark` (`color`, `font-size`,
// `font-weight`) out of the `style` attribute; everything else stays ignored.

/// A CSS length that can resolve to absolute pixels at parse time.
#[derive(Clone, Copy, Debug, PartialEq)]
enum CssLength {
    /// `12px`, `9pt` (already converted) or a unitless number.
    Px(f32),
    /// `1.5em` / `150%` — relative to the inherited font size, when known.
    Em(f32),
}

fn css_length(value: &str) -> Option<CssLength> {
    let value = value.trim().to_ascii_lowercase();
    let digits = value
        .find(|c: char| !c.is_ascii_digit() && c != '.' && c != '-')
        .unwrap_or(value.len());
    let number: f32 = value[..digits].trim().parse().ok()?;
    match value[digits..].trim() {
        "px" | "" => Some(CssLength::Px(number)),
        // CSS fixed relation: 1pt = 96/72 px.
        "pt" => Some(CssLength::Px(number * 96. / 72.)),
        "em" | "rem" => Some(CssLength::Em(number)),
        "%" => Some(CssLength::Em(number / 100.)),
        _ => None,
    }
}

/// Parse a CSS color: `#rgb` / `#rrggbb`, `rgb()` / `rgba()` (comma and space
/// forms) and a few named colors. Unknown values return `None` and inherit.
fn css_color(value: &str) -> Option<Hsla> {
    let value = value.trim();
    if value.starts_with('#') {
        return gpui::Rgba::try_from(value).ok().map(Into::into);
    }

    let lower = value.to_ascii_lowercase();
    let named: Option<u32> = match lower.as_str() {
        "black" => Some(0x000000),
        "white" => Some(0xffffff),
        "red" => Some(0xff0000),
        "green" => Some(0x008000),
        "blue" => Some(0x0000ff),
        "gray" | "grey" => Some(0x808080),
        "silver" => Some(0xc0c0c0),
        "orange" => Some(0xffa500),
        "yellow" => Some(0xffff00),
        "navy" => Some(0x000080),
        "teal" => Some(0x008080),
        _ => None,
    };
    if let Some(rgb) = named {
        return Some(gpui::rgb(rgb).into());
    }

    let is_rgba = lower.starts_with("rgba(");
    let inner = lower
        .strip_prefix("rgb(")
        .or_else(|| lower.strip_prefix("rgba("))?
        .trim_end_matches(')');
    let numbers: Vec<f32> = inner
        .split([',', ' ', '/'])
        .filter_map(|part| part.trim().parse::<f32>().ok())
        .collect();
    if numbers.len() < 3 {
        return None;
    }
    let channel = |v: f32| -> u32 { v.round().clamp(0., 255.) as u32 };
    let packed = (channel(numbers[0]) << 16) | (channel(numbers[1]) << 8) | channel(numbers[2]);
    let mut color: Hsla = gpui::rgb(packed).into();
    if is_rgba {
        color.a = numbers.get(3).copied().unwrap_or(1.).clamp(0., 1.);
    }
    Some(color)
}

/// Compose the inherited mark with this element's inline `style` declarations.
///
/// Block elements hand the result to their children (inheritance), inline
/// elements apply it to their own runs. A declaration only overrides the
/// inherited value when present — the CSS cascade.
fn style_mark(
    attrs: &RefCell<Vec<html5ever::Attribute>>,
    inherited: Option<&TextMark>,
) -> TextMark {
    let mut mark = inherited.cloned().unwrap_or_default();
    apply_style(&mut mark, attrs);
    mark
}

/// The declarations of one element's `style` attribute, applied onto `mark`.
///
/// A present declaration overwrites the inherited value; a missing one leaves
/// it — the CSS cascade. Legacy attributes (`<font color>`) are the fallback
/// when the modern declaration is absent.
fn apply_style(mark: &mut TextMark, attrs: &RefCell<Vec<html5ever::Attribute>>) {
    let styles = style_attrs(attrs);

    let declared_color = styles.get("color").and_then(|value| css_color(value));
    if let Some(color) = declared_color {
        mark.color = Some(color);
    } else if let Some(value) = attr_value(attrs, local_name!("color"))
        && let Some(color) = css_color(&value)
    {
        mark.color = Some(color);
    }

    if let Some(value) = styles.get("font-size") {
        if let Some(length) = css_length(value) {
            match length {
                CssLength::Px(px) => mark.font_size = Some(px),
                CssLength::Em(factor) => {
                    if let Some(base) = mark.font_size {
                        mark.font_size = Some(base * factor);
                    }
                }
            }
        }
    }

    if let Some(value) = styles.get("font-weight") {
        let value = value.trim();
        let bold = value
            .parse::<f32>()
            .map(|weight| weight >= 600.)
            .unwrap_or(false)
            || matches!(value.to_ascii_lowercase().as_str(), "bold" | "bolder");
        if bold {
            mark.bold = true;
        }
    }
}

/// [`style_mark`] plus the one declaration that only makes sense on an inline
/// run: an inline `background-color` becomes a text highlight, the closest
/// thing the document model has to painting behind a run. A block element's
/// background is painted by its container ([`block_style`]) instead, so the two
/// never double up.
fn style_mark_inline(
    attrs: &RefCell<Vec<html5ever::Attribute>>,
    inherited: Option<&TextMark>,
) -> TextMark {
    let mut mark = style_mark(attrs, inherited);
    if let Some(color) = style_attrs(attrs)
        .get("background-color")
        .and_then(|value| css_color(value))
    {
        mark.highlight = Some(color);
    }
    mark
}
// ---- end DUCK PATCH ---------------------------------------------------------

// ---- DUCK PATCH: block-level `style` support --------------------------------
//
// Background, padding and text alignment belong to the container, not to a text
// run, so they ride on `BlockStyle` / `BlockNode::Root` rather than `TextMark`.
// Marketing mail is built almost entirely from these three: coloured bands and
// cards (`background-color` on `table` / `td` / `div`), button and column
// insets (`padding`), and centred headers and footers (`text-align`).
//
// Only declarations that survive without a layout engine are lifted — a
// `padding` in `%` or `em` needs the container width / font size, which the
// document model does not track, so those are left to gpui's default.

/// Parse a CSS `text-align` value, or the legacy `align` attribute value.
fn css_text_align(value: &str) -> Option<TextAlign> {
    match value.trim().to_ascii_lowercase().as_str() {
        "center" | "middle" => Some(TextAlign::Center),
        "right" => Some(TextAlign::Right),
        "left" => Some(TextAlign::Left),
        // `justify` / `end` have no gpui equivalent: keep the default.
        _ => None,
    }
}

/// Resolve a padding length to CSS pixels. Only absolute units resolve here.
fn css_px(value: &str) -> Option<f32> {
    match css_length(value)? {
        CssLength::Px(value) => Some(value),
        CssLength::Em(_) => None,
    }
}

/// Expand the `padding` shorthand (`1px`, `1px 2px`, `1px 2px 3px`,
/// `1px 2px 3px 4px`) into top / right / bottom / left.
fn css_padding_shorthand(value: &str) -> [Option<f32>; 4] {
    let parts: Vec<Option<f32>> = value.split_whitespace().map(css_px).collect();
    match parts.as_slice() {
        [a] => [*a, *a, *a, *a],
        [a, b] => [*a, *b, *a, *b],
        [a, b, c] => [*a, *b, *c, *b],
        [a, b, c, d] => [*a, *b, *c, *d],
        _ => [None; 4],
    }
}

/// The first whitespace-separated token of `background:` that parses as a
/// color — `#eee url(sprite.png) no-repeat` is still a grey background.
fn first_color_token(value: &str) -> Option<Hsla> {
    value.split_whitespace().find_map(css_color)
}

/// Compose the block-level style a container element declares.
///
/// Reads the modern declarations first and falls back to the legacy attributes
/// (`bgcolor`, `align`) that email templates still use — the same layering the
/// inline [`style_mark`] applies, one level up.
fn block_style(attrs: &RefCell<Vec<html5ever::Attribute>>) -> BlockStyle {
    let styles = style_attrs(attrs);

    let background = styles
        .get("background-color")
        .and_then(|value| css_color(value))
        .or_else(|| {
            styles
                .get("background")
                .and_then(|value| first_color_token(value))
        })
        .or_else(|| attr_value(attrs, local_name!("bgcolor")).and_then(|value| css_color(&value)));

    let text_align = styles
        .get("text-align")
        .and_then(|value| css_text_align(value))
        .or_else(|| {
            attr_value(attrs, local_name!("align")).and_then(|value| css_text_align(&value))
        });

    // The shorthand first, then the long hands refine individual sides.
    let mut padding =
        css_padding_shorthand(styles.get("padding").map(String::as_str).unwrap_or(""));
    for (key, side) in [
        ("padding-top", 0usize),
        ("padding-right", 1),
        ("padding-bottom", 2),
        ("padding-left", 3),
    ] {
        if let Some(value) = styles.get(key).and_then(|value| css_px(value)) {
            padding[side] = Some(value);
        }
    }

    BlockStyle {
        background,
        text_align,
        padding_top: padding[0],
        padding_right: padding[1],
        padding_bottom: padding[2],
        padding_left: padding[3],
    }
}

/// Attach a container style to a block, wrapping it in a [`BlockNode::Root`] —
/// the same shape a `<div>` produces — so the renderer can apply it. A block
/// that declared nothing is returned untouched, keeping the tree compact.
fn with_block_style(node: BlockNode, style: BlockStyle) -> BlockNode {
    if style.is_empty() {
        return node;
    }

    BlockNode::Root {
        children: vec![node],
        span: None,
        style,
    }
}
// ---- end DUCK PATCH ---------------------------------------------------------

/// Parse length value from style attribute.
///
/// When is percentage, it will be converted to relative length.
/// Else, it will be converted to pixels.
fn value_to_length(value: &str) -> Option<DefiniteLength> {
    if value.ends_with("%") {
        value
            .trim_end_matches("%")
            .parse::<f32>()
            .ok()
            .map(|v| relative(v / 100.))
    } else {
        value
            .trim_end_matches("px")
            .parse()
            .ok()
            .map(|v| px(v).into())
    }
}

/// Get width, height from attributes or parse them from style attribute.
fn attr_width_height(
    attrs: &RefCell<Vec<html5ever::Attribute>>,
) -> (Option<DefiniteLength>, Option<DefiniteLength>) {
    let mut width = None;
    let mut height = None;

    if let Some(value) = attr_value(attrs, local_name!("width")) {
        width = value_to_length(&value);
    }

    if let Some(value) = attr_value(attrs, local_name!("height")) {
        height = value_to_length(&value);
    }

    if width.is_none() || height.is_none() {
        let styles = style_attrs(attrs);
        if width.is_none() {
            width = styles.get("width").and_then(|v| value_to_length(&v));
        }
        if height.is_none() {
            height = styles.get("height").and_then(|v| value_to_length(&v));
        }
    }

    (width, height)
}

fn parse_table_row(table: &mut Table, node: &Rc<Node>, inherited: Option<&TextMark>) {
    let mut row = TableRow::default();
    let mut count = 0;
    for child in node.children.borrow().iter() {
        match child.data {
            NodeData::Element {
                ref name,
                ref attrs,
                ..
            } if name.local == local_name!("td") || name.local == local_name!("th") => {
                if child.children.borrow().is_empty() {
                    continue;
                }

                count += 1;
                parse_table_cell(&mut row, child, attrs, inherited);
            }
            _ => {}
        }
    }

    if count > 0 {
        table.children.push(row);
    }
}

fn parse_table_cell(
    row: &mut node::TableRow,
    node: &Rc<Node>,
    attrs: &RefCell<Vec<html5ever::Attribute>>,
    inherited: Option<&TextMark>,
) {
    // DUCK PATCH: the cell's own `style` composes over the table's inherited
    // mark before it flows into the cell content.
    let mark = style_mark(attrs, inherited);
    let mut paragraph = Paragraph::default();
    for child in node.children.borrow().iter() {
        parse_paragraph(&mut paragraph, child, Some(&mark));
    }
    let width = attr_width_height(attrs).0;
    let table_cell = node::TableCell {
        children: paragraph,
        width,
    };
    row.children.push(table_cell);
}

/// Trim text but leave at least one space.
///
/// - Before: " \r\n Hello world \t "
/// - After: " Hello world "
#[allow(dead_code)]
fn trim_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());

    for (i, c) in text.chars().enumerate() {
        if c.is_whitespace() {
            if i > 0 && out.ends_with(' ') {
                continue;
            }
        }
        out.push(c);
    }

    out
}

/// DUCK PATCH: push a text node, carrying the inherited inline style when it
/// declares a color / font size (bare text inside a styled block would
/// otherwise lose it — `push_str` always uses the default mark).
fn push_text(paragraph: &mut Paragraph, text: &str, inherited: Option<&TextMark>) {
    let styled = inherited.filter(|mark| mark.color.is_some() || mark.font_size.is_some());
    if let Some(mark) = styled
        && !text.is_empty()
    {
        let mut inline = InlineNode::new(text.to_string());
        let len = inline.text.len();
        inline.marks.push((0..len, mark.clone()));
        paragraph.push(inline);
    } else {
        paragraph.push_str(text);
    }
}

fn parse_paragraph(paragraph: &mut Paragraph, node: &Rc<Node>, inherited: Option<&TextMark>) {
    fn push_merged(
        paragraph: &mut Paragraph,
        text: String,
        marks: Vec<(Range<usize>, TextMark)>,
        new_mark: Option<TextMark>,
    ) {
        if text.is_empty() {
            return;
        }
        let mut node = InlineNode::new(text).marks(marks);
        if let Some(new_mark) = new_mark {
            let len = node.text.len();
            if let Some(last) = node.marks.last_mut()
                && last.0.start == 0
                && last.0.end == len
            {
                last.1.merge(new_mark);
            } else {
                node.marks.push((0..node.text.len(), new_mark));
            }
        }
        paragraph.push(node);
    }

    // DUCK PATCH: children inherit the composed mark — the cascade flows down
    // through it (`new_mark` already carries the inherited values).
    fn merge_children_with_mark(
        node: &Node,
        paragraph: &mut Paragraph,
        new_mark: Option<TextMark>,
    ) {
        let mut merged_text = String::new();
        let mut merged_marks = Vec::new();

        for child in node.children.borrow().iter() {
            let mut child_paragraph = Paragraph::default();
            parse_paragraph(&mut child_paragraph, &child, new_mark.as_ref());

            for node in child_paragraph.children {
                let offset = merged_text.len();
                merged_text.push_str(&node.text);
                for (range, child_mark) in node.marks {
                    merged_marks.push((range.start + offset..range.end + offset, child_mark));
                }

                if let Some(mut image) = node.image {
                    if let Some(link_mark) = new_mark.as_ref().and_then(|mark| mark.link.clone()) {
                        image.link = Some(link_mark);
                    }

                    push_merged(
                        paragraph,
                        std::mem::take(&mut merged_text),
                        std::mem::take(&mut merged_marks),
                        new_mark.clone(),
                    );

                    paragraph.push(InlineNode::image(image));
                }
            }
        }

        push_merged(paragraph, merged_text, merged_marks, new_mark.clone());
    }

    // DUCK PATCH: the mark an element hands its children — inherited values
    // first, then the tag's own semantics, then its inline `style` (so
    // `<a style="color:#fff">` keeps both its link and its declared color).
    let compose = |own: TextMark, attrs: &RefCell<Vec<html5ever::Attribute>>| -> TextMark {
        let mut mark = inherited.cloned().unwrap_or_default();
        mark.merge(own);
        apply_style(&mut mark, attrs);
        mark
    };

    match &node.data {
        NodeData::Text { contents } => {
            let part = &contents.borrow();
            push_text(paragraph, part, inherited);
        }
        NodeData::Element { name, attrs, .. } => match name.local {
            local_name!("em") | local_name!("i") => {
                merge_children_with_mark(
                    node,
                    paragraph,
                    Some(compose(TextMark::default().italic(), attrs)),
                );
            }
            local_name!("strong") | local_name!("b") => {
                merge_children_with_mark(
                    node,
                    paragraph,
                    Some(compose(TextMark::default().bold(), attrs)),
                );
            }
            local_name!("del") | local_name!("s") => {
                merge_children_with_mark(
                    node,
                    paragraph,
                    Some(compose(TextMark::default().strikethrough(), attrs)),
                );
            }
            local_name!("u") => {
                merge_children_with_mark(
                    node,
                    paragraph,
                    Some(compose(TextMark::default().underline(), attrs)),
                );
            }
            local_name!("code") => {
                merge_children_with_mark(
                    node,
                    paragraph,
                    Some(compose(TextMark::default().code(), attrs)),
                );
            }
            local_name!("mark") => {
                let color = mark_color(&attrs).unwrap_or_else(|| gpui::rgb(0xfef08a).into());
                merge_children_with_mark(
                    node,
                    paragraph,
                    Some(compose(TextMark::default().highlight(color), attrs)),
                );
            }
            local_name!("a") => {
                let link_mark = LinkMark {
                    url: attr_value(&attrs, local_name!("href"))
                        .unwrap_or_default()
                        .into(),
                    title: attr_value(&attrs, local_name!("title")).map(Into::into),
                    ..Default::default()
                };

                merge_children_with_mark(
                    node,
                    paragraph,
                    Some(compose(TextMark::default().link(link_mark), attrs)),
                );
            }
            local_name!("img") => {
                let Some(src) = attr_value(attrs, local_name!("src")) else {
                    if cfg!(debug_assertions) {
                        tracing::warn!("Image node missing src attribute");
                    }
                    return;
                };

                let alt = attr_value(attrs, local_name!("alt"));
                let title = attr_value(attrs, local_name!("title"));
                let (width, height) = attr_width_height(attrs);

                paragraph.push_image(ImageNode {
                    url: src.into(),
                    alt: alt.map(Into::into),
                    width,
                    height,
                    title: title.map(Into::into),
                    ..Default::default()
                });
            }
            _ => {
                // DUCK PATCH: unknown inline elements (`span`, `font`, …) carry
                // their `style` declarations — including an inline background,
                // which becomes a highlight; bare containers just pass the
                // inherited mark through.
                merge_children_with_mark(
                    node,
                    paragraph,
                    Some(style_mark_inline(attrs, inherited)),
                );
            }
        },
        _ => {
            merge_children_with_mark(node, paragraph, None);
        }
    }
}

fn parse_node(
    node: &Rc<Node>,
    paragraph: &mut Paragraph,
    cx: &mut NodeContext,
    inherited: Option<&TextMark>,
) -> Option<BlockNode> {
    match node.data {
        NodeData::Text { ref contents } => {
            let text = contents.borrow().to_string();
            if text.len() > 0 {
                push_text(paragraph, &text, inherited);
            }

            None
        }
        NodeData::Element {
            ref name,
            ref attrs,
            ..
        } => match name.local {
            local_name!("br") => Some(BlockNode::Break {
                html: true,
                span: None,
            }),
            local_name!("h1")
            | local_name!("h2")
            | local_name!("h3")
            | local_name!("h4")
            | local_name!("h5")
            | local_name!("h6") => {
                let mut children = vec![];
                consume_paragraph(&mut children, paragraph);

                let level = name
                    .local
                    .chars()
                    .last()
                    .unwrap_or('6')
                    .to_digit(10)
                    .unwrap_or(6) as u8;

                let mut paragraph = Paragraph::default();
                // DUCK PATCH: heading text inherits the composed inline style.
                let mark = style_mark(attrs, inherited);
                for child in node.children.borrow().iter() {
                    parse_paragraph(&mut paragraph, child, Some(&mark));
                }

                // DUCK PATCH: the heading's own block style (background /
                // padding / alignment) rides along with it.
                let heading = with_block_style(
                    BlockNode::Heading {
                        level,
                        children: paragraph,
                        span: None,
                    },
                    block_style(attrs),
                );
                if children.len() > 0 {
                    children.push(heading);

                    Some(BlockNode::Root {
                        children,
                        span: None,
                        style: BlockStyle::default(),
                    })
                } else {
                    Some(heading)
                }
            }
            local_name!("img") => {
                let mut children = vec![];
                consume_paragraph(&mut children, paragraph);

                let Some(src) = attr_value(attrs, local_name!("src")) else {
                    if cfg!(debug_assertions) {
                        tracing::warn!("image node missing src attribute");
                    }
                    return None;
                };

                let alt = attr_value(&attrs, local_name!("alt"));
                let title = attr_value(&attrs, local_name!("title"));
                let (width, height) = attr_width_height(&attrs);

                let mut paragraph = Paragraph::default();
                paragraph.push_image(ImageNode {
                    url: src.into(),
                    title: title.map(Into::into),
                    alt: alt.map(Into::into),
                    width,
                    height,
                    ..Default::default()
                });

                if children.len() > 0 {
                    children.push(BlockNode::Paragraph(paragraph));
                    Some(BlockNode::Root {
                        children,
                        span: None,
                        style: BlockStyle::default(),
                    })
                } else {
                    Some(BlockNode::Paragraph(paragraph))
                }
            }
            local_name!("ul") | local_name!("ol") => {
                let ordered = name.local == local_name!("ol");
                // DUCK PATCH: list items inherit the composed inline style.
                let mark = style_mark(attrs, inherited);
                let children = consume_children_nodes(node, paragraph, cx, Some(&mark));
                // DUCK PATCH: the list container carries its own block style.
                Some(with_block_style(
                    BlockNode::List {
                        children,
                        ordered,
                        start: None,
                        span: None,
                    },
                    block_style(attrs),
                ))
            }
            local_name!("li") => {
                let mut children = vec![];
                consume_paragraph(&mut children, paragraph);

                // DUCK PATCH: list item content inherits the composed style.
                let mark = style_mark(attrs, inherited);
                for child in node.children.borrow().iter() {
                    let mut child_paragraph = Paragraph::default();
                    if let Some(child_node) =
                        parse_node(child, &mut child_paragraph, cx, Some(&mark))
                    {
                        children.push(child_node);
                    }
                    if child_paragraph.text_len() > 0 {
                        // If last child is paragraph, merge child
                        if let Some(last_child) = children.last_mut() {
                            if let BlockNode::Paragraph(last_paragraph) = last_child {
                                last_paragraph.merge(child_paragraph);
                                continue;
                            }
                        }

                        children.push(BlockNode::Paragraph(child_paragraph));
                    }
                }

                consume_paragraph(&mut children, paragraph);

                Some(BlockNode::ListItem {
                    children,
                    spread: false,
                    checked: None,
                    span: None,
                })
            }
            local_name!("table") => {
                let mut children = vec![];
                consume_paragraph(&mut children, paragraph);

                // DUCK PATCH: cells inherit the composed inline style.
                let mark = style_mark(attrs, inherited);
                let mut table = Table::default();
                for child in node.children.borrow().iter() {
                    match child.data {
                        NodeData::Element { ref name, .. }
                            if name.local == local_name!("tbody")
                                || name.local == local_name!("thead") =>
                        {
                            for sub_child in child.children.borrow().iter() {
                                parse_table_row(&mut table, &sub_child, Some(&mark));
                            }
                        }
                        _ => {
                            parse_table_row(&mut table, &child, Some(&mark));
                        }
                    }
                }
                consume_paragraph(&mut children, paragraph);

                // DUCK PATCH: the table's own block style wraps the grid.
                let table = with_block_style(BlockNode::Table(table), block_style(attrs));
                if children.len() > 0 {
                    children.push(table);
                    Some(BlockNode::Root {
                        children,
                        span: None,
                        style: BlockStyle::default(),
                    })
                } else {
                    Some(table)
                }
            }
            local_name!("blockquote") => {
                // DUCK PATCH: quote content inherits the composed inline style.
                let mark = style_mark(attrs, inherited);
                let children = consume_children_nodes(node, paragraph, cx, Some(&mark));
                // DUCK PATCH: the quote container carries its own block style.
                Some(with_block_style(
                    BlockNode::Blockquote {
                        children,
                        span: None,
                    },
                    block_style(attrs),
                ))
            }
            // DUCK PATCH: `<center>` is a block container in HTML; without this
            // it would fall through to the inline path and lose its line break.
            // `text-align: center` is what it means.
            local_name!("center") => {
                let mut children: Vec<BlockNode> = vec![];
                consume_paragraph(&mut children, paragraph);

                let mark = style_mark(attrs, inherited);
                for child in node.children.borrow().iter() {
                    if let Some(child_node) = parse_node(child, paragraph, cx, Some(&mark)) {
                        children.push(child_node);
                    }
                }
                consume_paragraph(&mut children, paragraph);

                if children.is_empty() {
                    None
                } else {
                    let mut style = block_style(attrs);
                    style.text_align.get_or_insert(TextAlign::Center);
                    Some(BlockNode::Root {
                        children,
                        span: None,
                        style,
                    })
                }
            }
            local_name!("style") | local_name!("script") => None,
            _ => {
                if BLOCK_ELEMENTS.contains(&name.local.trim()) {
                    let mut children: Vec<BlockNode> = vec![];

                    // Case:
                    //
                    // Hello <p>Inner text of block element</p> World

                    // Insert before text as a node -- The "Hello"
                    consume_paragraph(&mut children, paragraph);

                    // DUCK PATCH: block elements are the inheritance boundary —
                    // their `style` applies to everything inside them.
                    let mark = style_mark(attrs, inherited);

                    // Inner of the block element -- The "Inner text of block element"
                    for child in node.children.borrow().iter() {
                        if let Some(child_node) = parse_node(child, paragraph, cx, Some(&mark)) {
                            children.push(child_node);
                        }
                    }
                    consume_paragraph(&mut children, paragraph);

                    // DUCK PATCH: the container's own background / padding /
                    // alignment. An element with a style but no text (a spacer
                    // band) is kept, so its padding still paints.
                    let style = block_style(attrs);
                    if children.is_empty() && style.is_empty() {
                        None
                    } else {
                        Some(BlockNode::Root {
                            children,
                            span: None,
                            style,
                        })
                    }
                } else {
                    // Others to as Inline
                    parse_paragraph(paragraph, node, inherited);

                    if paragraph.is_image() {
                        Some(BlockNode::Paragraph(paragraph.take()))
                    } else {
                        None
                    }
                }
            }
        },
        NodeData::Document => {
            let children = consume_children_nodes(node, paragraph, cx, inherited);
            Some(BlockNode::Root {
                children,
                span: None,
                style: BlockStyle::default(),
            })
        }
        NodeData::Doctype { .. }
        | NodeData::Comment { .. }
        | NodeData::ProcessingInstruction { .. } => None,
    }
}

fn consume_children_nodes(
    node: &Node,
    paragraph: &mut Paragraph,
    cx: &mut NodeContext,
    inherited: Option<&TextMark>,
) -> Vec<BlockNode> {
    let mut children = vec![];
    consume_paragraph(&mut children, paragraph);
    for child in node.children.borrow().iter() {
        if let Some(child_node) = parse_node(child, paragraph, cx, inherited) {
            children.push(child_node);
        }
        consume_paragraph(&mut children, paragraph);
    }

    children
}

fn consume_paragraph(children: &mut Vec<BlockNode>, paragraph: &mut Paragraph) {
    if paragraph.is_empty() {
        return;
    }

    children.push(BlockNode::Paragraph(paragraph.take()));
}

#[cfg(test)]
mod tests {
    use gpui::{TextAlign, px, relative};
    use std::sync::Arc;

    use crate::text::{
        document::ParsedDocument,
        node::{BlockNode, BlockStyle, ImageNode, InlineNode, NodeContext, Paragraph},
    };

    use super::trim_text;

    #[test]
    fn test_cleanup_html() {
        let html = r#"<p>
            and
            <code>code</code>
            text
        </p>"#;
        let cleaned = super::cleanup_html(html);
        assert_eq!(
            String::from_utf8(cleaned).unwrap(),
            "<p>and <code>code</code> text"
        );

        let html = r#"<p>
            and
            <em>   <code>code</code>   <i>italic</i>   </em>
            text
        </p>"#;
        let cleaned = super::cleanup_html(html);
        assert_eq!(
            String::from_utf8(cleaned).unwrap(),
            "<p>and <em><code>code</code> <i>italic</i></em> text"
        );
    }

    #[test]
    fn test_trim_text() {
        assert_eq!(trim_text("  \n\tHello world \t\r "), " Hello world ",);
    }

    #[test]
    fn test_mark() {
        let mut cx = NodeContext::default();

        // `<mark>` is rendered as a highlight, kept as `==...==` in markdown.
        let html = r#"<p>Hello <mark>world</mark></p>"#;
        let node = super::parse(html, &mut cx).unwrap();
        assert_eq!(node.to_markdown(), "Hello ==world==");

        let html = r#"<p><mark color="blue">blue</mark> and <mark style="background-color: #336699">hex</mark></p>"#;
        let node = super::parse(html, &mut cx).unwrap();
        assert_eq!(node.to_markdown(), "==blue== and ==hex==");
    }

    #[test]
    fn test_keep_spaces() {
        let html = r#"<p>and <code>code</code> text</p>"#;
        let mut cx = NodeContext::default();
        let node = super::parse(html, &mut cx).unwrap();
        assert_eq!(node.to_markdown(), "and `code` text");

        let html = r#"
            <div>
            <p>
                and
                <em>   <code>code</code>   <i>italic</i>   </em>
                text
            </p>
            <p>
                <img src="https://example.com/image.png" alt="Example" width="100" height="200" title="Example Image" />
            </p>
            <ul>
                <li>Item 1</li>
                <li>Item 2
                </li>
            </ul>
            </div>
        "#;
        let node = super::parse(html, &mut cx).unwrap();
        assert_eq!(
            node.to_markdown(),
            indoc::indoc! {r#"
            and *code italic* text

            ![Example](https://example.com/image.png "Example Image")

            - Item 1
            - Item 2
            "#}
            .trim()
        );
    }

    #[test]
    fn test_value_to_length() {
        assert_eq!(super::value_to_length("100px"), Some(px(100.).into()));
        assert_eq!(super::value_to_length("100%"), Some(relative(1.)));
        assert_eq!(super::value_to_length("56%"), Some(relative(0.56)));
        assert_eq!(super::value_to_length("240"), Some(px(240.).into()));
    }

    // ---- DUCK PATCH tests: inline `style` support --------------------------

    /// First inline node of the first paragraph of a single-paragraph document.
    fn first_inline(html: &str) -> crate::text::node::InlineNode {
        use crate::text::node::{BlockNode, NodeContext};
        let mut cx = NodeContext::default();
        let node = super::parse(html, &mut cx).unwrap();
        let blocks: &[BlockNode] = match &node.blocks[0] {
            BlockNode::Root { children, .. } => children,
            other => std::slice::from_ref(other),
        };
        match &blocks[0] {
            BlockNode::Paragraph(paragraph) => paragraph.children[0].clone(),
            other => panic!("expected paragraph, got {other:?}"),
        }
    }

    #[test]
    fn test_inline_style_color_and_font_size() {
        let inline = first_inline(
            r#"<p><span style="color: #0496f7; font-size: 20px;">big blue</span></p>"#,
        );
        let mark = &inline.marks[0].1;
        assert_eq!(mark.color, Some(gpui::rgb(0x0496f7).into()));
        assert_eq!(mark.font_size, Some(20.));
    }

    #[test]
    fn test_inline_style_rgb_color_and_bold_weight() {
        let inline = first_inline(
            r#"<p><span style="color: rgb(3, 18, 66); font-weight: 700;">dark bold</span></p>"#,
        );
        let mark = &inline.marks[0].1;
        assert_eq!(mark.color, Some(gpui::rgb(0x031242).into()));
        assert!(mark.bold);
    }

    #[test]
    fn test_block_style_inherits_into_child_paragraph() {
        let inline =
            first_inline(r#"<div style="color:#b0b0b0; font-size:13px"><p>footnote</p></div>"#);
        let mark = &inline.marks[0].1;
        assert_eq!(mark.color, Some(gpui::rgb(0xb0b0b0).into()));
        assert_eq!(mark.font_size, Some(13.));
    }

    #[test]
    fn test_nested_style_overrides_inherited() {
        // The inner span wins for its own text; the outer color still applies
        // to the surrounding runs.
        let inline = first_inline(
            r#"<p><span style="color:#ff0000">red <span style="color:#00ff00">green</span></span></p>"#,
        );
        assert_eq!(inline.marks[0].1.color, Some(gpui::rgb(0xff0000).into()));
    }

    #[test]
    fn test_table_cell_style_inherits() {
        let html = r#"<table><tr><td style="color:#123456">cell</td></tr></table>"#;
        let mut cx = NodeContext::default();
        let node = super::parse(html, &mut cx).unwrap();
        let crate::text::node::BlockNode::Table(table) = &node.blocks[0] else {
            panic!("expected table");
        };
        let mark = &table.children[0].children[0].children.children[0].marks[0].1;
        assert_eq!(mark.color, Some(gpui::rgb(0x123456).into()));
    }

    #[test]
    fn test_css_length_variants() {
        assert_eq!(super::css_length("16px"), Some(super::CssLength::Px(16.)));
        assert_eq!(super::css_length("12pt"), Some(super::CssLength::Px(16.)));
        assert_eq!(super::css_length("150%"), Some(super::CssLength::Em(1.5)));
        assert_eq!(
            super::css_length("1.25em"),
            Some(super::CssLength::Em(1.25))
        );
        assert_eq!(super::css_length("2vw"), None);
    }

    #[test]
    fn test_css_color_variants() {
        assert_eq!(
            super::css_color("#0496f7"),
            Some(gpui::rgb(0x0496f7).into())
        );
        assert_eq!(
            super::css_color("rgba(0, 0, 0, 0.54)").map(|c: gpui::Hsla| c.a > 0. && c.a < 1.),
            Some(true)
        );
        assert_eq!(super::css_color("white"), Some(gpui::rgb(0xffffff).into()));
        assert_eq!(super::css_color("url(x)"), None);
    }

    #[test]
    fn test_image() {
        let html = r#"<img src="https://example.com/image.png" alt="Example" width="100" height="200" title="Example Image" />"#;
        let mut cx = NodeContext::default();
        let node = super::parse(html, &mut cx).unwrap();
        assert_eq!(
            node,
            ParsedDocument {
                source: html.to_string().into(),
                blocks: Arc::new(vec![BlockNode::Paragraph(Paragraph {
                    span: None,
                    children: vec![InlineNode::image(ImageNode {
                        url: "https://example.com/image.png".to_string().into(),
                        alt: Some("Example".to_string().into()),
                        width: Some(px(100.).into()),
                        height: Some(px(200.).into()),
                        title: Some("Example Image".to_string().into()),
                        ..Default::default()
                    })],
                    ..Default::default()
                })])
            }
        );

        let html = r#"<img src="https://example.com/image.png" alt="Example" style="width: 80%" title="Example Image" />"#;
        let node = super::parse(html, &mut cx).unwrap();
        assert_eq!(
            node,
            ParsedDocument {
                source: html.to_string().into(),
                blocks: Arc::new(vec![BlockNode::Paragraph(Paragraph {
                    span: None,
                    children: vec![InlineNode::image(ImageNode {
                        url: "https://example.com/image.png".to_string().into(),
                        alt: Some("Example".to_string().into()),
                        width: Some(relative(0.8)),
                        height: None,
                        title: Some("Example Image".to_string().into()),
                        ..Default::default()
                    })],
                    ..Default::default()
                })])
            }
        );
    }

    // ---- DUCK PATCH tests: block-level `style` support ---------------------

    /// The block style of the container the document parsed into.
    fn root_block_style(html: &str) -> BlockStyle {
        let mut cx = NodeContext::default();
        let node = super::parse(html, &mut cx).unwrap();
        match &node.blocks[0] {
            BlockNode::Root { style, .. } => style.clone(),
            other => panic!("expected a styled container, got {other:?}"),
        }
    }

    #[test]
    fn test_block_style_background_padding_and_align() {
        let style = root_block_style(
            r#"<div style="background-color:#f5f5f5; padding: 10px 20px 30px 40px; text-align:center">hi</div>"#,
        );
        assert_eq!(style.background, Some(gpui::rgb(0xf5f5f5).into()));
        assert_eq!(style.text_align, Some(TextAlign::Center));
        assert_eq!(style.padding_top, Some(10.));
        assert_eq!(style.padding_right, Some(20.));
        assert_eq!(style.padding_bottom, Some(30.));
        assert_eq!(style.padding_left, Some(40.));
    }

    #[test]
    fn test_block_style_padding_shorthand_expands() {
        let cases = [
            ("padding: 8px", [8., 8., 8., 8.]),
            ("padding: 8px 16px", [8., 16., 8., 16.]),
            ("padding: 1px 2px 3px", [1., 2., 3., 2.]),
            ("padding: 0 16px 0 0", [0., 16., 0., 0.]),
        ];
        for (declaration, expected) in cases {
            let style = root_block_style(&format!(r#"<div style="{declaration}">hi</div>"#));
            assert_eq!(
                [
                    style.padding_top,
                    style.padding_right,
                    style.padding_bottom,
                    style.padding_left,
                ],
                expected.map(Some),
                "{declaration}"
            );
        }
    }

    #[test]
    fn test_block_style_long_hand_overrides_shorthand() {
        let style = root_block_style(r#"<div style="padding: 4px; padding-left: 20px">hi</div>"#);
        assert_eq!(style.padding_top, Some(4.));
        assert_eq!(style.padding_right, Some(4.));
        assert_eq!(style.padding_left, Some(20.));
    }

    #[test]
    fn test_block_style_legacy_attributes_and_background_shorthand() {
        let style = root_block_style(r##"<div bgcolor="#eeeeee" align="right">hi</div>"##);
        assert_eq!(style.background, Some(gpui::rgb(0xeeeeee).into()));
        assert_eq!(style.text_align, Some(TextAlign::Right));

        // The first colour token of the `background:` shorthand is the colour.
        let style =
            root_block_style(r#"<div style="background: #333333 url(x.png) no-repeat">hi</div>"#);
        assert_eq!(style.background, Some(gpui::rgb(0x333333).into()));
    }

    #[test]
    fn test_block_style_center_tag_and_single_child_compaction() {
        // `<center>` is a block container; single-child compaction must not
        // drop its style.
        assert_eq!(
            root_block_style("<center>hi</center>").text_align,
            Some(TextAlign::Center)
        );
        assert_eq!(
            root_block_style(r##"<div style="background-color:#000">hi</div>"##).background,
            Some(gpui::rgb(0x000000).into())
        );
    }

    #[test]
    fn test_block_style_kept_for_a_style_only_container() {
        // A background-only div is the classic colour band — it must survive
        // even though it has no text.
        let style = root_block_style(r##"<div style="background-color:#eeeeee"></div>"##);
        assert_eq!(style.background, Some(gpui::rgb(0xeeeeee).into()));
    }

    #[test]
    fn test_heading_and_table_keep_their_block_style() {
        let style =
            root_block_style(r#"<h1 style="background-color:#123456;text-align:right">T</h1>"#);
        assert_eq!(style.background, Some(gpui::rgb(0x123456).into()));
        assert_eq!(style.text_align, Some(TextAlign::Right));

        let mut cx = NodeContext::default();
        let node = super::parse(
            r##"<table style="background-color:#fafafa"><tr><td>x</td></tr></table>"##,
            &mut cx,
        )
        .unwrap();
        let BlockNode::Root { style, .. } = &node.blocks[0] else {
            panic!(
                "expected the table to carry its own container, got {:?}",
                node.blocks[0]
            );
        };
        assert_eq!(style.background, Some(gpui::rgb(0xfafafa).into()));
    }

    #[test]
    fn test_inline_elements_keep_their_declared_style() {
        // An inline background becomes a highlight — the only "paint behind the
        // text" the document model has.
        let inline = first_inline(r##"<p><span style="background-color:#ffff00">hi</span></p>"##);
        assert_eq!(
            inline.marks[0].1.highlight,
            Some(gpui::rgb(0xffff00).into())
        );

        // `<a style="color:…">` keeps both its link and its declared colour.
        let inline = first_inline(r##"<p><a href="https://x" style="color:#ffffff">go</a></p>"##);
        let mark = &inline.marks[0].1;
        assert_eq!(mark.color, Some(gpui::rgb(0xffffff).into()));
        assert_eq!(mark.link.as_ref().unwrap().url.as_ref(), "https://x");

        // The legacy `<font color>` attribute.
        let inline = first_inline(r##"<p><font color="#ff0000">red</font></p>"##);
        assert_eq!(inline.marks[0].1.color, Some(gpui::rgb(0xff0000).into()));
    }
}
