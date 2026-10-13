<?php

namespace Shapes;

use Shapes\Contracts\Countable;

interface Named
{
    public function name(): string;
}

class Shape
{
    protected int $sides = 0;
}

enum Colour
{
    case Red;
    case Green;
}

// A circle is a named shape.
class Circle extends Shape implements Named
{
    public float $radius = 1.0;

    public function name(): string
    {
        return label('circle');
    }
}

function label(string $text): string
{
    return trim($text);
}
