using System;
using System.Collections.Generic;

namespace Shapes
{
    public interface INamed
    {
        string Name();
    }

    public class Shape
    {
        protected int sides = 0;
    }

    public enum Colour
    {
        Red,
        Green
    }

    /// A circle is a named shape.
    public class Circle : Shape, INamed
    {
        public double Radius { get; set; }

        public string Name()
        {
            // The label is fixed.
            return Label("circle");
        }

        private static string Label(string text)
        {
            return text.Trim();
        }

        [Fact]
        public void LabelsAreKept()
        {
            Console.WriteLine(Label("x"));
        }
    }
}
