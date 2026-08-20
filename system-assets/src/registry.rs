//! Bounded-реестр подключаемых пакетов.

/// Устойчивый идентификатор пакета. Встроенные пакеты используют небольшие
/// значения; внешний packer вычисляет ID из namespace автора и имени.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PackId(pub u32);

/// Читаемые метаданные пакета ресурсов.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PackMetadata {
    /// Уникальный ID.
    pub id: PackId,
    /// Короткое имя для UI и командной строки.
    pub name: &'static str,
    /// Версия содержимого пакета.
    pub version: u16,
}

/// Общая часть cursor/icon pack.
pub trait ResourcePack {
    /// Возвращает метаданные пакета.
    fn metadata(&self) -> PackMetadata;
}

/// Ошибка операции над реестром.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegistryError {
    /// Все bounded-слоты заняты.
    Full,
    /// Пакет с таким ID уже установлен.
    AlreadyInstalled,
    /// Пакет не найден.
    NotFound,
}

/// Реестр без allocator: установка и удаление не могут повредить heap и имеют
/// предсказуемое время. В будущем `resourced` будет держать такие же записи на
/// memory-mapped, проверенные pack-файлы.
pub struct PackRegistry<T: ResourcePack + Copy, const N: usize> {
    entries: [Option<T>; N],
    active: Option<usize>,
}

impl<T: ResourcePack + Copy, const N: usize> PackRegistry<T, N> {
    /// Создаёт пустой реестр.
    pub const fn new() -> Self {
        Self {
            entries: [None; N],
            active: None,
        }
    }

    /// Устанавливает пакет. Первый пакет автоматически становится активным.
    pub fn install(&mut self, pack: T) -> Result<(), RegistryError> {
        if self.find(pack.metadata().id).is_some() {
            return Err(RegistryError::AlreadyInstalled);
        }
        let Some(slot) = self.entries.iter().position(Option::is_none) else {
            return Err(RegistryError::Full);
        };
        self.entries[slot] = Some(pack);
        if self.active.is_none() {
            self.active = Some(slot);
        }
        Ok(())
    }

    /// Удаляет пакет. Если он был активным, выбирается первый оставшийся.
    pub fn remove(&mut self, id: PackId) -> Result<T, RegistryError> {
        let Some(slot) = self.find(id) else {
            return Err(RegistryError::NotFound);
        };
        let pack = self.entries[slot].take().expect("найденный слот заполнен");
        if self.active == Some(slot) {
            self.active = self.entries.iter().position(Option::is_some);
        }
        Ok(pack)
    }

    /// Делает пакет активным.
    pub fn select(&mut self, id: PackId) -> Result<(), RegistryError> {
        self.active = Some(self.find(id).ok_or(RegistryError::NotFound)?);
        Ok(())
    }

    /// Активный пакет или `None`, если реестр пуст.
    pub fn active(&self) -> Option<T> {
        self.active.and_then(|slot| self.entries[slot])
    }

    /// Количество установленных пакетов.
    pub fn len(&self) -> usize {
        self.entries.iter().filter(|entry| entry.is_some()).count()
    }

    /// Проверяет, пуст ли реестр.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn find(&self, id: PackId) -> Option<usize> {
        self.entries
            .iter()
            .position(|entry| entry.is_some_and(|pack| pack.metadata().id == id))
    }
}

impl<T: ResourcePack + Copy, const N: usize> Default for PackRegistry<T, N> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Copy)]
    struct Fake(PackId);

    impl ResourcePack for Fake {
        fn metadata(&self) -> PackMetadata {
            PackMetadata {
                id: self.0,
                name: "fake",
                version: 1,
            }
        }
    }

    #[test]
    fn active_pack_survives_add_remove_and_switch() {
        let mut registry = PackRegistry::<Fake, 2>::new();
        registry.install(Fake(PackId(1))).unwrap();
        registry.install(Fake(PackId(2))).unwrap();
        assert_eq!(registry.active().unwrap().0, PackId(1));
        registry.select(PackId(2)).unwrap();
        registry.remove(PackId(2)).unwrap();
        assert_eq!(registry.active().unwrap().0, PackId(1));
        assert_eq!(registry.len(), 1);
    }
}
