use std::ptr;

use crate::{
    GreenNode, GreenNodeData, GreenToken, GreenTokenData, NodeOrToken,
    green::{GreenElement, GreenElementRef},
};

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
#[repr(u8)]
pub(super) enum ElementTag {
    Node = 0,
    Token = 1,
}

#[repr(transparent)]
pub(super) struct UntaggedElement(ptr::NonNull<()>);

/// # Safety
/// GreenElement is Send + Sync
unsafe impl Send for UntaggedElement {}
/// # Safety
/// GreenElement is Send + Sync
unsafe impl Sync for UntaggedElement {}

impl UntaggedElement {
    #[must_use]
    #[inline]
    pub(super) fn from_element(el: GreenElement) -> (ElementTag, UntaggedElement) {
        match el {
            NodeOrToken::Node(node) => {
                (ElementTag::Node, UntaggedElement(GreenNode::into_raw(node).cast()))
            }
            NodeOrToken::Token(token) => {
                (ElementTag::Token, UntaggedElement(GreenToken::into_raw(token).cast()))
            }
        }
    }

    /// # Safety
    /// `tag` must be the correct tag otherwise this is ub
    #[inline]
    pub(super) unsafe fn into_element(self, tag: ElementTag) -> GreenElement {
        match tag {
            ElementTag::Node => NodeOrToken::Node(unsafe { GreenNode::from_raw(self.0.cast()) }),
            ElementTag::Token => NodeOrToken::Token(unsafe { GreenToken::from_raw(self.0.cast()) }),
        }
    }

    /// # Safety
    /// `tag` must be the correct tag otherwise this is ub
    #[inline]
    pub(super) unsafe fn as_element_ref<'a>(&'a self, tag: ElementTag) -> GreenElementRef<'a> {
        match tag {
            ElementTag::Node => {
                NodeOrToken::Node(unsafe { self.0.cast::<GreenNodeData>().as_ref() })
            }
            ElementTag::Token => {
                NodeOrToken::Token(unsafe { self.0.cast::<GreenTokenData>().as_ref() })
            }
        }
    }
}
