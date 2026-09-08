use smithay_client_toolkit::{
    session_lock::SessionLockSurface,
    shm::slot::{Buffer, SlotPool},
};
use wayland_client::{QueueHandle, protocol::wl_shm};

#[derive(Clone, Copy, Default)]
pub(crate) struct ButtonRect {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    visible: bool,
}

impl ButtonRect {
    pub(crate) fn contains(self, x: f64, y: f64) -> bool {
        self.visible
            && x >= self.x as f64
            && x < (self.x + self.width) as f64
            && y >= self.y as f64
            && y < (self.y + self.height) as f64
    }
}

pub(crate) struct LockSurfaceState {
    pub(crate) surface: SessionLockSurface,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) pool: Option<SlotPool>,
    pub(crate) buffer: Option<Buffer>,
    pub(crate) button: ButtonRect,
}

pub(crate) struct SurfaceContent<'a> {
    pub(crate) background: u32,
    pub(crate) clock: &'a str,
    pub(crate) timer: &'a str,
    pub(crate) password_len: usize,
    pub(crate) show_error: bool,
    pub(crate) button_enabled: bool,
}

pub(crate) fn draw_surface(
    surface: &mut LockSurfaceState,
    qh: &QueueHandle<crate::App>,
    content: SurfaceContent<'_>,
) {
    let button_height = (surface.height / 25).clamp(48, 80);
    let button_width = (surface.width / 3).clamp(220, 360);
    surface.button = ButtonRect {
        x: surface.width.saturating_sub(button_width) / 2,
        y: surface
            .height
            .saturating_sub(button_height + surface.height / 10),
        width: button_width,
        height: button_height,
        visible: content.button_enabled,
    };

    let Some(pool) = surface.pool.as_mut() else {
        return;
    };
    let (width, height) = (surface.width as i32, surface.height as i32);
    let (buffer, canvas) = match surface.buffer.take() {
        Some(buffer) => match buffer.canvas(pool) {
            Some(canvas) => (buffer, canvas),
            None => match pool.create_buffer(width, height, width * 4, wl_shm::Format::Argb8888) {
                Ok((buffer, canvas)) => (buffer, canvas),
                Err(_) => return,
            },
        },
        None => match pool.create_buffer(width, height, width * 4, wl_shm::Format::Argb8888) {
            Ok((buffer, canvas)) => (buffer, canvas),
            Err(_) => return,
        },
    };

    let background_pixel = argb(content.background);
    for pixel in canvas.chunks_exact_mut(4) {
        pixel.copy_from_slice(&background_pixel.to_le_bytes());
    }

    let clock_scale = (surface.height / 120).clamp(4, 12);
    let label_scale = (clock_scale / 2).max(2);
    draw_centered(
        canvas,
        surface.width,
        surface.height,
        content.clock,
        surface.height / 2 - clock_scale * 10,
        clock_scale,
        0xFFFFFFFF,
    );
    draw_password_dots(
        canvas,
        surface.width,
        surface.height,
        content.password_len,
        surface.height / 2 + clock_scale * 4,
        clock_scale,
        0xFFFFFFFF,
    );
    if content.show_error {
        draw_centered(
            canvas,
            surface.width,
            surface.height,
            "WRONG PASSWORD",
            surface.height / 2 + clock_scale * 25,
            label_scale,
            0xFFFFA0A0,
        );
    }
    if surface.button.visible {
        let button = surface.button;
        fill_rect(
            canvas,
            surface.width,
            button.x,
            button.y,
            button.width,
            button.height,
            0xFF2B2145,
        );
        let text_scale = (surface.height / 360).clamp(2, 4);
        let text = "SCREEN OFF";
        let text_width = text.chars().count() as u32 * 6 * text_scale - text_scale;
        draw_text(
            canvas,
            surface.width,
            surface.height,
            text,
            button.x + button.width.saturating_sub(text_width) / 2,
            button.y + button.height.saturating_sub(7 * text_scale) / 2,
            text_scale,
            0xFFFFFFFF,
        );
    }

    let timer_scale = (label_scale / 2).max(1);
    draw_centered(
        canvas,
        surface.width,
        surface.height,
        content.timer,
        surface.button.y + surface.button.height + timer_scale * 4,
        timer_scale,
        0xFFD8D2E8,
    );

    let _ = buffer.attach_to(surface.surface.wl_surface());
    surface
        .surface
        .wl_surface()
        .damage_buffer(0, 0, width, height);
    surface.surface.wl_surface().commit();
    surface.buffer = Some(buffer);
    let _ = qh;
}

fn fill_rect(
    canvas: &mut [u8],
    width: u32,
    x: u32,
    y: u32,
    rect_width: u32,
    rect_height: u32,
    color: u32,
) {
    for row in y..y.saturating_add(rect_height) {
        for column in x..x.saturating_add(rect_width) {
            let offset = ((row * width + column) * 4) as usize;
            if offset + 4 <= canvas.len() {
                canvas[offset..offset + 4].copy_from_slice(&color.to_le_bytes());
            }
        }
    }
}

fn draw_password_dots(
    canvas: &mut [u8],
    width: u32,
    height: u32,
    count: usize,
    y: u32,
    scale: u32,
    color: u32,
) {
    if count == 0 {
        return;
    }
    let size = scale.max(3);
    let gap = size * 2;
    let total = count as u32 * size + (count.saturating_sub(1) as u32 * gap);
    let mut x = width.saturating_sub(total) / 2;
    for _ in 0..count {
        for dy in 0..size {
            for dx in 0..size {
                let px = x + dx;
                let py = y + dy;
                if px < width && py < height {
                    let offset = ((py * width + px) * 4) as usize;
                    canvas[offset..offset + 4].copy_from_slice(&color.to_le_bytes());
                }
            }
        }
        x += size + gap;
    }
}

fn argb(rgb: u32) -> u32 {
    0xFF00_0000 | rgb
}

fn draw_centered(
    canvas: &mut [u8],
    width: u32,
    height: u32,
    text: &str,
    y: u32,
    scale: u32,
    color: u32,
) {
    let glyph_width = 5 * scale;
    let spacing = scale;
    let text_width = text
        .chars()
        .count()
        .saturating_mul((glyph_width + spacing) as usize)
        .saturating_sub(spacing as usize);
    let x = width.saturating_sub(text_width as u32) / 2;
    draw_text(canvas, width, height, text, x, y, scale, color);
}

#[allow(clippy::too_many_arguments)]
fn draw_text(
    canvas: &mut [u8],
    width: u32,
    height: u32,
    text: &str,
    x: u32,
    y: u32,
    scale: u32,
    color: u32,
) {
    let mut cursor = x;
    for character in text.chars() {
        if let Some(glyph) = glyph(character) {
            for (row, bits) in glyph.iter().enumerate() {
                for column in 0..5u32 {
                    if bits & (1 << (4 - column)) == 0 {
                        continue;
                    }
                    for dy in 0..scale {
                        for dx in 0..scale {
                            let px = cursor + column * scale + dx;
                            let py = y + row as u32 * scale + dy;
                            if px < width && py < height {
                                let offset = ((py * width + px) * 4) as usize;
                                canvas[offset..offset + 4].copy_from_slice(&color.to_le_bytes());
                            }
                        }
                    }
                }
            }
        }
        cursor += 6 * scale;
    }
}

fn glyph(character: char) -> Option<[u8; 7]> {
    Some(match character {
        '0' => [
            0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110,
        ],
        '1' => [
            0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
        ],
        '2' => [
            0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111,
        ],
        '3' => [
            0b11110, 0b00001, 0b00001, 0b01110, 0b00001, 0b00001, 0b11110,
        ],
        '4' => [
            0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010,
        ],
        '5' => [
            0b11111, 0b10000, 0b10000, 0b11110, 0b00001, 0b00001, 0b11110,
        ],
        '6' => [
            0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110,
        ],
        '7' => [
            0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000,
        ],
        '8' => [
            0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110,
        ],
        '9' => [
            0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b11100,
        ],
        ':' => [
            0b00000, 0b00100, 0b00100, 0b00000, 0b00100, 0b00100, 0b00000,
        ],
        'A' => [
            0b01110, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001,
        ],
        'C' => [
            0b01110, 0b10001, 0b10000, 0b10000, 0b10000, 0b10001, 0b01110,
        ],
        'D' => [
            0b11110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b11110,
        ],
        'E' => [
            0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b11111,
        ],
        'F' => [
            0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b10000,
        ],
        'G' => [
            0b01110, 0b10001, 0b10000, 0b10111, 0b10001, 0b10001, 0b01110,
        ],
        'I' => [
            0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b11111,
        ],
        'N' => [
            0b10001, 0b11001, 0b10101, 0b10011, 0b10001, 0b10001, 0b10001,
        ],
        'O' => [
            0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110,
        ],
        'P' => [
            0b11110, 0b10001, 0b10001, 0b11110, 0b10000, 0b10000, 0b10000,
        ],
        'R' => [
            0b11110, 0b10001, 0b10001, 0b11110, 0b10100, 0b10010, 0b10001,
        ],
        'S' => [
            0b01111, 0b10000, 0b10000, 0b01110, 0b00001, 0b00001, 0b11110,
        ],
        'T' => [
            0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100,
        ],
        'W' => [
            0b10001, 0b10001, 0b10001, 0b10101, 0b10101, 0b11011, 0b10001,
        ],
        'Y' => [
            0b10001, 0b10001, 0b01010, 0b00100, 0b00100, 0b00100, 0b00100,
        ],
        ' ' => [0; 7],
        _ => return None,
    })
}

pub(crate) fn local_clock() -> String {
    let now = unsafe { libc::time(std::ptr::null_mut()) };
    let mut local = unsafe { std::mem::zeroed::<libc::tm>() };
    unsafe { libc::localtime_r(&now, &mut local) };
    format!(
        "{:02}:{:02}:{:02}",
        local.tm_hour, local.tm_min, local.tm_sec
    )
}

pub(crate) fn format_duration(seconds: u64) -> String {
    format!("{:02}:{:02}", seconds / 60, seconds % 60)
}
