<script>
  // SVG markup as a drawing (lowercase svg, g, path, rect, circle, ellipse, line, polyline, polygon, text), and the same
  // shapes as Canvas elements; inks are tones and series slots, never colours. A press on a shape sends its value, and a shape
  // under the pointer takes its hover; a Group turns by rotate.
  const pts = [6, 40, 22, 20, 40, 27, 58, 11, 76, 16, 94, 8];
  let pressed = $state('nothing');
  let turn = $state(0);
</script>
<Column gap={8}>
<Row gap={24}>
  <Card title="SVG">
    <svg width="220" height="150" viewBox="0 0 220 150">
      <rect x="4" y="4" width="212" height="142" rx="10" fill="none" stroke="muted" stroke-width="1" />
      <g transform="translate(60 75)">
        <circle cx="0" cy="0" r="44" fill="series1" opacity="0.85" />
        <path d="M0,0 L44,0 A44,44 0 0 1 0,44 Z" fill="series2" />
        <text x="0" y="5" text-anchor="middle" font-size="13" fill="ink">60%</text>
      </g>
      <ellipse cx="160" cy="48" rx="34" ry="18" fill="series3" />
      <polygon points="130,120 160,80 190,120" fill="series4" stroke="ink" stroke-width="1.5" onclick={() => (pressed = 'the triangle')} />
      <line x1="120" y1="135" x2="205" y2="135" stroke="faint" stroke-width="2" />
      <polyline points="120,100 140,92 160,104 180,88 200,96" fill="none" stroke="accent" stroke-width="2" />
    </svg>
  </Card>
  <Card title="Canvas">
    <Canvas view={[0, 0, 100, 50]} width={220} height={110} value="the canvas" onclick={(v) => (pressed = v)}>
      <Rect x={0} y={0} width={100} height={50} radius={4} fill="faint" />
      <Polyline points={pts} stroke="series1" strokeWidth={1.5} fill="none" />
      <Group transform={[1, 0, 0, 1, 70, 30]} opacity={0.9} rotate={turn}>
        <Circle cx={0} cy={0} r={8} fill="series5" value="the dot" onclick={(v) => { pressed = v; turn += 45; }} hover={{ scale: 1.3, fill: 'accent' }} />
        <Line x1={-12} y1={12} x2={12} y2={12} stroke="muted" />
      </Group>
      <Path d="M5 35 Q 25 20 45 35 T 85 35" stroke="success" strokeWidth={1} fill="none" />
      <Text text="trend" x={4} y={10} size={6} fill="muted" value="the label" onclick={(v) => (pressed = v)} />
    </Canvas>
  </Card>
</Row>
<Text tone="muted">Pressed {pressed}</Text>
</Column>
