# RustOS SDK

SDK отделяет обычное приложение от syscall ABI и формата диска. Программа
пишет обычный `fn main`, использует upstream `std` и при необходимости safe
wrappers системных RUNE DLL. `_start`, ProcessStartInfo и capability slots
принадлежат `rustos-crt`/PAL, а не коду приложения.

Минимальный пример находится в [`examples/hello`](examples/hello). Текущий
S1 bootstrap собирает его с macOS/Linux:

```bash
bash scripts/build-std.sh build -p rustos-sdk-hello \
  --target targets/x86_64-unknown-rustos.json
```

Build pipeline преобразует ELF linker intermediate в `hello.rune`, кладёт
его в `/apps/examples/hello.rune` на VaraniaFS и QEMU-тест запускает именно с
диска. В RustOS тот же файл запускается как обычная программа; знать имя
`rune-runner` приложению и shell не требуется.

Публичная DLL описывается одним `RUNE-ABI 1` manifest в [`abi`](abi). На
границе разрешены `extern "C"`, `#[repr(C)]`, integer фиксированной ширины и
явные handles/buffers. Rust ABI наружу не экспортируется.

До появления native seed `rustc` этот пример является cross-hosted SDK, а не
доказательством self-hosting. Точный статус и критерий перехода описаны в
[`docs/SELF_HOSTING.md`](../docs/SELF_HOSTING.md).
