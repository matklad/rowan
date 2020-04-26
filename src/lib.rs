//! A generic library for lossless syntax trees.
//! See `examples/s_expressions.rs` for a tutorial.
#![forbid(
    // missing_debug_implementations,
    unconditional_recursion,
    future_incompatible,
    // missing_docs,
)]
#![deny(unsafe_code)]

#[allow(unsafe_code)]
mod green;
#[allow(unsafe_code)]
pub mod cursor;

pub mod api;
mod syntax_text;
mod utility_types;
#[cfg(feature = "serde")]
mod serde_impls;

// Reexport types for working with strings. We might be too opinionated about
// these, as a custom interner might work better, but `SmolStr` is a pretty good
// default.
pub use smol_str::SmolStr;
pub use text_size::{TextRange, TextSize, TextLen};

pub use crate::{
    api::{
        Language, SyntaxElement, SyntaxElementChildren, SyntaxNode, SyntaxNodeChildren, SyntaxToken,
    },
    green::{Checkpoint, Children, GreenNode, GreenNodeBuilder, GreenToken, SyntaxKind},
    syntax_text::SyntaxText,
    utility_types::{Direction, NodeOrToken, TokenAtOffset, WalkEvent},
};
