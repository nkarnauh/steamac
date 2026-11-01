//! The focused game's cursor, for the DXMT path's cursor overlay.
//!
//! On DXMT the game's frame bypasses gamescope and goes straight to the Mac, so the host covers
//! the guest compositor's cursor. `GAMESCOPE_CURSOR_VISIBLE_FEEDBACK` follows gamescope's own
//! auto-hide/lag, not the game's request. The X cursor set by Wine is the game's state: it is
//! blank (1x1, transparent) as soon as DayZ leaves inventory.
//!
//! The host draws the cursor itself, so the guest sends the XFixes cursor bitmap (BGRA, the
//! native little-endian bytes of the X cursor image) and its position in the focused game's X
//! window. The guest scanout cannot be used for this: while a game is remoted it may show a
//! white Steam / loading surface, which a black-keyed overlay would cover the game with.
//!
//! Messages (on change): `cursor 0|1`, `cursor-image <w> <h> <xhot> <yhot> <base64 BGRA>`,
//! `cursor-pos <x> <y> <win_w> <win_h>`.

use std::time::{Duration, Instant};
use x11rb::connection::Connection;
use x11rb::protocol::xfixes::ConnectionExt as _;
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt, Window};
use x11rb::rust_connection::RustConnection;

use crate::codec;
use crate::port::Port;

struct Conn {
    display: String,
    conn: RustConnection,
    root: Window,
    active_atom: u32,
    xfixes: bool,
    width: u16,
    height: u16,
}

/// X cursor bitmaps are small; the host rejects anything larger.
const MAX_CURSOR_DIMENSION: u16 = 256;

#[derive(PartialEq)]
struct Image {
    width: u16,
    height: u16,
    xhot: u16,
    yhot: u16,
    pixels: Vec<u32>,
}

#[derive(PartialEq, Clone, Copy)]
struct Pos {
    x: i16,
    y: i16,
    width: u16,
    height: u16,
}

#[derive(PartialEq, Clone, Copy)]
struct Target {
    conn: usize,
    window: Window,
    width: u16,
    height: u16,
}

pub struct Cursor {
    conns: Vec<Conn>,
    next_scan: Instant,
    next_poll: Instant,
    next_image: Instant,
    next_select: Instant,
    target: Option<Target>,
    last_visible: Option<bool>,
    last_image: Option<Image>,
    last_pos: Option<Pos>,
}

impl Cursor {
    pub fn new() -> Cursor {
        Cursor {
            conns: Vec::new(),
            next_scan: Instant::now(),
            next_poll: Instant::now(),
            next_image: Instant::now(),
            next_select: Instant::now(),
            target: None,
            last_visible: None,
            last_image: None,
            last_pos: None,
        }
    }

    /// Keep the main loop polling while a game is focused: an Xwayland display may appear or
    /// disappear after the agent starts, and the next scan then reconnects it.
    pub fn active(&self) -> bool {
        true
    }

    fn drop_conn(&mut self, i: usize) {
        self.conns.remove(i);
        self.target = None;
        self.last_pos = None;
        self.next_scan = Instant::now();
        self.next_select = Instant::now();
    }

    fn connect(&mut self) {
        let Ok(dir) = std::fs::read_dir("/tmp/.X11-unix") else { return };
        for e in dir.flatten() {
            let name = e.file_name();
            let Some(n) = name.to_str().and_then(|s| s.strip_prefix('X')) else { continue };
            if n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
                continue;
            }
            let display = format!(":{n}");
            if self.conns.iter().any(|c| c.display == display) {
                continue;
            }
            let Ok((conn, screen)) = RustConnection::connect(Some(&display)) else { continue };
            let Some(screen) = conn.setup().roots.get(screen) else { continue };
            let root = screen.root;
            let width = screen.width_in_pixels;
            let height = screen.height_in_pixels;
            let Ok(active_atom) = conn.intern_atom(false, b"_NET_ACTIVE_WINDOW") else { continue };
            let Ok(active_atom) = active_atom.reply() else { continue };
            let xfixes = conn.xfixes_query_version(5, 0).ok().and_then(|c| c.reply().ok()).is_some();
            self.conns.push(Conn {
                display,
                conn,
                root,
                active_atom: active_atom.atom,
                xfixes,
                width,
                height,
            });
            self.target = None;
        }
    }

    /// The active window of an Xwayland display.
    fn active_window(c: &Conn) -> Option<Window> {
        c.conn
            .get_property(false, c.root, c.active_atom, AtomEnum::WINDOW, 0, 1)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .and_then(|r| r.value32().and_then(|mut v| v.next()))
            .filter(|&w| w != 0)
    }

    /// The active window if it belongs to a game (WM_CLASS starts with `steam_app_`).
    fn active_game_window(c: &Conn) -> Option<Window> {
        let win = Self::active_window(c)?;
        let class: Vec<u8> = c
            .conn
            .get_property(false, win, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 1024)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .map(|r| r.value)
            .unwrap_or_default();
        if class.windows(10).any(|w| w == b"steam_app_") {
            Some(win)
        } else {
            None
        }
    }

    fn target_for(&self, i: usize, window: Window) -> Target {
        let c = &self.conns[i];
        let (width, height) = if window == c.root {
            (c.width, c.height)
        } else {
            c.conn
                .get_geometry(window)
                .ok()
                .and_then(|cookie| cookie.reply().ok())
                .map(|g| (g.width, g.height))
                .filter(|&(w, h)| w > 0 && h > 0)
                .unwrap_or((c.width, c.height))
        };
        Target { conn: i, window, width, height }
    }

    /// The game window to map cursor positions into. Its client pixels are what the remoted
    /// game layer presents; the X root can be larger when a title runs windowed.
    fn select_target(&self) -> Option<Target> {
        if let Some((i, window)) = self
            .conns
            .iter()
            .enumerate()
            .filter(|(_, c)| c.xfixes)
            .find_map(|(i, c)| Self::active_game_window(c).map(|w| (i, w)))
        {
            return Some(self.target_for(i, window));
        }
        if self.conns.len() == 1 {
            let c = &self.conns[0];
            return c.xfixes.then(|| self.target_for(0, c.root));
        }
        let i = self.conns.iter().position(|c| c.xfixes)?;
        Some(self.target_for(i, self.conns[i].root))
    }

    pub fn pump(&mut self, port: &mut Port) {
        let now = Instant::now();
        if now >= self.next_scan {
            self.next_scan = now + Duration::from_secs(5);
            self.connect();
        }
        if now < self.next_poll {
            return;
        }
        self.next_poll = now + Duration::from_millis(16);   // follow the cursor at ~60 Hz
        if now >= self.next_select {
            self.next_select = now + Duration::from_millis(250);
            let target = self.select_target();
            if target != self.target {
                self.last_pos = None;
            }
            self.target = target;
        }
        let Some(target) = self.target.filter(|t| t.conn < self.conns.len()) else { return };

        // Position is a cheap round-trip, needed every poll for smooth movement. Query the
        // game window, not the root: `win_x`/`win_y` are client pixels whatever the title's
        // window size and position are.
        let reply = {
            let c = &self.conns[target.conn];
            if !c.xfixes {
                return;
            }
            c.conn.query_pointer(target.window).ok().and_then(|cookie| cookie.reply().ok())
        };
        let Some(reply) = reply else {
            self.drop_conn(target.conn);
            return;
        };
        let pos = Pos { x: reply.win_x, y: reply.win_y, width: target.width, height: target.height };

        // The bitmap changes rarely; fetching it 20 times a second is enough for visibility and shape.
        if now >= self.next_image {
            self.next_image = now + Duration::from_millis(50);
            let reply = {
                let c = &self.conns[target.conn];
                c.conn.xfixes_get_cursor_image().ok().and_then(|cookie| cookie.reply().ok())
            };
            let Some(img) = reply else {
                self.drop_conn(target.conn);
                return;
            };
            let fits = img.width <= MAX_CURSOR_DIMENSION && img.height <= MAX_CURSOR_DIMENSION;
            let visible = fits
                && img.width > 1
                && img.height > 1
                && img.cursor_image.iter().any(|&p| (p >> 24) & 0xff != 0);
            if self.last_visible != Some(visible) {
                self.last_visible = Some(visible);
                port.send_quiet(&format!("cursor {}", if visible { 1 } else { 0 }));
            }
            if visible {
                let image = Image {
                    width: img.width,
                    height: img.height,
                    xhot: img.xhot,
                    yhot: img.yhot,
                    pixels: img.cursor_image,
                };
                if self.last_image.as_ref() != Some(&image) {
                    // X cursor pixels are ARGB; on the guest's little-endian CPU their bytes are BGRA,
                    // exactly what a premultiplied-first CGImage wants. Alpha is premultiplied.
                    let mut bytes = Vec::with_capacity(image.pixels.len() * 4);
                    for pixel in &image.pixels {
                        bytes.extend_from_slice(&pixel.to_ne_bytes());
                    }
                    port.send_quiet(&format!(
                        "cursor-image {} {} {} {} {}",
                        image.width,
                        image.height,
                        image.xhot,
                        image.yhot,
                        codec::base64(&bytes),
                    ));
                    self.last_image = Some(image);
                }
            }
        }

        if self.last_visible == Some(true) && self.last_pos != Some(pos) {
            self.last_pos = Some(pos);
            port.send_quiet(&format!(
                "cursor-pos {} {} {} {}",
                pos.x, pos.y, pos.width, pos.height
            ));
        }
    }
}
