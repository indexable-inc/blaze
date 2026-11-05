<script>
  // A video run live through a shader: the slider is read by the shader every frame ('@glow', no round trip), the shader samples
  // the video ('@clip') and its own last frame ('@fx', for trails), and ripples where the pointer is.
  let glow = $state(1.2);
  let seconds = $state(0);
</script>
<Column gap={12}>
  <Grid columns={2} gap={12}>
    <Video id="clip" src="/System/Library/Wallpapers/.default/Golden Gate.mov" aspect="16:9" fit="cover" autoplay loop bind:currentTime={seconds} />
    <Shader id="fx" aspect="16:9" textures={{ clip: '@clip', prev: '@fx' }} uniforms={{ glow: '@glow', tint: '#ff7a1a' }}>
float4 main(ShaderInput in) {
    float2 m = in.pointer / in.resolution;
    float r = length((in.uv - m) * float2(in.resolution.x / in.resolution.y, 1.0));
    float2 uv = in.uv + in.hovered * 0.012 * sin(60.0 * r - 8.0 * in.time) * exp(-5.0 * r);
    float2 o = float2(0.003 * in.u.glow, 0.0);
    float3 c = float3(texture(clip, uv - o).r, texture(clip, uv).g, texture(clip, uv + o).b);
    float l = dot(c, float3(0.299, 0.587, 0.114));
    c = lerp(c, in.u.tint.rgb * l * 1.6, 0.35) * in.u.glow;
    c *= 0.92 + 0.08 * sin(in.pixel.y * 1.4);
    return float4(lerp(c, texture(prev, in.uv).rgb, 0.55), 1.0);
}
    </Shader>
  </Grid>
  <Row gap={12}>
    <Slider id="glow" label="Glow" min={0} max={3} step={0.01} bind:value={glow} />
    <Text tone="muted">{seconds.toFixed(1)} s</Text>
  </Row>
  <Shader height={180}>
float4 main(ShaderInput in) {
    float2 p = in.pixel / in.resolution.y;
    float2 m = in.hovered > 0 ? in.pointer / in.resolution.y : float2(0.5 * in.resolution.x / in.resolution.y, 0.5);
    float d = length(p - m);
    float rings = 0.5 + 0.5 * cos(40.0 * d - 4.0 * in.time);
    float3 c = lerp(float3(0.10, 0.12, 0.20), float3(1.0, 0.48, 0.10), rings * exp(-3.0 * d));
    return float4(c + in.down * 0.2, 1.0);
}
  </Shader>
</Column>
