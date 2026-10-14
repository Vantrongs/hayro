//! Rich text (PDF 32000-1 12.7.3.4): XHTML whose elements and CSS `style`
//! attributes set the style of each part of an annotation's text, and the default
//! style (`/DS`), a CSS declaration list.
//!
//! Common viewers read rich text as HTML, so it is parsed as HTML is (html5ever,
//! the WHATWG algorithm): comments, quoted attribute values, character references,
//! and unclosed or misnested elements are read as those viewers read them. The
//! elements and declarations below are drawn as those viewers draw them, matched
//! by observing their output; any other one makes the appearance unsupported, so
//! that nothing is drawn differently from them unnoticed.

use super::Unsupported;
use super::out::Color;
use super::text::{Align, Family, Span, Style};
use html5ever::interface::{ElementFlags, NodeOrText, QuirksMode, TreeSink};
use html5ever::tendril::{StrTendril, TendrilSink};
use html5ever::{Attribute, ExpandedName, ParseOpts, QualName, ns, parse_document};
use rustc_hash::FxHashSet;
use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

/// How deep elements may nest. The parser looks through the elements open around
/// each one it reads, so its work grows with this depth; written rich text nests
/// a few elements deep.
const MAX_DEPTH: usize = 256;

/// The text of rich text `xhtml` in the styles its elements set: each element's
/// style over the style of the element it is in, the outermost over `base`.
/// Paragraphs (`p`, `div`) start and end a line, as does `br`; other whitespace is
/// kept. A line is aligned by the `text-align` of the paragraph it is in.
pub(super) fn rich_text(xhtml: &str, base: &Style) -> Result<Vec<Span>, Unsupported> {
    walk(&parse(xhtml)?, base)
}

/// `style` with the declarations of the CSS declaration list `css` (a `/DS`).
pub(super) fn css_style(css: &str, mut style: Style) -> Result<Style, Unsupported> {
    let size = style.size;
    if declarations(css, &mut style, size)? {
        Ok(style)
    } else {
        Err(Unsupported::RichText)
    }
}

/// Elements drawn, but for the style they set, as the text they hold.
const SHOWN: [&str; 10] = [
    "html", "body", "p", "div", "span", "br", "b", "strong", "i", "em",
];

/// Elements whose content is not drawn.
const HIDDEN: [&str; 6] = ["head", "title", "style", "script", "meta", "link"];

fn parse(xhtml: &str) -> Result<Rc<Node>, Unsupported> {
    parsed(xhtml).map(|dom| dom.document)
}

fn parsed(xhtml: &str) -> Result<Dom, Unsupported> {
    // Each node but the three the parser adds comes from at least one byte, but for
    // copies of formatting elements left open, made for each paragraph after them.
    let dom = Dom {
        document: Node::new(Kind::Document),
        nodes: Cell::new(0),
        max_nodes: xhtml.len() + 16,
        unsupported: Cell::new(false),
        names_looked_at: Cell::new(0),
    };
    let mut parser = parse_document(dom, ParseOpts::default());
    // Fed in pieces, so that parsing stops soon after the document turns out to be
    // one that is not drawn, before the parser's work outgrows it.
    let mut rest = xhtml;
    while !rest.is_empty() {
        let mut at = rest.len().min(256);
        while !rest.is_char_boundary(at) {
            at += 1;
        }
        let (piece, tail) = rest.split_at(at);
        parser.process(StrTendril::from_slice(piece));
        if parser.tokenizer.sink.sink.unsupported.get() {
            return Err(Unsupported::RichText);
        }
        rest = tail;
    }
    let dom = parser.finish();
    if dom.unsupported.get() {
        return Err(Unsupported::RichText);
    }
    Ok(dom)
}

/// The spans of the document under `document`.
fn walk(document: &Rc<Node>, base: &Style) -> Result<Vec<Span>, Unsupported> {
    enum Step {
        Enter(Rc<Node>),
        /// The end of an element; a paragraph ends its line.
        Leave {
            paragraph: bool,
        },
    }
    let mut spans: Vec<Span> = Vec::new();
    // The style of each open element, outermost first, with its `text-align`, and
    // the alignment of the lines of the paragraph it is in.
    let mut open: Vec<(Style, Align)> = vec![(base.clone(), base.align)];
    let mut steps: Vec<Step> = children(document).into_iter().map(Step::Enter).collect();
    steps.reverse();
    while let Some(step) = steps.pop() {
        let (parent, lines) = open.last().cloned().expect("the base style stays");
        let set = |style: &Style, align: Align| Style {
            align,
            ..style.clone()
        };
        let node = match step {
            Step::Leave { paragraph } => {
                if paragraph {
                    end_line(&mut spans, &set(&parent, lines));
                }
                open.pop();
                continue;
            }
            Step::Enter(node) => node,
        };
        let (name, attrs) = match &node.kind {
            Kind::Text(text) => {
                add(&mut spans, &text.borrow(), &set(&parent, lines));
                continue;
            }
            Kind::Element { name, attrs, .. } => (name, attrs),
            Kind::Document | Kind::Other => continue,
        };
        let tag: &str = &name.local;
        if HIDDEN.contains(&tag) {
            continue;
        }
        let mut style = parent.clone();
        match tag {
            "b" | "strong" => style.face.bold = true,
            "i" | "em" => style.face.italic = true,
            _ => {}
        }
        let mut shown = true;
        for attr in attrs.borrow().iter() {
            let value: &str = &attr.value;
            match &*attr.name.local {
                "style" => shown &= declarations(value, &mut style, parent.size)?,
                // Presentational attributes viewers draw by.
                "align" | "hidden" | "bgcolor" | "background" | "text" => {
                    return Err(Unsupported::RichText);
                }
                "dir" if !value.eq_ignore_ascii_case("ltr") => return Err(Unsupported::RichText),
                _ => {}
            }
        }
        if !shown {
            continue;
        }
        let paragraph = matches!(tag, "p" | "div");
        if tag == "br" {
            add(&mut spans, "\n", &set(&parent, lines));
            continue;
        }
        if paragraph {
            end_line(&mut spans, &set(&parent, lines));
        }
        // The lines of a block are aligned by its `text-align`; an inline element's
        // only passes on to the blocks it holds.
        let lines = if matches!(tag, "html" | "body" | "p" | "div") {
            style.align
        } else {
            lines
        };
        open.push((style, lines));
        steps.push(Step::Leave { paragraph });
        steps.extend(children(&node).into_iter().rev().map(Step::Enter));
    }
    Ok(spans)
}

fn children(node: &Rc<Node>) -> Vec<Rc<Node>> {
    node.children.borrow().clone()
}

/// Adds `text` in `style`, to the last span when that has the same style.
fn add(spans: &mut Vec<Span>, text: &str, style: &Style) {
    if text.is_empty() {
        return;
    }
    match spans.last_mut() {
        Some(last) if last.style == *style => last.text.push_str(text),
        _ => spans.push(Span {
            text: text.to_owned(),
            style: style.clone(),
        }),
    }
}

/// Ends the line unless the text is at the start of one.
fn end_line(spans: &mut Vec<Span>, style: &Style) {
    if spans.last().is_some_and(|s| !s.text.ends_with('\n')) {
        add(spans, "\n", style);
    }
}

/// Applies the CSS declarations of a `style` attribute to `style`, the element's
/// style so far; `parent` is the size relative sizes are of. Whether the element is
/// shown.
fn declarations(css: &str, style: &mut Style, parent: f32) -> Result<bool, Unsupported> {
    let unsupported = Err(Unsupported::RichText);
    if css.contains("/*") || css.contains(['"', '\'']) && !quotes_balanced(css) {
        return unsupported;
    }
    let mut shown = true;
    for declaration in split_outside_quotes(css, ';') {
        let declaration = declaration.trim();
        if declaration.is_empty() {
            continue;
        }
        let Some((property, value)) = declaration.split_once(':') else {
            return unsupported;
        };
        let property = property.trim();
        let value = value.trim();
        let value = value
            .strip_suffix("!important")
            .map_or(value, str::trim_end);
        match property {
            "font-size" => style.size = font_size(value, parent)?,
            "font-family" => style.face.family = family(value)?,
            "font-weight" => style.face.bold = bold(value)?,
            "font-style" => style.face.italic = italic(value)?,
            "font" => font(value, style, parent)?,
            "color" => style.color = color(value)?,
            "text-align" => {
                style.align = match value {
                    "left" => Align::Left,
                    "center" => Align::Center,
                    "right" => Align::Right,
                    "justify" => Align::Justify,
                    _ => return unsupported,
                }
            }
            "display" if value == "none" => shown = false,
            // Declarations that draw nothing differently at these values.
            "text-decoration" if value == "none" => {}
            "vertical-align" if value == "baseline" => {}
            "line-height" if value == "normal" => {}
            "visibility" if value == "visible" => {}
            "margin" | "margin-top" | "margin-right" | "margin-bottom" | "margin-left"
            | "padding" | "padding-top" | "padding-right" | "padding-bottom" | "padding-left"
            | "text-indent"
                if value.split_whitespace().all(zero) => {}
            // Declarations common viewers do not draw by.
            "font-stretch" | "font-variant" | "letter-spacing" | "word-spacing"
            | "text-transform" | "background-color" => {}
            property if property.starts_with("xfa-") => {}
            _ => return unsupported,
        }
    }
    Ok(shown)
}

fn quotes_balanced(css: &str) -> bool {
    let mut quote = None;
    for c in css.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            None if c == '"' || c == '\'' => quote = Some(c),
            _ => {}
        }
    }
    quote.is_none()
}

/// The parts of `s` between `separator`s that are not inside quotes.
fn split_outside_quotes(s: &str, separator: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let (mut quote, mut from) = (None, 0);
    for (i, c) in s.char_indices() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == '"' || c == '\'' => quote = Some(c),
            None if c == separator => {
                parts.push(&s[from..i]);
                from = i + c.len_utf8();
            }
            None => {}
        }
    }
    parts.push(&s[from..]);
    parts
}

/// A number with a unit, split.
fn number(value: &str) -> Option<(f32, &str)> {
    let digits = value
        .strip_prefix('+')
        .unwrap_or(value)
        .trim_start_matches(|c: char| c.is_ascii_digit() || c == '.');
    let number = &value[..value.len() - digits.len()];
    let unsigned = number.strip_prefix('+').unwrap_or(number);
    if unsigned.is_empty() || unsigned == "." || unsigned.matches('.').count() > 1 {
        return None;
    }
    Some((unsigned.parse().ok()?, digits))
}

/// A CSS number without a unit, signed: not the words for infinity and not-a-number
/// a float parser also reads.
fn signed(value: &str) -> Option<f32> {
    let (sign, value) = match value.strip_prefix('-') {
        Some(rest) => (-1.0, rest),
        None => (1.0, value),
    };
    match number(value)? {
        (n, "") => Some(sign * n),
        _ => None,
    }
}

fn zero(value: &str) -> bool {
    number(value).is_some_and(|(n, unit)| {
        n == 0.0 && ["", "pt", "px", "em", "%", "in", "mm", "cm"].contains(&unit)
    })
}

/// A `font-size`: points (`px` counted as points, as common viewers do), or `em`
/// and `%` of `parent`. A size too large to write is kept: text drawn in it fails
/// the appearance as every number too large to write does.
fn font_size(value: &str, parent: f32) -> Result<f32, Unsupported> {
    let (n, unit) = number(value).ok_or(Unsupported::RichText)?;
    let size = match unit.to_ascii_lowercase().as_str() {
        "pt" | "px" => n,
        "em" => n * parent,
        "%" => n * parent / 100.0,
        _ => return Err(Unsupported::RichText),
    };
    if size > 0.0 {
        Ok(size)
    } else {
        Err(Unsupported::RichText)
    }
}

/// The family of a `font-family` list, as common viewers map its names: by the
/// first word of each name, in order, Helvetica and Arial as Helvetica, Times as
/// Times, Courier as Courier, and the generic families; without one of those, the
/// default face ([`Family::Text`]), as for the generic `serif`. Symbol and
/// Dingbats faces are drawn in a font of other glyphs: unsupported.
fn family(value: &str) -> Result<Family, Unsupported> {
    let mut firsts = Vec::new();
    for name in split_outside_quotes(value, ',') {
        let name = name.trim();
        let name = ['"', '\'']
            .iter()
            .find_map(|q| name.strip_prefix(*q)?.strip_suffix(*q))
            .unwrap_or(name);
        // An empty name makes the list invalid.
        let first = name
            .split_whitespace()
            .next()
            .ok_or(Unsupported::RichText)?;
        firsts.push(first.to_ascii_lowercase());
    }
    for first in firsts {
        match first.as_str() {
            "helvetica" | "arial" | "sans-serif" => return Ok(Family::Sans),
            "times" => return Ok(Family::Serif),
            "courier" | "monospace" => return Ok(Family::Mono),
            "serif" => return Ok(Family::Text),
            "inherit" | "initial" | "unset" => return Err(Unsupported::RichText),
            _ if ["symbol", "zapf", "dingbats"]
                .iter()
                .any(|p| first.starts_with(p)) =>
            {
                return Err(Unsupported::RichText);
            }
            _ => {}
        }
    }
    Ok(Family::Text)
}

/// Whether a `font-weight` is bold: bold and bolder, and weights above normal (400).
fn bold(value: &str) -> Result<bool, Unsupported> {
    match value {
        "normal" | "lighter" => Ok(false),
        "bold" | "bolder" => Ok(true),
        _ => match number(value) {
            Some((n, "")) => Ok(n > 400.0),
            _ => Err(Unsupported::RichText),
        },
    }
}

/// Whether a `font-style` is italic: italic and oblique.
fn italic(value: &str) -> Result<bool, Unsupported> {
    match value {
        "normal" => Ok(false),
        "italic" | "oblique" => Ok(true),
        _ if value.starts_with("oblique ") => Ok(true),
        _ => Err(Unsupported::RichText),
    }
}

/// The `font` shorthand, its parts in any order as common viewers read it: style
/// and weight keywords, a size (with `/normal` line height), and the family names
/// in the remaining words. Style and weight not given are normal, a family not
/// given the default; a size not given stays.
fn font(value: &str, style: &mut Style, parent: f32) -> Result<(), Unsupported> {
    let (mut bold, mut italic) = (false, false);
    let mut words = Vec::new();
    for word in value.split_whitespace() {
        match word {
            "normal" | "small-caps" => {}
            "italic" | "oblique" => italic = true,
            "bold" | "bolder" => bold = true,
            "lighter" => bold = false,
            _ if word.starts_with(|c: char| c.is_ascii_digit() || c == '.' || c == '+') => {
                match number(word) {
                    Some((n, "")) if (100.0..=900.0).contains(&n) => bold = n > 400.0,
                    _ => {
                        let (size, line_height) = word.split_once('/').unwrap_or((word, "normal"));
                        if line_height != "normal" {
                            return Err(Unsupported::RichText);
                        }
                        style.size = font_size(size, parent)?;
                    }
                }
            }
            _ => words.push(word),
        }
    }
    style.face.bold = bold;
    style.face.italic = italic;
    style.face.family = if words.is_empty() {
        Family::Text
    } else {
        family(&words.join(" "))?
    };
    Ok(())
}

/// A CSS colour as an RGB colour: `#rrggbb`, `#rgb`, `rgb(r, g, b)` in numbers or
/// percentages, or one of the 16 basic colour names.
fn color(value: &str) -> Result<Color, Unsupported> {
    let rgb = color_rgb(value).ok_or(Unsupported::RichText)?;
    Ok(Color::Rgb(
        rgb.map(|v| f64::from((v * 10000.0).round() / 10000.0)),
    ))
}

fn color_rgb(value: &str) -> Option<[f32; 3]> {
    if let Some(hex) = value.strip_prefix('#') {
        let digits: Vec<u32> = hex.chars().map(|c| c.to_digit(16)).collect::<Option<_>>()?;
        return match digits.len() {
            6 => Some([0, 2, 4].map(|i| (digits[i] * 16 + digits[i + 1]) as f32 / 255.0)),
            3 => Some([0, 1, 2].map(|i| (digits[i] * 17) as f32 / 255.0)),
            _ => None,
        };
    }
    if let Some(inner) = value.strip_prefix("rgb(") {
        let parts: Vec<f32> = inner
            .strip_suffix(')')?
            .split(',')
            .map(|p| {
                let p = p.trim();
                match p.strip_suffix('%') {
                    Some(percent) => Some(signed(percent)? / 100.0),
                    None => Some(signed(p)? / 255.0),
                }
            })
            .collect::<Option<_>>()?;
        let [r, g, b] = parts.as_slice() else {
            return None;
        };
        return Some([*r, *g, *b].map(|v| v.clamp(0.0, 1.0)));
    }
    let hex = match value.to_ascii_lowercase().as_str() {
        "black" => 0x000000,
        "silver" => 0xc0c0c0,
        "gray" => 0x808080,
        "white" => 0xffffff,
        "maroon" => 0x800000,
        "red" => 0xff0000,
        "purple" => 0x800080,
        "fuchsia" => 0xff00ff,
        "green" => 0x008000,
        "lime" => 0x00ff00,
        "olive" => 0x808000,
        "yellow" => 0xffff00,
        "navy" => 0x000080,
        "blue" => 0x0000ff,
        "teal" => 0x008080,
        "aqua" => 0x00ffff,
        _ => return None,
    };
    Some([16, 8, 0].map(|shift| ((hex >> shift) & 0xff) as f32 / 255.0))
}

/// A node of the parsed document.
struct Node {
    kind: Kind,
    parent: RefCell<Option<Weak<Self>>>,
    children: RefCell<Vec<Rc<Self>>>,
    /// How many elements it is in, when it was added.
    depth: Cell<usize>,
}

enum Kind {
    Document,
    Element {
        name: QualName,
        attrs: RefCell<Vec<Attribute>>,
        /// The names of `attrs`, once attributes of a repeated tag are merged in.
        names: RefCell<Option<FxHashSet<QualName>>>,
    },
    Text(RefCell<String>),
    /// A comment or processing instruction.
    Other,
}

impl Node {
    fn new(kind: Kind) -> Rc<Self> {
        Rc::new(Self {
            kind,
            parent: RefCell::new(None),
            children: RefCell::new(Vec::new()),
            depth: Cell::new(0),
        })
    }

    fn parent(&self) -> Option<Rc<Self>> {
        self.parent.borrow().as_ref()?.upgrade()
    }
}

impl Drop for Node {
    // Without recursion, however deep the tree.
    fn drop(&mut self) {
        let mut nodes = std::mem::take(self.children.get_mut());
        while let Some(node) = nodes.pop() {
            if let Ok(mut node) = Rc::try_unwrap(node) {
                nodes.append(node.children.get_mut());
            }
        }
    }
}

/// The document the parser builds, which turns unsupported once it holds an
/// element that is not drawn, more than `max_nodes` nodes, or elements nested
/// deeper than [`MAX_DEPTH`].
struct Dom {
    document: Rc<Node>,
    nodes: Cell<usize>,
    max_nodes: usize,
    unsupported: Cell<bool>,
    /// The attribute names looked at to merge those of repeated tags.
    names_looked_at: Cell<usize>,
}

impl Dom {
    fn added(&self, node: &Rc<Node>, parent: &Rc<Node>) {
        *node.parent.borrow_mut() = Some(Rc::downgrade(parent));
        node.depth.set(parent.depth.get() + 1);
        if node.depth.get() > MAX_DEPTH {
            self.unsupported.set(true);
        }
    }

    fn looked_at(&self, names: usize) {
        self.names_looked_at.set(self.names_looked_at.get() + names);
    }

    fn counted(&self, node: Rc<Node>) -> Rc<Node> {
        self.nodes.set(self.nodes.get() + 1);
        if self.nodes.get() > self.max_nodes {
            self.unsupported.set(true);
        }
        node
    }

    fn text(&self, text: &str) -> Rc<Node> {
        self.counted(Node::new(Kind::Text(RefCell::new(text.to_owned()))))
    }
}

impl TreeSink for Dom {
    type Handle = Rc<Node>;
    type Output = Self;
    type ElemName<'a> = ExpandedName<'a>;

    fn finish(self) -> Self {
        self
    }

    fn parse_error(&self, _: Cow<'static, str>) {}

    fn get_document(&self) -> Rc<Node> {
        self.document.clone()
    }

    fn elem_name<'a>(&'a self, target: &'a Rc<Node>) -> ExpandedName<'a> {
        match &target.kind {
            Kind::Element { name, .. } => name.expanded(),
            _ => unreachable!("the parser asks only elements for their names"),
        }
    }

    fn create_element(
        &self,
        name: QualName,
        attrs: Vec<Attribute>,
        flags: ElementFlags,
    ) -> Rc<Node> {
        let html = name.ns == ns!(html);
        let known = SHOWN.iter().chain(&HIDDEN).any(|n| *n == &*name.local);
        if !html || !known || flags.template {
            self.unsupported.set(true);
        }
        self.counted(Node::new(Kind::Element {
            name,
            attrs: RefCell::new(attrs),
            names: RefCell::new(None),
        }))
    }

    fn create_comment(&self, _: StrTendril) -> Rc<Node> {
        Node::new(Kind::Other)
    }

    fn create_pi(&self, _: StrTendril, _: StrTendril) -> Rc<Node> {
        Node::new(Kind::Other)
    }

    fn append(&self, parent: &Rc<Node>, child: NodeOrText<Rc<Node>>) {
        let node = match child {
            NodeOrText::AppendNode(node) => node,
            NodeOrText::AppendText(text) => {
                if let Some(last) = parent.children.borrow().last()
                    && let Kind::Text(t) = &last.kind
                {
                    t.borrow_mut().push_str(&text);
                    return;
                }
                self.text(&text)
            }
        };
        self.added(&node, parent);
        parent.children.borrow_mut().push(node);
    }

    fn append_based_on_parent_node(
        &self,
        element: &Rc<Node>,
        prev_element: &Rc<Node>,
        child: NodeOrText<Rc<Node>>,
    ) {
        if element.parent().is_some() {
            self.append_before_sibling(element, child);
        } else {
            self.append(prev_element, child);
        }
    }

    fn append_doctype_to_document(&self, _: StrTendril, _: StrTendril, _: StrTendril) {}

    fn get_template_contents(&self, target: &Rc<Node>) -> Rc<Node> {
        // Templates are unsupported; the parser still needs a node.
        self.unsupported.set(true);
        target.clone()
    }

    fn same_node(&self, x: &Rc<Node>, y: &Rc<Node>) -> bool {
        Rc::ptr_eq(x, y)
    }

    fn set_quirks_mode(&self, _: QuirksMode) {}

    fn append_before_sibling(&self, sibling: &Rc<Node>, child: NodeOrText<Rc<Node>>) {
        let Some(parent) = sibling.parent() else {
            return;
        };
        let mut children = parent.children.borrow_mut();
        let Some(at) = children.iter().rposition(|c| Rc::ptr_eq(c, sibling)) else {
            return;
        };
        let node = match child {
            NodeOrText::AppendNode(node) => node,
            NodeOrText::AppendText(text) => {
                if let Some(before) = at.checked_sub(1).map(|i| &children[i])
                    && let Kind::Text(t) = &before.kind
                {
                    t.borrow_mut().push_str(&text);
                    return;
                }
                self.text(&text)
            }
        };
        self.added(&node, &parent);
        children.insert(at, node);
    }

    // A repeated `<html>` or `<body>` adds the attributes the element lacks; the
    // first of a name stays.
    fn add_attrs_if_missing(&self, target: &Rc<Node>, attrs: Vec<Attribute>) {
        let Kind::Element {
            attrs: existing,
            names,
            ..
        } = &target.kind
        else {
            return;
        };
        let mut existing = existing.borrow_mut();
        let mut names = names.borrow_mut();
        let names = names.get_or_insert_with(|| {
            self.looked_at(existing.len());
            existing.iter().map(|a| a.name.clone()).collect()
        });
        self.looked_at(attrs.len());
        for attr in attrs {
            if names.insert(attr.name.clone()) {
                existing.push(attr);
            }
        }
    }

    fn remove_from_parent(&self, target: &Rc<Node>) {
        if let Some(parent) = target.parent() {
            let mut children = parent.children.borrow_mut();
            if let Some(at) = children.iter().rposition(|c| Rc::ptr_eq(c, target)) {
                children.remove(at);
            }
        }
        *target.parent.borrow_mut() = None;
    }

    fn reparent_children(&self, node: &Rc<Node>, new_parent: &Rc<Node>) {
        let moved = std::mem::take(&mut *node.children.borrow_mut());
        for child in &moved {
            self.added(child, new_parent);
        }
        new_parent.children.borrow_mut().extend(moved);
    }
}

#[cfg(test)]
mod tests {
    use super::super::Unsupported;
    use super::super::out::Color;
    use super::super::text::{Align, Family, Style, Typeface};
    use super::{Kind, Node, css_style, parsed, rich_text};
    use std::rc::Rc;

    /// The attributes of the first `name` element under `node`, as `name=value`.
    fn attributes(node: &Rc<Node>, name: &str) -> Option<Vec<String>> {
        if let Kind::Element { name: n, attrs, .. } = &node.kind
            && &*n.local == name
        {
            return Some(
                attrs
                    .borrow()
                    .iter()
                    .map(|a| format!("{}={}", &*a.name.local, &*a.value))
                    .collect(),
            );
        }
        node.children
            .borrow()
            .iter()
            .find_map(|c| attributes(c, name))
    }

    /// A repeated `<body>` or `<html>` adds the attributes its element lacks, the
    /// first of a name staying, at a cost that grows with their number, not its
    /// square.
    #[test]
    fn repeated_tags_merge_attributes_in_linear_work() {
        for tag in ["body", "html"] {
            let dom = parsed(&format!("<{tag} a=1><{tag} a=2 b=3><p>A</p>")).expect("supported");
            assert_eq!(
                attributes(&dom.document, tag).expect("element"),
                ["a=1", "b=3"],
                "{tag}"
            );
            for n in [1_000, 10_000] {
                let tags: String = (0..n).map(|i| format!("<{tag} a{i}=x>")).collect();
                let dom = parsed(&format!("<{tag}>{tags}<p>A</p>")).expect("supported");
                assert_eq!(attributes(&dom.document, tag).expect("element").len(), n);
                let looked_at = dom.names_looked_at.get();
                assert!(looked_at <= 2 * n, "{tag} {n}: {looked_at}");
            }
        }
    }

    fn base() -> Style {
        Style {
            face: Family::Text.into(),
            size: 12.0,
            color: Color::BLACK,
            align: Align::Left,
        }
    }

    const RED: Color = Color::Rgb([1.0, 0.0, 0.0]);

    fn text(xhtml: &str) -> String {
        let spans = rich_text(xhtml, &base()).expect("supported");
        spans.iter().map(|s| s.text.as_str()).collect()
    }

    /// Each span's text with its face, size, colour and alignment.
    fn styled(xhtml: &str) -> Vec<(String, Typeface, f32, Color, Align)> {
        rich_text(xhtml, &base())
            .expect("supported")
            .into_iter()
            .map(|s| {
                let Style {
                    face,
                    size,
                    color,
                    align,
                } = s.style;
                (s.text, face, size, color, align)
            })
            .collect()
    }

    const fn face(family: Family, bold: bool, italic: bool) -> Typeface {
        Typeface {
            family,
            bold,
            italic,
        }
    }

    const SANS: Typeface = face(Family::Sans, false, false);
    const SANS_BOLD: Typeface = face(Family::Sans, true, false);

    /// A paragraph starts and ends a line; spaces and line breaks in the text stay.
    #[test]
    fn whitespace_stays_and_paragraphs_break_lines() {
        assert_eq!(
            text(
                "<?xml version=\"1.0\"?><body style=\"color:#f00\"><p>A</p>  <p>  B  </p>\
                 <span>C</span> D<br/>E &amp;\n F</body>"
            ),
            "A\n  \n  B  \nC D\nE &\n F"
        );
        // Unclosed and empty paragraphs, and a break after a carriage return.
        assert_eq!(text("<p>A<p>B<p></p><p>C&#13;</p>D"), "A\nB\nC\r\nD");
    }

    /// Comments end at `-->` and attribute values at their quote, whatever `>`
    /// they hold; CDATA sections and internal subsets are not read, as in HTML.
    #[test]
    fn markup_is_read_as_html() {
        let red = |t: &str| (t.to_owned(), SANS, 12.0, RED, Align::Left);
        let black = |t: &str| (t.to_owned(), SANS, 12.0, Color::BLACK, Align::Left);
        assert_eq!(
            styled(
                "<body style=\"font-family:Helvetica\"><p>A<!-- > MMMM --><span \
                 title='a>b' style=\"color:#ff0000\">IMPORTANT</span></p></body>"
            ),
            [black("A"), red("IMPORTANT"), black("\n")]
        );
        assert_eq!(text("<p>A<![CDATA[<b>x</b> & y]]>Z</p>"), "Ax & y]]>Z\n");
        assert_eq!(
            text("<!DOCTYPE body [<!ENTITY foo \"FOO\">]><p>a &foo; b</p>"),
            "]>\na &foo; b\n"
        );
    }

    /// Character references as HTML reads them: named ones of HTML, numeric ones,
    /// and an ampersand that starts none as itself.
    #[test]
    fn character_references_are_decoded() {
        assert_eq!(
            text("<p>A&nbsp;B caf&eacute; &mdash; &#65;&#x42;&#1046; A & B &amp C &zz; D</p>"),
            "A\u{a0}B café — ABЖ A & B & C &zz; D\n"
        );
    }

    /// Unclosed and misnested formatting elements continue as HTML continues them,
    /// and text outside the body takes its style.
    #[test]
    fn elements_nest_as_in_html() {
        let lines = |xhtml: &str| -> Vec<(String, bool)> {
            styled(&format!(
                "<body style=\"font-family:Helvetica\">{xhtml}</body>"
            ))
            .into_iter()
            .map(|(t, f, ..)| (t, f.bold))
            .collect()
        };
        let s = |t: &str, bold| (t.to_owned(), bold);
        assert_eq!(
            lines("<p>A<b>B<p>C"),
            [
                s("A", false),
                s("B", true),
                s("\n", false),
                s("C", true),
                s("\n", false)
            ]
        );
        assert_eq!(
            lines("<b>A<p>B</b>C</p>"),
            [s("A", true), s("\n", false), s("B", true), s("C\n", false)]
        );
        assert_eq!(
            styled("Pre<body style=\"font-family:Helvetica\"><p>A</p></body>Post")[0].1,
            SANS
        );
        // What head, style and script elements hold is not drawn.
        assert_eq!(
            text(
                "<html><head><title>T</title></head><body><p>A<style>p{color:red}</style>B<script>x</script>C</p></body></html>"
            ),
            "ABC\n"
        );
    }

    /// Styles nest; each element's style applies to what it holds.
    #[test]
    fn styles_nest() {
        let got: Vec<(String, Typeface, f32, Color)> = styled(
            "<body style=\"font-size:10pt\"><p style=\"font-family:Helvetica\">A \
             <span style=\"color:#ff0000\">B</span> C</p><p>D</p></body>",
        )
        .into_iter()
        .map(|(t, f, s, c, _)| (t, f, s, c))
        .collect();
        let text_face = Typeface::from(Family::Text);
        assert_eq!(
            got,
            [
                ("A ".to_owned(), SANS, 10.0, Color::BLACK),
                ("B".to_owned(), SANS, 10.0, RED),
                (" C\n".to_owned(), SANS, 10.0, Color::BLACK),
                ("D\n".to_owned(), text_face, 10.0, Color::BLACK),
            ]
        );
    }

    /// Families map by the first word of each name, in order; others, and no family,
    /// set the default face.
    #[test]
    fn families_map_by_their_first_word() {
        let family = |name: &str| {
            styled(&format!("<p style=\"font-family:{name}\">x</p>"))[0]
                .1
                .family
        };
        for (name, expected) in [
            ("Helvetica", Family::Sans),
            ("HELVETICA", Family::Sans),
            ("Helvetica Neue", Family::Sans),
            ("Arial Black", Family::Sans),
            ("'Arial'", Family::Sans),
            ("Foo, sans-serif", Family::Sans),
            ("cursive, Helvetica", Family::Sans),
            ("Times New Roman", Family::Serif),
            ("Times New Roman Cyr", Family::Serif),
            ("Courier New", Family::Mono),
            ("monospace,serif", Family::Mono),
            ("serif, Helvetica", Family::Text),
            ("Helv", Family::Text),
            ("ArialMT", Family::Text),
            ("Times-Roman", Family::Text),
            ("TimesNewRomanPSMT", Family::Text),
            ("Helvetica-Bold", Family::Text),
            ("Verdana", Family::Text),
        ] {
            assert_eq!(family(name), expected, "{name}");
        }
        assert_eq!(styled("<p>x</p>")[0].1, Typeface::from(Family::Text));
    }

    /// Bold and italic come from weights above normal, italic and oblique, the
    /// `font` shorthand in any order, and `b`, `strong`, `i` and `em`.
    #[test]
    fn weight_and_slant_set_the_face() {
        let face_of = |xhtml: &str| styled(xhtml)[0].1;
        for (css, bold, italic) in [
            ("font-weight:bold", true, false),
            ("font-weight:bolder", true, false),
            ("font-weight:500", true, false),
            ("font-weight:401", true, false),
            ("font-weight:400", false, false),
            ("font-weight:normal", false, false),
            ("font-style:italic", false, true),
            ("font-style:oblique", false, true),
            ("font:bold italic 14pt Helvetica", true, true),
            ("font:italic bold 14pt Times", true, true),
        ] {
            let got = face_of(&format!("<p style=\"font-family:Helvetica;{css}\">x</p>"));
            assert_eq!((got.bold, got.italic), (bold, italic), "{css}");
        }
        assert_eq!(face_of("<b>x</b>"), face(Family::Text, true, false));
        assert_eq!(
            face_of("<strong><em>x</em></strong>"),
            face(Family::Text, true, true)
        );
        assert_eq!(face_of("<i>x</i>"), face(Family::Text, false, true));
        // The shorthand resets weight, slant and family it does not give.
        let reset = "<body style=\"font-family:Helvetica;font-weight:bold;font-style:italic\">";
        assert_eq!(
            face_of(&format!("{reset}<p style=\"font:14pt Courier\">x</p>")),
            face(Family::Mono, false, false)
        );
        assert_eq!(
            face_of(&format!("{reset}<p style=\"font:italic 14pt\">x</p>")),
            face(Family::Text, false, true)
        );
    }

    /// Sizes in points, pixels (as points), em and percent of the parent's size.
    #[test]
    fn sizes_are_points_or_relative() {
        let size = |css: &str| {
            styled(&format!(
                "<body style=\"font-size:20pt\"><p style=\"{css}\">x</p></body>"
            ))[0]
                .2
        };
        assert_eq!(size("font-size:12.5pt"), 12.5);
        assert_eq!(size("font-size:16px"), 16.0);
        assert_eq!(size("font-size:20PX"), 20.0);
        assert_eq!(size("font-size:+14pt"), 14.0);
        assert_eq!(size("font-size:150%"), 30.0);
        assert_eq!(size("font-size:.5em"), 10.0);
        assert_eq!(size("font:Helvetica"), 20.0);
    }

    /// The default style of Acrobat's annotations, its family before its size.
    #[test]
    fn a_default_style_sets_the_base() {
        let style = css_style(
            "font: Helvetica,sans-serif 12.0pt; text-align:left; color:#FF0000",
            base(),
        )
        .unwrap();
        assert_eq!(
            (style.face, style.size, style.color, style.align),
            (SANS, 12.0, RED, Align::Left)
        );
        let style = css_style("font: bold Helvetica 18pt; text-align:center", base()).unwrap();
        assert_eq!(
            (style.face, style.size, style.align),
            (SANS_BOLD, 18.0, Align::Center)
        );
    }

    /// A paragraph's lines are aligned by its `text-align`, inherited; an inline
    /// element's own does not count.
    #[test]
    fn paragraphs_align_by_text_align() {
        let aligns: Vec<(String, Align)> = styled(
            "<body style=\"text-align:center\"><p>A</p><p style=\"text-align:right\">B\
             <span style=\"text-align:left\">C</span></p><p style=\"text-align:justify\">D</p></body>",
        )
        .into_iter()
        .map(|(t, .., a)| (t, a))
        .collect();
        assert_eq!(
            aligns,
            [
                ("A\n".to_owned(), Align::Center),
                ("BC\n".to_owned(), Align::Right),
                ("D\n".to_owned(), Align::Justify),
            ]
        );
    }

    /// Acrobat's rich text, with the declarations and attributes it writes.
    #[test]
    fn acrobat_rich_text_is_drawn() {
        let spans = styled(
            "<?xml version=\"1.0\"?><body xmlns=\"http://www.w3.org/1999/xhtml\" \
             xmlns:xfa=\"http://www.xfa.org/schema/xfa-data/1.0/\" xfa:APIVersion=\"Acrobat:7.0.8\" \
             xfa:spec=\"2.0.2\" style=\"font-size:12.0pt;text-align:left;color:#000000;\
             font-weight:normal;font-style:normal;font-family:Arial;font-stretch:normal\">\
             <p dir=\"ltr\"><span style=\"font-family:Times New Roman;font-size:12.0pt;\
             font-weight:700;xfa-spacerun:yes\">  &#1050;&#1053;&#1048;&#1043;&#1040; 1&#13;</span></p></body>",
        );
        assert_eq!(spans[0].0, "  КНИГА 1\r");
        assert_eq!(spans[0].1, face(Family::Serif, true, false));
    }

    /// Elements, attributes and declarations drawn otherwise than the generator
    /// can make the appearance unsupported, as do names the face maps to symbols.
    #[test]
    fn what_cannot_be_drawn_as_viewers_do_is_unsupported() {
        for xhtml in [
            "<h1>x</h1>",
            "<p>a<u>b</u></p>",
            "<p>a<sub>b</sub></p>",
            "<font color=red>x</font>",
            "<table><tr><td>x</td></tr></table>",
            "<p align=\"center\">x</p>",
            "<p dir=\"rtl\">x</p>",
            "<p style=\"text-decoration:underline\">x</p>",
            "<p style=\"vertical-align:super\">x</p>",
            "<p style=\"margin-left:30pt\">x</p>",
            "<p style=\"line-height:3\">x</p>",
            "<p style=\"font-family:Symbol\">x</p>",
            "<p style=\"font-family:ZapfDingbats\">x</p>",
            "<p style=\"font-family:Times,\">x</p>",
            "<p style=\"COLOR:#ff0000\">x</p>",
            "<p style=\"font-size:20pt; color\">x</p>",
            "<p style=\"font-size:x-large\">x</p>",
            "<p style=\"font-size:0pt\">x</p>",
            "<p style=\"font-weight:BOLD\">x</p>",
            "<p style=\"color:rgba(255,0,0,0.5)\">x</p>",
            "<p style=\"text-align:CENTER\">x</p>",
            "<svg><text>x</text></svg>",
        ] {
            assert_eq!(
                rich_text(xhtml, &base()).err(),
                Some(Unsupported::RichText),
                "{xhtml}"
            );
        }
        // Hidden by its style: nothing drawn, but supported.
        assert_eq!(
            text("<p>A<span style=\"display:none\">B</span>C</p>"),
            "AC\n"
        );
    }

    /// Nesting beyond what written rich text does is unsupported, before the
    /// parser's work outgrows the text.
    #[test]
    fn deep_nesting_is_unsupported() {
        let deep = format!("{}x", "<span>".repeat(300));
        assert_eq!(rich_text(&deep, &base()).err(), Some(Unsupported::RichText));
        let shallow = format!("{}x", "<span>".repeat(200));
        assert_eq!(text(&shallow), "x");
    }
}
