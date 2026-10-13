use std::fmt::Display;

/// A shape with a name.
pub trait Shape {
    fn name(&self) -> String;
}

pub struct Circle {
    pub radius: f64,
}

pub enum Colour {
    Red,
    Green,
}

impl Shape for Circle {
    fn name(&self) -> String {
        // The label is fixed.
        label("circle")
    }
}

pub fn label(text: &str) -> String {
    text.to_string()
}

#[test]
fn labels_are_kept() {
    assert_eq!(label("x"), "x");
}
