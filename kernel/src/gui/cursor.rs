//! Cursor service bootstrap-композитора.
//!
//! Здесь живёт сохранение фона и отрисовка, а сами изображения/темы — в
//! `rustos-system-assets`. Это важная граница: замена cursor pack не требует
//! менять input driver или window manager.

use crate::graphics::{Framebuffer, Rect};
use rustos_abi::input::PointerCursor;
use rustos_system_assets::{
    CursorImage, CursorPack, CursorPixel, PackId, PackRegistry, ResourcePack,
    HIGH_CONTRAST_CURSOR_PACK, LIGHT_CURSOR_PACK, MIDNIGHT_CURSOR_PACK,
};

const CURSOR_SIDE: usize = 24;
const ANIMATION_INTERVAL_MS: u64 = 80;

/// Курсор с back-save, bounded theme registry и анимацией busy spinner.
pub struct Cursor {
    saved: [u32; CURSOR_SIDE * CURSOR_SIDE],
    pointer_x: i32,
    pointer_y: i32,
    image_x: i32,
    image_y: i32,
    valid: bool,
    automatic_kind: PointerCursor,
    forced_kind: Option<PointerCursor>,
    frame: u8,
    last_animation_ms: u64,
    packs: PackRegistry<CursorPack, 8>,
}

impl Cursor {
    /// Создаёт cursor service и регистрирует три встроенные темы. Внешний
    /// resource service сможет занять ещё пять bounded-слотов.
    pub fn new() -> Self {
        let mut packs = PackRegistry::new();
        let _ = packs.install(LIGHT_CURSOR_PACK);
        let _ = packs.install(MIDNIGHT_CURSOR_PACK);
        let _ = packs.install(HIGH_CONTRAST_CURSOR_PACK);
        Self {
            saved: [0; CURSOR_SIDE * CURSOR_SIDE],
            pointer_x: 0,
            pointer_y: 0,
            image_x: 0,
            image_y: 0,
            valid: false,
            automatic_kind: PointerCursor::Arrow,
            forced_kind: None,
            frame: 0,
            last_animation_ms: 0,
            packs,
        }
    }

    /// Область последнего или следующего sprite, включая hotspot.
    pub fn rect(&self) -> Rect {
        let image = self.image();
        Rect::new(
            if self.valid {
                self.image_x
            } else {
                self.pointer_x - i32::from(image.hotspot_x)
            },
            if self.valid {
                self.image_y
            } else {
                self.pointer_y - i32::from(image.hotspot_y)
            },
            u32::from(image.width),
            u32::from(image.height),
        )
    }

    /// Сбрасывает back-save после полной перерисовки сцены.
    pub fn invalidate(&mut self) {
        self.valid = false;
    }

    /// Возвращает пиксели под старым sprite.
    pub fn restore(&mut self, framebuffer: &mut Framebuffer) {
        if !self.valid {
            return;
        }
        for dy in 0..CURSOR_SIDE {
            for dx in 0..CURSOR_SIDE {
                let x = self.image_x + dx as i32;
                let y = self.image_y + dy as i32;
                if x >= 0
                    && y >= 0
                    && x < framebuffer.width() as i32
                    && y < framebuffer.height() as i32
                {
                    framebuffer.write_raw(x as u32, y as u32, self.saved[dy * CURSOR_SIDE + dx]);
                }
            }
        }
        self.valid = false;
    }

    /// Выбирает форму по текущему hit-test. В preview-режиме запрос
    /// запоминается, но отображение остаётся зафиксированным.
    pub fn set_automatic_kind(&mut self, kind: PointerCursor) {
        if self.automatic_kind != kind {
            self.automatic_kind = kind;
            self.frame = 0;
        }
    }

    /// Фиксирует вид для UI preview (`None` возвращает автоматический режим).
    pub fn set_preview(&mut self, kind: Option<PointerCursor>) {
        self.forced_kind = kind;
        self.frame = 0;
        self.last_animation_ms = 0;
    }

    /// Меняет cursor pack.
    pub fn select_theme(&mut self, id: PackId) -> bool {
        self.packs.select(id).is_ok()
    }

    /// Имя активной темы.
    pub fn theme_name(&self) -> &'static str {
        self.packs
            .active()
            .map_or("none", |pack| pack.metadata().name)
    }

    /// Текущая семантическая форма.
    pub fn kind(&self) -> PointerCursor {
        self.forced_kind.unwrap_or(self.automatic_kind)
    }

    /// Продвигает только анимированный busy cursor. Возвращает true, если
    /// compositor должен восстановить и заново представить маленький sprite.
    pub fn animate(&mut self, now_ms: u64) -> bool {
        if self.kind() != PointerCursor::Busy {
            return false;
        }
        if self.last_animation_ms == 0 {
            self.last_animation_ms = now_ms;
            return false;
        }
        if now_ms.saturating_sub(self.last_animation_ms) < ANIMATION_INTERVAL_MS {
            return false;
        }
        self.last_animation_ms = now_ms;
        self.frame = self.frame.wrapping_add(1) % 8;
        true
    }

    /// Сохраняет фон и рисует cursor sprite в точке указателя.
    pub fn draw(&mut self, framebuffer: &mut Framebuffer, pointer_x: i32, pointer_y: i32) {
        self.pointer_x = pointer_x;
        self.pointer_y = pointer_y;
        let image = self.image();
        self.image_x = pointer_x - i32::from(image.hotspot_x);
        self.image_y = pointer_y - i32::from(image.hotspot_y);
        let pack = self.packs.active().unwrap_or(LIGHT_CURSOR_PACK);
        for dy in 0..CURSOR_SIDE {
            for dx in 0..CURSOR_SIDE {
                let x = self.image_x + dx as i32;
                let y = self.image_y + dy as i32;
                if x < 0
                    || y < 0
                    || x >= framebuffer.width() as i32
                    || y >= framebuffer.height() as i32
                {
                    continue;
                }
                self.saved[dy * CURSOR_SIDE + dx] = framebuffer.read_raw(x as u32, y as u32);
                match pack.pixel(image, dx as u16, dy as u16) {
                    CursorPixel::Transparent => {}
                    CursorPixel::Shadow => framebuffer.blend_pixel(x, y, pack.palette.shadow, 115),
                    CursorPixel::Outline => framebuffer.put_pixel(x, y, pack.palette.outline),
                    CursorPixel::Fill => framebuffer.put_pixel(x, y, pack.palette.fill),
                    CursorPixel::Accent => framebuffer.put_pixel(x, y, pack.palette.accent),
                }
            }
        }
        self.valid = true;
    }

    fn image(&self) -> CursorImage {
        CursorImage::new(self.kind(), self.frame)
    }
}
