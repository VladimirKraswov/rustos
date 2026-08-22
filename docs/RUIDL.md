# RUIDL и общий SDK cache

RUIDL — единственный исходник публичного ABI RUNE-библиотеки. Упаковщик не
складывает рядом с DLL копию Rust-«заголовков»: проверенный UTF-8 контракт
встраивается в record `INTERFACE_SCHEMA`, а SDK compiler извлекает его из
установленной DLL и детерминированно генерирует Rust crates.

```text
library.rune
  └─ INTERFACE_SCHEMA (RUIDL)
          │ structural/hash validation
          ▼
      rustos-ruidl
          │ SHA-256(schema + target ABI + generator version)
          ▼
build/sdk-cache/<hash>/
  ├─ Cargo.lock       детерминированный lock двух generated crates
  ├─ raw/             полный no_std extern "C" crate
  ├─ src/lib.rs       safe facade
  ├─ schema.ruidl     точный исходник для аудита
  ├─ RUIDL.md         отчёт safety generation
  └─ ruidl.lock       cache identity
```

## Команды

После обычной сборки cache уже содержит bindings системных DLL для выбранной
архитектуры. Повторное разрешение не переписывает готовый объект:

```bash
make build
make sdk-resolve

cargo run -p rustos-ruidl-compiler --bin rustos-ruidl -- resolve \
  build/rune-system/lib/vfs-1.rune \
  build/sdk-cache \
  x86_64-unknown-rustos
```

Команда печатает путь готового Cargo package. Cache key включает точные bytes
schema, target ABI и версию generator. Запись сначала выполняется в sibling
staging directory и публикуется одним `rename`; процесс никогда не видит
половину crate. Конкурирующий compiler либо атомарно публикует объект первым,
либо проверяет уже готовый `ruidl.lock`.

`ruidl.lock` хранит SHA-256 каждого исходного файла, включая `Cargo.lock`.
Неожиданный `build.rs`, `.cargo/config.toml`, symlink или иной посторонний
source/config entry делает cache-объект недействительным; локальный `target/`
игнорируется только как производный build output.

Для разработки разрешён прямой вход `*.rune-abi`, но установленный SDK всегда
берёт schema из RUNE. Так тестируется ровно тот контракт, который несёт DLL.

## Raw и safe crates

Raw crate содержит все C ABI declarations, `#[link_name]`, точные integer
types и непрозрачные `#[repr(C)]` types. Он явно называется `-sys` и является
unsafe boundary.

Safe facade публикует pointer-функцию только когда схема полностью задаёт
layout, borrow/out/slice ownership, linear handles, error set и bounds
возвращаемых размеров. Из них generator строит `&str`, `&[u8]`,
`&mut [u8]`, `Result`, opaque state и handle без `Copy`/`Clone`; `MaybeUninit`
остаётся только внутри сгенерированного кода. Неполный контракт доступен лишь
через явно названный `unsafe_api`: generator никогда не угадывает lifetime.
Системные высокоуровневые facades, например VFS `File`/`Read`/`Write`, могут
поставляться поверх generated raw crate; ручные raw declarations запрещены.

Минимальная схема:

```text
RUNE-ABI 1
package org.example.math
kind library
interface org.example.math/1
abi 1
export example_add add(i64,i64)->i64 function
```

Generated safe call выглядит как обычный Rust:

```rust,ignore
let value = rustos_math::add(20, 22);
```

Контракт VFS показывает полный синтаксис:

```text
opaque client 32 8
struct dirent 256 8
field dirent object u64 0
handle vfs_object u64 18446744073709551615
error-set vfs_error i32 0 -2147483648
error vfs_error NOT_FOUND -101

export rustos_vfs_open open(*mut_client,*const_u8,usize,u32,*mut_u64)->i32 function
borrow open 0 exclusive client
slice open 1 2 in utf8
out open 4 vfs_object
result open vfs_error
```

Layout проверяется на размер, alignment, выход за границу и overlap. Для
`result` все pointers обязаны иметь ровно один контракт; `out` должен быть
mutable, а slice length — `usize` и принадлежать ровно одному срезу. Shared
borrow принимает только raw const pointer, exclusive — mutable. Линейный
`consume` передаёт владение при входе в provider независимо от результата
вызова.

## Зависимости и package pin

По умолчанию dependency выбирает доверенный provider по interface и ABI:

```text
dependency math-1.rune org.example.math/1 1 2
```

Шестое поле закрепляет конкретный canonical package, не путь:

```text
dependency math-1.rune org.example.math/1 1 2 org.example.math-reference
```

Packer превращает имя в полный 128-битный `PackageId`. Resolver проверяет pin,
interface и ABI до commit; файл с подходящим именем, но другим package ID, не
становится provider.

## Атомарный запуск

User-space loader выполняет явные фазы:

1. `prepare` читает и проверяет весь package closure, package pins, cycles,
   imports, relocations, TLS, RELRO и executable entry без доступа к `Memory`;
2. supervisor сопоставляет capability requests всего closure со своей policy;
3. `commit` создаёт mappings и shared objects; любая ошибка откатывает весь
   набор;
4. target startup block получает только stdio/lifecycle и разрешённые service
   routes, а loader-only capabilities закрываются до entry point.

Required capability без route завершает только создаваемый процесс до запуска
его кода. Optional capability не выдаётся. Права в manifest являются
семантическими правами service; supervisor переводит один grant в минимальный
transport bundle (например VFS request + private reply endpoint), не раскрывая
это приложению.

## Ограничения текущего рубежа

- Формат и generator рассчитаны на 64-bit RustOS targets; cache разделяет
  AMD64 и AArch64 по target ABI.
- Output UTF-8 buffer намеренно не генерируется как `&mut str`: provider может
  нарушить инвариант UTF-8. Для такого API используется byte slice и явный
  validated-result type.
- Production release должен заменить публичный development trust anchor своим
  Ed25519 ключом. Development key подходит только для локальных образов.
- Текущий VFS route выдаётся целиком как read/write service. Раздельное
  ограничение операций одного VFS endpoint требует session policy внутри
  `vfsd`; manifest уже переносит маску, но transport handle сам по себе умеет
  ограничивать только SEND/RECEIVE, а не отдельные RPC opcodes.

GUI/GPU API намеренно не переводятся на эту границу до завершения её
fault-injection и package-signature policy. Публичное приложение не должно
зависеть от того, какой renderer позже выбран системой.
