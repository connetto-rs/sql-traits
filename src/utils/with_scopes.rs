//! Tracking which `WITH` names a walk over a query can see.

use alloc::vec::Vec;

use sqlparser::ast::{Ident, ObjectName, ObjectNamePart, Query};

use crate::utils::identifier_resolution::identifiers_match;

/// The `WITH` names visible where a walk over a query stands, scoped as
/// PostgreSQL scopes them.
///
/// An item's body sees the items before it, or every item under `WITH
/// RECURSIVE`, and the rest of the query sees them all, so a body naming its
/// own item reads the outer relation unless the `WITH` is recursive. The walk
/// calls [`enter`](Self::enter) before visiting each query and
/// [`leave`](Self::leave) after it.
#[derive(Default)]
pub(crate) struct WithScopes {
    frames: Vec<WithFrame>,
}

/// The items one `WITH` clause binds.
struct WithFrame {
    /// The query carrying the clause, compared by address.
    owner: *const Query,
    /// Each item's body, compared by address.
    bodies: Vec<*const Query>,
    /// Each item's name.
    names: Vec<Ident>,
    /// Whether the clause is `WITH RECURSIVE`.
    recursive: bool,
    /// How many items, from the first, the walk currently sees.
    visible: usize,
}

impl WithScopes {
    pub(crate) fn enter(&mut self, query: &Query) {
        if let Some(frame) = self.frames.last_mut()
            && let Some(position) = frame.bodies.iter().position(|body| core::ptr::eq(*body, query))
        {
            frame.visible = if frame.recursive { frame.names.len() } else { position };
        }
        if let Some(with) = &query.with {
            self.frames.push(WithFrame {
                owner: query,
                bodies: with.cte_tables.iter().map(|cte| &raw const *cte.query).collect(),
                names: with.cte_tables.iter().map(|cte| cte.alias.name.clone()).collect(),
                recursive: with.recursive,
                visible: 0,
            });
        }
    }

    pub(crate) fn leave(&mut self, query: &Query) {
        if self.frames.last().is_some_and(|frame| core::ptr::eq(frame.owner, query)) {
            self.frames.pop();
        }
        if let Some(frame) = self.frames.last_mut()
            && !frame.recursive
            && let Some(position) = frame.bodies.iter().position(|body| core::ptr::eq(*body, query))
        {
            frame.visible = position + 1;
        }
    }

    /// Whether `name` reads a visible `WITH` item rather than a stored
    /// relation.
    pub(crate) fn reads_item(&self, name: &ObjectName) -> bool {
        let [ObjectNamePart::Identifier(written)] = name.0.as_slice() else {
            return false;
        };
        self.frames.iter().any(|frame| {
            frame.names[..frame.visible].iter().any(|item| {
                identifiers_match(
                    item.value.as_str(),
                    item.quote_style.is_some(),
                    written.value.as_str(),
                    written.quote_style.is_some(),
                )
            })
        })
    }
}
