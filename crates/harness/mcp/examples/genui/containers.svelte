<script>
  // Every tier-3 element: layout, folding and tabs, floats, status, pictures and files, each bound to $state where it binds.
  // What the gallery shows and what an agent can copy; the Image and the FileRef name files of the blaze checkout.
  const icon = '/Volumes/Projects/blaze/docs/brand/blaze-icon-dark-180.png';
  const source = '/Volumes/Projects/blaze/crates/harness/mcp/src/ui/tree.ts';
  let tab = $state('files');
  let ratio = $state(0.4);
  let filters = $state({ rust: true, docs: false, tests: false });
  let details = $state(true);
  let history = $state(false);
  let tip = $state(true);
  let menu = $state(false);
  let picked = $state('nothing yet');
  let dialog = $state(false);
  let sheet = $state(false);
  let opened = $state(0);
  let done = $state(3);
  let badge = $state(0);
  let context = $state('nothing');
  const rows = ['Rename', '-', { label: 'Duplicate', value: 'dup', keys: '⌘D' }, { label: 'Delete', value: 'rm', danger: true }];
</script>
<Column gap={16}>
  <Tabs tabs={[{ label: 'Files', value: 'files' }, { label: 'Notes', value: 'notes' }, 'Status']} bind:selected={tab}>
    {#if tab === 'files'}
      <Wrap gap={8}>
        <Chip selected={filters.rust} onclick={() => (filters.rust = !filters.rust)}>rust</Chip>
        <Chip selected={filters.docs} onclick={() => (filters.docs = !filters.docs)}>docs</Chip>
        <Chip label="tests" value="tests" selected={filters.tests} onclick={() => (filters.tests = !filters.tests)} />
      </Wrap>
    {:else if tab === 'notes'}
      <Text tone="muted">Notes go here.</Text>
    {:else}
      <Text tone="muted">All green.</Text>
    {/if}
  </Tabs>
  <Grid columns={3} gap={12}>
    <Tile value="icon" onclick={() => opened++}>
      <Stack align="center">
        <Image src={icon} width={96} height={96} fit="cover" alt="the blaze icon" />
        <Layer anchor="top-right" x={-4} y={4} z={1} value="badge" onclick={() => badge++}><Badge tone="accent">new {badge}</Badge></Layer>
      </Stack>
      <Text>Opened {opened}</Text>
    </Tile>
    <Tile>
      <Row gap={8}><Icon kind="typescript" /><Icon name="rust" /><Text bold>tree.ts</Text></Row>
      <FileRef path={source} line={60} />
      <FileRef path={source} name="the UI tree" kind="ts" onclick={(path) => (picked = path)} />
    </Tile>
    <Tile>
      <Progress value={done} max={5} label="{done} of 5 steps" />
      <Loading>Building</Loading>
      <Skeleton lines={2} />
    </Tile>
  </Grid>
  <Split bind:ratio={ratio}>
    <Scroll height={140}>
      <Column gap={4}>
        {#each Array.from({ length: 12 }, (_, i) => i + 1) as n}
          <Text>Line {n} of the log</Text>
        {/each}
      </Column>
    </Scroll>
    <Accordion>
      <Disclosure title="Details" bind:open={details}><Text>The split is at {Math.round(ratio * 100)}%.</Text></Disclosure>
      <Disclosure title="History" bind:open={history}><Text>Three runs today.</Text></Disclosure>
    </Accordion>
  </Split>
  <Row gap={8}>
    <Popover placement="bottom" bind:open={tip}>
      <Button>Info</Button>
      <Text>A popover floats by its trigger.</Text>
    </Popover>
    <Popover placement="top" align="center" trigger="hover">
      <Button>Hover</Button>
      <Text>Centred over it while the pointer is on it.</Text>
    </Popover>
    <Menu items={['Copy', 'Paste']} context onselect={(v) => (context = String(v))}>
      <Chip>Right press: {context}</Chip>
    </Menu>
    <Menu items={rows} bind:open={menu} onselect={(v) => (picked = String(v))}>
      <Button>Actions</Button>
    </Menu>
    <Button onclick={() => (dialog = true)}>Dialog</Button>
    <Button onclick={() => (sheet = true)}>Sheet</Button>
    <Button tone="accent" value={1} onclick={(step) => (done = Math.min(5, done + step))}>Step</Button>
  </Row>
  <Dialog title="Delete the build?" bind:open={dialog}>
    <Text>This removes target/ for this lane.</Text>
    <Button tone="error" onclick={() => (dialog = false)}>Delete</Button>
  </Dialog>
  <Sheet title="Settings" bind:open={sheet} dismissable={false}>
    <Text>A sheet rises from the foot of the pane.</Text>
    <Button onclick={() => (sheet = false)}>Done</Button>
  </Sheet>
  <Toast tone="success" duration={0}>Picked {picked}</Toast>
</Column>
