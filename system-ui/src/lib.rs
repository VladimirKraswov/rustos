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

mod clipboard;
mod collections;
mod controls;
mod display_list;
mod event;
mod file_browser;
mod ir;
mod layout;
mod metrics;
mod runtime;
mod scroll;
mod selection;
mod semantics;
mod style;
mod text_engine;
mod text_input;
mod tree;

pub use clipboard::{Clipboard, ClipboardError, ClipboardFormat, LocalClipboard};
pub use collections::{ListViewState, VirtualList, VisibleRange, LIST_SELECTION_RANGES};
pub use controls::UiBuilder;
pub use display_list::{
    DisplayCommand, DisplayList, FontSpec, RenderBackend, TextAlign, VisualPrimitive,
};
pub use event::{
    modifiers, route, DispatchResult, EventPhase, EventRoute, InputEvent, Key, KeyEvent,
    PointerEvent, PointerKind, RoutedEvent,
};
pub use file_browser::{
    build_file_browser, FileBrowserItem, FileBrowserNodes, FileBrowserSpec, FileBrowserTreeItem,
    FileBrowserView,
};
pub use ir::{load_ir, validate_ir, IrError, UiIrHeader, UiIrNode, UI_IR_MAGIC};
pub use layout::{Align, Edges, LayoutSpec, Length};
pub use metrics::{WindowMetrics, SCALE_MILLI_ONE};
pub use runtime::{FrameResult, PerformanceCounters, Runtime, RuntimeError};
pub use rustos_video::{Color, Rect};
pub use scroll::{
    OverscrollPolicy, ScrollAxis, ScrollBarLayout, ScrollBarPolicy, ScrollBehavior, ScrollConfig,
    ScrollController, ScrollDelta, ScrollModel, ScrollState, ScrollUnit, ScrollbarGeometry,
    DEFAULT_SCROLLBAR_INSET, DEFAULT_SCROLLBAR_THICKNESS,
};
pub use selection::{SelectionError, SelectionMode, SelectionModel, SelectionRange};
pub use semantics::{SemanticAction, SemanticNode, SemanticRole, SemanticsTree};
pub use style::{style_class, ComputedStyle, Palette, Theme, ThemeKind};
pub use text_engine::{
    CompositionEvent, TextCommand, TextCommandError, TextDocument, TextEditorController, TextError,
    TextLocation, TextRange, TextSelection,
};
pub use text_input::{TextInputBuffer, TextInputError};
pub use tree::{
    CommandId, ComponentKind, Content, DirtyFlags, Node, NodeId, NodeSpec, NodeState, ResourceId,
    Tree, TreeError,
};

/// Наиболее употребимые типы для приложения.
pub mod prelude {
    pub use crate::{
        style_class, Align, Clipboard, ClipboardFormat, CommandId, ComponentKind, Content, Edges,
        InputEvent, Key, LayoutSpec, Length, ListViewState, LocalClipboard, NodeId, NodeSpec,
        NodeState, ResourceId, Runtime, ScrollConfig, ScrollModel, SelectionMode, SelectionModel,
        SemanticRole, TextAlign, TextCommand, TextDocument, TextEditorController, TextRange,
        TextSelection, Theme, UiBuilder, WindowMetrics,
    };
}

#[cfg(test)]
extern crate std;
