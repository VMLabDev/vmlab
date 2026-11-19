use super::{Mode, Rect, Resource, ShmFrame};
use crate::display::Damage;
use std::{collections::BTreeSet, sync::Arc};

const TILE: u32 = 16;

#[derive(Default)]
pub(super) struct Compose {
    current: Option<ShmFrame>,
    // One publication behind. A reader may still hold this allocation.
    spare: Option<Arc<Vec<u8>>>,
}

impl Compose {
    pub(super) fn frame(
        &mut self,
        mode: Mode,
        scanout: Option<(&Resource, Rect)>,
        flushes: &[Rect],
        dirty: bool,
    ) -> ShmFrame {
        let previous = self.current.take().filter(|frame| frame.mode == mode);
        if !dirty && let Some(mut frame) = previous {
            self.current = Some(frame.clone());
            frame.damage.clear();
            return frame;
        }

        let mut pixels = match (&previous, self.spare.take().and_then(Arc::into_inner)) {
            (Some(previous), Some(mut pixels)) => {
                for rect in &previous.damage {
                    for y in rect.y..rect.y + rect.height {
                        let start = (y * mode.stride + rect.x * 4) as usize;
                        let end = start + rect.width as usize * 4;
                        pixels[start..end].copy_from_slice(&previous.pixels[start..end]);
                    }
                }
                pixels
            }
            (Some(previous), _) => previous.pixels.as_ref().clone(),
            (None, _) => vec![0; mode.stride as usize * mode.height as usize],
        };

        let mut changed = BTreeSet::new();
        if let Some((resource, view)) = scanout {
            let flushes = if previous.is_none() || flushes.is_empty() {
                std::slice::from_ref(&view)
            } else {
                flushes
            };
            update(&mut pixels, resource, view, flushes, &mut changed);
        }
        let damage = if previous.is_none() {
            vec![Damage {
                x: 0,
                y: 0,
                width: mode.width,
                height: mode.height,
            }]
        } else {
            tile_damage(mode, changed)
        };
        let frame = match previous {
            Some(mut previous) if damage.is_empty() => {
                // The spare now matches the current publication.
                previous.damage.clear();
                self.spare = Some(Arc::new(pixels));
                previous
            }
            previous => {
                self.spare = previous.map(|frame| frame.pixels);
                ShmFrame {
                    mode,
                    pixels: Arc::new(pixels),
                    damage,
                }
            }
        };
        self.current = Some(frame.clone());
        frame
    }
}

fn update(
    pixels: &mut [u8],
    resource: &Resource,
    view: Rect,
    flushes: &[Rect],
    changed: &mut BTreeSet<(u32, u32)>,
) {
    // Within a band the same clipped flushes cover every row. Merge their
    // spans once so overlapping flushes never compare or write a pixel twice.
    let rects: Vec<_> = flushes
        .iter()
        .copied()
        .filter(|r| view.overlaps(*r))
        .collect();
    let mut edges: Vec<_> = rects
        .iter()
        .flat_map(|r| [r.y.max(view.y), (r.y + r.h).min(view.y + view.h)])
        .collect();
    edges.sort_unstable();
    edges.dedup();
    let mut spans = Vec::new();
    for band in edges.windows(2) {
        let [top, bottom] = [band[0], band[1]];
        spans.clear();
        spans.extend(
            rects
                .iter()
                .filter(|r| (r.y..r.y + r.h).contains(&top))
                .map(|r| {
                    (
                        r.x.max(view.x) - view.x,
                        (r.x + r.w).min(view.x + view.w) - view.x,
                    )
                }),
        );
        spans.sort_unstable();
        let mut covered = 0;
        for &(start, end) in &spans {
            for y in top..bottom {
                let row = y - view.y;
                let mut x = start.max(covered);
                while x < end {
                    let tile_x = x / TILE * TILE;
                    let right = end.min(tile_x + TILE);
                    let offset = ((y * resource.width + view.x + x) * 4) as usize;
                    let dest = ((row * view.w + x) * 4) as usize;
                    let len = ((right - x) * 4) as usize;
                    let mut differs = false;
                    for (i, pixel) in pixels[dest..dest + len].chunks_exact_mut(4).enumerate() {
                        let source = resource.pixel(offset + i * 4);
                        differs |= pixel[..3] != source[..3];
                        pixel[..3].copy_from_slice(&source[..3]);
                    }
                    if differs {
                        changed.insert((row / TILE * TILE, tile_x));
                    }
                    x = right;
                }
            }
            covered = covered.max(end);
        }
    }
}

fn tile_damage(mode: Mode, changed: BTreeSet<(u32, u32)>) -> Vec<Damage> {
    let mut damage: Vec<Damage> = Vec::new();
    for (y, x) in changed {
        let width = TILE.min(mode.width - x);
        if let Some(last) = damage.last_mut()
            && last.y == y
            && last.x + last.width == x
        {
            last.width += width;
        } else {
            damage.push(Damage {
                x,
                y,
                width,
                height: TILE.min(mode.height - y),
            });
        }
    }
    damage
}
