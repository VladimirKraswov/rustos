# GUI RustOS

## Слои

- `graphics.rs` — clipping, RGB/BGR packing, RAM back buffer и GOP present;
- `font.rs` — независимый от файловой системы аварийный bitmap font;
- `input.rs` — PS/2 keyboard/mouse и нормализованные события;
- `gui/components.rs` — theme и базовые widgets;
- `gui/session.rs` — compositor, desktop, taskbar и window manager;
- `apps/terminal.rs` — первый графический клиент.

Разрешение берётся из UEFI GOP. В тестовой q35 VM это 1280×800×32. Rendering
полностью выполняется CPU, закрытые GPU-драйверы не требуются.

Компоненты никогда не рисуют непосредственно в видимый GOP framebuffer.
Compositor сначала формирует целый кадр в back buffer из обычной RAM и только
потом публикует его быстрым построчным копированием. Поэтому при перемещении
окна не видны промежуточные стадии «стереть обои — нарисовать окно». Обычное
движение курсора публикует только две маленькие dirty-области, а ввод символа
в terminal — только текущую текстовую строку. Это не даёт software redraw
блокировать polling PS/2 при быстром наборе.

Размер back buffer вычисляется из выбранного GOP mode и не ограничен
константой разрешения: 1280×800 занимает около 4 MiB, 4K — около 32 MiB.

## Управление

- ввод с клавиатуры направляется в активный terminal;
- кнопки `-`, `+` и `X` сворачивают, разворачивают и закрывают окно;
- заголовок окна можно перетаскивать;
- taskbar восстанавливает свёрнутый terminal;
- Start menu содержит terminal и shutdown;
- desktop icon повторно открывает закрытый terminal.

## Компоненты

SDK-слой содержит `Widget`, `Panel`, `Label`, `Button`, `IconButton`,
`Checkbox`, `RadioButton`, `Toggle`, `TextEdit`, `ScrollView`, `ListView`,
`Tabs` и `Image`. Сейчас они компилируются вместе с bootstrap GUI. После IPC
milestone renderer/theme останутся в GUI server, а приложения получат тонкий
versioned client API без дублирования большого renderer-кода.

## Ограничения текущего этапа

- software compositor перерисовывает сцену синхронно;
- PS/2 работает polling-режимом;
- нет Unicode font shaping;
- только одно terminal window;
- display/input services ещё не изолированы в ring 3.
