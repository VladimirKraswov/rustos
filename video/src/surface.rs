//! Безопасные представления pixel surfaces и CPU raster operations.

use crate::{pixel::blend, Color, PixelFormat, Point, Rect};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SurfaceError {
    Empty,
    InvalidStride,
    StorageTooSmall,
}

/// Read-only surface. Stride измеряется в 32-bit пикселях, не в байтах.
#[derive(Clone, Copy)]
pub struct Surface<'a> {
    pixels: &'a [u32],
    width: u32,
    height: u32,
    stride: u32,
    format: PixelFormat,
}

impl<'a> Surface<'a> {
    pub fn new(
        pixels: &'a [u32],
        width: u32,
        height: u32,
        stride: u32,
        format: PixelFormat,
    ) -> Result<Self, SurfaceError> {
        validate(pixels.len(), width, height, stride)?;
        Ok(Self {
            pixels,
            width,
            height,
            stride,
            format,
        })
    }

    pub const fn width(self) -> u32 {
        self.width
    }

    pub const fn height(self) -> u32 {
        self.height
    }

    pub const fn format(self) -> PixelFormat {
        self.format
    }

    pub const fn bounds(self) -> Rect {
        Rect::new(0, 0, self.width, self.height)
    }

    pub fn pixel(self, x: u32, y: u32) -> Option<u32> {
        if x >= self.width || y >= self.height {
            return None;
        }
        Some(self.pixels[y as usize * self.stride as usize + x as usize])
    }

    pub fn row(self, y: u32, x: u32, width: u32) -> Option<&'a [u32]> {
        if y >= self.height || x > self.width || width > self.width.saturating_sub(x) {
            return None;
        }
        let start = y as usize * self.stride as usize + x as usize;
        Some(&self.pixels[start..start + width as usize])
    }

    /// Возвращает весь кадр одним span, только если surface плотно упакован.
    /// Scanout использует этот путь для единственного линейного copy кадра.
    pub fn contiguous_pixels(self) -> Option<&'a [u32]> {
        if self.stride != self.width {
            return None;
        }
        let length = self.width as usize * self.height as usize;
        self.pixels.get(..length)
    }
}

/// Mutable surface. Конструктор один раз доказывает все bounds, после чего
/// fill/blit не содержат unsafe и не проверяют каждый пиксель отдельно.
pub struct SurfaceMut<'a> {
    pixels: &'a mut [u32],
    width: u32,
    height: u32,
    stride: u32,
    format: PixelFormat,
}

impl<'a> SurfaceMut<'a> {
    pub fn new(
        pixels: &'a mut [u32],
        width: u32,
        height: u32,
        stride: u32,
        format: PixelFormat,
    ) -> Result<Self, SurfaceError> {
        validate(pixels.len(), width, height, stride)?;
        Ok(Self {
            pixels,
            width,
            height,
            stride,
            format,
        })
    }

    pub const fn width(&self) -> u32 {
        self.width
    }

    pub const fn height(&self) -> u32 {
        self.height
    }

    pub const fn format(&self) -> PixelFormat {
        self.format
    }

    pub const fn bounds(&self) -> Rect {
        Rect::new(0, 0, self.width, self.height)
    }

    pub fn as_surface(&self) -> Surface<'_> {
        Surface {
            pixels: self.pixels,
            width: self.width,
            height: self.height,
            stride: self.stride,
            format: self.format,
        }
    }

    /// Быстрая span-fill: bounds вычисляются один раз на прямоугольник,
    /// затем `slice::fill` даёт LLVM возможность применить wide stores.
    pub fn fill(&mut self, rect: Rect, color: Color) -> u64 {
        let clipped = rect.intersection(self.bounds());
        if clipped.is_empty() {
            return 0;
        }
        let raw = self.format.pack_color(color);
        for y in clipped.y as u32..clipped.bottom() as u32 {
            let row = self.row_mut(y, clipped.x as u32, clipped.width);
            row.fill(raw);
        }
        clipped.area()
    }

    /// Opaque blit с clipping. При одинаковом формате копирует целые spans;
    /// конвертация каналов выполняется только когда это действительно нужно.
    pub fn blit(&mut self, source: Surface<'_>, source_rect: Rect, destination: Point) -> u64 {
        let Some((source_rect, destination_rect)) =
            clip_blit(source.bounds(), source_rect, self.bounds(), destination)
        else {
            return 0;
        };
        for row in 0..destination_rect.height {
            let source_row = source
                .row(
                    (source_rect.y as u32) + row,
                    source_rect.x as u32,
                    source_rect.width,
                )
                .unwrap_or_default();
            let destination_format = self.format;
            let source_format = source.format;
            let destination_row = self.row_mut(
                (destination_rect.y as u32) + row,
                destination_rect.x as u32,
                destination_rect.width,
            );
            if source.format == destination_format {
                destination_row.copy_from_slice(source_row);
            } else {
                for (destination, source) in destination_row.iter_mut().zip(source_row) {
                    *destination = destination_format.pack(source_format.unpack(*source));
                }
            }
        }
        destination_rect.area()
    }

    /// Alpha-composition source-over с global opacity. ARGB surface использует
    /// собственный alpha каждого пикселя; GOP RGB/BGR считается непрозрачным.
    pub fn blend(
        &mut self,
        source: Surface<'_>,
        source_rect: Rect,
        destination: Point,
        opacity: u8,
    ) -> u64 {
        if opacity == 0 {
            return 0;
        }
        let Some((source_rect, destination_rect)) =
            clip_blit(source.bounds(), source_rect, self.bounds(), destination)
        else {
            return 0;
        };
        for row in 0..destination_rect.height {
            let source_row = source
                .row(
                    (source_rect.y as u32) + row,
                    source_rect.x as u32,
                    source_rect.width,
                )
                .unwrap_or_default();
            let destination_format = self.format;
            let source_format = source.format;
            let destination_row = self.row_mut(
                (destination_rect.y as u32) + row,
                destination_rect.x as u32,
                destination_rect.width,
            );
            for (destination, source) in destination_row.iter_mut().zip(source_row) {
                let result = blend(
                    source_format.unpack(*source),
                    destination_format.unpack(*destination),
                    opacity,
                );
                *destination = destination_format.pack(result);
            }
        }
        destination_rect.area()
    }

    /// Базовый scaler для иконок, изображений и decoded video frames.
    /// Nearest-neighbour выбран как простой гарантированный backend; более
    /// качественный bilinear/SIMD scaler сможет иметь тот же surface API.
    pub fn blit_scaled_nearest(
        &mut self,
        source: Surface<'_>,
        source_rect: Rect,
        destination_rect: Rect,
        opacity: u8,
    ) -> u64 {
        let source_rect = source_rect.intersection(source.bounds());
        let clipped_destination = destination_rect.intersection(self.bounds());
        if source_rect.is_empty() || destination_rect.is_empty() || clipped_destination.is_empty() {
            return 0;
        }
        let source_format = source.format;
        let destination_format = self.format;
        for y in clipped_destination.y as u32..clipped_destination.bottom() as u32 {
            let relative_y = y as i64 - i64::from(destination_rect.y);
            let source_y = source_rect.y as u32
                + ((relative_y as u64 * u64::from(source_rect.height))
                    / u64::from(destination_rect.height)) as u32;
            let destination_row =
                self.row_mut(y, clipped_destination.x as u32, clipped_destination.width);
            for (offset, destination) in destination_row.iter_mut().enumerate() {
                let x = clipped_destination.x as u32 + offset as u32;
                let relative_x = x as i64 - i64::from(destination_rect.x);
                let source_x = source_rect.x as u32
                    + ((relative_x as u64 * u64::from(source_rect.width))
                        / u64::from(destination_rect.width)) as u32;
                let source_raw = source.pixel(source_x, source_y).unwrap_or(0);
                let source_pixel = source_format.unpack(source_raw);
                if opacity == 255 && source_pixel.a == 255 {
                    *destination = destination_format.pack(source_pixel);
                } else {
                    *destination = destination_format.pack(blend(
                        source_pixel,
                        destination_format.unpack(*destination),
                        opacity,
                    ));
                }
            }
        }
        clipped_destination.area()
    }

    fn row_mut(&mut self, y: u32, x: u32, width: u32) -> &mut [u32] {
        let start = y as usize * self.stride as usize + x as usize;
        &mut self.pixels[start..start + width as usize]
    }
}

fn validate(storage_len: usize, width: u32, height: u32, stride: u32) -> Result<(), SurfaceError> {
    if width == 0 || height == 0 {
        return Err(SurfaceError::Empty);
    }
    if stride < width {
        return Err(SurfaceError::InvalidStride);
    }
    let required = (height as usize - 1)
        .checked_mul(stride as usize)
        .and_then(|offset| offset.checked_add(width as usize))
        .ok_or(SurfaceError::StorageTooSmall)?;
    if required > storage_len {
        return Err(SurfaceError::StorageTooSmall);
    }
    Ok(())
}

fn clip_blit(
    source_bounds: Rect,
    requested_source: Rect,
    destination_bounds: Rect,
    destination: Point,
) -> Option<(Rect, Rect)> {
    let source = requested_source.intersection(source_bounds);
    if source.is_empty() {
        return None;
    }
    let source_offset = Point::new(
        source.x.saturating_sub(requested_source.x),
        source.y.saturating_sub(requested_source.y),
    );
    let candidate = Rect::new(
        destination.x.saturating_add(source_offset.x),
        destination.y.saturating_add(source_offset.y),
        source.width,
        source.height,
    );
    let clipped_destination = candidate.intersection(destination_bounds);
    if clipped_destination.is_empty() {
        return None;
    }
    let clipped_source = Rect::new(
        source
            .x
            .saturating_add(clipped_destination.x.saturating_sub(candidate.x)),
        source
            .y
            .saturating_add(clipped_destination.y.saturating_sub(candidate.y)),
        clipped_destination.width,
        clipped_destination.height,
    );
    Some((clipped_source, clipped_destination))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_and_blit_clip_without_touching_padding() {
        let mut source_pixels = [0u32; 12];
        let mut source =
            SurfaceMut::new(&mut source_pixels, 3, 3, 4, PixelFormat::Argb8888).unwrap();
        source.fill(Rect::new(0, 0, 3, 3), Color::rgb(10, 20, 30));
        let mut destination_pixels = [0xdead_beefu32; 20];
        let mut destination =
            SurfaceMut::new(&mut destination_pixels, 4, 4, 5, PixelFormat::Bgr888).unwrap();
        assert_eq!(
            destination.blit(source.as_surface(), Rect::new(0, 0, 3, 3), Point::new(3, 2)),
            2
        );
        assert_eq!(
            PixelFormat::Bgr888.unpack(destination_pixels[2 * 5 + 3]),
            crate::Rgba::new(10, 20, 30, 255)
        );
        assert_eq!(destination_pixels[2 * 5 + 4], 0xdead_beef);
    }

    #[test]
    fn argb_blend_is_source_over() {
        let source_pixels = [PixelFormat::Argb8888.pack(crate::Rgba::new(255, 0, 0, 128))];
        let source = Surface::new(&source_pixels, 1, 1, 1, PixelFormat::Argb8888).unwrap();
        let mut destination_pixels = [PixelFormat::Rgb888.pack_color(Color::rgb(0, 0, 255))];
        let mut destination =
            SurfaceMut::new(&mut destination_pixels, 1, 1, 1, PixelFormat::Rgb888).unwrap();
        destination.blend(source, source.bounds(), Point::new(0, 0), 255);
        assert_eq!(
            PixelFormat::Rgb888.unpack(destination_pixels[0]),
            crate::Rgba::new(128, 0, 127, 255)
        );
    }

    #[test]
    fn nearest_scaler_preserves_source_quadrants() {
        let source_pixels = [
            PixelFormat::Rgb888.pack_color(Color::rgb(1, 0, 0)),
            PixelFormat::Rgb888.pack_color(Color::rgb(2, 0, 0)),
            PixelFormat::Rgb888.pack_color(Color::rgb(3, 0, 0)),
            PixelFormat::Rgb888.pack_color(Color::rgb(4, 0, 0)),
        ];
        let source = Surface::new(&source_pixels, 2, 2, 2, PixelFormat::Rgb888).unwrap();
        let mut destination_pixels = [0u32; 16];
        let mut destination =
            SurfaceMut::new(&mut destination_pixels, 4, 4, 4, PixelFormat::Rgb888).unwrap();
        assert_eq!(
            destination.blit_scaled_nearest(source, source.bounds(), Rect::new(0, 0, 4, 4), 255),
            16
        );
        assert_eq!(
            [
                destination_pixels[0] & 0xff,
                destination_pixels[3] & 0xff,
                destination_pixels[12] & 0xff,
                destination_pixels[15] & 0xff,
            ],
            [1, 2, 3, 4]
        );
    }
}
