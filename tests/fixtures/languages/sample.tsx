import { memo } from 'react';

interface Props {
  title: string;
}

interface Named {
  name(): string;
}

class Base {
  sides = 0;
}

export class Legacy extends Base implements Named {
  name(): string {
    return 'legacy';
  }
}

// A wrapped component is still a definition.
export const Card = memo(function Card(props: Props) {
  return <div className="card">{format(props.title)}</div>;
});

export function format(title: string): string {
  return title.trim();
}
