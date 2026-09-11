//! agent-spy: watch a directory, showing each image created or modified there in a window.
//!
//! The window talks to the X server directly (x11rb), so it needs no system libraries and stays
//! cheap over SSH X forwarding: the displayed image lives in a server-side pixmap, so repainting
//! an uncovered window doesn't resend any pixels.

use std::borrow::Cow;
use std::collections::HashMap;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant, SystemTime};
use std::{env, fs, process, thread};

use chrono::{DateTime, Local};
use image::{ImageFormat, ImageReader, RgbaImage, imageops};
use notify::event::{AccessKind, AccessMode, ModifyKind};
use notify::{Event as FsEvent, EventKind, RecursiveMode, Watcher};
use x11rb::connection::Connection;
use x11rb::image::{BitsPerPixel, Image as XImage, ImageOrder, ScanlinePad};
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    AtomEnum, Char2b, ClientMessageEvent, ConnectionExt as _, CreateGCAux, CreateWindowAux,
    EventMask, Gcontext, Keysym, Mapping, Pixmap, PropMode, Screen, VisualClass, Window, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::{COPY_DEPTH_FROM_PARENT, COPY_FROM_PARENT};

const USAGE: &str = "\
Usage: agent-spy [OPTIONS] [DIR]

Watch DIR (default: current directory) and show each image created or modified in it.

Options:
  -e, --existing   also list the images already in DIR (least recently modified first)
  -r, --recursive  watch subdirectories too
  -h, --help       print this help

Keys:
  Left / Right     previous / next image
  Home / End       first / latest image
  Esc / q          quit
";

const BACKGROUND: u32 = 0x1e1e1e;
const TEXT_COLOR: u32 = 0xb0b0b0;

/// Core fonts to try, best first: the iso10646 one can show any file name, and every X server has
/// "fixed".
const FONTS: [&str; 3] = ["-misc-fixed-medium-r-normal--15-*-*-*-c-90-iso10646-1", "9x15", "fixed"];
/// Distance of the file name label from the window corner, and of its text from the label's edge.
const LABEL_MARGIN: i32 = 8;
const LABEL_PADDING: i32 = 4;

/// How long a new file must go without writes before we try to load it. Usually the writer
/// closing the file triggers the load sooner.
const SETTLE: Duration = Duration::from_millis(300);
/// Delay between attempts to decode a file that isn't completely written yet.
const RETRY: Duration = Duration::from_millis(500);
/// Stop retrying a file that fails to decode once it has been quiet this long.
const GIVE_UP: Duration = Duration::from_secs(10);

// From X11/keysymdef.h.
const XK_HOME: Keysym = 0xff50;
const XK_LEFT: Keysym = 0xff51;
const XK_RIGHT: Keysym = 0xff53;
const XK_END: Keysym = 0xff57;
const XK_ESCAPE: Keysym = 0xff1b;
const XK_Q: Keysym = 0x71;

x11rb::atom_manager! {
    Atoms: AtomsCookie {
        WM_PROTOCOLS,
        WM_DELETE_WINDOW,
        _NET_WM_NAME,
        UTF8_STRING,
        WAKE: b"_AGENT_SPY_WAKE",
    }
}

struct Args {
    dir: PathBuf,
    existing: bool,
    recursive: bool,
}

/// Sent from the loader thread to the UI.
enum Update {
    /// A file was created or modified, finished writing, and decoded successfully.
    Loaded { path: PathBuf, image: RgbaImage },
    Removed(PathBuf),
}

fn main() {
    if let Err(e) = run(parse_args()) {
        eprintln!("agent-spy: {e}");
        process::exit(1);
    }
}

fn parse_args() -> Args {
    let usage_error = |message: String| -> ! {
        eprint!("agent-spy: {message}\n\n{USAGE}");
        process::exit(2);
    };
    let mut args = Args { dir: PathBuf::from("."), existing: false, recursive: false };
    let mut dir_given = false;
    for arg in env::args().skip(1) {
        match arg.as_str() {
            "-e" | "--existing" => args.existing = true,
            "-r" | "--recursive" => args.recursive = true,
            "-h" | "--help" => {
                print!("{USAGE}");
                process::exit(0);
            }
            _ if arg.starts_with('-') => usage_error(format!("unknown option {arg}")),
            _ if dir_given => usage_error("only one directory can be watched".into()),
            _ => {
                args.dir = PathBuf::from(arg);
                dir_given = true;
            }
        }
    }
    args
}

fn run(args: Args) -> Result<(), Box<dyn Error>> {
    let dir = fs::canonicalize(&args.dir).map_err(|e| format!("{}: {e}", args.dir.display()))?;
    if !dir.is_dir() {
        return Err(format!("{} is not a directory", dir.display()).into());
    }

    let (conn, screen_num) = x11rb::connect(None).map_err(|e| {
        format!("cannot connect to X display {:?}: {e}", env::var("DISPLAY").unwrap_or_default())
    })?;
    let conn = Arc::new(conn);
    let mut ui = Ui::new(conn.clone(), screen_num, dir.clone())?;

    // Start watching before listing existing files so nothing slips through in between.
    let (fs_tx, fs_rx) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(fs_tx)?;
    let mode = if args.recursive { RecursiveMode::Recursive } else { RecursiveMode::NonRecursive };
    watcher.watch(&dir, mode)?;

    let (update_tx, update_rx) = mpsc::channel();
    let (window, wake) = (ui.window, ui.atoms.WAKE);
    thread::spawn(move || {
        run_loader(fs_rx, |update| {
            if update_tx.send(update).is_ok() {
                // Interrupt the UI's wait for X events so it picks up the update.
                let event = ClientMessageEvent::new(32, window, wake, [0u32; 5]);
                let _ = conn.send_event(false, window, EventMask::NO_EVENT, event);
                let _ = conn.flush();
            }
        })
    });

    if args.existing {
        ui.entries = existing_images(&dir, args.recursive);
        ui.index = ui.entries.len().saturating_sub(1);
    }
    ui.run(&update_rx)
}

fn is_image(path: &Path) -> bool {
    ImageFormat::from_path(path).is_ok_and(|format| format.reading_enabled())
}

fn load(path: &Path) -> Result<RgbaImage, Box<dyn Error>> {
    let image = ImageReader::open(path)?.with_guessed_format()?.decode()?;
    if image.width() == 0 || image.height() == 0 {
        return Err("image is empty".into());
    }
    Ok(image.into_rgba8())
}

/// Images already in `dir`, least recently modified first.
fn existing_images(dir: &Path, recursive: bool) -> Vec<PathBuf> {
    fn collect(dir: &Path, recursive: bool, found: &mut Vec<(SystemTime, PathBuf)>) {
        let Ok(entries) = fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let (path, Ok(meta)) = (entry.path(), entry.metadata()) else { continue };
            if meta.is_dir() {
                if recursive {
                    collect(&path, recursive, found);
                }
            } else if is_image(&path) {
                let time = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                found.push((time, path));
            }
        }
    }
    let mut found = Vec::new();
    collect(dir, recursive, &mut found);
    found.sort();
    found.into_iter().map(|(_, path)| path).collect()
}

/// A file we have seen activity on and will try to load.
struct Pending {
    last_activity: Instant,
    next_attempt: Instant,
}

/// Turns file system events into decoded images, waiting until files are completely written.
fn run_loader(events: Receiver<notify::Result<FsEvent>>, mut send: impl FnMut(Update)) {
    let mut pending: HashMap<PathBuf, Pending> = HashMap::new();
    loop {
        let received = match pending.values().map(|p| p.next_attempt).min() {
            None => events.recv().map_err(|_| RecvTimeoutError::Disconnected),
            Some(next) => events.recv_timeout(next.saturating_duration_since(Instant::now())),
        };
        match received {
            Ok(Ok(event)) => on_fs_event(event, &mut pending, &mut send),
            Ok(Err(e)) => eprintln!("agent-spy: watch error: {e}"),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }

        let now = Instant::now();
        pending.retain(|path, p| {
            if p.next_attempt > now {
                return true;
            }
            match load(path) {
                Ok(image) => {
                    send(Update::Loaded { path: path.clone(), image });
                    false
                }
                // Most likely still being written.
                Err(_) if now < p.last_activity + GIVE_UP => {
                    p.next_attempt = now + RETRY;
                    true
                }
                Err(e) => {
                    eprintln!("agent-spy: skipping {}: {e}", path.display());
                    false
                }
            }
        });
    }
}

fn on_fs_event(event: FsEvent, pending: &mut HashMap<PathBuf, Pending>, send: &mut impl FnMut(Update)) {
    let now = Instant::now();
    for path in event.paths {
        if !is_image(&path) {
            continue;
        }
        // When to try loading the file.
        let delay = match event.kind {
            EventKind::Create(_) | EventKind::Modify(ModifyKind::Data(_) | ModifyKind::Any) => SETTLE,
            // Moved into place, so it is already complete.
            EventKind::Modify(ModifyKind::Name(_)) if path.exists() => Duration::ZERO,
            EventKind::Modify(ModifyKind::Name(_)) | EventKind::Remove(_) => {
                pending.remove(&path);
                send(Update::Removed(path));
                continue;
            }
            // The writer is done. Closing without writing anything, as `touch` does, doesn't count.
            EventKind::Access(AccessKind::Close(AccessMode::Write)) if pending.contains_key(&path) => Duration::ZERO,
            _ => continue,
        };
        pending.insert(path, Pending { last_activity: now, next_attempt: now + delay });
    }
}

/// The image currently selected in the window.
struct Shown {
    path: PathBuf,
    image: Result<RgbaImage, String>,
    modified: Option<SystemTime>,
}

impl Shown {
    fn new(path: PathBuf, image: Result<RgbaImage, String>) -> Self {
        let modified = fs::metadata(&path).and_then(|meta| meta.modified()).ok();
        Self { path, image, modified }
    }
}

/// An image uploaded to the X server.
struct Picture {
    id: Pixmap,
    size: (u16, u16),
}

struct FontMetrics {
    char_width: i32,
    ascent: i32,
    descent: i32,
}

impl FontMetrics {
    fn width(&self, text: &[Char2b]) -> i32 {
        text.len() as i32 * self.char_width
    }
}

/// Maps keycodes to keysyms, following the keyboard layout.
struct Keymap {
    min_keycode: u8,
    keysyms_per_keycode: usize,
    keysyms: Vec<Keysym>,
}

impl Keymap {
    fn fetch(conn: &RustConnection) -> Result<Self, Box<dyn Error>> {
        let (min, max) = (conn.setup().min_keycode, conn.setup().max_keycode);
        let reply = conn.get_keyboard_mapping(min, max - min + 1)?.reply()?;
        Ok(Self {
            min_keycode: min,
            keysyms_per_keycode: reply.keysyms_per_keycode.into(),
            keysyms: reply.keysyms,
        })
    }

    /// The unshifted keysym of `keycode`.
    fn keysym(&self, keycode: u8) -> Keysym {
        keycode
            .checked_sub(self.min_keycode)
            .and_then(|i| self.keysyms.get(usize::from(i) * self.keysyms_per_keycode))
            .copied()
            .unwrap_or(0)
    }
}

struct Ui {
    conn: Arc<RustConnection>,
    atoms: Atoms,
    window: Window,
    gc: Gcontext,
    depth: u8,
    font: Option<FontMetrics>,
    keymap: Keymap,
    dir: PathBuf,
    /// Images, least recently created or modified first.
    entries: Vec<PathBuf>,
    /// The selected entry.
    index: usize,
    /// The selected entry, decoded.
    shown: Option<Shown>,
    /// `shown`, scaled to fit the window, on the X server.
    picture: Option<Picture>,
    size: (u16, u16),
    title: String,
    dirty: bool,
}

impl Ui {
    fn new(conn: Arc<RustConnection>, screen_num: usize, dir: PathBuf) -> Result<Self, Box<dyn Error>> {
        let screen = &conn.setup().roots[screen_num];
        check_visual(screen)?;
        let (root, depth) = (screen.root, screen.root_depth);
        let size = (1024.min(screen.width_in_pixels), 768.min(screen.height_in_pixels));

        let atoms = Atoms::new(&*conn)?.reply()?;
        let window = conn.generate_id()?;
        let events = EventMask::EXPOSURE | EventMask::STRUCTURE_NOTIFY | EventMask::KEY_PRESS;
        conn.create_window(
            COPY_DEPTH_FROM_PARENT,
            window,
            root,
            0,
            0,
            size.0,
            size.1,
            0,
            WindowClass::INPUT_OUTPUT,
            COPY_FROM_PARENT,
            &CreateWindowAux::new().background_pixel(BACKGROUND).event_mask(events),
        )?;
        conn.change_property32(PropMode::REPLACE, window, atoms.WM_PROTOCOLS, AtomEnum::ATOM, &[atoms.WM_DELETE_WINDOW])?;
        conn.change_property8(PropMode::REPLACE, window, AtomEnum::WM_CLASS, AtomEnum::STRING, b"agent-spy\0agent-spy\0")?;

        let font_id = conn.generate_id()?;
        let font = open_font(&conn, font_id);
        let gc = conn.generate_id()?;
        conn.create_gc(
            gc,
            window,
            &CreateGCAux::new()
                .foreground(TEXT_COLOR)
                .background(BACKGROUND)
                .graphics_exposures(0)
                .font(font.as_ref().map(|_| font_id)),
        )?;
        conn.map_window(window)?;
        conn.flush()?;

        Ok(Self {
            keymap: Keymap::fetch(&conn)?,
            conn,
            atoms,
            window,
            gc,
            depth,
            font,
            dir,
            entries: Vec::new(),
            index: 0,
            shown: None,
            picture: None,
            size,
            title: String::new(),
            dirty: true,
        })
    }

    fn run(&mut self, updates: &Receiver<Update>) -> Result<(), Box<dyn Error>> {
        loop {
            // Handle everything that is queued before rendering, so that e.g. a held arrow key
            // doesn't decode every image it skips over.
            let mut event = Some(self.conn.wait_for_event()?);
            while let Some(e) = event {
                if !self.handle(e)? {
                    return Ok(());
                }
                event = self.conn.poll_for_event()?;
            }
            for update in updates.try_iter() {
                self.apply(update);
            }
            if std::mem::take(&mut self.dirty) {
                self.render()?;
            }
            self.conn.flush()?;
        }
    }

    /// Returns false when the window should close.
    fn handle(&mut self, event: Event) -> Result<bool, Box<dyn Error>> {
        match event {
            Event::Expose(e) if e.count == 0 => self.dirty = true,
            Event::ConfigureNotify(e) if (e.width, e.height) != self.size => {
                self.size = (e.width, e.height);
                self.dirty = true;
            }
            Event::KeyPress(e) => match self.keymap.keysym(e.detail) {
                XK_LEFT => self.select(self.index.saturating_sub(1)),
                XK_RIGHT => self.select(self.index + 1),
                XK_HOME => self.select(0),
                XK_END => self.select(usize::MAX),
                XK_ESCAPE | XK_Q => return Ok(false),
                _ => {}
            },
            Event::MappingNotify(e) if e.request == Mapping::KEYBOARD => self.keymap = Keymap::fetch(&self.conn)?,
            Event::ClientMessage(e)
                if e.type_ == self.atoms.WM_PROTOCOLS && e.data.as_data32()[0] == self.atoms.WM_DELETE_WINDOW =>
            {
                return Ok(false);
            }
            Event::Error(e) => eprintln!("agent-spy: X11 error: {e:?}"),
            _ => {}
        }
        Ok(true)
    }

    fn select(&mut self, index: usize) {
        let index = index.min(self.entries.len().saturating_sub(1));
        if index != self.index {
            self.index = index;
            self.dirty = true;
        }
    }

    fn apply(&mut self, update: Update) {
        match update {
            Update::Loaded { path, image } => {
                // A modified file counts as new, so it moves to the end.
                self.entries.retain(|p| *p != path);
                self.entries.push(path.clone());
                self.index = self.entries.len() - 1;
                self.set_shown(Some(Shown::new(path, Ok(image))));
            }
            Update::Removed(path) => {
                if let Some(i) = self.entries.iter().position(|p| *p == path) {
                    self.entries.remove(i);
                    if i < self.index || self.index == self.entries.len() {
                        self.index = self.index.saturating_sub(1);
                    }
                }
            }
        }
        self.dirty = true;
    }

    fn set_shown(&mut self, shown: Option<Shown>) {
        self.shown = shown;
        if let Some(picture) = self.picture.take() {
            let _ = self.conn.free_pixmap(picture.id);
        }
    }

    fn shown_image(&self) -> Option<&RgbaImage> {
        self.shown.as_ref().and_then(|s| s.image.as_ref().ok())
    }

    fn render(&mut self) -> Result<(), Box<dyn Error>> {
        let selected = self.entries.get(self.index);
        if self.shown.as_ref().map(|s| &s.path) != selected {
            let shown = selected.map(|path| Shown::new(path.clone(), load(path).map_err(|e| e.to_string())));
            self.set_shown(shown);
        }

        let target = self.shown_image().map(|image| fit(image.dimensions(), self.size));
        if self.picture.as_ref().map(|p| p.size) != target {
            if let Some(old) = self.picture.take() {
                self.conn.free_pixmap(old.id)?;
            }
            if let (Some(image), Some(size)) = (self.shown_image(), target) {
                self.picture = Some(self.upload(image, size)?);
            }
        }

        self.paint()?;
        self.update_title()
    }

    fn upload(&self, image: &RgbaImage, (width, height): (u16, u16)) -> Result<Picture, Box<dyn Error>> {
        let scaled = if image.dimensions() == (width.into(), height.into()) {
            Cow::Borrowed(image)
        } else {
            Cow::Owned(imageops::thumbnail(image, width.into(), height.into()))
        };
        let mut data = Vec::with_capacity(scaled.as_raw().len());
        for pixel in scaled.pixels() {
            data.extend_from_slice(&blend(pixel.0, BACKGROUND).to_le_bytes());
        }
        let ximage = XImage::new(
            width,
            height,
            ScanlinePad::Pad32,
            self.depth,
            BitsPerPixel::B32,
            ImageOrder::LsbFirst,
            Cow::Owned(data),
        )?;
        let id = self.conn.generate_id()?;
        self.conn.create_pixmap(self.depth, id, self.window, width, height)?;
        ximage.put(&*self.conn, id, self.gc, 0, 0)?;
        Ok(Picture { id, size: (width, height) })
    }

    fn paint(&self) -> Result<(), Box<dyn Error>> {
        match &self.picture {
            Some(picture) => self.paint_picture(picture)?,
            None => {
                self.conn.clear_area(false, self.window, 0, 0, 0, 0)?;
                match &self.shown {
                    None => self.draw_centered(&format!("Waiting for images in {}", self.dir.display()))?,
                    Some(Shown { image: Err(e), .. }) => self.draw_centered(&format!("Cannot display: {e}"))?,
                    Some(_) => {}
                }
            }
        }
        if let Some(shown) = &self.shown {
            self.draw_label(&self.label(shown))?;
        }
        Ok(())
    }

    fn paint_picture(&self, picture: &Picture) -> Result<(), Box<dyn Error>> {
        // Center the image and clear the borders around it.
        let (width, height) = self.size;
        let (w, h) = picture.size;
        let (x, y) = (width.saturating_sub(w) / 2, height.saturating_sub(h) / 2);
        let borders = [
            (0, 0, width, y),
            (0, y + h, width, height.saturating_sub(y + h)),
            (0, y, x, h),
            (x + w, y, width.saturating_sub(x + w), h),
        ];
        for (bx, by, bw, bh) in borders {
            // A zero size means "up to the edge of the window" to ClearArea.
            if bw > 0 && bh > 0 {
                self.conn.clear_area(false, self.window, bx as i16, by as i16, bw, bh)?;
            }
        }
        self.conn.copy_area(picture.id, self.window, self.gc, 0, 0, x as i16, y as i16, w, h)?;
        Ok(())
    }

    /// Draws `text` in the middle of the window.
    fn draw_centered(&self, text: &str) -> Result<(), Box<dyn Error>> {
        let Some(font) = &self.font else { return Ok(()) };
        let chars = to_char2b(text);
        let x = ((i32::from(self.size.0) - font.width(&chars)) / 2).max(LABEL_MARGIN);
        let y = (i32::from(self.size.1) + font.ascent - font.descent) / 2;
        self.conn.image_text16(self.window, self.gc, x as i16, y as i16, &chars)?;
        Ok(())
    }

    /// Draws `text` in the top left corner of the window, over the image, on a box of the
    /// background color.
    fn draw_label(&self, text: &str) -> Result<(), Box<dyn Error>> {
        let Some(font) = &self.font else { return Ok(()) };
        let chars = to_char2b(text);
        let width = font.width(&chars) + 2 * LABEL_PADDING;
        let height = font.ascent + font.descent + 2 * LABEL_PADDING;
        let size = |n: i32| n.clamp(1, u16::MAX.into()) as u16;
        let margin = LABEL_MARGIN as i16;
        self.conn.clear_area(false, self.window, margin, margin, size(width), size(height))?;
        let (x, y) = (LABEL_MARGIN + LABEL_PADDING, LABEL_MARGIN + LABEL_PADDING + font.ascent);
        self.conn.image_text16(self.window, self.gc, x as i16, y as i16, &chars)?;
        Ok(())
    }

    /// A file's name as shown to the user: relative to the watched directory.
    fn display_name(&self, path: &Path) -> String {
        path.strip_prefix(&self.dir).unwrap_or(path).display().to_string()
    }

    /// The overlay text for `shown`: the time it was last written, its position, and its name.
    fn label(&self, shown: &Shown) -> String {
        let name = self.display_name(&shown.path);
        // Pad the position to the width of the total so the name doesn't move while navigating.
        let total = self.entries.len().to_string();
        let counter = format!("{:>width$}/{total}", self.index + 1, width = total.len());
        match shown.modified {
            Some(time) => format!("{}  {counter}  {name}", DateTime::<Local>::from(time).format("%H:%M:%S")),
            None => format!("{counter}  {name}"),
        }
    }

    fn update_title(&mut self) -> Result<(), Box<dyn Error>> {
        let title = match &self.shown {
            None => format!("agent-spy — waiting for images in {}", self.dir.display()),
            Some(shown) => {
                let name = self.display_name(&shown.path);
                let detail = match &shown.image {
                    Ok(image) => format!("{}×{}", image.width(), image.height()),
                    Err(_) => "cannot display".into(),
                };
                format!("[{}/{}] {name} — {detail} — agent-spy", self.index + 1, self.entries.len())
            }
        };
        if title != self.title {
            let (conn, window) = (&self.conn, self.window);
            conn.change_property8(PropMode::REPLACE, window, self.atoms._NET_WM_NAME, self.atoms.UTF8_STRING, title.as_bytes())?;
            conn.change_property8(PropMode::REPLACE, window, AtomEnum::WM_NAME, AtomEnum::STRING, title.as_bytes())?;
            self.title = title;
        }
        Ok(())
    }
}

/// Pixels are sent as 0xRRGGBB, which needs the usual 24-bit TrueColor visual.
fn check_visual(screen: &Screen) -> Result<(), Box<dyn Error>> {
    let visual = screen.allowed_depths.iter().flat_map(|d| &d.visuals).find(|v| v.visual_id == screen.root_visual);
    match visual {
        Some(v)
            if screen.root_depth == 24
                && v.class == VisualClass::TRUE_COLOR
                && (v.red_mask, v.green_mask, v.blue_mask) == (0xff0000, 0xff00, 0xff) =>
        {
            Ok(())
        }
        _ => Err("unsupported X display: need a 24-bit TrueColor visual".into()),
    }
}

/// Opens the first of `FONTS` that the server has. They are monospaced, so the width of one
/// character is enough to lay out text.
fn open_font(conn: &RustConnection, id: u32) -> Option<FontMetrics> {
    FONTS.iter().find(|name| conn.open_font(id, name.as_bytes()).is_ok_and(|cookie| cookie.check().is_ok()))?;
    let extents = conn.query_text_extents(id, &to_char2b("M")).ok()?.reply().ok()?;
    Some(FontMetrics {
        char_width: extents.overall_width,
        ascent: extents.font_ascent.into(),
        descent: extents.font_descent.into(),
    })
}

/// Converts `text` for ImageText16, which draws at most 255 characters. Characters the font lacks,
/// like non-Latin-1 ones in a Latin-1 font, show as its default glyph.
fn to_char2b(text: &str) -> Vec<Char2b> {
    text.chars()
        .take(255)
        .map(|c| {
            let [byte1, byte2] = u16::try_from(u32::from(c)).unwrap_or(0xfffd).to_be_bytes();
            Char2b { byte1, byte2 }
        })
        .collect()
}

/// The size of an image of `size` shrunk to fit in `area`, keeping its aspect ratio. Images are
/// never enlarged.
fn fit((width, height): (u32, u32), area: (u16, u16)) -> (u16, u16) {
    let (area_w, area_h) = (f64::from(area.0.max(1)), f64::from(area.1.max(1)));
    let scale = (area_w / f64::from(width)).min(area_h / f64::from(height)).min(1.0);
    let scaled = |n: u32, max: f64| (f64::from(n) * scale).round().clamp(1.0, max) as u16;
    (scaled(width, area_w), scaled(height, area_h))
}

/// Composites an RGBA pixel over `background`, giving 0xRRGGBB.
fn blend([r, g, b, a]: [u8; 4], background: u32) -> u32 {
    let a = u32::from(a);
    let mix = |c: u8, shift: u32| (u32::from(c) * a + ((background >> shift) & 0xff) * (255 - a)) / 255;
    (mix(r, 16) << 16) | (mix(g, 8) << 8) | mix(b, 0)
}
