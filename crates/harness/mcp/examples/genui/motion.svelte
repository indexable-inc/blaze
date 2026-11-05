<script>
  // Motion: how elements come and go (Svelte's in:, out:, transition: and animate:flip, or enter and exit as props), their sprung
  // pose (x, y, scale, rotate, opacity), what is over what (z), how they look under the pointer (hover), sizes (width, grow, padX),
  // and the pointer, the wheel and the keys. What the gallery shows and what an agent can copy.
  let items = $state([{ id: 1, name: 'build' }, { id: 2, name: 'test' }, { id: 3, name: 'bench' }]);
  let next = 4;
  let shown = $state(true);
  let tilt = $state(15);
  let zoom = $state(1);
  let over = $state(false);
  let drag = $state({ tx: 0, ty: 0 });
  let at = $state('nowhere');
  let key = $state('none');
  const clamp = (v) => Math.min(2, Math.max(0.5, v));
</script>
<Column gap={16}>
  <Row gap={8}>
    <Button onclick={() => (items = [{ id: next, name: `step ${next++}` }, ...items])}>Add</Button>
    <Button onclick={() => (items = items.slice(0, -1))}>Remove the last</Button>
    <Button onclick={() => (items = [...items].reverse())}>Reverse</Button>
    <Button onclick={() => (shown = !shown)}>{shown ? 'Hide' : 'Show'} the note</Button>
  </Row>
  <Wrap gap={8}>
    {#each items as item (item.id)}
      <Chip label={item.name} in:scale={{ start: 0.6, duration: 250 }} out:fade={{ duration: 150 }} animate:flip />
    {/each}
  </Wrap>
  {#if shown}
    <Card title="A note" transition:fly={{ y: 12, duration: 300 }}>
      <Text enter={{ kind: 'fly', x: -16, ms: 300, delay: 120 }}>It flies in from below, and its words a moment later.</Text>
    </Card>
  {/if}
  <Row gap={16}>
    <Card title="Pose" width={200}>
      <Slider bind:value={tilt} min={-45} max={45} label="Turn" />
      <Canvas view="0 0 100 60" width={168} height={100}>
        <Group transform="translate(50 30)" rotate={tilt}>
          <Rect x={-20} y={-12} width={40} height={24} radius={4} fill="accent" hover={{ scale: 1.1, fill: 'success' }} />
        </Group>
      </Canvas>
      <Badge tone="accent" scale={zoom} opacity={0.5 + zoom / 4} x={8}>zoom {zoom.toFixed(1)}</Badge>
    </Card>
    <Card title="Under the pointer" grow onwheel={(w) => (zoom = clamp(zoom - w.dy / 200))}>
      <Tile hover={{ scale: 1.04, y: -2 }} onhover={(on) => (over = on)} onclick={() => {}}>
        <Text>{over ? 'The pointer is on me' : 'Rest the pointer here; the wheel zooms the badge'}</Text>
      </Tile>
      <Text tone="muted" onpointermove={(p) => (at = `${p.u.toFixed(2)}, ${p.v.toFixed(2)}`)}>Pointer at {at}</Text>
    </Card>
    <Card title="Drag and keys" padX={20} padY={12}>
      <Stack align="center" width={160} height={96}>
        <Tile x={drag.tx} y={drag.ty} z={2} ondrag={(d) => (drag = { tx: d.tx, ty: d.ty })}
          onkeydown={(k) => (key = k.key)} onfocus={() => (key = 'focused')} onblur={() => (key = 'none')}>
          <Text bold>Drag me</Text>
        </Tile>
      </Stack>
      <Text tone="muted">Key {key}</Text>
    </Card>
  </Row>
</Column>
