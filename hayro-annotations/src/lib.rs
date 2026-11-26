//! PDF annotation appearances.

mod rich;
pub use rich::{css_style, rich_text};

/// The font families generated text is set in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Family {
    /// Helvetica.
    Sans,
    /// Times.
    Serif,
    /// Courier.
    Mono,
    /// The face of rich text that names no family a viewer maps to a standard
    /// font (a default serif face).
    Text,
    /// `ZapfDingbats`: the check box mark, and text in its built-in encoding.
    Dingbats,
}

/// A family in a weight and slant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Typeface {
    /// Font family.
    pub family: Family,
    /// Bold face.
    pub bold: bool,
    /// Italic face.
    pub italic: bool,
}

impl From<Family> for Typeface {
    fn from(family: Family) -> Self {
        Self {
            family,
            bold: false,
            italic: false,
        }
    }
}

/// How large text is set: in points, or as a multiple of the size it is laid out at
/// (a default appearance's), as rich text's relative sizes are.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Size {
    /// Absolute size in points.
    Points(f32),
    /// Scale relative to the base size.
    Base(f32),
}

impl Size {
    /// The size at base size `base`.
    pub fn at(self, base: f32) -> f32 {
        match self {
            Self::Points(p) => p,
            Self::Base(k) => base * k,
        }
    }

    /// `n` times this size, as a size relative to it is.
    pub fn times(self, n: f32) -> Self {
        match self {
            Self::Points(p) => Self::Points(n * p),
            Self::Base(k) => Self::Base(n * k),
        }
    }

    /// `n` hundredths of this size.
    pub fn percent(self, n: f32) -> Self {
        match self {
            Self::Points(p) => Self::Points(n * p / 100.0),
            Self::Base(k) => Self::Base(n * k / 100.0),
        }
    }
}

/// How rich text is set: typeface, size, the fill colour, and how the lines of its
/// paragraph are aligned.
#[derive(Clone, Debug, PartialEq)]
pub struct Style {
    /// Font face.
    pub face: Typeface,
    /// Font size.
    pub size: Size,
    /// Fill color.
    pub color: Color,
    /// Paragraph alignment.
    pub align: Align,
}

/// How a line sits between the sides of its box.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Align {
    /// Align left.
    Left,
    /// Center the line.
    Center,
    /// Align right.
    Right,
    /// Stretched to both sides at its spaces, but for the last line of a paragraph,
    /// which is left aligned.
    Justify,
}

/// Rich text in one style.
#[derive(Debug)]
pub struct Span {
    /// Text of the run.
    pub text: String,
    /// Style of the run.
    pub style: Style,
}

/// Annotation paint color.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Color {
    /// Grayscale.
    Gray(f64),
    /// RGB.
    Rgb([f64; 3]),
}
impl Color {
    /// Black.
    pub const BLACK: Self = Self::Gray(0.0);
}
/// Reason for refusing to synthesize an appearance.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Unsupported {
    /// Unsupported rich text.
    RichText,
}
