//! The functions PostGIS 3.7.0 creates whose names sqlitegis also implements,
//! with the casts to `geography`, one row per `pg_proc` entry, generated on
//! PostgreSQL 18.7 by `CREATE SCHEMA postgis_ext`,
//! `CREATE EXTENSION postgis WITH SCHEMA postgis_ext` and
//!
//! ```sql
//! WITH members AS (
//!     SELECT p.oid
//!     FROM pg_proc p
//!     JOIN pg_depend d ON d.classid = 'pg_proc'::regclass AND d.objid = p.oid AND d.deptype = 'e'
//!     JOIN pg_extension e ON e.oid = d.refobjid AND e.extname = 'postgis'
//!     WHERE p.proname IN (
//!         'geography', 'st_area', 'st_asbinary', 'st_asewkb', 'st_asewkt',
//!         'st_asgeojson', 'st_astext', 'st_azimuth', 'st_buffer', 'st_centroid',
//!         'st_closestpoint', 'st_collect', 'st_contains', 'st_coorddim', 'st_coveredby',
//!         'st_covers', 'st_crosses', 'st_difference', 'st_dimension', 'st_disjoint',
//!         'st_distance', 'st_distancesphere', 'st_distancespheroid', 'st_dwithin', 'st_endpoint',
//!         'st_envelope', 'st_equals', 'st_exteriorring', 'st_geometryn', 'st_geometrytype',
//!         'st_geomfromewkb', 'st_geomfromgeojson', 'st_geomfromtext', 'st_geomfromwkb', 'st_hausdorffdistance',
//!         'st_interiorringn', 'st_intersection', 'st_intersects', 'st_isempty', 'st_isvalid',
//!         'st_isvalidreason', 'st_length', 'st_lengthspheroid', 'st_lineinterpolatepoint', 'st_lineinterpolatepoints',
//!         'st_linesubstring', 'st_makeenvelope', 'st_makeline', 'st_makepolygon', 'st_memsize',
//!         'st_ndims', 'st_npoints', 'st_numgeometries', 'st_numinteriorrings', 'st_numpoints',
//!         'st_overlaps', 'st_perimeter', 'st_point', 'st_pointn', 'st_pointonsurface',
//!         'st_project', 'st_relate', 'st_relatematch', 'st_segmentize', 'st_setsrid',
//!         'st_srid', 'st_startpoint', 'st_symdifference', 'st_tileenvelope', 'st_touches',
//!         'st_union', 'st_within', 'st_x', 'st_xmax', 'st_xmin',
//!         'st_y', 'st_ymax', 'st_ymin', 'st_z', 'st_zmflag'
//!     )
//! ),
//! arguments AS (
//!     SELECT
//!         p.oid,
//!         a.position,
//!         coalesce(p.proargmodes[a.position]::text, CASE
//!             WHEN p.provariadic <> 0 AND a.position = p.pronargs THEN 'v'
//!             ELSE 'i'
//!         END) AS mode,
//!         nullif(p.proargnames[a.position], '') AS name,
//!         a.type
//!     FROM pg_proc p
//!     JOIN members USING (oid)
//!     CROSS JOIN LATERAL unnest(coalesce(p.proallargtypes, p.proargtypes::oid[]))
//!         WITH ORDINALITY AS a(type, position)
//! ),
//! lists AS (
//!     SELECT
//!         oid,
//!         string_agg(
//!             concat_ws(' ',
//!                 CASE mode WHEN 'o' THEN 'OUT' WHEN 'b' THEN 'INOUT' WHEN 'v' THEN 'VARIADIC' END,
//!                 quote_ident(name),
//!                 format_type(type, NULL),
//!                 'DEFAULT ' || pg_get_function_arg_default(oid, position::int)
//!             ),
//!             ', ' ORDER BY position
//!         ) FILTER (WHERE mode <> 't') AS arguments,
//!         string_agg(quote_ident(name) || ' ' || format_type(type, NULL), ', ' ORDER BY position)
//!             FILTER (WHERE mode = 't') AS columns
//!     FROM arguments
//!     GROUP BY oid
//! )
//! SELECT replace(format(
//!     '    PostgresBuiltin(%s, %s, %s, %s, %s, %s, %s, %s, %s, %s),',
//!     to_json(p.proname::text),
//!     to_json(coalesce(lists.arguments, '')),
//!     to_json(CASE
//!         WHEN lists.columns IS NOT NULL THEN 'TABLE(' || lists.columns || ')'
//!         WHEN p.proretset THEN 'SETOF ' || format_type(p.prorettype, NULL)
//!         ELSE format_type(p.prorettype, NULL)
//!     END),
//!     CASE WHEN p.proisstrict THEN 'Strict' ELSE 'CalledOnNull' END,
//!     CASE p.provolatile WHEN 'i' THEN 'Immutable' WHEN 's' THEN 'Stable' ELSE 'Volatile' END,
//!     CASE WHEN p.proleakproof THEN 'Leakproof' ELSE 'NotLeakproof' END,
//!     CASE p.prokind
//!         WHEN 'w' THEN 'Window'
//!         WHEN 'a' THEN CASE a.aggkind
//!             WHEN 'o' THEN format('OrderedSet(%s)', a.aggnumdirectargs)
//!             WHEN 'h' THEN format('HypotheticalSet(%s)', a.aggnumdirectargs)
//!             ELSE 'Aggregate'
//!         END
//!         ELSE 'Normal'
//!     END,
//!     CASE l.lanname WHEN 'internal' THEN 'Internal' WHEN 'c' THEN 'C' ELSE 'Sql' END,
//!     CASE WHEN p.prorows = trunc(p.prorows)
//!         THEN p.prorows::numeric || '.0'
//!         ELSE p.prorows::text
//!     END,
//!     CASE WHEN p.prokind = 'a' THEN format(
//!         'Some(AggregateState { initial_null: %s, transition_strict: %s, final_nonstrict: %s })',
//!         CASE WHEN a.agginitval IS NULL THEN 'true' ELSE 'false' END,
//!         CASE WHEN coalesce((SELECT proisstrict FROM pg_proc WHERE oid = a.aggtransfn), false)
//!             THEN 'true' ELSE 'false' END,
//!         CASE WHEN a.aggfinalfn <> 0
//!             AND NOT (SELECT proisstrict FROM pg_proc WHERE oid = a.aggfinalfn)
//!             THEN 'true' ELSE 'false' END
//!     ) ELSE 'None' END
//! ), 'postgis_ext.', '@extschema@.')
//! FROM pg_proc p
//! JOIN members USING (oid)
//! JOIN pg_language l ON l.oid = p.prolang
//! LEFT JOIN pg_aggregate a ON a.aggfnoid = p.oid
//! LEFT JOIN lists ON lists.oid = p.oid
//! ORDER BY lower(p.proname) COLLATE "C", p.proname COLLATE "C", p.oid;
//! ```
//!
//! The names are those of sqlitegis's function catalog that PostGIS also
//! defines. A type PostGIS creates is written after `@extschema@.`, which
//! stands for the schema the extension lands in.

use super::postgres_builtins::{
    AggregateState,
    Kind::{Aggregate, Normal},
    Language::{C, Internal, Sql},
    Leakage::NotLeakproof,
    NullInput::{CalledOnNull, Strict},
    PostgresBuiltin,
    Volatility::{Immutable, Stable},
};

/// The functions of PostGIS 3.7.0 modelled here, sorted by the name an
/// unquoted reference folds to.
#[rustfmt::skip]
pub(super) static POSTGIS_FUNCTIONS: &[PostgresBuiltin] = &[
    PostgresBuiltin("geography", "@extschema@.geography, integer, boolean", "@extschema@.geography", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("geography", "bytea", "@extschema@.geography", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("geography", "@extschema@.geometry", "@extschema@.geography", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_area", "@extschema@.geometry", "double precision", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_area", "geog @extschema@.geography, use_spheroid boolean DEFAULT true", "double precision", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_area", "text", "double precision", Strict, Immutable, NotLeakproof, Normal, Sql, 0.0, None),
    PostgresBuiltin("st_asbinary", "@extschema@.geometry, text", "bytea", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_asbinary", "@extschema@.geometry", "bytea", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_asbinary", "@extschema@.geography", "bytea", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_asbinary", "@extschema@.geography, text", "bytea", CalledOnNull, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_asewkb", "@extschema@.geometry", "bytea", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_asewkb", "@extschema@.geometry, text", "bytea", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_asewkt", "@extschema@.geometry", "text", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_asewkt", "@extschema@.geometry, integer", "text", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_asewkt", "@extschema@.geography", "text", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_asewkt", "@extschema@.geography, integer", "text", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_asewkt", "text", "text", Strict, Immutable, NotLeakproof, Normal, Sql, 0.0, None),
    PostgresBuiltin("st_asgeojson", "geom @extschema@.geometry, maxdecimaldigits integer DEFAULT 9, options integer DEFAULT 8", "text", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_asgeojson", "r record, geom_column text DEFAULT ''::text, maxdecimaldigits integer DEFAULT 9, pretty_bool boolean DEFAULT false, id_column text DEFAULT ''::text", "text", Strict, Stable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_asgeojson", "geog @extschema@.geography, maxdecimaldigits integer DEFAULT 9, options integer DEFAULT 0", "text", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_asgeojson", "text", "text", Strict, Immutable, NotLeakproof, Normal, Sql, 0.0, None),
    PostgresBuiltin("st_astext", "@extschema@.geometry", "text", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_astext", "@extschema@.geometry, integer", "text", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_astext", "@extschema@.geography", "text", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_astext", "@extschema@.geography, integer", "text", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_astext", "text", "text", Strict, Immutable, NotLeakproof, Normal, Sql, 0.0, None),
    PostgresBuiltin("st_azimuth", "geom1 @extschema@.geometry, geom2 @extschema@.geometry", "double precision", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_azimuth", "geog1 @extschema@.geography, geog2 @extschema@.geography", "double precision", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_buffer", "geom @extschema@.geometry, radius double precision, options text DEFAULT ''::text", "@extschema@.geometry", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_buffer", "geom @extschema@.geometry, radius double precision, quadsegs integer", "@extschema@.geometry", Strict, Immutable, NotLeakproof, Normal, Sql, 0.0, None),
    PostgresBuiltin("st_buffer", "@extschema@.geography, double precision", "@extschema@.geography", Strict, Immutable, NotLeakproof, Normal, Sql, 0.0, None),
    PostgresBuiltin("st_buffer", "@extschema@.geography, double precision, integer", "@extschema@.geography", Strict, Immutable, NotLeakproof, Normal, Sql, 0.0, None),
    PostgresBuiltin("st_buffer", "@extschema@.geography, double precision, text", "@extschema@.geography", Strict, Immutable, NotLeakproof, Normal, Sql, 0.0, None),
    PostgresBuiltin("st_buffer", "text, double precision", "@extschema@.geometry", Strict, Immutable, NotLeakproof, Normal, Sql, 0.0, None),
    PostgresBuiltin("st_buffer", "text, double precision, integer", "@extschema@.geometry", Strict, Immutable, NotLeakproof, Normal, Sql, 0.0, None),
    PostgresBuiltin("st_buffer", "text, double precision, text", "@extschema@.geometry", Strict, Immutable, NotLeakproof, Normal, Sql, 0.0, None),
    PostgresBuiltin("st_centroid", "@extschema@.geometry", "@extschema@.geometry", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_centroid", "@extschema@.geography, use_spheroid boolean DEFAULT true", "@extschema@.geography", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_centroid", "text", "@extschema@.geometry", Strict, Immutable, NotLeakproof, Normal, Sql, 0.0, None),
    PostgresBuiltin("st_closestpoint", "geom1 @extschema@.geometry, geom2 @extschema@.geometry", "@extschema@.geometry", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_closestpoint", "@extschema@.geography, @extschema@.geography, use_spheroid boolean DEFAULT true", "@extschema@.geography", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_closestpoint", "text, text", "@extschema@.geometry", CalledOnNull, Immutable, NotLeakproof, Normal, Sql, 0.0, None),
    PostgresBuiltin("st_collect", "geom1 @extschema@.geometry, geom2 @extschema@.geometry", "@extschema@.geometry", CalledOnNull, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_collect", "@extschema@.geometry[]", "@extschema@.geometry", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_collect", "@extschema@.geometry", "@extschema@.geometry", CalledOnNull, Immutable, NotLeakproof, Aggregate, Internal, 0.0, Some(AggregateState { initial_null: true, transition_strict: false, final_nonstrict: true })),
    PostgresBuiltin("st_contains", "geom1 @extschema@.geometry, geom2 @extschema@.geometry", "boolean", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_coorddim", "geometry @extschema@.geometry", "smallint", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_coveredby", "geom1 @extschema@.geometry, geom2 @extschema@.geometry", "boolean", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_coveredby", "geog1 @extschema@.geography, geog2 @extschema@.geography", "boolean", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_coveredby", "text, text", "boolean", CalledOnNull, Immutable, NotLeakproof, Normal, Sql, 0.0, None),
    PostgresBuiltin("st_covers", "geom1 @extschema@.geometry, geom2 @extschema@.geometry", "boolean", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_covers", "geog1 @extschema@.geography, geog2 @extschema@.geography", "boolean", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_covers", "text, text", "boolean", CalledOnNull, Immutable, NotLeakproof, Normal, Sql, 0.0, None),
    PostgresBuiltin("st_crosses", "geom1 @extschema@.geometry, geom2 @extschema@.geometry", "boolean", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_difference", "geom1 @extschema@.geometry, geom2 @extschema@.geometry, gridsize double precision DEFAULT '-1.0'::numeric", "@extschema@.geometry", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_dimension", "@extschema@.geometry", "integer", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_disjoint", "geom1 @extschema@.geometry, geom2 @extschema@.geometry", "boolean", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_distance", "geom1 @extschema@.geometry, geom2 @extschema@.geometry", "double precision", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_distance", "geog1 @extschema@.geography, geog2 @extschema@.geography, use_spheroid boolean DEFAULT true", "double precision", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_distance", "text, text", "double precision", Strict, Immutable, NotLeakproof, Normal, Sql, 0.0, None),
    PostgresBuiltin("st_distancesphere", "geom1 @extschema@.geometry, geom2 @extschema@.geometry", "double precision", Strict, Immutable, NotLeakproof, Normal, Sql, 0.0, None),
    PostgresBuiltin("st_distancesphere", "geom1 @extschema@.geometry, geom2 @extschema@.geometry, radius double precision", "double precision", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_distancespheroid", "geom1 @extschema@.geometry, geom2 @extschema@.geometry, @extschema@.spheroid", "double precision", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_distancespheroid", "geom1 @extschema@.geometry, geom2 @extschema@.geometry", "double precision", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_dwithin", "geom1 @extschema@.geometry, geom2 @extschema@.geometry, double precision", "boolean", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_dwithin", "geog1 @extschema@.geography, geog2 @extschema@.geography, tolerance double precision, use_spheroid boolean DEFAULT true", "boolean", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_dwithin", "text, text, double precision", "boolean", CalledOnNull, Immutable, NotLeakproof, Normal, Sql, 0.0, None),
    PostgresBuiltin("st_endpoint", "@extschema@.geometry", "@extschema@.geometry", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_envelope", "@extschema@.geometry", "@extschema@.geometry", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_equals", "geom1 @extschema@.geometry, geom2 @extschema@.geometry", "boolean", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_exteriorring", "@extschema@.geometry", "@extschema@.geometry", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_geometryn", "@extschema@.geometry, integer", "@extschema@.geometry", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_geometrytype", "@extschema@.geometry", "text", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_geomfromewkb", "bytea", "@extschema@.geometry", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_geomfromgeojson", "text", "@extschema@.geometry", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_geomfromgeojson", "json", "@extschema@.geometry", Strict, Immutable, NotLeakproof, Normal, Sql, 0.0, None),
    PostgresBuiltin("st_geomfromgeojson", "jsonb", "@extschema@.geometry", Strict, Immutable, NotLeakproof, Normal, Sql, 0.0, None),
    PostgresBuiltin("st_geomfromtext", "text", "@extschema@.geometry", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_geomfromtext", "text, integer", "@extschema@.geometry", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_geomfromwkb", "bytea", "@extschema@.geometry", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_geomfromwkb", "bytea, integer", "@extschema@.geometry", Strict, Immutable, NotLeakproof, Normal, Sql, 0.0, None),
    PostgresBuiltin("st_hausdorffdistance", "geom1 @extschema@.geometry, geom2 @extschema@.geometry", "double precision", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_hausdorffdistance", "geom1 @extschema@.geometry, geom2 @extschema@.geometry, double precision", "double precision", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_interiorringn", "@extschema@.geometry, integer", "@extschema@.geometry", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_intersection", "geom1 @extschema@.geometry, geom2 @extschema@.geometry, gridsize double precision DEFAULT '-1'::integer", "@extschema@.geometry", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_intersection", "@extschema@.geography, @extschema@.geography", "@extschema@.geography", Strict, Immutable, NotLeakproof, Normal, Sql, 0.0, None),
    PostgresBuiltin("st_intersection", "text, text", "@extschema@.geometry", Strict, Immutable, NotLeakproof, Normal, Sql, 0.0, None),
    PostgresBuiltin("st_intersects", "geom1 @extschema@.geometry, geom2 @extschema@.geometry", "boolean", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_intersects", "geog1 @extschema@.geography, geog2 @extschema@.geography", "boolean", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_intersects", "text, text", "boolean", CalledOnNull, Immutable, NotLeakproof, Normal, Sql, 0.0, None),
    PostgresBuiltin("st_isempty", "@extschema@.geometry", "boolean", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_isvalid", "@extschema@.geometry, integer", "boolean", Strict, Immutable, NotLeakproof, Normal, Sql, 0.0, None),
    PostgresBuiltin("st_isvalid", "@extschema@.geometry", "boolean", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_isvalidreason", "@extschema@.geometry", "text", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_isvalidreason", "@extschema@.geometry, integer", "text", Strict, Immutable, NotLeakproof, Normal, Sql, 0.0, None),
    PostgresBuiltin("st_length", "@extschema@.geometry", "double precision", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_length", "geog @extschema@.geography, use_spheroid boolean DEFAULT true", "double precision", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_length", "text", "double precision", Strict, Immutable, NotLeakproof, Normal, Sql, 0.0, None),
    PostgresBuiltin("st_lengthspheroid", "@extschema@.geometry, @extschema@.spheroid", "double precision", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_lineinterpolatepoint", "@extschema@.geometry, double precision", "@extschema@.geometry", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_lineinterpolatepoint", "@extschema@.geography, double precision, use_spheroid boolean DEFAULT true", "@extschema@.geography", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_lineinterpolatepoint", "text, double precision", "@extschema@.geometry", CalledOnNull, Immutable, NotLeakproof, Normal, Sql, 0.0, None),
    PostgresBuiltin("st_lineinterpolatepoints", "@extschema@.geometry, double precision, repeat boolean DEFAULT true", "@extschema@.geometry", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_lineinterpolatepoints", "@extschema@.geography, double precision, use_spheroid boolean DEFAULT true, repeat boolean DEFAULT true", "@extschema@.geography", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_lineinterpolatepoints", "text, double precision", "@extschema@.geometry", CalledOnNull, Immutable, NotLeakproof, Normal, Sql, 0.0, None),
    PostgresBuiltin("st_linesubstring", "@extschema@.geometry, double precision, double precision", "@extschema@.geometry", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_linesubstring", "@extschema@.geography, double precision, double precision", "@extschema@.geography", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_linesubstring", "text, double precision, double precision", "@extschema@.geometry", CalledOnNull, Immutable, NotLeakproof, Normal, Sql, 0.0, None),
    PostgresBuiltin("st_makeenvelope", "double precision, double precision, double precision, double precision, integer DEFAULT 0", "@extschema@.geometry", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_makeline", "@extschema@.geometry[]", "@extschema@.geometry", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_makeline", "geom1 @extschema@.geometry, geom2 @extschema@.geometry", "@extschema@.geometry", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_makeline", "@extschema@.geometry", "@extschema@.geometry", CalledOnNull, Immutable, NotLeakproof, Aggregate, Internal, 0.0, Some(AggregateState { initial_null: true, transition_strict: false, final_nonstrict: true })),
    PostgresBuiltin("st_makepolygon", "@extschema@.geometry, @extschema@.geometry[]", "@extschema@.geometry", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_makepolygon", "@extschema@.geometry", "@extschema@.geometry", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_memsize", "@extschema@.geometry", "integer", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_ndims", "@extschema@.geometry", "smallint", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_npoints", "@extschema@.geometry", "integer", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_numgeometries", "@extschema@.geometry", "integer", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_numinteriorrings", "@extschema@.geometry", "integer", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_numpoints", "@extschema@.geometry", "integer", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_overlaps", "geom1 @extschema@.geometry, geom2 @extschema@.geometry", "boolean", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_perimeter", "@extschema@.geometry", "double precision", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_perimeter", "geog @extschema@.geography, use_spheroid boolean DEFAULT true", "double precision", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_point", "double precision, double precision", "@extschema@.geometry", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_point", "double precision, double precision, srid integer", "@extschema@.geometry", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_pointn", "@extschema@.geometry, integer", "@extschema@.geometry", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_pointonsurface", "@extschema@.geometry", "@extschema@.geometry", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_project", "geom1 @extschema@.geometry, distance double precision, azimuth double precision", "@extschema@.geometry", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_project", "geom1 @extschema@.geometry, geom2 @extschema@.geometry, distance double precision", "@extschema@.geometry", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_project", "geog @extschema@.geography, distance double precision, azimuth double precision", "@extschema@.geography", CalledOnNull, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_project", "geog_from @extschema@.geography, geog_to @extschema@.geography, distance double precision", "@extschema@.geography", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_relate", "geom1 @extschema@.geometry, geom2 @extschema@.geometry", "text", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_relate", "geom1 @extschema@.geometry, geom2 @extschema@.geometry, integer", "text", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_relate", "geom1 @extschema@.geometry, geom2 @extschema@.geometry, text", "boolean", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_relatematch", "text, text", "boolean", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_segmentize", "@extschema@.geometry, double precision", "@extschema@.geometry", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_segmentize", "geog @extschema@.geography, max_segment_length double precision", "@extschema@.geography", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_setsrid", "geom @extschema@.geometry, srid integer", "@extschema@.geometry", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_setsrid", "geog @extschema@.geography, srid integer", "@extschema@.geography", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_srid", "geom @extschema@.geometry", "integer", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_srid", "geog @extschema@.geography", "integer", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_startpoint", "@extschema@.geometry", "@extschema@.geometry", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_symdifference", "geom1 @extschema@.geometry, geom2 @extschema@.geometry, gridsize double precision DEFAULT '-1.0'::numeric", "@extschema@.geometry", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_tileenvelope", "zoom integer, x integer, y integer, bounds @extschema@.geometry DEFAULT '0102000020110F00000200000093107C45F81B73C193107C45F81B73C193107C45F81B734193107C45F81B7341'::@extschema@.geometry, margin double precision DEFAULT 0.0", "@extschema@.geometry", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_touches", "geom1 @extschema@.geometry, geom2 @extschema@.geometry", "boolean", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_union", "geom1 @extschema@.geometry, geom2 @extschema@.geometry", "@extschema@.geometry", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_union", "geom1 @extschema@.geometry, geom2 @extschema@.geometry, gridsize double precision", "@extschema@.geometry", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_union", "@extschema@.geometry[]", "@extschema@.geometry", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_union", "@extschema@.geometry", "@extschema@.geometry", CalledOnNull, Immutable, NotLeakproof, Aggregate, Internal, 0.0, Some(AggregateState { initial_null: true, transition_strict: false, final_nonstrict: false })),
    PostgresBuiltin("st_union", "@extschema@.geometry, gridsize double precision", "@extschema@.geometry", CalledOnNull, Immutable, NotLeakproof, Aggregate, Internal, 0.0, Some(AggregateState { initial_null: true, transition_strict: false, final_nonstrict: false })),
    PostgresBuiltin("st_within", "geom1 @extschema@.geometry, geom2 @extschema@.geometry", "boolean", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_x", "@extschema@.geometry", "double precision", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_xmax", "@extschema@.box3d", "double precision", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_xmin", "@extschema@.box3d", "double precision", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_y", "@extschema@.geometry", "double precision", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_ymax", "@extschema@.box3d", "double precision", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_ymin", "@extschema@.box3d", "double precision", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_z", "@extschema@.geometry", "double precision", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
    PostgresBuiltin("st_zmflag", "@extschema@.geometry", "smallint", Strict, Immutable, NotLeakproof, Normal, C, 0.0, None),
];
