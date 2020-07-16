use std::{
    cell::RefCell,
    fmt,
    hash::{Hash, Hasher},
    iter, mem, ptr,
    rc::Rc,
    sync::Arc,
};

use crate::{
    ArcBorrow, Children, Direction, GreenNode, GreenToken, NodeCache, NodeOrToken, SyntaxKind,
    SyntaxText, TextRange, TextSize, TokenAtOffset, WalkEvent,
};

#[derive(Debug, Clone)]
pub struct SyntaxNode(Rc<NodeData>);

impl Drop for SyntaxNode {
    fn drop(&mut self) {
        NodeData::delete(&mut self.0)
    }
}

// Identity semantics for hash & eq
impl PartialEq for SyntaxNode {
    fn eq(&self, other: &SyntaxNode) -> bool {
        ptr::eq(self.green(), other.green())
            && self.text_range().start() == other.text_range().start()
    }
}

impl Eq for SyntaxNode {}

impl Hash for SyntaxNode {
    fn hash<H: Hasher>(&self, state: &mut H) {
        ptr::hash(self.green(), state);
        self.text_range().start().hash(state);
    }
}

impl fmt::Display for SyntaxNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.preorder_with_tokens()
            .filter_map(|event| match event {
                WalkEvent::Enter(SyntaxElement::Token(token)) => Some(token),
                _ => None,
            })
            .try_for_each(|it| fmt::Display::fmt(&it, f))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SyntaxToken {
    parent: SyntaxNode,
    index: u32,
    offset: TextSize,
}

impl fmt::Display for SyntaxToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self.text(), f)
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum SyntaxElement {
    Node(SyntaxNode),
    Token(SyntaxToken),
}

impl From<SyntaxNode> for SyntaxElement {
    fn from(node: SyntaxNode) -> SyntaxElement {
        SyntaxElement::Node(node)
    }
}

impl From<SyntaxToken> for SyntaxElement {
    fn from(token: SyntaxToken) -> SyntaxElement {
        SyntaxElement::Token(token)
    }
}

impl fmt::Display for SyntaxElement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SyntaxElement::Node(it) => fmt::Display::fmt(it, f),
            SyntaxElement::Token(it) => fmt::Display::fmt(it, f),
        }
    }
}

impl SyntaxElement {
    pub fn into_node(self) -> Option<SyntaxNode> {
        match self {
            SyntaxElement::Node(node) => Some(node),
            SyntaxElement::Token(_token) => None,
        }
    }
    pub fn into_token(self) -> Option<SyntaxToken> {
        match self {
            SyntaxElement::Token(token) => Some(token),
            SyntaxElement::Node(_node) => None,
        }
    }
}

#[derive(Debug)]
enum Kind {
    Root(Arc<GreenNode>),
    Child { parent: SyntaxNode, index: u32, offset: TextSize },
    Free { next_free: Option<Rc<NodeData>> },
}

impl Kind {
    fn as_child(&self) -> Option<(&SyntaxNode, u32, TextSize)> {
        match self {
            Kind::Child { parent, index, offset } => Some((parent, *index, *offset)),
            _ => None,
        }
    }
}

#[derive(Debug)]
struct NodeData {
    kind: Kind,
    green: ptr::NonNull<GreenNode>,
}

struct FreeList {
    first_free: Option<Rc<NodeData>>,
    len: usize,
}

const FREE_LIST_LEN: usize = 128;

fn dangling_green_node() -> ptr::NonNull<GreenNode> {
    ptr::NonNull::new(ptr::slice_from_raw_parts_mut(64 as *mut (), 0) as *mut _).unwrap()
}

impl FreeList {
    fn new() -> FreeList {
        let mut res = FreeList { first_free: None, len: 0 };
        for _ in 0..FREE_LIST_LEN {
            res.try_push(&mut Rc::new(NodeData {
                kind: Kind::Free { next_free: None },
                green: dangling_green_node(),
            }))
        }
        res
    }

    fn with<T, F: FnOnce(&mut FreeList) -> T>(f: F) -> T {
        thread_local! {
            static INSTANCE: RefCell<FreeList> = RefCell::new(FreeList::new());
        }
        INSTANCE.with(|it| f(&mut *it.borrow_mut()))
    }

    fn pop(&mut self) -> Option<Rc<NodeData>> {
        let mut node = self.first_free.take()?;
        self.len -= 1;
        {
            let node = Rc::get_mut(&mut node).unwrap();
            self.first_free = match &mut node.kind {
                Kind::Free { next_free } => next_free.take(),
                _ => unreachable!(),
            }
        }
        Some(node)
    }

    fn try_push(&mut self, node: &mut Rc<NodeData>) {
        if self.len >= FREE_LIST_LEN {
            return;
        }
        Rc::get_mut(node).unwrap().kind = Kind::Free { next_free: self.first_free.take() };
        self.first_free = Some(Rc::clone(node));
        self.len += 1;
    }
}

impl NodeData {
    fn new(kind: Kind, green: ptr::NonNull<GreenNode>) -> Rc<NodeData> {
        let mut node = FreeList::with(|it| it.pop()).unwrap_or_else(|| {
            Rc::new(NodeData { kind: Kind::Free { next_free: None }, green: dangling_green_node() })
        });

        {
            let node = Rc::get_mut(&mut node).unwrap();
            node.kind = kind;
            node.green = green;
        }
        node
    }
    fn delete(this: &mut Rc<NodeData>) {
        if let Some(this_mut) = Rc::get_mut(this) {
            // NB: this might drop SyntaxNodes
            this_mut.kind = Kind::Free { next_free: None };
            FreeList::with(|it| it.try_push(this))
        }
    }
}

impl SyntaxNode {
    fn new(data: Rc<NodeData>) -> SyntaxNode {
        SyntaxNode(data)
    }

    pub fn new_root(green: Arc<GreenNode>) -> SyntaxNode {
        let data = NodeData::new(Kind::Root(green), dangling_green_node());
        let mut ret = SyntaxNode::new(data);
        let green: ptr::NonNull<GreenNode> = match &ret.0.kind {
            Kind::Root(green) => ptr::NonNull::new(Arc::as_ptr(green) as *mut _).unwrap(),
            _ => unreachable!(),
        };
        Rc::get_mut(&mut ret.0).unwrap().green = green;
        ret
    }

    // Technically, unsafe, but private so that's OK.
    // Safety: `green` must be a descendent of `parent.green()`
    fn new_child(
        green: &GreenNode,
        parent: SyntaxNode,
        index: u32,
        offset: TextSize,
    ) -> SyntaxNode {
        let data = NodeData::new(Kind::Child { parent, index, offset }, green.into());
        SyntaxNode::new(data)
    }

    /// Returns a green tree, equal to the green tree this node
    /// belongs two, except with this node substitute. The complexity
    /// of operation is proportional to the depth of the tree
    pub fn replace_with(&self, replacement: Arc<GreenNode>) -> Arc<GreenNode> {
        assert_eq!(self.kind(), replacement.kind());
        match self.0.kind.as_child() {
            None => replacement,
            Some((parent, me, _offset)) => {
                let mut replacement = Some(replacement);
                let children: Vec<_> = parent
                    .green()
                    .children()
                    .enumerate()
                    .map(|(i, child)| {
                        if i as u32 == me {
                            replacement.take().unwrap().into()
                        } else {
                            match child {
                                NodeOrToken::Node(node) => {
                                    NodeOrToken::Node(ArcBorrow::upgrade(node))
                                }
                                NodeOrToken::Token(node) => {
                                    NodeOrToken::Token(ArcBorrow::upgrade(node))
                                }
                            }
                        }
                    })
                    .collect();
                let new_parent = NodeCache::new().node(parent.kind(), children);
                parent.replace_with(new_parent)
            }
        }
    }

    pub fn kind(&self) -> SyntaxKind {
        self.green().kind()
    }

    pub fn text_range(&self) -> TextRange {
        let offset = match self.0.kind.as_child() {
            Some((_, _, it)) => it,
            _ => 0.into(),
        };
        TextRange::at(offset, self.green().len())
    }

    pub fn text(&self) -> SyntaxText {
        SyntaxText::new(self.clone())
    }

    pub fn green(&self) -> &GreenNode {
        unsafe { self.0.green.as_ref() }
    }

    pub fn parent(&self) -> Option<SyntaxNode> {
        match &self.0.kind {
            Kind::Root(_) => None,
            Kind::Child { parent, .. } => Some(parent.clone()),
            Kind::Free { .. } => unreachable!(),
        }
    }

    pub fn ancestors(&self) -> impl Iterator<Item = SyntaxNode> {
        iter::successors(Some(self.clone()), SyntaxNode::parent)
    }

    pub fn children(&self) -> SyntaxNodeChildren {
        SyntaxNodeChildren::new(self.clone())
    }

    pub fn children_with_tokens(&self) -> SyntaxElementChildren {
        SyntaxElementChildren::new(self.clone())
    }

    #[inline]
    pub fn first_child(&self) -> Option<SyntaxNode> {
        let (node, (index, offset)) =
            filter_nodes(children_from(self.green(), 0, self.text_range().start())).next()?;

        Some(SyntaxNode::new_child(node, self.clone(), index as u32, offset))
    }

    pub fn first_child_or_token(&self) -> Option<SyntaxElement> {
        let (element, (index, offset)) =
            children_from(self.green(), 0, self.text_range().start()).next()?;
        Some(SyntaxElement::new(element, self.clone(), index as u32, offset))
    }

    #[inline]
    pub fn last_child(&self) -> Option<SyntaxNode> {
        let (node, (index, offset)) = filter_nodes(children_to(
            self.green(),
            self.green().children().len(),
            self.text_range().end(),
        ))
        .next()?;

        Some(SyntaxNode::new_child(node, self.clone(), index as u32, offset))
    }

    pub fn last_child_or_token(&self) -> Option<SyntaxElement> {
        let (element, (index, offset)) =
            children_to(self.green(), self.green().children().len(), self.text_range().end())
                .next()?;
        Some(SyntaxElement::new(element, self.clone(), index as u32, offset))
    }

    pub fn next_sibling(&self) -> Option<SyntaxNode> {
        let (parent, index, _) = self.0.kind.as_child()?;

        let (node, (index, offset)) = filter_nodes(children_from(
            parent.green(),
            (index + 1) as usize,
            self.text_range().end(),
        ))
        .next()?;

        Some(SyntaxNode::new_child(node, parent.clone(), index as u32, offset))
    }

    pub fn next_sibling_or_token(&self) -> Option<SyntaxElement> {
        let (parent, index, _) = self.0.kind.as_child()?;

        let (element, (index, offset)) =
            children_from(parent.green(), (index + 1) as usize, self.text_range().end()).next()?;

        Some(SyntaxElement::new(element, parent.clone(), index as u32, offset))
    }

    pub fn prev_sibling(&self) -> Option<SyntaxNode> {
        let (parent, index, _) = self.0.kind.as_child()?;

        let (node, (index, offset)) =
            filter_nodes(children_to(parent.green(), index as usize, self.text_range().start()))
                .next()?;

        Some(SyntaxNode::new_child(node, parent.clone(), index as u32, offset))
    }

    pub fn prev_sibling_or_token(&self) -> Option<SyntaxElement> {
        let (parent, index, _) = self.0.kind.as_child()?;

        let (element, (index, offset)) =
            children_to(parent.green(), index as usize, self.text_range().start()).next()?;

        Some(SyntaxElement::new(element, parent.clone(), index as u32, offset))
    }

    /// Return the leftmost token in the subtree of this node
    #[inline]
    pub fn first_token(&self) -> Option<SyntaxToken> {
        self.first_child_or_token()?.first_token()
    }

    /// Return the rightmost token in the subtree of this node
    #[inline]
    pub fn last_token(&self) -> Option<SyntaxToken> {
        self.last_child_or_token()?.last_token()
    }

    pub fn siblings(&self, direction: Direction) -> impl Iterator<Item = SyntaxNode> {
        iter::successors(Some(self.clone()), move |node| match direction {
            Direction::Next => node.next_sibling(),
            Direction::Prev => node.prev_sibling(),
        })
    }

    pub fn siblings_with_tokens(
        &self,
        direction: Direction,
    ) -> impl Iterator<Item = SyntaxElement> {
        let me: SyntaxElement = self.clone().into();
        iter::successors(Some(me), move |el| match direction {
            Direction::Next => el.next_sibling_or_token(),
            Direction::Prev => el.prev_sibling_or_token(),
        })
    }

    pub fn descendants(&self) -> impl Iterator<Item = SyntaxNode> {
        self.preorder().filter_map(|event| match event {
            WalkEvent::Enter(node) => Some(node),
            WalkEvent::Leave(_) => None,
        })
    }

    pub fn descendants_with_tokens(&self) -> impl Iterator<Item = SyntaxElement> {
        self.preorder_with_tokens().filter_map(|event| match event {
            WalkEvent::Enter(it) => Some(it),
            WalkEvent::Leave(_) => None,
        })
    }

    /// Traverse the subtree rooted at the current node (including the current
    /// node) in preorder, excluding tokens.
    #[inline]
    pub fn preorder(&self) -> impl Iterator<Item = WalkEvent<SyntaxNode>> {
        let this = self.clone();
        iter::successors(Some(WalkEvent::Enter(self.clone())), move |pos| {
            let next = match pos {
                WalkEvent::Enter(node) => match node.first_child() {
                    Some(child) => WalkEvent::Enter(child),
                    None => WalkEvent::Leave(node.clone()),
                },
                WalkEvent::Leave(node) => {
                    if node == &this {
                        return None;
                    }
                    match node.next_sibling() {
                        Some(sibling) => WalkEvent::Enter(sibling),
                        None => WalkEvent::Leave(node.parent().unwrap()),
                    }
                }
            };
            Some(next)
        })
    }

    /// Traverse the subtree rooted at the current node (including the current
    /// node) in preorder, including tokens.
    #[inline]
    pub fn preorder_with_tokens<'a>(&'a self) -> impl Iterator<Item = WalkEvent<SyntaxElement>> {
        let start: SyntaxElement = self.clone().into();
        iter::successors(Some(WalkEvent::Enter(start.clone())), move |pos| {
            let next = match pos {
                WalkEvent::Enter(el) => match el {
                    SyntaxElement::Node(node) => match node.first_child_or_token() {
                        Some(child) => WalkEvent::Enter(child),
                        None => WalkEvent::Leave(node.clone().into()),
                    },
                    SyntaxElement::Token(token) => WalkEvent::Leave(token.clone().into()),
                },
                WalkEvent::Leave(el) => {
                    if el == &start {
                        return None;
                    }
                    match el.next_sibling_or_token() {
                        Some(sibling) => WalkEvent::Enter(sibling),
                        None => WalkEvent::Leave(el.parent().unwrap().into()),
                    }
                }
            };
            Some(next)
        })
    }

    /// Find a token in the subtree corresponding to this node, which covers the offset.
    /// Precondition: offset must be withing node's range.
    pub fn token_at_offset(&self, offset: TextSize) -> TokenAtOffset<SyntaxToken> {
        // TODO: this could be faster if we first drill-down to node, and only
        // then switch to token search. We should also replace explicit
        // recursion with a loop.
        let range = self.text_range();
        assert!(
            range.start() <= offset && offset <= range.end(),
            "Bad offset: range {:?} offset {:?}",
            range,
            offset
        );
        if range.is_empty() {
            return TokenAtOffset::None;
        }

        let mut children = self.children_with_tokens().filter(|child| {
            let child_range = child.text_range();
            !child_range.is_empty()
                && (child_range.start() <= offset && offset <= child_range.end())
        });

        let left = children.next().unwrap();
        let right = children.next();
        assert!(children.next().is_none());

        if let Some(right) = right {
            match (left.token_at_offset(offset), right.token_at_offset(offset)) {
                (TokenAtOffset::Single(left), TokenAtOffset::Single(right)) => {
                    TokenAtOffset::Between(left, right)
                }
                _ => unreachable!(),
            }
        } else {
            left.token_at_offset(offset)
        }
    }

    /// Return the deepest node or token in the current subtree that fully
    /// contains the range. If the range is empty and is contained in two leaf
    /// nodes, either one can be returned. Precondition: range must be contained
    /// withing the current node
    pub fn covering_element(&self, range: TextRange) -> SyntaxElement {
        let mut res: SyntaxElement = self.clone().into();
        loop {
            assert!(
                res.text_range().contains_range(range),
                "Bad range: node range {:?}, range {:?}",
                res.text_range(),
                range,
            );
            res = match &res {
                SyntaxElement::Token(_) => return res,
                SyntaxElement::Node(node) => {
                    match node
                        .children_with_tokens()
                        .find(|child| child.text_range().contains_range(range))
                    {
                        Some(child) => child,
                        None => return res,
                    }
                }
            };
        }
    }
}

impl SyntaxToken {
    fn new(parent: SyntaxNode, index: u32, offset: TextSize) -> SyntaxToken {
        SyntaxToken { parent, index, offset }
    }

    /// Returns a green tree, equal to the green tree this token
    /// belongs two, except with this token substitute. The complexity
    /// of operation is proportional to the depth of the tree
    pub fn replace_with(&self, replacement: Arc<GreenToken>) -> Arc<GreenNode> {
        assert_eq!(self.kind(), replacement.kind());
        let mut replacement = Some(replacement);
        let parent = self.parent();
        let me = self.index;

        let children: Vec<_> = parent
            .green()
            .children()
            .enumerate()
            .map(|(i, child)| {
                if i as u32 == me {
                    replacement.take().unwrap().into()
                } else {
                    match child {
                        NodeOrToken::Node(node) => NodeOrToken::Node(ArcBorrow::upgrade(node)),
                        NodeOrToken::Token(token) => NodeOrToken::Token(ArcBorrow::upgrade(token)),
                    }
                }
            })
            .collect();
        let new_parent = NodeCache::new().node(parent.kind(), children);
        parent.replace_with(new_parent)
    }

    pub fn kind(&self) -> SyntaxKind {
        self.green().kind()
    }

    pub fn text_range(&self) -> TextRange {
        TextRange::at(self.offset, self.green().len())
    }

    pub fn text(&self) -> &str {
        self.green().text()
    }

    pub fn green(&self) -> &GreenToken {
        let token =
            self.parent.green().children().nth(self.index as usize).unwrap().into_token().unwrap();
        ArcBorrow::downgrade(token)
    }

    pub fn parent(&self) -> SyntaxNode {
        self.parent.clone()
    }

    pub fn ancestors(&self) -> impl Iterator<Item = SyntaxNode> {
        self.parent().ancestors()
    }

    pub fn next_sibling_or_token(&self) -> Option<SyntaxElement> {
        let (element, (index, offset)) =
            children_from(self.parent.green(), (self.index + 1) as usize, self.text_range().end())
                .next()?;

        Some(SyntaxElement::new(element, self.parent(), index as u32, offset))
    }

    pub fn prev_sibling_or_token(&self) -> Option<SyntaxElement> {
        let parent = self.parent();
        let (element, (index, offset)) =
            children_to(self.parent.green(), self.index as usize, self.text_range().start())
                .next()?;

        Some(SyntaxElement::new(element, parent, index as u32, offset))
    }

    pub fn siblings_with_tokens(
        &self,
        direction: Direction,
    ) -> impl Iterator<Item = SyntaxElement> {
        let me: SyntaxElement = self.clone().into();
        iter::successors(Some(me), move |el| match direction {
            Direction::Next => el.next_sibling_or_token(),
            Direction::Prev => el.prev_sibling_or_token(),
        })
    }

    /// Next token in the tree (i.e, not necessary a sibling)
    pub fn next_token(&self) -> Option<SyntaxToken> {
        match self.next_sibling_or_token() {
            Some(element) => element.first_token(),
            None => self
                .parent()
                .ancestors()
                .find_map(|it| it.next_sibling_or_token())
                .and_then(|element| element.first_token()),
        }
    }
    /// Previous token in the tree (i.e, not necessary a sibling)
    pub fn prev_token(&self) -> Option<SyntaxToken> {
        match self.prev_sibling_or_token() {
            Some(element) => element.last_token(),
            None => self
                .parent()
                .ancestors()
                .find_map(|it| it.prev_sibling_or_token())
                .and_then(|element| element.last_token()),
        }
    }
}

impl SyntaxElement {
    fn new(
        element: NodeOrToken<&GreenNode, &GreenToken>,
        parent: SyntaxNode,
        index: u32,
        offset: TextSize,
    ) -> SyntaxElement {
        match element {
            NodeOrToken::Node(node) => {
                SyntaxNode::new_child(node, parent, index as u32, offset).into()
            }
            NodeOrToken::Token(_) => SyntaxToken::new(parent, index as u32, offset).into(),
        }
    }

    pub fn text_range(&self) -> TextRange {
        match self {
            SyntaxElement::Node(it) => it.text_range(),
            SyntaxElement::Token(it) => it.text_range(),
        }
    }

    pub fn kind(&self) -> SyntaxKind {
        match self {
            SyntaxElement::Node(it) => it.kind(),
            SyntaxElement::Token(it) => it.kind(),
        }
    }

    pub fn parent(&self) -> Option<SyntaxNode> {
        match self {
            SyntaxElement::Node(it) => it.parent(),
            SyntaxElement::Token(it) => Some(it.parent()),
        }
    }

    pub fn ancestors(&self) -> impl Iterator<Item = SyntaxNode> {
        match self {
            SyntaxElement::Node(it) => it.ancestors(),
            SyntaxElement::Token(it) => it.parent().ancestors(),
        }
    }

    #[inline]
    pub fn first_token(&self) -> Option<SyntaxToken> {
        match self {
            SyntaxElement::Node(it) => it.first_token(),
            SyntaxElement::Token(it) => Some(it.clone()),
        }
    }

    #[inline]
    pub fn last_token(&self) -> Option<SyntaxToken> {
        match self {
            SyntaxElement::Node(it) => it.last_token(),
            SyntaxElement::Token(it) => Some(it.clone()),
        }
    }

    pub fn next_sibling_or_token(&self) -> Option<SyntaxElement> {
        match self {
            SyntaxElement::Node(it) => it.next_sibling_or_token(),
            SyntaxElement::Token(it) => it.next_sibling_or_token(),
        }
    }

    pub fn prev_sibling_or_token(&self) -> Option<SyntaxElement> {
        match self {
            SyntaxElement::Node(it) => it.prev_sibling_or_token(),
            SyntaxElement::Token(it) => it.prev_sibling_or_token(),
        }
    }

    fn token_at_offset(&self, offset: TextSize) -> TokenAtOffset<SyntaxToken> {
        assert!(self.text_range().start() <= offset && offset <= self.text_range().end());
        match self {
            SyntaxElement::Token(token) => TokenAtOffset::Single(token.clone()),
            SyntaxElement::Node(node) => node.token_at_offset(offset),
        }
    }
}

#[derive(Clone, Debug)]
struct Iter {
    parent: SyntaxNode,
    green: Children<'static>,
    offset: TextSize,
    index: u32,
}

impl Iter {
    fn new(parent: SyntaxNode) -> Iter {
        let offset = parent.text_range().start();
        let green: Children<'_> = parent.green().children();
        // Dirty lifetime laundering: the memory for the children is
        // indirectly owned by parent.
        let green: Children<'static> =
            unsafe { mem::transmute::<Children<'_>, Children<'static>>(green) };
        Iter { parent, green, offset, index: 0 }
    }

    fn next(
        &mut self,
    ) -> Option<(NodeOrToken<ArcBorrow<'_, GreenNode>, ArcBorrow<'_, GreenToken>>, u32, TextSize)>
    {
        self.green.next().map(|element| {
            let offset = self.offset;
            let index = self.index;
            self.offset += element.len();
            self.index += 1;
            (element, index, offset)
        })
    }
}

#[derive(Clone, Debug)]
pub struct SyntaxNodeChildren(Iter);

impl SyntaxNodeChildren {
    fn new(parent: SyntaxNode) -> SyntaxNodeChildren {
        SyntaxNodeChildren(Iter::new(parent))
    }
}

impl Iterator for SyntaxNodeChildren {
    type Item = SyntaxNode;
    fn next(&mut self) -> Option<Self::Item> {
        let parent = self.0.parent.clone();
        while let Some((element, index, offset)) = self.0.next() {
            if let Some(node) = element.as_node() {
                return Some(SyntaxNode::new_child(node, parent, index, offset));
            }
        }
        None
    }
}

#[derive(Clone, Debug)]
pub struct SyntaxElementChildren(Iter);

impl SyntaxElementChildren {
    fn new(parent: SyntaxNode) -> SyntaxElementChildren {
        SyntaxElementChildren(Iter::new(parent))
    }
}

impl Iterator for SyntaxElementChildren {
    type Item = SyntaxElement;
    fn next(&mut self) -> Option<Self::Item> {
        let parent = self.0.parent.clone();
        self.0.next().map(|(green, index, offset)| {
            SyntaxElement::new(green.as_deref(), parent, index, offset)
        })
    }
}

fn children_from(
    node: &GreenNode,
    start_index: usize,
    mut offset: TextSize,
) -> impl Iterator<Item = (NodeOrToken<&'_ GreenNode, &'_ GreenToken>, (usize, TextSize))> {
    node.children().skip(start_index).enumerate().map(move |(index, element)| {
        let element_offset = offset;
        offset += element.len();
        (
            match element {
                NodeOrToken::Node(node) => NodeOrToken::Node(ArcBorrow::downgrade(node)),
                NodeOrToken::Token(token) => NodeOrToken::Token(ArcBorrow::downgrade(token)),
            },
            (start_index + index, element_offset),
        )
    })
}

fn children_to(
    node: &GreenNode,
    end_index: usize,
    mut offset: TextSize,
) -> impl Iterator<Item = (NodeOrToken<&'_ GreenNode, &'_ GreenToken>, (usize, TextSize))> {
    node.children().take(end_index).rev().enumerate().map(move |(index, element)| {
        offset -= element.len();
        (
            match element {
                NodeOrToken::Node(node) => NodeOrToken::Node(ArcBorrow::downgrade(node)),
                NodeOrToken::Token(token) => NodeOrToken::Token(ArcBorrow::downgrade(token)),
            },
            (end_index - index - 1, offset),
        )
    })
}

fn filter_nodes<'a, I: Iterator<Item = (NodeOrToken<&'a GreenNode, &'a GreenToken>, T)>, T>(
    iter: I,
) -> impl Iterator<Item = (&'a GreenNode, T)> {
    iter.filter_map(|(element, data)| match element {
        NodeOrToken::Node(it) => Some((it, data)),
        NodeOrToken::Token(_) => None,
    })
}
