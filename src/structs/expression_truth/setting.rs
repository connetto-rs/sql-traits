//! What a read of a request setting through `current_setting` evaluates to,
//! under what a deployment declares the setting holds.

use alloc::{collections::BTreeSet, string::String};

use sqlparser::ast::{
    CastKind, DataType, Expr, Function, FunctionArg, FunctionArgExpr, FunctionArguments, Value,
};
use sqlparser_expr_eval::{
    Context, Evaluation, ScalarType, Unlisted, Values, sql_values::postgres::PgValue,
};

use super::{TruthContext, any};
use crate::{
    errors::LookupError,
    structs::{Held, SessionSetting, SettingContent},
    traits::{DatabaseLike, FunctionLike},
    utils::{
        call_sites::{ExpressionForm, expression_form},
        postgres_types::PgType,
        type_modifier,
    },
};

/// What a node reading a request setting evaluates to, before a cast meets
/// the checks of the domain it names.
pub(super) enum SettingExpression<'e> {
    /// A read of a setting, `NULLIF` of it and `''`, or a read of a
    /// customized option.
    Text(Evaluation),
    /// A cast of a read of a declared setting to its declared type,
    /// `data_type`, giving `typed`.
    Cast {
        /// The type the node casts to.
        data_type: &'e DataType,
        /// What the cast gives before a domain checks it.
        typed: Evaluation,
    },
}

impl SettingExpression<'_> {
    /// What the node evaluates to as `context` reads its casts. The read
    /// stands for the whole cast, so it meets the checks of the type it
    /// casts to here.
    ///
    /// # Errors
    ///
    /// Returns the lookup errors `context` raises checking the cast.
    pub(super) fn checked<C: Context<Error = LookupError>>(
        self,
        context: &mut C,
    ) -> Result<Evaluation, LookupError> {
        let (data_type, typed) = match self {
            Self::Text(evaluation) => return Ok(evaluation),
            Self::Cast { data_type, typed } => (data_type, typed),
        };
        if !context.cast_checks(data_type)? {
            return Ok(typed);
        }
        let raises = typed.may_raise();
        let checked = context.checked_cast(data_type, typed)?;
        Ok(if raises { checked.with_error(true) } else { checked })
    }
}

impl<DB: DatabaseLike> TruthContext<'_, DB> {
    /// What `expression` evaluates to when it reads a request setting:
    /// `current_setting(...)` of a setting `declared` answers for, `NULLIF`
    /// of it and `''`, either cast to the setting's declared type, and a read
    /// of another customized option with `missing_ok`. `None` for any other
    /// node.
    ///
    /// `declared` gives the setting named by its argument, compared without
    /// case, with whether the request read sets it.
    ///
    /// # Errors
    ///
    /// Returns the lookup errors resolving a call `expression` writes.
    pub(super) fn setting_expression<'d, 'e>(
        &mut self,
        declared: impl Fn(&str) -> Option<(&'d SessionSetting, Held)>,
        expression: &'e Expr,
    ) -> Result<Option<SettingExpression<'e>>, LookupError> {
        if let Expr::Cast {
            kind: CastKind::Cast | CastKind::DoubleColon,
            expr,
            data_type,
            format: None,
            ..
        } = expression
        {
            let ty = self.data_type(data_type);
            return Ok(self
                .setting_operand(&declared, expr)?
                .and_then(|(read, nullified)| read.typed(data_type, ty, nullified))
                .map(|typed| SettingExpression::Cast { data_type, typed }));
        }
        Ok(match self.setting_operand(&declared, expression)? {
            Some((read, nullified)) => Some(SettingExpression::Text(read.text(nullified))),
            None => self.custom_read(expression)?.map(SettingExpression::Text),
        })
    }

    /// The declared setting `call` reads, when it is `current_setting(name[,
    /// missing_ok])` with literal arguments reaching PostgreSQL's own function
    /// alone, with what `declared` tells of it.
    fn setting_read<'d>(
        &mut self,
        declared: &impl Fn(&str) -> Option<(&'d SessionSetting, Held)>,
        call: &Function,
    ) -> Result<Option<SettingRead<'d>>, LookupError> {
        let FunctionArguments::List(list) = &call.args else { return Ok(None) };
        let (name, missing_ok) = match list.args.as_slice() {
            [name] => (literal(name), Some(&Value::Boolean(false))),
            [name, missing_ok] => (literal(name), literal(missing_ok)),
            _ => return Ok(None),
        };
        let (Some(Value::SingleQuotedString(name)), Some(Value::Boolean(missing_ok))) =
            (name, missing_ok)
        else {
            return Ok(None);
        };
        let Some((setting, held)) = declared(name) else { return Ok(None) };
        if !self.reaches_current_setting(call)? {
            return Ok(None);
        }
        let text = self.data_type(&DataType::Text);
        Ok(Some(SettingRead { setting, held, missing_ok: *missing_ok, text }))
    }

    /// Whether `call` runs PostgreSQL's own `current_setting` alone.
    fn reaches_current_setting(&mut self, call: &Function) -> Result<bool, LookupError> {
        let functions = self.runnable(call)?;
        Ok(!functions.is_empty()
            && functions.iter().all(|function| {
                function.name() == "current_setting"
                    && function.target_name().schema() == Some("pg_catalog")
            }))
    }

    /// What `current_setting(name, true)` reads where `name` is a literal
    /// customized option, one with a `.`, that no request sets as declared,
    /// which is any text or `NULL` read without raising. That holds unless
    /// a loaded extension defines the option as readable by superusers
    /// alone, which the schema does not tell.
    fn custom_read(&mut self, expression: &Expr) -> Result<Option<Evaluation>, LookupError> {
        let Expr::Function(call) = unnest(expression) else { return Ok(None) };
        let FunctionArguments::List(list) = &call.args else { return Ok(None) };
        let [name, missing_ok] = list.args.as_slice() else { return Ok(None) };
        let (Some(Value::SingleQuotedString(name)), Some(Value::Boolean(true))) =
            (literal(name), literal(missing_ok))
        else {
            return Ok(None);
        };
        if !name.contains('.') || !self.reaches_current_setting(call)? {
            return Ok(None);
        }
        Ok(Some(any(self.data_type(&DataType::Text))))
    }

    /// The declared setting `expression` reads, `current_setting(...)` or
    /// `NULLIF(current_setting(...), '')`, with whether `NULLIF` turns `''`
    /// into `NULL`.
    fn setting_operand<'d>(
        &mut self,
        declared: &impl Fn(&str) -> Option<(&'d SessionSetting, Held)>,
        expression: &Expr,
    ) -> Result<Option<(SettingRead<'d>, bool)>, LookupError> {
        let Expr::Function(call) = unnest(expression) else { return Ok(None) };
        if let Some(read) = self.setting_read(declared, call)? {
            return Ok(Some((read, false)));
        }
        let (Some(ExpressionForm::NullIf), FunctionArguments::List(list)) =
            (expression_form(&call.name), &call.args)
        else {
            return Ok(None);
        };
        let [
            FunctionArg::Unnamed(FunctionArgExpr::Expr(read)),
            FunctionArg::Unnamed(FunctionArgExpr::Expr(Expr::Value(empty))),
        ] = list.args.as_slice()
        else {
            return Ok(None);
        };
        let (Value::SingleQuotedString(empty), Expr::Function(read)) = (&empty.value, unnest(read))
        else {
            return Ok(None);
        };
        if !empty.is_empty() {
            return Ok(None);
        }
        Ok(self.setting_read(declared, read)?.map(|read| (read, true)))
    }
}

/// The value `argument` passes when it is a literal written in place.
fn literal(argument: &FunctionArg) -> Option<&Value> {
    match argument {
        FunctionArg::Unnamed(FunctionArgExpr::Expr(Expr::Value(value))) => Some(&value.value),
        _ => None,
    }
}

/// `expression` without the parentheses around it.
fn unnest(mut expression: &Expr) -> &Expr {
    while let Expr::Nested(inner) = expression {
        expression = inner;
    }
    expression
}

/// A read of a declared setting, under one request's presence.
struct SettingRead<'d> {
    setting: &'d SessionSetting,
    held: Held,
    /// Whether the read passes `missing_ok` as true, reading `NULL` rather
    /// than raising on a connection that never set the setting.
    missing_ok: bool,
    /// The type `current_setting` returns.
    text: ScalarType,
}

impl SettingRead<'_> {
    /// What `current_setting(...)` reads, or `NULLIF` of it and `''` when
    /// `nullified`. A request leaving the setting unset reads `NULL` on a
    /// connection that never set it, or raises without `missing_ok`, and `''`
    /// once a transaction that set it has ended. A setting set together with
    /// others reads text other than `''` where the request sets it, and one
    /// every request sets may read `''`.
    fn text(&self, nullified: bool) -> Evaluation {
        let empty = PgValue::Text(String::new());
        let mut excluded = Unlisted::any();
        excluded.excluded.insert(empty.clone());
        let set = Evaluation::new(self.text.clone(), false, false, Values::Unlisted(excluded));
        let empty = if nullified {
            Evaluation::null(self.text.clone())
        } else {
            Evaluation::new(
                self.text.clone(),
                false,
                false,
                Values::Listed(BTreeSet::from([empty])),
            )
        };
        let unset =
            if nullified { empty.clone() } else { empty.clone().with_null(self.missing_ok) };
        match self.held {
            Held::Always => set.join(&empty),
            Held::Set => set,
            Held::Unset => unset.with_error(!self.missing_ok),
            Held::Either => set.join(&unset.with_error(!self.missing_ok)),
        }
    }

    /// What casting the read, or `NULLIF` of it and `''` when `nullified`,
    /// to `target` gives, when `target` is the setting's declared type with
    /// the same modifiers, whose canonical rendering a set setting's text
    /// is. `''` raises the cast.
    fn typed(&self, target: &DataType, ty: ScalarType, nullified: bool) -> Option<Evaluation> {
        let SettingContent::Value(value) = self.setting.content() else { return None };
        let declared = value.single_type()?;
        let modifier = type_modifier::declared(declared);
        if PgType::of(declared) != PgType::of(target)
            || modifier.is_none()
            || modifier != type_modifier::declared(target)
        {
            return None;
        }
        let set = Evaluation::new(ty.clone(), false, false, Values::Unlisted(Unlisted::any()));
        let unset = match (nullified, self.missing_ok) {
            (true, missing_ok) => Evaluation::null(ty).with_error(!missing_ok),
            (false, true) => Evaluation::null(ty).with_error(true),
            (false, false) => Evaluation::raising(ty),
        };
        Some(match self.held {
            // A type whose rendering may be `''` reads `NULL` through
            // `NULLIF`.
            Held::Always => set.with_null(nullified),
            Held::Set => set,
            Held::Unset => unset,
            Held::Either => set.join(&unset),
        })
    }
}
