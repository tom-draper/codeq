use serde::ser::{self, Impossible, Serialize, SerializeSeq, SerializeTuple, SerializeTupleStruct};

pub(super) fn is_sequence<T: Serialize>(value: &T) -> bool {
    value.serialize(Probe).unwrap_or(false)
}

struct Probe;

#[derive(Debug)]
struct Unsupported;

impl std::fmt::Display for Unsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("not a sequence")
    }
}

impl std::error::Error for Unsupported {}

impl ser::Error for Unsupported {
    fn custom<T: std::fmt::Display>(_: T) -> Self {
        Self
    }
}

macro_rules! scalar {
    ($($method:ident($ty:ty)),* $(,)?) => {
        $(fn $method(self, _: $ty) -> Result<bool, Unsupported> { Ok(false) })*
    };
}

impl ser::Serializer for Probe {
    type Ok = bool;
    type Error = Unsupported;
    type SerializeSeq = Self;
    type SerializeTuple = Self;
    type SerializeTupleStruct = Self;
    type SerializeTupleVariant = Impossible<bool, Unsupported>;
    type SerializeMap = Impossible<bool, Unsupported>;
    type SerializeStruct = Impossible<bool, Unsupported>;
    type SerializeStructVariant = Impossible<bool, Unsupported>;

    scalar! {
        serialize_bool(bool), serialize_i8(i8), serialize_i16(i16), serialize_i32(i32),
        serialize_i64(i64), serialize_i128(i128), serialize_u8(u8), serialize_u16(u16),
        serialize_u32(u32), serialize_u64(u64), serialize_u128(u128), serialize_f32(f32),
        serialize_f64(f64), serialize_char(char), serialize_str(&str),
    }
    fn serialize_bytes(self, _: &[u8]) -> Result<bool, Unsupported> {
        Ok(true)
    }
    fn serialize_none(self) -> Result<bool, Unsupported> {
        Ok(false)
    }
    fn serialize_some<T: ?Sized + Serialize>(self, value: &T) -> Result<bool, Unsupported> {
        value.serialize(self)
    }
    fn serialize_unit(self) -> Result<bool, Unsupported> {
        Ok(false)
    }
    fn serialize_unit_struct(self, _: &'static str) -> Result<bool, Unsupported> {
        Ok(false)
    }
    fn serialize_unit_variant(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
    ) -> Result<bool, Unsupported> {
        Ok(false)
    }
    fn serialize_newtype_struct<T: ?Sized + Serialize>(
        self,
        _: &'static str,
        value: &T,
    ) -> Result<bool, Unsupported> {
        value.serialize(self)
    }
    fn serialize_newtype_variant<T: ?Sized + Serialize>(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: &T,
    ) -> Result<bool, Unsupported> {
        Ok(false)
    }
    fn serialize_seq(self, _: Option<usize>) -> Result<Self, Unsupported> {
        Ok(self)
    }
    fn serialize_tuple(self, _: usize) -> Result<Self, Unsupported> {
        Ok(self)
    }
    fn serialize_tuple_struct(self, _: &'static str, _: usize) -> Result<Self, Unsupported> {
        Ok(self)
    }
    fn serialize_tuple_variant(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: usize,
    ) -> Result<Self::SerializeTupleVariant, Unsupported> {
        Err(Unsupported)
    }
    fn serialize_map(self, _: Option<usize>) -> Result<Self::SerializeMap, Unsupported> {
        Err(Unsupported)
    }
    fn serialize_struct(
        self,
        _: &'static str,
        _: usize,
    ) -> Result<Self::SerializeStruct, Unsupported> {
        Err(Unsupported)
    }
    fn serialize_struct_variant(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: usize,
    ) -> Result<Self::SerializeStructVariant, Unsupported> {
        Err(Unsupported)
    }
}

impl SerializeSeq for Probe {
    type Ok = bool;
    type Error = Unsupported;
    fn serialize_element<T: ?Sized + Serialize>(&mut self, _: &T) -> Result<(), Unsupported> {
        Ok(())
    }
    fn end(self) -> Result<bool, Unsupported> {
        Ok(true)
    }
}
impl SerializeTuple for Probe {
    type Ok = bool;
    type Error = Unsupported;
    fn serialize_element<T: ?Sized + Serialize>(&mut self, _: &T) -> Result<(), Unsupported> {
        Ok(())
    }
    fn end(self) -> Result<bool, Unsupported> {
        Ok(true)
    }
}
impl SerializeTupleStruct for Probe {
    type Ok = bool;
    type Error = Unsupported;
    fn serialize_field<T: ?Sized + Serialize>(&mut self, _: &T) -> Result<(), Unsupported> {
        Ok(())
    }
    fn end(self) -> Result<bool, Unsupported> {
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_only_top_level_json_sequences() {
        #[derive(serde::Serialize)]
        struct Record {
            rows: Vec<u8>,
        }
        assert!(is_sequence(&Vec::<u8>::new()));
        assert!(is_sequence(&[1, 2]));
        assert!(is_sequence(&(1, "two")));
        assert!(is_sequence(&Some(vec![1])));
        assert!(is_sequence(&serde_json::json!([{"name": "item"}])));
        assert!(!is_sequence(&Record { rows: vec![1] }));
        assert!(!is_sequence(&serde_json::json!({"rows": [1]})));
        assert!(!is_sequence(&None::<Vec<u8>>));
        assert!(!is_sequence(&"text"));
    }

    #[test]
    fn recognizing_a_sequence_never_serializes_its_rows() {
        struct Row;
        impl Serialize for Row {
            fn serialize<S: ser::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
                panic!("shape inspection must not serialize rows")
            }
        }
        assert!(is_sequence(&vec![Row, Row]));
    }
}
