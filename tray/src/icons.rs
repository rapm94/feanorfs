//! Tray icon pixels — simple template-style glyphs per mirror state.

use tray_icon::Icon;

const SIZE: u32 = 22;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayVisual {
    Idle,
    OutOfSync,
    Offline,
    Conflict,
    Error,
    Syncing,
    Paused,
}

pub fn icon_for(visual: TrayVisual) -> Icon {
    let rgba = match visual {
        TrayVisual::Idle => circle_icon(0x33, 0xcc, 0x66),
        TrayVisual::OutOfSync => circle_icon(0x44, 0xaa, 0xff),
        TrayVisual::Offline => circle_icon(0x99, 0x99, 0x99),
        TrayVisual::Conflict => circle_icon(0xff, 0x66, 0x33),
        TrayVisual::Syncing => ring_icon(0x44, 0xaa, 0xff),
        TrayVisual::Error => circle_icon(0xcc, 0x33, 0x33),
        TrayVisual::Paused => circle_icon(0xcc, 0xcc, 0x44),
    };
    Icon::from_rgba(rgba, SIZE, SIZE).expect("valid icon rgba")
}

fn circle_icon(r: u8, g: u8, b: u8) -> Vec<u8> {
    let mut buf = vec![0u8; (SIZE * SIZE * 4) as usize];
    let cx = SIZE as f32 / 2.0;
    let cy = SIZE as f32 / 2.0;
    let radius = SIZE as f32 / 2.0 - 2.0;
    for y in 0..SIZE {
        for x in 0..SIZE {
            let dx = x as f32 + 0.5 - cx;
            let dy = y as f32 + 0.5 - cy;
            let i = ((y * SIZE + x) * 4) as usize;
            if dx * dx + dy * dy <= radius * radius {
                buf[i] = r;
                buf[i + 1] = g;
                buf[i + 2] = b;
                buf[i + 3] = 255;
            }
        }
    }
    buf
}

fn ring_icon(r: u8, g: u8, b: u8) -> Vec<u8> {
    let mut buf = vec![0u8; (SIZE * SIZE * 4) as usize];
    let cx = SIZE as f32 / 2.0;
    let cy = SIZE as f32 / 2.0;
    let outer = SIZE as f32 / 2.0 - 1.5;
    let inner = outer - 3.0;
    for y in 0..SIZE {
        for x in 0..SIZE {
            let dx = x as f32 + 0.5 - cx;
            let dy = y as f32 + 0.5 - cy;
            let d2 = dx * dx + dy * dy;
            let i = ((y * SIZE + x) * 4) as usize;
            if d2 <= outer * outer && d2 >= inner * inner {
                buf[i] = r;
                buf[i + 1] = g;
                buf[i + 2] = b;
                buf[i + 3] = 255;
            }
        }
    }
    buf
}

pub fn visual_from_state(mirror_state: &str, paused: bool) -> TrayVisual {
    if paused {
        return TrayVisual::Paused;
    }
    match mirror_state {
        "syncing" => TrayVisual::Syncing,
        "conflict" => TrayVisual::Conflict,
        "error" => TrayVisual::Error,
        "offline" => TrayVisual::Offline,
        "out_of_sync" => TrayVisual::OutOfSync,
        _ => TrayVisual::Idle,
    }
}
