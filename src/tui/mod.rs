//! The interactive screen. The one-shot CLI does not come through here.
//!
//! `chatwidget` is the state machine. `app` owns the terminal and the thread
//! that runs the existing agent loop. Colour, markdown, diffs, the composer
//! and the footer are pure and tested without a terminal.

mod app;
mod chatwidget;
mod composer;
mod diff_render;
mod footer;
mod history;
mod markdown;
mod shimmer;
mod styles;

pub use app::{SessionMeta, launch};
