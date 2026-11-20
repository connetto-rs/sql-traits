//! The columns SQLite's `fts3`, `fts4`, `fts5`, `rtree` and `rtree_i32`
//! modules declare for a virtual table, read from its arguments the way each
//! module's constructor reads them, and the shadow tables the modules keep.

use alloc::{
    borrow::Cow,
    format,
    string::{String, ToString},
    sync::Arc,
    vec,
    vec::Vec,
};

use sqlparser::ast::{ColumnDef, CreateTable, DataType, Ident, ObjectName, ObjectNamePart};

use super::{
    NamedColumn, ParserDBBuilder, StoredTable, postgres_catalog::PostgresCatalog,
    schema_qualifiers_match, table_schema_qualifier, type_arrays::TypeArrays,
    validate_distinct_columns, views,
};
use crate::{
    errors::{Error, VirtualTableArgumentRefusal as Refusal},
    structs::{ParserDB, VirtualTableSource},
    traits::TableLike,
    utils::object_name::object_name_identifiers,
};

/// A module whose argument grammar `ParserDB` reads.
#[derive(Clone, Copy)]
enum Module {
    Fts3,
    Fts4,
    Fts5,
    Rtree,
    RtreeI32,
}

impl Module {
    /// The module SQLite finds under `name`, which it compares without case.
    fn named(name: &str) -> Option<Self> {
        [
            ("fts3", Self::Fts3),
            ("fts4", Self::Fts4),
            ("fts5", Self::Fts5),
            ("rtree", Self::Rtree),
            ("rtree_i32", Self::RtreeI32),
        ]
        .into_iter()
        .find_map(|(known, module)| known.eq_ignore_ascii_case(name).then_some(module))
    }

    /// The suffixes of the shadow tables the module keeps, as it spells them.
    const fn shadow_suffixes(self) -> &'static [&'static str] {
        match self {
            Self::Fts3 | Self::Fts4 => &["content", "docsize", "segdir", "segments", "stat"],
            Self::Fts5 => &["config", "content", "data", "docsize", "idx"],
            Self::Rtree | Self::RtreeI32 => &["node", "parent", "rowid"],
        }
    }
}

/// The suffix module `module_name` keeps a shadow table under, spelled as
/// the module spells it, when `suffix` names one without regard to case.
pub(crate) fn shadow_suffix(module_name: &str, suffix: &str) -> Option<&'static str> {
    Module::named(module_name)?
        .shadow_suffixes()
        .iter()
        .copied()
        .find(|known| known.eq_ignore_ascii_case(suffix))
}

/// Gives `create_table` the columns its module declares and returns the
/// names of those the module hides.
///
/// # Errors
///
/// Returns [`Error::UnknownVirtualTableModule`] for a module whose grammar
/// `ParserDB` does not read, [`Error::VirtualTableArgumentsRefused`] for
/// arguments the module refuses, [`Error::ColumnAlreadyExists`] for an
/// `rtree` column repeating another, and
/// [`Error::VirtualTableContentColumnsUnknown`] for an `fts4` table taking
/// its columns from a view whose columns are not known.
pub(super) fn declare_columns(
    builder: &ParserDBBuilder,
    create_table: &mut CreateTable,
    source: &VirtualTableSource,
) -> Result<Vec<String>, Error> {
    let table_name = create_table.table_name().to_string();
    let refuse = |refusal| {
        Error::VirtualTableArgumentsRefused {
            table_name: table_name.clone(),
            module_name: source.module_name().to_string(),
            refusal,
        }
    };
    let arguments = source.arguments();
    let declared = match Module::named(source.module_name()) {
        Some(Module::Fts3) => fts3(builder, create_table, arguments, false)?.map_err(refuse)?,
        Some(Module::Fts4) => fts3(builder, create_table, arguments, true)?.map_err(refuse)?,
        Some(Module::Fts5) => fts5(&table_name, arguments).map_err(refuse)?,
        Some(module @ (Module::Rtree | Module::RtreeI32)) => {
            let coordinate = if matches!(module, Module::RtreeI32) {
                DataType::Int(None)
            } else {
                DataType::Real
            };
            create_table.columns = rtree_columns(arguments, &coordinate).map_err(refuse)?;
            // SQLite declares the columns, refusing a repeated one, before it
            // counts the dimensions.
            validate_distinct_columns(create_table, builder.identifier_case())?;
            rtree_dimensions(arguments).map_err(refuse)?;
            return Ok(Vec::new());
        }
        None => {
            return Err(Error::UnknownVirtualTableModule {
                table_name,
                module_name: source.module_name().to_string(),
            });
        }
    };
    create_table.columns = declared
        .columns
        .into_iter()
        .chain(declared.hidden.iter().cloned())
        .map(|name| column(name, DataType::Unspecified))
        .collect();
    Ok(declared.hidden)
}

/// The columns a full-text module declares, the hidden ones apart.
struct FullTextColumns {
    columns: Vec<String>,
    hidden: Vec<String>,
}

/// An untyped or typed column of a virtual table, named by [`column_ident`].
fn column(name: String, data_type: DataType) -> ColumnDef {
    ColumnDef { name: column_ident(name), data_type, options: Vec::new() }
}

/// The identifier of a column of a virtual table named `name`, quoted when
/// `name` is not a plain identifier.
fn column_ident(name: String) -> Ident {
    let plain =
        name.chars().next().is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
            && name.chars().all(|character| character.is_ascii_alphanumeric() || character == '_');
    let mut ident = Ident::new(name);
    if !plain {
        ident.quote_style = Some('"');
    }
    ident
}

/// Reads the arguments of an `fts3` or `fts4` table as `fts3InitVtab` does.
///
/// The outer error is one looking up the content table raises, the inner one
/// what the module refuses.
fn fts3(
    builder: &ParserDBBuilder,
    create_table: &CreateTable,
    arguments: &[String],
    fts4: bool,
) -> Result<Result<FullTextColumns, Refusal>, Error> {
    let mut columns: Vec<Cow<'_, str>> = Vec::new();
    let mut tokenizer_given = false;
    let mut prefix: Option<(&str, String)> = None;
    let mut compress: Option<&str> = None;
    let mut uncompress: Option<&str> = None;
    let mut content: Option<String> = None;
    let mut language_id: Option<String> = None;
    let mut not_indexed: Vec<String> = Vec::new();
    for argument in arguments {
        let bytes = argument.as_bytes();
        if !tokenizer_given
            && bytes.len() > 8
            && bytes[..8].eq_ignore_ascii_case(b"tokenize")
            && !is_fts3_id_char(bytes[8])
        {
            tokenizer_given = true;
            continue;
        }
        let Some((key, value)) = argument.split_once('=').filter(|_| fts4) else {
            columns.push(Cow::Borrowed(argument));
            continue;
        };
        let value = fts3_dequote(value);
        let malformed = |value: String| Refusal::MalformedOption { option: key.to_string(), value };
        match key.to_ascii_lowercase().as_str() {
            "matchinfo" if !value.eq_ignore_ascii_case("fts3") => return Ok(Err(malformed(value))),
            "order"
                if !value.eq_ignore_ascii_case("asc") && !value.eq_ignore_ascii_case("desc") =>
            {
                return Ok(Err(malformed(value)));
            }
            "matchinfo" | "order" => {}
            "prefix" => prefix = Some((key, value)),
            "compress" => compress = Some(key),
            "uncompress" => uncompress = Some(key),
            "content" => content = Some(value),
            "languageid" => language_id = Some(value),
            "notindexed" => not_indexed.push(value),
            _ => return Ok(Err(Refusal::UnrecognizedOption { option: key.to_string() })),
        }
    }
    if let Some(content_table) = content {
        // A content table takes the place of compression.
        compress = None;
        uncompress = None;
        if columns.is_empty() {
            let Some(content_columns) = content_columns(builder, create_table, &content_table)?
            else {
                return Ok(Err(Refusal::ContentTableNotFound { content_table }));
            };
            columns = content_columns.into_iter().map(Cow::Owned).collect();
            if let Some(language_id) = &language_id
                && let Some(position) =
                    columns.iter().position(|column| column.eq_ignore_ascii_case(language_id))
            {
                columns.remove(position);
            }
        }
    }
    if columns.is_empty() {
        columns.push(Cow::Borrowed("content"));
    }
    if let Some((key, value)) = prefix
        && !fts3_prefix_is_valid(&value)
    {
        return Ok(Err(Refusal::MalformedOption { option: key.to_string(), value }));
    }
    let columns: Vec<String> =
        columns.iter().map(|column| fts3_dequote(fts3_first_token(column))).collect();
    if let Some(column_name) = not_indexed
        .into_iter()
        .find(|name| !columns.iter().any(|column| column.eq_ignore_ascii_case(name)))
    {
        return Ok(Err(Refusal::NotIndexedColumnNotFound { column_name }));
    }
    match (compress, uncompress) {
        (Some(option), None) => {
            return Ok(Err(Refusal::UnpairedOption {
                option: option.to_string(),
                missing: "uncompress".to_string(),
            }));
        }
        (None, Some(option)) => {
            return Ok(Err(Refusal::UnpairedOption {
                option: option.to_string(),
                missing: "compress".to_string(),
            }));
        }
        _ => {}
    }
    let hidden = vec![
        create_table.table_name().to_string(),
        "docid".to_string(),
        language_id.unwrap_or_else(|| "__langid".to_string()),
    ];
    Ok(Ok(FullTextColumns { columns, hidden }))
}

/// The names `SELECT *` gives over `content_table`, which `fts3ContentColumns`
/// reads in the schema of the virtual table, or `None` when no table or view
/// holds the name.
fn content_columns(
    builder: &ParserDBBuilder,
    create_table: &CreateTable,
    content_table: &str,
) -> Result<Option<Vec<String>>, Error> {
    let mut parts: Vec<ObjectNamePart> = create_table.name.0.clone();
    parts.pop();
    parts.push(ObjectNamePart::Identifier(Ident::new(content_table)));
    let name = ObjectName(parts);
    if let Some(table) = builder.resolve_table_object_name(&name)? {
        let hidden = builder
            .stored_metadata(table)
            .map_or(&[][..], |metadata| metadata.hidden_column_names());
        return Ok(Some(
            table
                .columns
                .iter()
                .filter(|column| !hidden.contains(&column.name.value))
                .map(|column| column.name.value.clone())
                .collect(),
        ));
    }
    let Some(view) = builder.resolve_view_object_name(&name)? else {
        return Ok(None);
    };
    let (names, enumerable) = views::output_names(builder, view.declaration())?;
    if !enumerable {
        return Err(Error::VirtualTableContentColumnsUnknown {
            table_name: create_table.table_name().to_string(),
            content_table: content_table.to_string(),
        });
    }
    Ok(Some(names.into_iter().map(|(name, _)| name).collect()))
}

/// Whether `c` continues a token as `sqlite3Fts3IsIdChar` reads it.
const fn is_fts3_id_char(c: u8) -> bool {
    c >= 0x80 || c.is_ascii_alphanumeric() || c == b'_' || c == b'$'
}

/// The first token of `text` as `sqlite3Fts3NextToken` reads it, quotes kept.
fn fts3_first_token(text: &str) -> &str {
    let bytes = text.as_bytes();
    let mut start = 0;
    loop {
        let Some(&first) = bytes.get(start) else {
            return "";
        };
        let end = match first {
            b'\'' | b'"' | b'`' => {
                let mut end = start;
                loop {
                    end += 1;
                    match bytes.get(end) {
                        None => break,
                        Some(&byte) if byte != first => {}
                        Some(_) => {
                            end += 1;
                            if bytes.get(end) != Some(&first) {
                                break;
                            }
                        }
                    }
                }
                end.min(bytes.len())
            }
            b'[' => {
                let close = bytes[start..].iter().position(|&byte| byte == b']');
                close.map_or(bytes.len(), |close| start + close + 1)
            }
            byte if is_fts3_id_char(byte) => {
                start + bytes[start..].iter().take_while(|&&byte| is_fts3_id_char(byte)).count()
            }
            _ => {
                start += 1;
                continue;
            }
        };
        return &text[start..end];
    }
}

/// Dequotes `text` as `sqlite3Fts3Dequote` does, leaving an unquoted text as
/// it is.
fn fts3_dequote(text: &str) -> String {
    let Some(close) = text.chars().next().and_then(closing_quote) else {
        return text.to_string();
    };
    dequote_with(&text[1..], close).0
}

/// The quote closing a text `open` opens, if `open` is a quote.
const fn closing_quote(open: char) -> Option<char> {
    match open {
        '[' => Some(']'),
        '\'' | '"' | '`' => Some(open),
        _ => None,
    }
}

/// Reads `rest`, the text after an opening quote, up to the closing quote
/// `close`, a doubled one standing for itself, and returns the text inside
/// with the byte length consumed, the closing quote included, or `None` for
/// the length when no quote closes the text.
fn dequote_with(rest: &str, close: char) -> (String, Option<usize>) {
    let mut inside = String::new();
    let mut characters = rest.char_indices().peekable();
    while let Some((position, character)) = characters.next() {
        if character != close {
            inside.push(character);
            continue;
        }
        if characters.next_if(|&(_, next)| next == close).is_none() {
            return (inside, Some(position + close.len_utf8()));
        }
        inside.push(close);
    }
    (inside, None)
}

/// Whether `fts3PrefixParameter` reads `value` as a list of prefix lengths.
fn fts3_prefix_is_valid(value: &str) -> bool {
    if value.is_empty() {
        return true;
    }
    let bytes = value.as_bytes();
    let entries = value.split(',').count();
    let mut position = 0;
    for _ in 0..entries {
        let digits = bytes
            .get(position..)
            .map_or(0, |rest| rest.iter().take_while(|byte| byte.is_ascii_digit()).count());
        if digits == 0 {
            return false;
        }
        position += digits + 1;
    }
    true
}

/// How an `fts5` table keeps its content.
#[derive(PartialEq, Eq)]
enum Fts5Content {
    /// In its own `_content` table.
    Normal,
    /// Nowhere, from `content=''`.
    Contentless,
    /// In the table `content=` names.
    External,
}

/// The options `fts5ConfigParseSpecial` takes, in the order it tries them,
/// each matched by any prefix of its name.
const FTS5_OPTIONS: [&str; 10] = [
    "prefix",
    "tokenize",
    "content",
    "contentless_delete",
    "contentless_unindexed",
    "content_rowid",
    "columnsize",
    "locale",
    "detail",
    "tokendata",
];

/// The most prefix indexes an `fts5` table takes.
const FTS5_MAX_PREFIX_INDEXES: usize = 31;

/// What the options of an `fts5` table set.
struct Fts5Options {
    tokenize_given: bool,
    content: Fts5Content,
    content_rowid_given: bool,
    contentless_delete: bool,
    contentless_unindexed: bool,
    columnsize: bool,
    prefixes: usize,
}

/// Reads the arguments of an `fts5` table as `sqlite3Fts5ConfigParse` does.
fn fts5(table_name: &str, arguments: &[String]) -> Result<FullTextColumns, Refusal> {
    if table_name.eq_ignore_ascii_case("rank") {
        return Err(Refusal::ReservedTableName);
    }
    let mut options = Fts5Options {
        tokenize_given: false,
        content: Fts5Content::Normal,
        content_rowid_given: false,
        contentless_delete: false,
        contentless_unindexed: false,
        columnsize: true,
        prefixes: 0,
    };
    let mut columns = Vec::new();
    for argument in arguments {
        let malformed = || Refusal::MalformedArgument { argument: argument.clone() };
        let (first, quoted, rest) = fts5_word(argument).ok_or_else(malformed)?;
        let mut rest = fts5_skip_spaces(rest);
        let is_option = rest.starts_with('=');
        if is_option {
            if quoted {
                return Err(malformed());
            }
            rest = fts5_skip_spaces(&rest[1..]);
        }
        let second = if rest.is_empty() {
            None
        } else {
            let (second, _, after) = fts5_word(rest).ok_or_else(malformed)?;
            if !after.is_empty() {
                return Err(malformed());
            }
            Some(second)
        };
        if is_option {
            fts5_option(&mut options, &first, second.as_deref().unwrap_or(""))?;
        } else {
            if first.eq_ignore_ascii_case("rank") || first.eq_ignore_ascii_case("rowid") {
                return Err(Refusal::ReservedColumnName { column_name: first });
            }
            if let Some(option) = second
                && !option.eq_ignore_ascii_case("unindexed")
            {
                return Err(Refusal::UnrecognizedColumnOption { column_name: first, option });
            }
            columns.push(first);
        }
    }
    if options.contentless_delete && options.content != Fts5Content::Contentless {
        return Err(Refusal::RequiresContentless { option: "contentless_delete".to_string() });
    }
    if options.contentless_delete && !options.columnsize {
        return Err(Refusal::ConflictingOptions {
            option: "contentless_delete".to_string(),
            conflicting: "columnsize".to_string(),
        });
    }
    if options.contentless_unindexed && options.content != Fts5Content::Contentless {
        return Err(Refusal::RequiresContentless { option: "contentless_unindexed".to_string() });
    }
    if columns.is_empty() {
        return Err(Refusal::TooFewColumns);
    }
    Ok(FullTextColumns { columns, hidden: vec![table_name.to_string(), "rank".to_string()] })
}

/// Applies option `name`, as written, with `value` to `options`, as
/// `fts5ConfigParseSpecial` does.
fn fts5_option(options: &mut Fts5Options, name: &str, value: &str) -> Result<(), Refusal> {
    let malformed =
        || Refusal::MalformedOption { option: name.to_string(), value: value.to_string() };
    let repeated = || Refusal::RepeatedOption { option: name.to_string() };
    let flag = || {
        match value {
            "0" => Ok(false),
            "1" => Ok(true),
            _ => Err(malformed()),
        }
    };
    let option = FTS5_OPTIONS
        .into_iter()
        .find(|option| {
            option.len() >= name.len()
                && option.as_bytes()[..name.len()].eq_ignore_ascii_case(name.as_bytes())
        })
        .ok_or_else(|| Refusal::UnrecognizedOption { option: name.to_string() })?;
    match option {
        "prefix" => {
            options.prefixes = fts5_prefixes(value, options.prefixes).ok_or_else(malformed)?;
        }
        "tokenize" if options.tokenize_given => return Err(repeated()),
        "tokenize" => options.tokenize_given = true,
        "content" if options.content != Fts5Content::Normal => return Err(repeated()),
        "content" if value.is_empty() => options.content = Fts5Content::Contentless,
        "content" => options.content = Fts5Content::External,
        "contentless_delete" => options.contentless_delete = flag()?,
        "contentless_unindexed" => options.contentless_unindexed = flag()?,
        "content_rowid" if options.content_rowid_given => return Err(repeated()),
        "content_rowid" => options.content_rowid_given = true,
        "columnsize" => options.columnsize = flag()?,
        "detail" => {
            let matching = ["none", "full", "columns"]
                .into_iter()
                .filter(|level| {
                    level.len() >= value.len()
                        && level.as_bytes()[..value.len()].eq_ignore_ascii_case(value.as_bytes())
                })
                .count();
            if matching != 1 {
                return Err(malformed());
            }
        }
        _ => {
            flag()?;
        }
    }
    Ok(())
}

/// The count of prefix indexes after reading `value` on top of `held`, or
/// `None` when `fts5ConfigParseSpecial` refuses the value.
fn fts5_prefixes(value: &str, mut held: usize) -> Option<usize> {
    let bytes = value.as_bytes();
    let mut position = 0;
    let mut first = true;
    loop {
        while bytes.get(position) == Some(&b' ') {
            position += 1;
        }
        if !first && bytes.get(position) == Some(&b',') {
            position += 1;
            while bytes.get(position) == Some(&b' ') {
                position += 1;
            }
        } else if position == bytes.len() {
            return Some(held);
        }
        if !bytes.get(position).is_some_and(u8::is_ascii_digit) || held == FTS5_MAX_PREFIX_INDEXES {
            return None;
        }
        let mut length = 0_u32;
        while let Some(&digit) = bytes.get(position).filter(|digit| digit.is_ascii_digit()) {
            if length >= 1000 {
                break;
            }
            length = length * 10 + u32::from(digit - b'0');
            position += 1;
        }
        if length == 0 || length >= 1000 {
            return None;
        }
        held += 1;
        first = false;
    }
}

/// Skips the spaces `fts5ConfigSkipWhitespace` skips, which are only spaces.
fn fts5_skip_spaces(text: &str) -> &str {
    text.trim_start_matches(' ')
}

/// Whether `c` belongs to a bareword as `sqlite3Fts5IsBareword` reads it.
const fn is_fts5_bareword(c: u8) -> bool {
    c >= 0x80 || c.is_ascii_alphanumeric() || c == b'_' || c == 0x1A
}

/// Reads the bareword or quoted word `text` starts with as
/// `fts5ConfigGobbleWord` does, returning it dequoted, whether it was quoted,
/// and the text after it, or `None` when `text` starts with neither.
fn fts5_word(text: &str) -> Option<(String, bool, &str)> {
    if let Some(close) = text.chars().next().and_then(closing_quote) {
        let (word, consumed) = dequote_with(&text[1..], close);
        return Some((word, true, &text[1 + consumed.unwrap_or(text.len() - 1)..]));
    }
    let length = text.bytes().take_while(|&byte| is_fts5_bareword(byte)).count();
    (length > 0).then(|| (text[..length].to_string(), false, &text[length..]))
}

/// The columns of an `rtree` table as `rtreeInit` declares them, the
/// integer id first, then the coordinates of type `coordinate`, then the
/// untyped auxiliary columns.
fn rtree_columns(arguments: &[String], coordinate: &DataType) -> Result<Vec<ColumnDef>, Refusal> {
    if arguments.len() < 3 {
        return Err(Refusal::TooFewColumns);
    }
    if arguments.len() > 100 {
        return Err(Refusal::TooManyColumns);
    }
    let name = |argument: &str, written: &str| {
        sql_name(argument)
            .ok_or_else(|| Refusal::MalformedArgument { argument: written.to_string() })
    };
    let mut columns = vec![column(name(&arguments[0], &arguments[0])?, DataType::Int(None))];
    let mut auxiliary = false;
    for argument in &arguments[1..] {
        if let Some(auxiliary_name) = argument.strip_prefix('+') {
            auxiliary = true;
            columns.push(column(name(auxiliary_name, argument)?, DataType::Unspecified));
        } else if auxiliary {
            return Err(Refusal::AuxiliaryColumnNotLast);
        } else {
            columns.push(column(name(argument, argument)?, coordinate.clone()));
        }
    }
    Ok(columns)
}

/// Checks the coordinate count of an `rtree` table as `rtreeInit` does after
/// declaring its columns.
fn rtree_dimensions(arguments: &[String]) -> Result<(), Refusal> {
    let coordinates = arguments[1..].iter().filter(|argument| !argument.starts_with('+')).count();
    if coordinates < 2 {
        Err(Refusal::TooFewColumns)
    } else if coordinates > 10 {
        Err(Refusal::TooManyColumns)
    } else if coordinates % 2 == 1 {
        Err(Refusal::OddCoordinateCount)
    } else {
        Ok(())
    }
}

/// The name the first SQL token of `text` gives a column `rtreeInit`
/// declares, dequoted, or `None` when that token is no name.
fn sql_name(text: &str) -> Option<String> {
    let first = text.chars().next()?;
    if let Some(close) = closing_quote(first) {
        let (name, consumed) = dequote_with(&text[1..], close);
        return consumed.map(|_| name);
    }
    let is_start = |c: char| c.is_ascii_alphabetic() || c == '_' || !c.is_ascii();
    if !is_start(first) {
        return None;
    }
    let length = text
        .char_indices()
        .find(|&(_, c)| !(c.is_ascii_alphanumeric() || c == '_' || c == '$' || !c.is_ascii()))
        .map_or(text.len(), |(position, _)| position);
    Some(text[..length].to_string())
}

/// The stored tables SQLite reads as shadow tables of `owner`, each with the
/// suffix its module spells, none when `owner` is no virtual table.
pub(super) fn shadows_of(
    builder: &ParserDBBuilder,
    owner: &CreateTable,
) -> Vec<(StoredTable, &'static str)> {
    let Some(source) =
        builder.stored_metadata(owner).and_then(|metadata| metadata.virtual_source())
    else {
        return Vec::new();
    };
    let case = builder.identifier_case();
    builder
        .tables()
        .iter()
        .filter(|(_, metadata, _)| metadata.virtual_source().is_none())
        .filter_map(|(table, _, _)| {
            let (owner_name, suffix) = table.table_name().rsplit_once('_')?;
            let suffix = shadow_suffix(source.module_name(), suffix)?;
            let belongs = case.identifiers_match(
                owner_name,
                table.table_name_is_quoted(),
                owner.table_name(),
                owner.table_name_is_quoted(),
            ) && schema_qualifiers_match(
                table_schema_qualifier(table),
                table_schema_qualifier(owner),
                case,
            );
            belongs.then(|| (StoredTable::of(table), suffix))
        })
        .collect()
}

/// The names of the hidden columns of a table rebuilt from `previous` into
/// `replacement`, each following its column by position.
pub(super) fn follow_hidden_columns(
    hidden: &[String],
    previous: &CreateTable,
    replacement: &CreateTable,
) -> Vec<String> {
    hidden
        .iter()
        .map(|name| {
            previous
                .columns
                .iter()
                .position(|column| column.name.value == *name)
                .and_then(|position| replacement.columns.get(position))
                .map_or_else(|| name.clone(), |column| column.name.value.clone())
        })
        .collect()
}

/// Renames the table `old_name` reaches to `new_name`, and for a virtual
/// table also its shadow tables and the hidden column carrying its name, as
/// the module's `xRename` does.
///
/// # Errors
///
/// Returns [`Error::VirtualTableRenameBlocked`] when something reads the
/// hidden column carrying the table name, and the error renaming the table or
/// one of its shadow tables raises.
pub(super) fn rename_table(
    mut builder: ParserDBBuilder,
    old_name: &ObjectName,
    new_name: ObjectName,
    if_exists: bool,
    arrays: &mut Arc<TypeArrays>,
    catalog: &mut Arc<PostgresCatalog>,
) -> Result<ParserDBBuilder, Error> {
    let Some(table) = builder.resolve_table_object_name(old_name)? else {
        return ParserDB::rename_table_checked(
            builder, old_name, new_name, if_exists, arrays, catalog,
        );
    };
    let followers = rename_followers(&builder, table);
    let (new_schema, new_table) = object_name_identifiers(&new_name)?;
    let mut renamed = followers.table.clone();
    renamed.name.clone_from(&new_table.value);
    renamed.name_quoted = new_table.quote_style.is_some();
    if let Some(new_schema) = new_schema {
        renamed.schema = Some(new_schema.value.clone());
        renamed.schema_quoted = new_schema.quote_style.is_some();
    }
    builder =
        ParserDB::rename_table_checked(builder, old_name, new_name, if_exists, arrays, catalog)?;
    if let Some(named_column) = followers.named_column {
        // SQLite refuses a taken name first and checks the views after
        // rewriting them, so the readers are looked up on the renamed table.
        if let Some(reader) =
            ParserDB::column_readers(&builder, &[&renamed], &named_column)?.first()
        {
            return Err(Error::VirtualTableRenameBlocked {
                table_name: followers.table.name,
                column_name: named_column.name,
                dependent_kind: reader.kind(),
                dependent_name: reader.name().to_string(),
            });
        }
        let renamed_to = renamed.name.clone();
        builder = ParserDB::replace_table_node(builder, &renamed, |_, node| {
            if let Some(column) =
                node.columns.iter_mut().find(|column| column.name.value == named_column.name)
            {
                column.name = column_ident(renamed_to);
            }
            Ok(())
        })?;
    }
    for (shadow, suffix) in followers.shadows {
        let mut parts: Vec<ObjectNamePart> = shadow
            .schema
            .iter()
            .map(|schema| {
                let mut ident = Ident::new(schema.clone());
                ident.quote_style = shadow.schema_quoted.then_some('"');
                ObjectNamePart::Identifier(ident)
            })
            .collect();
        let mut old_ident = Ident::new(shadow.name.clone());
        old_ident.quote_style = shadow.name_quoted.then_some('"');
        let mut new_ident = Ident::new(format!("{}_{suffix}", renamed.name));
        new_ident.quote_style = old_ident.quote_style;
        let new_shadow_name = ObjectName(vec![ObjectNamePart::Identifier(new_ident)]);
        parts.push(ObjectNamePart::Identifier(old_ident));
        builder = ParserDB::rename_table_checked(
            builder,
            &ObjectName(parts),
            new_shadow_name,
            false,
            arrays,
            catalog,
        )?;
    }
    Ok(builder)
}

/// What renaming a table moves along with it, its shadow tables and the
/// hidden column carrying its name, both empty for a table no module keeps.
struct RenameFollowers {
    table: StoredTable,
    shadows: Vec<(StoredTable, &'static str)>,
    named_column: Option<NamedColumn>,
}

/// The followers of `table` for a rename.
fn rename_followers(builder: &ParserDBBuilder, table: &CreateTable) -> RenameFollowers {
    let hidden =
        builder.stored_metadata(table).map_or(&[][..], |metadata| metadata.hidden_column_names());
    let named_column = table
        .columns
        .iter()
        .find(|column| {
            column.name.value == table.table_name() && hidden.contains(&column.name.value)
        })
        .map(|column| NamedColumn::of(&column.name, builder.identifier_case()));
    RenameFollowers {
        table: StoredTable::of(table),
        shadows: shadows_of(builder, table),
        named_column,
    }
}
