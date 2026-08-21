//! ABI разделяемых графических буферов.
//!
//! Дескриптор описывает память, но не владеет ею. Сам объект передаётся между
//! процессами отдельным [`crate::Handle`]. Благодаря этому один и тот же
//! контракт подходит CPU rasterizer'у, compositor'у, GPU и video decoder'у.

/// Первая версия ABI графических буферов.
pub const GRAPHICS_BUFFER_ABI_VERSION: u16 = 1;
/// Максимальное число planes одного буфера.
pub const GRAPHICS_BUFFER_MAX_PLANES: usize = 4;

/// Стабильный код формата пикселей.
///
/// В отличие от Rust `enum` неизвестный код можно безопасно отвергнуть или
/// передать более новому драйверу. Названия описывают порядок компонентов в
/// памяти и не зависят от endian-представления целого числа.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PixelFormatCode(pub u32);

impl PixelFormatCode {
    /// В памяти: R8, G8, B8, неиспользуемые 8 бит.
    pub const R8G8B8X8_UNORM: Self = Self(1);
    /// В памяти: B8, G8, R8, неиспользуемые 8 бит.
    pub const B8G8R8X8_UNORM: Self = Self(2);
    /// В памяти: B8, G8, R8, A8.
    pub const B8G8R8A8_UNORM: Self = Self(3);
    /// Packed RGB 5:6:5, 16 бит на pixel.
    pub const R5G6B5_UNORM: Self = Self(4);
    /// Один 8-битный нормализованный канал.
    pub const R8_UNORM: Self = Self(5);
    /// Packed RGB 10:10:10 и два неиспользуемых бита.
    pub const R10G10B10X2_UNORM: Self = Self(6);
    /// Четыре half-float компонента.
    pub const R16G16B16A16_FLOAT: Self = Self(7);
    /// Packed RGB 10:10:10 и 2-битный alpha-канал.
    pub const R10G10B10A2_UNORM: Self = Self(8);
    /// Y plane и interleaved UV plane, 8 бит на компонент, 4:2:0.
    pub const NV12: Self = Self(0x100);
    /// Y plane и interleaved UV plane, 10 значащих бит в 16-битных словах.
    pub const P010: Self = Self(0x101);
    /// Три независимых Y, U и V plane, 8 бит, 4:2:0.
    pub const YUV420: Self = Self(0x102);
    /// Packed Y0, U, Y1, V, 8 бит на компонент.
    pub const YUYV: Self = Self(0x103);
    /// Packed U, Y0, V, Y1, 8 бит на компонент.
    pub const UYVY: Self = Self(0x104);

    /// Проверяет, что код известен этой версии ABI.
    pub const fn is_known(self) -> bool {
        self.required_plane_count().is_some()
    }

    /// Возвращает обязательное число planes для формата.
    pub const fn required_plane_count(self) -> Option<u8> {
        match self {
            Self::NV12 | Self::P010 => Some(2),
            Self::YUV420 => Some(3),
            Self::R8G8B8X8_UNORM
            | Self::B8G8R8X8_UNORM
            | Self::B8G8R8A8_UNORM
            | Self::R5G6B5_UNORM
            | Self::R8_UNORM
            | Self::R10G10B10X2_UNORM
            | Self::R16G16B16A16_FLOAT
            | Self::R10G10B10A2_UNORM
            | Self::YUYV
            | Self::UYVY => Some(1),
            _ => None,
        }
    }

    /// Формат содержит цветовой alpha-канал.
    pub const fn has_alpha(self) -> bool {
        matches!(
            self,
            Self::B8G8R8A8_UNORM | Self::R16G16B16A16_FLOAT | Self::R10G10B10A2_UNORM
        )
    }

    /// Формат хранит YUV, а не RGB/одноканальные данные.
    pub const fn is_yuv(self) -> bool {
        matches!(
            self,
            Self::NV12 | Self::P010 | Self::YUV420 | Self::YUYV | Self::UYVY
        )
    }

    const fn minimum_row_bytes(self, width: u32, plane: usize) -> Option<u64> {
        let width = width as u64;
        let half = width / 2 + width % 2;
        match (self, plane) {
            (Self::R8G8B8X8_UNORM, 0)
            | (Self::B8G8R8X8_UNORM, 0)
            | (Self::B8G8R8A8_UNORM, 0)
            | (Self::R10G10B10X2_UNORM, 0)
            | (Self::R10G10B10A2_UNORM, 0) => width.checked_mul(4),
            (Self::R5G6B5_UNORM, 0) => width.checked_mul(2),
            (Self::R8_UNORM, 0) => Some(width),
            (Self::R16G16B16A16_FLOAT, 0) => width.checked_mul(8),
            (Self::NV12, 0) | (Self::YUV420, 0) => Some(width),
            (Self::NV12, 1) => half.checked_mul(2),
            (Self::P010, 0) => width.checked_mul(2),
            (Self::P010, 1) => half.checked_mul(4),
            (Self::YUV420, 1) | (Self::YUV420, 2) => Some(half),
            (Self::YUYV, 0) | (Self::UYVY, 0) => half.checked_mul(4),
            _ => None,
        }
    }

    const fn plane_height(self, height: u32, plane: usize) -> Option<u64> {
        let height = height as u64;
        match (self, plane) {
            (Self::NV12, 1) | (Self::P010, 1) | (Self::YUV420, 1 | 2) => {
                Some(height / 2 + height % 2)
            }
            (_, 0) => Some(height),
            _ => None,
        }
    }
}

/// Способ интерпретации alpha-компонента.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AlphaMode(pub u8);

impl AlphaMode {
    /// Alpha отсутствует или каждый pixel непрозрачен.
    pub const OPAQUE: Self = Self(1);
    /// RGB уже умножены на alpha; канонический режим compositor'а.
    pub const PREMULTIPLIED: Self = Self(2);
    /// RGB не умножены на alpha; импортёр при необходимости конвертирует их.
    pub const STRAIGHT: Self = Self(3);

    /// Проверяет известный режим.
    pub const fn is_known(self) -> bool {
        matches!(self, Self::OPAQUE | Self::PREMULTIPLIED | Self::STRAIGHT)
    }
}

/// Цветовые primaries.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ColorPrimaries(pub u16);

impl ColorPrimaries {
    /// sRGB/BT.709 primaries.
    pub const BT709: Self = Self(1);
    /// Display P3 primaries.
    pub const DISPLAY_P3: Self = Self(2);
    /// BT.2020 primaries.
    pub const BT2020: Self = Self(3);

    /// Проверяет известное значение.
    pub const fn is_known(self) -> bool {
        matches!(self, Self::BT709 | Self::DISPLAY_P3 | Self::BT2020)
    }
}

/// Функция переноса цвета.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransferFunction(pub u16);

impl TransferFunction {
    /// Стандартная sRGB transfer function.
    pub const SRGB: Self = Self(1);
    /// Линейные значения света.
    pub const LINEAR: Self = Self(2);
    /// HDR Perceptual Quantizer.
    pub const PQ: Self = Self(3);
    /// HDR Hybrid Log-Gamma.
    pub const HLG: Self = Self(4);

    /// Проверяет известное значение.
    pub const fn is_known(self) -> bool {
        matches!(self, Self::SRGB | Self::LINEAR | Self::PQ | Self::HLG)
    }
}

/// Матрица преобразования YUV в RGB.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ColorMatrix(pub u16);

impl ColorMatrix {
    /// RGB без YUV-преобразования.
    pub const IDENTITY: Self = Self(1);
    /// BT.601 YUV.
    pub const BT601: Self = Self(2);
    /// BT.709 YUV.
    pub const BT709: Self = Self(3);
    /// BT.2020 YUV.
    pub const BT2020: Self = Self(4);

    /// Проверяет известное значение.
    pub const fn is_known(self) -> bool {
        matches!(
            self,
            Self::IDENTITY | Self::BT601 | Self::BT709 | Self::BT2020
        )
    }
}

/// Диапазон кодированных цветовых значений.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ColorRange(pub u16);

impl ColorRange {
    /// Полный диапазон значений.
    pub const FULL: Self = Self(1);
    /// Ограниченный video range.
    pub const LIMITED: Self = Self(2);

    /// Проверяет известное значение.
    pub const fn is_known(self) -> bool {
        matches!(self, Self::FULL | Self::LIMITED)
    }
}

/// Полное описание цветового пространства buffer'а.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ColorDescription {
    /// Цветовые primaries.
    pub primaries: ColorPrimaries,
    /// Transfer function.
    pub transfer: TransferFunction,
    /// RGB identity либо YUV matrix.
    pub matrix: ColorMatrix,
    /// Full или limited range.
    pub range: ColorRange,
}

impl ColorDescription {
    /// Обычный SDR sRGB buffer.
    pub const SRGB: Self = Self {
        primaries: ColorPrimaries::BT709,
        transfer: TransferFunction::SRGB,
        matrix: ColorMatrix::IDENTITY,
        range: ColorRange::FULL,
    };

    /// Обычный SDR video buffer BT.709 limited range.
    pub const BT709_LIMITED: Self = Self {
        primaries: ColorPrimaries::BT709,
        transfer: TransferFunction::SRGB,
        matrix: ColorMatrix::BT709,
        range: ColorRange::LIMITED,
    };

    /// Все поля распознаны текущей версией ABI.
    pub const fn is_known(self) -> bool {
        self.primaries.is_known()
            && self.transfer.is_known()
            && self.matrix.is_known()
            && self.range.is_known()
    }
}

/// Назначения, для которых buffer разрешено использовать.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BufferUsage(pub u64);

impl BufferUsage {
    /// CPU может читать mapped память.
    pub const CPU_READ: Self = Self(1 << 0);
    /// CPU может записывать mapped память.
    pub const CPU_WRITE: Self = Self(1 << 1);
    /// Texture/sample source.
    pub const TEXTURE: Self = Self(1 << 2);
    /// Render target GPU или software renderer'а.
    pub const RENDER_TARGET: Self = Self(1 << 3);
    /// Primary/overlay scanout plane.
    pub const SCANOUT: Self = Self(1 << 4);
    /// Hardware cursor plane.
    pub const CURSOR: Self = Self(1 << 5);
    /// Поверхность video decoder'а.
    pub const VIDEO_DECODE: Self = Self(1 << 6);
    /// Поверхность video encoder'а.
    pub const VIDEO_ENCODE: Self = Self(1 << 7);
    /// Источник copy/transfer.
    pub const TRANSFER_SOURCE: Self = Self(1 << 8);
    /// Назначение copy/transfer.
    pub const TRANSFER_DESTINATION: Self = Self(1 << 9);
    /// Нет разрешённых операций.
    pub const NONE: Self = Self(0);
    /// Все биты версии 1.
    pub const KNOWN: Self = Self((1 << 10) - 1);

    /// Объединяет назначения.
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Проверяет наличие всех назначений.
    pub const fn contains(self, required: Self) -> bool {
        self.0 & required.0 == required.0
    }

    /// Неизвестные биты отсутствуют.
    pub const fn is_valid(self) -> bool {
        self.0 != 0 && self.0 & !Self::KNOWN.0 == 0
    }
}

/// Допустимые области размещения buffer'а.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MemoryDomain(pub u32);

impl MemoryDomain {
    /// Обычная системная RAM.
    pub const SYSTEM: Self = Self(1 << 0);
    /// Device-local память, например VRAM.
    pub const DEVICE_LOCAL: Self = Self(1 << 1);
    /// Объект разрешено импортировать в несколько процессов/devices.
    pub const SHARED: Self = Self(1 << 2);
    /// CPU может отображать память через graphics buffer manager.
    pub const HOST_VISIBLE: Self = Self(1 << 3);
    /// Защищённое содержимое, недоступное обычному CPU mapping.
    pub const PROTECTED: Self = Self(1 << 4);
    /// Все биты версии 1.
    pub const KNOWN: Self = Self((1 << 5) - 1);

    /// Объединяет допустимые области/свойства.
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Проверяет наличие свойства.
    pub const fn contains(self, required: Self) -> bool {
        self.0 & required.0 == required.0
    }

    /// Неизвестные биты отсутствуют.
    pub const fn is_valid(self) -> bool {
        self.0 != 0 && self.0 & !Self::KNOWN.0 == 0
    }
}

/// Пространство кодов layout modifier.
pub mod modifier {
    /// Линейные строки и planes без аппаратного tiling/compression.
    pub const LINEAR: u64 = 0;
}

/// Layout одного plane внутри общего memory object.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlaneLayout {
    /// Смещение первого байта plane.
    pub offset: u64,
    /// Расстояние между началами соседних строк в байтах.
    pub stride_bytes: u32,
    /// Зарезервировано; отправитель заполняет нулём.
    pub reserved: u32,
    /// Число байт, принадлежащих plane, начиная с `offset`.
    pub size_bytes: u64,
}

impl PlaneLayout {
    /// Пустая запись для неиспользуемого plane.
    pub const EMPTY: Self = Self {
        offset: 0,
        stride_bytes: 0,
        reserved: 0,
        size_bytes: 0,
    };

    /// Создаёт layout используемого plane.
    pub const fn new(offset: u64, stride_bytes: u32, size_bytes: u64) -> Self {
        Self {
            offset,
            stride_bytes,
            reserved: 0,
            size_bytes,
        }
    }

    const fn is_empty(self) -> bool {
        self.offset == 0 && self.stride_bytes == 0 && self.reserved == 0 && self.size_bytes == 0
    }
}

/// Полное wire-описание одного graphics buffer.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GraphicsBufferDesc {
    /// [`GRAPHICS_BUFFER_ABI_VERSION`].
    pub version: u16,
    /// Размер структуры в байтах.
    pub size: u16,
    /// Ширина luma/RGB plane в физических пикселях.
    pub width: u32,
    /// Высота luma/RGB plane в физических пикселях.
    pub height: u32,
    /// Стабильный код pixel format.
    pub format: PixelFormatCode,
    /// Число используемых элементов `planes`.
    pub plane_count: u8,
    /// [`AlphaMode`].
    pub alpha_mode: AlphaMode,
    /// Зарезервировано; отправитель заполняет нулём.
    pub reserved_header: u16,
    /// Явное padding-поле перед 64-битными значениями.
    pub reserved_alignment: u32,
    /// Разрешённые операции.
    pub usage: BufferUsage,
    /// Допустимое/фактическое размещение памяти.
    pub memory_domains: MemoryDomain,
    /// В версии 1 должно быть равно нулю.
    pub flags: u32,
    /// Общий размер memory object в байтах.
    pub byte_size: u64,
    /// Vendor-namespaced tiling/compression modifier или [`modifier::LINEAR`].
    pub modifier: u64,
    /// Цветовое пространство содержимого.
    pub color: ColorDescription,
    /// Layout до четырёх planes.
    pub planes: [PlaneLayout; GRAPHICS_BUFFER_MAX_PLANES],
    /// Зарезервировано для расширения ABI; отправитель заполняет нулями.
    pub reserved_tail: [u64; 2],
}

/// Причина отказа от graphics buffer descriptor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GraphicsBufferError {
    /// Версия не поддерживается.
    UnsupportedVersion,
    /// Размер структуры не совпадает с выбранной версией.
    UnsupportedSize,
    /// Размер изображения или memory object равен нулю.
    Empty,
    /// Pixel format неизвестен.
    UnsupportedFormat,
    /// Число planes не соответствует формату.
    InvalidPlaneCount,
    /// Usage содержит неизвестные биты или пуст.
    InvalidUsage,
    /// Memory domain содержит неизвестные биты или пуст.
    InvalidMemoryDomain,
    /// Alpha mode неизвестен или несовместим с форматом.
    InvalidAlphaMode,
    /// Color description неизвестен или несовместим с RGB/YUV.
    InvalidColor,
    /// Зарезервированное поле или неизвестный флаг не равен нулю.
    ReservedNonZero,
    /// Plane имеет некорректные stride, offset или size.
    InvalidPlaneLayout,
    /// Linear planes перекрываются.
    OverlappingPlanes,
    /// Арифметика размера переполнилась.
    SizeOverflow,
    /// Protected buffer запрошен одновременно с CPU-доступом.
    ProtectedCpuAccess,
}

impl GraphicsBufferDesc {
    /// Строит плотно упакованный linear descriptor без скрытых padding rows.
    /// Production allocator может увеличить strides/offsets, сохранив те же
    /// правила [`Self::validate`].
    pub fn linear(
        width: u32,
        height: u32,
        format: PixelFormatCode,
        usage: BufferUsage,
        memory_domains: MemoryDomain,
    ) -> Result<Self, GraphicsBufferError> {
        if width == 0 || height == 0 {
            return Err(GraphicsBufferError::Empty);
        }
        let plane_count = format
            .required_plane_count()
            .ok_or(GraphicsBufferError::UnsupportedFormat)?;
        let mut planes = [PlaneLayout::EMPTY; GRAPHICS_BUFFER_MAX_PLANES];
        let mut next_offset = 0u64;
        let mut plane = 0usize;
        while plane < plane_count as usize {
            let row_bytes = format
                .minimum_row_bytes(width, plane)
                .ok_or(GraphicsBufferError::UnsupportedFormat)?;
            let plane_height = format
                .plane_height(height, plane)
                .ok_or(GraphicsBufferError::UnsupportedFormat)?;
            let size_bytes = row_bytes
                .checked_mul(plane_height)
                .ok_or(GraphicsBufferError::SizeOverflow)?;
            let stride_bytes =
                u32::try_from(row_bytes).map_err(|_| GraphicsBufferError::SizeOverflow)?;
            planes[plane] = PlaneLayout::new(next_offset, stride_bytes, size_bytes);
            next_offset = next_offset
                .checked_add(size_bytes)
                .ok_or(GraphicsBufferError::SizeOverflow)?;
            plane += 1;
        }
        let descriptor = Self {
            version: GRAPHICS_BUFFER_ABI_VERSION,
            size: core::mem::size_of::<Self>() as u16,
            width,
            height,
            format,
            plane_count,
            alpha_mode: if format.has_alpha() {
                AlphaMode::PREMULTIPLIED
            } else {
                AlphaMode::OPAQUE
            },
            reserved_header: 0,
            reserved_alignment: 0,
            usage,
            memory_domains,
            flags: 0,
            byte_size: next_offset,
            modifier: modifier::LINEAR,
            color: if format.is_yuv() {
                ColorDescription::BT709_LIMITED
            } else {
                ColorDescription::SRGB
            },
            planes,
            reserved_tail: [0; 2],
        };
        descriptor.validate()?;
        Ok(descriptor)
    }

    /// Проверяет wire descriptor до mapping или передачи устройству.
    pub fn validate(&self) -> Result<(), GraphicsBufferError> {
        if self.version != GRAPHICS_BUFFER_ABI_VERSION {
            return Err(GraphicsBufferError::UnsupportedVersion);
        }
        if self.size as usize != core::mem::size_of::<Self>() {
            return Err(GraphicsBufferError::UnsupportedSize);
        }
        if self.width == 0 || self.height == 0 || self.byte_size == 0 {
            return Err(GraphicsBufferError::Empty);
        }
        let required_planes = self
            .format
            .required_plane_count()
            .ok_or(GraphicsBufferError::UnsupportedFormat)?;
        if self.plane_count != required_planes {
            return Err(GraphicsBufferError::InvalidPlaneCount);
        }
        if !self.usage.is_valid() {
            return Err(GraphicsBufferError::InvalidUsage);
        }
        if !self.memory_domains.is_valid() {
            return Err(GraphicsBufferError::InvalidMemoryDomain);
        }
        if !self.alpha_mode.is_known()
            || (!self.format.has_alpha() && self.alpha_mode != AlphaMode::OPAQUE)
        {
            return Err(GraphicsBufferError::InvalidAlphaMode);
        }
        if !self.color.is_known()
            || (self.format.is_yuv() && self.color.matrix == ColorMatrix::IDENTITY)
            || (!self.format.is_yuv() && self.color.matrix != ColorMatrix::IDENTITY)
        {
            return Err(GraphicsBufferError::InvalidColor);
        }
        if self.flags != 0
            || self.reserved_header != 0
            || self.reserved_alignment != 0
            || self.reserved_tail != [0; 2]
        {
            return Err(GraphicsBufferError::ReservedNonZero);
        }
        if self.memory_domains.contains(MemoryDomain::PROTECTED)
            && (self.memory_domains.contains(MemoryDomain::HOST_VISIBLE)
                || self.usage.0 & BufferUsage::CPU_READ.union(BufferUsage::CPU_WRITE).0 != 0)
        {
            return Err(GraphicsBufferError::ProtectedCpuAccess);
        }

        let mut plane = 0usize;
        while plane < GRAPHICS_BUFFER_MAX_PLANES {
            let layout = self.planes[plane];
            if plane >= self.plane_count as usize {
                if !layout.is_empty() {
                    return Err(GraphicsBufferError::InvalidPlaneLayout);
                }
                plane += 1;
                continue;
            }
            if layout.reserved != 0 || layout.stride_bytes == 0 || layout.size_bytes == 0 {
                return Err(GraphicsBufferError::InvalidPlaneLayout);
            }
            let minimum_row = self
                .format
                .minimum_row_bytes(self.width, plane)
                .ok_or(GraphicsBufferError::InvalidPlaneLayout)?;
            let plane_height = self
                .format
                .plane_height(self.height, plane)
                .ok_or(GraphicsBufferError::InvalidPlaneLayout)?;
            if u64::from(layout.stride_bytes) < minimum_row {
                return Err(GraphicsBufferError::InvalidPlaneLayout);
            }
            let required_size = plane_height
                .saturating_sub(1)
                .checked_mul(u64::from(layout.stride_bytes))
                .and_then(|rows| rows.checked_add(minimum_row))
                .ok_or(GraphicsBufferError::SizeOverflow)?;
            let end = layout
                .offset
                .checked_add(layout.size_bytes)
                .ok_or(GraphicsBufferError::SizeOverflow)?;
            if required_size > layout.size_bytes || end > self.byte_size {
                return Err(GraphicsBufferError::InvalidPlaneLayout);
            }
            plane += 1;
        }

        if self.modifier == modifier::LINEAR {
            let mut left = 0usize;
            while left < self.plane_count as usize {
                let left_end = self.planes[left]
                    .offset
                    .checked_add(self.planes[left].size_bytes)
                    .ok_or(GraphicsBufferError::SizeOverflow)?;
                let mut right = left + 1;
                while right < self.plane_count as usize {
                    let right_end = self.planes[right]
                        .offset
                        .checked_add(self.planes[right].size_bytes)
                        .ok_or(GraphicsBufferError::SizeOverflow)?;
                    if self.planes[left].offset < right_end && self.planes[right].offset < left_end
                    {
                        return Err(GraphicsBufferError::OverlappingPlanes);
                    }
                    right += 1;
                }
                left += 1;
            }
        }
        Ok(())
    }
}

const _: () = assert!(core::mem::size_of::<PixelFormatCode>() == 4);
const _: () = assert!(core::mem::size_of::<ColorDescription>() == 8);
const _: () = assert!(core::mem::size_of::<PlaneLayout>() == 24);
const _: () = assert!(core::mem::size_of::<GraphicsBufferDesc>() == 176);
const _: () = assert!(core::mem::align_of::<GraphicsBufferDesc>() == 8);

#[cfg(test)]
mod tests {
    use super::*;

    fn cpu_usage() -> BufferUsage {
        BufferUsage::CPU_READ
            .union(BufferUsage::CPU_WRITE)
            .union(BufferUsage::TEXTURE)
    }

    fn shared_memory() -> MemoryDomain {
        MemoryDomain::SYSTEM
            .union(MemoryDomain::HOST_VISIBLE)
            .union(MemoryDomain::SHARED)
    }

    #[test]
    fn packed_linear_descriptor_is_tight_and_valid() {
        let descriptor = GraphicsBufferDesc::linear(
            1920,
            1080,
            PixelFormatCode::B8G8R8A8_UNORM,
            cpu_usage(),
            shared_memory(),
        )
        .unwrap();
        assert_eq!(descriptor.plane_count, 1);
        assert_eq!(descriptor.planes[0].stride_bytes, 1920 * 4);
        assert_eq!(descriptor.byte_size, 1920 * 1080 * 4);
        assert_eq!(descriptor.alpha_mode, AlphaMode::PREMULTIPLIED);
        assert_eq!(descriptor.validate(), Ok(()));
    }

    #[test]
    fn odd_nv12_uses_two_non_overlapping_planes() {
        let descriptor = GraphicsBufferDesc::linear(
            5,
            3,
            PixelFormatCode::NV12,
            BufferUsage::VIDEO_DECODE.union(BufferUsage::TEXTURE),
            shared_memory(),
        )
        .unwrap();
        assert_eq!(descriptor.plane_count, 2);
        assert_eq!(descriptor.planes[0], PlaneLayout::new(0, 5, 15));
        assert_eq!(descriptor.planes[1], PlaneLayout::new(15, 6, 12));
        assert_eq!(descriptor.byte_size, 27);
    }

    #[test]
    fn rejects_reserved_overlap_and_too_short_plane() {
        let mut descriptor = GraphicsBufferDesc::linear(
            8,
            8,
            PixelFormatCode::YUV420,
            BufferUsage::TEXTURE,
            shared_memory(),
        )
        .unwrap();
        descriptor.reserved_tail[1] = 1;
        assert_eq!(
            descriptor.validate(),
            Err(GraphicsBufferError::ReservedNonZero)
        );
        descriptor.reserved_tail = [0; 2];
        descriptor.planes[1].offset = 1;
        assert_eq!(
            descriptor.validate(),
            Err(GraphicsBufferError::OverlappingPlanes)
        );
        descriptor.planes[1].offset = descriptor.planes[0].size_bytes;
        descriptor.planes[0].stride_bytes = 1;
        assert_eq!(
            descriptor.validate(),
            Err(GraphicsBufferError::InvalidPlaneLayout)
        );
    }

    #[test]
    fn protected_memory_cannot_be_cpu_visible() {
        let result = GraphicsBufferDesc::linear(
            64,
            64,
            PixelFormatCode::R8G8B8X8_UNORM,
            BufferUsage::CPU_WRITE,
            MemoryDomain::DEVICE_LOCAL.union(MemoryDomain::PROTECTED),
        );
        assert_eq!(result, Err(GraphicsBufferError::ProtectedCpuAccess));
    }

    #[test]
    fn version_and_size_are_rejected_before_mapping() {
        let mut descriptor = GraphicsBufferDesc::linear(
            64,
            64,
            PixelFormatCode::R8G8B8X8_UNORM,
            BufferUsage::SCANOUT,
            MemoryDomain::SYSTEM,
        )
        .unwrap();
        descriptor.version += 1;
        assert_eq!(
            descriptor.validate(),
            Err(GraphicsBufferError::UnsupportedVersion)
        );
        descriptor.version = GRAPHICS_BUFFER_ABI_VERSION;
        descriptor.size = 0;
        assert_eq!(
            descriptor.validate(),
            Err(GraphicsBufferError::UnsupportedSize)
        );
    }
}
