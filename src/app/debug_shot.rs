//! Dev-only screenshot hook.
//!
//! Launch with `INXM_SCREENSHOT=/path/out.png` to render for a moment,
//! capture the viewport to a PNG, and exit. Combine with `INXM_VIEW=` and
//! `INXM_DEMO=1` to capture specific views with demo content. Used to verify
//! visual changes headlessly; does nothing unless the env var is set.

use std::path::PathBuf;

use egui::ColorImage;

pub const SCREENSHOT_ENV: &str = "INXM_SCREENSHOT";
/// Wall-clock seconds before capturing — TIME-based, not frame-based: the
/// continuous repaints below make frames essentially free, so a frame count
/// would fire before the engine bootstrap finishes.
const CAPTURE_AFTER_SECS: f64 = 3.0;

pub struct ShotState {
    target: Option<PathBuf>,
    requested: bool,
}

impl ShotState {
    pub fn from_env() -> Self {
        Self {
            target: std::env::var(SCREENSHOT_ENV).ok().map(PathBuf::from),
            requested: false,
        }
    }

    /// Call once per `update`. Requests the capture after a settle delay,
    /// saves the resulting image, and closes the app.
    pub fn tick(&mut self, ctx: &egui::Context) {
        let Some(path) = self.target.clone() else {
            return;
        };
        ctx.request_repaint();

        if !self.requested && ctx.input(|i| i.time) >= CAPTURE_AFTER_SECS {
            self.requested = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
        }

        let image = ctx.input(|i| {
            i.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(image) = image {
            match save_png(&image, &path) {
                Ok(()) => eprintln!("screenshot saved: {}", path.display()),
                Err(e) => eprintln!("screenshot failed: {e}"),
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }
}

// ─── Minimal PNG writer (RGBA8, stored/uncompressed deflate) ─────────────────
//
// Deliberately dependency-free: this is a dev utility, size does not matter.

fn save_png(image: &ColorImage, path: &std::path::Path) -> std::io::Result<()> {
    let [width, height] = image.size;
    let raw: Vec<u8> = image
        .pixels
        .chunks(width)
        .flat_map(|row| {
            std::iter::once(0u8) // filter type: None
                .chain(row.iter().flat_map(|px| px.to_array()))
        })
        .collect();

    let ihdr: Vec<u8> = (width as u32)
        .to_be_bytes()
        .into_iter()
        .chain((height as u32).to_be_bytes())
        .chain([8u8, 6, 0, 0, 0]) // 8-bit, RGBA, deflate, none, no interlace
        .collect();

    let file: Vec<u8> = [0x89u8, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]
        .into_iter()
        .chain(chunk(b"IHDR", &ihdr))
        .chain(chunk(b"IDAT", &zlib_stored(&raw)))
        .chain(chunk(b"IEND", &[]))
        .collect();

    std::fs::write(path, file)
}

fn chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let body: Vec<u8> = kind.iter().copied().chain(data.iter().copied()).collect();
    (data.len() as u32)
        .to_be_bytes()
        .into_iter()
        .chain(body.iter().copied())
        .chain(crc32(&body).to_be_bytes())
        .collect()
}

/// zlib stream with stored (uncompressed) deflate blocks.
fn zlib_stored(raw: &[u8]) -> Vec<u8> {
    const MAX_BLOCK: usize = 65_535;
    let block_count = raw.len().div_ceil(MAX_BLOCK).max(1);
    let blocks = raw.chunks(MAX_BLOCK.min(raw.len()).max(1)).enumerate();
    [0x78u8, 0x01]
        .into_iter()
        .chain(blocks.flat_map(move |(i, block)| {
            let is_final = if i + 1 == block_count { 1u8 } else { 0 };
            let len = block.len() as u16;
            [is_final]
                .into_iter()
                .chain(len.to_le_bytes())
                .chain((!len).to_le_bytes())
                .chain(block.iter().copied())
                .collect::<Vec<u8>>()
        }))
        .chain(adler32(raw).to_be_bytes())
        .collect()
}

fn crc32(bytes: &[u8]) -> u32 {
    const POLY: u32 = 0xEDB8_8320;
    !bytes.iter().fold(!0u32, |crc, &byte| {
        (0..8).fold(crc ^ byte as u32, |c, _| (c >> 1) ^ (POLY * (c & 1)))
    })
}

fn adler32(bytes: &[u8]) -> u32 {
    const MOD: u32 = 65_521;
    let (a, b) = bytes.iter().fold((1u32, 0u32), |(a, b), &byte| {
        let a = (a + byte as u32) % MOD;
        (a, (b + a) % MOD)
    });
    (b << 16) | a
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_matches_known_vector() {
        // CRC-32 of "123456789" is 0xCBF43926.
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn adler32_matches_known_vector() {
        // Adler-32 of "Wikipedia" is 0x11E60398.
        assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
    }

    #[test]
    fn png_round_trips_through_a_parser_smoke_check() {
        let image = ColorImage::filled([3, 2], egui::Color32::from_rgb(10, 20, 30));
        let tmp = std::env::temp_dir().join("inxm_shot_test.png");
        save_png(&image, &tmp).unwrap();
        let bytes = std::fs::read(&tmp).unwrap();
        assert_eq!(
            &bytes[..8],
            &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]
        );
        assert!(bytes.windows(4).any(|w| w == b"IEND"));
        let _ = std::fs::remove_file(&tmp);
    }
}
