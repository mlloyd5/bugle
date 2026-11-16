//! Markup editor logic: what a click, drag or key does to the list of shapes.
//!
//! Nothing here touches AppKit. [`Editor::handle`] takes the window's
//! [`EditorEvent`]s and answers with [`Effects`] telling the glue in
//! `editor_window.rs` what to refresh. Finished shapes are drawn by the same
//! renderer `bugle annotate` uses, so the saved file matches the preview;
//! only the shape under the pointer is drawn by the window, from
//! [`Editor::overlay`].

use bugle_core::{Point, Rect};
use bugle_markup::{limits, Annotation, ArrowStyle, Color, RedactStyle, Shape, Style};
use bugle_platform::{EditorCommand, EditorEvent, EditorPrim, EditorState, EditorTool};

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
}

impl Tool {
    /// Toolbar order.
    pub const ALL: [Tool; 11] = [
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

#[derive(Debug, Clone, PartialEq)]
enum Drag {
    /// Dragging out a new shape with the current tool.
    Draw { start: Point, current: Point, points: Vec<Point> },
    /// Moving the selected shape; `original` is what it was at mouse down.
    Move { index: usize, start: Point, original: Shape, moved: bool },
    /// Dragging one of the selected shape's handles.
    Resize { index: usize, handle: usize, start: Point, original: Shape, moved: bool },
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
    shapes: Vec<Shape>,
    saved: Vec<Shape>,
    undo: Vec<Vec<Shape>>,
    redo: Vec<Vec<Shape>>,
    tool: Tool,
    color: usize,
    size: usize,
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
            shapes: Vec::new(),
            saved: Vec::new(),
            undo: Vec::new(),
            redo: Vec::new(),
            tool: Tool::Arrow,
            color: 0,
            size: 1,
            selected: None,
            drag: None,
            text: None,
            hidden: None,
            px_per_pt: 2.0,
        }
    }

    /// The shapes for the live preview: everything except a shape the window
    /// is drawing itself right now.
    pub fn document(&self) -> Annotation {
        let shapes = self.shapes.iter().enumerate().filter(|(i, _)| Some(*i) != self.hidden).map(|(_, s)| s.clone()).collect();
        Annotation { shapes, ..Default::default() }
    }

    /// Every shape, for saving and copying.
    pub fn final_document(&self) -> Annotation {
        Annotation { shapes: self.shapes.clone(), ..Default::default() }
    }

    pub fn is_empty(&self) -> bool {
        self.shapes.is_empty()
    }

    pub fn is_dirty(&self) -> bool {
        self.shapes != self.saved
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
                self.px_per_pt = s.clamp(0.05, 100.0);
                Effects::default()
            }
        }
    }

    fn pick_tool(&mut self, tool: Tool) -> Effects {
        // A cancelled move put a shape back that the image was drawn without.
        let render = self.cancel_drag();
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
        if self.shapes.len() >= limits::MAX_SHAPES && self.tool != Tool::Select {
            return fx;
        }
        match self.tool {
            Tool::Select => {
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
            Tool::Counter => {
                let next = self.shapes.iter().filter(|s| matches!(s, Shape::Counter { .. })).map(counter_number).max();
                let style = Style { color: Some(self.color()), font_size: Some(self.font()), ..Default::default() };
                let before = self.shapes.clone();
                self.shapes.push(Shape::Counter { at, number: Some(next.unwrap_or(0).saturating_add(1)), style });
                self.commit(before, &mut fx);
            }
            _ => self.drag = Some(Drag::Draw { start: at, current: at, points: vec![at] }),
        }
        fx
    }

    fn mouse_drag(&mut self, at: Point, shift: bool) -> Effects {
        let mut fx = Effects::default();
        let (tool, slack) = (self.tool, 2.0 * self.px_per_pt);
        match &mut self.drag {
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
                    let before = self.shapes.clone();
                    self.shapes.push(shape);
                    self.commit(before, fx);
                }
            }
            Drag::Move { index, original, moved, .. } | Drag::Resize { index, original, moved, .. } => {
                self.hidden = None;
                if moved {
                    let mut before = self.shapes.clone();
                    before[index] = original;
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
                let (from, to) = if c == EditorCommand::Undo { (&mut self.undo, &mut self.redo) } else { (&mut self.redo, &mut self.undo) };
                if let Some(shapes) = from.pop() {
                    to.push(std::mem::replace(&mut self.shapes, shapes));
                    self.selected = None;
                }
                fx.render = true;
            }
            EditorCommand::Delete => {
                fx.render = self.cancel_drag();
                if let Some(index) = self.selected.take() {
                    let before = self.shapes.clone();
                    self.shapes.remove(index);
                    self.commit(before, &mut fx);
                }
                fx.overlay = true;
            }
            EditorCommand::Escape => {
                // Back out one step at a time: the drag, then the selection,
                // then the tool. Never closes the window.
                if self.cancel_drag() {
                    fx.render = true;
                } else if self.selected.take().is_none() && self.tool != Tool::Select {
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
        // No longer than the renderer accepts.
        let typed: String = typed.trim().chars().take(limits::MAX_TEXT_CHARS).collect();
        let typed = typed.trim_end();
        if typed.is_empty() {
            // Cancelled or left blank: an edited label stays as it was.
            return fx;
        }
        let before = self.shapes.clone();
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
            let before = self.shapes.clone();
            change(&mut self.shapes[index]);
            if self.shapes != before {
                self.commit(before, &mut fx);
            }
        }
        fx
    }

    /// Record `before` for undo after `self.shapes` changed.
    fn commit(&mut self, before: Vec<Shape>, fx: &mut Effects) {
        self.undo.push(before);
        if self.undo.len() > UNDO_LIMIT {
            self.undo.remove(0);
        }
        self.redo.clear();
        fx.render = true;
        fx.state = true;
        fx.overlay = true;
    }

    /// The topmost shape under `p`.
    fn hit(&self, p: Point) -> Option<usize> {
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
            Tool::Arrow => long_enough.then(|| Shape::Arrow { from: start, to: end, arrow: ArrowStyle::Standard, style }),
            Tool::Line => long_enough.then(|| Shape::Line { from: start, to: end, style }),
            Tool::Rectangle => big_enough.then(|| Shape::Rectangle { rect, radius: 0.0, style }),
            Tool::Ellipse => big_enough.then(|| Shape::Ellipse { rect, style }),
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
            Tool::Redact => big_enough.then(|| Shape::Redact { rect, mode: RedactStyle::Pixelate, strength: 0.0 }),
            Tool::Spotlight => big_enough.then(|| Shape::Spotlight { rect, dim: 0.6 }),
            Tool::Select | Tool::Text | Tool::Counter => None,
        }
    }

    /// What the window draws above the image: the shape being dragged out
    /// or moved, and the selection marks.
    pub fn overlay(&self) -> Vec<EditorPrim> {
        let mut out = Vec::new();
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
        self.saved = self.shapes.clone();
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
            let widest = text.lines().map(|l| l.chars().count()).max().unwrap_or(0).max(1) as f64;
            let lines = text.lines().count().max(1) as f64;
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
    let mv = |p: &mut Point| {
        p.x += dx;
        p.y += dy;
    };
    let mut s = shape.clone();
    match &mut s {
        Shape::Rectangle { rect, .. }
        | Shape::Ellipse { rect, .. }
        | Shape::Highlight { rect, .. }
        | Shape::Redact { rect, .. }
        | Shape::Spotlight { rect, .. } => {
            rect.x += dx;
            rect.y += dy;
        }
        Shape::Line { from, to, .. } | Shape::Arrow { from, to, .. } => {
            mv(from);
            mv(to);
        }
        Shape::Freehand { points, .. } => points.iter_mut().for_each(mv),
        Shape::Text { at, .. } | Shape::Counter { at, .. } | Shape::Image { at, .. } => mv(at),
    }
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
        | Shape::Spotlight { rect, .. } => {
            let r = normalized(rect);
            vec![
                Point { x: r.x, y: r.y },
                Point { x: r.right(), y: r.y },
                Point { x: r.right(), y: r.bottom() },
                Point { x: r.x, y: r.bottom() },
            ]
        }
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
        Shape::Arrow { from, to, style, .. } => {
            let (color, width) = ink(style);
            vec![EditorPrim::Arrow { from: *from, to: *to, color, width }]
        }
        Shape::Freehand { points, style } => {
            let (color, width) = ink(style);
            vec![EditorPrim::Polyline { points: points.clone(), color, width }]
        }
        Shape::Highlight { rect, style } => {
            let c = style.color.unwrap_or(Color::YELLOW).0;
            vec![EditorPrim::Fill { rect: normalized(rect), color: [c[0], c[1], c[2], 0x66] }]
        }
        Shape::Redact { rect, .. } => vec![
            EditorPrim::Fill { rect: normalized(rect), color: [0x80, 0x80, 0x80, 0xC0] },
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
        assert_eq!(e.shapes.len(), Tool::ALL.len() - 1);
        let src = image::RgbaImage::from_pixel(120, 100, image::Rgba([200, 200, 200, 255]));
        let out = bugle_markup::render(&src, &e.final_document()).unwrap();
        assert_eq!(out.dimensions(), (120, 100));
        assert_ne!(out, src);
    }
}
