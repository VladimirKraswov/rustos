//! CI-проверка GUI по трём PPM-скриншотам QEMU:
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
    /// Читает и валидирует PPM-файл (max=255, размер не меньше 800×600).
    fn read(path: &Path) -> Result<Self, String> {
        let data = fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut cursor = 0;
        let magic = token(&data, &mut cursor)?;
        if magic != b"P6" {
            return Err(format!("{}: expected binary P6 PPM", path.display()));
        }
        let width = parse_number(token(&data, &mut cursor)?)?;
        let height = parse_number(token(&data, &mut cursor)?)?;
        let max = parse_number(token(&data, &mut cursor)?)?;
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

    fn rgb(&self, x: usize, y: usize) -> [u8; 3] {
        let index = (y * self.width + x) * 3;
        [
            self.pixels[index],
            self.pixels[index + 1],
            self.pixels[index + 2],
        ]
    }
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
        verify_virgl_triangle(&image);
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

fn verify_virgl_triangle(image: &Image) {
    let background = image.rgb(8, 8);
    if background[0] > 24 || background[1] > 32 || !(18..=48).contains(&background[2]) {
        fatal(format!(
            "VirGL background has unexpected color: {background:?}"
        ));
    }
    let center = image.rgb(image.width / 2, image.height / 2);
    let distance = center
        .iter()
        .zip(background)
        .map(|(value, base)| value.abs_diff(base) as usize)
        .sum::<usize>();
    if distance < 90 {
        fatal(format!(
            "center was not rasterized by triangle pipeline: bg={background:?}, center={center:?}"
        ));
    }
    let mut colored = 0usize;
    for y in (image.height / 5..image.height * 4 / 5).step_by(4) {
        for x in (image.width / 5..image.width * 4 / 5).step_by(4) {
            let pixel = image.rgb(x, y);
            let delta = pixel
                .iter()
                .zip(background)
                .map(|(value, base)| value.abs_diff(base) as usize)
                .sum::<usize>();
            colored += usize::from(delta > 80);
        }
    }
    if colored < 1000 {
        fatal(format!(
            "VirGL triangle area is too small or absent: samples={colored}"
        ));
    }
    println!(
        "VirGL verify OK: {}x{}, GPU triangle covers {} sampled pixels",
        image.width, image.height, colored
    );
}

fn usage() {
    eprintln!("usage: rustos-gui-check <terminal.ppm> <dragged.ppm> <minimized.ppm> | --virgl <triangle.ppm>");
}

fn fatal(message: String) -> ! {
    eprintln!("GUI verify FAIL: {message}");
    std::process::exit(1)
}
