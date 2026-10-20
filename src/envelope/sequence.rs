use serde::ser::{
    self, Impossible, Serialize, SerializeSeq, SerializeStruct, SerializeTuple,
    SerializeTupleStruct,
};

pub(super) enum Shape {
    Array,
    Object(Option<String>),
    Scalar,
}

pub(super) fn shape<T: Serialize>(value: &T) -> Option<Shape> {
    value.serialize(Probe).ok()
}

struct Probe;

#[derive(Debug)]
struct Unsupported;

impl std::fmt::Display for Unsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("unsupported result shape")
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
        $(fn $method(self, _: $ty) -> Result<Shape, Unsupported> { Ok(Shape::Scalar) })*
    };
}

impl ser::Serializer for Probe {
    type Ok = Shape;
    type Error = Unsupported;
    type SerializeSeq = Self;
    type SerializeTuple = Self;
    type SerializeTupleStruct = Self;
    type SerializeTupleVariant = Impossible<Shape, Unsupported>;
    type SerializeMap = Impossible<Shape, Unsupported>;
    type SerializeStruct = Fields;
    type SerializeStructVariant = Impossible<Shape, Unsupported>;

    scalar! {
        serialize_bool(bool), serialize_i8(i8), serialize_i16(i16), serialize_i32(i32),
        serialize_i64(i64), serialize_i128(i128), serialize_u8(u8), serialize_u16(u16),
        serialize_u32(u32), serialize_u64(u64), serialize_u128(u128), serialize_f32(f32),
        serialize_f64(f64), serialize_char(char), serialize_str(&str),
    }
    fn serialize_bytes(self, _: &[u8]) -> Result<Shape, Unsupported> {
        Ok(Shape::Array)
    }
    fn serialize_none(self) -> Result<Shape, Unsupported> {
        Ok(Shape::Scalar)
    }
    fn serialize_some<T: ?Sized + Serialize>(self, value: &T) -> Result<Shape, Unsupported> {
        value.serialize(self)
    }
    fn serialize_unit(self) -> Result<Shape, Unsupported> {
        Ok(Shape::Scalar)
    }
    fn serialize_unit_struct(self, _: &'static str) -> Result<Shape, Unsupported> {
        Ok(Shape::Scalar)
    }
    fn serialize_unit_variant(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
    ) -> Result<Shape, Unsupported> {
        Ok(Shape::Scalar)
    }
    fn serialize_newtype_struct<T: ?Sized + Serialize>(
        self,
        _: &'static str,
        value: &T,
    ) -> Result<Shape, Unsupported> {
        value.serialize(self)
    }
    fn serialize_newtype_variant<T: ?Sized + Serialize>(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: &T,
    ) -> Result<Shape, Unsupported> {
        Err(Unsupported)
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
        Ok(Fields::default())
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
    type Ok = Shape;
    type Error = Unsupported;
    fn serialize_element<T: ?Sized + Serialize>(&mut self, _: &T) -> Result<(), Unsupported> {
        Ok(())
    }
    fn end(self) -> Result<Shape, Unsupported> {
        Ok(Shape::Array)
    }
}
impl SerializeTuple for Probe {
    type Ok = Shape;
    type Error = Unsupported;
    fn serialize_element<T: ?Sized + Serialize>(&mut self, _: &T) -> Result<(), Unsupported> {
        Ok(())
    }
    fn end(self) -> Result<Shape, Unsupported> {
        Ok(Shape::Array)
    }
}
impl SerializeTupleStruct for Probe {
    type Ok = Shape;
    type Error = Unsupported;
    fn serialize_field<T: ?Sized + Serialize>(&mut self, _: &T) -> Result<(), Unsupported> {
        Ok(())
    }
    fn end(self) -> Result<Shape, Unsupported> {
        Ok(Shape::Array)
    }
}

#[derive(Default)]
struct Fields {
    array: Option<String>,
    multiple: bool,
}

impl SerializeStruct for Fields {
    type Ok = Shape;
    type Error = Unsupported;

    fn serialize_field<T: ?Sized + Serialize>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<(), Unsupported> {
        if matches!(value.serialize(Probe), Ok(Shape::Array)) {
            if self.array.is_some() {
                self.multiple = true;
            } else {
                self.array = Some(format!("data.{key}"));
            }
        }
        Ok(())
    }

    fn end(self) -> Result<Shape, Unsupported> {
        Ok(Shape::Object(if self.multiple { None } else { self.array }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_sequence<T: Serialize>(value: &T) -> bool {
        matches!(shape(value), Some(Shape::Array))
    }

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
    #[test]
    fn record_shapes_match_value_row_pointers_without_visiting_rows() {
        #[derive(serde::Serialize)]
        struct Search<T> {
            matches: T,
            truncated: bool,
        }
        #[derive(serde::Serialize)]
        struct Multiple {
            left: Vec<u8>,
            right: Vec<u8>,
        }
        #[derive(serde::Serialize)]
        struct Nested {
            nested: Search<Vec<u8>>,
            rows: Vec<u8>,
        }
        fn check(value: &impl Serialize) {
            let pointer = match shape(value).unwrap() {
                Shape::Array => Some("data".into()),
                Shape::Object(pointer) => pointer,
                Shape::Scalar => None,
            };
            assert_eq!(
                pointer,
                super::super::rows_pointer(&serde_json::to_value(value).unwrap())
            );
        }
        check(&Search {
            matches: vec![1, 2],
            truncated: false,
        });
        check(&Search {
            matches: Vec::<u8>::new(),
            truncated: true,
        });
        check(&Search {
            matches: Some(vec![1]),
            truncated: false,
        });
        check(&Search {
            matches: None::<Vec<u8>>,
            truncated: false,
        });
        check(&Multiple {
            left: vec![1],
            right: vec![],
        });
        check(&Nested {
            nested: Search {
                matches: vec![1],
                truncated: false,
            },
            rows: vec![],
        });
        check(&42);
        #[derive(serde::Serialize)]
        enum Tagged {
            Rows(Vec<u8>),
        }
        assert!(shape(&Tagged::Rows(vec![1])).is_none());
        struct Never;
        impl Serialize for Never {
            fn serialize<S: ser::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
                panic!("row was serialized while probing its container")
            }
        }
        assert!(
            matches!(shape(&Search { matches: vec![Never], truncated: false }), Some(Shape::Object(Some(pointer))) if pointer == "data.matches")
        );
    }
}
