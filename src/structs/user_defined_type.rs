//! The stored form of a user-defined type a `CREATE TYPE` statement defines.

use sqlparser::ast::{
    DataType, ObjectName, UserDefinedTypeCompositeAttributeDef, UserDefinedTypeRepresentation,
};

/// A user-defined type `CREATE TYPE` defines, stored after its type
/// references are bound.
///
/// PostgreSQL keeps user-defined types, the domains, and the row type of every
/// table, view and materialized view in one pool of names per schema. A range
/// type brings a multirange of its own, which names that pool but is not a type
/// the schema lists of its own, and a shell, the bare `CREATE TYPE name;`,
/// holds a name until a later definition fills it or a domain or relation takes
/// it over.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UserDefinedType {
    /// The type's own identifier, bound with the schema it landed in.
    pub(crate) name: ObjectName,
    /// The definition, with every type reference bound, or `None` for a shell.
    pub(crate) representation: Option<UserDefinedTypeRepresentation>,
    /// The multirange a range type brings, named after the type it comes with
    /// or the name `MULTIRANGE_TYPE_NAME` gave it.
    pub(crate) multirange_name: Option<ObjectName>,
}

impl UserDefinedType {
    /// The attributes a composite type declares, in the order written, or
    /// `None` for a type that is no composite.
    #[must_use]
    pub fn composite_attributes(&self) -> Option<&[UserDefinedTypeCompositeAttributeDef]> {
        match &self.representation {
            Some(UserDefinedTypeRepresentation::Composite { attributes }) => Some(attributes),
            _ => None,
        }
    }

    /// The `SUBTYPE` a range type is declared over, or `None` for a type that
    /// is no range.
    #[must_use]
    pub fn range_subtype(&self) -> Option<&DataType> {
        match &self.representation {
            Some(UserDefinedTypeRepresentation::Range { options }) => {
                options.iter().find_map(|option| {
                    match option {
                        sqlparser::ast::UserDefinedTypeRangeOption::Subtype(data_type) => {
                            Some(data_type)
                        }
                        _ => None,
                    }
                })
            }
            _ => None,
        }
    }

    /// The `ELEMENT` a base type declares, or `None` for a type that is no
    /// base type, or one that declares none.
    #[must_use]
    pub fn element(&self) -> Option<&DataType> {
        match &self.representation {
            Some(UserDefinedTypeRepresentation::SqlDefinition { options }) => {
                options.iter().find_map(|option| {
                    match option {
                        sqlparser::ast::UserDefinedTypeSqlDefinitionOption::Element(data_type) => {
                            Some(data_type)
                        }
                        _ => None,
                    }
                })
            }
            _ => None,
        }
    }

    /// The definition the type was created with, or `None` for a shell.
    #[must_use]
    pub fn representation(&self) -> Option<&UserDefinedTypeRepresentation> {
        self.representation.as_ref()
    }

    /// The multirange a range type brings, named and bound as the type itself,
    /// or `None` for a type that is no range.
    #[must_use]
    pub fn multirange_name(&self) -> Option<&ObjectName> {
        self.multirange_name.as_ref()
    }

    /// Whether the type is a shell, a name held by the bare `CREATE TYPE name;`
    /// and never filled.
    #[must_use]
    pub fn is_shell(&self) -> bool {
        self.representation.is_none()
    }
}
