//! Capability handles и права доступа.
//!
//! Handle — не адрес и не глобальный номер объекта. Это индекс в таблице
//! конкретного процесса. Передача handle другому процессу возможна только
//! через IPC и всегда создаёт новую запись с явно урезанным набором прав.

/// Непрозрачный capability handle в таблице текущего процесса.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Handle(pub u32);

impl Handle {
    /// Невалидный handle, используемый как отсутствие результата.
    pub const INVALID: Self = Self(0);

    /// Проверяет, что handle не равен [`Self::INVALID`].
    pub const fn is_valid(self) -> bool {
        self.0 != 0
    }
}

/// Маска прав capability.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Rights(pub u64);

impl Rights {
    /// Объект недоступен ни для одной операции.
    pub const NONE: Self = Self(0);
    /// Чтение данных или метаданных.
    pub const READ: Self = Self(1 << 0);
    /// Изменение данных или метаданных.
    pub const WRITE: Self = Self(1 << 1);
    /// Выполнение кода из объекта.
    pub const EXECUTE: Self = Self(1 << 2);
    /// Отображение объекта в виртуальную память.
    pub const MAP: Self = Self(1 << 3);
    /// Ожидание события объекта.
    pub const WAIT: Self = Self(1 << 4);
    /// Отправка сообщений endpoint'у.
    pub const SEND: Self = Self(1 << 5);
    /// Получение сообщений из endpoint'а.
    pub const RECEIVE: Self = Self(1 << 6);
    /// Создание дочерних объектов внутри объекта-контейнера.
    pub const CREATE: Self = Self(1 << 7);
    /// Удаление или завершение объекта.
    pub const DESTROY: Self = Self(1 << 8);
    /// Передача производного capability через IPC.
    pub const TRANSFER: Self = Self(1 << 9);
    /// Изменение набора прав или политики объекта.
    pub const ADMIN: Self = Self(1 << 10);

    /// Возвращает объединение двух наборов прав.
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Проверяет наличие всех прав из `required`.
    pub const fn contains(self, required: Self) -> bool {
        self.0 & required.0 == required.0
    }

    /// Создаёт производный набор, не позволяя добавить отсутствующие права.
    pub const fn attenuate(self, requested: Self) -> Self {
        Self(self.0 & requested.0)
    }
}

const _: () = assert!(core::mem::size_of::<Handle>() == 4);
const _: () = assert!(core::mem::size_of::<Rights>() == 8);
