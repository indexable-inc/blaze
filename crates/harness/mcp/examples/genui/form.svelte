<script>
  // Every control, each bound to $state, and a pie that follows them: what the gallery shows and what an agent can copy.
  let presses = $state(0);
  let name = $state('Ada');
  let notes = $state('');
  let notify = $state(true);
  let agree = $state(false);
  let cpu = $state(40);
  let copies = $state(2);
  let size = $state('medium');
  let shape = $state('pie');
  let region = $state('us-east');
  let colour = $state('accent');
  let last = $state('');
  const sizes = ['small', 'medium', 'large'];
  const regions = [
    { label: 'US East', value: 'us-east' },
    { label: 'US West', value: 'us-west' },
    { label: 'Europe', value: 'eu' },
  ];
  const weight = { small: 1, medium: 2, large: 4 };
  const share = $derived([
    { label: 'cpu', value: cpu },
    { label: 'copies', value: copies * 10 },
    { label: size, value: weight[size] * 10 },
    { label: 'presses', value: presses * 5 },
  ]);
</script>
<Column gap={16}>
  <Card title="Order">
    <Input bind:value={name} label="Name" placeholder="Who is it for?" onsubmit={() => presses++} />
    <TextArea bind:value={notes} placeholder="Notes (Cmd+Enter to send)" lines={3} />
    <Row gap={16}>
      <Toggle bind:value={notify} label="Notify me" />
      <Checkbox bind:checked={agree} label="I agree" />
    </Row>
    <Slider bind:value={cpu} min={0} max={100} step={5} label="CPU share" />
    <NumberField bind:value={copies} min={1} max={10} label="Copies" />
    <Choice options={sizes} bind:selected={size} />
    <Radio options={['pie', 'donut']} bind:selected={shape} row />
    <Select options={regions} bind:selected={region} placeholder="Region" />
    <ColorPicker bind:selected={colour} label="Colour" options={['accent', 'success', 'warning', 'error', '#5e5ce6', '#ff9f0a']} />
    <Row gap={8}>
      <Button tone="accent" disabled={!agree} value="press" onclick={(v) => { presses++; last = v; }}>Pressed {presses}</Button>
      <Button value="reset" onclick={(v) => { presses = 0; cpu = 40; copies = 2; size = 'medium'; last = v; }}>Reset</Button>
    </Row>
  </Card>
  <Card title={`${name || 'Nobody'}: ${size} in ${region}${notify ? ', notified' : ''}, in ${colour}${last ? `, last ${last}` : ''}`}>
    <Chart kind={shape} data={share} height={120} />
  </Card>
</Column>
