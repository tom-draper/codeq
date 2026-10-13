package sample;

import java.util.List;

interface Named {
    String name();
}

class Base {
    protected int sides = 0;
}

enum Colour {
    RED,
    GREEN
}

/** A circle is a named shape. */
public class Sample extends Base implements Named {
    private double radius = 1.0;

    @Override
    public String name() {
        // The label is fixed.
        return label("circle");
    }

    static String label(String text) {
        return text.trim();
    }

    @Test
    void labelsAreKept() {
        List.of(label("x"));
    }
}
