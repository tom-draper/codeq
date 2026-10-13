import { readFile } from 'fs';

export interface Named {
  name(): string;
}

export enum Colour {
  Red,
  Green,
}

export type Id = string;

class Base {
  protected sides = 0;
}

// A circle is a named shape.
export class Circle extends Base implements Named {
  radius: number = 1;

  name(): string {
    return label('circle');
  }
}

export function label(text: string): string {
  return text.trim();
}

export const LIMIT = 10;
