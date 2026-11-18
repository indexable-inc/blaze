<script lang="ts">
  // Line glyphs on a 20 grid, stroked in the current ink.
  let { name, size = 20 }: { name: "back" | "check" | "cross" | "again" | "arrow"; size?: number } = $props();
</script>

<svg
  width={size}
  height={size}
  viewBox="0 0 20 20"
  fill="none"
  stroke="currentColor"
  stroke-width="1.75"
  stroke-linecap="round"
  stroke-linejoin="round"
  aria-hidden="true"
>
  {#if name === "back"}
    <path d="M12.5 4.5 7 10l5.5 5.5" />
  {:else if name === "check"}
    <path d="M4.5 10.5 8.25 14 15.5 6.5" />
  {:else if name === "cross"}
    <path d="M5.5 5.5l9 9M14.5 5.5l-9 9" />
  {:else if name === "again"}
    <path d="M15.5 10a5.5 5.5 0 1 1-1.6-3.9" />
    <path d="M15.5 4v3.5H12" />
  {:else}
    <path d="M4.5 10h11M11 5.5 15.5 10 11 14.5" />
  {/if}
</svg>
