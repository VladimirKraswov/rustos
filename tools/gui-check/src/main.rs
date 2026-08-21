//! CI-проверка GUI по PPM-скриншотам QEMU и XWD surface из Xvfb:
//! `<terminal.ppm>` (окно открыто) `<dragged.ppm>` (окно перетащено)
//! `<minimized.ppm>` (окно свёрнуто в taskbar).
//!
//! Проверяются именно изменения геометрии: старый левый край окна стал
//! обоями, новый правый — тёмным terminal, центр при минимизации — синий
//! desktop, taskbar не пустой, и общее число изменившихся пикселей
//! достаточно велико (защита от «счастливой» пустой VM).

use std::{env, fs, path::Path};

/// Разобранный binary PPM (P6) — RGB, 8 бит на канал.
struct Image {
    width: usize,
    height: usize,
    pixels: Vec<u8>,
}

impl Image {
    /// Автоматически различает PPM P6 и XWD. XWD нужен для GL/dmabuf
    /// scanout, который QEMU намеренно не экспортирует через `screendump`.
    fn read(path: &Path) -> Result<Self, String> {
        let data = fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        if data.starts_with(b"P6") {
            return Self::read_ppm(path, &data);
        }
        Self::read_xwd(path, &data)
    }

    fn read_ppm(path: &Path, data: &[u8]) -> Result<Self, String> {
        let mut cursor = 0;
        let magic = token(data, &mut cursor)?;
        if magic != b"P6" {
            return Err(format!("{}: expected binary P6 PPM", path.display()));
        }
        let width = parse_number(token(data, &mut cursor)?)?;
        let height = parse_number(token(data, &mut cursor)?)?;
        let max = parse_number(token(data, &mut cursor)?)?;
        if max != 255 || width < 800 || height < 600 {
            return Err(format!(
                "{}: invalid framebuffer {width}x{height}, max={max}",
                path.display()
            ));
        }
        // После max-value PPM содержит ровно один whitespace-разделитель.
        // Нельзя пропускать «все whitespace»: первый красный байт картинки
        // вполне может быть 10 (`\n`), как у тёмных обоев RustOS.
        match (data.get(cursor), data.get(cursor + 1)) {
            (Some(b'\r'), Some(b'\n')) => cursor += 2,
            (Some(byte), _) if byte.is_ascii_whitespace() => cursor += 1,
            _ => return Err(format!("{}: missing PPM pixel separator", path.display())),
        }
        let bytes = width
            .checked_mul(height)
            .and_then(|v| v.checked_mul(3))
            .ok_or("PPM dimensions overflow")?;
        let end = cursor.checked_add(bytes).ok_or("PPM offset overflow")?;
        let pixels = data
            .get(cursor..end)
            .ok_or_else(|| format!("{}: truncated pixel data", path.display()))?
            .to_vec();
        Ok(Self {
            width,
            height,
            pixels,
        })
    }

    /// Разбирает ZPixmap XWD, который Xvfb поддерживает как mmap framebuffer.
    /// Header всегда big-endian, а порядок байтов pixels задан отдельно.
    fn read_xwd(path: &Path, data: &[u8]) -> Result<Self, String> {
        const HEADER_WORDS: usize = 25;
        const XWD_VERSION: usize = 7;
        const ZPIXMAP: usize = 2;
        if data.len() < HEADER_WORDS * 4 {
            return Err(format!("{}: truncated XWD header", path.display()));
        }
        let mut header = [0usize; HEADER_WORDS];
        for (index, value) in header.iter_mut().enumerate() {
            let offset = index * 4;
            *value = u32::from_be_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
        }
        let header_size = header[0];
        let width = header[4];
        let height = header[5];
        let byte_order = header[7];
        let bits_per_pixel = header[11];
        let bytes_per_line = header[12];
        let red_mask = header[14] as u32;
        let green_mask = header[15] as u32;
        let blue_mask = header[16] as u32;
        let colors = header[19];
        if header[1] != XWD_VERSION
            || header[2] != ZPIXMAP
            || width < 800
            || height < 600
            || !matches!(bits_per_pixel, 16 | 24 | 32)
            || bytes_per_line < width * bits_per_pixel.div_ceil(8)
            || red_mask == 0
            || green_mask == 0
            || blue_mask == 0
            || byte_order > 1
        {
            return Err(format!(
                "{}: unsupported XWD {}x{} bpp={} order={} masks={red_mask:#x}/{green_mask:#x}/{blue_mask:#x}",
                path.display(), width, height, bits_per_pixel, byte_order
            ));
        }
        let pixel_offset = header_size
            .checked_add(colors.checked_mul(12).ok_or("XWD color table overflow")?)
            .ok_or("XWD pixel offset overflow")?;
        let pixel_bytes = height
            .checked_mul(bytes_per_line)
            .ok_or("XWD dimensions overflow")?;
        let source = data
            .get(pixel_offset..pixel_offset + pixel_bytes)
            .ok_or_else(|| format!("{}: truncated XWD pixels", path.display()))?;
        let bytes_per_pixel = bits_per_pixel / 8;
        let mut pixels = vec![0; width * height * 3];
        for y in 0..height {
            for x in 0..width {
                let source_offset = y * bytes_per_line + x * bytes_per_pixel;
                let bytes = &source[source_offset..source_offset + bytes_per_pixel];
                let mut pixel = 0u32;
                if byte_order == 0 {
                    for (shift, byte) in bytes.iter().enumerate() {
                        pixel |= u32::from(*byte) << (shift * 8);
                    }
                } else {
                    for byte in bytes {
                        pixel = (pixel << 8) | u32::from(*byte);
                    }
                }
                let target = (y * width + x) * 3;
                pixels[target] = component(pixel, red_mask);
                pixels[target + 1] = component(pixel, green_mask);
                pixels[target + 2] = component(pixel, blue_mask);
            }
        }
        Ok(Self {
            width,
            height,
            pixels,
        })
    }

    fn write_ppm(&self, path: &Path) -> Result<(), String> {
        use std::io::Write;

        let mut output = fs::File::create(path).map_err(|e| format!("{}: {e}", path.display()))?;
        write!(output, "P6\n{} {}\n255\n", self.width, self.height)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        output
            .write_all(&self.pixels)
            .map_err(|e| format!("{}: {e}", path.display()))
    }

    fn rgb(&self, x: usize, y: usize) -> [u8; 3] {
        let index = (y * self.width + x) * 3;
        [
            self.pixels[index],
            self.pixels[index + 1],
            self.pixels[index + 2],
        ]
    }
}

fn component(pixel: u32, mask: u32) -> u8 {
    let shift = mask.trailing_zeros();
    let maximum = mask >> shift;
    let value = (pixel & mask) >> shift;
    ((u64::from(value) * 255 + u64::from(maximum) / 2) / u64::from(maximum)) as u8
}

/// Следующий whitespace-токен заголовка PPM, с пропуском `#`-комментариев.
fn token<'a>(data: &'a [u8], cursor: &mut usize) -> Result<&'a [u8], String> {
    loop {
        while data.get(*cursor).is_some_and(u8::is_ascii_whitespace) {
            *cursor += 1;
        }
        if data.get(*cursor) != Some(&b'#') {
            break;
        }
        while data.get(*cursor).is_some_and(|b| *b != b'\n') {
            *cursor += 1;
        }
    }
    let start = *cursor;
    while data
        .get(*cursor)
        .is_some_and(|byte| !byte.is_ascii_whitespace())
    {
        *cursor += 1;
    }
    if start == *cursor {
        return Err("missing PPM token".into());
    }
    Ok(&data[start..*cursor])
}

fn parse_number(token: &[u8]) -> Result<usize, String> {
    std::str::from_utf8(token)
        .map_err(|_| "non-UTF8 PPM number".to_string())?
        .parse()
        .map_err(|_| "invalid PPM number".to_string())
}

fn main() {
    let mut args = env::args_os().skip(1);
    let before_path = args.next().unwrap_or_else(|| {
        usage();
        std::process::exit(2);
    });
    if before_path == "--virgl" {
        let path = args.next().unwrap_or_else(|| {
            usage();
            std::process::exit(2);
        });
        let image = match Image::read(Path::new(&path)) {
            Ok(image) => image,
            Err(error) => fatal(error),
        };
        verify_virgl_showcase(&image);
        if let Some(output) = args.next() {
            if let Err(error) = image.write_ppm(Path::new(&output)) {
                fatal(error);
            }
        }
        if args.next().is_some() {
            usage();
            std::process::exit(2);
        }
        return;
    }
    let dragged_path = args.next().unwrap_or_else(|| {
        usage();
        std::process::exit(2);
    });
    let after_path = args.next().unwrap_or_else(|| {
        usage();
        std::process::exit(2);
    });
    let before = match Image::read(Path::new(&before_path)) {
        Ok(image) => image,
        Err(error) => fatal(error),
    };
    let after = match Image::read(Path::new(&after_path)) {
        Ok(image) => image,
        Err(error) => fatal(error),
    };
    let dragged = match Image::read(Path::new(&dragged_path)) {
        Ok(image) => image,
        Err(error) => fatal(error),
    };
    if before.width != dragged.width
        || before.height != dragged.height
        || before.width != after.width
        || before.height != after.height
    {
        fatal("screenshots have different dimensions".into());
    }

    // Окно стартует с x=120,y=57 и после тестового drag смещается вправо-
    // вниз. Старый левый участок должен стать обоями, а новый правый —
    // тёмной областью terminal. Это проверяет именно изменение геометрии,
    // а не только получение mouse packet.
    let old_only_before = before.rgb(130, 110);
    let old_only_dragged = dragged.rgb(130, 110);
    let new_only_before = before.rgb(before.width - 80, 200);
    let new_only_dragged = dragged.rgb(before.width - 80, 200);
    if old_only_before == old_only_dragged
        || old_only_dragged[2] < 30
        || new_only_dragged.iter().any(|channel| *channel > 45)
        || new_only_before == new_only_dragged
    {
        fatal(format!(
            "drag geometry did not move the window: old={old_only_before:?}->{old_only_dragged:?}, new={new_only_before:?}->{new_only_dragged:?}"
        ));
    }

    let center = (before.width / 2, before.height / 2);
    let terminal_pixel = before.rgb(center.0, center.1);
    let desktop_pixel = after.rgb(center.0, center.1);
    if terminal_pixel.iter().any(|channel| *channel > 45) {
        fatal(format!("terminal center is not dark: {terminal_pixel:?}"));
    }
    if desktop_pixel[2] < 35 || desktop_pixel == terminal_pixel {
        fatal(format!(
            "minimize did not expose blue desktop: before={terminal_pixel:?}, after={desktop_pixel:?}"
        ));
    }
    let taskbar = after.rgb(before.width / 2, before.height - 12);
    if taskbar.iter().all(|channel| *channel < 3) {
        fatal("taskbar is black or missing".into());
    }

    let mut changed = 0;
    for pixel in (0..before.width * before.height).step_by(97) {
        let offset = pixel * 3;
        if before.pixels[offset..offset + 3] != after.pixels[offset..offset + 3] {
            changed += 1;
        }
    }
    if changed < 1000 {
        fatal(format!(
            "window action changed too few sampled pixels: {changed}"
        ));
    }
    println!(
        "GUI verify OK: {}x{}, terminal/VFS + drag + minimize changed {} sampled pixels",
        before.width, before.height, changed
    );
}

fn verify_virgl_showcase(image: &Image) {
    // XWD содержит весь X11 screen. Stage Aurora отличается от чёрного Xvfb
    // blue-dominant градиентом; по нему находим QEMU surface независимо от
    // положения окна и декораций window manager.
    let mut stage_pixels = 0usize;
    let mut min_x = image.width;
    let mut min_y = image.height;
    let mut max_x = 0usize;
    let mut max_y = 0usize;
    for y in 0..image.height {
        for x in 0..image.width {
            let pixel = image.rgb(x, y);
            if pixel[2] >= 18
                && pixel[2] > pixel[0].saturating_add(7)
                && pixel[2] > pixel[1].saturating_add(4)
            {
                stage_pixels += 1;
                min_x = min_x.min(x);
                min_y = min_y.min(y);
                max_x = max_x.max(x);
                max_y = max_y.max(y);
            }
        }
    }
    if stage_pixels < 100_000 || max_x <= min_x + 900 || max_y <= min_y + 560 {
        fatal(format!(
            "Aurora stage was not found: pixels={stage_pixels}, bounds={min_x},{min_y}..{max_x},{max_y}"
        ));
    }

    let mut lit = 0usize;
    let mut cyan = 0usize;
    let mut violet = 0usize;
    let horizontal_margin = (max_x - min_x) / 5;
    let vertical_margin = (max_y - min_y) / 5;
    for y in (min_y + vertical_margin..max_y - vertical_margin).step_by(3) {
        for x in (min_x + horizontal_margin..max_x - horizontal_margin).step_by(3) {
            let pixel = image.rgb(x, y);
            let highest = pixel.iter().copied().max().unwrap_or(0);
            let lowest = pixel.iter().copied().min().unwrap_or(0);
            lit += usize::from(highest >= 70 && highest.saturating_sub(lowest) >= 35);
            cyan += usize::from(
                pixel[2] >= 90 && pixel[1] >= 45 && pixel[2] > pixel[0].saturating_add(30),
            );
            violet += usize::from(
                pixel[2] >= 70 && pixel[0] >= 45 && pixel[1].saturating_add(20) < pixel[2],
            );
        }
    }
    if lit < 1200 || cyan < 250 || violet < 120 {
        fatal(format!(
            "lit 3D object is absent: lit={lit}, cyan={cyan}, violet={violet}"
        ));
    }
    println!(
        "Mesa/VirGL verify OK: {}x{}, stage={} lit={} cyan={} violet={}",
        image.width, image.height, stage_pixels, lit, cyan, violet
    );
}

fn usage() {
    eprintln!("usage: rustos-gui-check <terminal.ppm> <dragged.ppm> <minimized.ppm> | --virgl <showcase.ppm|screen.xwd> [converted.ppm]");
}

fn fatal(message: String) -> ! {
    eprintln!("GUI verify FAIL: {message}");
    std::process::exit(1)
}
