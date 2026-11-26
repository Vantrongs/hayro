use crate::{Appearance, Glyph};
use hayro_interpret::color::Color;
use hayro_interpret::paint_path;
use hayro_interpret::{ClipPath, Device, DrawMode, DrawProps, FillRule, Paint, StrokeProps};
use kurbo::{Affine, BezPath, Cap, Join, Rect, Shape};
use std::fmt::{self, Write};
use std::rc::Rc;

#[derive(Clone, Debug)]
pub(crate) enum Mark {
    Move([f32; 2]),
    Line([f32; 2]),
    Curve([f32; 6]),
    Rect([f32; 4]),
    Close,
    Discard,
    Save,
    Restore,
    Clip,
    Transform([f32; 6]),
    Width(f32),
    Miter(f32),
    Round(bool),
    Color {
        stroke: bool,
        components: Ink,
    },
    Dash(Rc<[f32]>),
    Paint {
        close: bool,
        fill: bool,
        stroke: bool,
    },
    Glyph(Rc<Glyph>),
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum Ink {
    Gray([f32; 1]),
    Rgb([f32; 3]),
    Cmyk([f32; 4]),
}

impl Ink {
    pub(crate) fn components(&self) -> &[f32] {
        match self {
            Self::Gray(v) => v,
            Self::Rgb(v) => v,
            Self::Cmyk(v) => v,
        }
    }
    fn color(self, opacity: f32) -> Color {
        match self {
            Self::Gray(v) => Color::gray(v, opacity),
            Self::Rgb(v) => Color::rgb(v, opacity),
            Self::Cmyk(v) => Color::cmyk(v, opacity),
        }
    }
}

impl Mark {
    pub(crate) const FILL: Self = Self::Paint {
        close: false,
        fill: true,
        stroke: false,
    };
    pub(crate) const STROKE: Self = Self::Paint {
        close: false,
        fill: false,
        stroke: true,
    };
    pub(crate) const CLOSE_FILL: Self = Self::Paint {
        close: true,
        fill: true,
        stroke: false,
    };
    pub(crate) const CLOSE_STROKE: Self = Self::Paint {
        close: true,
        fill: false,
        stroke: true,
    };
    pub(crate) const CLOSE_FILL_STROKE: Self = Self::Paint {
        close: true,
        fill: true,
        stroke: true,
    };

    fn add_path(&self, path: &mut BezPath) -> bool {
        match *self {
            Self::Move([x, y]) => path.move_to((f64::from(x), f64::from(y))),
            Self::Line([x, y]) => path.line_to((f64::from(x), f64::from(y))),
            Self::Curve(v) => {
                let [x, y, u, w, a, b] = v.map(f64::from);
                path.curve_to((x, y), (u, w), (a, b));
            }
            Self::Rect(v) => {
                let [x, y, w, h] = v.map(f64::from);
                path.extend(Rect::new(x, y, x + w, y + h).path_elements(0.1));
            }
            Self::Close => path.close_path(),
            _ => return false,
        }
        true
    }
}

#[derive(Default, Debug)]
pub(crate) struct Marks(pub(crate) Vec<Mark>);

impl Marks {
    pub(crate) fn paints(&self) -> bool {
        self.0
            .iter()
            .any(|m| matches!(m, Mark::Paint { .. } | Mark::Glyph(_)))
    }
    pub(crate) fn outline(&self) -> BezPath {
        let mut path = BezPath::new();
        for m in &self.0 {
            m.add_path(&mut path);
        }
        path
    }
}

impl fmt::Display for Marks {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        for mark in &self.0 {
            let (v, op): (&[f32], &str) = match mark {
                Mark::Move(v) => (v, "m"),
                Mark::Line(v) => (v, "l"),
                Mark::Curve(v) => (v, "c"),
                Mark::Rect(v) => (v, "re"),
                Mark::Close => (&[], "h"),
                Mark::Discard => (&[], "n"),
                Mark::Save => (&[], "q"),
                Mark::Restore => (&[], "Q"),
                Mark::Clip => (&[], "W n"),
                Mark::Transform(v) => (v, "cm"),
                Mark::Width(v) => (std::slice::from_ref(v), "w"),
                Mark::Miter(v) => (std::slice::from_ref(v), "M"),
                Mark::Round(round) => (&[], if *round { "1 J 1 j" } else { "0 J 0 j" }),
                Mark::Color { stroke, components } => (
                    components.components(),
                    match (stroke, components) {
                        (true, Ink::Gray(_)) => "G",
                        (false, Ink::Gray(_)) => "g",
                        (true, Ink::Rgb(_)) => "RG",
                        (false, Ink::Rgb(_)) => "rg",
                        (true, Ink::Cmyk(_)) => "K",
                        (false, Ink::Cmyk(_)) => "k",
                    },
                ),
                Mark::Dash(v) => {
                    out.write_char('[')?;
                    for n in &**v {
                        write!(out, "{} ", f64::from(*n))?;
                    }
                    (&[], "] 0 d")
                }
                Mark::Glyph(g) => {
                    writeln!(out, "/{} Do", g.name())?;
                    continue;
                }
                Mark::Paint {
                    close,
                    fill,
                    stroke,
                } => (
                    &[],
                    match (*close, *fill, *stroke) {
                        (true, true, true) => "b",
                        (false, true, true) => "B",
                        (true, false, true) => "s",
                        (false, false, true) => "S",
                        (true, true, false) => "h f",
                        _ => "f",
                    },
                ),
            };
            for n in v {
                write!(out, "{} ", f64::from(*n))?;
            }
            writeln!(out, "{op}")?;
        }
        Ok(())
    }
}

#[derive(Clone)]
struct State {
    transform: Affine,
    fill: Color,
    stroke: Color,
    pen: StrokeProps,
    clips: usize,
}

impl Appearance {
    /// Serialize the appearance in PDF default user space.
    pub fn content(&self) -> String {
        self.marks.to_string()
    }

    /// Replay the appearance into a device in the supplied page transform.
    pub fn replay<'a>(&self, device: &mut impl Device<'a>, transform: Affine) {
        let black = Color::gray([0.0], self.opacity);
        let mut state = State {
            transform,
            fill: black.clone(),
            stroke: black,
            pen: StrokeProps::default(),
            clips: 0,
        };
        let mut saved = Vec::new();
        let mut path = BezPath::new();
        for m in &self.marks.0 {
            if m.add_path(&mut path) {
                continue;
            }
            let props = |color: &Color| DrawProps {
                transform: state.transform,
                paint: Paint::Color(color.clone()),
                soft_mask: None,
                blend_mode: self.blend,
            };
            match m {
                Mark::Discard => path.truncate(0),
                Mark::Save => saved.push(state.clone()),
                Mark::Restore => {
                    let old = saved.pop().expect("balanced generated graphics state");
                    for _ in old.clips..state.clips {
                        device.pop_clip();
                    }
                    state = old;
                }
                Mark::Clip => {
                    device.push_clip_path(&ClipPath {
                        path: state.transform * std::mem::take(&mut path),
                        fill: FillRule::NonZero,
                    });
                    state.clips += 1;
                }
                Mark::Transform(v) => state.transform *= Affine::new(v.map(f64::from)),
                Mark::Width(w) => state.pen.line_width = *w,
                Mark::Miter(m) => state.pen.miter_limit = *m,
                Mark::Round(round) => {
                    state.pen.line_join = if *round { Join::Round } else { Join::Miter };
                    state.pen.line_cap = if *round { Cap::Round } else { Cap::Butt };
                }
                Mark::Color { stroke, components } => {
                    if *stroke {
                        state.stroke = components.color(self.opacity);
                    } else {
                        state.fill = components.color(self.opacity);
                    }
                }
                Mark::Dash(v) => state.pen.dash_array = v.as_ref().into(),
                Mark::Glyph(g) => {
                    let [x, y, u, v] = g.bbox.map(f64::from);
                    device.push_clip_path(&ClipPath {
                        path: state.transform * Rect::new(x, y, u, v).to_path(0.1),
                        fill: FillRule::NonZero,
                    });
                    paint_path(
                        device,
                        &g.path,
                        props(&state.fill),
                        &DrawMode::Fill(FillRule::NonZero),
                    );
                    device.pop_clip();
                }
                Mark::Paint {
                    close,
                    fill,
                    stroke,
                } => {
                    if *close {
                        path.close_path();
                    }
                    if *fill {
                        paint_path(
                            device,
                            &path,
                            props(&state.fill),
                            &DrawMode::Fill(FillRule::NonZero),
                        );
                    }
                    if *stroke {
                        paint_path(
                            device,
                            &path,
                            props(&state.stroke),
                            &DrawMode::Stroke(state.pen.clone()),
                        );
                    }
                    path.truncate(0);
                }
                _ => unreachable!("path segment handled above"),
            }
        }
        for _ in 0..state.clips {
            device.pop_clip();
        }
    }
}
