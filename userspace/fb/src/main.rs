//! NovaX ring-3 framebuffer console.
//!
//! The kernel grants a bounded `CapKind::Mmio` capability for the linear
//! framebuffer and sends the screen geometry as an `FbInfo` IPC message.
//! Everything below runs in ring 3: the kernel never draws a pixel.

#![no_std]
#![no_main]

#[path = "../../common/abi.rs"]
mod abi;

mod font8x16;

const CAP_MMIO: u64 = 1;
const CAP_INFO: u64 = 2;
const CAP_CONSOLE: u64 = 3;

const FONT_W: usize = 8;
const FONT_H: usize = 16;
const TAB: usize = 4;

const BG: u32 = 0x000A_0A0C;
const FG: u32 = 0x00E5_E5E5;
const RED: u32 = 0x00E6_0000;
const DIM: u32 = 0x002A_2A30;

const BANNER: &[u8] = b"NOVAX v0.1.2 // HEART MICROKERNEL";

#[panic_handler]
fn on_panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}

fn le_u16(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}

fn le_u32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

fn le_u64(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes([
        b[off],
        b[off + 1],
        b[off + 2],
        b[off + 3],
        b[off + 4],
        b[off + 5],
        b[off + 6],
        b[off + 7],
    ])
}

/// Screen geometry delivered by the kernel in an `FbInfo` message.
struct FbInfo {
    width: usize,
    height: usize,
    pitch: usize,
    bpp: u16,
    red_mask_size: u8,
    red_mask_shift: u8,
    green_mask_size: u8,
    green_mask_shift: u8,
    blue_mask_size: u8,
    blue_mask_shift: u8,
}

impl FbInfo {
    fn parse(msg: &[u8]) -> Option<Self> {
        if msg.len() < abi::FB_INFO_LEN || le_u32(msg, 0) != abi::FB_INFO_MAGIC {
            return None;
        }
        let width = le_u32(msg, 8) as usize;
        let height = le_u32(msg, 12) as usize;
        let pitch = le_u32(msg, 16) as usize;
        if width == 0 || height == 0 || pitch == 0 {
            return None;
        }
        Some(Self {
            width,
            height,
            pitch,
            bpp: le_u16(msg, 20),
            red_mask_size: msg[22],
            red_mask_shift: msg[23],
            green_mask_size: msg[24],
            green_mask_shift: msg[25],
            blue_mask_size: msg[26],
            blue_mask_shift: msg[27],
        })
    }
}

/// Text console rendered directly into the linear framebuffer.
struct Framebuffer {
    base: *mut u8,
    width: usize,
    height: usize,
    pitch: usize,
    bpp: u16,
    bytes_per_pixel: usize,
    red_mask_size: u8,
    red_mask_shift: u8,
    green_mask_size: u8,
    green_mask_shift: u8,
    blue_mask_size: u8,
    blue_mask_shift: u8,
    cols: usize,
    rows: usize,
    top: usize,
    cx: usize,
    cy: usize,
}

impl Framebuffer {
    fn new(virt: u64, info: &FbInfo) -> Option<Self> {
        let bytes_per_pixel = (info.bpp as usize).div_ceil(8);
        if virt == 0
            || !matches!(info.bpp, 16 | 24 | 32)
            || info.width < FONT_W
            || info.height < FONT_H
            || info.pitch < info.width * bytes_per_pixel
        {
            return None;
        }

        let cols = info.width / FONT_W;
        let rows = info.height / FONT_H;
        if cols == 0 || rows == 0 {
            return None;
        }

        Some(Self {
            base: virt as *mut u8,
            width: info.width,
            height: info.height,
            pitch: info.pitch,
            bpp: info.bpp,
            bytes_per_pixel,
            red_mask_size: info.red_mask_size,
            red_mask_shift: info.red_mask_shift,
            green_mask_size: info.green_mask_size,
            green_mask_shift: info.green_mask_shift,
            blue_mask_size: info.blue_mask_size,
            blue_mask_shift: info.blue_mask_shift,
            cols,
            rows,
            top: 0,
            cx: 0,
            cy: 0,
        })
    }

    fn pack(&self, r: u8, g: u8, b: u8) -> u32 {
        fn channel(value: u8, size: u8, shift: u8) -> u32 {
            if size == 0 {
                return 0;
            }
            let v = if size >= 8 {
                value as u32
            } else {
                (value as u32) >> (8 - size)
            };
            v << shift
        }
        channel(r, self.red_mask_size, self.red_mask_shift)
            | channel(g, self.green_mask_size, self.green_mask_shift)
            | channel(b, self.blue_mask_size, self.blue_mask_shift)
    }

    fn write_pixel(&mut self, x: usize, y: usize, color: u32) {
        if x >= self.width || y >= self.height {
            return;
        }
        let offset = y * self.pitch + x * self.bytes_per_pixel;
        // SAFETY: `offset` is inside the framebuffer window granted by the
        // MMIO capability; the kernel mapped exactly `height * pitch` bytes.
        unsafe {
            let p = self.base.add(offset);
            match self.bpp {
                32 => core::ptr::write_volatile(p as *mut u32, color),
                24 => {
                    core::ptr::write_volatile(p, (color & 0xFF) as u8);
                    core::ptr::write_volatile(p.add(1), ((color >> 8) & 0xFF) as u8);
                    core::ptr::write_volatile(p.add(2), ((color >> 16) & 0xFF) as u8);
                }
                16 => core::ptr::write_volatile(p as *mut u16, color as u16),
                _ => {}
            }
        }
    }

    fn fill_rect(&mut self, x: usize, y: usize, w: usize, h: usize, color: u32) {
        for row in y..y.saturating_add(h).min(self.height) {
            for col in x..x.saturating_add(w).min(self.width) {
                self.write_pixel(col, row, color);
            }
        }
    }

    fn draw_glyph(&mut self, x: usize, y: usize, ch: u8, color: u32) {
        let glyph = &font8x16::FONT_8X16[(ch & 0x7F) as usize];
        for (row, bits) in glyph.iter().enumerate() {
            let py = y + row;
            for col in 0..FONT_W {
                if *bits & (0x80 >> (FONT_W - 1 - col)) != 0 {
                    self.write_pixel(x + col, py, color);
                }
            }
        }
    }

    fn draw_str(&mut self, x: usize, y: usize, text: &[u8], color: u32) {
        for (i, &b) in text.iter().enumerate() {
            let px = x + i * FONT_W;
            if px + FONT_W > self.width {
                break;
            }
            let ch = if (0x20..=0x7E).contains(&b) { b } else { b'?' };
            self.draw_glyph(px, y, ch, color);
        }
    }

    fn clear_cell(&mut self, cx: usize, cy: usize) {
        self.fill_rect(
            cx * FONT_W,
            cy * FONT_H,
            FONT_W,
            FONT_H,
            self.pack(10, 10, 12),
        );
    }

    fn scroll(&mut self) {
        if self.rows <= self.top + 1 {
            return;
        }
        let cell_bytes = FONT_H * self.pitch;
        for row in self.top..self.rows - 1 {
            let dst = row * cell_bytes;
            let src = (row + 1) * cell_bytes;
            // SAFETY: both offsets are inside the mapped framebuffer and
            // one full 8x16 cell row fits below `rows`.
            unsafe {
                core::ptr::copy(self.base.add(src), self.base.add(dst), cell_bytes);
            }
        }
        let last = (self.rows - 1) * FONT_H;
        self.fill_rect(0, last, self.width, FONT_H, self.pack(10, 10, 12));
    }

    fn newline(&mut self) {
        self.cx = 0;
        if self.cy + 1 >= self.rows {
            self.scroll();
            self.cy = self.rows - 1;
        } else {
            self.cy += 1;
        }
    }

    fn backspace(&mut self) {
        if self.cx > 0 {
            self.cx -= 1;
        } else if self.cy > self.top {
            self.cy -= 1;
            self.cx = self.cols - 1;
        } else {
            return;
        }
        self.clear_cell(self.cx, self.cy);
    }

    fn put_byte(&mut self, b: u8) {
        match b {
            b'\n' => self.newline(),
            b'\r' => self.cx = 0,
            0x08 => self.backspace(),
            b'\t' => {
                let target = (self.cx + TAB) & !(TAB - 1);
                while self.cx < target && self.cx < self.cols {
                    let x = self.cx * FONT_W;
                    let y = self.cy * FONT_H;
                    self.draw_glyph(x, y, b' ', self.pack(229, 229, 229));
                    self.cx += 1;
                    if self.cx >= self.cols {
                        self.newline();
                    }
                }
            }
            0x20..=0x7E => {
                let x = self.cx * FONT_W;
                let y = self.cy * FONT_H;
                self.draw_glyph(x, y, b, self.pack(229, 229, 229));
                self.cx += 1;
                if self.cx >= self.cols {
                    self.newline();
                }
            }
            _ => {}
        }
    }

    fn init(&mut self) {
        let bg = self.pack(10, 10, 12);
        self.fill_rect(0, 0, self.width, self.height, bg);

        if self.rows >= 5 {
            let x = if self.cols > BANNER.len() {
                (self.cols - BANNER.len()) / 2
            } else {
                0
            };
            self.draw_str(x * FONT_W, 0, BANNER, self.pack(230, 0, 0));
            for col in 0..self.cols {
                self.draw_glyph(col * FONT_W, FONT_H, b'-', self.pack(42, 42, 48));
            }
            self.top = 3;
            self.cy = 3;
        }
        self.cx = 0;
    }
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    let mut map = [0u8; 24];
    if abi::map_mmio(CAP_MMIO, &mut map) == u64::MAX {
        abi::write(b"fb: map_mmio failed\n");
        abi::exit(1);
    }
    let virt = le_u64(&map, 0);

    let mut info_msg = [0u8; 64];
    let n = abi::recv(CAP_INFO, &mut info_msg);
    if n != abi::FB_INFO_LEN as u64 {
        abi::write(b"fb: missing FbInfo\n");
        abi::exit(1);
    }
    let Some(info) = FbInfo::parse(&info_msg[..n as usize]) else {
        abi::write(b"fb: bad FbInfo\n");
        abi::exit(1);
    };
    let Some(mut fb) = Framebuffer::new(virt, &info) else {
        abi::write(b"fb: unsupported framebuffer format\n");
        abi::exit(1);
    };

    fb.init();
    abi::write(b"fb: ready\n");

    let mut buf = [0u8; 64];
    loop {
        let n = abi::recv(CAP_CONSOLE, &mut buf);
        if n == u64::MAX || n == 0 {
            continue;
        }
        for &b in &buf[..n as usize] {
            fb.put_byte(b);
        }
    }
}
