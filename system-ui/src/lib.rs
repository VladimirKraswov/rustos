//! Нативное UI-ядро RustOS.
//!
//! Crate не знает о framebuffer, оконном сервере и конкретном процессе. Он
//! превращает типизированное дерево компонентов в layout и display list,
//! обрабатывает ввод/фокус и сообщает точные повреждённые области. Поэтому
//! один UI работает с CPU rasterizer, будущим GPU, headless-тестами и
//! удалённым выводом.
//!
//! Runtime намеренно `no_std` и не выделяет память: лимиты дерева, display
//! list и damage задаёт владелец через const generics. В user space поверх
//! этого ядра появится allocator-backed facade, но системные сервисы могут
//! оставаться полностью bounded.

#![no_std]
#![warn(missing_docs)]

mod collections;
mod controls;
mod display_list;
mod event;
mod ir;
mod layout;
mod runtime;
mod semantics;
mod style;
mod tree;

pub use collections::{VirtualList, VisibleRange};
pub use controls::UiBuilder;
pub use display_list::{DisplayCommand, DisplayList, FontSpec, RenderBackend, VisualPrimitive};
pub use event::{
    route, DispatchResult, EventPhase, EventRoute, InputEvent, Key, KeyEvent, PointerEvent,
    PointerKind, RoutedEvent,
};
pub use ir::{load_ir, validate_ir, IrError, UiIrHeader, UiIrNode, UI_IR_MAGIC};
pub use layout::{Align, Edges, LayoutSpec, Length};
pub use runtime::{FrameResult, PerformanceCounters, Runtime, RuntimeError};
pub use rustos_video::{Color, Rect};
pub use semantics::{SemanticAction, SemanticNode, SemanticRole, SemanticsTree};
pub use style::{ComputedStyle, Palette, Theme, ThemeKind};
pub use tree::{
    CommandId, ComponentKind, Content, DirtyFlags, Node, NodeId, NodeSpec, NodeState, ResourceId,
    Tree, TreeError,
};

/// Наиболее употребимые типы для приложения.
pub mod prelude {
    pub use crate::{
        Align, CommandId, ComponentKind, Content, Edges, InputEvent, Key, LayoutSpec, Length,
        NodeId, NodeSpec, NodeState, ResourceId, Runtime, SemanticRole, Theme, UiBuilder,
    };
}

#[cfg(test)]
extern crate std;
