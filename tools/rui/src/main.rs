//! Компилятор небольшого декларативного языка RUI source -> `.rui` IR.
//!
//! Source остаётся удобным для человека, но в образ ОС попадает только
//! проверенное fixed-record представление. Runtime не содержит text parser.

use std::{collections::BTreeMap, env, fs, path::Path, process};

use rustos_system_ui::{validate_ir, UiIrNode, UI_IR_MAGIC};

const HEADER_SIZE: usize = 32;
const NODE_SIZE: usize = 64;

fn main() {
    if let Err(error) = run() {
        eprintln!("rustos-rui: {error}");
        process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let mut args = env::args().skip(1);
    let command = args.next().ok_or_else(usage)?;
    let input = args.next().ok_or_else(usage)?;
    if command == "check" {
        if args.next().is_some() {
            return Err(usage());
        }
        let source = fs::read_to_string(&input).map_err(|error| format!("{input}: {error}"))?;
        let output = compile(&source)?;
        validate_ir(&output).map_err(|error| format!("internal IR validation: {error:?}"))?;
        println!(
            "RUI OK: {} node(s)",
            (output.len() - HEADER_SIZE) / NODE_SIZE
        );
        return Ok(());
    }
    if command != "compile" {
        return Err(usage());
    }
    let output = args.next().ok_or_else(usage)?;
    if args.next().is_some() {
        return Err(usage());
    }
    let source = fs::read_to_string(&input).map_err(|error| format!("{input}: {error}"))?;
    let bytes = compile(&source)?;
    validate_ir(&bytes).map_err(|error| format!("internal IR validation: {error:?}"))?;
    fs::write(Path::new(&output), &bytes).map_err(|error| format!("{output}: {error}"))?;
    println!(
        "RUI v1: {input} -> {output} ({} node(s), {} bytes)",
        (bytes.len() - HEADER_SIZE) / NODE_SIZE,
        bytes.len()
    );
    Ok(())
}

fn usage() -> String {
    "usage: rustos-rui compile INPUT.rui OUTPUT.rui | rustos-rui check INPUT.rui".into()
}

fn compile(source: &str) -> Result<Vec<u8>, String> {
    let mut nodes = Vec::new();
    let mut ids = BTreeMap::<String, u32>::new();
    let mut saw_header = false;
    for (line_index, raw) in source.lines().enumerate() {
        let line_number = line_index + 1;
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if !saw_header {
            if line != "rui 1" {
                return Err(format!(
                    "line {line_number}: first statement must be `rui 1`"
                ));
            }
            saw_header = true;
            continue;
        }
        let node = parse_node(line, line_number, &ids)?;
        let id =
            property(line, "id").ok_or_else(|| format!("line {line_number}: missing id=NAME"))?;
        if id == "root" || ids.contains_key(id) {
            return Err(format!("line {line_number}: duplicate/reserved id `{id}`"));
        }
        ids.insert(id.to_owned(), nodes.len() as u32);
        nodes.push(node);
    }
    if !saw_header {
        return Err("empty source: expected `rui 1`".into());
    }
    encode(&nodes)
}

fn parse_node(line: &str, number: usize, ids: &BTreeMap<String, u32>) -> Result<UiIrNode, String> {
    let kind_name = line
        .split_ascii_whitespace()
        .next()
        .ok_or_else(|| format!("line {number}: component expected"))?;
    let kind = component(kind_name)
        .ok_or_else(|| format!("line {number}: unknown component `{kind_name}`"))?;
    let parent_name = property(line, "parent").unwrap_or("root");
    let parent_index = if parent_name == "root" {
        u32::MAX
    } else {
        *ids.get(parent_name).ok_or_else(|| {
            format!("line {number}: parent `{parent_name}` must be declared earlier")
        })?
    };
    let (width_kind, width) = length(property(line, "width").unwrap_or("auto"), number)?;
    let (height_kind, height) = length(property(line, "height").unwrap_or("auto"), number)?;
    let padding = parse_u16(property(line, "padding").unwrap_or("0"), "padding", number)?;
    let content = if let Some(text) = property(line, "text") {
        (1, parse_u32(text, "text", number)?)
    } else if let Some(resource) = property(line, "resource") {
        (2, parse_u32(resource, "resource", number)?)
    } else if let Some(value) = property(line, "value") {
        let value = parse_u32(value, "value", number)?;
        if value > 1000 {
            return Err(format!("line {number}: value must be 0..=1000"));
        }
        (3, value)
    } else {
        (0, 0)
    };
    Ok(UiIrNode {
        parent_index,
        kind,
        style: parse_u16(property(line, "style").unwrap_or("0"), "style", number)?,
        width_kind,
        height_kind,
        align: align(property(line, "align").unwrap_or("stretch"))
            .ok_or_else(|| format!("line {number}: align must be start|center|end|stretch"))?,
        grid_columns: parse_u8(property(line, "columns").unwrap_or("1"), "columns", number)?.max(1),
        width,
        height,
        min_width: parse_u16(
            property(line, "min-width").unwrap_or("0"),
            "min-width",
            number,
        )?,
        min_height: parse_u16(
            property(line, "min-height").unwrap_or("0"),
            "min-height",
            number,
        )?,
        max_width: parse_u16(
            property(line, "max-width").unwrap_or("0"),
            "max-width",
            number,
        )?,
        max_height: parse_u16(
            property(line, "max-height").unwrap_or("0"),
            "max-height",
            number,
        )?,
        padding_left: padding,
        padding_top: padding,
        padding_right: padding,
        padding_bottom: padding,
        gap: parse_u16(property(line, "gap").unwrap_or("0"), "gap", number)?,
        breakpoint: parse_u16(
            property(line, "breakpoint").unwrap_or("0"),
            "breakpoint",
            number,
        )?,
        state: parse_u16(property(line, "state").unwrap_or("0"), "state", number)?,
        semantic_role: role(property(line, "role").unwrap_or(default_role(kind)))
            .ok_or_else(|| format!("line {number}: unknown semantic role"))?,
        tab_index: property(line, "tab")
            .unwrap_or(if focusable(kind) { "0" } else { "-1" })
            .parse::<i16>()
            .map_err(|_| format!("line {number}: tab must be i16"))?,
        content_kind: content.0,
        content_value: content.1,
        command: parse_u32(property(line, "command").unwrap_or("0"), "command", number)?,
        accessible_name: parse_u32(
            property(line, "accessible")
                .or_else(|| property(line, "text"))
                .unwrap_or("0"),
            "accessible",
            number,
        )?,
        reserved_tail: [0; 8],
    })
}

fn encode(nodes: &[UiIrNode]) -> Result<Vec<u8>, String> {
    let byte_count = nodes
        .len()
        .checked_mul(NODE_SIZE)
        .and_then(|bytes| bytes.checked_add(HEADER_SIZE))
        .ok_or("IR is too large")?;
    let mut output = vec![0u8; byte_count];
    output[..8].copy_from_slice(&UI_IR_MAGIC);
    put_u16(&mut output, 8, rustos_abi::ui::UI_IR_VERSION);
    put_u16(&mut output, 10, HEADER_SIZE as u16);
    put_u16(&mut output, 12, NODE_SIZE as u16);
    put_u32(&mut output, 16, nodes.len() as u32);
    put_u32(&mut output, 20, HEADER_SIZE as u32);
    put_u32(&mut output, 24, 1);
    for (index, node) in nodes.iter().enumerate() {
        let offset = HEADER_SIZE + index * NODE_SIZE;
        put_u32(&mut output, offset, node.parent_index);
        put_u16(&mut output, offset + 4, node.kind);
        put_u16(&mut output, offset + 6, node.style);
        output[offset + 8] = node.width_kind;
        output[offset + 9] = node.height_kind;
        output[offset + 10] = node.align;
        output[offset + 11] = node.grid_columns;
        for (relative, value) in [
            (12, node.width),
            (14, node.height),
            (16, node.min_width),
            (18, node.min_height),
            (20, node.max_width),
            (22, node.max_height),
            (24, node.padding_left),
            (26, node.padding_top),
            (28, node.padding_right),
            (30, node.padding_bottom),
            (32, node.gap),
            (34, node.breakpoint),
            (36, node.state),
            (38, node.semantic_role),
            (40, node.tab_index as u16),
            (42, node.content_kind),
        ] {
            put_u16(&mut output, offset + relative, value);
        }
        put_u32(&mut output, offset + 44, node.content_value);
        put_u32(&mut output, offset + 48, node.command);
        put_u32(&mut output, offset + 52, node.accessible_name);
    }
    Ok(output)
}

fn property<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    line.split_ascii_whitespace().skip(1).find_map(|token| {
        let (key, value) = token.split_once('=')?;
        (key == name).then_some(value)
    })
}

fn length(value: &str, line: usize) -> Result<(u8, u16), String> {
    if value == "auto" {
        return Ok((0, 0));
    }
    if value == "fill" {
        return Ok((3, 1));
    }
    if let Some(weight) = value.strip_prefix("fill:") {
        return Ok((3, parse_u16(weight, "fill weight", line)?));
    }
    if let Some(px) = value.strip_prefix("px:") {
        return Ok((1, parse_u16(px, "pixels", line)?));
    }
    if let Some(percent) = value.strip_prefix("pct:") {
        let percent = parse_u16(percent, "percent", line)?;
        if percent > 1000 {
            return Err(format!("line {line}: pct uses 0..=1000"));
        }
        return Ok((2, percent));
    }
    Err(format!(
        "line {line}: length must be auto|fill[:WEIGHT]|px:N|pct:0..1000"
    ))
}

fn component(name: &str) -> Option<u16> {
    Some(match name {
        "Panel" => 1,
        "Row" => 2,
        "Column" => 3,
        "Stack" => 4,
        "Grid" => 5,
        "Text" => 6,
        "Image" => 7,
        "Icon" => 8,
        "Button" => 9,
        "CheckBox" => 10,
        "RadioButton" => 11,
        "Switch" => 12,
        "TextField" => 13,
        "TextArea" => 14,
        "Slider" => 15,
        "Select" => 16,
        "ScrollView" => 17,
        "ListView" => 18,
        "Divider" => 19,
        "ProgressBar" => 20,
        "TabView" => 21,
        "Menu" => 22,
        "Dialog" => 23,
        "ScrollBar" => 24,
        _ => return None,
    })
}
fn focusable(kind: u16) -> bool {
    matches!(kind, 9..=18 | 21 | 22 | 24)
}
fn align(name: &str) -> Option<u8> {
    Some(match name {
        "start" => 0,
        "center" => 1,
        "end" => 2,
        "stretch" => 3,
        _ => return None,
    })
}
fn role(name: &str) -> Option<u16> {
    Some(match name {
        "none" => 0,
        "group" => 1,
        "text" => 2,
        "heading" => 3,
        "button" => 4,
        "checkbox" => 5,
        "radio" => 6,
        "switch" => 7,
        "textfield" => 8,
        "list" => 9,
        "listitem" => 10,
        "menu" => 11,
        "menuitem" => 12,
        "dialog" => 13,
        "progress" => 14,
        "image" => 15,
        "scrollbar" => 16,
        _ => return None,
    })
}
fn default_role(kind: u16) -> &'static str {
    match kind {
        6 => "text",
        7 | 8 => "image",
        9 => "button",
        10 => "checkbox",
        11 => "radio",
        12 => "switch",
        13 | 14 => "textfield",
        18 => "list",
        20 => "progress",
        22 => "menu",
        23 => "dialog",
        _ => "none",
    }
}

fn parse_u8(value: &str, field: &str, line: usize) -> Result<u8, String> {
    value
        .parse()
        .map_err(|_| format!("line {line}: {field} must be u8"))
}
fn parse_u16(value: &str, field: &str, line: usize) -> Result<u16, String> {
    value
        .parse()
        .map_err(|_| format!("line {line}: {field} must be u16"))
}
fn parse_u32(value: &str, field: &str, line: usize) -> Result<u32, String> {
    value
        .parse()
        .map_err(|_| format!("line {line}: {field} must be u32"))
}
fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}
fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiler_produces_runtime_valid_ir() {
        let source = "rui 1\nColumn id=page width=fill height=fill padding=12 gap=8\nButton id=save parent=page text=7 command=42 width=fill height=px:40\n";
        let output = compile(source).unwrap();
        assert_eq!(validate_ir(&output).unwrap().node_count, 2);
    }

    #[test]
    fn forward_parent_reference_is_rejected() {
        let source = "rui 1\nButton id=save parent=page text=7\nColumn id=page\n";
        assert!(compile(source).unwrap_err().contains("declared earlier"));
    }
}
