use std::{
    borrow::Borrow,
    fmt,
    iter::FusedIterator,
    mem::{self, ManuallyDrop},
    ops, ptr, slice,
};

use crate::{
    arc::{self, Arc, HeaderSlice, ThinArc},
    green::{GreenElement, GreenElementRef, SyntaxKind},
    utility_types::static_assert,
    GreenToken, NodeOrToken, TextRange, TextSize,
};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct GreenNodeHead {
    kind: SyntaxKind,
    text_len: TextSize,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum GreenChild {
    Node { offset_in_parent: TextSize, node: GreenNode },
    Token { offset_in_parent: TextSize, token: GreenToken },
}
#[cfg(target_pointer_width = "64")]
static_assert!(mem::size_of::<GreenChild>() == mem::size_of::<usize>() * 2);

type Repr = HeaderSlice<GreenNodeHead, [GreenChild]>;
type ReprThin = HeaderSlice<GreenNodeHead, [GreenChild; 0]>;
#[repr(transparent)]
pub struct GreenNodeData {
    data: ReprThin,
}

/// Internal node in the immutable tree.
/// It has other nodes and tokens as children.
#[derive(Clone, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct GreenNode {
    ptr: ThinArc<GreenNodeHead, GreenChild>,
}

impl ToOwned for GreenNodeData {
    type Owned = GreenNode;

    #[inline]
    fn to_owned(&self) -> GreenNode {
        unsafe {
            let green = GreenNode::from_raw(ptr::NonNull::from(self));
            let green = ManuallyDrop::new(green);
            GreenNode::clone(&green)
        }
    }
}

impl Borrow<GreenNodeData> for GreenNode {
    #[inline]
    fn borrow(&self) -> &GreenNodeData {
        &*self
    }
}

impl fmt::Debug for GreenNodeData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GreenNode")
            .field("kind", &self.kind())
            .field("text_len", &self.text_len())
            .field("n_children", &self.children().len())
            .finish()
    }
}

impl fmt::Debug for GreenNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let data: &GreenNodeData = &*self;
        fmt::Debug::fmt(data, f)
    }
}

impl GreenNodeData {
    #[inline]
    fn repr(&self) -> &Repr {
        unsafe { &*arc::thin_to_thick_ref(&self.data as *const _ as *mut _) }
    }

    #[inline]
    fn header(&self) -> &GreenNodeHead {
        &self.data.header
    }

    #[inline]
    fn slice(&self) -> &[GreenChild] {
        &self.repr().slice
    }

    /// Kind of this node.
    #[inline]
    pub fn kind(&self) -> SyntaxKind {
        self.header().kind
    }

    /// Returns the length of the text covered by this node.
    #[inline]
    pub fn text_len(&self) -> TextSize {
        self.header().text_len
    }

    /// Children of this node.
    #[inline]
    pub fn children(&self) -> Children<'_> {
        Children { inner: self.slice().iter() }
    }

    pub(crate) fn child_at_range(
        &self,
        range: TextRange,
    ) -> Option<(usize, TextSize, GreenElementRef<'_>)> {
        let idx = self
            .slice()
            .binary_search_by(|it| {
                let child_range = it.range_in_parent();
                TextRange::ordering(child_range, range)
            })
            // XXX: this handles empty ranges
            .unwrap_or_else(|it| it.saturating_sub(1));
        let child =
            &self.slice().get(idx).filter(|it| it.range_in_parent().contains_range(range))?;
        Some((idx, child.offset_in_parent(), child.as_ref()))
    }

    pub(crate) fn replace_child(&self, idx: usize, new_child: GreenElement) -> GreenNode {
        let mut replacement = Some(new_child);
        let children = self.children().enumerate().map(|(i, child)| {
            if i == idx {
                replacement.take().unwrap()
            } else {
                child.cloned()
            }
        });
        GreenNode::new(self.kind(), children)
    }
}

impl ops::Deref for GreenNode {
    type Target = GreenNodeData;

    #[inline]
    fn deref(&self) -> &GreenNodeData {
        unsafe {
            let repr: &Repr = &self.ptr;
            let repr: &ReprThin = &*(repr as *const Repr as *const ReprThin);
            mem::transmute::<&ReprThin, &GreenNodeData>(repr)
        }
    }
}

impl GreenNode {
    /// Creates new Node.
    #[inline]
    pub fn new<I>(kind: SyntaxKind, children: I) -> GreenNode
    where
        I: IntoIterator<Item = GreenElement>,
        I::IntoIter: ExactSizeIterator,
    {
        let mut text_len: TextSize = 0.into();
        let children = children.into_iter().map(|el| {
            let offset_in_parent = text_len;
            text_len += el.text_len();
            match el {
                NodeOrToken::Node(node) => GreenChild::Node { offset_in_parent, node },
                NodeOrToken::Token(token) => GreenChild::Token { offset_in_parent, token },
            }
        });

        let data =
            ThinArc::from_header_and_iter(GreenNodeHead { kind, text_len: 0.into() }, children);

        // XXX: fixup `text_len` after construction, because we can't iterate
        // `children` twice.
        let data = {
            let mut data = Arc::from_thin(data);
            Arc::get_mut(&mut data).unwrap().header.text_len = text_len;
            Arc::into_thin(data)
        };

        GreenNode { ptr: data }
    }

    #[inline]
    pub(crate) fn into_raw(this: GreenNode) -> ptr::NonNull<GreenNodeData> {
        let green = ManuallyDrop::new(this);
        let green: &GreenNodeData = &*green;
        ptr::NonNull::from(&*green)
    }

    #[inline]
    pub(crate) unsafe fn from_raw(ptr: ptr::NonNull<GreenNodeData>) -> GreenNode {
        let arc = Arc::from_raw(&ptr.as_ref().data as *const ReprThin);
        let arc = mem::transmute::<Arc<ReprThin>, ThinArc<GreenNodeHead, GreenChild>>(arc);
        GreenNode { ptr: arc }
    }
}

impl GreenChild {
    #[inline]
    fn as_ref(&self) -> GreenElementRef {
        match self {
            GreenChild::Node { node, .. } => NodeOrToken::Node(node),
            GreenChild::Token { token, .. } => NodeOrToken::Token(token),
        }
    }
    #[inline]
    fn offset_in_parent(&self) -> TextSize {
        match self {
            GreenChild::Node { offset_in_parent, .. }
            | GreenChild::Token { offset_in_parent, .. } => *offset_in_parent,
        }
    }
    #[inline]
    fn range_in_parent(&self) -> TextRange {
        let len = self.as_ref().text_len();
        TextRange::at(self.offset_in_parent(), len)
    }
}

#[derive(Debug, Clone)]
pub struct Children<'a> {
    inner: slice::Iter<'a, GreenChild>,
}

// NB: forward everything stable that iter::Slice specializes as of Rust 1.39.0
impl ExactSizeIterator for Children<'_> {
    #[inline(always)]
    fn len(&self) -> usize {
        self.inner.len()
    }
}

impl<'a> Iterator for Children<'a> {
    type Item = GreenElementRef<'a>;

    #[inline]
    fn next(&mut self) -> Option<GreenElementRef<'a>> {
        self.inner.next().map(GreenChild::as_ref)
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }

    #[inline]
    fn count(self) -> usize
    where
        Self: Sized,
    {
        self.inner.count()
    }

    #[inline]
    fn nth(&mut self, n: usize) -> Option<Self::Item> {
        self.inner.nth(n).map(GreenChild::as_ref)
    }

    #[inline]
    fn last(mut self) -> Option<Self::Item>
    where
        Self: Sized,
    {
        self.next_back()
    }

    #[inline]
    fn fold<Acc, Fold>(mut self, init: Acc, mut f: Fold) -> Acc
    where
        Fold: FnMut(Acc, Self::Item) -> Acc,
    {
        let mut accum = init;
        while let Some(x) = self.next() {
            accum = f(accum, x);
        }
        accum
    }
}

impl<'a> DoubleEndedIterator for Children<'a> {
    #[inline]
    fn next_back(&mut self) -> Option<Self::Item> {
        self.inner.next_back().map(GreenChild::as_ref)
    }

    #[inline]
    fn nth_back(&mut self, n: usize) -> Option<Self::Item> {
        self.inner.nth_back(n).map(GreenChild::as_ref)
    }

    #[inline]
    fn rfold<Acc, Fold>(mut self, init: Acc, mut f: Fold) -> Acc
    where
        Fold: FnMut(Acc, Self::Item) -> Acc,
    {
        let mut accum = init;
        while let Some(x) = self.next_back() {
            accum = f(accum, x);
        }
        accum
    }
}

impl FusedIterator for Children<'_> {}
