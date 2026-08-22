# GUI RustOS

Архитектура декларативной системной библиотеки, stable ABI, `.rui`,
layout/event/display-list pipeline и UI Gallery описаны в
[`SYSTEM_UI.md`](SYSTEM_UI.md). Этот файл документирует bootstrap desktop и
его прямую интеграцию с текущим compositor.

## Слои

- `graphics.rs` — clipping, RGB/BGR packing, RAM back buffer и scanout present;
- `rustos-video` — безопасные CPU surfaces, blit/alpha, damage и layers;
- `font.rs` — системные Console/Sans bitmap fonts, UTF-8 и типографический API;
- `input/` — xHCI USB HID, PS/2/virtio fallback и нормализованные события;
- `rustos-system-assets` — cursor/icon packs и CPU-friendly обои;
- `gui/components.rs` — theme и базовые widgets;
- `gui/session.rs` — compositor, desktop, taskbar, registry окон и lifecycle
  независимых экземпляров приложений;
- `apps/terminal.rs` — первый графический клиент;
- `apps/file_explorer.rs` — независимый компонентный Проводник с общим VFS;
  полный сценарий описан в [FILE_EXPLORER.md](FILE_EXPLORER.md).

На QEMU ядро после GRUB hand-off подключает modern PCI virtio-gpu,
читает EDID и переводит scanout на выбранный wide mode. GRUB framebuffer
остаётся fallback для машин без поддерживаемого display device. Rendering
полностью выполняется CPU, закрытые GPU-драйверы не требуются.

На HiDPI/Retina monitor native EDID mode остаётся в `DISPLAY MODES`, но
стартовый logical scanout ограничен комфортным 1600×900. Поэтому интерфейс не
становится микроскопическим до появления полноценного fractional DPI scaling.
Для monitor меньше лимита сохраняется его preferred mode; после загрузки любой
поддержанный режим по-прежнему выбирается командой `DISPLAY MODE WxH`.

## Pixel mapping и HiDPI

На macOS интерактивный `make run` использует policy `integer`: читает размер
Cocoa backing surface основного экрана, подбирает wide guest mode не больше
1600×900 и открывает его fullscreen с целым коэффициентом ×2/×3/×4. Например,
2880×1800 получает guest 1440×900 ×2, а внешний 2048×1152 — 1024×576 ×2.
QEMU zoom interpolation отключён, поэтому каждый guest pixel превращается в
одинаковый квадрат физических пикселей, но окно остаётся крупным.

Доступны три явные policy:

```sh
make run                                  # integer-fit на основном экране macOS
RUSTOS_DISPLAY_POLICY=actual make run    # маленькое эталонное окно 1:1
RUSTOS_DISPLAY_POLICY=fit make run       # заполнение экрана, scale может быть дробным
```

Для нескольких экранов `RUSTOS_HOST_DISPLAY=0|1|...` выбирает профиль, а
launcher печатает host surface, guest mode и коэффициент до запуска. EDID можно
переопределить вручную; оба размера задаются вместе:

```sh
RUSTOS_DISPLAY_WIDTH=1920 RUSTOS_DISPLAY_HEIGHT=1080 make run
```

Cocoa не предоставляет QEMU аргумент для принудительного выбора монитора:
нужный экран должен быть основным/активным при открытии fullscreen. Поэтому
virtio-gpu повторяет расчёт уже из фактической host surface во время загрузки;
если предварительный профиль launcher не совпал с экраном окна, scanout всё
равно получает корректный integer mode.

`RUSTOS_FULLSCREEN` и `RUSTOS_FIT_TO_WINDOW` остаются низкоуровневыми
override'ами policy. На Linux безопасный default — `actual`, потому что Wayland
и X11 не дают одного переносимого способа определить backing surface; integer
mode можно указать вручную после просмотра разрешения host.

На уровне UI уже действует отдельный `WindowMetrics`: logical size, physical
raster surface и `device_scale_milli`. Значение хранится как fixed-point
(`1600` = `1.600`), а compositor scale остаётся `1.000`. Сейчас bootstrap
desktop честно работает в 1:1; fractional constructor и conversions покрыты
host-тестами и являются границей следующего этапа — применения scale ко всем
display primitives и glyph rasterization, а не растяжения готового кадра.

Команда `DISPLAY` выводит диагностику `LOGICAL RESOLUTION`, `PHYSICAL SURFACE`,
`DEVICE SCALE`, `FRAMEBUFFER` и `COMPOSITOR SCALE`. Те же значения доступны в
serial marker `[display-metrics]`, который проверяет GUI regression test.

Подробный контракт scanout/surfaces/compositor и путь к software OpenGL описан
в [VIDEO.md](VIDEO.md). Команды, события, style-флаги и state machine окон
описаны отдельно в [WINDOWS.md](WINDOWS.md), а семейства, размеры и
начертания — в [FONTS.md](FONTS.md). Курсоры, настройки мыши, icon packs и
обои документированы в [SYSTEM_ASSETS.md](SYSTEM_ASSETS.md).

Компоненты никогда не рисуют непосредственно в видимый framebuffer.
Compositor сначала формирует кадр в back buffer из обычной RAM и только потом
публикует его damage-областями. Перед move gesture отдельный bounded layer
сохраняет полностью отрисованное окно, а background layer — desktop и все
остальные окна. На каждом mouse packet compositor восстанавливает старую
область и переносит готовый window layer в новую: пользователь всегда видит
содержимое, controls и тень, но дорогое приложение не растеризуется заново.
Resize меняет layout, поэтому честно рисует новый полный client frame.

Input service объединяет накопившиеся движения с неизменным состоянием кнопок.
Переходы mouse-down/up сохраняются отдельно, но вместо очереди устаревших
промежуточных кадров compositor сразу рисует последнюю позицию. Обычное
движение курсора публикует две маленькие dirty-области, а ввод символа — только
текущую строку terminal.

Hover проходит сквозным incremental path: component runtime возвращает bounds
старого и нового target, window server перерисовывает только display commands
этого application viewport, compositor добавляет старую/новую позицию курсора,
а `virtio-gpu` получает те же rectangles. `consumed=true` без изменения state
не считается repaint. Первый реальный кадр каждого scope отмечается marker'ом
`[compositor] repaint=incremental ... full-screen=no`; GUI-тест ограничивает
application hover бюджетом 299 kpx против 1024 kpx полного кадра 1280×800.

Примитивы UI не являются «низкоразрешёнными bitmap». Прямые участки cards,
buttons, checkbox, radio и toggle остаются быстрыми span-fill, а их curved edge
использует 4×4 coverage supersampling только внутри небольших corner tiles.
Поэтому скругления сглажены, но стоимость не зависит от полной площади окна.

Размер буферов вычисляется из выбранного monitor mode и не ограничен константой:
1280×800 занимает около 4 MiB на слой, 4K — около 32 MiB. Если для второго
слоя не хватает непрерывной RAM, GUI остаётся работоспособным и использует
медленный full-redraw fallback.

## Управление

- ввод с клавиатуры направляется только в приложение с keyboard focus;
- кнопки `-`, `+` и `X` сворачивают, разворачивают и закрывают окно;
- заголовок окна можно перетаскивать;
- рамка и углы меняют размер окна с соблюдением minimum size;
- taskbar содержит отдельную кнопку каждого экземпляра, переключает focus и
  восстанавливает свёрнутое окно;
- Start menu создаёт новый Terminal, Проводник или UI Gallery, а не заменяет содержимое
  уже открытого окна;
- кнопка Start, само меню, его пункты, изображения и часы построены тем же
  `rustos-system-ui`, что интерфейсы приложений: `Button` выдаёт `CommandId`,
  `Image` разрешается через общий icon/resource backend, `Menu` владеет
  focus scope, а дата и время являются обычными `Text`-компонентами;
- правая кнопка на свободной области desktop открывает принадлежащий shell
  context `Menu`: `ARRANGE ICONS` возвращает ярлыки в системную сетку, а
  `PROPERTIES` запускает отдельное окно Desktop Settings;
- один клик выбирает desktop icon, двойной — создаёт новый terminal; интервал и
  допустимое смещение берутся из общего mouse profile.

Оконный registry хранит стабильные `WindowId`, отдельный Z-order и неизменный
порядок taskbar. Каждое приложение размещается в собственных физических кадрах.
Нажатие `X` проходит через `CLOSE_REQUESTED`/`CLOSED`, уничтожает объект клиента
и возвращает кадры общему allocator'у. Поэтому новый Terminal всегда получает
чистый экран, `/` как cwd и новый ID; файлы при этом сохраняются, потому что VFS
является общим сервисом, а не частью процесса shell. Текущий защитный лимит —
16 одновременно существующих окон.

Команда `DISPLAY` показывает display driver, реальное разрешение,
физический размер монитора и цветовой профиль. `DISPLAY MODES` выводит EDID
и дополнительные wide modes. `DISPLAY COLOR TRUECOLOR|RGB565|GRAY8` меняет
цветность renderer'а без невыравненных framebuffer writes.

`DISPLAY MODE WxH` проходит через общий mode-set API. `virtio-gpu` применяет
режим сразу, после чего GUI перевыделяет слои и пересчитывает geometry.
Bootstrap `grub-fb` не умеет mode-set после hand-off и предлагает выбрать
режим в GRUB и перезапуститься.

Команда `FONT` показывает текущую типографику terminal. `FONT FAMILY
CONSOLE|SANS`, `FONT SIZE 10..48` и `FONT STYLE
REGULAR|BOLD|ITALIC|BOLDITALIC` применяются сразу и не требуют перезапуска GUI.

Команды `MOUSE`, `CURSOR`, `ICONS` и `WALLPAPER` меняют input profile,
курсорную/иконную тему и природный фон без перезапуска. Оконный manager
автоматически показывает I-beam над terminal, hand над действиями,
grab/grabbing над заголовком и правильную стрелку на каждой стороне/углу.

Taskbar показывает `HH:MM` и `DD.MM.YYYY`. На x86-64 wall clock читается из
CMOS RTC двумя совпадающими snapshot, поэтому секундный rollover не может
смешать старую дату с новым временем. Опрос ограничен одним разом в секунду,
но repaint выполняется только при смене минуты и затрагивает taskbar (и
открытое Start menu), а не все окна. Таймауты и scheduler по-прежнему используют
только monotonic clock. На ARM без описанного в ACPI/Device Tree RTC выводится
честный uptime fallback. Текущее RTC-время считается UTC; выбор часового пояса
будет policy отдельного time service, а не скрытой поправкой ядра.

## Свойства рабочего стола

Desktop Settings является отдельным application object с собственным окном,
focus, taskbar entry и lifecycle. Клиент не обращается к framebuffer: кнопки
возвращают типизированные команды desktop/display service. Сейчас доступны:

- `1280×720`, `1280×800` и `1600×900` через общий runtime mode-set;
- True Color 24, RGB565 16 и Grayscale 8 как software color profile;
- три встроенных набора обоев: весна, осень и зима;
- общесистемный UI font scale 100%, 125% и 150%.

Активные варианты имеют состояние `SELECTED` и разрешаются общей темой, а не
рисуются вручную приложением. Изменение применяется сразу и синхронизируется
со всеми открытыми окнами Settings. Масштаб влияет на shell, title/taskbar,
desktop labels и component UI; собственная сетка terminal остаётся отдельной
настройкой, чтобы неожиданно не менять число строк и колонок консоли.

При закрытии Settings уничтожается только объект приложения и освобождаются
его кадры. Выбранные обои, цветность и масштаб принадлежат desktop service,
поэтому ожидаемо сохраняются до завершения текущего сеанса.

Команда `RUN /apps/examples/hello.rune student` запускает настоящий
изолированный процесс с VaraniaFS. Persistent `vfsd` обслуживает executable и
DLL, stdout/stderr идут через capability pipe обратно в окно. Заполнение pipe
не замораживает GUI: process manager возвращает управление, console bridge
дренирует буфер, будит writer и продолжает выполнение.

## Компоненты

Retained SDK-слой содержит `Panel`, `Label/Text`, `Button`, `Menu`, `Image`,
layout, focus, commands и renderer-neutral display list. Start уже переведён
на этот слой и служит системным vertical slice: shell и приложение используют
одинаковые controls, но разные bounded component trees. Старые bootstrap
`Widget`-примитивы пока оставлены только в frame/title-bar пути. После IPC
milestone renderer/theme останутся в GUI server, а приложения получат тонкий
versioned client API без дублирования большого renderer-кода.

## Ограничения текущего этапа

- software compositor остаётся синхронным и пока не привязан к VSync;
- xHCI bootstrap transport пока работает bounded polling-режимом;
- UTF-8 Latin/Cyrillic работает, но пока нет shaping, bidi и сложных scripts;
- terminal, Проводник и UI Gallery уже имеют независимые экземпляры, geometry, focus,
  Z-order и lifecycle, но их event handlers пока исполняются последовательно
  внутри bootstrap window-server loop;
- parser shell и console bridge ещё находятся в kernel; запускаемые программы
  и `vfsd` уже исполняются в ring 3;
- display/input services ещё не изолированы в ring 3.
