//! Implementation of the `SchemaLike` trait for the `Schema` struct.

use crate::{
    structs::{ParserDB, Schema},
    traits::{Metadata, SchemaLike},
};

impl Metadata for Schema {
    type Meta = ();
}

impl SchemaLike for Schema {
    type DB = ParserDB;

    fn name(&self) -> &str {
        Schema::name(self)
    }

    fn name_is_quoted(&self) -> bool {
        self.is_quoted()
    }

    fn authorization(&self) -> Option<&str> {
        Schema::authorization(self)
    }

    fn public_holds_usage(&self) -> bool {
        Schema::public_holds_usage(self)
    }

    fn owner_holds_usage(&self) -> bool {
        Schema::owner_holds_usage(self)
    }
}
