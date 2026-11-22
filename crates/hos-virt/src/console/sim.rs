//! In-process simulated VNC server for the `mock` provider (demo tenant,
//! tests): RFB 3.8 with "VNC authentication", an 800×480 true-colour screen
//! with a fake login prompt that echoes typed keys, raw encoding only.
//! Nothing touches the network: the relay talks to it over an in-memory pipe.

use std::io;

use hos_core::secret::SecretString;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream};

use super::{parse_version, vnc_auth_response};

/// Screen width.
pub const WIDTH: u16 = 800;
/// Screen height.
pub const HEIGHT: u16 = 480;
const SCALE: usize = 2;
const CELL_W: usize = 6 * SCALE;
const CELL_H: usize = 10 * SCALE;
const MARGIN: usize = 16;
const HEADER_H: usize = 36;
const MAX_INPUT: usize = 48;
const MAX_CUT_TEXT: u32 = 1024 * 1024;

const BG: (u8, u8, u8) = (11, 15, 20);
const HEADER_BG: (u8, u8, u8) = (31, 41, 55);
const HEADER_FG: (u8, u8, u8) = (229, 231, 235);
const TEXT: (u8, u8, u8) = (209, 213, 219);
const ACCENT: (u8, u8, u8) = (52, 211, 153);

/// Starts a simulated server and returns the client end of the pipe.
/// `title` is shown on the screen (VM name / hostname).
pub fn spawn(password: SecretString, title: &str) -> DuplexStream {
    let (client, server) = tokio::io::duplex(512 * 1024);
    let title = title.to_owned();
    tokio::spawn(async move {
        let _ = serve(server, password, title).await;
    });
    client
}

#[derive(Debug, Clone, Copy)]
struct PixelFormat {
    bpp: u8,
    big_endian: bool,
    max: [u16; 3],
    shift: [u8; 3],
}

impl PixelFormat {
    const DEFAULT: Self = Self {
        bpp: 32,
        big_endian: false,
        max: [255, 255, 255],
        shift: [16, 8, 0],
    };

    fn to_bytes(self) -> [u8; 16] {
        let mut b = [0u8; 16];
        b[0] = self.bpp;
        b[1] = 24;
        b[2] = u8::from(self.big_endian);
        b[3] = 1;
        b[4..6].copy_from_slice(&self.max[0].to_be_bytes());
        b[6..8].copy_from_slice(&self.max[1].to_be_bytes());
        b[8..10].copy_from_slice(&self.max[2].to_be_bytes());
        b[10] = self.shift[0];
        b[11] = self.shift[1];
        b[12] = self.shift[2];
        b
    }

    fn parse(b: &[u8]) -> Option<Self> {
        let bpp = b[0];
        let true_colour = b[3] != 0;
        if !matches!(bpp, 8 | 16 | 32) || !true_colour {
            return None;
        }
        Some(Self {
            bpp,
            big_endian: b[2] != 0,
            max: [
                u16::from_be_bytes([b[4], b[5]]),
                u16::from_be_bytes([b[6], b[7]]),
                u16::from_be_bytes([b[8], b[9]]),
            ],
            shift: [b[10], b[11], b[12]],
        })
    }

    fn encode(&self, (r, g, b): (u8, u8, u8), out: &mut Vec<u8>) {
        let c = |v: u8, i: usize| -> u32 {
            (u32::from(v) * u32::from(self.max[i]) / 255) << u32::from(self.shift[i].min(31))
        };
        let px = c(r, 0) | c(g, 1) | c(b, 2);
        match (self.bpp, self.big_endian) {
            (8, _) => out.push(px as u8),
            (16, true) => out.extend_from_slice(&(px as u16).to_be_bytes()),
            (16, false) => out.extend_from_slice(&(px as u16).to_le_bytes()),
            (_, true) => out.extend_from_slice(&px.to_be_bytes()),
            (_, false) => out.extend_from_slice(&px.to_le_bytes()),
        }
    }
}

struct Screen {
    title: String,
    lines: Vec<String>,
    input: String,
    pixels: Vec<(u8, u8, u8)>,
}

impl Screen {
    fn new(title: String) -> Self {
        let mut s = Self {
            title,
            lines: vec![String::new(), "UBUNTU 24.04 LTS TTY1".into(), String::new()],
            input: String::new(),
            pixels: vec![BG; usize::from(WIDTH) * usize::from(HEIGHT)],
        };
        s.render();
        s
    }

    fn rows(&self) -> usize {
        (usize::from(HEIGHT) - HEADER_H - MARGIN) / CELL_H
    }

    fn key(&mut self, keysym: u32) -> bool {
        match keysym {
            0x20..=0x7e if self.input.chars().count() < MAX_INPUT => {
                if let Some(c) = char::from_u32(keysym) {
                    self.input.push(c);
                }
            }
            0xff08 => {
                self.input.pop();
            }
            0xff0d | 0xff8d => {
                let typed = std::mem::take(&mut self.input);
                self.lines.push(format!("LOGIN: {typed}"));
                self.lines
                    .push("SIMULATED CONSOLE - NO REAL SERVER BEHIND IT.".into());
                self.lines.push(String::new());
                let keep = self.rows().saturating_sub(2);
                if self.lines.len() > keep {
                    let drop = self.lines.len() - keep;
                    self.lines.drain(..drop);
                }
            }
            _ => return false,
        }
        self.render();
        true
    }

    fn render(&mut self) {
        let w = usize::from(WIDTH);
        for (i, p) in self.pixels.iter_mut().enumerate() {
            *p = if i / w < HEADER_H { HEADER_BG } else { BG };
        }
        let title = format!("HOSTINGOS DEMO CONSOLE  -  {}", self.title);
        self.text(MARGIN, (HEADER_H - 7 * SCALE) / 2, &title, HEADER_FG);
        let mut y = HEADER_H + MARGIN;
        let lines = self.lines.clone();
        for line in &lines {
            self.text(MARGIN, y, line, TEXT);
            y += CELL_H;
        }
        let prompt = "LOGIN: ";
        self.text(MARGIN, y, prompt, ACCENT);
        let input = self.input.clone();
        let x = MARGIN + prompt.len() * CELL_W;
        self.text(x, y, &input, TEXT);
        // Block cursor.
        let cx = x + input.chars().count() * CELL_W;
        self.fill(cx, y, 5 * SCALE, 7 * SCALE, TEXT);
    }

    fn fill(&mut self, x: usize, y: usize, w: usize, h: usize, c: (u8, u8, u8)) {
        let (sw, sh) = (usize::from(WIDTH), usize::from(HEIGHT));
        for yy in y..(y + h).min(sh) {
            for xx in x..(x + w).min(sw) {
                self.pixels[yy * sw + xx] = c;
            }
        }
    }

    fn text(&mut self, x: usize, y: usize, s: &str, c: (u8, u8, u8)) {
        for (i, ch) in s.chars().enumerate() {
            let rows = glyph(ch);
            let gx = x + i * CELL_W;
            for (ry, row) in rows.iter().enumerate() {
                for (rx, bit) in row.bytes().enumerate() {
                    if bit == b'#' {
                        self.fill(gx + rx * SCALE, y + ry * SCALE, SCALE, SCALE, c);
                    }
                }
            }
        }
    }

    fn rect(&self, pf: &PixelFormat, x: u16, y: u16, w: u16, h: u16) -> Vec<u8> {
        let sw = usize::from(WIDTH);
        let mut out = Vec::with_capacity(usize::from(w) * usize::from(h) * 4 + 12);
        out.extend_from_slice(&x.to_be_bytes());
        out.extend_from_slice(&y.to_be_bytes());
        out.extend_from_slice(&w.to_be_bytes());
        out.extend_from_slice(&h.to_be_bytes());
        out.extend_from_slice(&0i32.to_be_bytes());
        for yy in usize::from(y)..usize::from(y) + usize::from(h) {
            for xx in usize::from(x)..usize::from(x) + usize::from(w) {
                pf.encode(self.pixels[yy * sw + xx], &mut out);
            }
        }
        out
    }
}

/// 5×7 glyphs; lower case is shown in upper case, unknown characters as `?`.
fn glyph(c: char) -> [&'static str; 7] {
    match c.to_ascii_uppercase() {
        'A' => [
            ".###.", "#...#", "#...#", "#####", "#...#", "#...#", "#...#",
        ],
        'B' => [
            "####.", "#...#", "#...#", "####.", "#...#", "#...#", "####.",
        ],
        'C' => [
            ".###.", "#...#", "#....", "#....", "#....", "#...#", ".###.",
        ],
        'D' => [
            "####.", "#...#", "#...#", "#...#", "#...#", "#...#", "####.",
        ],
        'E' => [
            "#####", "#....", "#....", "####.", "#....", "#....", "#####",
        ],
        'F' => [
            "#####", "#....", "#....", "####.", "#....", "#....", "#....",
        ],
        'G' => [
            ".###.", "#...#", "#....", "#.###", "#...#", "#...#", ".####",
        ],
        'H' => [
            "#...#", "#...#", "#...#", "#####", "#...#", "#...#", "#...#",
        ],
        'I' => [
            ".###.", "..#..", "..#..", "..#..", "..#..", "..#..", ".###.",
        ],
        'J' => [
            "..###", "...#.", "...#.", "...#.", "...#.", "#..#.", ".##..",
        ],
        'K' => [
            "#...#", "#..#.", "#.#..", "##...", "#.#..", "#..#.", "#...#",
        ],
        'L' => [
            "#....", "#....", "#....", "#....", "#....", "#....", "#####",
        ],
        'M' => [
            "#...#", "##.##", "#.#.#", "#.#.#", "#...#", "#...#", "#...#",
        ],
        'N' => [
            "#...#", "#...#", "##..#", "#.#.#", "#..##", "#...#", "#...#",
        ],
        'O' => [
            ".###.", "#...#", "#...#", "#...#", "#...#", "#...#", ".###.",
        ],
        'P' => [
            "####.", "#...#", "#...#", "####.", "#....", "#....", "#....",
        ],
        'Q' => [
            ".###.", "#...#", "#...#", "#...#", "#.#.#", "#..#.", ".##.#",
        ],
        'R' => [
            "####.", "#...#", "#...#", "####.", "#.#..", "#..#.", "#...#",
        ],
        'S' => [
            ".####", "#....", "#....", ".###.", "....#", "....#", "####.",
        ],
        'T' => [
            "#####", "..#..", "..#..", "..#..", "..#..", "..#..", "..#..",
        ],
        'U' => [
            "#...#", "#...#", "#...#", "#...#", "#...#", "#...#", ".###.",
        ],
        'V' => [
            "#...#", "#...#", "#...#", "#...#", "#...#", ".#.#.", "..#..",
        ],
        'W' => [
            "#...#", "#...#", "#...#", "#.#.#", "#.#.#", "#.#.#", ".#.#.",
        ],
        'X' => [
            "#...#", "#...#", ".#.#.", "..#..", ".#.#.", "#...#", "#...#",
        ],
        'Y' => [
            "#...#", "#...#", ".#.#.", "..#..", "..#..", "..#..", "..#..",
        ],
        'Z' => [
            "#####", "....#", "...#.", "..#..", ".#...", "#....", "#####",
        ],
        '0' => [
            ".###.", "#...#", "#..##", "#.#.#", "##..#", "#...#", ".###.",
        ],
        '1' => [
            "..#..", ".##..", "..#..", "..#..", "..#..", "..#..", ".###.",
        ],
        '2' => [
            ".###.", "#...#", "....#", "...#.", "..#..", ".#...", "#####",
        ],
        '3' => [
            "####.", "....#", "....#", ".###.", "....#", "....#", "####.",
        ],
        '4' => [
            "...#.", "..##.", ".#.#.", "#..#.", "#####", "...#.", "...#.",
        ],
        '5' => [
            "#####", "#....", "####.", "....#", "....#", "#...#", ".###.",
        ],
        '6' => [
            "..##.", ".#...", "#....", "####.", "#...#", "#...#", ".###.",
        ],
        '7' => [
            "#####", "....#", "...#.", "..#..", ".#...", ".#...", ".#...",
        ],
        '8' => [
            ".###.", "#...#", "#...#", ".###.", "#...#", "#...#", ".###.",
        ],
        '9' => [
            ".###.", "#...#", "#...#", ".####", "....#", "...#.", ".##..",
        ],
        ' ' => [
            ".....", ".....", ".....", ".....", ".....", ".....", ".....",
        ],
        '.' => [
            ".....", ".....", ".....", ".....", ".....", ".##..", ".##..",
        ],
        ',' => [
            ".....", ".....", ".....", ".....", ".##..", "..#..", ".#...",
        ],
        ':' => [
            ".....", ".##..", ".##..", ".....", ".##..", ".##..", ".....",
        ],
        ';' => [
            ".....", ".##..", ".##..", ".....", ".##..", "..#..", ".#...",
        ],
        '-' => [
            ".....", ".....", ".....", "#####", ".....", ".....", ".....",
        ],
        '_' => [
            ".....", ".....", ".....", ".....", ".....", ".....", "#####",
        ],
        '/' => [
            "....#", "....#", "...#.", "..#..", ".#...", "#....", "#....",
        ],
        '\\' => [
            "#....", "#....", ".#...", "..#..", "...#.", "....#", "....#",
        ],
        '(' => [
            "...#.", "..#..", ".#...", ".#...", ".#...", "..#..", "...#.",
        ],
        ')' => [
            ".#...", "..#..", "...#.", "...#.", "...#.", "..#..", ".#...",
        ],
        '[' => [
            ".###.", ".#...", ".#...", ".#...", ".#...", ".#...", ".###.",
        ],
        ']' => [
            ".###.", "...#.", "...#.", "...#.", "...#.", "...#.", ".###.",
        ],
        '@' => [
            ".###.", "#...#", "#.###", "#.#.#", "#.###", "#....", ".###.",
        ],
        '!' => [
            "..#..", "..#..", "..#..", "..#..", "..#..", ".....", "..#..",
        ],
        '#' => [
            ".#.#.", ".#.#.", "#####", ".#.#.", "#####", ".#.#.", ".#.#.",
        ],
        '=' => [
            ".....", ".....", "#####", ".....", "#####", ".....", ".....",
        ],
        '+' => [
            ".....", "..#..", "..#..", "#####", "..#..", "..#..", ".....",
        ],
        '*' => [
            ".....", "..#..", "#.#.#", ".###.", "#.#.#", "..#..", ".....",
        ],
        '\'' => [
            "..#..", "..#..", ".#...", ".....", ".....", ".....", ".....",
        ],
        '"' => [
            ".#.#.", ".#.#.", ".....", ".....", ".....", ".....", ".....",
        ],
        '<' => [
            "...#.", "..#..", ".#...", "#....", ".#...", "..#..", "...#.",
        ],
        '>' => [
            ".#...", "..#..", "...#.", "....#", "...#.", "..#..", ".#...",
        ],
        '|' => [
            "..#..", "..#..", "..#..", "..#..", "..#..", "..#..", "..#..",
        ],
        '~' => [
            ".....", ".....", ".#...", "#.#.#", "...#.", ".....", ".....",
        ],
        '$' => [
            "..#..", ".####", "#.#..", ".###.", "..#.#", "####.", "..#..",
        ],
        '%' => [
            "##..#", "##..#", "...#.", "..#..", ".#...", "#..##", "#..##",
        ],
        '&' => [
            ".##..", "#..#.", "#.#..", ".#...", "#.#.#", "#..#.", ".##.#",
        ],
        _ => [
            ".###.", "#...#", "....#", "...#.", "..#..", ".....", "..#..",
        ],
    }
}

async fn serve<S: AsyncRead + AsyncWrite + Unpin>(
    mut s: S,
    password: SecretString,
    title: String,
) -> io::Result<()> {
    s.write_all(b"RFB 003.008\n").await?;
    let mut v = [0u8; 12];
    s.read_exact(&mut v).await?;
    if parse_version(&v) != Some(8) {
        return Ok(());
    }
    // One security type: VNC authentication.
    s.write_all(&[1, 2]).await?;
    let mut t = [0u8; 1];
    s.read_exact(&mut t).await?;
    let mut challenge = [0u8; 16];
    rand::fill(&mut challenge);
    s.write_all(&challenge).await?;
    let mut response = [0u8; 16];
    s.read_exact(&mut response).await?;
    let expected = vnc_auth_response(password.expose_str().as_bytes(), &challenge);
    if t[0] != 2 || response != expected {
        let reason = b"Authentication failed";
        s.write_all(&1u32.to_be_bytes()).await?;
        s.write_all(&(reason.len() as u32).to_be_bytes()).await?;
        s.write_all(reason).await?;
        return Ok(());
    }
    s.write_all(&0u32.to_be_bytes()).await?;
    // ClientInit (shared flag), ServerInit.
    s.read_exact(&mut t).await?;
    let mut pf = PixelFormat::DEFAULT;
    let name = b"HostingOS demo";
    let mut init = Vec::new();
    init.extend_from_slice(&WIDTH.to_be_bytes());
    init.extend_from_slice(&HEIGHT.to_be_bytes());
    init.extend_from_slice(&pf.to_bytes());
    init.extend_from_slice(&(name.len() as u32).to_be_bytes());
    init.extend_from_slice(name);
    s.write_all(&init).await?;

    let mut screen = Screen::new(title);
    let mut waiting = false;
    let mut dirty = true;
    loop {
        let mut kind = [0u8; 1];
        if s.read_exact(&mut kind).await.is_err() {
            return Ok(());
        }
        match kind[0] {
            0 => {
                let mut b = [0u8; 19];
                s.read_exact(&mut b).await?;
                match PixelFormat::parse(&b[3..]) {
                    Some(p) => pf = p,
                    None => return Ok(()),
                }
                dirty = true;
            }
            2 => {
                let mut b = [0u8; 3];
                s.read_exact(&mut b).await?;
                let n = usize::from(u16::from_be_bytes([b[1], b[2]]));
                let mut skip = vec![0u8; n * 4];
                s.read_exact(&mut skip).await?;
            }
            3 => {
                let mut b = [0u8; 9];
                s.read_exact(&mut b).await?;
                let incremental = b[0] != 0;
                if !incremental {
                    dirty = true;
                }
                waiting = true;
            }
            4 => {
                let mut b = [0u8; 7];
                s.read_exact(&mut b).await?;
                if b[0] != 0 && screen.key(u32::from_be_bytes([b[3], b[4], b[5], b[6]])) {
                    dirty = true;
                }
            }
            5 => {
                let mut b = [0u8; 5];
                s.read_exact(&mut b).await?;
            }
            6 => {
                let mut b = [0u8; 7];
                s.read_exact(&mut b).await?;
                let len = u32::from_be_bytes([b[3], b[4], b[5], b[6]]);
                if len > MAX_CUT_TEXT {
                    return Ok(());
                }
                let mut skip = vec![0u8; len as usize];
                s.read_exact(&mut skip).await?;
            }
            _ => return Ok(()),
        }
        if waiting && dirty {
            let mut msg = vec![0u8, 0];
            msg.extend_from_slice(&1u16.to_be_bytes());
            msg.extend_from_slice(&screen.rect(&pf, 0, 0, WIDTH, HEIGHT));
            s.write_all(&msg).await?;
            waiting = false;
            dirty = false;
        }
    }
}
