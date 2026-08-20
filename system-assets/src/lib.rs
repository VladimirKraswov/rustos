//! Системный каталог визуальных ресурсов RustOS.
//!
//! Главное правило слоя — приложения называют смысл (`Folder`, `Link`,
//! `ResizeHorizontal`), но не знают конкретных пикселей. Оконный сервер
//! выбирает активный пакет, рисует его и может заменить тему без перезапуска
//! приложений. Реестр не использует heap и потому пригоден уже на раннем boot.

#![no_std]
#![warn(missing_docs)]

mod cursor;
mod icon;
mod registry;
mod wallpaper;

pub use cursor::{
    CursorImage, CursorPack, CursorPalette, CursorPixel, HIGH_CONTRAST_CURSOR_PACK,
    LIGHT_CURSOR_PACK, MIDNIGHT_CURSOR_PACK,
};
pub use icon::{
    icon_for_path, IconKind, IconPack, IconPalette, IconTarget, CLASSIC_ICON_PACK,
    MIDNIGHT_ICON_PACK, MONO_ICON_PACK,
};
pub use registry::{PackId, PackMetadata, PackRegistry, RegistryError, ResourcePack};
pub use wallpaper::{wallpaper, Wallpaper, WallpaperId, WALLPAPERS};

#[cfg(test)]
extern crate std;
