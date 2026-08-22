# Мышь, курсоры, иконки и обои RustOS

## Что уже работает

Этот этап отделяет семантику указателя и файлов от конкретных картинок.
Приложение просит `Link`, `Folder` или `DynamicLibrary`, а системный resource
service выбирает активный пакет. Поэтому смена темы не требует пересборки и не
дублирует изображения во всех приложениях.

Реализованы:

- стабильные `PointerCursor`, `MouseSettings` и `MouseCapabilities` в ABI;
- xHCI USB HID mouse/keyboard на AMD64 и AArch64; PS/2 умеет аппаратные
  sample rate 10/20/40/60/80/100/200 Гц и resolution level 0..3;
- software sensitivity 25..400%, acceleration 0..300% и fixed-point остаток;
- debounce одиночного клика, интервал двойного клика и drag threshold;
- автоматические Arrow, Text, Link, Grab, Grabbing, Busy, Crosshair,
  NotAllowed и четыре resize-курсора;
- 8-кадровый loader, который compositor обновляет маленьким damage даже без
  движения мыши;
- темы курсоров `light`, `midnight`, `contrast`;
- icon packs `aurora` (по умолчанию), `classic`, `midnight`, `mono` и более
  тридцати семантических типов;
- три природных RGB565-фона: spring, autumn и winter;
- bounded-реестр `install/remove/select` без heap.

## Границы системы

```text
xHCI USB HID, PS/2 fallback, virtio-input fallback
              │ raw reports
              ▼
          input service ── MouseSettings ── settings UI / terminal
              │ normalized pointer events
              ▼
          window server ── hit-test ── PointerCursor semantic value
              │
              ▼
         cursor service ── active CursorPack ── damage-only compositor

VFS path ── icon_for_path ── IconKind ── active IconPack ── CPU/GPU target
```

Все перечисленные устройства — transport drivers, а не часть UI API. При
переключении USB/PS2/virtio `MouseSettings`, cursor service и приложения
остаются прежними. Если устройство не умеет менять hardware rate/resolution, драйвер
возвращает это в `MouseCapabilities`, а software speed/click settings всё равно
работают.

## Автоматический курсор окон

| Область | `PointerCursor` |
|---|---|
| desktop, пустая область | `Arrow` |
| terminal/text content | `Text` |
| кнопка, taskbar, ярлык, menu item | `Link` |
| свободный заголовок окна | `Grab` |
| перетаскиваемый заголовок | `Grabbing` |
| левая/правая рамка | `ResizeHorizontal` |
| верхняя/нижняя рамка | `ResizeVertical` |
| левый верхний/правый нижний угол | `ResizeNwSe` |
| правый верхний/левый нижний угол | `ResizeNeSw` |

`Busy`, `Crosshair` и `NotAllowed` доступны приложениям через ABI. Для
проверки всех форм без отдельного приложения есть `CURSOR PREVIEW`.

Hotspot хранится в `CursorImage`, а не предполагается compositor'ом. Cursor
service перед рисованием сохраняет 24×24 пикселя фона и при следующем кадре
возвращает их. Обычное движение публикует только старую и новую cursor-область.

## Настройка мыши

```text
MOUSE INFO
MOUSE RATE 200
MOUSE RESOLUTION 3
MOUSE SENSITIVITY 125
MOUSE ACCELERATION 35
MOUSE DOUBLE 450
MOUSE DEBOUNCE 25
MOUSE DRAG 5
```

PS/2 driver перед программированием останавливает data reporting, отправляет
`F3 rate`, `E8 resolution` и снова включает `F4`. Каждый байт требует ACK; при
ошибке boot и клавиатура продолжают работать, а terminal явно пишет
`DEVICE DID NOT ACK`.

`SENSITIVITY` и `ACCELERATION` применяются программно. Деление ведётся с
остатком: при 25% четыре маленьких отчёта превращаются в один пиксель, а не
теряются. `DEBOUNCE` — минимальный интервал, подавляющий дребезг одиночного
клика. `DOUBLE` — максимальный интервал между двумя нажатиями. `DRAG` одновременно
ограничивает смещение двух кликов и задаёт будущий порог начала drag.

## Cursor и icon packs

```text
CURSOR THEME LIGHT|MIDNIGHT|CONTRAST
CURSOR PREVIEW ARROW|TEXT|LINK|GRAB|GRABBING|BUSY
CURSOR PREVIEW CROSSHAIR|FORBIDDEN|HRESIZE|VRESIZE|NWSE|NESW
CURSOR AUTO

ICONS THEME AURORA|CLASSIC|MIDNIGHT|MONO
```

Crate `rustos-system-assets` не зависит от kernel. Новый пакет создаётся через
`CursorPack::new` или `IconPack::new`, после чего добавляется в
`PackRegistry`. Registry имеет фиксированную capacity, отвергает duplicate ID,
может удалить активный пакет и атомарно выбрать первый оставшийся. Сейчас
bootstrap desktop регистрирует встроенные пакеты. После выделения `resourced`
эти же дескрипторы будут указывать на проверенные read-only страницы RUNE
resource package; ABI приложения от этого не изменится.

Современная `aurora`-тема включает жёлтые закрытые/открытые папки, обычный и
текстовый файл, Rust source, RUNE DLL, executable, image, audio, video,
archive, drive, settings, trash и многослойную terminal icon. Тот же словарь
содержит UI-действия Home, Search, Menu, Grid, Power, Info, Success, Warning,
Minimize, Maximize, Restore, Close и навигационные chevrons. Window manager и
приложения запрашивают именно `IconKind`, поэтому сменный пакет не меняет их
логику.

`IconTarget` предоставляет прямоугольные и скруглённые fill/stroke primitives.
Старый backend автоматически получает прямоугольный fallback, а framebuffer и
будущий GPU backend рисуют современную rounded geometry. Команда терминала
`ICONS THEME AURORA|CLASSIC|MIDNIGHT|MONO` переключает пакет без перезапуска.

`icon_for_path` знает основные расширения (`txt`, `md`, `rs`, `dll`, `rdll`,
`rune`, `elf`, изображения, звук, видео и архивы), но неизвестное расширение
всегда безопасно становится обычным файлом. VFS не зависит от presentation
policy.

## Обои

```text
WALLPAPER SPRING
WALLPAPER AUTUMN
WALLPAPER WINTER
```

Исходники 16:9 лежат в `system-assets/assets/wallpapers/source`. Ядро включает
640×360 RGB565-копии из `packed`: по 450 KiB на фон, без PNG/JPEG decoder и
heap. Compositor масштабирует изображение с сохранением пропорций (`cover`) и
один раз сохраняет готовый desktop layer. Поэтому смена фона стоит один полный
кадр, а дальнейшие движения окна и курсора не декодируют его повторно.

Опциональная перепаковка после замены PNG:

```sh
./scripts/pack-wallpapers.sh
```

Для обычной сборки ffmpeg не нужен: готовые детерминированные `.rgb565` уже
хранятся в репозитории.

## Тесты

```sh
cargo test -p rustos-system-assets -p rustos-abi
RUSTOS_BOOT_TEST=1 ./scripts/build.sh && ./scripts/test-boot.sh
./scripts/test-gui.sh
```

Unit-тесты проверяют видимость и hotspot каждого курсора, изменение кадров
loader, все icon primitives, case-insensitive extension mapping, bounds
RGB565 и lifecycle реестра пакетов.
