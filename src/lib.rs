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
pub mod cursor;

pub mod api;
mod syntax_text;
mod utility_types;
#[cfg(feature = "serde1")]
mod serde_impls;

pub use sorbus::{
    green::{
        Builder as NodeCache, Checkpoint, Children, ChildrenWithOffsets, Node as GreenNode,
        Token as GreenToken, TreeBuilder as GreenNodeBuilder,
    },
    ArcBorrow, Kind as SyntaxKind, NodeOrToken, TextRange, TextSize,
};

pub use crate::{
    api::{
        Language, SyntaxElement, SyntaxElementChildren, SyntaxNode, SyntaxNodeChildren, SyntaxToken,
    },
    syntax_text::SyntaxText,
    utility_types::{Direction, TokenAtOffset, WalkEvent},
};
