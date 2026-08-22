# Mesa и системное приложение Aurora 3D

## Что работает сейчас

RustOS содержит первый исполняемый срез порта Mesa:

- `rustos-mesa` — `no_std` Gallium-like state tracker и platform boundary;
- `rustos-virgl` — узкий VirGL winsys transport;
- `renderd.rune` — единственный ring-3 владелец `GpuRender` capability;
- `compositord.rune` — vblank-paced frame policy и передача `GraphicsBuffer`;
- `displayd.rune` — эксклюзивный atomic scanout;
- `gpu-demo.rune` — непривилегированный launcher приложения Aurora 3D.

Ярлык **Aurora 3D** находится на рабочем столе. Двойной щелчок создаёт обычное
независимое окно: оно сворачивается, растягивается и закрывается тем же оконным
сервером, что Терминал и Проводник. Сцена содержит анимированный perspective mesh, градиентный stage,
diffuse/specular lighting и TGSI vertex/fragment shaders. Guest CPU формирует
только вершины и команды: target не имеет `CPU_WRITE`, а rasterization и запись
пикселей выполняются VirGL renderer'ом.

```text
Aurora window ── semantic Canvas3D ──► renderd ──► VirGL/host GPU
      │                                      │ GPU-only BGRA surface
      │                                      v
      └──────── retained window surface ◄─ ordered GPU copy
                                             │
desktop + windows ── z-order/transforms ──► GPU compositor ──► scanout
```

Приложение не получает GPU, scanout или MMIO capability. Ошибка/завершение
launcher'а не уничтожает графические сервисы; supervisor продолжает следить за
ними как за постоянными процессами.

Readback в оконном пути удалён. Анимация меняет только содержимое Canvas;
неизменившиеся chrome/текст/рамка остаются в retained surface, а перетаскивание
меняет лишь transform готового слоя. Если VirGL отсутствует или потерян,
Aurora локально включает software renderer; остальные окна и shell не меняют
режим.

## Воспроизводимая upstream база

Порт закреплён на Mesa 26.2.0:

```sh
make bootstrap-mesa
```

Скрипт загружает официальный tarball и проверяет SHA-256
`efd4bb08…b799a4ef`. Версия и hash также записаны в `ports/mesa/port.toml`.
Это соответствует модели Mesa: Gallium отделяет state tracker от драйвера, а
winsys связывает драйвер с конкретной ОС и её buffer/synchronization API.

Источники: [Mesa 26.2.0 release notes](https://docs.mesa3d.org/relnotes/26.2.0.html),
[Gallium introduction](https://docs.mesa3d.org/gallium/intro.html),
[Mesa source tree and winsys](https://docs.mesa3d.org/sourcetree.html),
[VirGL driver](https://docs.mesa3d.org/drivers/virgl.html).

## Почему это ещё не вся upstream Mesa

Полная Mesa — большой C/C++ проект с Meson, pthreads, TLS, libc/libm, файловой
системой, dynamic loading и генераторами build-time. Подключить host `.dylib`
или Linux `.so` было бы ложным портом: они не исполняются внутри RustOS.

Оставшийся честный маршрут:

1. завершить native C ABI libc/libm поверх RustOS `std` и VFS;
2. реализовать pthread facade поверх process/thread/futex ABI;
3. дать `dlopen` RUNE/DLL adapter и Mesa-compatible TLS destructors;
4. добавить Meson cross-file и собирать `gallium/virgl` + EGL без X11/Wayland;
5. заменить Rust state tracker upstream `libgallium`, сохранив нынешний winsys,
`GraphicsBuffer`, `SyncTimeline` и capability boundary;
6. затем добавить GLSL/NIR, OpenGL ES и ускорение SystemUI.

Название `platform-seed` в manifest специально не выдаёт этот этап за полный
OpenGL 4.6 port. GPU-сцена уже является настоящим end-to-end VirGL приложением,
но texture sampling, normal/parallax maps и публичные EGL/GL/GLES entry points
появятся только с upstream state tracker. Software fallback визуально проверяет
процедурную texture/normal/parallax математику, однако это не считается тестом
Mesa или OpenGL.

## Проверка

На Linux с QEMU, собранным с virglrenderer:

```sh
make test-virgl
```

Тест проверяет два независимых оконных Canvas (включая retained animation
update), 48 последовательных GPU frames,
timeline/vblank, цветной screenshot и отсутствие guest CPU rasterization. Артефакты:
`build/test-results/virgl/showcase.{xwd,ppm}`.

Homebrew QEMU на macOS не предоставляет пригодный `virtio-vga-gl`, поэтому
ускоренный профиль Apple Silicon использует UTM:

```sh
make run-utm-gpu
make test-utm-gpu
```

В этой конфигурации RustOS исполняется нативно через HVF, guest VirGL проходит
через UTM virglrenderer и ANGLE/Metal. Это настоящий ускоренный transport и
рабочий вертикальный 3D-срез, но пока не полный upstream Mesa. Штатный SystemUI
уже растеризуется и композируется VirGL backend'ом; CPU остаётся для layout,
формирования команд и recovery fallback. EGL/GLES/GLSL/NIR cross-port остаётся
следующим самостоятельным этапом.

Render path уже использует настоящий трёхбуферный swapchain: три различных
`GraphicsBuffer`, три VirGL surface и независимые `SyncTimeline` допускают три
GPU submission in-flight. `compositord` применяет mailbox-политику и удаляет
устаревший готовый frame вместо накопления FIFO-задержки; reuse каждого buffer
разрешён только после его fence. `make test-utm-gpu` требует наблюдаемый
`peak-inflight=2` или `3` (в зависимости от скорости host completion), поэтому
это не декларативный флаг: очередь действительно держит несколько команд в
работе, а три swapchain image остаются независимыми.
