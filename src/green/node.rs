use std::{
    borrow::{Borrow, Cow},
    fmt,
    hash::Hash,
    iter::{self, FusedIterator},
    mem::{self, ManuallyDrop},
    ops, option, ptr, slice, u32,
};

use countme::Count;

use crate::{
    GreenToken, GreenTokenData, NodeOrToken, TextRange, TextSize, arc::{Arc, HeaderSlice, ThinArc}, green::{
        GreenElement, GreenElementRef, SyntaxKind,
        untagged_element::{ElementTag, UntaggedElement},
    }
};

pub(super) struct GreenNodeHead {
    kind: SyntaxKind,
    text_len: TextSize,
    compact: bool,
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
        // clone first element
        let first_ptr = self.first_ref().map(|el| UntaggedElement::from_element(el.to_owned()).1);
        Self {
            kind: self.kind,
            text_len: self.text_len,
            compact: self.compact,
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

const TAG_BITS: usize = 2;
const TAG_MASK: u32 = (1 << TAG_BITS) - 1;
#[derive(Debug)]
struct GreenChildCompact {
    tag_rel_offset: u32, ptr_offset: i32
}
fn ptr_to_offset<T>(align: usize, ptr_base: usize, ptr: *const T) -> i32 {
    let offset = (unsafe { ptr.byte_offset_from(ptr_base as *const ()) } >> align.trailing_zeros()).try_into().unwrap();
    assert_eq!(ptr, unsafe { offset_to_ptr(align, ptr_base, offset) });
    offset
}

unsafe fn offset_to_ptr<T>(align: usize, ptr_base: usize, offset: i32) -> *const T {
    unsafe { (ptr_base as *const T).byte_offset((offset as isize) << align.trailing_zeros()) }
}

type ReprThin = HeaderSlice<GreenNodeHead, [GreenChild; 0]>;
type ReprCompact = HeaderSlice<GreenNodeHead, [GreenChildCompact; 0]>;
#[repr(transparent)]
pub struct GreenNodeData {
    // align GreenNodeData on GreenNodeHead field
    header: ManuallyDrop<GreenNodeHead>,
}

impl PartialEq for GreenNodeData {
    fn eq(&self, other: &Self) -> bool {
        self.kind() == other.kind() && self.children().eq(other.children())
    }
}

impl Hash for GreenNodeData {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.kind().hash(state);
        for child in self.children() {
            child.hash(state);
        }
    }
}

/// Internal node in the immutable tree.
/// It has other nodes and tokens as children.
#[repr(transparent)]
pub struct GreenNode {
    header_ptr: *const GreenNodeHead,
}

unsafe impl Send for GreenNode {}
unsafe impl Sync for GreenNode {}
impl Clone for GreenNode {
    fn clone(&self) -> Self {
        fn clone_inner<H, T>(p: &HeaderSlice<H, [T; 0]>) -> *const H {
            unsafe {
                let thin = ThinArc::from_raw(p as *const _ as *mut _);
                let cloned = ThinArc::into_raw(thin.clone());
                assert_eq!(p as *const _, &*cloned as *const _);
                let _ = ManuallyDrop::new(thin);
                &(*cloned).header as *const H
            }
        }
        match self.outer() {
            Ok(plain) => Self { header_ptr: clone_inner(plain) },
            Err(compact) => Self { header_ptr: clone_inner(compact) },
        }
    }
}
impl PartialEq for GreenNode {
    fn eq(&self, other: &Self) -> bool {
        self as &GreenNodeData == other as &GreenNodeData
    }
}
impl Eq for GreenNode {}
impl Hash for GreenNode {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        (self as &GreenNodeData).hash(state);
    }
}

impl Drop for GreenNode {
    fn drop(&mut self) {
        match self.outer() {
            Ok(plain) => {
                let _ = unsafe { ThinArc::from_raw((plain as *const ReprThin).cast_mut()) };
            },
            Err(compact) => {
                let ptr_base = self.header.first_ptr.as_ref().map(|e| e.as_usize()).unwrap_or_default();
                let thin = unsafe { ThinArc::from_raw((compact as *const ReprCompact).cast_mut()) };
                let mut fat = Arc::from_thin(thin);
                if let Some(hs) = Arc::get_mut(&mut fat) {
                    for compact in hs.slice_mut() {
                        compact.drop_underlying(ptr_base);
                    }
                }
            },
        }
    }
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
        &self.header
    }

    fn outer(&self) -> Result<&ReprThin, &ReprCompact> {
        // SAFETY: checked compact / non-compact
        if self.header().compact {
            Err(unsafe { ReprCompact::from_header_ref(self.header()) })
        } else {
            Ok(unsafe { ReprThin::from_header_ref(self.header()) })
        }
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
        match self.outer() {
            Ok(plain) => ChildrenExt::create(self.header().first_ref(), plain.slice().iter()),
            Err(compact) => {
                let ptr_base = self.header.first_ptr.as_ref().map(|e| e.as_usize()).unwrap_or_default();
                ChildrenExt::create_compact(self.header().first_ref(), compact.slice().iter(), ptr_base)
            }
        }
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
        match self.outer() {
            Ok(plain) => {
                let idx = plain
                    .slice()
                    .binary_search_by(|it| {
                        let child_range = it.rel_range();
                        TextRange::ordering(child_range, rel_range)
                    })
                    // XXX: this handles empty ranges
                    .unwrap_or_else(|it| it.saturating_sub(1));
                let child = &plain
                    .slice()
                    .get(idx)
                    .filter(|it| it.rel_range().contains_range(rel_range))?;
                Some((idx + 1, child.rel_offset(), child.as_ref()))
            }
            Err(compact) => {
                let ptr_base = compact.header.first_ptr.as_ref().map(|e| e.as_usize()).unwrap_or_default();
                let idx = compact
                    .slice()
                    .binary_search_by(|it| {
                        let child_range = it.rel_range(ptr_base);
                        TextRange::ordering(child_range, rel_range)
                    })
                    // XXX: this handles empty ranges
                    .unwrap_or_else(|it| it.saturating_sub(1));
                let child = &compact
                    .slice()
                    .get(idx)
                    .filter(|it| it.rel_range(ptr_base).contains_range(rel_range))?;
                Some((idx + 1, child.rel_offset(), child.as_ref(ptr_base)))
            },
        }
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
        assert_ne!(self as *const _, ptr::null());
        // SAFETY: GreenNodeData must be transparent repr for GreenNodeHead
        unsafe {
            mem::transmute::<&GreenNodeHead, &GreenNodeData>(&*self.header_ptr)
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
        let mut ptr_base = 0usize;
        // extract first element
        let mut iter = children.into_iter();
        let (first_tag, first_ptr) = match iter.next() {
            Some(el) => {
                text_len += el.text_len();
                let t = UntaggedElement::from_element(el);
                ptr_base = t.1.as_usize();
                (t.0, Some(t.1))
            }
            None => (ElementTag::Node, None),
        };
        let children = iter.map(|el| {
            let rel_offset = text_len;
            text_len += el.text_len();
            // match el {
            //     NodeOrToken::Node(node) => GreenChild::Node { rel_offset, node },
            //     NodeOrToken::Token(token) => GreenChild::Token { rel_offset, token },
            // }
            let rel_offset = u32::from(rel_offset);
            assert!(rel_offset < (1 << (32 - TAG_BITS)));
            const ALIGN: usize = mem::align_of::<GreenNodeData>();
            assert_eq!(ALIGN, mem::align_of::<crate::GreenTokenData>());
            match el {
                NodeOrToken::Node(node) => {
                    let ptr = GreenNode::into_raw(node).as_ptr();
                    let tag_rel_offset = (u32::from(rel_offset) << TAG_BITS) + ElementTag::Node as u32;
                    GreenChildCompact {
                        tag_rel_offset,
                        ptr_offset: ptr_to_offset(ALIGN, ptr_base, ptr),
                    }
                },
                NodeOrToken::Token(token) => {
                    let ptr = GreenToken::into_raw(token).as_ptr();
                    let tag_rel_offset = (u32::from(rel_offset) << TAG_BITS) + ElementTag::Token as u32;
                    GreenChildCompact {
                        tag_rel_offset,
                        ptr_offset: ptr_to_offset(ALIGN, ptr_base, ptr),
                    }
                },
            }
        });

        let data = ThinArc::from_header_and_iter(
            GreenNodeHead {
                kind,
                text_len: 0.into(),
                compact: true,
                first_tag,
                first_ptr,
                _c: Count::new(),
            },
            children,
        );

        // XXX: fixup `text_len` after construction, because we can't iterate
        // `children` twice.
        let data = {
            let mut data = Arc::from_thin(data);
            Arc::get_mut(&mut data).unwrap().header.text_len = text_len;
            Arc::into_thin(data)
        };

        let header_ptr = unsafe { &(*ThinArc::into_raw(data)).header } as _;
        GreenNode { header_ptr }
    }

    #[inline]
    pub(crate) fn into_raw(this: GreenNode) -> ptr::NonNull<GreenNodeData> {
        let green = ManuallyDrop::new(this);
        let green: &GreenNodeData = &*green;
        ptr::NonNull::from(&*green)
    }

    #[inline]
    pub(crate) unsafe fn from_raw(ptr: ptr::NonNull<GreenNodeData>) -> GreenNode {
        unsafe {
            GreenNode { header_ptr: &*ptr.as_ref().header }
        }
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

impl GreenChildCompact {
    #[inline]
    fn as_ref(&self, ptr_base: usize) -> GreenElementRef<'_> {
        const ALIGN: usize = mem::align_of::<GreenNodeData>();
        let tag = self.tag_rel_offset & TAG_MASK;
        if tag == ElementTag::Node as u32 {
            NodeOrToken::Node(unsafe { &*offset_to_ptr(ALIGN, ptr_base, self.ptr_offset) })
        } else if tag == ElementTag::Token as u32 {
            NodeOrToken::Token(unsafe { &*offset_to_ptr(ALIGN, ptr_base, self.ptr_offset) })
        } else {
            panic!()
        }
    }
    #[inline]
    fn drop_underlying(&mut self, ptr_base: usize) {
        const ALIGN: usize = mem::align_of::<GreenNodeData>();
        let tag = self.tag_rel_offset & TAG_MASK;
        unsafe {
            if tag == ElementTag::Node as u32 {
                let _ = GreenNode::from_raw(ptr::NonNull::new_unchecked(offset_to_ptr::<GreenNodeData>(ALIGN, ptr_base, self.ptr_offset).cast_mut()));
            } else if tag == ElementTag::Token as u32 {
                let _ = GreenToken::from_raw(ptr::NonNull::new_unchecked(offset_to_ptr::<GreenTokenData>(ALIGN, ptr_base, self.ptr_offset).cast_mut()));
            } else {
                panic!()
            }
        }
        self.tag_rel_offset = u32::MAX;  // tag is invalid now
    }
    #[inline]
    fn rel_offset(&self) -> TextSize {
        (self.tag_rel_offset >> TAG_BITS).into()
    }
    #[inline]
    fn rel_range(&self, ptr_base: usize) -> TextRange {
        let len = self.as_ref(ptr_base).text_len();
        TextRange::at(self.rel_offset(), len)
    }
}

pub type Children<'a> =
    iter::Map<ChildrenExt<'a>, fn(<ChildrenExt<'a> as Iterator>::Item) -> GreenElementRef<'a>>;

type ChildrenExtHead<'a> = option::IntoIter<(GreenElementRef<'a>, TextSize)>;
type ChildrenExtTail<'a> =
    iter::Map<slice::Iter<'a, GreenChild>, fn(&'a GreenChild) -> (GreenElementRef<'a>, TextSize)>;
type ChildrenExtTailCompact<'a> =
    iter::Map<
        iter::Zip<slice::Iter<'a, GreenChildCompact>, iter::RepeatN<usize>>,
        fn((&'a GreenChildCompact, usize)) -> (GreenElementRef<'a>, TextSize)>;

#[derive(Debug, Clone)]
pub enum ChildrenExt<'a> {
    Plain {
        raw: std::iter::Chain<ChildrenExtHead<'a>, ChildrenExtTail<'a>>,
    },
    Compact {
        raw: std::iter::Chain<ChildrenExtHead<'a>, ChildrenExtTailCompact<'a>>,
    }
}

impl<'a> ChildrenExt<'a> {
    fn create(head: Option<GreenElementRef<'a>>, tail: slice::Iter<'a, GreenChild>) -> Self {
        let head_iter: ChildrenExtHead<'a> = head.map(|el| (el, 0.into())).into_iter();
        let tail_iter: ChildrenExtTail<'a> = tail.map(|child| (child.as_ref(), child.rel_offset()));
        ChildrenExt::Plain { raw: head_iter.chain(tail_iter) }
    }
    fn create_compact(head: Option<GreenElementRef<'a>>, tail: slice::Iter<'a, GreenChildCompact>, ptr_base: usize) -> Self {
        let head_iter: ChildrenExtHead<'a> = head.map(|el| (el, 0.into())).into_iter();
        let tail_len = tail.len();
        let tail_iter: ChildrenExtTailCompact<'a> = tail.zip(iter::repeat_n(ptr_base, tail_len)).map(|(child, ptr_base)| (child.as_ref(ptr_base), child.rel_offset()));
        ChildrenExt::Compact { raw: head_iter.chain(tail_iter) }
    }
    pub(crate) fn empty() -> Self {
        ChildrenExt::create(None, [].iter())
    }
}

// NB: forward everything stable that iter::Slice specializes as of Rust 1.39.0
impl ExactSizeIterator for ChildrenExt<'_> {
    #[inline(always)]
    fn len(&self) -> usize {
        let (l, u) = match self {
            ChildrenExt::Plain { raw, .. } => raw.size_hint(),
            ChildrenExt::Compact { raw, .. } => raw.size_hint(),
        };
        let u = u.expect("iter size overflow");
        assert_eq!(l, u);
        l
    }
}

impl<'a> Iterator for ChildrenExt<'a> {
    type Item = (GreenElementRef<'a>, /* rel_offset */ TextSize);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        match self {
            ChildrenExt::Plain { raw, .. } => raw.next(),
            ChildrenExt::Compact { raw, .. } => raw.next(),
        }
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        match self {
            ChildrenExt::Plain { raw, .. } => raw.size_hint(),
            ChildrenExt::Compact { raw, .. } => raw.size_hint(),
        }
    }

    #[inline]
    fn count(self) -> usize
    where
        Self: Sized,
    {
        match self {
            ChildrenExt::Plain { raw, .. } => raw.count(),
            ChildrenExt::Compact { raw, .. } => raw.count(),
        }
    }

    #[inline]
    fn nth(&mut self, n: usize) -> Option<Self::Item> {
        match self {
            ChildrenExt::Plain { raw, .. } => raw.nth(n),
            ChildrenExt::Compact { raw, .. } => raw.nth(n),
        }
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
        match self {
            ChildrenExt::Plain { raw, .. } => raw.next_back(),
            ChildrenExt::Compact { raw, .. } => raw.next_back(),
        }
    }

    #[inline]
    fn nth_back(&mut self, n: usize) -> Option<Self::Item> {
        match self {
            ChildrenExt::Plain { raw, .. } => raw.nth_back(n),
            ChildrenExt::Compact { raw, .. } => raw.nth_back(n),
        }
    }
}

impl FusedIterator for ChildrenExt<'_> {}
