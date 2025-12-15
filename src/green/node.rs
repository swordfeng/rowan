use std::{
    borrow::{Borrow, Cow},
    fmt,
    hash::Hash,
    iter::{self, FusedIterator},
    mem::{self, ManuallyDrop},
    ops, option, ptr, slice,
};

use countme::Count;

use crate::{
    arc::{Arc, HeaderSlice, ThinArc},
    utility_types::static_assert,
    GreenToken, NodeOrToken, TextRange, TextSize,
    green::{
        GreenElement, GreenElementRef, SyntaxKind,
        untagged_element::{ElementTag, UntaggedElement},
    },
};

pub(super) struct GreenNodeHead {
    kind: SyntaxKind,
    text_len: TextSize,
    first_tag: ElementTag,
    first_ptr: Option<UntaggedElement>,
    _c: Count<GreenNode>,
}

impl fmt::Debug for GreenNodeHead {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GreenNodeHead")
            .field("kind", &self.kind)
            .field("text_len", &self.text_len)
            .field("first_element", &self.first_ref().map(|el| el.to_owned()))
            .finish()
    }
}
impl Clone for GreenNodeHead {
    fn clone(&self) -> Self {
        let first_ptr = self.first_ref().map(|el| UntaggedElement::from_element(el.to_owned()).1);
        Self {
            kind: self.kind,
            text_len: self.text_len,
            first_tag: self.first_tag,
            first_ptr,
            _c: self._c.clone(),
        }
    }
}
impl PartialEq for GreenNodeHead {
    fn eq(&self, other: &Self) -> bool {
        self.kind == other.kind
            && self.text_len == other.text_len
            && self.first_ref() == other.first_ref()
    }
}
impl Eq for GreenNodeHead {}
impl Hash for GreenNodeHead {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.kind.hash(state);
        self.text_len.hash(state);
        self.first_ref().hash(state);
        self._c.hash(state);
    }
}

impl GreenNodeHead {
    fn first_ref(&self) -> Option<GreenElementRef<'_>> {
        // SAFETY: self.first_ptr is created from element_to_raw (if present)
        self.first_ptr.as_ref().map(|p| unsafe { p.as_element_ref(self.first_tag) })
    }
}

impl Drop for GreenNodeHead {
    fn drop(&mut self) {
        // drop first element
        // SAFETY: self.first_ptr is created from element_to_raw (if present)
        let _ = self.first_ptr.take().map(|p| unsafe { p.into_element(self.first_tag) });
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum GreenChild {
    Node { rel_offset: TextSize, node: GreenNode },
    Token { rel_offset: TextSize, token: GreenToken },
}
#[cfg(target_pointer_width = "64")]
static_assert!(mem::size_of::<GreenChild>() == mem::size_of::<usize>() * 2);

type Repr = HeaderSlice<GreenNodeHead, [GreenChild]>;
type ReprThin = HeaderSlice<GreenNodeHead, [GreenChild; 0]>;
#[repr(transparent)]
pub struct GreenNodeData {
    data: ReprThin,
}

impl PartialEq for GreenNodeData {
    fn eq(&self, other: &Self) -> bool {
        self.header() == other.header() && self.slice() == other.slice()
    }
}

impl Hash for GreenNodeData {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.header().hash(state);
        self.slice().hash(state);
    }
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

impl From<Cow<'_, GreenNodeData>> for GreenNode {
    #[inline]
    fn from(cow: Cow<'_, GreenNodeData>) -> Self {
        cow.into_owned()
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

impl fmt::Display for GreenNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let data: &GreenNodeData = &*self;
        fmt::Display::fmt(data, f)
    }
}

impl fmt::Display for GreenNodeData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for child in self.children() {
            write!(f, "{}", child)?;
        }
        Ok(())
    }
}

impl GreenNodeData {
    #[inline]
    fn header(&self) -> &GreenNodeHead {
        &self.data.header
    }

    // Note: this does not contain first element
    #[inline]
    fn slice(&self) -> &[GreenChild] {
        self.data.slice()
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
        self.children_ext().map(|(el, _)| el)
    }

    #[inline]
    pub(crate) fn children_ext<'a>(&'a self) -> ChildrenExt<'a> {
        ChildrenExt::create(self.header().first_ref(), self.slice().iter())
    }

    pub(crate) fn child_at_range(
        &self,
        rel_range: TextRange,
    ) -> Option<(usize, TextSize, GreenElementRef<'_>)> {
        // first check first element
        if let Some(first_el) = self.header().first_ref() {
            if TextRange::new(0.into(), first_el.text_len()).contains_range(rel_range) {
                return Some((0, 0.into(), first_el));
            }
        }
        let idx = self
            .slice()
            .binary_search_by(|it| {
                let child_range = it.rel_range();
                TextRange::ordering(child_range, rel_range)
            })
            // XXX: this handles empty ranges
            .unwrap_or_else(|it| it.saturating_sub(1));
        let child = &self.slice().get(idx).filter(|it| it.rel_range().contains_range(rel_range))?;
        Some((idx + 1, child.rel_offset(), child.as_ref()))
    }

    #[must_use]
    pub fn replace_child(&self, index: usize, new_child: GreenElement) -> GreenNode {
        let mut replacement = Some(new_child);
        let children = self.children().enumerate().map(|(i, child)| {
            if i == index {
                replacement.take().unwrap()
            } else {
                child.to_owned()
            }
        });
        GreenNode::new(self.kind(), children)
    }
    #[must_use]
    pub fn insert_child(&self, index: usize, new_child: GreenElement) -> GreenNode {
        // https://github.com/rust-lang/rust/issues/34433
        self.splice_children(index..index, iter::once(new_child))
    }
    #[must_use]
    pub fn remove_child(&self, index: usize) -> GreenNode {
        self.splice_children(index..=index, iter::empty())
    }
    #[must_use]
    pub fn splice_children<R, I>(&self, range: R, replace_with: I) -> GreenNode
    where
        R: ops::RangeBounds<usize>,
        I: IntoIterator<Item = GreenElement>,
    {
        let mut children: Vec<_> = self.children().map(|it| it.to_owned()).collect();
        children.splice(range, replace_with);
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
        // extract first element
        let mut iter = children.into_iter();
        let (first_tag, first_ptr) = match iter.next() {
            Some(el) => {
                text_len += el.text_len();
                let t = UntaggedElement::from_element(el);
                (t.0, Some(t.1))
            }
            None => (ElementTag::Node, None),
        };
        let children = iter.map(|el| {
            let rel_offset = text_len;
            text_len += el.text_len();
            match el {
                NodeOrToken::Node(node) => GreenChild::Node { rel_offset, node },
                NodeOrToken::Token(token) => GreenChild::Token { rel_offset, token },
            }
        });

        let data = ThinArc::from_header_and_iter(
            GreenNodeHead { kind, text_len: 0.into(), first_tag, first_ptr, _c: Count::new() },
            children,
        );

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
    fn as_ref(&self) -> GreenElementRef<'_> {
        match self {
            GreenChild::Node { node, .. } => NodeOrToken::Node(node),
            GreenChild::Token { token, .. } => NodeOrToken::Token(token),
        }
    }
    #[inline]
    fn rel_offset(&self) -> TextSize {
        match self {
            GreenChild::Node { rel_offset, .. } | GreenChild::Token { rel_offset, .. } => {
                *rel_offset
            }
        }
    }
    #[inline]
    fn rel_range(&self) -> TextRange {
        let len = self.as_ref().text_len();
        TextRange::at(self.rel_offset(), len)
    }
}

pub type Children<'a> =
    iter::Map<ChildrenExt<'a>, fn(<ChildrenExt<'a> as Iterator>::Item) -> GreenElementRef<'a>>;

type ChildrenExtHead<'a> = option::IntoIter<(GreenElementRef<'a>, TextSize)>;
type ChildrenExtTail<'a> =
    iter::Map<slice::Iter<'a, GreenChild>, fn(&'a GreenChild) -> (GreenElementRef<'a>, TextSize)>;

#[derive(Debug, Clone)]
pub struct ChildrenExt<'a> {
    raw: std::iter::Chain<ChildrenExtHead<'a>, ChildrenExtTail<'a>>,
    raw_len: usize,
}

impl<'a> ChildrenExt<'a> {
    fn create(head: Option<GreenElementRef<'a>>, tail: slice::Iter<'a, GreenChild>) -> Self {
        let raw_len = tail.len() + if head.is_some() { 1 } else { 0 };
        let head_iter: ChildrenExtHead<'a> = head.map(|el| (el, 0.into())).into_iter();
        let tail_iter: ChildrenExtTail<'a> = tail.map(|child| (child.as_ref(), child.rel_offset()));
        ChildrenExt { raw: head_iter.chain(tail_iter), raw_len }
    }
    pub(crate) fn empty() -> Self {
        ChildrenExt::create(None, [].iter())
    }
}

// NB: forward everything stable that iter::Slice specializes as of Rust 1.39.0
impl ExactSizeIterator for ChildrenExt<'_> {
    #[inline(always)]
    fn len(&self) -> usize {
        self.raw_len
    }
}

impl<'a> Iterator for ChildrenExt<'a> {
    type Item = (GreenElementRef<'a>, /* rel_offset */ TextSize);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        self.raw.next()
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.raw.size_hint()
    }

    #[inline]
    fn count(self) -> usize
    where
        Self: Sized,
    {
        self.raw.count()
    }

    #[inline]
    fn nth(&mut self, n: usize) -> Option<Self::Item> {
        self.raw.nth(n)
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

impl<'a> DoubleEndedIterator for ChildrenExt<'a> {
    #[inline]
    fn next_back(&mut self) -> Option<Self::Item> {
        self.raw.next_back()
    }

    #[inline]
    fn nth_back(&mut self, n: usize) -> Option<Self::Item> {
        self.raw.nth_back(n)
    }
}

impl FusedIterator for ChildrenExt<'_> {}
