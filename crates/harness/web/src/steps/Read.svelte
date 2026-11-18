<script lang="ts">
  import Inline from "../Inline.svelte";
  import { paragraphs } from "../text";

  let { title, body, code }: { title: string; body: string; code: string } = $props();
  const paras = $derived(paragraphs(body));
</script>

<h2 class="prompt">{title}</h2>
{#if paras.length > 0}
  <div class="body rich read-body">
    {#each paras as p, i (i)}
      <p><Inline text={p} /></p>
    {/each}
  </div>
{/if}
{#if code}
  <pre class="code"><code>{code}</code></pre>
{/if}

<style>
  .read-body {
    margin-top: 20px;
  }
  pre {
    margin-top: 28px;
  }
</style>
