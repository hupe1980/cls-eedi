//! Serde's standard JSON deserializer permits arrays as positional structs.
//! CLS.EEDI declares structs as JSON objects. Preserve that rule at *every*
//! nesting level while continuing to skip unfamiliar object members.
use serde::{
    de::{
        value::{MapDeserializer, SeqDeserializer},
        Error as _, IntoDeserializer, Visitor,
    },
    Deserializer,
};
use serde_json::{Error, Value};

pub(super) struct Json(pub Value);

impl<'de> IntoDeserializer<'de, Error> for Json {
    type Deserializer = Self;
    fn into_deserializer(self) -> Self {
        self
    }
}
impl<'de> Deserializer<'de> for Json {
    type Error = Error;
    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match self.0 {
            Value::Array(items) => {
                SeqDeserializer::new(items.into_iter().map(Json)).deserialize_any(visitor)
            }
            Value::Object(items) => {
                MapDeserializer::new(items.into_iter().map(|(k, v)| (k, Json(v))))
                    .deserialize_any(visitor)
            }
            value => value.deserialize_any(visitor),
        }
    }
    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _: &'static str,
        _: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error> {
        if !self.0.is_object() {
            return Err(Error::custom("expected an object"));
        }
        self.deserialize_any(visitor)
    }
    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        if self.0.is_null() {
            visitor.visit_none()
        } else {
            visitor.visit_some(self)
        }
    }
    fn deserialize_enum<V: Visitor<'de>>(
        self,
        name: &'static str,
        variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error> {
        // All CLS.EEDI type discriminators are strings.
        if !self.0.is_string() {
            return Err(Error::custom("expected a discriminator string"));
        }
        self.0.deserialize_enum(name, variants, visitor)
    }
    fn deserialize_ignored_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_unit()
    }
    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string bytes byte_buf unit
        unit_struct newtype_struct seq tuple tuple_struct map identifier
    }
}
