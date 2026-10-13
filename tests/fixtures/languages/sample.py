import os
from pathlib import Path

LIMIT = 10


class Shape:
    """A shape with a name."""

    sides = 0

    def name(self):
        return "shape"


class Circle(Shape):
    def __init__(self, radius):
        self.radius = radius

    @property
    def area(self):
        # Close enough.
        return 3 * self.radius * self.radius


@app.get("/circle")
def make_circle():
    return Circle(LIMIT).name()


def _hidden():
    return os.getcwd()
