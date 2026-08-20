//! Проверяемое заранее скомпилированное декларативное представление `.rui`.
//!
//! Host compiler позже добавит удобный source syntax. Runtime читает только
//! компактные fixed-size records и не содержит XML/JS/parser dependencies.

use crate::{
    Align, CommandId, ComponentKind, Content, Edges, LayoutSpec, Length, NodeId, NodeSpec,
    NodeState, ResourceId, SemanticRole, Tree, TreeError,
};

/// Magic скомпилированного UI IR.
pub const UI_IR_MAGIC: [u8; 8] = *b"RUI\0\r\n\x1a\n";
const HEADER_SIZE: usize = 32;
const NODE_SIZE: usize = 64;

/// Заголовок wire format. Parser всё равно читает поля little-endian вручную,
/// поэтому unaligned shared-memory input никогда не разыменовывается как Rust.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UiIrHeader {
    /// [`UI_IR_MAGIC`].
    pub magic: [u8; 8],
    /// [`rustos_abi::ui::UI_IR_VERSION`].
    pub version: u16,
    /// 32 в v1.
    pub header_size: u16,
    /// 64 в v1.
    pub node_size: u16,
    /// Ноль.
    pub reserved: u16,
    /// Число records, не включая runtime Root.
    pub node_count: u32,
    /// Смещение records.
    pub nodes_offset: u32,
    /// Версия package resource schema.
    pub resources_version: u32,
    /// Optional feature flags.
    pub flags: u32,
}

/// Декодированная запись одного узла.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UiIrNode {
    /// Индекс родителя среди records, `u32::MAX` = runtime Root.
    pub parent_index: u32,
    /// Component kind.
    pub kind: u16,
    /// Style class.
    pub style: u16,
    /// Length discriminator ширины.
    pub width_kind: u8,
    /// Length discriminator высоты.
    pub height_kind: u8,
    /// Align.
    pub align: u8,
    /// Grid columns.
    pub grid_columns: u8,
    /// Length payload ширины.
    pub width: u16,
    /// Length payload высоты.
    pub height: u16,
    /// Minimum width.
    pub min_width: u16,
    /// Minimum height.
    pub min_height: u16,
    /// Maximum width.
    pub max_width: u16,
    /// Maximum height.
    pub max_height: u16,
    /// Padding left.
    pub padding_left: u16,
    /// Padding top.
    pub padding_top: u16,
    /// Padding right.
    pub padding_right: u16,
    /// Padding bottom.
    pub padding_bottom: u16,
    /// Gap.
    pub gap: u16,
    /// Container-query breakpoint.
    pub breakpoint: u16,
    /// NodeState bits.
    pub state: u16,
    /// SemanticRole.
    pub semantic_role: u16,
    /// Tab index.
    pub tab_index: i16,
    /// 0 none, 1 text, 2 resource, 3 numeric value.
    pub content_kind: u16,
    /// Resource/value.
    pub content_value: u32,
    /// Command ID.
    pub command: u32,
    /// Accessible name resource.
    pub accessible_name: u32,
    /// Нули в v1.
    pub reserved_tail: [u8; 8],
}

/// Ошибка bounded parser/loader.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IrError {
    /// Заголовок/диапазон выходит за input.
    Truncated,
    /// Неизвестные magic/version/sizes/reserved.
    Unsupported,
    /// Integer overflow или overlapping/invalid range.
    InvalidRange,
    /// Неизвестный enum/property encoding.
    InvalidRecord,
    /// Parent обязан предшествовать ребёнку.
    InvalidHierarchy,
    /// Runtime tree capacity недостаточна.
    Capacity,
}

/// Проверяет весь IR до создания первого runtime-компонента.
pub fn validate_ir(bytes: &[u8]) -> Result<UiIrHeader, IrError> {
    let header = parse_header(bytes)?;
    let count = header.node_count as usize;
    let total = count.checked_mul(NODE_SIZE).ok_or(IrError::InvalidRange)?;
    let end = (header.nodes_offset as usize)
        .checked_add(total)
        .ok_or(IrError::InvalidRange)?;
    if end > bytes.len() {
        return Err(IrError::Truncated);
    }
    for index in 0..count {
        let record = parse_node(bytes, header.nodes_offset as usize + index * NODE_SIZE)?;
        validate_node(record, index)?;
    }
    Ok(header)
}

/// Атомарно загружает IR в то же дерево, которое использует Rust builder.
pub fn load_ir<const N: usize>(bytes: &[u8], tree: &mut Tree<N>) -> Result<UiIrHeader, IrError> {
    let header = validate_ir(bytes)?;
    if header.node_count as usize >= N {
        return Err(IrError::Capacity);
    }
    let mut candidate = Tree::<N>::new();
    let mut ids = [NodeId::NONE; N];
    for index in 0..header.node_count as usize {
        let record = parse_node(bytes, header.nodes_offset as usize + index * NODE_SIZE)?;
        let parent = if record.parent_index == u32::MAX {
            candidate.root()
        } else {
            ids[record.parent_index as usize]
        };
        let id = candidate
            .create(parent, to_spec(record)?)
            .map_err(|error| match error {
                TreeError::Capacity => IrError::Capacity,
                TreeError::InvalidNode | TreeError::InvalidHierarchy => IrError::InvalidHierarchy,
            })?;
        ids[index] = id;
    }
    *tree = candidate;
    Ok(header)
}

fn parse_header(bytes: &[u8]) -> Result<UiIrHeader, IrError> {
    if bytes.len() < HEADER_SIZE {
        return Err(IrError::Truncated);
    }
    let mut magic = [0u8; 8];
    magic.copy_from_slice(&bytes[..8]);
    let header = UiIrHeader {
        magic,
        version: u16_at(bytes, 8)?,
        header_size: u16_at(bytes, 10)?,
        node_size: u16_at(bytes, 12)?,
        reserved: u16_at(bytes, 14)?,
        node_count: u32_at(bytes, 16)?,
        nodes_offset: u32_at(bytes, 20)?,
        resources_version: u32_at(bytes, 24)?,
        flags: u32_at(bytes, 28)?,
    };
    if header.magic != UI_IR_MAGIC
        || header.version != rustos_abi::ui::UI_IR_VERSION
        || header.header_size as usize != HEADER_SIZE
        || header.node_size as usize != NODE_SIZE
        || header.reserved != 0
        || header.nodes_offset < header.header_size as u32
    {
        return Err(IrError::Unsupported);
    }
    Ok(header)
}

fn parse_node(bytes: &[u8], offset: usize) -> Result<UiIrNode, IrError> {
    let end = offset.checked_add(NODE_SIZE).ok_or(IrError::InvalidRange)?;
    let data = bytes.get(offset..end).ok_or(IrError::Truncated)?;
    let mut reserved_tail = [0u8; 8];
    reserved_tail.copy_from_slice(&data[56..64]);
    Ok(UiIrNode {
        parent_index: u32_at(data, 0)?,
        kind: u16_at(data, 4)?,
        style: u16_at(data, 6)?,
        width_kind: data[8],
        height_kind: data[9],
        align: data[10],
        grid_columns: data[11],
        width: u16_at(data, 12)?,
        height: u16_at(data, 14)?,
        min_width: u16_at(data, 16)?,
        min_height: u16_at(data, 18)?,
        max_width: u16_at(data, 20)?,
        max_height: u16_at(data, 22)?,
        padding_left: u16_at(data, 24)?,
        padding_top: u16_at(data, 26)?,
        padding_right: u16_at(data, 28)?,
        padding_bottom: u16_at(data, 30)?,
        gap: u16_at(data, 32)?,
        breakpoint: u16_at(data, 34)?,
        state: u16_at(data, 36)?,
        semantic_role: u16_at(data, 38)?,
        tab_index: i16_at(data, 40)?,
        content_kind: u16_at(data, 42)?,
        content_value: u32_at(data, 44)?,
        command: u32_at(data, 48)?,
        accessible_name: u32_at(data, 52)?,
        reserved_tail,
    })
}

fn validate_node(record: UiIrNode, index: usize) -> Result<(), IrError> {
    if record.parent_index != u32::MAX && record.parent_index as usize >= index {
        return Err(IrError::InvalidHierarchy);
    }
    if record.reserved_tail != [0; 8] {
        return Err(IrError::InvalidRecord);
    }
    let _ = to_spec(record)?;
    Ok(())
}

fn to_spec(record: UiIrNode) -> Result<NodeSpec, IrError> {
    let kind = component_kind(record.kind).ok_or(IrError::InvalidRecord)?;
    let align = match record.align {
        0 => Align::Start,
        1 => Align::Center,
        2 => Align::End,
        3 => Align::Stretch,
        _ => return Err(IrError::InvalidRecord),
    };
    let role = semantic_role(record.semantic_role).ok_or(IrError::InvalidRecord)?;
    let content = match record.content_kind {
        0 => Content::None,
        1 => Content::Text(ResourceId(record.content_value)),
        2 => Content::Resource(ResourceId(record.content_value)),
        3 if record.content_value <= u16::MAX as u32 => Content::Value(record.content_value as u16),
        _ => return Err(IrError::InvalidRecord),
    };
    Ok(NodeSpec {
        kind,
        layout: LayoutSpec {
            width: length(record.width_kind, record.width)?,
            height: length(record.height_kind, record.height)?,
            min_width: record.min_width,
            min_height: record.min_height,
            max_width: record.max_width,
            max_height: record.max_height,
            padding: Edges {
                left: record.padding_left,
                top: record.padding_top,
                right: record.padding_right,
                bottom: record.padding_bottom,
            },
            gap: record.gap,
            align,
            grid_columns: record.grid_columns.max(1),
            container_breakpoint: record.breakpoint,
        },
        style: record.style,
        state: NodeState(record.state),
        content,
        command: CommandId(record.command),
        role,
        accessible_name: ResourceId(record.accessible_name),
        tab_index: record.tab_index,
    })
}

fn length(kind: u8, value: u16) -> Result<Length, IrError> {
    Ok(match kind {
        0 => Length::Auto,
        1 => Length::Px(value),
        2 if value <= 1000 => Length::Percent(value),
        3 => Length::Fill(value),
        _ => return Err(IrError::InvalidRecord),
    })
}

fn component_kind(value: u16) -> Option<ComponentKind> {
    Some(match value {
        0 => ComponentKind::Root,
        1 => ComponentKind::Panel,
        2 => ComponentKind::Row,
        3 => ComponentKind::Column,
        4 => ComponentKind::Stack,
        5 => ComponentKind::Grid,
        6 => ComponentKind::Text,
        7 => ComponentKind::Image,
        8 => ComponentKind::Icon,
        9 => ComponentKind::Button,
        10 => ComponentKind::CheckBox,
        11 => ComponentKind::RadioButton,
        12 => ComponentKind::Switch,
        13 => ComponentKind::TextField,
        14 => ComponentKind::TextArea,
        15 => ComponentKind::Slider,
        16 => ComponentKind::Select,
        17 => ComponentKind::ScrollView,
        18 => ComponentKind::ListView,
        19 => ComponentKind::Divider,
        20 => ComponentKind::ProgressBar,
        21 => ComponentKind::TabView,
        22 => ComponentKind::Menu,
        23 => ComponentKind::Dialog,
        _ => return None,
    })
}

fn semantic_role(value: u16) -> Option<SemanticRole> {
    Some(match value {
        0 => SemanticRole::None,
        1 => SemanticRole::Group,
        2 => SemanticRole::Text,
        3 => SemanticRole::Heading,
        4 => SemanticRole::Button,
        5 => SemanticRole::CheckBox,
        6 => SemanticRole::RadioButton,
        7 => SemanticRole::Switch,
        8 => SemanticRole::TextField,
        9 => SemanticRole::List,
        10 => SemanticRole::ListItem,
        11 => SemanticRole::Menu,
        12 => SemanticRole::MenuItem,
        13 => SemanticRole::Dialog,
        14 => SemanticRole::Progress,
        15 => SemanticRole::Image,
        _ => return None,
    })
}

fn u16_at(bytes: &[u8], offset: usize) -> Result<u16, IrError> {
    let raw = bytes.get(offset..offset + 2).ok_or(IrError::Truncated)?;
    Ok(u16::from_le_bytes([raw[0], raw[1]]))
}
fn i16_at(bytes: &[u8], offset: usize) -> Result<i16, IrError> {
    Ok(u16_at(bytes, offset)? as i16)
}
fn u32_at(bytes: &[u8], offset: usize) -> Result<u32, IrError> {
    let raw = bytes.get(offset..offset + 4).ok_or(IrError::Truncated)?;
    Ok(u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]))
}

const _: () = assert!(core::mem::size_of::<UiIrHeader>() == HEADER_SIZE);
const _: () = assert!(core::mem::size_of::<UiIrNode>() == NODE_SIZE);

#[cfg(test)]
mod tests {
    use super::*;

    fn header(node_count: u32) -> [u8; HEADER_SIZE] {
        let mut bytes = [0u8; HEADER_SIZE];
        bytes[..8].copy_from_slice(&UI_IR_MAGIC);
        bytes[8..10].copy_from_slice(&rustos_abi::ui::UI_IR_VERSION.to_le_bytes());
        bytes[10..12].copy_from_slice(&(HEADER_SIZE as u16).to_le_bytes());
        bytes[12..14].copy_from_slice(&(NODE_SIZE as u16).to_le_bytes());
        bytes[16..20].copy_from_slice(&node_count.to_le_bytes());
        bytes[20..24].copy_from_slice(&(HEADER_SIZE as u32).to_le_bytes());
        bytes
    }

    #[test]
    fn empty_ir_loads_as_same_runtime_tree() {
        let bytes = header(0);
        let mut tree = Tree::<4>::new();
        assert!(load_ir(&bytes, &mut tree).is_ok());
        assert_eq!(tree.len(), 1);
    }

    #[test]
    fn truncated_records_are_rejected_before_tree_mutation() {
        let bytes = header(1);
        let mut tree = Tree::<4>::new();
        let before = tree.len();
        assert_eq!(load_ir(&bytes, &mut tree), Err(IrError::Truncated));
        assert_eq!(tree.len(), before);
    }
}
