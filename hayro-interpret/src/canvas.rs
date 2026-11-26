use crate::encode::{Cost, Exhausted, Meter, take};
use crate::gradient::ColorLut;
use kurbo::{Point, Rect};

pub(crate) struct Canvas {
    pub(crate) width: usize,
    pub(crate) height: usize,
    layout: Layout,
    pub(crate) pixels: Vec<[f32; 4]>,
}

impl Canvas {
    pub(crate) fn new(width: u32, height: u32, background: [f32; 4]) -> Self {
        let (width, height) = (width as usize, height as usize);

        Self {
            width,
            height,
            layout: Layout::Dense,
            pixels: vec![background; width * height],
        }
    }

    pub(crate) fn for_rows(width: u32, height: u32, background: [f32; 4]) -> Self {
        Self {
            width: width as usize,
            height: height as usize,
            pixels: vec![background; width as usize],
            layout: Layout::Rows {
                spans: Vec::new(),
                lut: None,
            },
        }
    }

    pub(crate) fn finish_parameter_mesh(&mut self, colors: ColorLut) {
        if let Layout::Rows { lut, .. } = &mut self.layout {
            *lut = Some(colors);
        }
    }

    pub(crate) fn emit_rows(
        mut self,
        background: [f32; 4],
        meter: &mut dyn Meter,
        mut emit: impl FnMut(&[[f32; 4]]),
    ) -> Result<(), Exhausted> {
        let Layout::Rows { mut spans, lut } = self.layout else {
            for row in self.pixels.chunks_exact(self.width) {
                emit(row);
            }
            return Ok(());
        };
        take(meter, crate::function::sorting(Cost::of(spans.len())))?;
        spans.sort_unstable_by_key(|s| (s.y, s.order));
        let mut spans = spans.into_iter().peekable();
        for y in 0..self.height {
            self.pixels.fill(background);
            while spans.peek().is_some_and(|s| s.y == y) {
                let span = spans.next().unwrap();
                span.paint(&mut self.pixels, &|value: &[f32; 4]| {
                    lut.as_ref().map_or(*value, |lut| lut.get(value[0]))
                });
            }
            emit(&self.pixels);
        }
        Ok(())
    }

    pub(crate) fn for_points(
        points: &[Point],
        background: [f32; 4],
        meter: &mut dyn Meter,
    ) -> Result<Self, Exhausted> {
        let bytes = points
            .len()
            .checked_mul(size_of::<(Point, usize)>() + size_of::<[f32; 4]>())
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
            layout: Layout::Points(Sparse {
                area,
                locations,
                by_x,
            }),
        })
    }

    pub(crate) fn bounds(&self) -> Rect {
        match &self.layout {
            Layout::Points(s) => s.area,
            _ => Rect::new(0.0, 0.0, self.width as f64, self.height as f64),
        }
    }

    pub(crate) fn is_visible(&self, points: &[Point]) -> bool {
        bounds(points).overlaps(self.bounds())
    }

    fn prepare_span<const N: usize>(
        &mut self,
        shade: &impl Fn(&[f32; N]) -> [f32; 4],
        meter: &mut dyn Meter,
    ) -> Result<(), Exhausted> {
        let Layout::Rows { spans, .. } = &self.layout else {
            return Ok(());
        };
        if spans.len() < spans.capacity() {
            return Ok(());
        }
        let next = crate::encode::capacity_after(spans, 1).saturating_mul(size_of::<Span>());
        let Some(count) = self.width.checked_mul(self.height) else {
            return Ok(());
        };
        let Some(bytes) = count
            .checked_mul(size_of::<[f32; 4]>())
            .filter(|&bytes| bytes <= next)
        else {
            return Ok(());
        };
        take(meter, Cost::of(count))?;
        if !meter.hold(bytes) {
            return Err(Exhausted);
        }
        let row_bytes = self.pixels.capacity() * size_of::<[f32; 4]>();
        self.pixels
            .try_reserve_exact(count - self.pixels.len())
            .map_err(|_| Exhausted)?;
        self.pixels.resize(count, self.pixels[0]);
        meter.release_bytes(row_bytes);
        let Layout::Rows { spans, .. } = std::mem::replace(&mut self.layout, Layout::Dense) else {
            unreachable!()
        };
        let span_bytes = spans.capacity() * size_of::<Span>();
        for span in spans {
            let row = &mut self.pixels[span.y * self.width..][..self.width];
            span.paint(row, shade);
        }
        meter.release_bytes(span_bytes);
        Ok(())
    }

    pub(crate) fn fill_triangle<const N: usize>(
        &mut self,
        points: [Point; 3],
        values: [[f32; N]; 3],
        shade: &impl Fn(&[f32; N]) -> [f32; 4],
        meter: &mut dyn Meter,
    ) -> Result<(), Exhausted> {
        let [p0, p1, p2] = points;
        let (e1, e2) = (p1 - p0, p2 - p0);
        let det = e1.x * e2.y - e1.y * e2.x;

        if det == 0.0 || !det.is_finite() {
            return Ok(());
        }

        let mut dx = [0.0; N];
        let mut dy = [0.0; N];

        for c in 0..N {
            let d1 = (values[1][c] - values[0][c]) as f64;
            let d2 = (values[2][c] - values[0][c]) as f64;
            dx[c] = ((d1 * e2.y - d2 * e1.y) / det) as f32;
            dy[c] = ((d2 * e1.x - d1 * e2.x) / det) as f32;
        }

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
            take(meter, Cost::of(upper - lower).times((N + 4) as u64))?;
            for &(point, index) in &sparse.locations[lower..upper] {
                let d = point - p0;
                let u = d.cross(e2) / det;
                let v = e1.cross(d) / det;
                if u >= 0.0 && v >= 0.0 && u + v <= 1.0 {
                    self.pixels[index] = shade(&std::array::from_fn(|c| {
                        values[0][c] + (f64::from(dx[c]) * d.x + f64::from(dy[c]) * d.y) as f32
                    }));
                }
            }
            return Ok(());
        }

        let mut sorted = points;
        sorted.sort_by(|a, b| a.y.total_cmp(&b.y).then(a.x.total_cmp(&b.x)));
        let [top, middle, bottom] = sorted;

        let first_row = pixel_index(top.y, self.height);
        let end_row = pixel_index(bottom.y, self.height);

        take(meter, Cost::of(end_row - first_row))?;
        for y in first_row..end_row {
            let center_y = y as f64 + 0.5;
            let long = edge_x(top, bottom, center_y);
            let short = if center_y < middle.y {
                edge_x(top, middle, center_y)
            } else {
                edge_x(middle, bottom, center_y)
            };

            let start = pixel_index(long.min(short), self.width);
            let end = pixel_index(long.max(short), self.width);

            if start >= end {
                continue;
            }

            let mut value = [0.0; N];
            let (offset_x, offset_y) = (start as f64 + 0.5 - p0.x, center_y - p0.y);

            for c in 0..N {
                value[c] =
                    values[0][c] + (dx[c] as f64 * offset_x + dy[c] as f64 * offset_y) as f32;
            }

            take(meter, Cost::of(end - start).times((N + 1) as u64))?;
            self.prepare_span(shade, meter)?;
            if let Layout::Rows { spans, .. } = &mut self.layout {
                crate::encode::reserve_metered(spans, 1, meter)?;
                let mut initial = [0.0; 4];
                initial[..N].copy_from_slice(&value);
                let mut step = [0.0; 4];
                step[..N].copy_from_slice(&dx);
                spans.push(Span {
                    order: spans.len(),
                    y,
                    start,
                    end,
                    value: initial,
                    dx: step,
                });
                continue;
            }
            for pixel in &mut self.pixels[y * self.width + start..y * self.width + end] {
                *pixel = shade(&value);

                for c in 0..N {
                    value[c] += dx[c];
                }
            }
        }
        Ok(())
    }
}

enum Layout {
    Dense,
    Points(Sparse),
    Rows {
        spans: Vec<Span>,
        lut: Option<ColorLut>,
    },
}

struct Span {
    order: usize,
    y: usize,
    start: usize,
    end: usize,
    value: [f32; 4],
    dx: [f32; 4],
}

impl Span {
    fn paint<const N: usize>(&self, row: &mut [[f32; 4]], shade: &impl Fn(&[f32; N]) -> [f32; 4]) {
        let mut value = std::array::from_fn(|i| self.value[i]);
        for pixel in &mut row[self.start..self.end] {
            *pixel = shade(&value);
            for (v, dx) in value.iter_mut().zip(self.dx) {
                *v += dx;
            }
        }
    }
}

struct Sparse {
    area: Rect,
    locations: Vec<(Point, usize)>,
    by_x: bool,
}

impl Sparse {
    fn axis(p: Point, x: bool) -> f64 {
        if x { p.x } else { p.y }
    }
}

fn pixel_index(coordinate: f64, len: usize) -> usize {
    (coordinate - 0.5).ceil().clamp(0.0, len as f64) as usize
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
