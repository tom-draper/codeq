<script lang="ts">
  import { onMount } from 'svelte';

  interface Named {
    name(): string;
  }

  class Base {
    sides = 0;
  }

  class Circle extends Base implements Named {
    name(): string {
      return label('circle');
    }
  }

  // The label is fixed.
  function label(text: string): string {
    return text.trim();
  }

  export let title = 'circle';
</script>

<h1>{label(title)}</h1>

{#snippet row(item)}
  <li>{item}</li>
{/snippet}
