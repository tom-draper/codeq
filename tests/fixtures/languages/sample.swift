import Foundation

protocol Named {
    func name() -> String
}

class Shape {
    var sides: Int = 0
}

enum Colour {
    case red
    case green
}

// A circle is a named shape.
class Circle: Shape, Named {
    let radius: Double = 1.0

    func name() -> String {
        return label("circle")
    }
}

struct Point {
    var x: Int
}

func label(_ text: String) -> String {
    return text.uppercased()
}
