<script>
  // Files and cells: a listing of File rows, a File that names a path (sent as its FileRef chip), FileRefs with their own name and
  // kind, a DropZone that hears files carried from Finder, ColorPickers, and CellRefs that show this lane's first two cells (t1, t2).
  // What the gallery shows and what an agent can copy; the files are the blaze checkout's.
  const root = '/Volumes/Projects/blaze/crates/harness/mcp';
  let picked = $state('nothing');
  let dropped = $state([]);
  let colour = $state('#0a84ff');
  let tone = $state('accent');
</script>
<Column gap={16}>
  <Row gap={16}>
    <Card title="Listing" grow>
      <File name="src/" modified="today" onclick={(v) => (picked = v)} />
      <File name="examples/" value="the examples" onclick={(v) => (picked = v)} />
      <File name="package.json" size={812} modified="2 hours ago" onclick={(v) => (picked = v)} />
      <File name="README" kind="markdown" size={14336} modified="yesterday" />
      <File name="bun.lock" size={48211} modified="last week" />
      <Text tone="muted">Picked {picked}</Text>
    </Card>
    <Card title="Chips">
      <File path={`${root}/src/ui/tree.ts`} line={300} />
      <FileRef path={`${root}/package.json`} name="the kernel's package" kind="json" onclick={(path) => (picked = path)} />
      <FileRef path={`${root}/examples/genui/files.svelte`} kind="text" />
    </Card>
  </Row>
  <Row gap={16}>
    <DropZone label="Drop files from Finder" ondrop={(paths) => (dropped = paths)} width={240} height={110} />
    <Column gap={4} grow>
      {#each dropped as path}
        <FileRef {path} />
      {:else}
        <Text tone="muted">Nothing dropped yet</Text>
      {/each}
    </Column>
  </Row>
  <Card title="Colour">
    <ColorPicker bind:selected={colour} label="Accent" options={['#0a84ff', '#bf5af2', '#ff375f', '#ff9f0a', '#30d158', '#8e8e93']} />
    <ColorPicker bind:selected={tone} label="Tone" />
    <Text>Picked {colour} and {tone}</Text>
  </Card>
  <CellRef task="t1" title="The first cell" />
  <CellRef task={2} />
</Column>
