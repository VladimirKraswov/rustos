# Системные шрифты RustOS

## Цели

Ранний GUI обязан показать диагностику даже при повреждённой VFS, поэтому
растеризованные глифы встроены в kernel image. Runtime не читает TTF, не
использует heap и не выполняет floating-point код. Позже тот же versioned API
перейдёт в изолированный font service; приложения не должны хранить собственную
копию renderer'а.

Система предоставляет два стабильных семейства:

- `FontFamily::Console` — моноширинное для terminal, логов, editor и debugger;
- `FontFamily::Sans` — пропорциональное sans-serif для окон и UI, по назначению
  соответствует Arial/Helvetica, но не копирует их метрики или имя.

Latin растеризуется из M+ Code/M+ 1 в 4-bit grayscale. Кириллица берётся из
X11 Cyrillic (console) и Inconsolata Cyrillic (sans); это не fallback-квадраты,
а полные русские глифы U+0400..U+052F, включая `Ё` и `ё`. M+ распространяется
по SIL Open Font License; данные X11/Inconsolata поставляются через
лицензированный набор U8g2.

## API

`FontStyle` не содержит указателей и allocation:

```rust
let normal = FontStyle::sans(15);
let heading = FontStyle::sans(24).bold();
let code = FontStyle::console(18);
let comment = FontStyle::console(16).italic();

let metrics = font::measure_text("Привет, RustOS!", heading);
font::draw_text(framebuffer, x, y, "Привет, RustOS!", color, heading);
```

Поле `size` — em-size в пикселях, допустимый диапазон 10..=48. Высота строки
сейчас равна `4/3 em`: этого хватает для диакритики и кириллицы, но terminal не
получает чрезмерный типографский line gap. `Regular` и `Bold` выбирают отдельные
bitmap faces, где они доступны; для кириллического Sans жирность добавляет
bounded raster pass. `Italic` выполняет integer shear, а `BoldItalic`
совмещает оба механизма.

## Terminal

Terminal по умолчанию использует `Console 18 Regular`. Настройки доступны
непосредственно из shell:

```text
FONT
FONT FAMILY CONSOLE
FONT FAMILY SANS
FONT SIZE 20
FONT STYLE REGULAR
FONT STYLE BOLD
FONT STYLE ITALIC
FONT STYLE BOLDITALIC
```

Вывод программ и `CAT` декодируется как UTF-8. Scrollback хранит BMP code
point и индекс палитры в компактной 4-байтной клетке: Latin/Cyrillic
поддерживаются без удвоения terminal buffer и риска для раннего 128-KiB kernel
stack. Невалидный UTF-8 показывается символом замены `�`.

## Текущие границы

- есть Basic Latin, Latin-1 и Cyrillic; полного Unicode пока нет;
- нет ligatures, bidi, shaping и font fallback для сложных письменностей;
- scaling 10..=48 integer/nearest для bitmap; Latin использует 4-bit
  antialiasing в базовом размере;
- keyboard layout пока вводит ASCII, но русский UTF-8 уже корректно приходит
  из файлов, IPC и stdout программ.
