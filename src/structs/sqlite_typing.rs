//! The affinity and storage classes SQLite gives an expression, and the
//! conversions it applies to the operands of an operator or the arguments of
//! a built-in function, as <https://www.sqlite.org/datatype3.html> defines
//! them.

use alloc::vec::Vec;
use core::fmt::{self, Display, Write};

/// The type affinity of a SQLite column or expression.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Affinity {
    /// `TEXT` affinity.
    Text,
    /// `NUMERIC` affinity.
    Numeric,
    /// `INTEGER` affinity.
    Integer,
    /// `REAL` affinity.
    Real,
    /// `BLOB` affinity, which prefers no storage class.
    Blob,
}

impl Affinity {
    /// Returns the affinity SQLite gives a column declared with the type
    /// `declared`, by the rules of section 3.1 of the datatype document in
    /// the order SQLite applies them, an empty declaration taking `BLOB`.
    ///
    /// # Example
    ///
    /// ```rust
    /// use sql_traits::prelude::*;
    ///
    /// assert_eq!(Affinity::of_declared_type("UNSIGNED BIG INT"), Affinity::Integer);
    /// assert_eq!(Affinity::of_declared_type("VARCHAR(255)"), Affinity::Text);
    /// assert_eq!(Affinity::of_declared_type(""), Affinity::Blob);
    /// assert_eq!(Affinity::of_declared_type("DOUBLE PRECISION"), Affinity::Real);
    /// assert_eq!(Affinity::of_declared_type("FLOATING POINT"), Affinity::Integer);
    /// assert_eq!(Affinity::of_declared_type("STRING"), Affinity::Numeric);
    /// ```
    #[must_use]
    pub fn of_declared_type(declared: &str) -> Self {
        let mut scan = DeclaredType::default();
        scan.scan(declared);
        scan.affinity()
    }

    /// The affinity of a column declared with the type `declared` prints.
    pub(crate) fn of_written(declared: &impl Display) -> Self {
        let mut scan = DeclaredType::default();
        // Scanning never fails, so neither does writing into it.
        let _ = write!(scan, "{declared}");
        scan.affinity()
    }

    /// Returns whether the affinity is `INTEGER`, `REAL` or `NUMERIC`, the
    /// ones a comparison applies `NUMERIC` affinity against.
    #[must_use]
    pub const fn is_numeric(self) -> bool {
        matches!(self, Self::Integer | Self::Real | Self::Numeric)
    }

    /// Returns the storage classes a column of this affinity stores the
    /// values it converts in, any class for `BLOB`.
    #[must_use]
    pub const fn preferred_storage(self) -> StorageClasses {
        match self {
            Self::Text => StorageClasses::TEXT,
            Self::Numeric => StorageClasses::NUMBERS,
            Self::Integer => StorageClasses::INTEGER,
            Self::Real => StorageClasses::REAL,
            Self::Blob => StorageClasses::ANY,
        }
    }
}

/// The substrings of a declared type section 3.1 assigns an affinity by,
/// scanned case-insensitively.
#[derive(Default)]
struct DeclaredType {
    /// The last three bytes scanned, upper-cased.
    window: [u8; 3],
    written: bool,
    integer: bool,
    text: bool,
    blob: bool,
    real: bool,
}

impl DeclaredType {
    fn scan(&mut self, declared: &str) {
        for byte in declared.bytes() {
            let byte = byte.to_ascii_uppercase();
            let [first, second, third] = self.window;
            let last_four = [first, second, third, byte];
            self.integer |= [second, third, byte] == *b"INT";
            self.text |= matches!(&last_four, b"CHAR" | b"CLOB" | b"TEXT");
            self.blob |= &last_four == b"BLOB";
            self.real |= matches!(&last_four, b"REAL" | b"FLOA" | b"DOUB");
            self.written |= !byte.is_ascii_whitespace();
            self.window = [second, third, byte];
        }
    }

    const fn affinity(&self) -> Affinity {
        if self.integer {
            Affinity::Integer
        } else if self.text {
            Affinity::Text
        } else if self.blob || !self.written {
            Affinity::Blob
        } else if self.real {
            Affinity::Real
        } else {
            Affinity::Numeric
        }
    }
}

impl Write for DeclaredType {
    fn write_str(&mut self, written: &str) -> fmt::Result {
        self.scan(written);
        Ok(())
    }
}

/// The storage classes the values of a SQLite expression may take besides
/// `NULL`.
///
/// # Example
///
/// ```rust
/// use sql_traits::prelude::*;
///
/// let numbers = StorageClasses::INTEGER.union(StorageClasses::REAL);
/// assert_eq!(numbers, StorageClasses::NUMBERS);
/// assert!(StorageClasses::ANY.contains(numbers));
/// assert!(!numbers.intersects(StorageClasses::TEXT));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StorageClasses(u8);

impl StorageClasses {
    /// No class, the expression being `NULL` whatever it reads.
    pub const NULL: Self = Self(0);
    /// The `INTEGER` storage class.
    pub const INTEGER: Self = Self(1);
    /// The `REAL` storage class.
    pub const REAL: Self = Self(1 << 1);
    /// The `TEXT` storage class.
    pub const TEXT: Self = Self(1 << 2);
    /// The `BLOB` storage class.
    pub const BLOB: Self = Self(1 << 3);
    /// The `INTEGER` and `REAL` storage classes.
    pub const NUMBERS: Self = Self(Self::INTEGER.0 | Self::REAL.0);
    /// Every storage class, where nothing fixes the class of the values.
    pub const ANY: Self = Self(Self::NUMBERS.0 | Self::TEXT.0 | Self::BLOB.0);

    /// Returns the classes either set holds.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Returns whether this set holds every class `other` holds.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Returns whether the sets share a class.
    #[must_use]
    pub const fn intersects(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }

    /// Whether every value of a class this set holds differs in class from
    /// the classes `accepted` holds, so reading the value under `accepted`
    /// converts it, which nothing tells for `NULL`.
    pub(crate) const fn converts_under(self, accepted: Self) -> bool {
        self.0 != 0 && !self.intersects(accepted)
    }
}

/// A conversion SQLite applies to an operand or an argument before using it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SqliteConversion {
    /// A comparison applies the affinity, `NUMERIC` or `TEXT`, which converts
    /// the value only when the conversion loses nothing.
    Affinity(Affinity),
    /// The value is read as a number, `TEXT` and `BLOB` converting to
    /// `INTEGER` or `REAL` even when the conversion loses information.
    Numeric,
    /// The value is read as an `INTEGER`, as `CAST(... AS INTEGER)` reads it.
    Integer,
    /// The value is read as a `REAL`.
    Real,
    /// The value is read as `TEXT`.
    Text,
    /// The value is read as a `BLOB`.
    Blob,
}

/// The conversion SQLite applies to each operand of an operator.
///
/// An operand gets a conversion only when the class of its values is known,
/// from the expression or presumed from its affinity, and differs from the
/// class the operator reads it as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SqliteOperator {
    left: Option<SqliteConversion>,
    right: Option<SqliteConversion>,
}

impl SqliteOperator {
    /// Creates the conversions of the `left` and `right` operands.
    pub(crate) const fn new(
        left: Option<SqliteConversion>,
        right: Option<SqliteConversion>,
    ) -> Self {
        Self { left, right }
    }

    /// Returns the conversion of the left operand, `None` for an operand
    /// SQLite reads as it is and for a prefix operator.
    #[must_use]
    pub const fn left(&self) -> Option<SqliteConversion> {
        self.left
    }

    /// Returns the conversion of the right operand, `None` for an operand
    /// SQLite reads as it is.
    #[must_use]
    pub const fn right(&self) -> Option<SqliteConversion> {
        self.right
    }
}

/// The conversion a SQLite built-in scalar function applies to each argument
/// and the storage classes of what it returns.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SqliteCall {
    arguments: Vec<Option<SqliteConversion>>,
    storage: StorageClasses,
}

impl SqliteCall {
    /// Creates the call converting each argument by `arguments` and
    /// returning values of `storage`.
    pub(crate) const fn new(
        arguments: Vec<Option<SqliteConversion>>,
        storage: StorageClasses,
    ) -> Self {
        Self { arguments, storage }
    }

    /// Returns the conversion of each argument, in the order the call writes
    /// them, `None` for an argument the function reads as it is.
    #[must_use]
    pub fn arguments(&self) -> &[Option<SqliteConversion>] {
        &self.arguments
    }

    /// Returns the storage classes of the values the call returns.
    #[must_use]
    pub const fn storage(&self) -> StorageClasses {
        self.storage
    }
}
