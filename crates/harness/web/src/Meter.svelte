<script lang="ts">
  let { value, label, width = "100%" }: { value: number; label: string; width?: string } = $props();
  const v = $derived(Math.max(0, Math.min(1, value)));
</script>

<div
  class="meter"
  style:width
  role="progressbar"
  aria-label={label}
  aria-valuemin="0"
  aria-valuemax="100"
  aria-valuenow={Math.round(v * 100)}
>
  <span style:width="{v * 100}%"></span>
</div>
