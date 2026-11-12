//! The facts PostgreSQL's type conversion rules read about a type.

use crate::structs::TargetName;

/// A type's `pg_type.typcategory` code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TypeCategory(u8);

impl TypeCategory {
    /// `A`, array types.
    pub const ARRAY: Self = Self(b'A');
    /// `B`, boolean types.
    pub const BOOLEAN: Self = Self(b'B');
    /// `C`, composite types.
    pub const COMPOSITE: Self = Self(b'C');
    /// `D`, date and time types.
    pub const DATETIME: Self = Self(b'D');
    /// `E`, enum types.
    pub const ENUM: Self = Self(b'E');
    /// `G`, geometric types.
    pub const GEOMETRIC: Self = Self(b'G');
    /// `I`, network address types.
    pub const NETWORK: Self = Self(b'I');
    /// `N`, numeric types.
    pub const NUMERIC: Self = Self(b'N');
    /// `P`, pseudo-types.
    pub const PSEUDO: Self = Self(b'P');
    /// `R`, range types.
    pub const RANGE: Self = Self(b'R');
    /// `S`, string types.
    pub const STRING: Self = Self(b'S');
    /// `T`, timespan types.
    pub const TIMESPAN: Self = Self(b'T');
    /// `U`, user-defined types.
    pub const USER_DEFINED: Self = Self(b'U');
    /// `V`, bit-string types.
    pub const BIT_STRING: Self = Self(b'V');
    /// `X`, the `unknown` type.
    pub const UNKNOWN: Self = Self(b'X');
    /// `Z`, internal-use types.
    pub const INTERNAL: Self = Self(b'Z');

    /// Creates the category a `typcategory` code names, custom codes included.
    #[must_use]
    pub const fn new(code: u8) -> Self {
        Self(code)
    }

    /// Returns the `typcategory` code.
    #[must_use]
    pub const fn code(self) -> u8 {
        self.0
    }
}

/// The base type a domain coerces through, with the typmod it applies.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DomainFacts<'db> {
    base: TargetName<'db>,
    base_typmod: Option<i32>,
}

impl<'db> DomainFacts<'db> {
    /// Creates the facts of a domain over `base`, nested domains already
    /// followed, applying `base_typmod` as `pg_type.typtypmod` stores it.
    #[must_use]
    pub const fn new(base: TargetName<'db>, base_typmod: Option<i32>) -> Self {
        Self { base, base_typmod }
    }

    /// Returns the base type.
    #[must_use]
    pub const fn base(&self) -> &TargetName<'db> {
        &self.base
    }

    /// Returns the typmod applied to the base type, or `None` for none.
    #[must_use]
    pub const fn base_typmod(&self) -> Option<i32> {
        self.base_typmod
    }
}

/// The facts PostgreSQL's type conversion rules read about one type.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TypeFacts<'db> {
    category: TypeCategory,
    preferred: bool,
    domain: Option<DomainFacts<'db>>,
    array_element: Option<TargetName<'db>>,
    array_type: Option<TargetName<'db>>,
}

impl<'db> TypeFacts<'db> {
    /// Creates the facts of a type that is neither a domain nor an array.
    #[must_use]
    pub const fn new(category: TypeCategory, preferred: bool) -> Self {
        Self { category, preferred, domain: None, array_element: None, array_type: None }
    }

    /// Records that the type is a domain.
    #[must_use]
    pub fn with_domain(mut self, domain: DomainFacts<'db>) -> Self {
        self.domain = Some(domain);
        self
    }

    /// Records that the type is the array over `element`.
    #[must_use]
    pub fn with_array_element(mut self, element: TargetName<'db>) -> Self {
        self.array_element = Some(element);
        self
    }

    /// Records the array type over the type, `pg_type.typarray`.
    #[must_use]
    pub fn with_array_type(mut self, array: TargetName<'db>) -> Self {
        self.array_type = Some(array);
        self
    }

    /// Returns `pg_type.typcategory`.
    #[must_use]
    pub const fn category(&self) -> TypeCategory {
        self.category
    }

    /// Returns `pg_type.typispreferred`.
    #[must_use]
    pub const fn is_preferred(&self) -> bool {
        self.preferred
    }

    /// Returns the domain facts, or `None` when the type is no domain.
    #[must_use]
    pub const fn domain(&self) -> Option<&DomainFacts<'db>> {
        self.domain.as_ref()
    }

    /// Returns the element type, or `None` when the type is no array.
    #[must_use]
    pub const fn array_element(&self) -> Option<&TargetName<'db>> {
        self.array_element.as_ref()
    }

    /// Returns the array type over the type, or `None` when it has none.
    #[must_use]
    pub const fn array_type(&self) -> Option<&TargetName<'db>> {
        self.array_type.as_ref()
    }
}
