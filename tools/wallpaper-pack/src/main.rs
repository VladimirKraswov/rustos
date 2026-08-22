//! Host-конвертер RGB888 → фиксированные 4×4 блоки RustOS wallpaper.

use std::{env, fs, path::PathBuf, process};

const BLOCK_SIDE: usize = 4;
const BLOCK_BYTES: usize = 8;

fn main() {
    let arguments = env::args_os().skip(1).collect::<Vec<_>>();
    if arguments.len() != 4 {
        eprintln!("usage: rustos-wallpaper-pack WIDTH HEIGHT INPUT.rgb OUTPUT.rbc1");
        process::exit(2);
    }
    let width = arguments[0]
        .to_string_lossy()
        .parse::<usize>()
        .unwrap_or_else(|_| fail("invalid width"));
    let height = arguments[1]
        .to_string_lossy()
        .parse::<usize>()
        .unwrap_or_else(|_| fail("invalid height"));
    let input_path = PathBuf::from(&arguments[2]);
    let output_path = PathBuf::from(&arguments[3]);
    let input = fs::read(&input_path).unwrap_or_else(|error| fail(&error.to_string()));
    let expected = width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(3))
        .unwrap_or_else(|| fail("image size overflow"));
    if width == 0 || height == 0 || width % BLOCK_SIDE != 0 || height % BLOCK_SIDE != 0 {
        fail("dimensions must be non-zero multiples of four");
    }
    if input.len() != expected {
        fail("RGB888 input has unexpected size");
    }
    let output = encode(&input, width, height);
    fs::write(&output_path, &output).unwrap_or_else(|error| fail(&error.to_string()));
    println!(
        "wallpaper-pack: {}x{} {} -> {} bytes",
        width,
        height,
        input.len(),
        output.len()
    );
}

fn encode(input: &[u8], width: usize, height: usize) -> Vec<u8> {
    let mut output = Vec::with_capacity(width / 4 * (height / 4) * BLOCK_BYTES);
    for block_y in 0..height / BLOCK_SIDE {
        for block_x in 0..width / BLOCK_SIDE {
            let mut colors = [[0u8; 3]; 16];
            for local_y in 0..BLOCK_SIDE {
                for local_x in 0..BLOCK_SIDE {
                    let x = block_x * BLOCK_SIDE + local_x;
                    let y = block_y * BLOCK_SIDE + local_y;
                    let source = (y * width + x) * 3;
                    colors[local_y * BLOCK_SIDE + local_x]
                        .copy_from_slice(&input[source..source + 3]);
                }
            }
            let (minimum, maximum) = color_endpoints(&colors);
            let endpoint0 = pack_rgb565(maximum);
            let endpoint1 = pack_rgb565(minimum);
            let palette = palette(endpoint0, endpoint1);
            let mut indices = 0u32;
            for (index, color) in colors.iter().enumerate() {
                let selected = nearest(*color, palette);
                indices |= (selected as u32) << (index * 2);
            }
            output.extend_from_slice(&endpoint0.to_le_bytes());
            output.extend_from_slice(&endpoint1.to_le_bytes());
            output.extend_from_slice(&indices.to_le_bytes());
        }
    }
    output
}

fn color_endpoints(colors: &[[u8; 3]; 16]) -> ([u8; 3], [u8; 3]) {
    let mut minimum = colors[0];
    let mut maximum = colors[0];
    let mut minimum_luma = luma(colors[0]);
    let mut maximum_luma = minimum_luma;
    for color in colors.iter().copied().skip(1) {
        let value = luma(color);
        if value < minimum_luma {
            minimum = color;
            minimum_luma = value;
        }
        if value > maximum_luma {
            maximum = color;
            maximum_luma = value;
        }
    }
    (minimum, maximum)
}

fn luma(color: [u8; 3]) -> u32 {
    u32::from(color[0]) * 77 + u32::from(color[1]) * 150 + u32::from(color[2]) * 29
}

fn pack_rgb565(color: [u8; 3]) -> u16 {
    (((u16::from(color[0]) * 31 + 127) / 255) << 11)
        | (((u16::from(color[1]) * 63 + 127) / 255) << 5)
        | ((u16::from(color[2]) * 31 + 127) / 255)
}

fn unpack_rgb565(value: u16) -> [u8; 3] {
    let red = u32::from((value >> 11) & 0x1f);
    let green = u32::from((value >> 5) & 0x3f);
    let blue = u32::from(value & 0x1f);
    [
        ((red * 255 + 15) / 31) as u8,
        ((green * 255 + 31) / 63) as u8,
        ((blue * 255 + 15) / 31) as u8,
    ]
}

fn palette(first: u16, second: u16) -> [[u8; 3]; 4] {
    let first = unpack_rgb565(first);
    let second = unpack_rgb565(second);
    let interpolate = |a: [u8; 3], b: [u8; 3], left: u16, right: u16| {
        let mut result = [0u8; 3];
        for channel in 0..3 {
            result[channel] =
                ((u16::from(a[channel]) * left + u16::from(b[channel]) * right + 1) / 3) as u8;
        }
        result
    };
    [
        first,
        second,
        interpolate(first, second, 2, 1),
        interpolate(first, second, 1, 2),
    ]
}

fn nearest(color: [u8; 3], palette: [[u8; 3]; 4]) -> usize {
    palette
        .iter()
        .enumerate()
        .min_by_key(|(_, candidate)| {
            candidate
                .iter()
                .zip(color)
                .map(|(candidate, actual)| {
                    let difference = i32::from(*candidate) - i32::from(actual);
                    (difference * difference) as u32
                })
                .sum::<u32>()
        })
        .map(|(index, _)| index)
        .unwrap_or(0)
}

fn fail(message: &str) -> ! {
    eprintln!("wallpaper-pack: {message}");
    process::exit(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_block_has_fixed_size_and_distinct_indices() {
        let mut input = [0u8; 4 * 4 * 3];
        for (index, pixel) in input.as_chunks_mut::<3>().0.iter_mut().enumerate() {
            pixel.copy_from_slice(&[(index * 16) as u8, 80, 220]);
        }
        let encoded = encode(&input, 4, 4);
        assert_eq!(encoded.len(), BLOCK_BYTES);
        assert_ne!(&encoded[4..], &[0; 4]);
    }
}
