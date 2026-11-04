//! Golden-vector conformance tests for the v1 fingerprint encoding
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable)]
//! (FINGERPRINT_SPEC §14, audit §9 / P-14).
//!
//! Each vector pins both the canonical SHA-256 input bytes (hex) and
//! the resulting digest (fp256/128/64). Any encoding drift is caught
//! before the SHA-256 step.
//!
//! # Coverage (audit §9 mandatory list)
//! - single-column INT PK
//! - composite PK in both orders (SEN-007)
//! - every canonical type token in `fingerprint_type_token::match_known_type`:
//!   INT, FLOAT, DECIMAL, STRING, BYTES, BOOL, DATE, TIME, TIMESTAMP,
//!   TIMESTAMPTZ, UUID, JSON, JSONB
//! - one `OTHER:` token (via GEOGRAPHY)
//! - a generated/identity column (SERIAL)
//! - nullable and non-nullable variants of the same schema
//! - empty schema name vs explicit schema name
//! - a Unicode-NFC-divergent identifier pair (precomposed vs decomposed)

use sha2::{Digest, Sha256};
use sql_traits::{
    structs::{IdentifierCase, ParserDB, TargetName, canonical_bytes_v1},
    traits::{DatabaseLike, TableLike},
};
use sqlparser::dialect::GenericDialect;

/// A single golden vector.
struct GoldenVector {
    id: &'static str,
    description: &'static str,
    sql: &'static str,
    schema: Option<&'static str>,
    table_name: &'static str,
    canonical_bytes_hex: &'static str,
    fp256: &'static str,
    fp128: &'static str,
    fp64: u64,
}

const GOLDEN_VECTORS: &[GoldenVector] = &[
    GoldenVector {
        id: "v01_single_int_pk",
        description: "Minimal single-column INT primary key",
        sql: "CREATE TABLE t (id INT PRIMARY KEY);",
        schema: None,
        table_name: "t",
        canonical_bytes_hex: "5346503100010001000000000000000174000000010000000000000002696400000003494e5400000000000100000000",
        fp256: "908e455ca648742d8fab9bf9ff42abd2c0f53c845c9e22b18f2643312407269a",
        fp128: "908e455ca648742d8fab9bf9ff42abd2",
        fp64: 10_416_339_252_383_609_901_u64,
    },
    GoldenVector {
        id: "v02_users_int_pk_text",
        description: "Legacy `users(id INT PK, name TEXT)` fixture",
        sql: "CREATE TABLE users (id INT PRIMARY KEY, name TEXT);",
        schema: None,
        table_name: "users",
        canonical_bytes_hex: "534650310001000100000000000000057573657273000000020000000000000002696400000003494e54000000000001000000046e616d6500000006535452494e4701000000000100000000",
        fp256: "961dddeb22561e74e0e58c55b43afd875c5eb6b1b030facb48ec1237acf6f9d3",
        fp128: "961dddeb22561e74e0e58c55b43afd87",
        fp64: 10_817_045_881_976_921_716_u64,
    },
    GoldenVector {
        id: "v03_composite_pk_ab",
        description: "Composite PK declared as (a, b)",
        sql: "CREATE TABLE t (a INT, b INT, PRIMARY KEY (a, b));",
        schema: None,
        table_name: "t",
        canonical_bytes_hex: "53465031000100010000000000000001740000000200000000000000016100000003494e54000000000001000000016200000003494e540000000000020000000000000001",
        fp256: "891fb81c7ab8d59561fb7aa02f59943cb719d75630dd1e9bb59f178176a3d46c",
        fp128: "891fb81c7ab8d59561fb7aa02f59943c",
        fp64: 9_880_818_539_931_686_293_u64,
    },
    GoldenVector {
        id: "v04_composite_pk_ba",
        description: "Composite PK declared as (b, a) — SEN-007",
        sql: "CREATE TABLE t (a INT, b INT, PRIMARY KEY (b, a));",
        schema: None,
        table_name: "t",
        canonical_bytes_hex: "53465031000100010000000000000001740000000200000000000000016100000003494e54000000000001000000016200000003494e540000000000020000000100000000",
        fp256: "53681ef48a8511bc05a131cca5d4efb599e50d5bb391c3c38b93701c33644887",
        fp128: "53681ef48a8511bc05a131cca5d4efb5",
        fp64: 6_010_087_738_370_757_052_u64,
    },
    GoldenVector {
        id: "v05_token_int_via_smallint",
        description: "SMALLINT folds to canonical INT token",
        sql: "CREATE TABLE t (id INT PRIMARY KEY, level SMALLINT);",
        schema: None,
        table_name: "t",
        canonical_bytes_hex: "5346503100010001000000000000000174000000020000000000000002696400000003494e54000000000001000000056c6576656c00000003494e5401000000000100000000",
        fp256: "adc512c0e6d6c97b6cd508080e8ef415958712ffddb09c7cd307179e82ff9740",
        fp128: "adc512c0e6d6c97b6cd508080e8ef415",
        fp64: 12_521_434_958_689_388_923_u64,
    },
    GoldenVector {
        id: "v06_token_float",
        description: "REAL folds to canonical FLOAT token",
        sql: "CREATE TABLE t (id INT PRIMARY KEY, score REAL);",
        schema: None,
        table_name: "t",
        canonical_bytes_hex: "5346503100010001000000000000000174000000020000000000000002696400000003494e540000000000010000000573636f726500000005464c4f415401000000000100000000",
        fp256: "3d08b1c417f1c7802b39035120b6b11e979d291de367611a91b71add15975864",
        fp128: "3d08b1c417f1c7802b39035120b6b11e",
        fp64: 4_397_960_491_900_716_928_u64,
    },
    GoldenVector {
        id: "v07_token_string_varchar",
        description: "VARCHAR folds to canonical STRING token",
        sql: "CREATE TABLE t (id INT PRIMARY KEY, name VARCHAR);",
        schema: None,
        table_name: "t",
        canonical_bytes_hex: "5346503100010001000000000000000174000000020000000000000002696400000003494e54000000000001000000046e616d6500000006535452494e4701000000000100000000",
        fp256: "516a7c3331156b892ffba355c51ac3c4f5e9c0550064f6aad052d0d1663cca1b",
        fp128: "516a7c3331156b892ffba355c51ac3c4",
        fp64: 5_866_638_023_912_156_041_u64,
    },
    GoldenVector {
        id: "v08_token_bool",
        description: "BOOLEAN folds to canonical BOOL token",
        sql: "CREATE TABLE t (id INT PRIMARY KEY, active BOOLEAN);",
        schema: None,
        table_name: "t",
        canonical_bytes_hex: "5346503100010001000000000000000174000000020000000000000002696400000003494e540000000000010000000661637469766500000004424f4f4c01000000000100000000",
        fp256: "a7766ed9cb49731e6c1f7b2cd5eb41250b6a300958934234c318b071ce981de1",
        fp128: "a7766ed9cb49731e6c1f7b2cd5eb4125",
        fp64: 12_066_954_133_283_369_758_u64,
    },
    GoldenVector {
        id: "v09_token_timestamp",
        description: "TIMESTAMP canonical token",
        sql: "CREATE TABLE t (id INT PRIMARY KEY, created_at TIMESTAMP);",
        schema: None,
        table_name: "t",
        canonical_bytes_hex: "5346503100010001000000000000000174000000020000000000000002696400000003494e540000000000010000000a637265617465645f61740000000954494d455354414d5001000000000100000000",
        fp256: "c221cb991b3d03218bfda966dabddc2d740e1d072ed36441650f96812f5f5ffd",
        fp128: "c221cb991b3d03218bfda966dabddc2d",
        fp64: 13_988_685_776_036_889_377_u64,
    },
    GoldenVector {
        id: "v10_token_timestamptz",
        description: "TIMESTAMP WITH TIME ZONE folds to distinct TIMESTAMPTZ token",
        sql: "CREATE TABLE t (id INT PRIMARY KEY, created_at TIMESTAMP WITH TIME ZONE);",
        schema: None,
        table_name: "t",
        canonical_bytes_hex: "5346503100010001000000000000000174000000020000000000000002696400000003494e540000000000010000000a637265617465645f61740000000b54494d455354414d50545a01000000000100000000",
        fp256: "0bde2036f32c1000c5488b7dc12e46d38d5f0e1fac77ca2cd9e331c7072b0e02",
        fp128: "0bde2036f32c1000c5488b7dc12e46d3",
        fp64: 855_156_399_627_046_912_u64,
    },
    GoldenVector {
        id: "v11_token_uuid",
        description: "UUID canonical token",
        sql: "CREATE TABLE t (id UUID PRIMARY KEY, name TEXT);",
        schema: None,
        table_name: "t",
        canonical_bytes_hex: "534650310001000100000000000000017400000002000000000000000269640000000455554944000000000001000000046e616d6500000006535452494e4701000000000100000000",
        fp256: "c213666fc1d6a7805d9bab4bf94923de6da605ebb1e2a4bdb59f88fce5f32303",
        fp128: "c213666fc1d6a7805d9bab4bf94923de",
        fp64: 13_984_633_898_094_995_328_u64,
    },
    GoldenVector {
        id: "v12_token_other_geography",
        description: "GEOGRAPHY(Point, 4326) emits an OTHER: token",
        sql: "CREATE TABLE t (id INT PRIMARY KEY, location GEOGRAPHY(Point, 4326));",
        schema: None,
        table_name: "t",
        canonical_bytes_hex: "5346503100010001000000000000000174000000020000000000000002696400000003494e54000000000001000000086c6f636174696f6e0000001c4f544845523a67656f67726170687928706f696e742c20343332362901000000000100000000",
        fp256: "a417caa343fde4241c712f367de9a7e0264528e956da9f1b3499fae43a503d60",
        fp128: "a417caa343fde4241c712f367de9a7e0",
        fp64: 11_824_142_149_253_719_076_u64,
    },
    GoldenVector {
        id: "v13_generated_serial",
        description: "SERIAL column is recorded as generated_flag=1",
        sql: "CREATE TABLE t (id SERIAL PRIMARY KEY, name TEXT);",
        schema: None,
        table_name: "t",
        canonical_bytes_hex: "5346503100010001000000000000000174000000020000000000000002696400000003494e54000100000001000000046e616d6500000006535452494e4701000000000100000000",
        fp256: "478ef83861f29bc77098fee8d89b0aaf3b4daba0e62e9b833175a6dd415f4809",
        fp128: "478ef83861f29bc77098fee8d89b0aaf",
        fp64: 5_156_331_544_430_943_175_u64,
    },
    GoldenVector {
        id: "v14_nullable_name",
        description: "name TEXT (nullable variant) — equals v07 by construction",
        sql: "CREATE TABLE t (id INT PRIMARY KEY, name TEXT);",
        schema: None,
        table_name: "t",
        canonical_bytes_hex: "5346503100010001000000000000000174000000020000000000000002696400000003494e54000000000001000000046e616d6500000006535452494e4701000000000100000000",
        fp256: "516a7c3331156b892ffba355c51ac3c4f5e9c0550064f6aad052d0d1663cca1b",
        fp128: "516a7c3331156b892ffba355c51ac3c4",
        fp64: 5_866_638_023_912_156_041_u64,
    },
    GoldenVector {
        id: "v15_not_null_name",
        description: "name TEXT NOT NULL (non-nullable variant) — differs from v14 only on the nullable_flag",
        sql: "CREATE TABLE t (id INT PRIMARY KEY, name TEXT NOT NULL);",
        schema: None,
        table_name: "t",
        canonical_bytes_hex: "5346503100010001000000000000000174000000020000000000000002696400000003494e54000000000001000000046e616d6500000006535452494e4700000000000100000000",
        fp256: "f1dd4c3c8cf856e8539ec0b6626703d7349eff319d79e93495b7a29da65e0f0d",
        fp128: "f1dd4c3c8cf856e8539ec0b6626703d7",
        fp64: 17_428_169_955_940_521_704_u64,
    },
    GoldenVector {
        id: "v16_no_schema",
        description: "Table with empty (None) schema qualifier — equals v01",
        sql: "CREATE TABLE t (id INT PRIMARY KEY);",
        schema: None,
        table_name: "t",
        canonical_bytes_hex: "5346503100010001000000000000000174000000010000000000000002696400000003494e5400000000000100000000",
        fp256: "908e455ca648742d8fab9bf9ff42abd2c0f53c845c9e22b18f2643312407269a",
        fp128: "908e455ca648742d8fab9bf9ff42abd2",
        fp64: 10_416_339_252_383_609_901_u64,
    },
    GoldenVector {
        id: "v17_explicit_schema",
        description: "Table with explicit `my_schema` qualifier — differs from v01 only on the schema-name bytes",
        sql: "CREATE SCHEMA my_schema; CREATE TABLE my_schema.t (id INT PRIMARY KEY);",
        schema: Some("my_schema"),
        table_name: "t",
        canonical_bytes_hex: "5346503100010001000000096d795f736368656d610000000174000000010000000000000002696400000003494e5400000000000100000000",
        fp256: "da72d09475940c9cfba5d9995937d667d8159c35eaac05af0a323b88fe646723",
        fp128: "da72d09475940c9cfba5d9995937d667",
        fp64: 15_740_872_983_659_678_876_u64,
    },
    GoldenVector {
        id: "v18_nfc_precomposed",
        description: "Table name `café` precomposed (U+00E9)",
        sql: "CREATE TABLE \"caf\u{00e9}\" (id INT PRIMARY KEY);",
        schema: None,
        table_name: "caf\u{00e9}",
        canonical_bytes_hex: "53465031000100010000000000000005636166c3a9000000010000000000000002696400000003494e5400000000000100000000",
        fp256: "cad0a33f82b489f403e511365bf787151f68f5ab60d792f1b1a2dc7a650fc262",
        fp128: "cad0a33f82b489f403e511365bf78715",
        fp64: 14_614_360_283_988_396_532_u64,
    },
    GoldenVector {
        id: "v19_nfc_decomposed",
        description: "Table name `café` decomposed (U+0065 U+0301) — same digest as v18",
        sql: "CREATE TABLE \"cafe\u{0301}\" (id INT PRIMARY KEY);",
        schema: None,
        table_name: "cafe\u{0301}",
        canonical_bytes_hex: "53465031000100010000000000000005636166c3a9000000010000000000000002696400000003494e5400000000000100000000",
        fp256: "cad0a33f82b489f403e511365bf787151f68f5ab60d792f1b1a2dc7a650fc262",
        fp128: "cad0a33f82b489f403e511365bf78715",
        fp64: 14_614_360_283_988_396_532_u64,
    },
    GoldenVector {
        id: "v20_quoted_mixed_case",
        description: "Quoted `\"Foo\"` preserves case after NFC",
        sql: "CREATE TABLE \"Foo\" (id INT PRIMARY KEY);",
        schema: None,
        // Quoted lookup form: stored "Foo" is case-preserved, so the
        // textual lookup must also be quoted to match.
        table_name: "\"Foo\"",
        canonical_bytes_hex: "53465031000100010000000000000003466f6f000000010000000000000002696400000003494e5400000000000100000000",
        fp256: "5f046a221985fbe17bac7e90903c73d2f062a882df4b7fde2cf5773136346fe8",
        fp128: "5f046a221985fbe17bac7e90903c73d2",
        fp64: 6_846_714_028_199_640_033_u64,
    },
    GoldenVector {
        id: "v21_composite_pk_typed_mix",
        description: "Composite PK over mixed types",
        sql: "CREATE TABLE t (a INT, b TEXT, c REAL, PRIMARY KEY (a, b));",
        schema: None,
        table_name: "t",
        canonical_bytes_hex: "53465031000100010000000000000001740000000300000000000000016100000003494e54000000000001000000016200000006535452494e47000000000002000000016300000005464c4f41540100000000020000000000000001",
        fp256: "274ee27859e004bb7c2712398414a7f9c74db21269f3064ffb67c05ec53cc2d9",
        fp128: "274ee27859e004bb7c2712398414a7f9",
        fp64: 2_832_450_222_194_427_067_u64,
    },
    GoldenVector {
        id: "v22_token_decimal",
        description: "DECIMAL canonical token (via DataType::Decimal)",
        sql: "CREATE TABLE t (id INT PRIMARY KEY, price DECIMAL);",
        schema: None,
        table_name: "t",
        canonical_bytes_hex: "5346503100010001000000000000000174000000020000000000000002696400000003494e5400000000000100000005707269636500000007444543494d414c01000000000100000000",
        fp256: "6deae7d0ac2837f72d88b622050fda4258b3d815d094b923d7de2225e1d019bc",
        fp128: "6deae7d0ac2837f72d88b622050fda42",
        fp64: 7_920_397_778_111_969_271_u64,
    },
    GoldenVector {
        id: "v23_token_bytes_bytea",
        description: "BYTES canonical token (via DataType::Bytea)",
        sql: "CREATE TABLE t (id INT PRIMARY KEY, payload BYTEA);",
        schema: None,
        table_name: "t",
        canonical_bytes_hex: "5346503100010001000000000000000174000000020000000000000002696400000003494e54000000000001000000077061796c6f616400000005425954455301000000000100000000",
        fp256: "103b39e3bcb084a40a10c0608e2a71f250498ff4fe08632f8c21906960ec158d",
        fp128: "103b39e3bcb084a40a10c0608e2a71f2",
        fp64: 1_169_592_178_518_820_004_u64,
    },
    GoldenVector {
        id: "v24_token_date",
        description: "DATE canonical token",
        sql: "CREATE TABLE t (id INT PRIMARY KEY, day DATE);",
        schema: None,
        table_name: "t",
        canonical_bytes_hex: "5346503100010001000000000000000174000000020000000000000002696400000003494e5400000000000100000003646179000000044441544501000000000100000000",
        fp256: "f5f2d49c51adfe8cf6071baf67a6b015c691c650372933cae81d93a2b5cc55b8",
        fp128: "f5f2d49c51adfe8cf6071baf67a6b015",
        fp64: 17_722_461_251_506_667_148_u64,
    },
    GoldenVector {
        id: "v25_token_time",
        description: "TIME canonical token",
        sql: "CREATE TABLE t (id INT PRIMARY KEY, t TIME);",
        schema: None,
        table_name: "t",
        canonical_bytes_hex: "5346503100010001000000000000000174000000020000000000000002696400000003494e5400000000000100000001740000000454494d4501000000000100000000",
        fp256: "e73d4b7f2566a5d453b0709654c6816ff0a872524882c82d251e5de433484895",
        fp128: "e73d4b7f2566a5d453b0709654c6816f",
        fp64: 16_662_557_205_801_117_140_u64,
    },
    GoldenVector {
        id: "v26_token_json",
        description: "JSON canonical token",
        sql: "CREATE TABLE t (id INT PRIMARY KEY, payload JSON);",
        schema: None,
        table_name: "t",
        canonical_bytes_hex: "5346503100010001000000000000000174000000020000000000000002696400000003494e54000000000001000000077061796c6f6164000000044a534f4e01000000000100000000",
        fp256: "9b4e0f4a6141b318668dbc0811ad56f11d5328ab39c875e26bd12114b39ae868",
        fp128: "9b4e0f4a6141b318668dbc0811ad56f1",
        fp64: 11_190_898_936_195_953_432_u64,
    },
    GoldenVector {
        id: "v27_token_jsonb",
        description: "JSONB canonical token (distinct from JSON)",
        sql: "CREATE TABLE t (id INT PRIMARY KEY, payload JSONB);",
        schema: None,
        table_name: "t",
        canonical_bytes_hex: "5346503100010001000000000000000174000000020000000000000002696400000003494e54000000000001000000077061796c6f6164000000054a534f4e4201000000000100000000",
        fp256: "42ae4713ee3fabf30331385c4971f7194984447de9d1f2a2a1f0fd8ee3d5944a",
        fp128: "42ae4713ee3fabf30331385c4971f719",
        fp64: 4_804_856_003_377_998_835_u64,
    },
];

/// The name a golden vector writes for its table, read out of the text the
/// vector carries so a quoted spelling stays quoted.
fn target<'a>(schema: Option<&'a str>, name: &'a str) -> TargetName<'a> {
    let target = TargetName::parse(name).expect("the vector names one identifier");
    match schema {
        Some(schema) => target.with_schema(schema, false),
        None => target,
    }
}

fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[test]
fn test_golden_vectors() {
    let mut failures: Vec<String> = Vec::new();

    for v in GOLDEN_VECTORS {
        let db = ParserDB::parse::<GenericDialect>(v.sql)
            .unwrap_or_else(|e| panic!("[{}] parse failed: {e:?}", v.id));
        let table = db
            .table_by_target(target(v.schema, v.table_name), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .unwrap_or_else(|| panic!("[{}] table `{}` not found", v.id, v.table_name));

        let bytes = canonical_bytes_v1(table, &db)
            .unwrap_or_else(|e| panic!("[{}] canonical_bytes_v1 failed: {e:?}", v.id));
        let digest: [u8; 32] = Sha256::digest(&bytes).into();
        let fp = table
            .schema_fingerprint(&db)
            .unwrap_or_else(|e| panic!("[{}] schema_fingerprint failed: {e:?}", v.id));

        let actual_bytes_hex = to_hex(&bytes);
        let actual_fp256 = fp.to_hex();
        let actual_fp128 = to_hex(&fp.fingerprint128());
        let actual_fp64 = fp.fingerprint64();

        // Sanity: bytes hash to the digest we expose via `schema_fingerprint`.
        assert_eq!(
            to_hex(&digest),
            actual_fp256,
            "[{}] internal inconsistency: SHA-256(canonical_bytes) != fp.to_hex()",
            v.id,
        );

        let mut local_fail = false;
        if v.canonical_bytes_hex != actual_bytes_hex {
            failures.push(format!(
                "[{}] canonical_bytes_hex mismatch\n  expected: {}\n  actual:   {}",
                v.id, v.canonical_bytes_hex, actual_bytes_hex
            ));
            local_fail = true;
        }
        if v.fp256 != actual_fp256 {
            failures.push(format!(
                "[{}] fp256 mismatch\n  expected: {}\n  actual:   {}",
                v.id, v.fp256, actual_fp256
            ));
            local_fail = true;
        }
        if v.fp128 != actual_fp128 {
            failures.push(format!(
                "[{}] fp128 mismatch\n  expected: {}\n  actual:   {}",
                v.id, v.fp128, actual_fp128
            ));
            local_fail = true;
        }
        if v.fp64 != actual_fp64 {
            failures.push(format!(
                "[{}] fp64 mismatch\n  expected: {}\n  actual:   {}",
                v.id, v.fp64, actual_fp64
            ));
            local_fail = true;
        }

        if local_fail && std::env::var_os("GOLDEN_DUMP").is_some() {
            eprintln!(
                "// {}\nGoldenVector {{\n    id: \"{}\",\n    description: \"{}\",\n    sql: r#\"{}\"#,\n    schema: {:?},\n    table_name: {:?},\n    canonical_bytes_hex: \"{}\",\n    fp256: \"{}\",\n    fp128: \"{}\",\n    fp64: {}_u64,\n}},",
                v.description,
                v.id,
                v.description,
                v.sql,
                v.schema,
                v.table_name,
                actual_bytes_hex,
                actual_fp256,
                actual_fp128,
                actual_fp64,
            );
        }
    }

    // NFC equivalence cross-check: v18 (precomposed) and v19 (decomposed)
    // must produce identical canonical bytes after normalization.
    let v18 = GOLDEN_VECTORS.iter().find(|v| v.id == "v18_nfc_precomposed").unwrap();
    let v19 = GOLDEN_VECTORS.iter().find(|v| v.id == "v19_nfc_decomposed").unwrap();
    let db18 = ParserDB::parse::<GenericDialect>(v18.sql).expect("v18 parse");
    let db19 = ParserDB::parse::<GenericDialect>(v19.sql).expect("v19 parse");
    let t18 = db18
        .table_by_target(target(v18.schema, v18.table_name), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("v18 lookup");
    let t19 = db19
        .table_by_target(target(v19.schema, v19.table_name), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("v19 lookup");
    let b18 = canonical_bytes_v1(t18, &db18).expect("v18 bytes");
    let b19 = canonical_bytes_v1(t19, &db19).expect("v19 bytes");
    assert_eq!(
        b18, b19,
        "NFC pair must produce identical canonical bytes (precomposed vs decomposed)"
    );

    assert!(
        failures.is_empty(),
        "{} golden-vector mismatches (run with `GOLDEN_DUMP=1` to print the corrected fixtures):\n{}",
        failures.len(),
        failures.join("\n"),
    );
}

#[test]
fn test_at_least_twenty_golden_vectors() {
    // Spec §14 / audit §9 P-14: ≥20 vectors.
    assert!(
        GOLDEN_VECTORS.len() >= 20,
        "spec requires at least 20 golden vectors; found {}",
        GOLDEN_VECTORS.len()
    );
}
