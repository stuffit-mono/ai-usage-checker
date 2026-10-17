//! 트레이 아이콘 이미지를 직접 그린다: 사용률 단계 색의 둥근 사각형 위에 흰 숫자.
//! Windows 트레이는 글자를 표시할 수 없어서 숫자를 아이콘 픽셀로 그린다.

pub const SIZE: u32 = 32;

/// 5x7 비트맵 숫자. 각 줄의 하위 5비트가 왼쪽→오른쪽 픽셀이다.
const DIGITS: [[u8; 7]; 10] = [
    [0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110], // 0
    [0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110], // 1
    [0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111], // 2
    [0b11110, 0b00001, 0b00001, 0b01110, 0b00001, 0b00001, 0b11110], // 3
    [0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010], // 4
    [0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110], // 5
    [0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110], // 6
    [0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000], // 7
    [0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110], // 8
    [0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100], // 9
];
const BANG: [u8; 7] = [0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00000, 0b00100];

/// 위젯 화면과 같은 4단계 색 (파랑 → 초록 → 노랑 → 빨강)
pub fn level_color(percent: f64, warn: f64, crit: f64) -> [u8; 3] {
    if percent >= crit {
        [0xe0, 0x4f, 0x45]
    } else if percent >= warn {
        [0xe8, 0xb2, 0x1a]
    } else if percent >= warn.min(50.0) {
        [0x3f, 0x9a, 0x5f]
    } else {
        [0x3d, 0x7f, 0xd9]
    }
}

struct Canvas {
    px: Vec<u8>,
}

impl Canvas {
    fn new() -> Self {
        Self { px: vec![0; (SIZE * SIZE * 4) as usize] }
    }

    fn set(&mut self, x: i32, y: i32, rgba: [u8; 4]) {
        if x < 0 || y < 0 || x >= SIZE as i32 || y >= SIZE as i32 {
            return;
        }
        let i = ((y as u32 * SIZE + x as u32) * 4) as usize;
        self.px[i..i + 4].copy_from_slice(&rgba);
    }

    /// (inset, inset)부터 안쪽을 모서리가 둥근 사각형으로 채운다. inset 0이면 아이콘 전체.
    fn rounded_rect(&mut self, rgb: [u8; 3], inset: i32, radius: i32) {
        let (lo, hi) = (inset, SIZE as i32 - inset);
        for y in lo..hi {
            for x in lo..hi {
                let cx = x.clamp(lo + radius, hi - radius - 1);
                let cy = y.clamp(lo + radius, hi - radius - 1);
                let (dx, dy) = (x - cx, y - cy);
                if dx * dx + dy * dy <= radius * radius {
                    self.set(x, y, [rgb[0], rgb[1], rgb[2], 255]);
                }
            }
        }
    }

    /// `bold`만큼 오른쪽으로 획을 두껍게 그린다 (16px로 줄었을 때도 읽히도록).
    fn glyph(&mut self, rows: &[u8; 7], x0: i32, y0: i32, sx: i32, sy: i32, bold: i32, ink: [u8; 4]) {
        for (row, bits) in rows.iter().enumerate() {
            for col in 0..5 {
                if bits & (1 << (4 - col)) == 0 {
                    continue;
                }
                for dy in 0..sy {
                    for dx in 0..sx + bold {
                        self.set(x0 + col * sx + dx, y0 + row as i32 * sy + dy, ink);
                    }
                }
            }
        }
    }
}

/// 배경이 밝은 노랑일 때는 흰 글자가 묻히므로 어두운 글자를 쓴다.
fn ink_for(bg: [u8; 3]) -> [u8; 4] {
    let luma = 0.299 * bg[0] as f64 + 0.587 * bg[1] as f64 + 0.114 * bg[2] as f64;
    if luma > 150.0 {
        [0x1f, 0x1e, 0x1d, 255]
    } else {
        [255, 255, 255, 255]
    }
}

/// 서비스 대표색 (Claude 주황, ChatGPT 초록)
pub const CLAUDE_RGB: [u8; 3] = [0xd9, 0x77, 0x57];
pub const CHATGPT_RGB: [u8; 3] = [0x10, 0xa3, 0x7f];
/// 경고·위험 단계일 때 아이콘 둘레에 두르는 테두리 두께(px)
const RING: i32 = 4;

/// 사용률(%)을 그린 32x32 RGBA 픽셀. 100% 이상이면 "!"를 그린다.
/// 배경은 서비스 대표색이고, 경고 기준 이상이면 둘레를 노랑(경고)·빨강(위험) 테두리로 감싼다.
pub fn render(percent: f64, service: [u8; 3], warn: f64, crit: f64) -> Vec<u8> {
    let mut c = Canvas::new();
    let ink = ink_for(service);
    if percent >= warn {
        // 테두리와 배경 사이에 1px 어두운 선을 넣어 비슷한 색(주황 배경·빨강 테두리)도 구분되게 한다
        c.rounded_rect(level_color(percent, warn, crit), 0, 7);
        c.rounded_rect([0x1f, 0x1e, 0x1d], RING - 1, 5);
        c.rounded_rect(service, RING, 4);
    } else {
        c.rounded_rect(service, 0, 7);
    }

    let p = percent.round().max(0.0) as u32;
    let glyphs: Vec<&[u8; 7]> = if p >= 100 {
        vec![&BANG]
    } else if p >= 10 {
        vec![&DIGITS[(p / 10) as usize], &DIGITS[(p % 10) as usize]]
    } else {
        vec![&DIGITS[p as usize]]
    };

    // 두 자리는 가로 2배·세로 3배에 획 1px 보강(글자당 11px, 전체 24x21), 한 자리는 3배(15x21)
    let (sx, sy, gap, bold) = if glyphs.len() == 2 { (2, 3, 2, 1) } else { (3, 3, 0, 0) };
    let glyph_w = 5 * sx + bold;
    let width = glyphs.len() as i32 * glyph_w + (glyphs.len() as i32 - 1) * gap;
    let height = 7 * sy;
    let mut x = (SIZE as i32 - width) / 2;
    let y = (SIZE as i32 - height) / 2;
    for g in glyphs {
        c.glyph(g, x, y, sx, sy, bold, ink);
        x += glyph_w + gap;
    }
    c.px
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixel(px: &[u8], x: u32, y: u32) -> [u8; 4] {
        let i = ((y * SIZE + x) * 4) as usize;
        [px[i], px[i + 1], px[i + 2], px[i + 3]]
    }

    #[test]
    fn colors_follow_levels() {
        assert_eq!(level_color(10.0, 80.0, 95.0), [0x3d, 0x7f, 0xd9]);
        assert_eq!(level_color(60.0, 80.0, 95.0), [0x3f, 0x9a, 0x5f]);
        assert_eq!(level_color(85.0, 80.0, 95.0), [0xe8, 0xb2, 0x1a]);
        assert_eq!(level_color(99.0, 80.0, 95.0), [0xe0, 0x4f, 0x45]);
    }

    #[test]
    fn service_background_and_digits() {
        let px = render(42.0, CLAUDE_RGB, 80.0, 95.0);
        assert_eq!(px.len(), (SIZE * SIZE * 4) as usize);
        // 모서리는 투명, 가장자리 중앙은 서비스 색 (경고 전이라 테두리 없음)
        assert_eq!(pixel(&px, 0, 0)[3], 0);
        assert_eq!(pixel(&px, 16, 1), [0xd9, 0x77, 0x57, 255]);
        // 흰 픽셀(숫자)이 있어야 한다
        let white = px.chunks(4).filter(|p| p == &[255, 255, 255, 255]).count();
        assert!(white > 40);
    }

    #[test]
    fn warning_adds_level_ring() {
        let px = render(85.0, CHATGPT_RGB, 80.0, 95.0);
        assert_eq!(pixel(&px, 16, 1), [0xe8, 0xb2, 0x1a, 255]); // 노랑 테두리
        assert_eq!(pixel(&px, 16, 5), [0x10, 0xa3, 0x7f, 255]); // 안쪽은 서비스 색
        let px = render(97.0, CLAUDE_RGB, 80.0, 95.0);
        assert_eq!(pixel(&px, 16, 1), [0xe0, 0x4f, 0x45, 255]); // 빨강 테두리
    }

    /// 아이콘 모양을 눈으로 확인할 때만 실행: `ICON_DUMP_DIR=경로 cargo test dump_icons -- --ignored`
    #[test]
    #[ignore]
    fn dump_icons() {
        let dir = std::env::var("ICON_DUMP_DIR").expect("ICON_DUMP_DIR 필요");
        let cases = [(7.0, CLAUDE_RGB), (25.0, CHATGPT_RGB), (61.0, CLAUDE_RGB), (84.0, CHATGPT_RGB), (97.0, CLAUDE_RGB), (100.0, CHATGPT_RGB)];
        for (p, svc) in cases {
            std::fs::write(format!("{dir}/icon_{p}.rgba"), render(p, svc, 80.0, 95.0)).unwrap();
        }
    }

    #[test]
    fn full_usage_shows_bang() {
        let px = render(100.0, CLAUDE_RGB, 80.0, 95.0);
        let white = px.chunks(4).filter(|p| p == &[255, 255, 255, 255]).count();
        // "!"는 6칸 × 3×3 = 54픽셀
        assert_eq!(white, 54);
    }
}
