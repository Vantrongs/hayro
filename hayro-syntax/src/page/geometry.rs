use super::*;

/// Effective page geometry in default user space.
#[derive(Debug, Clone, Copy)]
pub struct VisibleGeometry {
    /// Effective crop before scaling and rotation.
    pub visible: Rect,
    /// Clockwise quarter turn in degrees.
    pub rotation: u16,
    /// Points per default user space unit.
    pub user_unit: f64,
    /// Displayed width and height in points.
    pub size_pt: (f64, f64),
    /// First unreadable geometry operand.
    pub lost: Lost,
}

/// Failure to determine a finite page geometry.
#[derive(Debug, Clone, Copy)]
pub enum GeometryError {
    /// The repaired parent chain contains a cycle.
    ParentLoop,
    /// The visible page has no finite bounds.
    Unbounded,
}

pub(super) fn visible_geometry(
    media: Option<Rect>,
    crop: Option<Rect>,
    rotate: Option<Object<'_>>,
    user_unit: f64,
) -> Result<VisibleGeometry, GeometryError> {
    let infinite = |r: Rect| {
        [r.x0, r.y0, r.x1, r.y1] == [-2147483648., -2147483648., 2147483520., 2147483520.]
    };
    let media = media.unwrap_or(Rect::ZERO);
    let visible = match crop {
        Some(c) if infinite(media) => c,
        Some(c) if !infinite(c) => media.intersect(c),
        _ => media,
    };
    let visible = if !visible.width().is_finite()
        || !visible.height().is_finite()
        || visible.width() <= 0.
        || visible.height() <= 0.
    {
        Rect::new(0., 0., 612., 792.)
    } else {
        visible
    };
    let degrees = match rotate {
        Some(Object::Number(n)) if n.is_integer() => n.as_i64() as i32,
        Some(Object::Number(n)) => {
            let half = n.as_f64() + 0.5;
            if (-2147483648.0..2147483648.0).contains(&half) {
                let int = half as i32;
                int - i32::from(half < f64::from(int))
            } else {
                i32::MIN
            }
        }
        _ => 0,
    };
    let rotation = ((degrees.rem_euclid(360) + 45) / 90 % 4 * 90) as u16;
    let mut size_pt = (
        visible.width() * user_unit.abs(),
        visible.height() * user_unit.abs(),
    );
    if infinite(visible) || !size_pt.0.is_finite() || !size_pt.1.is_finite() {
        return Err(GeometryError::Unbounded);
    }
    if rotation % 180 != 0 {
        core::mem::swap(&mut size_pt.0, &mut size_pt.1);
    }
    Ok(VisibleGeometry {
        visible,
        rotation,
        user_unit,
        size_pt,
        lost: None,
    })
}

pub(super) struct Operands<'a> {
    xref: &'a XRef,
    numbers: Memo<ContentKey<'a>, Option<f64>>,
    units: Memo<ContentKey<'a>, f64>,
    boxes: Memo<ContentKey<'a>, (Option<Rect>, bool)>,
}

impl<'a> Operands<'a> {
    pub(super) fn new(xref: &'a XRef) -> Self {
        Self {
            xref,
            numbers: Memo::default(),
            units: Memo::default(),
            boxes: Memo::default(),
        }
    }

    pub(super) fn unit(&mut self, dict: &Dict<'a>) -> f64 {
        *self.units.get(self.xref, dict.content_key(), || {
            dict.get_raw(b"UserUnit")
                .and_then(|value| number(self.xref, &mut self.numbers, value))
                .unwrap_or(1.0)
        })
    }

    pub(super) fn rect(&mut self, value: Option<Object<'a>>) -> (Option<Rect>, bool) {
        match value {
            Some(Object::Array(array)) => *self.boxes.get(self.xref, array.content_key(), || {
                let mut items = array.raw_iter();
                let coords = [(); 4].map(|()| {
                    items
                        .next()
                        .and_then(|v| number(self.xref, &mut self.numbers, v))
                });
                let [x0, y0, x1, y1] = coords.map(|v| v.unwrap_or(0.0));
                (
                    Some(Rect::new(x0.min(x1), y0.min(y1), x0.max(x1), y0.max(y1))),
                    coords.iter().all(Option::is_some),
                )
            }),
            Some(_) => (Some(Rect::ZERO), false),
            None => (None, false),
        }
    }
}

fn number<'a>(
    xref: &'a XRef,
    numbers: &mut Memo<ContentKey<'a>, Option<f64>>,
    item: MaybeRef<Object<'a>>,
) -> Option<f64> {
    match item {
        MaybeRef::NotRef(Object::Number(n)) => Some(n.as_f64()),
        MaybeRef::Ref(r) => {
            let key = xref.content_key(r.into())?;
            *numbers.get(xref, key, || {
                match xref
                    .get_with::<MaybeRef<Object<'a>>>(r.into(), &ReaderContext::new(xref, false))?
                {
                    MaybeRef::NotRef(Object::Number(n)) => Some(n.as_f64()),
                    _ => None,
                }
            })
        }
        _ => None,
    }
}
