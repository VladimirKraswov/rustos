# GUI RustOS

## Слои

- `graphics.rs` — clipping, RGB/BGR packing, RAM back buffer и GOP present;
- `rustos-video` — безопасные CPU surfaces, blit/alpha, damage и layers;
- `font.rs` — независимый от файловой системы аварийный bitmap font;
- `input.rs` — PS/2 keyboard/mouse и нормализованные события;
- `gui/components.rs` — theme и базовые widgets;
- `gui/session.rs` — compositor, desktop, taskbar и window manager;
- `apps/terminal.rs` — первый графический клиент.

Разрешение берётся из UEFI GOP. В тестовой q35 VM это 1280×800×32. Rendering
полностью выполняется CPU, закрытые GPU-драйверы не требуются.
Подробный контракт scanout/surfaces/compositor и путь к software OpenGL описан
в [VIDEO.md](VIDEO.md).

Компоненты никогда не рисуют непосредственно в видимый GOP framebuffer.
Compositor сначала формирует кадр в back buffer из обычной RAM и только потом
публикует его быстрым построчным копированием. Статический слой desktop
(обои, иконки, taskbar) хранится во втором RAM-буфере. Во время drag compositor
показывает лёгкий preview из title bar и контура: старый preview восстанавливает
из кэша, а в GOP отправляет только несколько узких damage-полос. Полное окно
рисуется один раз при mouse-up. Это осознанный software-rendering режим для
QEMU TCG, где копирование мегабайтов GOP на каждый пакет мыши слишком дорого.

PS/2 service объединяет накопившиеся движения с неизменным состоянием кнопок.
Переходы mouse-down/up сохраняются отдельно, но вместо очереди устаревших
промежуточных кадров compositor сразу рисует последнюю позицию. Обычное
движение курсора публикует две маленькие dirty-области, а ввод символа — только
текущую строку terminal.

Размер буферов вычисляется из выбранного GOP mode и не ограничен константой:
1280×800 занимает около 4 MiB на слой, 4K — около 32 MiB. Если для второго
слоя не хватает непрерывной RAM, GUI остаётся работоспособным и использует
медленный full-redraw fallback.

## Управление

- ввод с клавиатуры направляется в активный terminal;
- кнопки `-`, `+` и `X` сворачивают, разворачивают и закрывают окно;
- заголовок окна можно перетаскивать;
- taskbar восстанавливает свёрнутый terminal;
- Start menu содержит terminal и shutdown;
- desktop icon повторно открывает закрытый terminal.

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
