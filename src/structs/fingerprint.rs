//! SHA-256 based schema fingerprint for deterministic, persistable table
//! identity.
#![allow(
    clippy::expect_used,
    reason = "column counts, ordinals, and identifier lengths fit in u32/u64 for any real schema; the digest slice is a fixed 8 bytes"
)]

use alloc::{borrow::Cow, string::String, vec::Vec};
use core::fmt;

use sha2::{Digest, Sha256};

use crate::{
    errors::LookupError,
    traits::{ColumnLike, DataTypeLike, TableLike},
    utils::fingerprint_type_token::canonical_type_token,
};

/// Current fingerprint canonicalization version (FINGERPRINT_SPEC §10.1).
///
/// Written as `u16` big-endian into the canonical envelope.
const FINGERPRINT_VERSION: u16 = 1;

/// Persistence profile identifier (FINGERPRINT_SPEC §10.1).
///
/// Written as `u16` big-endian immediately after the canonicalization version.
/// `v1` defines a single profile (`1`); future profiles carry distinct ids so
/// fingerprints produced under different profiles are not comparable.
const PROFILE_PERSISTENCE_V1: u16 = 1;

/// Magic bytes identifying a schema fingerprint payload.
const MAGIC: &[u8; 4] = b"SFP1";

/// Hash algorithm identifier for a [`SchemaFingerprint`] envelope
/// (FINGERPRINT_SPEC §12).
///
/// Two fingerprints are comparable only when they share the same algorithm;
/// identical canonical bytes hashed under different algorithms produce
/// unrelated digests and are therefore not interchangeable.
///
/// The `v1` persistence profile emits [`AlgorithmId::Sha2_256`] exclusively.
/// Additional variants are reserved so that decoded envelopes from future
/// profiles can be represented and compared as not-comparable against `v1`
/// digests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum AlgorithmId {
    /// SHA-256 (the v1 default and only currently-emitted algorithm).
    Sha2_256,
    /// SHA3-256. Reserved for future profiles; not emitted by any current
    /// `compute_*` function. Variants exist so
    /// [`SchemaFingerprint::is_comparable_to`] can discriminate algorithms
    /// when decoded fingerprints from a future profile are inspected.
    Sha3_256,
}

impl AlgorithmId {
    /// Returns the canonical lowercase string identifier for this algorithm.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sha2_256 => "sha2-256",
            Self::Sha3_256 => "sha3-256",
        }
    }
}

impl fmt::Display for AlgorithmId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Errors produced when validating a canonical model prior to hashing
/// (FINGERPRINT_SPEC §10.2, audit §4 / P-12).
///
/// A fingerprint computation MUST fail when the input table fails any of
/// these contract checks; silently returning a digest for a malformed
/// schema is non-conformant.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FingerprintError {
    /// Column ordinals (as reported by [`ColumnLike::column_id`])
    /// do not form a contiguous range `[0, column_count)`. The first
    /// position whose reported ordinal disagrees with the iteration
    /// index is recorded.
    #[error("non-contiguous column ordinals: expected {expected}, got {got}")]
    NonContiguousOrdinals {
        /// Iteration index (the expected ordinal at this position).
        expected: u32,
        /// Ordinal reported by the column at this position.
        got: u32,
    },
    /// A primary-key ordinal appears more than once in the table's
    /// primary-key column list.
    #[error("duplicate primary-key ordinal: {0}")]
    DuplicatePkOrdinal(u32),
    /// A primary-key ordinal is greater than or equal to `column_count`,
    /// i.e. it points outside the table's column range.
    #[error("primary-key ordinal {ordinal} out of range for column_count {column_count}")]
    PkOrdinalOutOfRange {
        /// The offending primary-key ordinal.
        ordinal: u32,
        /// The table's column count.
        column_count: u32,
    },
    /// The table, or one of its columns, is not present in the database the
    /// fingerprint is being computed against.
    #[error(transparent)]
    Lookup(#[from] LookupError),
}

/// Validates the canonical layout used by the v1 persistence profile.
///
/// `column_ordinals` is the per-column ordinal as reported by
/// [`ColumnLike::column_id`], in iteration order. `pk_ordinals` is the
/// list of primary-key column ordinals (also from `column_id`).
fn validate_v1_layout_inner(
    column_count: u32,
    column_ordinals: &[u32],
    pk_ordinals: &[u32],
) -> Result<(), FingerprintError> {
    for (idx, &ord) in column_ordinals.iter().enumerate() {
        let expected = u32::try_from(idx).expect("column index fits in u32");
        if ord != expected {
            return Err(FingerprintError::NonContiguousOrdinals { expected, got: ord });
        }
    }

    let mut seen: alloc::collections::BTreeSet<u32> = alloc::collections::BTreeSet::new();
    for &pk in pk_ordinals {
        if pk >= column_count {
            return Err(FingerprintError::PkOrdinalOutOfRange { ordinal: pk, column_count });
        }
        if !seen.insert(pk) {
            return Err(FingerprintError::DuplicatePkOrdinal(pk));
        }
    }

    Ok(())
}

/// A deterministic schema fingerprint with a self-describing envelope
/// (FINGERPRINT_SPEC §12).
///
/// The fingerprint encodes the table's schema name, table name, columns
/// (ordinal, name, canonical type token, nullability, generated flag), and
/// primary-key ordinals using a versioned binary format. The resulting
/// digest is stable across Rust toolchain versions and can be safely
/// persisted.
///
/// Two fingerprints are *comparable* only when they share the same
/// `(algorithm_id, canonicalization_version, profile_id)` triple — see
/// [`Self::is_comparable_to`]. [`PartialEq`] enforces this: cross-envelope
/// equality is treated as a category error and always returns `false`.
///
/// # Examples
///
/// ```rust
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// use sql_traits::prelude::*;
///
/// let db =
///     ParserDB::parse::<GenericDialect>("CREATE TABLE users (id INT PRIMARY KEY, name TEXT);")?;
/// let table =
///     db.table_by_target(TargetName::new("users", false), IdentifierCase::AsWritten)?.unwrap();
/// let fp = table.schema_fingerprint(&db)?;
///
/// assert_eq!(fp.to_hex().len(), 64);
/// assert_eq!(fp.canonicalization_version(), 1);
/// assert_eq!(fp.profile_id(), 1);
/// assert_eq!(fp.algorithm_id(), AlgorithmId::Sha2_256);
/// # Ok(())
/// # }
/// ```
#[derive(Clone, Copy)]
pub struct SchemaFingerprint {
    algorithm_id: AlgorithmId,
    canonicalization_version: u16,
    profile_id: u16,
    digest: [u8; 32],
}

impl SchemaFingerprint {
    /// Creates a new fingerprint with an explicit envelope.
    ///
    /// Intended for in-crate use by `compute_*` functions and for tests
    /// asserting comparability rules across mismatched envelopes.
    #[must_use]
    pub(crate) fn new(
        algorithm_id: AlgorithmId,
        canonicalization_version: u16,
        profile_id: u16,
        digest: [u8; 32],
    ) -> Self {
        Self { algorithm_id, canonicalization_version, profile_id, digest }
    }

    /// Returns the full 256-bit digest.
    #[must_use]
    pub fn fingerprint256(&self) -> [u8; 32] {
        self.digest
    }

    /// Returns the first 128 bits of the digest.
    #[must_use]
    pub fn fingerprint128(&self) -> [u8; 16] {
        let mut buf = [0u8; 16];
        buf.copy_from_slice(&self.digest[..16]);
        buf
    }

    /// Returns the first 64 bits of the digest as a big-endian `u64`.
    #[must_use]
    pub fn fingerprint64(&self) -> u64 {
        u64::from_be_bytes(self.digest[..8].try_into().expect("slice is 8 bytes"))
    }

    /// Returns the hash algorithm that produced this digest.
    #[must_use]
    pub fn algorithm_id(&self) -> AlgorithmId {
        self.algorithm_id
    }

    /// Returns the canonicalization version of the encoded envelope.
    #[must_use]
    pub fn canonicalization_version(&self) -> u16 {
        self.canonicalization_version
    }

    /// Returns the persistence profile identifier of the encoded envelope.
    #[must_use]
    pub fn profile_id(&self) -> u16 {
        self.profile_id
    }

    /// Returns the digest as a lowercase hex string (64 characters).
    #[must_use]
    pub fn to_hex(&self) -> String {
        let mut hex = String::with_capacity(64);
        for byte in &self.digest {
            use fmt::Write;
            let _ = write!(hex, "{byte:02x}");
        }
        hex
    }

    /// Returns whether two fingerprints are comparable.
    ///
    /// Comparability requires that all three envelope metadata fields match:
    /// `algorithm_id`, `canonicalization_version`, and `profile_id`.
    /// Fingerprints whose envelopes differ were produced by different
    /// encoding schemes or hash algorithms and must not be compared for
    /// equality.
    #[must_use]
    pub fn is_comparable_to(&self, other: &Self) -> bool {
        self.algorithm_id == other.algorithm_id
            && self.canonicalization_version == other.canonicalization_version
            && self.profile_id == other.profile_id
    }
}

impl core::hash::Hash for SchemaFingerprint {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        self.algorithm_id.hash(state);
        self.canonicalization_version.hash(state);
        self.profile_id.hash(state);
        self.digest.hash(state);
    }
}

impl PartialEq for SchemaFingerprint {
    fn eq(&self, other: &Self) -> bool {
        self.is_comparable_to(other) && self.digest == other.digest
    }
}

impl Eq for SchemaFingerprint {}

impl fmt::Debug for SchemaFingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SchemaFingerprint")
            .field("algorithm_id", &self.algorithm_id)
            .field("canonicalization_version", &self.canonicalization_version)
            .field("profile_id", &self.profile_id)
            .field("digest", &self.to_hex())
            .finish()
    }
}

impl fmt::Display for SchemaFingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}:v{}:p{}:{}",
            self.algorithm_id,
            self.canonicalization_version,
            self.profile_id,
            self.to_hex()
        )
    }
}

/// Writes a length-prefixed UTF-8 string into the buffer.
fn write_str(buf: &mut Vec<u8>, s: &str) {
    let len = u32::try_from(s.len()).expect("string length fits in u32");
    buf.extend_from_slice(&len.to_be_bytes());
    buf.extend_from_slice(s.as_bytes());
}

/// Builds the canonical SHA-256 input bytes for a table per the v1
/// persistence profile (FINGERPRINT_SPEC §10.1).
///
/// This is the low-level introspection counterpart of
/// [`SchemaFingerprint`] — it exposes the bytes that get hashed, useful
/// for golden-vector tests and for downstream consumers that need to
/// verify "what gets hashed" independently of the digest.
///
/// Fails fast (no bytes produced) when the canonical model is malformed
/// — see [`FingerprintError`] and FINGERPRINT_SPEC §10.2.
///
/// # Errors
///
/// Returns [`FingerprintError`] when the input table fails validation, or when
/// `database` does not hold the table.
pub fn canonical_bytes_v1<T: TableLike>(
    table: &T,
    database: &T::DB,
) -> Result<Vec<u8>, FingerprintError> {
    // Collect the columns and the ordinals reported by `column_id`. The
    // encoding writes iteration index as the ordinal; the validator
    // checks that `column_id` agrees with the iteration index, catching
    // any `TableLike` implementer that reports inconsistent ordinals.
    let columns: Vec<_> = table.columns(database)?.collect();
    let col_count = u32::try_from(columns.len()).expect("column count fits in u32");

    let mut column_ordinals: Vec<u32> = Vec::with_capacity(columns.len());
    for column in &columns {
        column_ordinals.push(
            column.column_id(database)?.and_then(|id| u32::try_from(id).ok()).unwrap_or(u32::MAX),
        );
    }

    let mut pk_ordinals: Vec<u32> = Vec::new();
    for column in table.primary_key_columns(database)? {
        if let Some(id) = column.column_id(database)? {
            pk_ordinals.push(u32::try_from(id).expect("pk ordinal fits in u32"));
        }
    }

    validate_v1_layout_inner(col_count, &column_ordinals, &pk_ordinals)?;

    let mut buf = Vec::with_capacity(256);

    // Magic + version + profile id (FINGERPRINT_SPEC §10.1)
    buf.extend_from_slice(MAGIC);
    buf.extend_from_slice(&FINGERPRINT_VERSION.to_be_bytes());
    buf.extend_from_slice(&PROFILE_PERSISTENCE_V1.to_be_bytes());

    // Schema name (empty string if None)
    let schema_name = table.stored_table_schema().unwrap_or(Cow::Borrowed(""));
    write_str(&mut buf, &schema_name);

    // Table name
    let table_name = table.stored_table_name();
    write_str(&mut buf, &table_name);

    // Columns
    buf.extend_from_slice(&col_count.to_be_bytes());

    for (ordinal, column) in columns.iter().enumerate() {
        let ordinal_u32 = u32::try_from(ordinal).expect("ordinal fits in u32");
        buf.extend_from_slice(&ordinal_u32.to_be_bytes());

        let col_name = column.stored_column_name();
        write_str(&mut buf, &col_name);

        let type_token = canonical_type_token(&column.data_type(database).name(database));
        write_str(&mut buf, &type_token);

        buf.push(u8::from(column.is_nullable(database)?));
        buf.push(u8::from(column.is_generated(database)));
    }

    // Primary key ordinals
    let pk_count = u32::try_from(pk_ordinals.len()).expect("pk count fits in u32");
    buf.extend_from_slice(&pk_count.to_be_bytes());
    for ord in &pk_ordinals {
        buf.extend_from_slice(&ord.to_be_bytes());
    }

    Ok(buf)
}

/// Computes a v1 schema fingerprint for the given table.
///
/// Fails fast (without producing a digest) when the canonical model is
/// malformed — see [`FingerprintError`] and FINGERPRINT_SPEC §10.2.
pub(crate) fn compute_persistence_v1<T: TableLike>(
    table: &T,
    database: &T::DB,
) -> Result<SchemaFingerprint, FingerprintError> {
    let buf = canonical_bytes_v1(table, database)?;

    // SHA-256
    let hash = Sha256::digest(&buf);
    let digest: [u8; 32] = hash.into();

    Ok(SchemaFingerprint::new(
        AlgorithmId::Sha2_256,
        FINGERPRINT_VERSION,
        PROFILE_PERSISTENCE_V1,
        digest,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------------------------------------------------------
    // Fingerprint validator (audit §4, P-12 / spec §10.2 — COL-003).
    //
    // The validator MUST reject malformed canonical models. Tests
    // exercise the pure inner predicate so that contract violations are
    // caught regardless of which `TableLike` impl drives the encoding.
    // ---------------------------------------------------------------

    #[test]
    fn test_col_003_valid_layout_passes() {
        // 2 columns, ordinals [0, 1], single PK at ordinal 0 — well-formed.
        assert!(validate_v1_layout_inner(2, &[0, 1], &[0]).is_ok());
    }

    #[test]
    fn test_col_003_non_contiguous_ordinals() {
        // ordinals [0, 2] — index 1 reports ordinal 2 instead of 1.
        let err = validate_v1_layout_inner(2, &[0, 2], &[]).expect_err("must reject");
        assert!(
            matches!(err, FingerprintError::NonContiguousOrdinals { expected: 1, got: 2 }),
            "expected NonContiguousOrdinals{{1,2}}, got {err:?}"
        );
    }

    #[test]
    fn test_col_003_duplicate_pk_ordinal() {
        // pk ordinals = [0, 0] — ordinal 0 appears twice.
        let err = validate_v1_layout_inner(2, &[0, 1], &[0, 0]).expect_err("must reject");
        assert!(
            matches!(err, FingerprintError::DuplicatePkOrdinal(0)),
            "expected DuplicatePkOrdinal(0), got {err:?}"
        );
    }

    #[test]
    fn test_col_003_pk_out_of_range() {
        // pk ordinal 5 with only 2 columns — out of range.
        let err = validate_v1_layout_inner(2, &[0, 1], &[5]).expect_err("must reject");
        assert!(
            matches!(err, FingerprintError::PkOrdinalOutOfRange { ordinal: 5, column_count: 2 }),
            "expected PkOrdinalOutOfRange{{5,2}}, got {err:?}"
        );
    }
}
