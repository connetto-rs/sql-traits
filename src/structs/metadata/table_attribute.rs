//! Submodule for objects whose main metadata is just that they are part of a
//! table.

use alloc::sync::Arc;
use core::{
    cmp::Ordering,
    fmt::{self, Debug, Display},
    hash::{Hash, Hasher},
};

use yoke::Yoke;

/// A struct associating a table with one of its attributes, such as a column or
/// constraint.
///
/// An attribute the table declares is lent out of the shared table, never
/// copied.
pub struct TableAttribute<T: 'static, A: 'static> {
    held: Held<T, A>,
}

enum Held<T: 'static, A: 'static> {
    /// An attribute lent out of the table that declares it.
    Declared(Yoke<&'static A, Arc<T>>),
    /// An attribute the table does not hold as written.
    Own { attribute: A, table: Arc<T> },
}

impl<T: 'static, A: Clone + 'static> Clone for TableAttribute<T, A> {
    fn clone(&self) -> Self {
        let held = match &self.held {
            Held::Declared(lent) => Held::Declared(lent.clone()),
            Held::Own { attribute, table } => {
                Held::Own { attribute: attribute.clone(), table: Arc::clone(table) }
            }
        };
        Self { held }
    }
}

impl<T: Debug + 'static, A: Debug + 'static> Debug for TableAttribute<T, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TableAttribute")
            .field("attribute", self.attribute())
            .field("table", self.shared_table())
            .finish()
    }
}

impl<T: PartialEq + 'static, A: PartialEq + 'static> PartialEq for TableAttribute<T, A> {
    fn eq(&self, other: &Self) -> bool {
        self.attribute() == other.attribute() && self.shared_table() == other.shared_table()
    }
}

impl<T: Eq + 'static, A: Eq + 'static> Eq for TableAttribute<T, A> {}

impl<T: Hash + 'static, A: Hash + 'static> Hash for TableAttribute<T, A> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.attribute().hash(state);
        self.shared_table().hash(state);
    }
}

impl<T: PartialOrd + 'static, A: PartialOrd + 'static> PartialOrd for TableAttribute<T, A> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        match self.attribute().partial_cmp(other.attribute()) {
            Some(Ordering::Equal) => self.shared_table().partial_cmp(other.shared_table()),
            ordering => ordering,
        }
    }
}

impl<T: Ord + 'static, A: Ord + 'static> Ord for TableAttribute<T, A> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.attribute()
            .cmp(other.attribute())
            .then_with(|| self.shared_table().cmp(other.shared_table()))
    }
}

impl<T: 'static, A: Display + 'static> Display for TableAttribute<T, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.attribute())
    }
}

impl<T: 'static, A: 'static> TableAttribute<T, A> {
    /// Creates a new `TableAttribute` associating the given table with the
    /// given attribute, which the table does not hold as written.
    ///
    /// # Example
    ///
    /// ```rust
    /// use alloc::sync::Arc;
    /// extern crate alloc;
    /// use sql_traits::structs::TableAttribute;
    ///
    /// let table = Arc::new("users");
    /// let attr = TableAttribute::new(table, "id");
    /// assert_eq!(*attr.table(), "users");
    /// assert_eq!(*attr.attribute(), "id");
    /// ```
    #[inline]
    pub fn new(table: Arc<T>, attribute: A) -> Self {
        Self { held: Held::Own { attribute, table } }
    }

    /// Lends the attribute `find` picks out of `table`, read in place rather
    /// than copied, or [`None`] when `find` picks nothing.
    ///
    /// # Example
    ///
    /// ```rust
    /// use alloc::sync::Arc;
    /// extern crate alloc;
    /// use sql_traits::structs::TableAttribute;
    ///
    /// let table = Arc::new(vec!["id", "name"]);
    /// let name = TableAttribute::declared_in(table, |columns| columns.get(1)).unwrap();
    /// assert_eq!(*name.attribute(), "name");
    /// assert!(TableAttribute::declared_in(Arc::new(vec!["id"]), |columns| columns.get(1)).is_none());
    /// ```
    pub fn declared_in(
        table: Arc<T>,
        find: impl for<'table> FnOnce(&'table T) -> Option<&'table A>,
    ) -> Option<Self> {
        Yoke::try_attach_to_cart(table, |table| find(table).ok_or(()))
            .ok()
            .map(|lent| Self { held: Held::Declared(lent) })
    }

    /// Lends every attribute of the list `list` reads out of `table`, in
    /// order, each read in place rather than copied.
    ///
    /// # Example
    ///
    /// ```rust
    /// use alloc::sync::Arc;
    /// extern crate alloc;
    /// use sql_traits::structs::TableAttribute;
    ///
    /// let table = Arc::new(vec!["id", "name"]);
    /// let columns: Vec<_> = TableAttribute::each_declared(&table, |columns| columns.as_slice())
    ///     .map(|column| *column.attribute())
    ///     .collect();
    /// assert_eq!(columns, ["id", "name"]);
    /// ```
    pub fn each_declared(
        table: &Arc<T>,
        list: impl for<'table> FnOnce(&'table T) -> &'table [A],
    ) -> impl Iterator<Item = Self> {
        let lent: Yoke<&'static [A], Arc<T>> =
            Yoke::attach_to_cart(Arc::clone(table), |table| list(table));
        (0..lent.get().len()).map(move |position| {
            Self { held: Held::Declared(lent.map_project_cloned(|list, _| &list[position])) }
        })
    }

    /// Returns a reference to the table.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>("CREATE TABLE t (id INT);")?;
    /// let table =
    ///     db.table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)?.unwrap();
    /// let column = table.column("id", &db, IdentifierCase::AsWritten)?.unwrap();
    /// // `column` is a `TableAttribute<CreateTable, ColumnDef>` — its
    /// // `.table()` accessor returns the host table.
    /// assert_eq!(column.table().table_name(), "t");
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    pub fn table(&self) -> &T {
        self.shared_table()
    }

    /// Returns the shared handle on the table.
    pub(crate) fn shared_table(&self) -> &Arc<T> {
        match &self.held {
            Held::Declared(lent) => lent.backing_cart(),
            Held::Own { table, .. } => table,
        }
    }

    /// Returns a reference to the attribute.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>("CREATE TABLE t (id INT);")?;
    /// let table =
    ///     db.table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)?.unwrap();
    /// let column = table.column("id", &db, IdentifierCase::AsWritten)?.unwrap();
    /// // `column.attribute()` is the underlying `sqlparser::ast::ColumnDef`.
    /// assert_eq!(column.attribute().name.value, "id");
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    pub fn attribute(&self) -> &A {
        match &self.held {
            Held::Declared(lent) => lent.get(),
            Held::Own { attribute, .. } => attribute,
        }
    }

    /// Lends the part `find` picks out of the attribute, copied only when the
    /// attribute is held here, or [`None`] when `find` picks nothing.
    pub fn part<B: Clone + 'static>(
        &self,
        find: impl for<'part> FnOnce(&'part A) -> Option<&'part B>,
    ) -> Option<TableAttribute<T, B>> {
        match &self.held {
            Held::Declared(lent) => {
                lent.try_map_project_cloned(|attribute, _| find(*attribute).ok_or(()))
                    .ok()
                    .map(|lent| TableAttribute { held: Held::Declared(lent) })
            }
            Held::Own { attribute, table } => {
                find(attribute).map(|part| TableAttribute::new(Arc::clone(table), part.clone()))
            }
        }
    }

    /// Changes the attribute through `edit`, copying one the table declares
    /// out of it first.
    pub(crate) fn edit_attribute<R>(&mut self, edit: impl FnOnce(&mut A) -> R) -> R
    where
        A: Clone,
    {
        match &mut self.held {
            Held::Own { attribute, .. } => edit(attribute),
            Held::Declared(lent) => {
                let mut attribute = A::clone(lent.get());
                let table = Arc::clone(lent.backing_cart());
                let edited = edit(&mut attribute);
                self.held = Held::Own { attribute, table };
                edited
            }
        }
    }
}

#[cfg(all(test, feature = "std"))]
mod tests {
    use alloc::{
        string::{String, ToString},
        sync::Arc,
    };
    use core::{cmp::Ordering, hash::BuildHasher};
    use std::hash::RandomState;

    use super::TableAttribute;

    /// How an attribute is held stays invisible to equality, hashing,
    /// ordering and formatting.
    #[test]
    fn a_lent_attribute_matches_an_owned_copy() {
        let table = Arc::new(vec!["id", "name"]);
        let lent =
            TableAttribute::declared_in(Arc::clone(&table), |columns| columns.get(1)).unwrap();
        let owned = TableAttribute::new(Arc::clone(&table), "name");
        let hasher = RandomState::new();
        assert_eq!(lent, owned);
        assert_eq!(hasher.hash_one(&lent), hasher.hash_one(&owned));
        assert_eq!(lent.partial_cmp(&owned), Some(Ordering::Equal));
        assert_eq!(lent.cmp(&owned), Ordering::Equal);
        assert_eq!(format!("{lent:?}"), format!("{owned:?}"));
        assert_eq!(lent.to_string(), owned.to_string());
    }

    /// Attributes order by attribute before table.
    #[test]
    fn attributes_order_by_attribute_before_table() {
        let first = TableAttribute::new(Arc::new(vec!["z"]), "a");
        let second = TableAttribute::new(Arc::new(vec!["a"]), "b");
        assert_eq!(first.partial_cmp(&second), Some(Ordering::Less));
        assert_eq!(first.cmp(&second), Ordering::Less);
    }

    /// Editing a lent attribute copies it and leaves the table as declared.
    #[test]
    fn editing_a_lent_attribute_leaves_the_table_as_declared() {
        let table = Arc::new(vec![String::from("id")]);
        let mut column =
            TableAttribute::declared_in(Arc::clone(&table), |columns| columns.first()).unwrap();
        let length = column.edit_attribute(|name| {
            name.push_str("_key");
            name.len()
        });
        assert_eq!(length, 6);
        assert_eq!(column.attribute(), "id_key");
        assert_eq!(table[0], "id");
        assert!(Arc::ptr_eq(column.shared_table(), &table));
    }

    /// A part of an owned attribute is picked out of it.
    #[test]
    fn a_part_of_an_owned_attribute_is_picked() {
        let column = TableAttribute::new(Arc::new(vec!["t"]), ("id", 4));
        let width = column.part(|column| Some(&column.1)).unwrap();
        assert_eq!(*width.attribute(), 4);
        assert!(column.part(|_| None::<&i32>).is_none());
    }
}
