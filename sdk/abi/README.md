# RUNE ABI manifest

`*.rune-abi` — единый исходник бинарного контракта программы или DLL. Он
намеренно мал и пригоден для разбора native-инструментом без сторонних crates.

```text
RUNE-ABI 1
package org.example.math
kind library
interface org.example.math/1
abi 1
export add add(i64,i64)->i64 function
```

Значения разделяются пробелами и сами пробелов не содержат. Поддерживаются
`application`, `library`, `service`, `driver` и символы `function`, `data`,
`tls`. Зависимость и импорт задаются явно:

```text
dependency math-1.rune org.example.math/1 1 2
import add org.example.math/1 add(i64,i64)->i64 1 2 function
```

Optional шестое поле dependency закрепляет конкретный canonical package:

```text
dependency math-1.rune org.example.math/1 1 2 org.example.math-reference
```

Из canonical interface/signature упаковщик детерминированно строит 128-битные
ID, проверяет `.dynsym` ELF intermediate и не позволяет получить RUNE с
необъявленным импортом. Это одновременно документация, проверка совместимости
ABI и вход реализованного RUIDL compiler. Команда и cache layout описаны в
[`docs/RUIDL.md`](../../docs/RUIDL.md).

Для application/service тот же UTF-8 manifest может содержать package fields:

```text
runtime-abi 1 1
version 1 0 0
lifecycle multi-instance
name default "Files"
name ru-RU "Проводник"
summary ru-RU "Просмотр файлов"
vendor "RustOS Project"
category system.files
capability required org.rustos.vfs/1 1 0x3 4
icon 64 64 100 svg any application assets/files.svg
resource ui/main application/rui assets/main.rui
```

Последние поля `capability` — policy, service interface, ABI, rights mask и
startup slot hint. Запрос не выдаёт право сам: supervisor может ослабить либо
отклонить его. Пути icons/resources разрешаются относительно manifest,
встраиваются в RUNE и проверяются общим container hash.
