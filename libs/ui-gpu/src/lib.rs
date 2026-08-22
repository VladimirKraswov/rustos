//! GPU backend для display list SystemUI.
//!
//! Backend выполняет layout-to-physical преобразование и формирует bounded
//! instance stream. Он принципиально не содержит framebuffer и не обходит
//! pixels: прямоугольники растрирует fragment shader, а text/image resource ID
//! разрешает общий atlas renderd. Низкоуровневый VirGL/Vulkan transport живёт
//! ниже и может меняться без изменения компонентов.

#![no_std]
#![warn(missing_docs)]

use rustos_system_ui::{Color, FontSpec, Rect, RenderBackend, ResourceId, WindowMetrics};

/// Тип shader primitive. Значения стабильны внутри renderer ABI, но не
/// экспортируются как kernel ABI.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GpuPrimitiveKind {
    /// Аналитически скруглённая заливка.
    RoundedFill = 1,
    /// Аналитическая рамка без четырёх отдельных CPU rectangles.
    RoundedBorder = 2,
    /// Мягкая тень, вычисляемая fragment shader.
    Shadow = 3,
    /// Строка, которую atlas layer разворачивает в glyph quads.
    TextRun = 4,
    /// Иконка или изображение из общего texture atlas.
    Image = 5,
}

/// Physical rectangle GPU instance.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PhysicalRect {
    /// Левая координата в physical pixels.
    pub x: i32,
    /// Верхняя координата в physical pixels.
    pub y: i32,
    /// Ширина в physical pixels.
    pub width: u32,
    /// Высота в physical pixels.
    pub height: u32,
}

impl PhysicalRect {
    /// Пустой rectangle.
    pub const EMPTY: Self = Self {
        x: 0,
        y: 0,
        width: 0,
        height: 0,
    };

    /// Проверяет отсутствие площади.
    pub const fn is_empty(self) -> bool {
        self.width == 0 || self.height == 0
    }
}

/// Одна instance, которую renderd превращает в quad/glyph batch.
///
/// Цвет хранится как premultiplied RGBA8. `resource` равен нулю для
/// геометрических primitives. Для текста `font_flags` кодирует системное
/// семейство/начертание, а `font_size_px` уже учитывает device scale.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GpuUiInstance {
    /// Тип fragment pipeline.
    pub kind: GpuPrimitiveKind,
    /// Physical bounds primitive.
    pub bounds: PhysicalRect,
    /// Physical scissor.
    pub clip: PhysicalRect,
    /// Premultiplied RGBA8.
    pub color: u32,
    /// Радиус в physical pixels.
    pub radius_px: u16,
    /// Толщина рамки в physical pixels.
    pub border_px: u16,
    /// Строка/иконка/изображение в resource table приложения.
    pub resource: u32,
    /// Размер glyph в physical pixels.
    pub font_size_px: u16,
    /// Биты [`font_flag`].
    pub font_flags: u16,
}

impl GpuUiInstance {
    const EMPTY: Self = Self {
        kind: GpuPrimitiveKind::RoundedFill,
        bounds: PhysicalRect::EMPTY,
        clip: PhysicalRect::EMPTY,
        color: 0,
        radius_px: 0,
        border_px: 0,
        resource: 0,
        font_size_px: 0,
        font_flags: 0,
    };
}

/// Flags системного font atlas.
pub mod font_flag {
    /// Жирное начертание.
    pub const BOLD: u16 = 1 << 0;
    /// Курсив.
    pub const ITALIC: u16 = 1 << 1;
    /// Моноширинное семейство.
    pub const MONOSPACE: u16 = 1 << 2;
    /// Горизонтальное центрирование.
    pub const ALIGN_CENTER: u16 = 1 << 3;
    /// Выравнивание к правому краю.
    pub const ALIGN_END: u16 = 1 << 4;
    /// Вертикальное центрирование.
    pub const VERTICAL_CENTER: u16 = 1 << 5;
}

/// Ошибка compilation stage. Переполненный frame нельзя публиковать частично.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GpuUiError {
    /// Instance budget недостаточен для всего display list.
    Capacity,
    /// Metrics имеют нулевой physical или logical размер.
    InvalidMetrics,
    /// Размер одного transport batch равен нулю.
    InvalidBatchSize,
}

/// Один непрерывный кусок instance buffer для bounded GPU submission.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InstanceBatch<'a> {
    /// Последовательность instances в paint order.
    pub instances: &'a [GpuUiInstance],
}

/// Итератор не создаёт временный `Vec` и никогда не превышает transport budget.
pub struct BatchIter<'a> {
    instances: &'a [GpuUiInstance],
    cursor: usize,
    max_instances: usize,
}

impl<'a> Iterator for BatchIter<'a> {
    type Item = InstanceBatch<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.cursor == self.instances.len() {
            return None;
        }
        let end = self
            .cursor
            .saturating_add(self.max_instances)
            .min(self.instances.len());
        let batch = InstanceBatch {
            instances: &self.instances[self.cursor..end],
        };
        self.cursor = end;
        Some(batch)
    }
}

/// Основной SystemUI backend GPU-сеанса.
///
/// Вызов `Runtime::render` заполняет этот объект. После успешного `finish`
/// renderd загружает instances и atlas patches, запускает GPU shaders и
/// публикует получившийся `GraphicsBuffer` в surface queue.
pub struct GpuRenderBackend<const N: usize> {
    metrics: WindowMetrics,
    instances: [GpuUiInstance; N],
    len: usize,
    overflowed: bool,
}

#[derive(Clone, Copy)]
struct InstanceStyle {
    color: Color,
    radius: u8,
    border: u8,
    resource: ResourceId,
    font: Option<FontSpec>,
}

impl InstanceStyle {
    const fn geometry(color: Color, radius: u8, border: u8) -> Self {
        Self {
            color,
            radius,
            border,
            resource: ResourceId(0),
            font: None,
        }
    }

    const fn resource(color: Color, resource: ResourceId, font: Option<FontSpec>) -> Self {
        Self {
            color,
            radius: 0,
            border: 0,
            resource,
            font,
        }
    }
}

impl<const N: usize> GpuRenderBackend<N> {
    /// Создаёт backend для конкретной physical surface.
    pub fn new(metrics: WindowMetrics) -> Result<Self, GpuUiError> {
        if metrics.logical_width() == 0
            || metrics.logical_height() == 0
            || metrics.physical_width() == 0
            || metrics.physical_height() == 0
        {
            return Err(GpuUiError::InvalidMetrics);
        }
        Ok(Self {
            metrics,
            instances: [GpuUiInstance::EMPTY; N],
            len: 0,
            overflowed: false,
        })
    }

    /// Очищает только command storage следующего кадра; atlas и GPU pipeline
    /// намеренно не пересоздаются.
    pub fn begin_frame(&mut self) {
        self.len = 0;
        self.overflowed = false;
    }

    /// Завершает frame. При overflow caller обязан сохранить предыдущий frame
    /// или показать recovery UI, но не отправлять префикс списка.
    pub fn finish(&self) -> Result<&[GpuUiInstance], GpuUiError> {
        if self.overflowed {
            Err(GpuUiError::Capacity)
        } else {
            Ok(&self.instances[..self.len])
        }
    }

    /// Разбивает готовый frame на bounded submissions.
    pub fn batches(&self, max_instances: usize) -> Result<BatchIter<'_>, GpuUiError> {
        if max_instances == 0 {
            return Err(GpuUiError::InvalidBatchSize);
        }
        let instances = self.finish()?;
        Ok(BatchIter {
            instances,
            cursor: 0,
            max_instances,
        })
    }

    /// Метрики, применённые до rasterization.
    pub const fn metrics(&self) -> WindowMetrics {
        self.metrics
    }

    fn push(&mut self, kind: GpuPrimitiveKind, rect: Rect, clip: Rect, style: InstanceStyle) {
        let visible = rect.intersection(clip);
        if visible.is_empty() {
            return;
        }
        let Some(slot) = self.instances.get_mut(self.len) else {
            self.overflowed = true;
            return;
        };
        let scale = u32::from(self.metrics.device_scale_milli());
        let font_size_px = style
            .font
            .map(|font| scale_unsigned(u32::from(font.size), scale))
            .unwrap_or(0)
            .min(u32::from(u16::MAX)) as u16;
        *slot = GpuUiInstance {
            kind,
            bounds: scale_rect(rect, scale),
            clip: scale_rect(clip, scale),
            color: premultiplied_rgba(style.color),
            radius_px: scale_unsigned(u32::from(style.radius), scale).min(u32::from(u16::MAX))
                as u16,
            border_px: scale_unsigned(u32::from(style.border), scale).min(u32::from(u16::MAX))
                as u16,
            resource: style.resource.0,
            font_size_px,
            font_flags: style.font.map(font_flags).unwrap_or(0),
        };
        self.len += 1;
    }
}

impl<const N: usize> RenderBackend for GpuRenderBackend<N> {
    fn fill(&mut self, rect: Rect, color: Color, clip: Rect) {
        self.push(
            GpuPrimitiveKind::RoundedFill,
            rect,
            clip,
            InstanceStyle::geometry(color, 0, 0),
        );
    }

    fn border(&mut self, rect: Rect, color: Color, width: u8, clip: Rect) {
        self.push(
            GpuPrimitiveKind::RoundedBorder,
            rect,
            clip,
            InstanceStyle::geometry(color, 0, width),
        );
    }

    fn text(&mut self, rect: Rect, resource: ResourceId, color: Color, font: FontSpec, clip: Rect) {
        self.push(
            GpuPrimitiveKind::TextRun,
            rect,
            clip,
            InstanceStyle::resource(color, resource, Some(font)),
        );
    }

    fn image(&mut self, rect: Rect, resource: ResourceId, tint: Color, clip: Rect) {
        self.push(
            GpuPrimitiveKind::Image,
            rect,
            clip,
            InstanceStyle::resource(tint, resource, None),
        );
    }

    fn shadow(&mut self, rect: Rect, radius: u8, color: Color, clip: Rect) {
        self.push(
            GpuPrimitiveKind::Shadow,
            rect,
            clip,
            InstanceStyle::geometry(color, radius, 0),
        );
    }

    fn rounded_fill(&mut self, rect: Rect, color: Color, radius: u8, clip: Rect) {
        self.push(
            GpuPrimitiveKind::RoundedFill,
            rect,
            clip,
            InstanceStyle::geometry(color, radius, 0),
        );
    }

    fn rounded_border(&mut self, rect: Rect, color: Color, width: u8, radius: u8, clip: Rect) {
        self.push(
            GpuPrimitiveKind::RoundedBorder,
            rect,
            clip,
            InstanceStyle::geometry(color, radius, width),
        );
    }
}

fn scale_rect(rect: Rect, scale_milli: u32) -> PhysicalRect {
    let x = scale_signed(rect.x, scale_milli);
    let y = scale_signed(rect.y, scale_milli);
    let right = scale_signed(rect.right(), scale_milli);
    let bottom = scale_signed(rect.bottom(), scale_milli);
    PhysicalRect {
        x,
        y,
        width: right.saturating_sub(x).max(0) as u32,
        height: bottom.saturating_sub(y).max(0) as u32,
    }
}

fn scale_signed(value: i32, scale_milli: u32) -> i32 {
    let scaled = i64::from(value) * i64::from(scale_milli);
    let rounded = (if scaled >= 0 {
        scaled + 500
    } else {
        scaled - 500
    }) / 1000;
    rounded.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

fn scale_unsigned(value: u32, scale_milli: u32) -> u32 {
    (u64::from(value) * u64::from(scale_milli) + 500)
        .checked_div(1000)
        .unwrap_or(0)
        .min(u64::from(u32::MAX)) as u32
}

fn premultiplied_rgba(color: Color) -> u32 {
    // `Color` в video — непрозрачный RGB token. Alpha будет добавлен в ABI
    // после появления translucent theme tokens; сейчас premultiplication
    // тривиальна и format остаётся совместимым с compositor'ом.
    u32::from(color.r)
        | (u32::from(color.g) << 8)
        | (u32::from(color.b) << 16)
        | (u32::from(u8::MAX) << 24)
}

fn font_flags(font: FontSpec) -> u16 {
    let mut flags = 0;
    flags |= if font.bold { font_flag::BOLD } else { 0 };
    flags |= if font.italic { font_flag::ITALIC } else { 0 };
    flags |= if font.monospace {
        font_flag::MONOSPACE
    } else {
        0
    };
    flags |= match font.align {
        rustos_system_ui::TextAlign::Start => 0,
        rustos_system_ui::TextAlign::Center => font_flag::ALIGN_CENTER,
        rustos_system_ui::TextAlign::End => font_flag::ALIGN_END,
    };
    flags |= if font.vertical_center {
        font_flag::VERTICAL_CENTER
    } else {
        0
    };
    flags
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustos_system_ui::{CommandId, LayoutSpec, Length, Runtime, TextAlign, Theme};

    #[test]
    fn hidpi_is_applied_before_gpu_rasterization() {
        let metrics = WindowMetrics::from_physical(2048, 1280, 1600).unwrap();
        let mut backend = GpuRenderBackend::<8>::new(metrics).unwrap();
        backend.rounded_fill(
            Rect::new(10, 20, 120, 36),
            Color::rgb(20, 80, 240),
            10,
            Rect::new(0, 0, 1280, 800),
        );
        let instance = backend.finish().unwrap()[0];
        assert_eq!(instance.bounds.x, 16);
        assert_eq!(instance.bounds.y, 32);
        assert_eq!(instance.bounds.width, 192);
        assert_eq!(instance.bounds.height, 58);
        assert_eq!(instance.radius_px, 16);
        assert_eq!(backend.metrics().compositor_scale_milli(), 1000);
    }

    #[test]
    fn text_and_image_remain_atlas_resources_not_cpu_pixels() {
        let mut backend = GpuRenderBackend::<8>::new(WindowMetrics::one_to_one(800, 600)).unwrap();
        let font = FontSpec {
            size: 16,
            bold: true,
            italic: false,
            monospace: true,
            align: TextAlign::Center,
            vertical_center: true,
        };
        backend.text(
            Rect::new(20, 20, 200, 32),
            ResourceId(41),
            Color::rgb(255, 255, 255),
            font,
            Rect::new(0, 0, 800, 600),
        );
        backend.image(
            Rect::new(20, 60, 64, 64),
            ResourceId(77),
            Color::rgb(255, 255, 255),
            Rect::new(0, 0, 800, 600),
        );
        let frame = backend.finish().unwrap();
        assert_eq!(frame[0].kind, GpuPrimitiveKind::TextRun);
        assert_eq!(frame[0].resource, 41);
        assert_eq!(
            frame[0].font_flags & font_flag::MONOSPACE,
            font_flag::MONOSPACE
        );
        assert_eq!(frame[1].kind, GpuPrimitiveKind::Image);
        assert_eq!(frame[1].resource, 77);
    }

    #[test]
    fn overflow_rejects_complete_frame_and_batches_are_bounded() {
        let mut backend = GpuRenderBackend::<3>::new(WindowMetrics::one_to_one(800, 600)).unwrap();
        for x in [0, 10, 20] {
            backend.fill(
                Rect::new(x, 0, 8, 8),
                Color::rgb(1, 2, 3),
                Rect::new(0, 0, 800, 600),
            );
        }
        let sizes: [usize; 2] = {
            let mut result = [0; 2];
            for (index, batch) in backend.batches(2).unwrap().enumerate() {
                result[index] = batch.instances.len();
            }
            result
        };
        assert_eq!(sizes, [2, 1]);
        backend.fill(
            Rect::new(30, 0, 8, 8),
            Color::rgb(1, 2, 3),
            Rect::new(0, 0, 800, 600),
        );
        assert_eq!(backend.finish(), Err(GpuUiError::Capacity));
    }

    #[test]
    fn clipped_primitives_do_not_consume_instance_budget() {
        let mut backend = GpuRenderBackend::<1>::new(WindowMetrics::one_to_one(800, 600)).unwrap();
        backend.fill(
            Rect::new(0, 0, 10, 10),
            Color::rgb(1, 2, 3),
            Rect::new(20, 20, 10, 10),
        );
        assert!(backend.finish().unwrap().is_empty());
    }

    #[test]
    fn real_component_tree_compiles_through_gpu_backend() {
        let mut runtime = Runtime::<16, 64, 8>::new(Rect::new(0, 0, 640, 360), Theme::light());
        {
            let root = runtime.tree().root();
            let mut ui = runtime.builder();
            let column = ui
                .column(
                    root,
                    LayoutSpec {
                        width: Length::Fill(1),
                        height: Length::Fill(1),
                        gap: 8,
                        ..LayoutSpec::default()
                    },
                )
                .unwrap();
            ui.button(
                column,
                ResourceId(1),
                CommandId(1),
                LayoutSpec {
                    width: Length::Px(180),
                    height: Length::Px(40),
                    ..LayoutSpec::default()
                },
            )
            .unwrap();
            ui.checkbox(
                column,
                ResourceId(2),
                CommandId(2),
                LayoutSpec {
                    width: Length::Px(180),
                    height: Length::Px(36),
                    ..LayoutSpec::default()
                },
            )
            .unwrap();
            ui.image(
                column,
                ResourceId(3),
                ResourceId(4),
                LayoutSpec {
                    width: Length::Px(64),
                    height: Length::Px(64),
                    ..LayoutSpec::default()
                },
            )
            .unwrap();
        }
        let mut backend = GpuRenderBackend::<64>::new(WindowMetrics::one_to_one(640, 360)).unwrap();
        let result = runtime.render(&mut backend).unwrap();
        let instances = backend.finish().unwrap();
        assert!(result.commands > 0);
        assert!(instances
            .iter()
            .any(|instance| instance.kind == GpuPrimitiveKind::TextRun));
        assert!(instances
            .iter()
            .any(|instance| instance.kind == GpuPrimitiveKind::Image));
        assert!(instances
            .iter()
            .any(|instance| instance.kind == GpuPrimitiveKind::RoundedFill));
    }
}
