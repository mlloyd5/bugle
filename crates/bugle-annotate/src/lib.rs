//! Annotation editor core: a JSON-serialisable list of drawing objects
//! plus a renderer that bakes them
//! into a raster image.
//!
//! Agents send an [`Annotation`] document; humans get the same thing from
//! `bugle annotate --spec`. Everything is in image pixel coordinates.

pub mod color;
pub mod model;
pub mod render;

pub use color::Color;
pub use model::*;
pub use render::{render, Renderer};
