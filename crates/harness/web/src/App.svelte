<script lang="ts">
  import { onMount } from "svelte";
  import { BadMessage, inApp, parseDown, post, setLocal, type Screen } from "./protocol";
  import { MockApp } from "./mock";
  import MockIndex from "./MockIndex.svelte";
  import Start from "./screens/Start.svelte";
  import Making from "./screens/Making.svelte";
  import Step from "./screens/Step.svelte";
  import Course from "./screens/Course.svelte";
  import Done from "./screens/Done.svelte";

  let screen: Screen | null = $state(null);
  let error: string | null = $state(null);
  let index = $state(false);

  function recv(msg: unknown) {
    try {
      screen = parseDown(msg).screen;
      error = null;
    } catch (e) {
      // An unknown tag or a malformed screen is shown, never ignored.
      error = e instanceof BadMessage ? e.message : String(e);
      console.error("harnessRecv", error, msg);
    }
  }

  onMount(() => {
    window.harnessRecv = recv;
    if (inApp()) {
      post({ t: "ready" });
      return;
    }
    // Outside the app: draw a fixture and play the app's part locally, or list the fixtures.
    const q = new URLSearchParams(location.search);
    const name = q.get("mock");
    if (!name) {
      // `?mock` alone lists the fixtures. Nothing at all is the app's page loaded before its binding is in (the host
      // reloads it once it is): it draws nothing.
      index = q.has("mock");
      return;
    }
    try {
      const app = new MockApp(name, (s) => recv({ t: "state", screen: s }));
      setLocal((up) => app.handle(up));
      recv({ t: "state", screen: app.screen });
    } catch (e) {
      error = String(e);
    }
  });
</script>

{#if error}
  <main class="error column" role="alert">
    <p class="kicker">This page cannot draw what the app sent</p>
    <pre class="code">{error}</pre>
  </main>
{:else if index}
  <MockIndex />
{:else if screen?.kind === "start"}
  <Start {screen} />
{:else if screen?.kind === "making"}
  <Making {screen} />
{:else if screen?.kind === "step"}
  <Step {screen} />
{:else if screen?.kind === "course"}
  <Course {screen} />
{:else if screen?.kind === "done"}
  <Done {screen} />
{/if}

<style>
  .error {
    padding-top: 30vh;
  }
  .error pre {
    margin-top: 14px;
    white-space: pre-wrap;
    color: var(--error);
  }
</style>
