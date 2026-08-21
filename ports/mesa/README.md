# Upstream Mesa source

`make bootstrap-mesa` загружает Mesa 26.2.0 из официального архива и проверяет
SHA-256 из release notes. Исходник распаковывается только в
`build/third-party/mesa-26.2.0` и не раздувает Git-репозиторий.

Рабочий системный срез находится в `libs/mesa`: это `no_std` platform/winsys
boundary, уже используемый ring-3 `renderd`. Полная C Mesa ещё не собирается в
образ: актуальные блокеры перечислены в `docs/MESA.md`, а не замаскированы
host-библиотеками.
