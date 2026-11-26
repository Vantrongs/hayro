use hayro_syntax::object::dict::keys::{BBOX, MATRIX};
use hayro_syntax::object::{Array, Dict, Object};
use kurbo::{Affine, Rect};

fn coordinates<const N: usize>(dict: &Dict<'_>, key: &[u8]) -> Option<[f64; N]> {
    let array = dict.get::<Array<'_>>(key)?;
    let mut items = array.flex_iter();
    Some(std::array::from_fn(|_| match items.next::<Object<'_>>() {
        Some(Object::Number(n)) => n.as_f64(),
        _ => 0.0,
    }))
}

pub(crate) fn bbox(dict: &Dict<'_>) -> Option<Rect> {
    let [x0, y0, x1, y1] = coordinates(dict, BBOX)?;
    Some(Rect::new(x0, y0, x1, y1).abs())
}

pub(crate) fn matrix(dict: &Dict<'_>) -> Affine {
    coordinates(dict, MATRIX)
        .map(Affine::new)
        .unwrap_or_default()
}

pub(crate) fn box_and_matrix(dict: &Dict<'_>) -> Option<([f32; 4], Affine)> {
    Some((
        coordinates::<4>(dict, BBOX)?.map(|v| v as f32),
        matrix(dict),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hayro_syntax::object::FromBytes;

    #[test]
    fn preserve_positions() {
        let dict = Dict::from_bytes(b"<< /BBox [8 /bad 2 3 4] /Matrix [1 2 null 4] >>").unwrap();
        assert_eq!(box_and_matrix(&dict).unwrap().0, [8., 0., 2., 3.]);
        assert_eq!(bbox(&dict), Some(Rect::new(2., 0., 8., 3.)));
        assert_eq!(matrix(&dict).as_coeffs(), [1., 2., 0., 4., 0., 0.]);
        let dict = Dict::from_bytes(b"<< /Matrix null >>").unwrap();
        assert_eq!(box_and_matrix(&dict), None);
        assert_eq!(matrix(&dict), Affine::IDENTITY);
    }
}
