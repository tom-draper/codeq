package shapes

import kotlin.math.PI

interface Named {
    fun name(): String
}

open class Shape(val sides: Int)

enum class Colour {
    RED,
    GREEN
}

// A circle is a named shape.
class Circle(val radius: Double) : Shape(0), Named {
    val limit: Int = 10

    override fun name(): String {
        return label("circle")
    }

    @Test
    fun labelsAreKept() {
        println(label("x"))
    }
}

fun label(text: String): String = text.trim()
