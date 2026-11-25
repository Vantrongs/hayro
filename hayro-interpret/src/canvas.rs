use crate::encode::{
    Cost, EncodedShadingPattern, Exhausted, Meter, SamplePixel, capacity_after, push_metered,
    reserve_metered, take,
};
use crate::gradient::ParameterColors;
use kurbo::{Point, Rect};

pub(crate) struct Canvas<'a, P = [f32; 4]> {
    pub(crate) width: usize,
    pub(crate) height: usize,
    bounds: Rect,
    layout: Layout<'a>,
    pub(crate) pixels: Vec<P>,
}

impl<'a, P: SamplePixel> Canvas<'a, P> {
    pub(crate) fn new(width: u32, height: u32, background: P) -> Self {
        let (width, height) = (width as usize, height as usize);

        Self {
            width,
            height,
            bounds: Rect::new(0., 0., width as f64, height as f64),
            layout: Layout::Dense,
            pixels: vec![background; width * height],
        }
    }

    pub(crate) fn for_rows(width: u32, height: u32, background: P) -> Self {
        Self {
            width: width as usize,
            height: height as usize,
            pixels: vec![background; width as usize],
            bounds: Rect::new(0., 0., f64::from(width), f64::from(height)),
            layout: Layout::Rows {
                rows: Rows::default(),
                shade: RowShade::Rgba,
            },
        }
    }

    pub(crate) fn set_row_shade(&mut self, shading: RowShade<'a>) {
        if let Layout::Rows { shade, .. } = &mut self.layout {
            *shade = shading;
        }
    }

    fn make_dense(&mut self, meter: &mut dyn Meter) -> Result<Rows, Exhausted> {
        let len = self.width.checked_mul(self.height).ok_or(Exhausted)?;
        let bytes = len
            .checked_mul(size_of::<P>())
            .filter(|&n| n <= isize::MAX as usize)
            .ok_or(Exhausted)?;
        if !meter.hold(bytes) {
            return Err(Exhausted);
        }
        take(meter, Cost::of(len))?;
        let Layout::Rows { rows, .. } = std::mem::replace(&mut self.layout, Layout::Dense) else {
            unreachable!()
        };
        let old_bytes = self.pixels.capacity() * size_of::<P>();
        self.pixels.resize(len, self.pixels[0]);
        meter.release_bytes(old_bytes);
        Ok(rows)
    }

    pub(crate) fn emit_rows(
        mut self,
        background: P,
        meter: &mut dyn Meter,
        mut emit: impl FnMut(&mut [P]),
    ) -> Result<(), Exhausted> {
        let Layout::Rows { mut rows, shade } = self.layout else {
            for row in self.pixels.chunks_exact_mut(self.width) {
                emit(row);
            }
            return Ok(());
        };
        take(meter, crate::function::sorting(Cost::of(rows.spans.len())))?;
        rows.spans.sort_unstable_by_key(|s| (s.y, s.order));
        let bytes = rows.bytes_after(0, 0);
        let mut spans = rows.spans.iter().peekable();
        for y in 0..self.height {
            self.pixels.fill(background);
            while let Some(span) = spans.next_if(|s| s.y as usize == y) {
                let pixels = &mut self.pixels[span.start as usize..span.end as usize];
                match &shade {
                    RowShade::Rgba => {
                        rows.paint(span, pixels, &|v: &[f32; 4]| P::evaluated(Some(*v)));
                    }
                    RowShade::Parameter(lut) => {
                        rows.paint(span, pixels, &|v: &[f32; 1]| P::evaluated(lut.color(v[0])));
                    }
                    RowShade::Direct {
                        pattern,
                        num_components,
                    } => match num_components {
                        1 => rows.paint(span, pixels, &|v: &[f32; 1]| {
                            P::evaluated(Some(pattern.convert(v)))
                        }),
                        2 => rows.paint(span, pixels, &|v: &[f32; 2]| {
                            P::evaluated(Some(pattern.convert(v)))
                        }),
                        3 => rows.paint(span, pixels, &|v: &[f32; 3]| {
                            P::evaluated(Some(pattern.convert(v)))
                        }),
                        4 => rows.paint(span, pixels, &|v: &[f32; 4]| {
                            P::evaluated(Some(pattern.convert(v)))
                        }),
                        _ => rows.paint(
                            span,
                            pixels,
                            &|v: &[f32; crate::function::MAX_COMPONENTS]| {
                                P::evaluated(Some(pattern.convert(&v[..*num_components])))
                            },
                        ),
                    },
                }
            }
            emit(&mut self.pixels);
        }
        drop(rows);
        meter.release_bytes(bytes);
        Ok(())
    }

    pub(crate) fn for_points(
        points: &[Point],
        background: P,
        meter: &mut dyn Meter,
    ) -> Result<Self, Exhausted> {
        let bytes = points
            .len()
            .checked_mul(size_of::<(Point, usize)>() + size_of::<P>())
            .ok_or(Exhausted)?;
        if !meter.hold(bytes) {
            return Err(Exhausted);
        }
        take(
            meter,
            crate::function::sorting(Cost::of(points.len())) + Cost::of(points.len()),
        )?;
        let area = bounds(points);
        let mut locations: Vec<_> = points.iter().copied().zip(0..).collect();
        let by_x = area.width() > area.height();
        locations.sort_unstable_by(|(a, _), (b, _)| {
            Sparse::axis(*a, by_x).total_cmp(&Sparse::axis(*b, by_x))
        });
        Ok(Self {
            width: 0,
            height: 0,
            pixels: vec![background; points.len()],
            bounds: area,
            layout: Layout::Points(Sparse { locations, by_x }),
        })
    }

    pub(crate) fn bounds(&self) -> Rect {
        self.bounds
    }

    pub(crate) fn is_visible(&self, points: &[Point]) -> bool {
        bounds(points).overlaps(self.bounds())
    }

    pub(crate) fn fill_triangle<const N: usize>(
        &mut self,
        points: [Point; 3],
        values: [[f32; N]; 3],
        shade: &impl Fn(&[f32; N]) -> P,
        sample_cost: Cost,
        meter: &mut dyn Meter,
    ) -> Result<(), Exhausted> {
        let [p0, p1, p2] = points;
        let (e1, e2) = (p1 - p0, p2 - p0);
        let det = e1.x * e2.y - e1.y * e2.x;

        if det == 0.0 || !det.is_finite() {
            return Ok(());
        }

        let coefficients: [[f64; 3]; N] = std::array::from_fn(|c| {
            let v0 = f64::from(values[0][c]);
            let d1 = f64::from(values[1][c]) - v0;
            let d2 = f64::from(values[2][c]) - v0;
            [
                v0,
                (d1 * e2.y - d2 * e1.y) / det,
                (d2 * e1.x - d1 * e2.x) / det,
            ]
        });
        let mut sorted = points;
        sorted.sort_by(|a, b| a.y.total_cmp(&b.y).then(a.x.total_cmp(&b.x)));
        let [top, middle, bottom] = sorted;

        let span = |y| {
            let long = edge_x(top, bottom, y);
            let short = if y < middle.y {
                edge_x(top, middle, y)
            } else {
                edge_x(middle, bottom, y)
            };
            long.min(short)..long.max(short)
        };
        if let Layout::Points(sparse) = &self.layout {
            let area = bounds(&points);
            let (lo, hi) = if sparse.by_x {
                (area.x0, area.x1)
            } else {
                (area.y0, area.y1)
            };
            take(
                meter,
                Cost::new(2 * u64::from(sparse.locations.len().max(1).ilog2() + 1)),
            )?;
            if !lo.is_finite() || !hi.is_finite() {
                return Ok(());
            }
            let lower = sparse
                .locations
                .partition_point(|(p, _)| Sparse::axis(*p, sparse.by_x) < lo);
            let upper = sparse
                .locations
                .partition_point(|(p, _)| Sparse::axis(*p, sparse.by_x) <= hi);
            take(
                meter,
                (sample_cost + Cost::of(N + 4)).times((upper - lower) as u64),
            )?;
            for &(point, index) in &sparse.locations[lower..upper] {
                if (top.y..bottom.y).contains(&point.y) && span(point.y).contains(&point.x) {
                    let d = point - p0;
                    self.pixels[index] = shade(&PlaneRow::new(&coefficients, d.y).at(d.x));
                }
            }
            return Ok(());
        }

        let first_row = pixel_index(top.y, self.height);
        let end_row = pixel_index(bottom.y, self.height);

        take(meter, Cost::of(end_row - first_row))?;
        let mut plane = None;
        for y in first_row..end_row {
            let center_y = center(y);
            let range = span(center_y);
            let start = pixel_index(range.start, self.width);
            let end = pixel_index(range.end, self.width);

            if start >= end {
                continue;
            }

            take(
                meter,
                (sample_cost + Cost::of(N + 1)).times((end - start) as u64),
            )?;
            // When pending spans would take more space than the image, paint in place.
            let dense_bytes = self
                .width
                .saturating_mul(self.height)
                .saturating_mul(size_of::<P>());
            if let Layout::Rows {
                rows,
                shade: row_shade,
            } = &mut self.layout
            {
                let add = usize::from(plane.is_none());
                let count = match row_shade {
                    RowShade::Direct { num_components, .. } => *num_components,
                    _ => N,
                };
                if rows.bytes_after(1, add * count) <= dense_bytes {
                    let index = match plane {
                        Some(index) => index,
                        None => {
                            let index = compact(rows.planes.len())?;
                            let offset = rows.data.len();
                            push_metered(&mut rows.planes, Plane { p0, offset }, meter)?;
                            rows.components = count;
                            reserve_metered(&mut rows.data, count, meter)?;
                            rows.data.extend_from_slice(&coefficients[..count]);
                            plane = Some(index);
                            index
                        }
                    };
                    let span = Span {
                        order: compact(rows.spans.len())?,
                        y: compact(y)?,
                        start: compact(start)?,
                        end: compact(end)?,
                        plane: index,
                    };
                    push_metered(&mut rows.spans, span, meter)?;
                    continue;
                }
                let rows = self.make_dense(meter)?;
                for span in &rows.spans {
                    let offset = span.y as usize * self.width;
                    rows.paint(
                        span,
                        &mut self.pixels[offset + span.start as usize..offset + span.end as usize],
                        shade,
                    );
                }
                let bytes = rows.bytes_after(0, 0);
                drop(rows);
                meter.release_bytes(bytes);
            }
            PlaneRow::new(&coefficients, center_y - p0.y).paint(
                start,
                p0.x,
                &mut self.pixels[y * self.width + start..y * self.width + end],
                shade,
            );
        }
        Ok(())
    }
}

enum Layout<'a> {
    Dense,
    Points(Sparse),
    Rows { rows: Rows, shade: RowShade<'a> },
}

pub(crate) enum RowShade<'a> {
    Rgba,
    Parameter(ParameterColors<'a>),
    Direct {
        pattern: &'a EncodedShadingPattern,
        num_components: usize,
    },
}

#[derive(Default)]
struct Rows {
    components: usize,
    spans: Vec<Span>,
    planes: Vec<Plane>,
    data: Vec<[f64; 3]>,
}

impl Rows {
    fn bytes_after(&self, spans: usize, coefficients: usize) -> usize {
        fn bytes<T>(v: &Vec<T>, additional: usize) -> usize {
            capacity_after(v, additional).saturating_mul(size_of::<T>())
        }
        bytes(&self.spans, spans)
            .saturating_add(bytes(&self.planes, usize::from(coefficients > 0)))
            .saturating_add(bytes(&self.data, coefficients))
    }

    fn paint<P, const N: usize>(
        &self,
        span: &Span,
        pixels: &mut [P],
        shade: &impl Fn(&[f32; N]) -> P,
    ) {
        let plane = &self.planes[span.plane as usize];
        let coefficients = &self.data[plane.offset..plane.offset + self.components];
        PlaneRow::new(coefficients, center(span.y as usize) - plane.p0.y).paint(
            span.start as usize,
            plane.p0.x,
            pixels,
            shade,
        );
    }
}

struct PlaneRow<const N: usize>([[f64; 2]; N]);

impl<const N: usize> PlaneRow<N> {
    fn new(coefficients: &[[f64; 3]], y: f64) -> Self {
        let mut row = [[0.; 2]; N];
        for (component, &[v, dx, dy]) in row.iter_mut().zip(coefficients) {
            *component = [v + dy * y, dx];
        }
        Self(row)
    }

    fn at(&self, x: f64) -> [f32; N] {
        self.0.map(|[base, dx]| (base + dx * x) as f32)
    }

    fn paint<P>(
        &self,
        start: usize,
        origin: f64,
        pixels: &mut [P],
        shade: &impl Fn(&[f32; N]) -> P,
    ) {
        for (x, pixel) in (start..).zip(pixels) {
            *pixel = shade(&self.at(center(x) - origin));
        }
    }
}

struct Plane {
    p0: Point,
    offset: usize,
}

struct Span {
    order: u32,
    y: u32,
    start: u32,
    end: u32,
    plane: u32,
}

fn compact(value: usize) -> Result<u32, Exhausted> {
    u32::try_from(value).map_err(|_| Exhausted)
}

struct Sparse {
    locations: Vec<(Point, usize)>,
    by_x: bool,
}

impl Sparse {
    fn axis(p: Point, x: bool) -> f64 {
        if x { p.x } else { p.y }
    }
}

fn center(index: usize) -> f64 {
    index as f64 + 0.5
}

fn pixel_index(coordinate: f64, len: usize) -> usize {
    ((coordinate - 0.5).ceil() as usize).min(len)
}

fn edge_x(a: Point, b: Point, y: f64) -> f64 {
    let (a, b) = if (a.y, a.x) <= (b.y, b.x) {
        (a, b)
    } else {
        (b, a)
    };

    a.x + (y - a.y) * (b.x - a.x) / (b.y - a.y)
}

pub(crate) fn bounds(points: &[Point]) -> Rect {
    points
        .iter()
        .fold(Rect::from_points(points[0], points[0]), |bounds, point| {
            bounds.union_pt(*point)
        })
}
