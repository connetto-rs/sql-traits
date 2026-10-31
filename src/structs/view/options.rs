//! The options a view's `WITH (...)` and an `ALTER VIEW` or `ALTER TABLE ...
//! SET (...)` or `RESET (...)` naming a view write.
//!
//! Every rule here was measured against PostgreSQL 18.6 in Docker. A plain view
//! takes exactly three options and refuses any other name. A materialized view
//! takes storage parameters, which the model does not read, and refuses those
//! three. An option named without a value is set to `true`, and a `toast.`
//! option on a plain view, which has no TOAST table, is dropped unread.
//! `RESET` refuses nothing, whatever it names.

use alloc::string::{String, ToString};

use sqlparser::ast::{
    Expr, Ident, ObjectName, ObjectNamePart, SqlOption, UnaryOperator, Value, ValueWithSpan,
};

use crate::{
    errors::{Error, ObjectKind},
    utils::identifier_resolution::identifiers_match,
};

/// An option a plain view takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ViewOptionName {
    /// `check_option`, `local` or `cascaded`.
    CheckOption,
    /// `security_barrier`, a boolean.
    SecurityBarrier,
    /// `security_invoker`, a boolean.
    SecurityInvoker,
}

impl ViewOptionName {
    /// Every option a plain view takes.
    const ALL: [Self; 3] = [Self::CheckOption, Self::SecurityBarrier, Self::SecurityInvoker];

    /// The name PostgreSQL stores the option under.
    fn as_str(self) -> &'static str {
        match self {
            Self::CheckOption => "check_option",
            Self::SecurityBarrier => "security_barrier",
            Self::SecurityInvoker => "security_invoker",
        }
    }

    /// The option `key` names, which folds like any identifier, or `None` for a
    /// name no view takes.
    fn named_by(key: &Ident) -> Option<Self> {
        Self::ALL.into_iter().find(|name| {
            identifiers_match(name.as_str(), false, &key.value, key.quote_style.is_some())
        })
    }
}

/// What one option list sets on a plain view.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct PlainViewOptions {
    /// The `security_barrier` setting the list writes, if it writes one.
    pub(crate) security_barrier: Option<bool>,
    /// The `security_invoker` setting the list writes, if it writes one.
    pub(crate) security_invoker: Option<bool>,
}

/// The name and the value an option writes, the value being [`None`] for an
/// option named alone, or [`None`] altogether for an option of another
/// dialect's shape.
fn named_value(option: &SqlOption) -> Option<(&ObjectName, Option<&Expr>)> {
    match option {
        SqlOption::KeyValue { key, value } => Some((key, Some(value))),
        SqlOption::Ident(key) => Some((key, None)),
        _ => None,
    }
}

/// Where an option key sends the option.
enum KeyScope<'key> {
    /// The relation's own option of this name.
    Own(&'key Ident),
    /// The option of this name on the relation's TOAST table.
    Toast(&'key Ident),
    /// Nowhere PostgreSQL accepts: another namespace, or more than two parts.
    Unrecognized,
}

impl<'key> KeyScope<'key> {
    /// The scope `key` writes.
    fn of(key: &'key ObjectName) -> Self {
        match key.0.as_slice() {
            [ObjectNamePart::Identifier(name)] => Self::Own(name),
            [ObjectNamePart::Identifier(namespace), ObjectNamePart::Identifier(name)]
                if identifiers_match(
                    "toast",
                    false,
                    &namespace.value,
                    namespace.quote_style.is_some(),
                ) =>
            {
                Self::Toast(name)
            }
            _ => Self::Unrecognized,
        }
    }
}

/// Reads the options written on plain view `view_name`.
///
/// `check_option` is checked and not recorded.
///
/// # Errors
///
/// Returns [`Error::UnrecognizedViewOption`] for a name a plain view does not
/// take, [`Error::InvalidViewOptionValue`] for a value PostgreSQL does not read
/// as the option's type, and [`Error::RepeatedViewOption`] for an option the
/// list names twice.
pub(crate) fn plain_view_options(
    view_name: &ObjectName,
    options: &[SqlOption],
) -> Result<PlainViewOptions, Error> {
    let mut read = PlainViewOptions::default();
    let mut check_option_written = false;
    for option in options {
        let Some((key, value)) = named_value(option) else {
            return Err(unrecognized(ObjectKind::View, view_name, option.to_string()));
        };
        // A view has no TOAST table, and PostgreSQL drops a `toast.` option
        // written on one unread, whatever its name or value.
        let key = match KeyScope::of(key) {
            KeyScope::Own(key) => key,
            KeyScope::Toast(_) => continue,
            KeyScope::Unrecognized => {
                return Err(unrecognized(ObjectKind::View, view_name, key.to_string()));
            }
        };
        let Some(name) = ViewOptionName::named_by(key) else {
            return Err(unrecognized(ObjectKind::View, view_name, key.to_string()));
        };
        let repeated = match name {
            ViewOptionName::CheckOption => core::mem::replace(&mut check_option_written, true),
            ViewOptionName::SecurityBarrier => read.security_barrier.is_some(),
            ViewOptionName::SecurityInvoker => read.security_invoker.is_some(),
        };
        if repeated {
            return Err(Error::RepeatedViewOption {
                view_name: view_name.to_string(),
                option_name: name.as_str().to_string(),
            });
        }
        let invalid = || {
            Error::InvalidViewOptionValue {
                view_name: view_name.to_string(),
                option_name: name.as_str().to_string(),
                option_value: value.map_or_else(|| "true".to_string(), ToString::to_string),
            }
        };
        match name {
            ViewOptionName::CheckOption if !value.is_some_and(is_check_option) => {
                return Err(invalid());
            }
            ViewOptionName::CheckOption => {}
            ViewOptionName::SecurityBarrier => {
                read.security_barrier =
                    Some(value.map_or(Some(true), postgres_bool).ok_or_else(invalid)?);
            }
            ViewOptionName::SecurityInvoker => {
                read.security_invoker =
                    Some(value.map_or(Some(true), postgres_bool).ok_or_else(invalid)?);
            }
        }
    }
    Ok(read)
}

/// Reads what a `RESET (...)` naming plain view options turns off.
///
/// PostgreSQL refuses nothing here. A name no view takes, a name written twice
/// and a namespaced name such as `toast.security_invoker` are all accepted,
/// and none of them resets anything. Only the names `RESET` wrote without a
/// namespace reach here.
pub(crate) fn plain_view_reset<'name>(
    names: impl IntoIterator<Item = &'name Ident>,
) -> PlainViewOptions {
    let mut reset = PlainViewOptions::default();
    for key in names {
        match ViewOptionName::named_by(key) {
            Some(ViewOptionName::SecurityBarrier) => reset.security_barrier = Some(false),
            Some(ViewOptionName::SecurityInvoker) => reset.security_invoker = Some(false),
            Some(ViewOptionName::CheckOption) | None => {}
        }
    }
    reset
}

/// Refuses a plain view option written on materialized view `view_name`.
///
/// The materialized view's TOAST table takes storage parameters too, so a
/// view option is refused under `toast.` as well.
///
/// # Errors
///
/// Returns [`Error::UnrecognizedViewOption`] for the first option a plain view
/// takes.
pub(crate) fn refuse_plain_view_options(
    view_name: &ObjectName,
    options: &[SqlOption],
) -> Result<(), Error> {
    for option in options {
        if let Some((key, _)) = named_value(option)
            && let KeyScope::Own(name) | KeyScope::Toast(name) = KeyScope::of(key)
            && ViewOptionName::named_by(name).is_some()
        {
            return Err(unrecognized(ObjectKind::MaterializedView, view_name, key.to_string()));
        }
    }
    Ok(())
}

/// The refusal of option `option_name` on a view of `object_kind`.
fn unrecognized(object_kind: ObjectKind, view_name: &ObjectName, option_name: String) -> Error {
    Error::UnrecognizedViewOption { object_kind, view_name: view_name.to_string(), option_name }
}

/// Whether `value` is one of the two `check_option` settings, in any case.
fn is_check_option(value: &Expr) -> bool {
    option_text(value).is_some_and(|text| {
        text.eq_ignore_ascii_case("local") || text.eq_ignore_ascii_case("cascaded")
    })
}

/// Reads a boolean option value as PostgreSQL does, or `None` for a value it
/// refuses.
///
/// An integer literal is read by its value, so `01`, `0x1` and `+1` all spell
/// 1, and `-0` spells 0. Every other value is read as a word.
fn postgres_bool(value: &Expr) -> Option<bool> {
    match value {
        Expr::UnaryOp { op: UnaryOperator::Plus, expr } => integer_bool(expr),
        Expr::UnaryOp { op: UnaryOperator::Minus, expr } => integer_bool(expr).filter(|one| !one),
        Expr::Value(ValueWithSpan {
            value: Value::Number(..) | Value::HexStringLiteral(_),
            ..
        }) => integer_bool(value),
        _ => option_text(value).and_then(bool_word),
    }
}

/// Whether an integer literal spells 1 or 0, or `None` for any other number.
///
/// The parser hands over an integer as written, after a `0x`, `0o` or `0b`
/// prefix in any case, or as the digits of a hexadecimal literal in a dialect
/// that reads `0x` that way. Underscores between digit groups and leading
/// zeros leave the value alone, and a float never reads as a boolean. A lone
/// `1` means 1 in every base, so the value is 1 when a lone `1` follows any
/// run of zeros, and 0 when nothing does.
fn integer_bool(value: &Expr) -> Option<bool> {
    let digits = match value {
        Expr::Value(ValueWithSpan { value: Value::Number(written, _), .. }) => {
            match written.get(..2) {
                Some(prefix)
                    if ["0x", "0o", "0b"]
                        .iter()
                        .any(|radix| prefix.eq_ignore_ascii_case(radix)) =>
                {
                    &written[2..]
                }
                _ => written.as_str(),
            }
        }
        Expr::Value(ValueWithSpan { value: Value::HexStringLiteral(digits), .. }) => digits,
        _ => return None,
    };
    let mut significant =
        digits.chars().filter(|digit| *digit != '_').skip_while(|digit| *digit == '0');
    match (significant.next(), significant.next()) {
        (None, _) => Some(false),
        (Some('1'), None) => Some(true),
        _ => None,
    }
}

/// The text PostgreSQL reads off a word or string option value.
///
/// An unquoted word folds to lowercase on the server, which changes nothing
/// here because every comparison ignores case.
fn option_text(value: &Expr) -> Option<&str> {
    match value {
        Expr::Identifier(word) => Some(&word.value),
        Expr::Value(ValueWithSpan { value, .. }) => {
            match value {
                Value::Boolean(true) => Some("true"),
                Value::Boolean(false) => Some("false"),
                Value::SingleQuotedString(text)
                | Value::EscapedStringLiteral(text)
                | Value::UnicodeStringLiteral(text) => Some(text),
                Value::DollarQuotedString(text) => Some(&text.value),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Reads `text` as PostgreSQL's `parse_bool` does.
///
/// That takes, in any case, any prefix of `true`, `false`, `yes` and `no`, at
/// least two letters of `on` and `off`, since one would not tell them apart,
/// and a lone `1` or `0`.
fn bool_word(text: &str) -> Option<bool> {
    let abbreviates = |word: &str, shortest: usize| {
        text.len() >= shortest
            && word.get(..text.len()).is_some_and(|prefix| prefix.eq_ignore_ascii_case(text))
    };
    [
        ("true", 1, true),
        ("false", 1, false),
        ("yes", 1, true),
        ("no", 1, false),
        ("on", 2, true),
        ("off", 2, false),
        ("1", 1, true),
        ("0", 1, false),
    ]
    .into_iter()
    .find_map(|(word, shortest, value)| abbreviates(word, shortest).then_some(value))
}
