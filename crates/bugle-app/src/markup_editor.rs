//! Markup editor logic: what a click, drag or key does to the list of shapes.
//!
//! Nothing here touches AppKit. [`Editor::handle`] takes the window's
//! [`EditorEvent`]s and answers with [`Effects`] telling the glue in
//! `editor_window.rs` what to refresh. Finished shapes are drawn by the same
//! renderer `bugle annotate` uses, so the saved file matches the preview;
//! only the shape under the pointer is drawn by the window, from
//! [`Editor::overlay`].
//!
//! Shapes and the crop are kept in pixels of the original image, whatever
//! is cropped away or added around it; [`Editor::document`] translates them
//! for the renderer, and the window maps its input back with
//! [`Annotation::source_origin`].

use bugle_core::{Point, Rect};
use bugle_markup::{limits, text_lines, Annotation, ArrowStyle, Background, Canvas, Color, RedactStyle, Shape, Style};
use bugle_platform::{EditorCommand, EditorEvent, EditorMenu, EditorMenuItem, EditorPrim, EditorState, EditorTool};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Select,
    Arrow,
    Rectangle,
    Ellipse,
    Line,
    Pen,
    Text,
    Highlight,
    Counter,
    Redact,
    Spotlight,
    Crop,
}

impl Tool {
    /// Toolbar order.
    pub const ALL: [Tool; 12] = [
        Tool::Select,
        Tool::Arrow,
        Tool::Rectangle,
        Tool::Ellipse,
        Tool::Line,
        Tool::Pen,
        Tool::Text,
        Tool::Highlight,
        Tool::Counter,
        Tool::Redact,
        Tool::Spotlight,
        Tool::Crop,
    ];

    /// The key that picks the tool.
    pub fn key(self) -> char {
        match self {
            Tool::Select => 'v',
            Tool::Arrow => 'a',
            Tool::Rectangle => 'r',
            Tool::Ellipse => 'o',
            Tool::Line => 'l',
            Tool::Pen => 'p',
            Tool::Text => 't',
            Tool::Highlight => 'h',
            Tool::Counter => 'n',
            Tool::Redact => 'b',
            Tool::Spotlight => 's',
            Tool::Crop => 'c',
        }
    }

    /// Tooltip and SF Symbol for the toolbar.
    pub fn button(self) -> EditorTool {
        let (label, symbol) = match self {
            Tool::Select => ("Select and move (V)", "cursorarrow"),
            Tool::Arrow => ("Arrow (A)", "arrow.up.right"),
            Tool::Rectangle => ("Rectangle (R)", "rectangle"),
            Tool::Ellipse => ("Ellipse (O)", "circle"),
            Tool::Line => ("Line (L)", "line.diagonal"),
            Tool::Pen => ("Pen (P)", "scribble"),
            Tool::Text => ("Text (T)", "textformat"),
            Tool::Highlight => ("Highlight (H)", "highlighter"),
            Tool::Counter => ("Numbered step (N)", "1.circle"),
            Tool::Redact => ("Redact (B)", "eye.slash"),
            Tool::Spotlight => ("Spotlight (S)", "rectangle.dashed"),
            Tool::Crop => ("Crop (C); Delete removes the crop", "crop"),
        };
        EditorTool { label, symbol }
    }

    fn index(self) -> usize {
        Self::ALL.iter().position(|t| *t == self).unwrap_or(0)
    }
}

/// Toolbar swatches.
pub const PALETTE: [[u8; 4]; 8] = [
    [0xFF, 0x3B, 0x30, 0xFF],
    [0xFF, 0x95, 0x00, 0xFF],
    [0xFF, 0xE6, 0x00, 0xFF],
    [0x34, 0xC7, 0x59, 0xFF],
    [0x00, 0x7A, 0xFF, 0xFF],
    [0xAF, 0x52, 0xDE, 0xFF],
    [0x00, 0x00, 0x00, 0xFF],
    [0xFF, 0xFF, 0xFF, 0xFF],
];

/// Toolbar size labels; [`STROKES`] and [`FONTS`] are what they stand for.
pub const SIZES: [&str; 3] = ["S", "M", "L"];
/// Stroke widths in image pixels on a small image (multiplied by the unit).
const STROKES: [f64; 3] = [3.0, 5.0, 9.0];
/// Font sizes, likewise.
const FONTS: [f64; 3] = [18.0, 28.0, 44.0];
const HIGHLIGHT_ALPHA: u8 = 0x80;
/// Smallest crop, in image pixels.
const MIN_CROP: f64 = 8.0;

/// The toolbar menus, by index.
pub const MENUS: usize = 2;
pub(crate) const MENU_STYLE: usize = 0;
pub(crate) const MENU_CANVAS: usize = 1;
const ARROW_STYLES: [(ArrowStyle, &str); 4] = [
    (ArrowStyle::Standard, "Standard"),
    (ArrowStyle::Double, "Double-headed"),
    (ArrowStyle::Slim, "Slim"),
    (ArrowStyle::Fancy, "Tapered"),
];
const REDACT_STYLES: [(RedactStyle, &str); 4] = [
    (RedactStyle::Pixelate, "Pixelate"),
    (RedactStyle::Blur, "Blur"),
    (RedactStyle::BlackOut, "Black out"),
    (RedactStyle::SecureBlur, "Secure blur"),
];
/// Canvas padding choices: label and pixels on a small image (multiplied by
/// the unit, like strokes).
const PADDINGS: [(&str, f64); 4] = [("None", 0.0), ("Small", 32.0), ("Medium", 64.0), ("Large", 112.0)];
/// Canvas backgrounds: label and the renderer's background name.
const BACKGROUNDS: [(&str, &str); 10] = [
    ("Transparent", "none"),
    ("White", "#FFFFFF"),
    ("Sunset", "sunset"),
    ("Ocean", "ocean"),
    ("Forest", "forest"),
    ("Candy", "candy"),
    ("Peach", "peach"),
    ("Mint", "mint"),
    ("Midnight", "midnight"),
    ("Graphite", "mono"),
];
/// Canvas menu layout: a heading, the paddings, a separator, a heading,
/// the backgrounds.
const CANVAS_PADDING_ITEM: usize = 1;
pub(crate) const CANVAS_BACKGROUND_ITEM: usize = CANVAS_PADDING_ITEM + PADDINGS.len() + 2;
/// Padding picked when a background is chosen with no padding.
const DEFAULT_PADDING: usize = 2;
const UNDO_LIMIT: usize = 200;
/// Points kept for one pen stroke; the rest of a very long drag is dropped.
const MAX_STROKE_POINTS: usize = 5_000;

/// What the glue should do after an event.
#[derive(Debug, Default, PartialEq)]
pub struct Effects {
    /// The shapes baked into the image changed: render [`Editor::document`].
    pub render: bool,
    /// Redraw [`Editor::overlay`].
    pub overlay: bool,
    /// Refresh the toolbar from [`Editor::state`].
    pub state: bool,
    /// Show the text field.
    pub begin_text: Option<TextRequest>,
    pub request: Option<Request>,
    /// Something was refused; tell the user why.
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TextRequest {
    pub at: Point,
    pub font_px: f64,
    pub initial: String,
    pub color: [u8; 4],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request {
    Save,
    Copy,
}

/// The canvas around the image, as picked in the canvas menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Frame {
    /// Index into [`PADDINGS`]; 0 is no canvas.
    padding: usize,
    /// Index into [`BACKGROUNDS`].
    background: usize,
}

impl Default for Frame {
    fn default() -> Self {
        Self { padding: 0, background: 2 }
    }
}

/// Everything an undo step restores.
#[derive(Debug, Clone, PartialEq, Default)]
struct Snapshot {
    shapes: Vec<Shape>,
    crop: Option<Rect>,
    frame: Frame,
}

/// What the style menu is about right now.
#[derive(Debug, Clone, Copy, PartialEq)]
enum StyleTarget {
    Arrow(ArrowStyle),
    Redact(RedactStyle),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum CropDrag {
    New,
    Move,
    /// Dragging a corner, clockwise from top-left.
    Corner(usize),
}

#[derive(Debug, Clone, PartialEq)]
enum Drag {
    /// Dragging out a new shape with the current tool.
    Draw { start: Point, current: Point, points: Vec<Point> },
    /// Moving the selected shape; `original` is what it was at mouse down.
    Move { index: usize, start: Point, original: Shape, moved: bool },
    /// Dragging one of the selected shape's handles.
    Resize { index: usize, handle: usize, start: Point, original: Shape, moved: bool },
    /// Drawing, moving or resizing the crop; `original` is the crop at mouse down.
    Crop { mode: CropDrag, start: Point, original: Option<Rect>, moved: bool },
}

/// A label being typed in the window's text field.
#[derive(Debug, Clone, PartialEq)]
struct TextEdit {
    at: Point,
    /// The text shape being edited, when it is not a new one.
    replacing: Option<usize>,
}

pub struct Editor {
    /// Image-pixel multiplier for strokes and fonts, so they look the same on
    /// a Retina capture as on a small one.
    unit: f64,
    /// The edited image's size in pixels.
    image: (u32, u32),
    shapes: Vec<Shape>,
    /// Whole pixels inside the image; `None` keeps all of it.
    crop: Option<Rect>,
    frame: Frame,
    saved: Snapshot,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    tool: Tool,
    color: usize,
    size: usize,
    /// Style for new arrows and redactions.
    arrow: ArrowStyle,
    redact: RedactStyle,
    selected: Option<usize>,
    drag: Option<Drag>,
    text: Option<TextEdit>,
    /// A shape left out of [`Editor::document`] because the window is
    /// drawing it instead (it is being dragged or retyped).
    hidden: Option<usize>,
    /// Image pixels per screen point at the window's zoom.
    px_per_pt: f64,
}

impl Editor {
    pub fn new(image_size: (u32, u32)) -> Self {
        let long = image_size.0.max(image_size.1) as f64;
        Self {
            unit: (long / 1400.0).clamp(1.0, 3.0),
            image: image_size,
            shapes: Vec::new(),
            crop: None,
            frame: Frame::default(),
            saved: Snapshot::default(),
            undo: Vec::new(),
            redo: Vec::new(),
            tool: Tool::Arrow,
            color: 0,
            size: 1,
            arrow: ArrowStyle::Standard,
            redact: RedactStyle::Pixelate,
            selected: None,
            drag: None,
            text: None,
            hidden: None,
            px_per_pt: 2.0,
        }
    }

    /// The document for the live preview: everything except a shape the
    /// window is drawing itself right now. With the crop tool picked the
    /// whole image shows, without crop or canvas, so the crop can be
    /// changed; the window maps its input with [`Annotation::source_origin`].
    pub fn document(&self) -> Annotation {
        let shapes = self.shapes.iter().enumerate().filter(|(i, _)| Some(*i) != self.hidden).map(|(_, s)| s.clone()).collect();
        if self.tool == Tool::Crop {
            return Annotation { shapes, ..Default::default() };
        }
        self.compose(shapes)
    }

    /// Every shape with the crop and canvas, for saving and copying.
    pub fn final_document(&self) -> Annotation {
        self.compose(self.shapes.clone())
    }

    /// `shapes` (in original-image pixels) as a document for the renderer,
    /// whose shape coordinates are relative to the crop.
    fn compose(&self, mut shapes: Vec<Shape>) -> Annotation {
        if let Some(c) = self.crop {
            shapes.iter_mut().for_each(|s| s.translate(-c.x, -c.y));
        }
        Annotation { shapes, crop: self.crop, canvas: self.canvas(), ..Default::default() }
    }

    fn canvas(&self) -> Option<Canvas> {
        self.canvas_for(self.frame)
    }

    fn canvas_for(&self, frame: Frame) -> Option<Canvas> {
        let pad = PADDINGS[frame.padding.min(PADDINGS.len() - 1)].1 * self.unit;
        if pad <= 0.0 {
            return None;
        }
        let background = BACKGROUNDS[frame.background.min(BACKGROUNDS.len() - 1)].1;
        Some(Canvas {
            padding: (pad.round() as u32).min(limits::MAX_CANVAS_INSET),
            background: Background::Named(background.into()),
            corner_radius: (10.0 * self.unit).round() as u32,
            shadow: true,
            border: None,
            border_width: 0,
        })
    }

    /// Nothing drawn, cropped or framed: there is no copy worth saving.
    pub fn is_empty(&self) -> bool {
        self.shapes.is_empty() && self.crop.is_none() && self.canvas().is_none()
    }

    /// With no padding there is no canvas, whatever background was picked
    /// before: such frames compare equal, so dirty agrees with empty.
    fn snapshot(&self) -> Snapshot {
        let frame = if self.frame.padding == 0 { Frame::default() } else { self.frame };
        Snapshot { shapes: self.shapes.clone(), crop: self.crop, frame }
    }

    fn restore(&mut self, s: Snapshot) -> Snapshot {
        let now = self.snapshot();
        self.shapes = s.shapes;
        self.crop = s.crop;
        self.frame = s.frame;
        now
    }

    pub fn is_dirty(&self) -> bool {
        self.snapshot() != self.saved
    }

    pub fn state(&self) -> EditorState {
        EditorState {
            tool: self.tool.index(),
            color: self.color,
            width: self.size,
            can_undo: !self.undo.is_empty(),
            can_redo: !self.redo.is_empty(),
            dirty: self.is_dirty(),
        }
    }

    fn color(&self) -> Color {
        Color(PALETTE[self.color.min(PALETTE.len() - 1)])
    }

    fn stroke(&self) -> f32 {
        (STROKES[self.size.min(2)] * self.unit) as f32
    }

    fn font(&self) -> f32 {
        (FONTS[self.size.min(2)] * self.unit) as f32
    }

    fn style(&self) -> Style {
        Style { color: Some(self.color()), stroke_width: Some(self.stroke()), ..Default::default() }
    }

    /// The style menu applies to the selected arrow or redaction, or to the
    /// next one drawn.
    fn style_target(&self) -> Option<StyleTarget> {
        match (self.tool, self.selected.and_then(|i| self.shapes.get(i))) {
            (Tool::Arrow, _) => Some(StyleTarget::Arrow(self.arrow)),
            (Tool::Redact, _) => Some(StyleTarget::Redact(self.redact)),
            (Tool::Select, Some(Shape::Arrow { arrow, .. })) => Some(StyleTarget::Arrow(*arrow)),
            (Tool::Select, Some(Shape::Redact { mode, .. })) => Some(StyleTarget::Redact(*mode)),
            _ => None,
        }
    }

    /// The toolbar menus: the style of arrows and redactions, and the canvas
    /// (padding and background) around the image.
    pub fn menus(&self) -> [EditorMenu; MENUS] {
        fn choices<T: PartialEq + Copy>(list: &[(T, &str)], current: T) -> (String, Vec<EditorMenuItem>) {
            let title = list.iter().find(|(v, _)| *v == current).map_or("Style", |(_, l)| l).to_string();
            (title, list.iter().map(|(v, l)| EditorMenuItem::choice(*l, *v == current)).collect())
        }
        let style = match self.style_target() {
            Some(StyleTarget::Arrow(a)) => {
                let (title, items) = choices(&ARROW_STYLES, a);
                EditorMenu { title, tip: "Arrow style".into(), items, enabled: true }
            }
            Some(StyleTarget::Redact(r)) => {
                let (title, items) = choices(&REDACT_STYLES, r);
                EditorMenu { title, tip: "Redaction style".into(), items, enabled: true }
            }
            None => EditorMenu {
                title: "Style".into(),
                tip: "Arrow and redaction styles: pick the arrow or redact tool, or select one".into(),
                items: Vec::new(),
                enabled: false,
            },
        };
        let mut items = vec![EditorMenuItem::Heading("Padding".into())];
        items.extend(PADDINGS.iter().enumerate().map(|(i, (l, _))| EditorMenuItem::choice(*l, i == self.frame.padding)));
        items.push(EditorMenuItem::Separator);
        items.push(EditorMenuItem::Heading("Background".into()));
        items.extend(BACKGROUNDS.iter().enumerate().map(|(i, (l, _))| EditorMenuItem::choice(*l, i == self.frame.background)));
        let title = if self.frame.padding == 0 { "Canvas" } else { BACKGROUNDS[self.frame.background].0 };
        let canvas = EditorMenu { title: title.into(), tip: "Padding and background around the image".into(), items, enabled: true };
        [style, canvas]
    }

    /// Pointer slack for grabbing thin things, in image pixels.
    fn slack(&self) -> f64 {
        6.0 * self.px_per_pt
    }

    // -----------------------------------------------------------------------
    // Events
    // -----------------------------------------------------------------------

    pub fn handle(&mut self, event: EditorEvent) -> Effects {
        match event {
            EditorEvent::MouseDown { at, clicks, .. } => self.mouse_down(at, clicks),
            EditorEvent::MouseDrag { at, shift } => self.mouse_drag(at, shift),
            EditorEvent::MouseUp { at, shift } => {
                let mut fx = self.mouse_drag(at, shift);
                self.mouse_up(&mut fx);
                fx
            }
            EditorEvent::Tool(i) => match Tool::ALL.get(i) {
                Some(t) => self.pick_tool(*t),
                None => Effects::default(),
            },
            EditorEvent::Key(c) => match Tool::ALL.iter().find(|t| t.key() == c) {
                Some(t) => self.pick_tool(*t),
                None => Effects::default(),
            },
            EditorEvent::Color(i) if i < PALETTE.len() => {
                self.color = i;
                let color = self.color();
                self.restyle(|s| recolor(s, color))
            }
            EditorEvent::Width(i) if i < SIZES.len() => {
                self.size = i;
                let (stroke, font) = (self.stroke(), self.font());
                self.restyle(|s| resize_strokes(s, stroke, font))
            }
            EditorEvent::Color(_) | EditorEvent::Width(_) => Effects::default(),
            EditorEvent::Command(c) => self.command(c),
            EditorEvent::Text(t) => self.finish_text(&t),
            EditorEvent::Scale(s) => {
                self.px_per_pt = s.clamp(0.01, 100.0);
                Effects::default()
            }
            EditorEvent::Menu { menu, item } => self.menu(menu, item),
        }
    }

    fn menu(&mut self, menu: usize, item: usize) -> Effects {
        match (menu, self.style_target()) {
            (MENU_STYLE, Some(StyleTarget::Arrow(_))) if item < ARROW_STYLES.len() => {
                let style = ARROW_STYLES[item].0;
                self.arrow = style;
                self.restyle(|s| {
                    if let Shape::Arrow { arrow, .. } = s {
                        *arrow = style;
                    }
                })
            }
            (MENU_STYLE, Some(StyleTarget::Redact(_))) if item < REDACT_STYLES.len() => {
                let style = REDACT_STYLES[item].0;
                self.redact = style;
                self.restyle(|s| {
                    if let Shape::Redact { mode, strength, .. } = s {
                        *mode = style;
                        // A strength means different things per mode.
                        *strength = 0.0;
                    }
                })
            }
            (MENU_CANVAS, _) => {
                let mut frame = self.frame;
                match item.checked_sub(CANVAS_PADDING_ITEM) {
                    Some(i) if i < PADDINGS.len() => frame.padding = i,
                    _ => match item.checked_sub(CANVAS_BACKGROUND_ITEM) {
                        Some(i) if i < BACKGROUNDS.len() => {
                            frame.background = i;
                            if frame.padding == 0 {
                                frame.padding = DEFAULT_PADDING;
                            }
                        }
                        _ => return Effects::default(),
                    },
                }
                let mut fx = Effects { state: true, ..Default::default() };
                // Refuse a canvas that would make the image larger than the
                // renderer accepts, rather than leave a stale preview.
                let (w, h) = self.crop.map_or(self.image, |c| (c.w as u32, c.h as u32));
                let check = Annotation { canvas: self.canvas_for(frame), ..Default::default() };
                if let Err(e) = check.output_size(w, h) {
                    fx.error = Some(format!("{e:#}"));
                    return fx;
                }
                if frame != self.frame && self.drag.is_none() {
                    let before = self.snapshot();
                    self.frame = frame;
                    self.commit(before, &mut fx);
                }
                fx
            }
            _ => Effects::default(),
        }
    }

    fn pick_tool(&mut self, tool: Tool) -> Effects {
        // A cancelled move put a shape back that the image was drawn without;
        // entering or leaving the crop tool shows or hides what is cropped.
        let render = self.cancel_drag() | ((self.tool == Tool::Crop) != (tool == Tool::Crop));
        self.tool = tool;
        if tool != Tool::Select {
            self.selected = None;
        }
        Effects { render, overlay: true, state: true, ..Default::default() }
    }

    fn mouse_down(&mut self, at: Point, clicks: u32) -> Effects {
        // A drag still armed here lost its mouse-up; put its shape back.
        let render = self.cancel_drag();
        let mut fx = Effects { render, overlay: true, ..Default::default() };
        if self.shapes.len() >= limits::MAX_SHAPES && !matches!(self.tool, Tool::Select | Tool::Crop) {
            return fx;
        }
        match self.tool {
            Tool::Select => {
                // The selection (and with it the style menu) may change.
                fx.state = true;
                if let Some(index) = self.selected {
                    let grab = handles(&self.shapes[index]).iter().position(|h| distance(*h, at) <= self.slack());
                    if let Some(handle) = grab {
                        let original = self.shapes[index].clone();
                        self.drag = Some(Drag::Resize { index, handle, start: at, original, moved: false });
                        return fx;
                    }
                }
                self.selected = self.hit(at);
                let Some(index) = self.selected else { return fx };
                if let (true, Shape::Text { at: origin, text, style, .. }) = (clicks >= 2, &self.shapes[index]) {
                    fx.begin_text = Some(TextRequest {
                        at: *origin,
                        font_px: style.font_size.unwrap_or(28.0) as f64,
                        initial: text.clone(),
                        color: style.color.unwrap_or(Color::RED).0,
                    });
                    self.text = Some(TextEdit { at: *origin, replacing: Some(index) });
                    self.hidden = Some(index);
                    self.selected = None;
                    fx.render = true;
                    return fx;
                }
                self.drag = Some(Drag::Move { index, start: at, original: self.shapes[index].clone(), moved: false });
            }
            Tool::Text => {
                self.text = Some(TextEdit { at, replacing: None });
                fx.begin_text =
                    Some(TextRequest { at, font_px: self.font() as f64, initial: String::new(), color: self.color().0 });
            }
            Tool::Crop => {
                let original = self.crop;
                let mode = match original {
                    Some(r) => match corners(&r).iter().position(|c| distance(*c, at) <= self.slack()) {
                        Some(corner) => CropDrag::Corner(corner),
                        None if r.contains(at) => CropDrag::Move,
                        None => CropDrag::New,
                    },
                    None => CropDrag::New,
                };
                self.drag = Some(Drag::Crop { mode, start: at, original, moved: false });
            }
            Tool::Counter => {
                let next = self.shapes.iter().filter(|s| matches!(s, Shape::Counter { .. })).map(counter_number).max();
                let style = Style { color: Some(self.color()), font_size: Some(self.font()), ..Default::default() };
                let before = self.snapshot();
                self.shapes.push(Shape::Counter { at, number: Some(next.unwrap_or(0).saturating_add(1)), style });
                self.commit(before, &mut fx);
            }
            _ => self.drag = Some(Drag::Draw { start: at, current: at, points: vec![at] }),
        }
        fx
    }

    fn mouse_drag(&mut self, at: Point, shift: bool) -> Effects {
        let mut fx = Effects::default();
        let (tool, slack, image) = (self.tool, 2.0 * self.px_per_pt, self.image);
        match &mut self.drag {
            Some(Drag::Crop { mode, start, original, moved }) => {
                if !*moved && distance(at, *start) < slack {
                    return fx;
                }
                *moved = true;
                let (dx, dy) = (at.x - start.x, at.y - start.y);
                let wanted = match (*mode, *original) {
                    (CropDrag::Move, Some(r)) => {
                        // Slide, keeping the size, without leaving the image.
                        let x = (r.x + dx).clamp(0.0, (image.0 as f64 - r.w).max(0.0));
                        let y = (r.y + dy).clamp(0.0, (image.1 as f64 - r.h).max(0.0));
                        Rect::new(x, y, r.w, r.h)
                    }
                    (CropDrag::Corner(c), Some(r)) => rect_between(corners(&r)[(c + 2) % 4], at),
                    _ => rect_between(*start, at),
                };
                self.crop = crop_within(wanted, image).or(*original);
            }
            None => return fx,
            Some(Drag::Draw { start, current, points }) => {
                *current = if shift { constrain(tool, *start, at) } else { at };
                if points.len() < MAX_STROKE_POINTS && points.last().is_none_or(|l| distance(*l, at) >= 1.0) {
                    points.push(at);
                }
            }
            Some(Drag::Move { index, start, original, moved }) => {
                let (dx, dy) = (at.x - start.x, at.y - start.y);
                if !*moved && dx.hypot(dy) < slack {
                    return fx;
                }
                let first = !*moved;
                *moved = true;
                let index = *index;
                self.shapes[index] = translate(original, dx, dy);
                if first {
                    self.hide_while_dragging(index, &mut fx);
                }
            }
            Some(Drag::Resize { index, handle, start, original, moved }) => {
                // A click on a handle is not a resize.
                if !*moved && distance(at, *start) < slack {
                    return fx;
                }
                let first = !*moved;
                *moved = true;
                let index = *index;
                self.shapes[index] = reshape(original, *handle, at, shift);
                if first {
                    self.hide_while_dragging(index, &mut fx);
                }
            }
        }
        fx.overlay = true;
        fx
    }

    /// While a shape the window can draw by itself is dragged, take it out
    /// of the image so it does not show twice.
    fn hide_while_dragging(&mut self, index: usize, fx: &mut Effects) {
        if !preview(&self.shapes[index]).is_empty() {
            self.hidden = Some(index);
            fx.render = true;
        }
    }

    fn mouse_up(&mut self, fx: &mut Effects) {
        let Some(drag) = self.drag.take() else { return };
        fx.overlay = true;
        match drag {
            Drag::Draw { start, current, points } => {
                if let Some(shape) = self.shape_for(start, current, &points) {
                    let before = self.snapshot();
                    self.shapes.push(shape);
                    self.commit(before, fx);
                }
            }
            Drag::Move { index, original, moved, .. } | Drag::Resize { index, original, moved, .. } => {
                self.hidden = None;
                if moved {
                    let mut before = self.snapshot();
                    before.shapes[index] = original;
                    self.commit(before, fx);
                }
            }
            Drag::Crop { original, .. } => {
                // A crop of the whole image is no crop.
                if self.crop.is_some_and(|r| r == Rect::new(0.0, 0.0, self.image.0 as f64, self.image.1 as f64)) {
                    self.crop = None;
                }
                if self.crop != original {
                    let mut before = self.snapshot();
                    before.crop = original;
                    self.commit(before, fx);
                }
            }
        }
    }

    /// Abandon a drag in progress, putting a moved shape back.
    fn cancel_drag(&mut self) -> bool {
        match self.drag.take() {
            Some(Drag::Move { index, original, .. }) | Some(Drag::Resize { index, original, .. }) => {
                self.shapes[index] = original;
                self.hidden = None;
                true
            }
            Some(Drag::Crop { original, .. }) => {
                self.crop = original;
                false
            }
            Some(Drag::Draw { .. }) => true,
            None => false,
        }
    }

    fn command(&mut self, c: EditorCommand) -> Effects {
        let mut fx = Effects::default();
        match c {
            EditorCommand::Save | EditorCommand::Copy => {
                // Mid-drag, the shape goes back where it was first.
                if self.cancel_drag() {
                    fx.render = true;
                    fx.overlay = true;
                }
                fx.request = Some(if c == EditorCommand::Save { Request::Save } else { Request::Copy });
            }
            EditorCommand::Undo | EditorCommand::Redo => {
                self.cancel_drag();
                let undo = c == EditorCommand::Undo;
                if let Some(snapshot) = if undo { self.undo.pop() } else { self.redo.pop() } {
                    let now = self.restore(snapshot);
                    if undo { self.redo.push(now) } else { self.undo.push(now) }
                    self.selected = None;
                }
                fx.render = true;
            }
            EditorCommand::Delete => {
                fx.render = self.cancel_drag();
                if self.tool == Tool::Crop && self.crop.is_some() {
                    let before = self.snapshot();
                    self.crop = None;
                    self.commit(before, &mut fx);
                } else if let Some(index) = self.selected.take() {
                    let before = self.snapshot();
                    self.shapes.remove(index);
                    self.commit(before, &mut fx);
                }
                fx.overlay = true;
            }
            EditorCommand::Escape => {
                // Back out one step at a time: the drag, then the selection,
                // then the tool. Never closes the window.
                if self.drag.is_some() {
                    fx.render = self.cancel_drag();
                } else if self.selected.take().is_none() && self.tool != Tool::Select {
                    // Leaving the crop tool applies the crop.
                    fx.render = self.tool == Tool::Crop;
                    self.tool = Tool::Select;
                }
                fx.overlay = true;
                fx.state = true;
            }
        }
        fx
    }

    fn finish_text(&mut self, typed: &str) -> Effects {
        let mut fx = Effects { overlay: true, ..Default::default() };
        let Some(edit) = self.text.take() else { return fx };
        let was_hidden = self.hidden.take().is_some();
        fx.render = was_hidden;
        // One kind of line break, no blank lines around the label, and no
        // longer than the renderer accepts.
        let typed = text_lines(typed).join("\n");
        let typed: String = typed.trim().chars().take(limits::MAX_TEXT_CHARS).collect();
        let typed = typed.trim_end();
        if typed.is_empty() {
            // Cancelled or left blank: an edited label stays as it was.
            return fx;
        }
        let before = self.snapshot();
        match edit.replacing {
            Some(index) => match self.shapes.get_mut(index) {
                Some(Shape::Text { text, .. }) if text != typed => *text = typed.to_string(),
                _ => return fx,
            },
            None => {
                let style = Style { color: Some(self.color()), font_size: Some(self.font()), ..Default::default() };
                self.shapes.push(Shape::Text { at: edit.at, text: typed.to_string(), background: None, style });
            }
        }
        self.commit(before, &mut fx);
        fx
    }

    /// Apply a style change to the selected shape, if it takes one.
    fn restyle(&mut self, change: impl Fn(&mut Shape)) -> Effects {
        let mut fx = Effects { state: true, ..Default::default() };
        if self.drag.is_some() {
            return fx;
        }
        if let Some(index) = self.selected {
            let before = self.snapshot();
            change(&mut self.shapes[index]);
            if self.shapes != before.shapes {
                self.commit(before, &mut fx);
            }
        }
        fx
    }

    /// Record `before` for undo after the document changed.
    fn commit(&mut self, before: Snapshot, fx: &mut Effects) {
        self.undo.push(before);
        if self.undo.len() > UNDO_LIMIT {
            self.undo.remove(0);
        }
        self.redo.clear();
        fx.render = true;
        fx.state = true;
        fx.overlay = true;
    }

    /// The topmost shape under `p`. Nothing is picked outside the crop,
    /// where nothing shows.
    fn hit(&self, p: Point) -> Option<usize> {
        if self.crop.is_some_and(|c| !c.contains(p)) {
            return None;
        }
        (0..self.shapes.len()).rev().find(|i| hits(&self.shapes[*i], p, self.slack()))
    }

    /// The shape a drag from `start` to `end` makes with the current tool;
    /// `None` when it is too small to be meant.
    fn shape_for(&self, start: Point, end: Point, points: &[Point]) -> Option<Shape> {
        let min = 3.0 * self.px_per_pt;
        let rect = rect_between(start, end);
        let big_enough = rect.w >= min && rect.h >= min;
        let long_enough = distance(start, end) >= min;
        let style = self.style();
        match self.tool {
            Tool::Arrow => long_enough.then_some(Shape::Arrow { from: start, to: end, arrow: self.arrow, style }),
            Tool::Line => long_enough.then_some(Shape::Line { from: start, to: end, style }),
            Tool::Rectangle => big_enough.then_some(Shape::Rectangle { rect, radius: 0.0, style }),
            Tool::Ellipse => big_enough.then_some(Shape::Ellipse { rect, style }),
            Tool::Pen => {
                let travelled: f64 = points.windows(2).map(|w| distance(w[0], w[1])).sum();
                let used: usize = self.shapes.iter().map(|s| if let Shape::Freehand { points, .. } = s { points.len() } else { 0 }).sum();
                let fits = used + points.len() <= limits::MAX_FREEHAND_POINTS;
                (travelled >= min && fits).then(|| Shape::Freehand { points: points.to_vec(), style })
            }
            Tool::Highlight => big_enough.then(|| Shape::Highlight {
                rect,
                style: Style { color: Some(self.color().with_alpha(HIGHLIGHT_ALPHA)), ..Default::default() },
            }),
            Tool::Redact => big_enough.then_some(Shape::Redact { rect, mode: self.redact, strength: 0.0 }),
            Tool::Spotlight => big_enough.then_some(Shape::Spotlight { rect, dim: 0.6 }),
            Tool::Select | Tool::Text | Tool::Counter | Tool::Crop => None,
        }
    }

    /// What the window draws above the image: the shape being dragged out
    /// or moved, and the selection marks.
    pub fn overlay(&self) -> Vec<EditorPrim> {
        let mut out = Vec::new();
        if self.tool == Tool::Crop {
            // The whole image shows: dim what the crop leaves out.
            if let Some(r) = self.crop {
                let (w, h) = (self.image.0 as f64, self.image.1 as f64);
                let shade = [0, 0, 0, 0xA0];
                for rect in [
                    Rect::new(0.0, 0.0, w, r.y),
                    Rect::new(0.0, r.bottom(), w, h - r.bottom()),
                    Rect::new(0.0, r.y, r.x, r.h),
                    Rect::new(r.right(), r.y, w - r.right(), r.h),
                ] {
                    if rect.w > 0.0 && rect.h > 0.0 {
                        out.push(EditorPrim::Fill { rect, color: shade });
                    }
                }
                out.push(EditorPrim::Selection { rect: r });
                out.extend(corners(&r).into_iter().map(|at| EditorPrim::Handle { at }));
            }
            return out;
        }
        if let Some(Drag::Draw { start, current, points }) = &self.drag {
            if let Some(shape) = self.shape_for(*start, *current, points) {
                out.extend(preview(&shape));
            }
        }
        if let Some(index) = self.hidden.filter(|_| self.text.is_none()) {
            out.extend(preview(&self.shapes[index]));
        }
        if let Some(shape) = self.selected.and_then(|i| self.shapes.get(i)) {
            out.push(EditorPrim::Selection { rect: bounds(shape) });
            out.extend(handles(shape).into_iter().map(|at| EditorPrim::Handle { at }));
        }
        out
    }

    /// The document as saved is now the baseline for [`Editor::is_dirty`].
    #[cfg(test)]
    fn mark_saved(&mut self) {
        self.saved = self.snapshot();
    }
}

// ---------------------------------------------------------------------------
// Shape geometry
// ---------------------------------------------------------------------------

fn distance(a: Point, b: Point) -> f64 {
    (a.x - b.x).hypot(a.y - b.y)
}

fn rect_between(a: Point, b: Point) -> Rect {
    Rect::new(a.x.min(b.x), a.y.min(b.y), (a.x - b.x).abs(), (a.y - b.y).abs())
}

fn normalized(r: &Rect) -> Rect {
    rect_between(r.origin(), Point { x: r.x + r.w, y: r.y + r.h })
}

/// Corners clockwise from top-left.
fn corners(r: &Rect) -> [Point; 4] {
    let r = normalized(r);
    [Point { x: r.x, y: r.y }, Point { x: r.right(), y: r.y }, Point { x: r.right(), y: r.bottom() }, Point { x: r.x, y: r.bottom() }]
}

/// `r` cut to the image and rounded to whole pixels; `None` when less than
/// [`MIN_CROP`] is left either way.
fn crop_within(r: Rect, image: (u32, u32)) -> Option<Rect> {
    let r = normalized(&r);
    let (w, h) = (image.0 as f64, image.1 as f64);
    let x0 = r.x.clamp(0.0, w).round();
    let y0 = r.y.clamp(0.0, h).round();
    let x1 = r.right().clamp(0.0, w).round();
    let y1 = r.bottom().clamp(0.0, h).round();
    (x1 - x0 >= MIN_CROP && y1 - y0 >= MIN_CROP).then(|| Rect::new(x0, y0, x1 - x0, y1 - y0))
}

/// Shift-drag: lines snap to 45° steps, boxes become squares.
fn constrain(tool: Tool, start: Point, at: Point) -> Point {
    let (dx, dy) = (at.x - start.x, at.y - start.y);
    match tool {
        Tool::Arrow | Tool::Line => {
            let step = std::f64::consts::FRAC_PI_4;
            let angle = (dy.atan2(dx) / step).round() * step;
            let len = dx.hypot(dy);
            Point { x: start.x + len * angle.cos(), y: start.y + len * angle.sin() }
        }
        Tool::Rectangle | Tool::Ellipse => {
            let side = dx.abs().max(dy.abs());
            Point { x: start.x + side.copysign(dx), y: start.y + side.copysign(dy) }
        }
        _ => at,
    }
}

fn counter_number(s: &Shape) -> u32 {
    match s {
        Shape::Counter { number, .. } => number.unwrap_or(0),
        _ => 0,
    }
}

fn stroke_of(style: &Style) -> f64 {
    style.stroke_width.unwrap_or(4.0) as f64
}

/// Bounding box of a shape in image pixels. Text is measured roughly (the
/// box is only used for picking and the selection outline).
fn bounds(shape: &Shape) -> Rect {
    let around = |pts: &[Point]| {
        let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
        for p in pts {
            x0 = x0.min(p.x);
            y0 = y0.min(p.y);
            x1 = x1.max(p.x);
            y1 = y1.max(p.y);
        }
        if pts.is_empty() {
            Rect::default()
        } else {
            Rect::new(x0, y0, x1 - x0, y1 - y0)
        }
    };
    match shape {
        Shape::Rectangle { rect, .. }
        | Shape::Ellipse { rect, .. }
        | Shape::Highlight { rect, .. }
        | Shape::Redact { rect, .. }
        | Shape::Spotlight { rect, .. } => normalized(rect),
        Shape::Line { from, to, .. } | Shape::Arrow { from, to, .. } => around(&[*from, *to]),
        Shape::Freehand { points, .. } => around(points),
        Shape::Text { at, text, style, .. } => {
            let size = style.font_size.unwrap_or(28.0) as f64;
            let lines = text_lines(text);
            let widest = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0).max(1) as f64;
            let lines = lines.len().max(1) as f64;
            Rect::new(at.x, at.y, widest * size * 0.58, lines * size * 1.25)
        }
        Shape::Counter { at, style, .. } => {
            let r = style.font_size.unwrap_or(28.0) as f64 * 0.75;
            Rect::new(at.x - r, at.y - r, 2.0 * r, 2.0 * r)
        }
        Shape::Image { at, .. } => Rect::new(at.x, at.y, 0.0, 0.0),
    }
}

fn distance_to_segment(p: Point, a: Point, b: Point) -> f64 {
    let (dx, dy) = (b.x - a.x, b.y - a.y);
    let len2 = dx * dx + dy * dy;
    if len2 <= f64::EPSILON {
        return distance(p, a);
    }
    let t = (((p.x - a.x) * dx + (p.y - a.y) * dy) / len2).clamp(0.0, 1.0);
    distance(p, Point { x: a.x + t * dx, y: a.y + t * dy })
}

fn grown(r: &Rect, by: f64) -> Rect {
    Rect::new(r.x - by, r.y - by, r.w + 2.0 * by, r.h + 2.0 * by)
}

/// Whether a click at `p` picks `shape`. Outlines are picked by their
/// stroke, so a box drawn around something does not shadow what is inside.
fn hits(shape: &Shape, p: Point, slack: f64) -> bool {
    match shape {
        Shape::Line { from, to, style } | Shape::Arrow { from, to, style, .. } => {
            distance_to_segment(p, *from, *to) <= slack + stroke_of(style) / 2.0
        }
        Shape::Freehand { points, style } => {
            let reach = slack + stroke_of(style) / 2.0;
            match points.as_slice() {
                [only] => distance(p, *only) <= reach,
                pts => pts.windows(2).any(|w| distance_to_segment(p, w[0], w[1]) <= reach),
            }
        }
        Shape::Rectangle { rect, style, .. } => {
            let (r, reach) = (normalized(rect), slack + stroke_of(style) / 2.0);
            let filled = style.fill.is_some();
            grown(&r, reach).contains(p) && (filled || !grown(&r, -reach).contains(p))
        }
        Shape::Ellipse { rect, style } => {
            let (r, reach) = (normalized(rect), slack + stroke_of(style) / 2.0);
            let (rx, ry) = (r.w / 2.0, r.h / 2.0);
            if rx <= 0.0 || ry <= 0.0 {
                return false;
            }
            let c = r.center();
            let d = ((p.x - c.x) / rx).hypot((p.y - c.y) / ry);
            let off = (d - 1.0) * rx.min(ry);
            if style.fill.is_some() {
                off <= reach
            } else {
                off.abs() <= reach
            }
        }
        _ => grown(&bounds(shape), slack / 2.0).contains(p),
    }
}

fn translate(shape: &Shape, dx: f64, dy: f64) -> Shape {
    let mut s = shape.clone();
    s.translate(dx, dy);
    s
}

/// Drag handles: the corners of box shapes (clockwise from top-left), the
/// two ends of lines and arrows, none for the rest.
fn handles(shape: &Shape) -> Vec<Point> {
    match shape {
        Shape::Rectangle { rect, .. }
        | Shape::Ellipse { rect, .. }
        | Shape::Highlight { rect, .. }
        | Shape::Redact { rect, .. }
        | Shape::Spotlight { rect, .. } => corners(rect).to_vec(),
        Shape::Line { from, to, .. } | Shape::Arrow { from, to, .. } => vec![*from, *to],
        _ => Vec::new(),
    }
}

/// `original` with handle `handle` dragged to `to`.
fn reshape(original: &Shape, handle: usize, to: Point, shift: bool) -> Shape {
    let corners = handles(original);
    let mut s = original.clone();
    match &mut s {
        Shape::Rectangle { rect, .. } | Shape::Ellipse { rect, .. } => {
            let anchor = corners[(handle + 2) % 4];
            *rect = rect_between(anchor, if shift { constrain(Tool::Rectangle, anchor, to) } else { to });
        }
        Shape::Highlight { rect, .. } | Shape::Redact { rect, .. } | Shape::Spotlight { rect, .. } => {
            *rect = rect_between(corners[(handle + 2) % 4], to);
        }
        Shape::Line { from, to: end, .. } | Shape::Arrow { from, to: end, .. } => {
            if handle == 0 {
                *from = if shift { constrain(Tool::Line, *end, to) } else { to };
            } else {
                *end = if shift { constrain(Tool::Line, *from, to) } else { to };
            }
        }
        _ => {}
    }
    s
}

fn recolor(shape: &mut Shape, color: Color) {
    match shape {
        Shape::Rectangle { style, .. }
        | Shape::Ellipse { style, .. }
        | Shape::Line { style, .. }
        | Shape::Arrow { style, .. }
        | Shape::Freehand { style, .. }
        | Shape::Text { style, .. }
        | Shape::Counter { style, .. } => style.color = Some(color),
        Shape::Highlight { style, .. } => style.color = Some(color.with_alpha(HIGHLIGHT_ALPHA)),
        Shape::Redact { .. } | Shape::Spotlight { .. } | Shape::Image { .. } => {}
    }
}

fn resize_strokes(shape: &mut Shape, stroke: f32, font: f32) {
    match shape {
        Shape::Rectangle { style, .. }
        | Shape::Ellipse { style, .. }
        | Shape::Line { style, .. }
        | Shape::Arrow { style, .. }
        | Shape::Freehand { style, .. } => style.stroke_width = Some(stroke),
        Shape::Text { style, .. } | Shape::Counter { style, .. } => style.font_size = Some(font),
        Shape::Highlight { .. } | Shape::Redact { .. } | Shape::Spotlight { .. } | Shape::Image { .. } => {}
    }
}

/// How the window draws a shape while it is being dragged. Empty for shapes
/// it cannot imitate (text, badges): those stay in the image until released.
fn preview(shape: &Shape) -> Vec<EditorPrim> {
    let ink = |style: &Style| (style.color.unwrap_or(Color::RED).0, stroke_of(style));
    match shape {
        Shape::Rectangle { rect, style, .. } => {
            let (color, width) = ink(style);
            vec![EditorPrim::Rect { rect: normalized(rect), color, width }]
        }
        Shape::Ellipse { rect, style } => {
            let (color, width) = ink(style);
            vec![EditorPrim::Ellipse { rect: normalized(rect), color, width }]
        }
        Shape::Line { from, to, style } => {
            let (color, width) = ink(style);
            vec![EditorPrim::Polyline { points: vec![*from, *to], color, width }]
        }
        Shape::Arrow { from, to, style, arrow } => {
            let (color, width) = ink(style);
            let width = if *arrow == ArrowStyle::Slim { width * 0.6 } else { width };
            vec![EditorPrim::Arrow { from: *from, to: *to, color, width, double: *arrow == ArrowStyle::Double }]
        }
        Shape::Freehand { points, style } => {
            let (color, width) = ink(style);
            vec![EditorPrim::Polyline { points: points.clone(), color, width }]
        }
        Shape::Highlight { rect, style } => {
            let c = style.color.unwrap_or(Color::YELLOW).0;
            vec![EditorPrim::Fill { rect: normalized(rect), color: [c[0], c[1], c[2], 0x66] }]
        }
        Shape::Redact { rect, mode, .. } => vec![
            EditorPrim::Fill {
                rect: normalized(rect),
                color: if *mode == RedactStyle::BlackOut { [0, 0, 0, 0xFF] } else { [0x80, 0x80, 0x80, 0xC0] },
            },
            EditorPrim::Rect { rect: normalized(rect), color: [0xFF, 0xFF, 0xFF, 0xC0], width: 0.0 },
        ],
        Shape::Spotlight { rect, .. } => vec![
            EditorPrim::Fill { rect: normalized(rect), color: [0xFF, 0xFF, 0xFF, 0x30] },
            EditorPrim::Rect { rect: normalized(rect), color: [0xFF, 0xFF, 0xFF, 0xE0], width: 0.0 },
        ],
        Shape::Text { .. } | Shape::Counter { .. } | Shape::Image { .. } => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pt(x: f64, y: f64) -> Point {
        Point { x, y }
    }

    fn editor() -> Editor {
        let mut e = Editor::new((800, 600));
        e.handle(EditorEvent::Scale(1.0));
        e
    }

    fn drag(e: &mut Editor, from: Point, to: Point) -> Effects {
        e.handle(EditorEvent::MouseDown { at: from, shift: false, clicks: 1 });
        e.handle(EditorEvent::MouseDrag { at: pt((from.x + to.x) / 2.0, (from.y + to.y) / 2.0), shift: false });
        e.handle(EditorEvent::MouseUp { at: to, shift: false })
    }

    fn pick(e: &mut Editor, tool: Tool) {
        e.handle(EditorEvent::Tool(tool.index()));
    }

    fn cmd(e: &mut Editor, c: EditorCommand) -> Effects {
        e.handle(EditorEvent::Command(c))
    }

    #[test]
    fn tools_have_distinct_keys_and_buttons() {
        let keys: std::collections::HashSet<char> = Tool::ALL.iter().map(|t| t.key()).collect();
        assert_eq!(keys.len(), Tool::ALL.len());
        for (i, t) in Tool::ALL.iter().enumerate() {
            assert_eq!(t.index(), i);
            let b = t.button();
            assert!(!b.symbol.is_empty());
            assert!(b.label.contains(&format!("({})", t.key().to_ascii_uppercase())), "{}", b.label);
        }
    }

    #[test]
    fn dragging_draws_the_current_tool() {
        let mut e = editor();
        assert_eq!(e.state().tool, Tool::Arrow.index());
        let fx = drag(&mut e, pt(10.0, 10.0), pt(110.0, 60.0));
        assert!(fx.render && fx.state);
        assert!(matches!(&e.shapes[..], [Shape::Arrow { from, to, .. }] if *from == pt(10.0, 10.0) && *to == pt(110.0, 60.0)));
        pick(&mut e, Tool::Rectangle);
        drag(&mut e, pt(200.0, 200.0), pt(100.0, 120.0));
        assert!(matches!(&e.shapes[1], Shape::Rectangle { rect, .. } if *rect == Rect::new(100.0, 120.0, 100.0, 80.0)));
        for (tool, kind) in [
            (Tool::Ellipse, "ellipse"),
            (Tool::Line, "line"),
            (Tool::Pen, "freehand"),
            (Tool::Highlight, "highlight"),
            (Tool::Redact, "redact"),
            (Tool::Spotlight, "spotlight"),
        ] {
            pick(&mut e, tool);
            drag(&mut e, pt(300.0, 300.0), pt(400.0, 380.0));
            assert_eq!(e.shapes.last().unwrap().kind(), kind);
        }
        assert!(e.is_dirty());
        e.final_document().validate().unwrap();
    }

    #[test]
    fn a_click_without_a_drag_draws_nothing() {
        let mut e = editor();
        for tool in [Tool::Arrow, Tool::Rectangle, Tool::Pen, Tool::Highlight] {
            pick(&mut e, tool);
            let fx = drag(&mut e, pt(50.0, 50.0), pt(51.0, 51.0));
            assert!(!fx.render, "{tool:?}");
        }
        assert!(e.is_empty() && !e.is_dirty() && !e.state().can_undo);
    }

    #[test]
    fn the_shape_being_dragged_out_is_in_the_overlay() {
        let mut e = editor();
        e.handle(EditorEvent::MouseDown { at: pt(10.0, 10.0), shift: false, clicks: 1 });
        let fx = e.handle(EditorEvent::MouseDrag { at: pt(90.0, 10.0), shift: false });
        assert!(fx.overlay && !fx.render);
        assert!(matches!(&e.overlay()[..], [EditorPrim::Arrow { to, .. }] if *to == pt(90.0, 10.0)));
        e.handle(EditorEvent::MouseUp { at: pt(90.0, 10.0), shift: false });
        assert!(e.overlay().is_empty(), "a finished shape is part of the image");
    }

    #[test]
    fn shift_constrains() {
        let mut e = editor();
        e.handle(EditorEvent::MouseDown { at: pt(0.0, 0.0), shift: false, clicks: 1 });
        e.handle(EditorEvent::MouseUp { at: pt(100.0, 8.0), shift: true });
        let Shape::Arrow { to, .. } = &e.shapes[0] else { panic!() };
        assert!(to.y.abs() < 1e-9 && (to.x - pt(100.0, 8.0).x.hypot(8.0)).abs() < 1e-9, "{to:?}");
        pick(&mut e, Tool::Ellipse);
        e.handle(EditorEvent::MouseDown { at: pt(100.0, 100.0), shift: false, clicks: 1 });
        e.handle(EditorEvent::MouseUp { at: pt(160.0, 70.0), shift: true });
        assert!(matches!(&e.shapes[1], Shape::Ellipse { rect, .. } if *rect == Rect::new(100.0, 40.0, 60.0, 60.0)));
    }

    #[test]
    fn undo_and_redo_walk_the_history() {
        let mut e = editor();
        drag(&mut e, pt(10.0, 10.0), pt(110.0, 60.0));
        drag(&mut e, pt(10.0, 100.0), pt(110.0, 160.0));
        assert!(cmd(&mut e, EditorCommand::Undo).render);
        assert_eq!(e.shapes.len(), 1);
        assert!(e.state().can_redo);
        cmd(&mut e, EditorCommand::Undo);
        assert!(e.is_empty() && !e.is_dirty() && !e.state().can_undo);
        cmd(&mut e, EditorCommand::Undo);
        assert!(e.is_empty());
        cmd(&mut e, EditorCommand::Redo);
        cmd(&mut e, EditorCommand::Redo);
        assert_eq!(e.shapes.len(), 2);
        cmd(&mut e, EditorCommand::Undo);
        drag(&mut e, pt(300.0, 300.0), pt(400.0, 400.0));
        assert!(!e.state().can_redo, "a new edit drops the redo stack");
    }

    #[test]
    fn dirty_follows_the_saved_baseline() {
        let mut e = editor();
        drag(&mut e, pt(10.0, 10.0), pt(110.0, 60.0));
        e.mark_saved();
        assert!(!e.state().dirty);
        drag(&mut e, pt(10.0, 100.0), pt(110.0, 160.0));
        assert!(e.state().dirty);
        cmd(&mut e, EditorCommand::Undo);
        assert!(!e.state().dirty, "undoing back to the saved document is clean");
    }

    #[test]
    fn select_moves_and_deletes() {
        let mut e = editor();
        pick(&mut e, Tool::Rectangle);
        drag(&mut e, pt(100.0, 100.0), pt(200.0, 180.0));
        pick(&mut e, Tool::Select);
        // Inside the outline is not the outline.
        e.handle(EditorEvent::MouseDown { at: pt(150.0, 140.0), shift: false, clicks: 1 });
        e.handle(EditorEvent::MouseUp { at: pt(150.0, 140.0), shift: false });
        assert_eq!(e.selected, None);
        // Grab the top edge and move by (30, 20).
        e.handle(EditorEvent::MouseDown { at: pt(150.0, 100.0), shift: false, clicks: 1 });
        assert_eq!(e.selected, Some(0));
        let fx = e.handle(EditorEvent::MouseDrag { at: pt(180.0, 120.0), shift: false });
        assert!(fx.render, "the moving shape leaves the image");
        assert!(e.document().shapes.is_empty());
        assert!(e.overlay().iter().any(|p| matches!(p, EditorPrim::Rect { rect, .. } if *rect == Rect::new(130.0, 120.0, 100.0, 80.0))));
        let fx = e.handle(EditorEvent::MouseUp { at: pt(180.0, 120.0), shift: false });
        assert!(fx.render);
        assert!(matches!(&e.shapes[0], Shape::Rectangle { rect, .. } if *rect == Rect::new(130.0, 120.0, 100.0, 80.0)));
        assert_eq!(e.document().shapes.len(), 1);
        // One undo step for the whole move.
        cmd(&mut e, EditorCommand::Undo);
        assert!(matches!(&e.shapes[0], Shape::Rectangle { rect, .. } if *rect == Rect::new(100.0, 100.0, 100.0, 80.0)));
        cmd(&mut e, EditorCommand::Redo);
        e.handle(EditorEvent::MouseDown { at: pt(130.0, 150.0), shift: false, clicks: 1 });
        e.handle(EditorEvent::MouseUp { at: pt(130.0, 150.0), shift: false });
        assert_eq!(e.selected, Some(0));
        assert!(!e.state().can_redo || e.shapes.len() == 1);
        let undo_depth = e.undo.len();
        assert!(cmd(&mut e, EditorCommand::Delete).render);
        assert!(e.is_empty());
        assert_eq!(e.undo.len(), undo_depth + 1);
        // Delete with nothing selected does nothing.
        assert!(!cmd(&mut e, EditorCommand::Delete).render);
    }

    #[test]
    fn a_click_on_a_shape_selects_without_an_undo_step() {
        let mut e = editor();
        drag(&mut e, pt(10.0, 10.0), pt(110.0, 10.0));
        pick(&mut e, Tool::Select);
        let depth = e.undo.len();
        e.handle(EditorEvent::MouseDown { at: pt(60.0, 12.0), shift: false, clicks: 1 });
        let fx = e.handle(EditorEvent::MouseUp { at: pt(60.5, 12.0), shift: false });
        assert_eq!(e.selected, Some(0));
        assert!(!fx.render);
        assert_eq!(e.undo.len(), depth);
        assert!(e.overlay().iter().any(|p| matches!(p, EditorPrim::Selection { .. })));
        assert_eq!(e.overlay().iter().filter(|p| matches!(p, EditorPrim::Handle { .. })).count(), 2);
    }

    #[test]
    fn the_topmost_shape_wins() {
        let mut e = editor();
        pick(&mut e, Tool::Line);
        drag(&mut e, pt(0.0, 50.0), pt(200.0, 50.0));
        drag(&mut e, pt(100.0, 0.0), pt(100.0, 200.0));
        pick(&mut e, Tool::Select);
        e.handle(EditorEvent::MouseDown { at: pt(100.0, 50.0), shift: false, clicks: 1 });
        assert_eq!(e.selected, Some(1));
        e.handle(EditorEvent::MouseUp { at: pt(100.0, 50.0), shift: false });
        e.handle(EditorEvent::MouseDown { at: pt(20.0, 51.0), shift: false, clicks: 1 });
        assert_eq!(e.selected, Some(0));
    }

    #[test]
    fn handles_resize() {
        let mut e = editor();
        pick(&mut e, Tool::Rectangle);
        drag(&mut e, pt(100.0, 100.0), pt(200.0, 180.0));
        pick(&mut e, Tool::Select);
        e.handle(EditorEvent::MouseDown { at: pt(100.0, 140.0), shift: false, clicks: 1 });
        e.handle(EditorEvent::MouseUp { at: pt(100.0, 140.0), shift: false });
        // Bottom-right corner out to (260, 220): the top-left stays put.
        e.handle(EditorEvent::MouseDown { at: pt(201.0, 181.0), shift: false, clicks: 1 });
        e.handle(EditorEvent::MouseDrag { at: pt(240.0, 200.0), shift: false });
        e.handle(EditorEvent::MouseUp { at: pt(260.0, 220.0), shift: false });
        assert!(matches!(&e.shapes[0], Shape::Rectangle { rect, .. } if *rect == Rect::new(100.0, 100.0, 160.0, 120.0)));
        // Dragging a corner across the anchor flips the box instead of
        // producing a negative size.
        e.handle(EditorEvent::MouseDown { at: pt(260.0, 220.0), shift: false, clicks: 1 });
        e.handle(EditorEvent::MouseUp { at: pt(40.0, 60.0), shift: false });
        assert!(matches!(&e.shapes[0], Shape::Rectangle { rect, .. } if *rect == Rect::new(40.0, 60.0, 60.0, 40.0)));
        // An arrow's end.
        pick(&mut e, Tool::Arrow);
        drag(&mut e, pt(300.0, 300.0), pt(400.0, 300.0));
        pick(&mut e, Tool::Select);
        e.handle(EditorEvent::MouseDown { at: pt(350.0, 300.0), shift: false, clicks: 1 });
        e.handle(EditorEvent::MouseUp { at: pt(350.0, 300.0), shift: false });
        e.handle(EditorEvent::MouseDown { at: pt(400.0, 300.0), shift: false, clicks: 1 });
        e.handle(EditorEvent::MouseUp { at: pt(420.0, 380.0), shift: false });
        assert!(matches!(&e.shapes[1], Shape::Arrow { from, to, .. } if *from == pt(300.0, 300.0) && *to == pt(420.0, 380.0)));
    }

    #[test]
    fn a_click_on_a_handle_is_not_a_resize() {
        let mut e = editor();
        pick(&mut e, Tool::Rectangle);
        drag(&mut e, pt(100.0, 100.0), pt(200.0, 180.0));
        pick(&mut e, Tool::Select);
        e.handle(EditorEvent::MouseDown { at: pt(100.0, 140.0), shift: false, clicks: 1 });
        e.handle(EditorEvent::MouseUp { at: pt(100.0, 140.0), shift: false });
        let depth = e.undo.len();
        e.handle(EditorEvent::MouseDown { at: pt(203.0, 183.0), shift: false, clicks: 1 });
        let fx = e.handle(EditorEvent::MouseUp { at: pt(203.5, 183.0), shift: false });
        assert!(!fx.render);
        assert_eq!(e.undo.len(), depth);
        assert!(matches!(&e.shapes[0], Shape::Rectangle { rect, .. } if *rect == Rect::new(100.0, 100.0, 100.0, 80.0)));
    }

    #[test]
    fn a_drag_cancelled_by_another_input_redraws_the_shape() {
        let start = |e: &mut Editor| {
            pick(e, Tool::Select);
            e.handle(EditorEvent::MouseDown { at: pt(150.0, 100.0), shift: false, clicks: 1 });
            assert!(e.handle(EditorEvent::MouseDrag { at: pt(190.0, 140.0), shift: false }).render);
            assert!(e.document().shapes.is_empty());
        };
        let mut e = editor();
        pick(&mut e, Tool::Rectangle);
        drag(&mut e, pt(100.0, 100.0), pt(200.0, 180.0));
        let depth = e.undo.len();
        let home = Rect::new(100.0, 100.0, 100.0, 80.0);
        // A tool key, a save, and a mouse-down after a lost mouse-up.
        start(&mut e);
        assert!(e.handle(EditorEvent::Key('a')).render);
        assert_eq!(e.document().shapes.len(), 1);
        start(&mut e);
        let fx = cmd(&mut e, EditorCommand::Save);
        assert!(fx.render && fx.request == Some(Request::Save));
        assert!(matches!(&e.final_document().shapes[0], Shape::Rectangle { rect, .. } if *rect == home));
        start(&mut e);
        assert!(e.handle(EditorEvent::MouseDown { at: pt(500.0, 500.0), shift: false, clicks: 1 }).render);
        assert!(matches!(&e.shapes[0], Shape::Rectangle { rect, .. } if *rect == home));
        assert_eq!(e.undo.len(), depth);
    }

    #[test]
    fn the_renderer_limits_are_kept() {
        let mut e = editor();
        pick(&mut e, Tool::Text);
        e.handle(EditorEvent::MouseDown { at: pt(10.0, 10.0), shift: false, clicks: 1 });
        e.handle(EditorEvent::Text("x".repeat(limits::MAX_TEXT_CHARS + 50)));
        assert!(matches!(&e.shapes[0], Shape::Text { text, .. } if text.chars().count() == limits::MAX_TEXT_CHARS));
        // One endless pen stroke keeps a bounded number of points.
        pick(&mut e, Tool::Pen);
        e.handle(EditorEvent::MouseDown { at: pt(0.0, 0.0), shift: false, clicks: 1 });
        for i in 0..(MAX_STROKE_POINTS * 2) {
            e.handle(EditorEvent::MouseDrag { at: pt(i as f64 * 2.0, 5.0), shift: false });
        }
        e.handle(EditorEvent::MouseUp { at: pt(1.0, 9.0), shift: false });
        assert!(matches!(&e.shapes[1], Shape::Freehand { points, .. } if points.len() == MAX_STROKE_POINTS));
        e.final_document().validate().unwrap();
        // A full document takes no more shapes.
        let line = Shape::Line { from: pt(0.0, 0.0), to: pt(9.0, 9.0), style: Style::default() };
        e.shapes = vec![line; limits::MAX_SHAPES];
        pick(&mut e, Tool::Counter);
        e.handle(EditorEvent::MouseDown { at: pt(10.0, 10.0), shift: false, clicks: 1 });
        pick(&mut e, Tool::Arrow);
        drag(&mut e, pt(10.0, 10.0), pt(200.0, 200.0));
        assert_eq!(e.shapes.len(), limits::MAX_SHAPES);
    }

    #[test]
    fn escape_backs_out_one_step_at_a_time() {
        let mut e = editor();
        pick(&mut e, Tool::Rectangle);
        drag(&mut e, pt(100.0, 100.0), pt(200.0, 180.0));
        pick(&mut e, Tool::Select);
        e.handle(EditorEvent::MouseDown { at: pt(150.0, 100.0), shift: false, clicks: 1 });
        e.handle(EditorEvent::MouseDrag { at: pt(190.0, 140.0), shift: false });
        // Mid-drag: the shape goes back where it was.
        let fx = cmd(&mut e, EditorCommand::Escape);
        assert!(fx.render);
        assert!(matches!(&e.shapes[0], Shape::Rectangle { rect, .. } if *rect == Rect::new(100.0, 100.0, 100.0, 80.0)));
        assert_eq!(e.document().shapes.len(), 1);
        // The mouse-up that follows changes nothing.
        let depth = e.undo.len();
        e.handle(EditorEvent::MouseUp { at: pt(190.0, 140.0), shift: false });
        assert_eq!(e.undo.len(), depth);
        assert_eq!(e.selected, Some(0));
        cmd(&mut e, EditorCommand::Escape);
        assert_eq!(e.selected, None);
        pick(&mut e, Tool::Pen);
        cmd(&mut e, EditorCommand::Escape);
        assert_eq!(e.tool, Tool::Select);
        assert_eq!(cmd(&mut e, EditorCommand::Escape).request, None, "Esc never closes");
    }

    #[test]
    fn counters_number_themselves() {
        let mut e = editor();
        pick(&mut e, Tool::Counter);
        for x in [10.0, 60.0, 110.0] {
            let fx = e.handle(EditorEvent::MouseDown { at: pt(x, 10.0), shift: false, clicks: 1 });
            assert!(fx.render);
            e.handle(EditorEvent::MouseUp { at: pt(x, 10.0), shift: false });
        }
        let numbers: Vec<u32> = e.shapes.iter().map(counter_number).collect();
        assert_eq!(numbers, vec![1, 2, 3]);
        // Deleting the middle one does not renumber, and the next continues.
        pick(&mut e, Tool::Select);
        e.handle(EditorEvent::MouseDown { at: pt(60.0, 10.0), shift: false, clicks: 1 });
        e.handle(EditorEvent::MouseUp { at: pt(60.0, 10.0), shift: false });
        cmd(&mut e, EditorCommand::Delete);
        pick(&mut e, Tool::Counter);
        e.handle(EditorEvent::MouseDown { at: pt(200.0, 10.0), shift: false, clicks: 1 });
        assert_eq!(e.shapes.iter().map(counter_number).collect::<Vec<_>>(), vec![1, 3, 4]);
    }

    #[test]
    fn text_is_typed_in_the_window() {
        let mut e = editor();
        pick(&mut e, Tool::Text);
        let fx = e.handle(EditorEvent::MouseDown { at: pt(40.0, 50.0), shift: false, clicks: 1 });
        let req = fx.begin_text.expect("text field");
        assert_eq!((req.at, req.initial.as_str()), (pt(40.0, 50.0), ""));
        e.handle(EditorEvent::MouseUp { at: pt(40.0, 50.0), shift: false });
        // Cancelled: nothing added.
        assert!(!e.handle(EditorEvent::Text(String::new())).render);
        assert!(e.is_empty());
        // A stray Text event with no field open is ignored.
        assert!(!e.handle(EditorEvent::Text("stray".into())).render);
        assert!(e.is_empty());
        e.handle(EditorEvent::MouseDown { at: pt(40.0, 50.0), shift: false, clicks: 1 });
        assert!(e.handle(EditorEvent::Text("  Look here ".into())).render);
        assert!(matches!(&e.shapes[..], [Shape::Text { at, text, .. }] if *at == pt(40.0, 50.0) && text == "Look here"));

        // Double-click to retype: the old label leaves the image meanwhile.
        pick(&mut e, Tool::Select);
        let fx = e.handle(EditorEvent::MouseDown { at: pt(60.0, 60.0), shift: false, clicks: 2 });
        assert_eq!(fx.begin_text.map(|r| r.initial), Some("Look here".into()));
        assert!(fx.render && e.document().shapes.is_empty());
        e.handle(EditorEvent::MouseUp { at: pt(60.0, 60.0), shift: false });
        // Cancelling keeps the label.
        let fx = e.handle(EditorEvent::Text(String::new()));
        assert!(fx.render && e.document().shapes.len() == 1);
        let depth = e.undo.len();
        e.handle(EditorEvent::MouseDown { at: pt(60.0, 60.0), shift: false, clicks: 2 });
        e.handle(EditorEvent::Text("Changed".into()));
        assert!(matches!(&e.shapes[..], [Shape::Text { text, .. }] if text == "Changed"));
        assert_eq!(e.undo.len(), depth + 1);
    }

    #[test]
    fn colour_and_size_apply_to_new_shapes_and_the_selection() {
        let mut e = editor();
        e.handle(EditorEvent::Color(4));
        e.handle(EditorEvent::Width(2));
        drag(&mut e, pt(10.0, 10.0), pt(110.0, 10.0));
        let Shape::Arrow { style, .. } = &e.shapes[0] else { panic!() };
        assert_eq!(style.color, Some(Color(PALETTE[4])));
        assert_eq!(style.stroke_width, Some(9.0));
        pick(&mut e, Tool::Select);
        e.handle(EditorEvent::MouseDown { at: pt(60.0, 10.0), shift: false, clicks: 1 });
        e.handle(EditorEvent::MouseUp { at: pt(60.0, 10.0), shift: false });
        let depth = e.undo.len();
        assert!(e.handle(EditorEvent::Color(3)).render);
        assert!(e.handle(EditorEvent::Width(0)).render);
        let Shape::Arrow { style, .. } = &e.shapes[0] else { panic!() };
        assert_eq!((style.color, style.stroke_width), (Some(Color(PALETTE[3])), Some(3.0)));
        assert_eq!(e.undo.len(), depth + 2);
        // Picking the colour it already has is not an edit.
        assert!(!e.handle(EditorEvent::Color(3)).render);
        assert_eq!(e.undo.len(), depth + 2);
        // Out-of-range indices are ignored.
        assert_eq!(e.handle(EditorEvent::Color(99)), Effects::default());
        assert_eq!(e.handle(EditorEvent::Tool(99)), Effects::default());
    }

    #[test]
    fn strokes_scale_with_the_image() {
        let small = Editor::new((800, 600));
        let retina = Editor::new((2880, 1800));
        let huge = Editor::new((12000, 3000));
        assert_eq!(small.stroke(), 5.0);
        assert!(retina.stroke() > 10.0 && retina.stroke() < 10.5);
        assert_eq!(huge.stroke(), 15.0);
        assert_eq!(small.font(), 28.0);
    }

    #[test]
    fn keys_pick_tools_and_save_copy_are_requests() {
        let mut e = editor();
        assert!(e.handle(EditorEvent::Key('r')).state);
        assert_eq!(e.tool, Tool::Rectangle);
        assert_eq!(e.handle(EditorEvent::Key('z')), Effects::default());
        assert_eq!(cmd(&mut e, EditorCommand::Save).request, Some(Request::Save));
        assert_eq!(cmd(&mut e, EditorCommand::Copy).request, Some(Request::Copy));
    }

    #[test]
    fn everything_the_editor_draws_renders() {
        let mut e = editor();
        for tool in Tool::ALL {
            pick(&mut e, tool);
            match tool {
                Tool::Select => {}
                Tool::Text => {
                    e.handle(EditorEvent::MouseDown { at: pt(20.0, 20.0), shift: false, clicks: 1 });
                    e.handle(EditorEvent::Text("Hi".into()));
                }
                Tool::Counter => {
                    e.handle(EditorEvent::MouseDown { at: pt(60.0, 60.0), shift: false, clicks: 1 });
                }
                _ => {
                    drag(&mut e, pt(30.0, 30.0), pt(90.0, 80.0));
                }
            }
        }
        // Select and Crop draw no shape; the crop drag cropped.
        assert_eq!(e.shapes.len(), Tool::ALL.len() - 2);
        assert_eq!(e.crop, Some(Rect::new(30.0, 30.0, 60.0, 50.0)));
        let src = image::RgbaImage::from_pixel(120, 100, image::Rgba([200, 200, 200, 255]));
        let out = bugle_markup::render(&src, &e.final_document()).unwrap();
        assert_eq!(out.dimensions(), (60, 50));
        assert_ne!(out, image::imageops::crop_imm(&src, 30, 30, 60, 50).to_image());
    }

    fn menu(e: &mut Editor, menu: usize, item: usize) -> Effects {
        e.handle(EditorEvent::Menu { menu, item })
    }

    fn select_at(e: &mut Editor, at: Point) {
        pick(e, Tool::Select);
        e.handle(EditorEvent::MouseDown { at, shift: false, clicks: 1 });
        e.handle(EditorEvent::MouseUp { at, shift: false });
    }

    #[test]
    fn arrow_styles_come_from_the_style_menu() {
        let mut e = editor();
        // The menu follows the tool.
        assert!(e.menus()[MENU_STYLE].enabled);
        assert_eq!(e.menus()[MENU_STYLE].title, "Standard");
        pick(&mut e, Tool::Pen);
        assert!(!e.menus()[MENU_STYLE].enabled);
        assert_eq!(menu(&mut e, MENU_STYLE, 1), Effects::default(), "no style for the pen");
        pick(&mut e, Tool::Arrow);
        let fx = menu(&mut e, MENU_STYLE, 1);
        assert!(fx.state && !fx.render, "nothing selected: only the next arrow changes");
        assert_eq!(e.menus()[MENU_STYLE].title, "Double-headed");
        assert!(matches!(&e.menus()[MENU_STYLE].items[1], EditorMenuItem::Choice { checked: true, .. }));
        drag(&mut e, pt(10.0, 10.0), pt(110.0, 10.0));
        assert!(matches!(&e.shapes[0], Shape::Arrow { arrow: ArrowStyle::Double, .. }));
        // The overlay draws both heads while dragging one out.
        e.handle(EditorEvent::MouseDown { at: pt(10.0, 50.0), shift: false, clicks: 1 });
        e.handle(EditorEvent::MouseDrag { at: pt(90.0, 50.0), shift: false });
        assert!(matches!(&e.overlay()[..], [EditorPrim::Arrow { double: true, .. }]));
        e.handle(EditorEvent::MouseUp { at: pt(90.0, 50.0), shift: false });
        // Restyle the selected one: one undo step.
        select_at(&mut e, pt(60.0, 10.0));
        assert_eq!(e.menus()[MENU_STYLE].title, "Double-headed");
        let depth = e.undo.len();
        assert!(menu(&mut e, MENU_STYLE, 3).render);
        assert!(matches!(&e.shapes[0], Shape::Arrow { arrow: ArrowStyle::Fancy, .. }));
        assert_eq!(e.undo.len(), depth + 1);
        cmd(&mut e, EditorCommand::Undo);
        assert!(matches!(&e.shapes[0], Shape::Arrow { arrow: ArrowStyle::Double, .. }));
        // Out of range.
        assert_eq!(menu(&mut e, MENU_STYLE, 9), Effects::default());
    }

    #[test]
    fn redaction_styles_come_from_the_style_menu() {
        let mut e = editor();
        pick(&mut e, Tool::Redact);
        assert_eq!(e.menus()[MENU_STYLE].title, "Pixelate");
        menu(&mut e, MENU_STYLE, 2);
        drag(&mut e, pt(100.0, 100.0), pt(200.0, 150.0));
        assert!(matches!(&e.shapes[0], Shape::Redact { mode: RedactStyle::BlackOut, .. }));
        select_at(&mut e, pt(150.0, 125.0));
        assert_eq!(e.menus()[MENU_STYLE].title, "Black out");
        menu(&mut e, MENU_STYLE, 3);
        assert!(matches!(&e.shapes[0], Shape::Redact { mode: RedactStyle::SecureBlur, .. }));
        e.final_document().validate().unwrap();
    }

    #[test]
    fn the_canvas_menu_frames_the_image() {
        let mut e = editor();
        assert!(e.final_document().canvas.is_none());
        assert_eq!(e.menus()[MENU_CANVAS].title, "Canvas");
        // A background on its own brings the default padding.
        let fx = menu(&mut e, MENU_CANVAS, CANVAS_BACKGROUND_ITEM + 3);
        assert!(fx.render && fx.state);
        let canvas = e.final_document().canvas.expect("canvas");
        assert_eq!((canvas.padding, canvas.background), (64, Background::Named("ocean".into())));
        assert_eq!(e.menus()[MENU_CANVAS].title, "Ocean");
        assert!(e.is_dirty() && !e.is_empty());
        // Padding: large, then none (no canvas at all).
        menu(&mut e, MENU_CANVAS, CANVAS_PADDING_ITEM + 3);
        assert_eq!(e.final_document().canvas.map(|c| c.padding), Some(112));
        menu(&mut e, MENU_CANVAS, CANVAS_PADDING_ITEM);
        assert!(e.final_document().canvas.is_none());
        assert!(e.is_empty());
        // Every step undoes.
        cmd(&mut e, EditorCommand::Undo);
        assert_eq!(e.final_document().canvas.map(|c| c.padding), Some(112));
        cmd(&mut e, EditorCommand::Undo);
        cmd(&mut e, EditorCommand::Undo);
        assert!(e.final_document().canvas.is_none() && !e.is_dirty());
        // Headings and separators do nothing; picking what is set is no edit.
        assert_eq!(menu(&mut e, MENU_CANVAS, 0), Effects::default());
        assert_eq!(menu(&mut e, MENU_CANVAS, CANVAS_BACKGROUND_ITEM - 1), Effects::default());
        assert_eq!(menu(&mut e, MENU_CANVAS, 99), Effects::default());
        let depth = e.undo.len();
        assert!(!menu(&mut e, MENU_CANVAS, CANVAS_PADDING_ITEM).render);
        assert_eq!(e.undo.len(), depth);
        // Padding scales with the image, like strokes.
        let mut big = Editor::new((4200, 2400));
        menu(&mut big, MENU_CANVAS, CANVAS_PADDING_ITEM + 1);
        assert_eq!(big.final_document().canvas.map(|c| c.padding), Some(96));
    }

    #[test]
    fn crop_keeps_shapes_in_image_pixels() {
        let mut e = editor();
        pick(&mut e, Tool::Rectangle);
        drag(&mut e, pt(200.0, 200.0), pt(300.0, 260.0));
        // Crop drawn past the image edge: cut to the image, whole pixels.
        let fx = {
            pick(&mut e, Tool::Crop);
            drag(&mut e, pt(100.4, 150.6), pt(900.0, 400.0))
        };
        assert!(fx.state && fx.overlay);
        assert_eq!(e.crop, Some(Rect::new(100.0, 151.0, 700.0, 249.0)));
        // While cropping, the preview is the whole image and the crop is drawn on top.
        assert_eq!(e.document().crop, None);
        assert!(e.overlay().iter().any(|p| matches!(p, EditorPrim::Selection { .. })));
        assert_eq!(e.overlay().iter().filter(|p| matches!(p, EditorPrim::Handle { .. })).count(), 4);
        // Leaving the tool applies it: shapes are moved for the renderer only.
        assert!(e.handle(EditorEvent::Key('v')).render);
        let doc = e.document();
        assert_eq!(doc.crop, e.crop);
        assert!(matches!(&doc.shapes[0], Shape::Rectangle { rect, .. } if *rect == Rect::new(100.0, 49.0, 100.0, 60.0)));
        assert!(matches!(&e.shapes[0], Shape::Rectangle { rect, .. } if *rect == Rect::new(200.0, 200.0, 100.0, 60.0)));
        assert_eq!(doc.source_origin(), pt(-100.0, -151.0));
        // Nothing outside the crop can be picked.
        pick(&mut e, Tool::Line);
        drag(&mut e, pt(10.0, 10.0), pt(90.0, 10.0));
        select_at(&mut e, pt(50.0, 10.0));
        assert_eq!(e.selected, None);
        // The rendered copy is the crop.
        let src = image::RgbaImage::from_pixel(800, 600, image::Rgba([200, 200, 200, 255]));
        let out = bugle_markup::render(&src, &e.final_document()).unwrap();
        assert_eq!(out.dimensions(), (700, 249));
        assert_ne!(*out.get_pixel(100, 49), image::Rgba([200, 200, 200, 255]), "the rectangle's corner");
    }

    #[test]
    fn crop_moves_resizes_and_undoes() {
        let mut e = editor();
        pick(&mut e, Tool::Crop);
        // A click is not a crop.
        drag(&mut e, pt(100.0, 100.0), pt(102.0, 101.0));
        assert_eq!(e.crop, None);
        assert!(!e.state().can_undo);
        drag(&mut e, pt(100.0, 100.0), pt(300.0, 200.0));
        let first = Rect::new(100.0, 100.0, 200.0, 100.0);
        assert_eq!(e.crop, Some(first));
        // Drag from inside moves it, never past the image.
        drag(&mut e, pt(150.0, 150.0), pt(1150.0, 150.0));
        assert_eq!(e.crop, Some(Rect::new(600.0, 100.0, 200.0, 100.0)));
        // A corner resizes it; the opposite corner stays.
        drag(&mut e, pt(600.0, 100.0), pt(500.0, 50.0));
        assert_eq!(e.crop, Some(Rect::new(500.0, 50.0, 300.0, 150.0)));
        // Esc mid-drag puts it back and stays in the crop tool.
        e.handle(EditorEvent::MouseDown { at: pt(10.0, 10.0), shift: false, clicks: 1 });
        e.handle(EditorEvent::MouseDrag { at: pt(400.0, 400.0), shift: false });
        assert_eq!(e.crop, Some(Rect::new(10.0, 10.0, 390.0, 390.0)));
        cmd(&mut e, EditorCommand::Escape);
        assert_eq!(e.crop, Some(Rect::new(500.0, 50.0, 300.0, 150.0)));
        assert_eq!(e.tool, Tool::Crop);
        e.handle(EditorEvent::MouseUp { at: pt(400.0, 400.0), shift: false });
        assert_eq!(e.crop, Some(Rect::new(500.0, 50.0, 300.0, 150.0)));
        // Delete removes it; undo walks back through every step.
        assert!(cmd(&mut e, EditorCommand::Delete).render);
        assert_eq!(e.crop, None);
        cmd(&mut e, EditorCommand::Undo);
        cmd(&mut e, EditorCommand::Undo);
        cmd(&mut e, EditorCommand::Undo);
        assert_eq!(e.crop, Some(first));
        cmd(&mut e, EditorCommand::Undo);
        assert!(e.crop.is_none() && !e.is_dirty());
        // Cropping to the whole image is no crop.
        drag(&mut e, pt(-50.0, -50.0), pt(900.0, 700.0));
        assert_eq!(e.crop, None);
        // Esc leaves the tool and shows the crop.
        drag(&mut e, pt(100.0, 100.0), pt(300.0, 200.0));
        let fx = cmd(&mut e, EditorCommand::Escape);
        assert!(fx.render && e.tool == Tool::Select);
    }

    #[test]
    fn labels_keep_their_lines() {
        let mut e = editor();
        pick(&mut e, Tool::Text);
        e.handle(EditorEvent::MouseDown { at: pt(40.0, 50.0), shift: false, clicks: 1 });
        e.handle(EditorEvent::Text("\n  First\r\nSecond\r\rFourth\u{2028}Fifth \n\n".into()));
        let Shape::Text { text, .. } = &e.shapes[0] else { panic!() };
        assert_eq!(text, "First\nSecond\n\nFourth\nFifth");
        // Selection covers every line: a click on the last line picks it.
        select_at(&mut e, pt(50.0, 50.0 + 4.5 * 28.0 * 1.25));
        assert_eq!(e.selected, Some(0));
        // Retyping starts from the lines as they are.
        let fx = e.handle(EditorEvent::MouseDown { at: pt(50.0, 60.0), shift: false, clicks: 2 });
        assert_eq!(fx.begin_text.map(|t| t.initial), Some("First\nSecond\n\nFourth\nFifth".into()));
    }

    #[test]
    fn a_background_without_padding_is_not_an_unsaved_change() {
        let mut e = editor();
        // A background brings medium padding; taking the padding away again
        // leaves nothing to save, and nothing unsaved.
        menu(&mut e, MENU_CANVAS, CANVAS_BACKGROUND_ITEM + 4);
        assert!(e.is_dirty() && !e.is_empty());
        menu(&mut e, MENU_CANVAS, CANVAS_PADDING_ITEM);
        assert!(e.is_empty());
        assert!(!e.is_dirty() && !e.state().dirty);
        // The background picked is still offered for next time.
        assert_eq!(e.frame.background, 4);
        // And undo goes back to the framed image.
        cmd(&mut e, EditorCommand::Undo);
        assert!(e.final_document().canvas.is_some());
    }

    #[test]
    fn a_canvas_too_large_to_render_is_refused() {
        // 10000 x 9990 is just under 100 megapixels; any padding is too much.
        let mut e = Editor::new((10_000, 9_990));
        let fx = menu(&mut e, MENU_CANVAS, CANVAS_BACKGROUND_ITEM + 2);
        assert!(fx.error.as_deref().is_some_and(|m| m.contains("megapixels")), "{fx:?}");
        assert!(!fx.render && e.final_document().canvas.is_none() && !e.state().can_undo);
        // Cropped smaller, it fits.
        pick(&mut e, Tool::Crop);
        drag(&mut e, pt(0.0, 0.0), pt(5000.0, 5000.0));
        let fx = menu(&mut e, MENU_CANVAS, CANVAS_BACKGROUND_ITEM + 2);
        assert!(fx.error.is_none() && fx.render);
    }

    #[test]
    fn dirty_and_empty_follow_crop_and_canvas() {
        let mut e = editor();
        assert!(e.is_empty() && !e.is_dirty());
        pick(&mut e, Tool::Crop);
        drag(&mut e, pt(100.0, 100.0), pt(300.0, 200.0));
        assert!(!e.is_empty() && e.is_dirty());
        e.mark_saved();
        assert!(!e.is_dirty());
        menu(&mut e, MENU_CANVAS, CANVAS_BACKGROUND_ITEM);
        assert!(e.is_dirty());
        let doc = e.final_document();
        doc.validate().unwrap();
        let src = image::RgbaImage::from_pixel(800, 600, image::Rgba([200, 200, 200, 255]));
        let out = bugle_markup::render(&src, &doc).unwrap();
        assert_eq!(out.dimensions(), (200 + 128, 100 + 128));
        // Transparent background: the padding corner is see-through.
        assert_eq!(out.get_pixel(0, 0).0[3], 0);
    }
}
