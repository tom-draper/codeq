<script setup lang="ts">
import { ref } from 'vue';

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

const title = ref('circle');
</script>

<template>
  <h1>{{ label(title) }}</h1>
</template>
