<script lang="ts">
  import { post } from "./protocol";
  import { inline } from "./text";

  let { text }: { text: string } = $props();
  const segs = $derived(inline(text));
</script>

{#each segs as seg, i (i)}{#if seg.k === "t"}{seg.s}{:else if seg.k === "c"}<code>{seg.s}</code>{:else}<a
      href={seg.url}
      onclick={(e) => {
        e.preventDefault();
        post({ t: "link", url: seg.url });
      }}>{seg.s}</a
    >{/if}{/each}
