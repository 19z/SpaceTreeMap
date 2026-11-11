use egui::{pos2, Rect};

/// Squarified layout. Zero-byte entries have no area and remain available in the list.
/// Inputs must be sorted by descending size; calculations use f64 for large volumes.
pub fn layout(items: &[(usize, u64)], rect: Rect) -> Vec<(usize, Rect)> {
    let total: f64 = items.iter().map(|(_, size)| *size as f64).sum();
    if total == 0.0 || rect.width() <= 0.0 || rect.height() <= 0.0 {
        return Vec::new();
    }
    let scale = rect.area() as f64 / total;
    let areas: Vec<_> = items
        .iter()
        .filter(|(_, size)| *size > 0)
        .map(|(id, size)| (*id, *size as f64 * scale))
        .collect();
    let mut out = Vec::with_capacity(areas.len());
    let mut bounds = rect;
    let mut start = 0;
    while start < areas.len() {
        let side = bounds.width().min(bounds.height()) as f64;
        if side <= 0.0 {
            break;
        }
        let mut end = start + 1;
        let mut sum = areas[start].1;
        let mut worst = aspect(sum, areas[start].1, areas[start].1, side);
        while end < areas.len() {
            let next_sum = sum + areas[end].1;
            let next = aspect(next_sum, areas[end].1, areas[start].1, side);
            if next > worst {
                break;
            }
            worst = next;
            sum = next_sum;
            end += 1;
        }
        let vertical = bounds.width() >= bounds.height();
        let thickness = (sum / side) as f32;
        let mut offset = if vertical {
            bounds.top()
        } else {
            bounds.left()
        };
        for (i, (id, area)) in areas[start..end].iter().enumerate() {
            let last = i + start + 1 == end;
            let next = if last {
                if vertical {
                    bounds.bottom()
                } else {
                    bounds.right()
                }
            } else {
                offset + (*area / f64::from(thickness)) as f32
            };
            let tile = if vertical {
                Rect::from_min_max(
                    pos2(bounds.left(), offset),
                    pos2((bounds.left() + thickness).min(bounds.right()), next),
                )
            } else {
                Rect::from_min_max(
                    pos2(offset, bounds.top()),
                    pos2(next, (bounds.top() + thickness).min(bounds.bottom())),
                )
            };
            out.push((*id, tile));
            offset = next;
        }
        if vertical {
            bounds.min.x = (bounds.min.x + thickness).min(bounds.max.x);
        } else {
            bounds.min.y = (bounds.min.y + thickness).min(bounds.max.y);
        }
        start = end;
    }
    out
}

fn aspect(sum: f64, min: f64, max: f64, side: f64) -> f64 {
    let square = sum * sum;
    (side * side * max / square).max(square / (side * side * min))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn partitions_area_without_overlap_and_preserves_weights() {
        let bounds = Rect::from_min_max(pos2(10.0, 20.0), pos2(810.0, 620.0));
        let items = [(0, 100), (1, 50), (2, 23), (3, 2), (4, 0)];
        let tiles = layout(&items, bounds);
        assert_eq!(tiles.len(), 4);
        for (id, tile) in &tiles {
            assert!(bounds.expand(0.001).contains_rect(*tile));
            let expected = bounds.area() * items[*id].1 as f32 / 175.0;
            assert!((tile.area() - expected).abs() < 0.1);
        }
        for a in 0..tiles.len() {
            for b in a + 1..tiles.len() {
                let overlap = tiles[a].1.intersect(tiles[b].1);
                assert!(overlap.width() <= 0.001 || overlap.height() <= 0.001);
            }
        }
    }
    #[test]
    fn empty_zero_and_extreme_sizes_are_safe() {
        let rect = Rect::from_min_max(pos2(0.0, 0.0), pos2(100.0, 100.0));
        assert!(layout(&[], rect).is_empty());
        assert!(layout(&[(1, 0)], rect).is_empty());
        assert_eq!(layout(&[(1, u64::MAX)], rect)[0].1, rect);
    }
}
