import { readFile } from 'fs';
const path = require('path');

export const LIMIT = 10;

class Shape {
  sides = 0;

  name() {
    return 'shape';
  }
}

// A circle is a shape.
export class Circle extends Shape {
  area() {
    return LIMIT * 3;
  }
}

export function makeCircle() {
  return new Circle().name();
}

exports.helper = function helper() {
  return path.join('a', 'b');
};
