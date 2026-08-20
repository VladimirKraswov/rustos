# GUI RustOS

Архитектура декларативной системной библиотеки, stable ABI, `.rui`,
layout/event/display-list pipeline и UI Gallery описаны в
[`SYSTEM_UI.md`](SYSTEM_UI.md). Этот файл документирует bootstrap desktop и
его прямую интеграцию с текущим compositor.

## Слои

- `graphics.rs` — clipping, RGB/BGR packing, RAM back buffer и scanout present;
- `rustos-video` — безопасные CPU surfaces, blit/alpha, damage и layers;
- `font.rs` — системные Console/Sans bitmap fonts, UTF-8 и типографический API;
- `input.rs` — PS/2 keyboard/mouse и нормализованные события;
- `rustos-system-assets` — cursor/icon packs и CPU-friendly обои;
- `gui/components.rs` — theme и базовые widgets;
- `gui/session.rs` — compositor, desktop, taskbar, registry окон и lifecycle
  независимых экземпляров приложений;
- `apps/terminal.rs` — первый графический клиент.

На QEMU ядро после GRUB hand-off подключает modern PCI virtio-gpu,
читает EDID и переводит scanout на выбранный wide mode. GRUB framebuffer
остаётся fallback для машин без поддерживаемого display device. Rendering
полностью выполняется CPU, закрытые GPU-драйверы не требуются.

На HiDPI/Retina monitor native EDID mode остаётся в `DISPLAY MODES`, но
стартовый logical scanout ограничен комфортным 1600×900. Поэтому интерфейс не
становится микроскопическим до появления полноценного fractional DPI scaling.
Для monitor меньше лимита сохраняется его preferred mode; после загрузки любой
поддержанный режим по-прежнему выбирается командой `DISPLAY MODE WxH`.

Подробный контракт scanout/surfaces/compositor и путь к software OpenGL описан
в [VIDEO.md](VIDEO.md). Команды, события, style-флаги и state machine окон
описаны отдельно в [WINDOWS.md](WINDOWS.md), а семейства, размеры и
начертания — в [FONTS.md](FONTS.md). Курсоры, настройки мыши, icon packs и
обои документированы в [SYSTEM_ASSETS.md](SYSTEM_ASSETS.md).

Компоненты никогда не рисуют непосредственно в видимый framebuffer.
Compositor сначала формирует кадр в back buffer из обычной RAM и только потом
публикует его одним линейным или быстрым построчным копированием. Перед drag
второй RAM-буфер перестраивается из desktop и всех окон, кроме
перетаскиваемого. Поэтому preview из title bar и контура корректно проходит над
другими программами, не стирает их и отправляет в scanout только несколько
узких damage-полос. Полное окно рисуется один раз при mouse-up. Это осознанный
software-rendering режим для QEMU TCG, где копирование мегабайтов scanout на
каждый пакет мыши слишком дорого.

PS/2 service объединяет накопившиеся движения с неизменным состоянием кнопок.
Переходы mouse-down/up сохраняются отдельно, но вместо очереди устаревших
промежуточных кадров compositor сразу рисует последнюю позицию. Обычное
движение курсора публикует две маленькие dirty-области, а ввод символа — только
текущую строку terminal.

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
- Start menu создаёт новый Terminal или UI Gallery, а не заменяет содержимое
  уже открытого окна;
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

Команда `RUN /apps/examples/hello.rune student` запускает настоящий
изолированный процесс с VaraniaFS. Persistent `vfsd` обслуживает executable и
DLL, stdout/stderr идут через capability pipe обратно в окно. Заполнение pipe
не замораживает GUI: process manager возвращает управление, console bridge
дренирует буфер, будит writer и продолжает выполнение.

## Компоненты

SDK-слой содержит `Widget`, `Panel`, `Label`, `Button`, `IconButton`,
`Tabs` и `Image`. Сейчас они компилируются вместе с bootstrap GUI. После IPC
milestone renderer/theme останутся в GUI server, а приложения получат тонкий
versioned client API без дублирования большого renderer-кода.

## Ограничения текущего этапа

- software compositor остаётся синхронным и пока не привязан к VSync;
- PS/2 работает polling-режимом;
- UTF-8 Latin/Cyrillic работает, но пока нет shaping, bidi и сложных scripts;
- terminal и UI Gallery уже имеют независимые экземпляры, geometry, focus,
  Z-order и lifecycle, но их event handlers пока исполняются последовательно
  внутри bootstrap window-server loop;
- parser shell и console bridge ещё находятся в kernel; запускаемые программы
  и `vfsd` уже исполняются в ring 3;
- display/input services ещё не изолированы в ring 3.
