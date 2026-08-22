# USB в RustOS

RustOS использует xHCI как основной USB host transport на AMD64 и эталонной
AArch64-платформе QEMU `virt`. Один и тот же драйвер обслуживает USB 1.x, 2.0
и 3.x устройства через root ports; PS/2 и virtio-input остаются только
независимыми fallback backend'ами для машин без доступного xHCI.

## Слои

```text
USB descriptors / HID reports       crate rustos-usb, no_std, без MMIO
                 │
                 ▼
xHCI transport                     command/event/transfer rings, DMA
                 │
                 ▼
input multiplexer                  USB отдельно захватывает keyboard/mouse
                 │
                 ▼
нормализованный input ABI          Key, MouseEvent, MouseSettings
                 │
                 ▼
window server / приложения         не знают вид контроллера и USB speed
```

`rustos-usb` намеренно не зависит от ядра. Он строго проверяет длины device,
configuration, interface и endpoint descriptors, выбирает HID Boot keyboard
или mouse с interrupt-IN endpoint и декодирует отчёты. Ошибочный descriptor,
HID rollover или укороченный report отвергается, а не превращается в выход за
границы буфера. Host-тесты этого crate выполняются обычным `cargo test`.

`kernel/src/input/xhci.rs` — bounded bootstrap transport:

- ищет PCI class `0c:03:30`, включает MMIO и bus mastering;
- передаёт владение от firmware, останавливает и сбрасывает контроллер;
- создаёт DCBAA, scratchpad array, command ring, event ring, ERST и отдельные
  transfer rings;
- сбрасывает root port, выполняет `Enable Slot`, `Address Device`, чтение
  дескрипторов, `Configure Endpoint`, `SET_CONFIGURATION`, `SET_PROTOCOL` и
  `SET_IDLE`;
- поддерживает до 32 root ports и 8 одновременно подключённых HID-устройств;
- повторно перечисляет устройство после Port Status Change и освобождает DMA
  frames после отключения;
- ограничивает все ожидания и очереди, поэтому неисправное USB-устройство не
  может навсегда остановить загрузку или безгранично расходовать память.

Клавиатура выдаёт только новые нажатия, отдельно ведёт Shift/Caps Lock и не
повторяет уже удерживаемую клавишу как новое событие. Мышь принимает boot
reports длиной 3 или 4 байта: относительные X/Y, три кнопки и колесо.
Чувствительность и ускорение применяются fixed-point арифметикой с остатком,
поэтому медленное движение не теряется.

## Переносимость и отображение MMIO

На AMD64 PCI config читается через legacy configuration I/O, BAR xHCI
отображается как uncached device memory. На AArch64 QEMU `virt` используется
low ECAM `0x3f00_0000`; PCI apertures и BAR отображаются как Device-nGnRE.
Загрузчик получает поддерживаемую физическую ширину из
`ID_AA64MMFR0_EL1.PARange`, поэтому высокие 64-битные BAR не требуют
неподдерживаемого конкретным CPU значения `TCR_EL1.IPS`.

Input multiplexer выбирает backend независимо для клавиатуры и мыши. Например,
USB-клавиатура может работать вместе с PS/2-мышью. При hot-unplug USB перестаёт
затенять fallback, и GUI продолжает принимать события без перезагрузки.

## Сборка и проверка

Интерактивные QEMU/UTM-профили подключают `qemu-xhci`, `usb-kbd` и
`usb-mouse`. Проверить parser отдельно и полный аппаратный маршрут можно так:

```sh
cargo test -p rustos-usb
make test-gui
make test-arm-gui
```

Оба GUI-теста требуют serial-маркеры `[usb] hid attached`, отправляют реальные
keyboard/mouse events через QEMU и проверяют результат в terminal/SystemUI.
AArch64-тест намеренно не подключает параллельные virtio-input устройства,
чтобы событие не могло случайно пройти через fallback вместо USB.
UTM-профиль следует тому же правилу: одновременные `usb-mouse` и
`virtio-mouse` создают два host input handler'а, тогда как guest multiplexor
подавляет fallback при найденном USB HID. Для одного типа ввода в VM должен
существовать ровно один основной виртуальный контроллер.

## Честная граница текущего этапа

Сейчас реализованы xHCI root ports и HID Boot keyboard/mouse. Ещё не заявлены
готовыми USB hubs, полноценный HID report-descriptor interpreter, mass storage,
audio/video, isochronous endpoints, suspend/resume и USB-C policy. Транспорт
пока опрашивает bounded event ring из bootstrap kernel.

Следующая архитектурная граница — IRQ/MSI-X и IOMMU capabilities, после чего
xHCI transport переносится в изолированный ring-3 `usbd`. Сбой parser'а или
конкретного class driver тогда завершит только сервис; kernel сохранит DMA
объекты, capability isolation и возможность supervisor restart.

Спецификации, на которых основана реализация:

- [Intel xHCI Requirements Specification 1.2c](https://www.intel.com/content/www/us/en/content-details/868295/extensible-host-controller-interface-for-universal-serial-bus-xhci-requirements-specification-r1-2c.html);
- [USB-IF HID specifications](https://www.usb.org/hid);
- [QEMU USB emulation](https://www.qemu.org/docs/master/system/devices/usb).
