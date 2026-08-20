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

Из canonical interface/signature упаковщик детерминированно строит 128-битные
ID, проверяет `.dynsym` ELF intermediate и не позволяет получить RUNE с
необъявленным импортом. Это одновременно документация, проверка совместимости
ABI и вход будущего генератора безопасных Rust/C wrappers.
