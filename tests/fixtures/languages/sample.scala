package shapes

import scala.collection.mutable

trait Named {
  def name: String
}

class Shape(val sides: Int)

// A circle is a named shape.
class Circle(val radius: Double) extends Shape(0) with Named {
  val limit: Int = 10

  def name: String = Circle.label("circle")
}

object Circle {
  def label(text: String): String = text.trim
}

enum Colour {
  case Red, Green
}
