import 'dart:math';

abstract class Named {
  String name();
}

class Shape {
  int sides = 0;
}

enum Colour { red, green }

// A circle is a named shape.
class Circle extends Shape implements Named {
  final double radius = 1.0;

  @override
  String name() {
    return label('circle');
  }
}

String label(String text) {
  return text.trim();
}
