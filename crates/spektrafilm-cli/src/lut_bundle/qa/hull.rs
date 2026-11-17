use std::collections::BTreeMap;

type Point = [f64; 3];

struct Face {
    vertices: [usize; 3],
    origin: Point,
    normal: Point,
}

impl Face {
    fn new(mut vertices: [usize; 3], points: &[Point], inside: Point) -> Option<Self> {
        let [a, b, c] = vertices.map(|i| points[i]);
        let raw = cross(subtract(b, a), subtract(c, a));
        let length = norm(raw);
        if length <= 1e-16 {
            return None;
        }
        let mut normal = raw.map(|v| v / length);
        if dot(normal, subtract(inside, a)) > 0.0 {
            vertices.swap(1, 2);
            normal = normal.map(|v| -v);
        }
        Some(Self {
            vertices,
            origin: a,
            normal,
        })
    }

    fn is_visible(&self, point: Point) -> bool {
        dot(self.normal, subtract(point, self.origin)) > 1e-12
    }
}

/// Volume of the hull, incrementally replacing visible faces with the
/// triangles formed by the horizon and the newly exposed vertex.
pub(super) fn volume(points: &[Point]) -> Option<f64> {
    let a = (0..points.len()).min_by(|&i, &j| points[i][0].total_cmp(&points[j][0]))?;
    let b = farthest(points, a, |p| norm(subtract(p, points[a])))?;
    let axis = subtract(points[b], points[a]);
    let c = farthest(points, b, |p| norm(cross(subtract(p, points[a]), axis)))?;
    let normal = cross(axis, subtract(points[c], points[a]));
    let d = farthest(points, c, |p| dot(normal, subtract(p, points[a])).abs())?;
    if dot(normal, subtract(points[d], points[a])).abs() <= 1e-12 {
        return None;
    }
    let inside =
        std::array::from_fn(|i| (points[a][i] + points[b][i] + points[c][i] + points[d][i]) * 0.25);
    let mut faces: Vec<_> = [[a, b, c], [a, b, d], [a, c, d], [b, c, d]]
        .into_iter()
        .filter_map(|vertices| Face::new(vertices, points, inside))
        .collect();
    for (i, &p) in points.iter().enumerate() {
        if [a, b, c, d].contains(&i) {
            continue;
        }
        let mut horizon = BTreeMap::new();
        faces.retain(|face| {
            if !face.is_visible(p) {
                return true;
            }
            let [a, b, c] = face.vertices;
            for [u, v] in [[a, b], [b, c], [c, a]] {
                let index = (u.min(v), u.max(v));
                if horizon.remove(&index).is_none() {
                    horizon.insert(index, [u, v]);
                }
            }
            false
        });
        for [u, v] in horizon.into_values() {
            if let Some(face) = Face::new([u, v, i], points, inside) {
                faces.push(face);
            }
        }
    }
    let volume: f64 = faces
        .iter()
        .map(|face| {
            let [a, b, c] = face.vertices.map(|i| subtract(points[i], inside));
            dot(a, cross(b, c)) / 6.0
        })
        .sum();
    (volume > 0.0 && volume.is_finite()).then_some(volume)
}

fn farthest(points: &[Point], excluded: usize, distance: impl Fn(Point) -> f64) -> Option<usize> {
    points
        .iter()
        .copied()
        .enumerate()
        .filter(|&(i, _)| i != excluded)
        .max_by(|&(_, a), &(_, b)| distance(a).total_cmp(&distance(b)))
        .map(|(i, _)| i)
}

fn subtract(a: Point, b: Point) -> Point {
    std::array::from_fn(|i| a[i] - b[i])
}

fn cross(a: Point, b: Point) -> Point {
    std::array::from_fn(|i| a[(i + 1) % 3] * b[(i + 2) % 3] - a[(i + 2) % 3] * b[(i + 1) % 3])
}

fn dot(a: Point, b: Point) -> f64 {
    a.into_iter().zip(b).map(|(x, y)| x * y).sum()
}

fn norm(a: Point) -> f64 {
    dot(a, a).sqrt()
}

#[cfg(test)]
mod tests {
    use super::volume;

    #[test]
    fn cube_hull_omits_interior_and_coplanar_points() {
        let points = [
            [0.5, 0.5, 0.5],
            [0., 0., 0.],
            [0., 0., 1.],
            [0., 1., 0.],
            [0., 1., 1.],
            [1., 0., 0.],
            [1., 0., 1.],
            [1., 1., 0.],
            [1., 1., 1.],
            [1., 0.5, 0.5],
            [0., 0., 0.],
        ];
        assert!((volume(&points).unwrap() - 1.0).abs() < 1e-12);
        assert!(volume(&points.map(|[x, y, _]| [x, y, 0.])).is_none());
    }
}
