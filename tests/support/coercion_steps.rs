//! A label naming the steps of a coercion plan.

use sql_traits::prelude::*;

/// The steps of a conversion plan, each named by what it applies, joined by
/// `,`.
pub(crate) fn steps(db: &ParserDB, plan: &[CoercionStep<'_, ParserDB>]) -> String {
    let cast = |cast: &PostgresCatalogCast| {
        format!("{}->{}", cast.source_type(db).name(), cast.target_type(db).name())
    };
    plan.iter()
        .map(|step| {
            match step {
                CoercionStep::LiteralInput { type_name } => format!("input {}", type_name.name()),
                CoercionStep::DomainToBase { base } => format!("base {}", base.name()),
                CoercionStep::Relabel(relabel) => format!("relabel {}", cast(relabel)),
                CoercionStep::Function(function) => format!("function {}", cast(function)),
                CoercionStep::InOut(io) => format!("io {}", cast(io)),
                CoercionStep::AutomaticInOut { target } => format!("text io {}", target.name()),
                CoercionStep::Array { element } => format!("array[{}]", steps(db, element)),
                CoercionStep::RecordToComposite => "record->composite".to_owned(),
                CoercionStep::CompositeToRecord => "composite->record".to_owned(),
                CoercionStep::Inherited => "inherited".to_owned(),
                CoercionStep::LengthCoercion { cast: length, typmod } => {
                    format!("length {} {typmod}", cast(length))
                }
                CoercionStep::DomainCheck { domain } => format!("check {}", domain.name()),
            }
        })
        .collect::<Vec<_>>()
        .join(",")
}
