<script>
  // Every Chart kind: what the gallery shows (HARNESS_NATIVE_GALLERY=genui) and what an agent can copy.
  const usage = [
    { label: 'system prompt', value: 4200 },
    { label: 'tools', value: 2600 },
    { label: 'chat', value: 9100 },
    { label: 'files', value: 3300 },
    { label: 'other', value: 600 },
  ];
  const days = ['Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat', 'Sun'];
</script>
<Column gap={16}>
  <Row gap={24}>
    <Card title="Tokens by part">
      <Chart kind="pie" data={usage} unit="tok" height={120} />
    </Card>
    <Card title="Context used">
      <Chart kind="donut" data={usage} unit="tok" caption="tokens" height={120} />
    </Card>
  </Row>
  <Card title="Build time (s)">
    <Chart kind="line" labels={days} series={[
      { name: 'cold', data: [92, 88, 95, 71, 64, 66, 58] },
      { name: 'warm', data: [21, 19, 24, 15, 12, 13, 11] },
    ]} unit="s" height={140} />
  </Card>
  <Row gap={24}>
    <Card title="Cells run">
      <Chart data={[12, 30, 22, 41, 18, 7, 3]} labels={days} height={110} />
    </Card>
    <Card title="Cache hits and misses">
      <Chart kind="stacked" labels={days} series={[
        { name: 'hit', data: [40, 52, 47, 61, 58, 20, 12] },
        { name: 'miss', data: [9, 7, 12, 4, 6, 3, 2] },
      ]} height={110} />
    </Card>
  </Row>
  <Row gap={24}>
    <Card title="Memory (GB)">
      <Chart kind="area" data={[3.1, 3.4, 4.8, 4.2, 5.9, 6.3, 5.1, 4.4]} unit="GB" height={100} />
    </Card>
    <Card title="Size against time">
      <Chart kind="scatter" data={[[1, 2], [2, 3.5], [3, 3.1], [4, 5.2], [5, 4.8], [6, 6.9], [7, 7.4]]} height={100} />
    </Card>
  </Row>
</Column>
