use hashbrown::HashMap;

use crate::{
    green::{GreenElement, GreenNode, GreenToken, SyntaxKind},
    NodeOrToken, SmolStr,
};

#[derive(Default, Debug)]
pub struct NodeCache {
    nodes: HashMap<GreenNode, ()>,
    tokens: HashMap<GreenToken, ()>,
}

impl NodeCache {
    fn node<I>(&mut self, kind: SyntaxKind, children: I) -> GreenNode
    where
        I: IntoIterator<Item = GreenElement>,
        I::IntoIter: ExactSizeIterator,
    {
        let node = GreenNode::new(kind, children);
        // Green nodes are fully immutable, so it's ok to deduplicate them.
        // This is the same optimization that Roslyn does
        // https://github.com/KirillOsenkov/Bliki/wiki/Roslyn-Immutable-Trees
        //
        // For example, all `#[inline]` in this file share the same green node!
        // For `libsyntax/parse/parser.rs`, measurements show that deduping saves
        // 17% of the memory for green nodes!
        // Future work: make hashing faster by avoiding rehashing of subtrees.
        if node.children().len() <= 3 {
            self.nodes.raw_entry_mut().from_key(&node).or_insert(node, ()).0.clone()
        } else {
            node
        }
    }

    fn token(&mut self, kind: SyntaxKind, text: SmolStr) -> GreenToken {
        let token = GreenToken::new(kind, text);
        self.tokens.raw_entry_mut().from_key(&token).or_insert(token, ()).0.clone()
    }

    fn collect_root_nodes(&mut self) -> Vec<GreenNode> {
        // NB: `drain_filter` is `retain` but with an iterator of the removed elements.
        // i.e.: elements where the predicate is FALSE are removed and iterated over.
        self.nodes.drain_filter(|node, ()| node.strong_count() > 1).map(|(node, _)| node).collect()
    }

    fn collect_tokens(&mut self) {
        self.tokens.retain(|token, ()| token.strong_count() > 1)
    }

    /// Garbage collect any elements in this cache that are only held by this cache.
    pub fn gc(&mut self) {
        let mut to_drop = self.collect_root_nodes();

        while let Some(node) = to_drop.pop() {
            if node.strong_count() <= 2 {
                self.nodes.remove(&node);

                // queue children for (potential) removal from the cache
                for child in node.children() {
                    if let Some(node) = child.into_node() {
                        to_drop.push(node.clone());
                    }
                }
            }
        }

        self.collect_tokens();
    }
}

#[derive(Debug)]
enum MaybeOwned<'a, T> {
    Owned(T),
    Borrowed(&'a mut T),
}

impl<T> std::ops::Deref for MaybeOwned<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        match self {
            MaybeOwned::Owned(it) => it,
            MaybeOwned::Borrowed(it) => *it,
        }
    }
}

impl<T> std::ops::DerefMut for MaybeOwned<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        match self {
            MaybeOwned::Owned(it) => it,
            MaybeOwned::Borrowed(it) => *it,
        }
    }
}

impl<T: Default> Default for MaybeOwned<'_, T> {
    fn default() -> Self {
        MaybeOwned::Owned(T::default())
    }
}

/// A checkpoint for maybe wrapping a node. See `GreenNodeBuilder::checkpoint` for details.
#[derive(Clone, Copy, Debug)]
pub struct Checkpoint(usize);

/// A builder for a green tree.
#[derive(Default, Debug)]
pub struct GreenNodeBuilder<'cache> {
    cache: MaybeOwned<'cache, NodeCache>,
    parents: Vec<(SyntaxKind, usize)>,
    children: Vec<GreenElement>,
}

impl GreenNodeBuilder<'_> {
    /// Creates new builder.
    pub fn new() -> GreenNodeBuilder<'static> {
        GreenNodeBuilder::default()
    }

    /// Reusing `NodeCache` between different `GreenNodeBuilder`s saves memory.
    /// It allows to structurally share underlying trees.
    pub fn with_cache(cache: &mut NodeCache) -> GreenNodeBuilder<'_> {
        GreenNodeBuilder {
            cache: MaybeOwned::Borrowed(cache),
            parents: Vec::new(),
            children: Vec::new(),
        }
    }

    /// Adds new token to the current branch.
    #[inline]
    pub fn token(&mut self, kind: SyntaxKind, text: SmolStr) {
        let token = self.cache.token(kind, text);
        self.children.push(token.into());
    }

    /// Start new node and make it current.
    #[inline]
    pub fn start_node(&mut self, kind: SyntaxKind) {
        let len = self.children.len();
        self.parents.push((kind, len));
    }

    /// Finish current branch and restore previous
    /// branch as current.
    #[inline]
    pub fn finish_node(&mut self) {
        let (kind, first_child) = self.parents.pop().unwrap();
        let children = self.children.drain(first_child..);
        let node = self.cache.node(kind, children);
        self.children.push(node.into());
    }

    /// Prepare for maybe wrapping the next node.
    /// The way wrapping works is that you first of all get a checkpoint,
    /// then you place all tokens you want to wrap, and then *maybe* call
    /// `start_node_at`.
    /// Example:
    /// ```rust
    /// # use rowan::{GreenNodeBuilder, SyntaxKind};
    /// # const PLUS: SyntaxKind = SyntaxKind(0);
    /// # const OPERATION: SyntaxKind = SyntaxKind(1);
    /// # struct Parser;
    /// # impl Parser {
    /// #     fn peek(&self) -> Option<SyntaxKind> { None }
    /// #     fn parse_expr(&mut self) {}
    /// # }
    /// # let mut builder = GreenNodeBuilder::new();
    /// # let mut parser = Parser;
    /// let checkpoint = builder.checkpoint();
    /// parser.parse_expr();
    /// if parser.peek() == Some(PLUS) {
    ///   // 1 + 2 = Add(1, 2)
    ///   builder.start_node_at(checkpoint, OPERATION);
    ///   parser.parse_expr();
    ///   builder.finish_node();
    /// }
    /// ```
    #[inline]
    pub fn checkpoint(&self) -> Checkpoint {
        Checkpoint(self.children.len())
    }

    /// Wrap the previous branch marked by `checkpoint` in a new branch and
    /// make it current.
    #[inline]
    pub fn start_node_at(&mut self, checkpoint: Checkpoint, kind: SyntaxKind) {
        let Checkpoint(checkpoint) = checkpoint;
        assert!(
            checkpoint <= self.children.len(),
            "checkpoint no longer valid, was finish_node called early?"
        );

        if let Some(&(_, first_child)) = self.parents.last() {
            assert!(
                checkpoint >= first_child,
                "checkpoint no longer valid, was an unmatched start_node_at called?"
            );
        }

        self.parents.push((kind, checkpoint));
    }

    /// Complete tree building. Make sure that
    /// `start_node_at` and `finish_node` calls
    /// are paired!
    #[inline]
    pub fn finish(mut self) -> GreenNode {
        assert_eq!(self.children.len(), 1);
        match self.children.pop().unwrap() {
            NodeOrToken::Node(node) => node,
            NodeOrToken::Token(_) => panic!(),
        }
    }
}
