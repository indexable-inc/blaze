<script lang="ts">
  import { fixtures } from "./mock";

  const theme = new URLSearchParams(location.search).get("theme");
  const names = Object.keys(fixtures);
  function href(name: string): string {
    const q = new URLSearchParams({ mock: name });
    if (theme) q.set("theme", theme);
    return `?${q.toString()}`;
  }
</script>

<main class="index">
  <div class="column inner">
    <p class="kicker">Development</p>
    <h1>Fixtures</h1>
    <p class="lede">Each screen and step state, drawn with mock data. Add <code>&amp;theme=light</code> for the light look.</p>
    <ul>
      {#each names as name (name)}
        <li>
          <a href={href(name)}>
            <span class="name">{name}</span>
            <span class="about">{fixtures[name]?.about}</span>
          </a>
        </li>
      {/each}
    </ul>
  </div>
</main>

<style>
  .index {
    height: 100%;
    overflow-y: auto;
  }
  .inner {
    padding-top: 72px;
    padding-bottom: 72px;
  }
  h1 {
    margin: 8px 0 0;
    font-size: 34px;
    font-weight: 600;
    letter-spacing: -0.02em;
    color: var(--ink-strong);
  }
  .lede {
    margin: 10px 0 32px;
    color: var(--ink-muted);
  }
  .lede code {
    font-size: 0.86em;
    padding: 0.08em 0.32em;
    border-radius: 5px;
    background: var(--wash);
  }
  ul {
    list-style: none;
    margin: 0;
    padding: 0;
  }
  a {
    display: flex;
    align-items: baseline;
    gap: 16px;
    margin: 0 -12px;
    padding: 10px 12px;
    border-radius: var(--radius);
    color: var(--ink);
  }
  a:hover {
    background: var(--hover);
    text-decoration: none;
  }
  .name {
    width: 140px;
    flex: none;
    font-family: var(--mono);
    font-size: 14px;
    color: var(--ink-strong);
  }
  .about {
    color: var(--ink-muted);
  }
</style>
