# GUI RustOS

## Слои

- `graphics.rs` — clipping, RGB/BGR packing, RAM back buffer и scanout present;
- `rustos-video` — безопасные CPU surfaces, blit/alpha, damage и layers;
- `font.rs` — независимый от файловой системы аварийный bitmap font;
- `input.rs` — PS/2 keyboard/mouse и нормализованные события;
- `gui/components.rs` — theme и базовые widgets;
- `gui/session.rs` — compositor, desktop, taskbar и window manager;
- `apps/terminal.rs` — первый графический клиент.

На QEMU ядро после GRUB hand-off подключает modern PCI virtio-gpu,
читает EDID и переводит scanout на выбранный wide mode. GRUB framebuffer
остаётся fallback для машин без поддерживаемого display device. Rendering
полностью выполняется CPU, закрытые GPU-драйверы не требуются.
Подробный контракт scanout/surfaces/compositor и путь к software OpenGL описан
в [VIDEO.md](VIDEO.md). Команды, события, style-флаги и state machine окон
описаны отдельно в [WINDOWS.md](WINDOWS.md).

Компоненты никогда не рисуют непосредственно в видимый framebuffer.
Compositor сначала формирует кадр в back buffer из обычной RAM и только потом
публикует его одним линейным или быстрым построчным копированием. Статический слой desktop
(обои, иконки, taskbar) хранится во втором RAM-буфере. Во время drag compositor
показывает лёгкий preview из title bar и контура: старый preview восстанавливает
из кэша, а в scanout отправляет только несколько узких damage-полос. Полное окно
рисуется один раз при mouse-up. Это осознанный software-rendering режим для
QEMU TCG, где копирование мегабайтов scanout на каждый пакет мыши слишком дорого.

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

- ввод с клавиатуры направляется в активный terminal;
- кнопки `-`, `+` и `X` сворачивают, разворачивают и закрывают окно;
- заголовок окна можно перетаскивать;
- рамка и углы меняют размер окна с соблюдением minimum size;
- taskbar восстанавливает свёрнутый terminal;
- Start menu содержит terminal и shutdown;
- desktop icon повторно открывает закрытый terminal.

Команда `DISPLAY` показывает display driver, реальное разрешение,
физический размер монитора и цветовой профиль. `DISPLAY MODES` выводит EDID
и дополнительные wide modes. `DISPLAY COLOR TRUECOLOR|RGB565|GRAY8` меняет
цветность renderer'а без невыравненных framebuffer writes.

`DISPLAY MODE WxH` проходит через общий mode-set API. `virtio-gpu` применяет
режим сразу, после чего GUI перевыделяет слои и пересчитывает geometry.
Bootstrap `grub-fb` не умеет mode-set после hand-off и предлагает выбрать
режим в GRUB и перезапуститься.

Команда `RUN /apps/examples/hello.rune student` запускает настоящий
изолированный процесс с VaraniaFS. Persistent `vfsd` обслуживает executable и
DLL, stdout/stderr идут через capability pipe обратно в окно. Заполнение pipe
не замораживает GUI: process manager возвращает управление, console bridge
дренирует буфер, будит writer и продолжает выполнение.

## Компоненты

SDK-слой содержит `Widget`, `Panel`, `Label`, `Button`, `IconButton`,
`Checkbox`, `RadioButton`, `Toggle`, `TextEdit`, `ScrollView`, `ListView`,
`Tabs` и `Image`. Сейчас они компилируются вместе с bootstrap GUI. После IPC
milestone renderer/theme останутся в GUI server, а приложения получат тонкий
versioned client API без дублирования большого renderer-кода.

## Ограничения текущего этапа

- software compositor остаётся синхронным и пока не привязан к VSync;
- PS/2 работает polling-режимом;
- нет Unicode font shaping;
- только одно terminal window;
- parser shell и console bridge ещё находятся в kernel; сами программы и
  `vfsd` уже исполняются в ring 3;
- display/input services ещё не изолированы в ring 3.
