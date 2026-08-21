//! Многослойная CPU-композиция по damage rectangles.

use crate::{CpuPixelFormat, CpuSurface, CpuSurfaceMut, DamageRegion, Point, Rect};

/// Окно, курсор, overlay или видеокадр как независимый surface-слой.
#[derive(Clone, Copy)]
pub struct Layer<'a> {
    pub surface: CpuSurface<'a>,
    pub position: Point,
    pub opacity: u8,
    pub visible: bool,
}

impl Layer<'_> {
    pub fn bounds(self) -> Rect {
        Rect::new(
            self.position.x,
            self.position.y,
            self.surface.width(),
            self.surface.height(),
        )
    }
}

/// Пересобирает только повреждённые части target: сначала background, затем
/// видимые layers в порядке снизу вверх. Число окон не зашито в API.
pub fn composite<const CAPACITY: usize>(
    target: &mut CpuSurfaceMut<'_>,
    background: CpuSurface<'_>,
    layers: &[Layer<'_>],
    damage: &DamageRegion<CAPACITY>,
) -> u64 {
    let mut written = 0u64;
    for damaged in damage.iter().copied() {
        written = written.saturating_add(target.blit(
            background,
            damaged,
            Point::new(damaged.x, damaged.y),
        ));
        for layer in layers.iter().copied().filter(|layer| layer.visible) {
            let visible = damaged.intersection(layer.bounds());
            if visible.is_empty() {
                continue;
            }
            let source_rect = Rect::new(
                visible.x.saturating_sub(layer.position.x),
                visible.y.saturating_sub(layer.position.y),
                visible.width,
                visible.height,
            );
            if layer.opacity == 255 && layer.surface.format() != CpuPixelFormat::Argb8888 {
                written = written.saturating_add(target.blit(
                    layer.surface,
                    source_rect,
                    Point::new(visible.x, visible.y),
                ));
            } else {
                written = written.saturating_add(target.blend(
                    layer.surface,
                    source_rect,
                    Point::new(visible.x, visible.y),
                    layer.opacity,
                ));
            }
        }
    }
    written
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Color, Rgba};

    #[test]
    fn compositor_respects_z_order_alpha_and_damage() {
        let background_pixels = [CpuPixelFormat::Rgb888.pack_color(Color::rgb(0, 0, 20)); 16];
        let background =
            CpuSurface::new(&background_pixels, 4, 4, 4, CpuPixelFormat::Rgb888).unwrap();
        let opaque_pixels = [CpuPixelFormat::Rgb888.pack_color(Color::rgb(20, 0, 0)); 4];
        let opaque = CpuSurface::new(&opaque_pixels, 2, 2, 2, CpuPixelFormat::Rgb888).unwrap();
        let alpha_pixels = [CpuPixelFormat::Argb8888.pack(Rgba::new(0, 200, 0, 128))];
        let alpha = CpuSurface::new(&alpha_pixels, 1, 1, 1, CpuPixelFormat::Argb8888).unwrap();
        let layers = [
            Layer {
                surface: opaque,
                position: Point::new(1, 1),
                opacity: 255,
                visible: true,
            },
            Layer {
                surface: alpha,
                position: Point::new(2, 2),
                opacity: 255,
                visible: true,
            },
        ];
        let mut target_pixels = [0u32; 16];
        let mut target =
            CpuSurfaceMut::new(&mut target_pixels, 4, 4, 4, CpuPixelFormat::Rgb888).unwrap();
        let mut damage = DamageRegion::<4>::new(Rect::new(0, 0, 4, 4));
        damage.add(Rect::new(1, 1, 2, 2));
        assert_eq!(composite(&mut target, background, &layers, &damage), 9);
        assert_eq!(
            CpuPixelFormat::Rgb888.unpack(target_pixels[2 * 4 + 2]),
            Rgba::new(10, 100, 0, 255)
        );
        assert_eq!(target_pixels[0], 0);
    }
}
