// harness-native: the panel, line and glyph passes. One slangc run, one metallib.
// Copied from den-native's shaders/den.slang (github.com/andrewgazelka/den) and stripped to what harness draws: plates (base,
// meter ring, caret, disclosure chevron), hairlines, and atlas glyphs. The structs are den's, unchanged, so pack.rs matches
// byte for byte. den's outline text (the Slug algorithm, Eric Lengyel, JCGT 6(2) 2017; MIT OR Apache-2.0) is not used here.
// Units: points in the instance buffers; Frame.view is the window size in points, Frame.scale the backing scale.
// All per-pass buffers and textures are entry parameters (never globals) so each entry's Metal binding table is its own.

struct Frame
{
    float2 view;
    float scale;
    float time;
    float dark;      // 1 in the dark appearance, 0 in the light one
    float live;      // device pixels to a point this frame (the eased zoom's; `scale` is the zoom stepped to), 0: scale
    float2 textfx;   // expressive text (textfx.rs): x how far past SDR white glowing words go (1: none), y a motion's seconds (0: nothing moves)
    float4 tune;     // x: text gamma dark, y: text gamma light, z: frosted glass saturation, w: unused
    float4 slug;     // outline glyphs: x emboldening (ems a side), y coverage gamma dark, z light, w the emboldening's limit (device pixels)
};

struct PanelInst
{
    float4 rect;     // x, y, w, h (points, y down)
    float4 fill_d;
    float4 fill_l;
    float4 edge_d;
    float4 edge_l;
    float4 shape;    // radius, squircle exponent n, border width, feather
    float4 fx;       // shadow blur, shadow alpha, glass (0 or 1), kind (0 panel, 2 compress glyph, 4 caret, 11 ink, ...; see pack.rs)
    float4 fx2;      // shadow dy, opacity, kind parameter, corner mask (bits 1 TL, 2 TR, 4 BR, 8 BL: square corner)
    float4 lens;     // liquid glass: bevel (points, 0 none), magnify, dispersion, rim light; a caret's clip rect
    float4 fx3;      // frosted plate: sheen (the inner highlight's alpha), saturation (0: the frame's); image: 0, 0, peak, headroom
    float4 clip;     // the band it shows in: top, foot (points, y down; a foot of 0 is none), 0, 0
};


struct LineInst
{
    float4 seg;      // ax, ay, bx, by (points, y down)
    float4 around;   // the previous segment's start and the next segment's end: prev.x, prev.y, next.x, next.y
    float4 col_d;
    float4 col_l;
    float4 shape;    // half width (points), 1 when a segment comes before this one, 1 when one comes after, opacity
};

struct GlyphInst
{
    float4 rect;     // x, y, w, h (points): a bitmap on the device-pixel grid, or an outline glyph's ink box
    float4 uv;       // bitmap: u0, v0, u1, v1 in the atlas; outline: its ink box in ems, top-left then bottom-right (x0, y1, x1, y0)
    float4 col_d;
    float4 col_l;
    float4 anim;     // birth time, settle omega, rise (points), opacity
    float4 kind;     // colour (1: a colour bitmap such as an emoji, drawn as it is, not tinted), drop below its line's top, its line's height (CPU only), outline (1: drawn from its curves)
    float4 clip;     // the band it shows in: top, foot (points, y down; a foot of 0 is none); a bitmap's turn about its middle (radians), where swelling words grow from (points below rect.y)
    float4 band;     // outline: band scale x, scale y, offset x, offset y
    uint4 gdata;     // outline: band header start in `bands`, greatest vertical (x) band, greatest horizontal (y) band, 0
    float4 fx;       // expressive text: its motion's start (frame time), code (motion | 16 glow | place << 5), swell x (points right of rect.x), em (points)
};


static const float2 CORNERS[4] = { float2(0, 0), float2(1, 0), float2(0, 1), float2(1, 1) };

float2 to_ndc(float2 pt, float2 view)
{
    return float2(pt.x / view.x * 2.0 - 1.0, 1.0 - pt.y / view.y * 2.0);
}

// Continuous-corner (squircle) rounded-rect distance: a superellipse of exponent n in the corner region.
float sd_squircle(float2 p, float2 half_size, float r, float n)
{
    float2 q = abs(p) - (half_size - r);
    float2 qp = max(q, 0.0);
    float corner = pow(pow(qp.x, n) + pow(qp.y, n), 1.0 / n);
    return corner + min(max(q.x, q.y), 0.0) - r;
}

float sd_seg(float2 p, float2 a, float2 b)
{
    float2 pa = p - a;
    float2 ba = b - a;
    float h = saturate(dot(pa, ba) / dot(ba, ba));
    return length(pa - ba * h);
}

// Signed distance to the quad v0 v1 v2 v3 (Inigo Quilez's polygon SDF); degenerate edges are skipped.
float sd_quad(float2 p, float2 v0, float2 v1, float2 v2, float2 v3)
{
    float2 v[4] = { v0, v1, v2, v3 };
    float d = dot(p - v0, p - v0);
    float s = 1.0;
    for (int i = 0; i < 4; i++)
    {
        float2 a = v[i];
        float2 e = v[(i + 3) % 4] - a;
        float2 w = p - a;
        float ee = dot(e, e);
        if (ee > 1e-8)
        {
            float2 b = w - e * saturate(dot(w, e) / ee);
            d = min(d, dot(b, b));
        }
        bool c0 = p.y >= a.y;
        bool c1 = p.y < v[(i + 3) % 4].y;
        bool c2 = e.x * w.y > e.y * w.x;
        if ((c0 && c1 && c2) || (!c0 && !c1 && !c2))
        {
            s = -s;
        }
    }
    return s * sqrt(d);
}

// The caret's blink, `t` seconds after it last moved: solid for `hold` seconds, then a cycle of `period` seconds (the os
// text field's), on for half, its edges a short eased fade (`fade`, a share of the cycle) rather than a cut. 1 shown,
// 0 hidden. `hold` and `period` come from the scene's CaretLook (the caret panel's shape.z and fx.y).
float caret_blink(float t, float hold, float period)
{
    if (t < hold)
        return 1.0;
    float ph = frac((t - hold) / period);
    float fade = 0.09;
    float off = smoothstep(0.5 - fade, 0.5, ph) * (1.0 - smoothstep(1.0 - fade, 1.0, ph));
    return 1.0 - off;
}

float4 pick(float dark, float4 d, float4 l)
{
    return dark > 0.5 ? d : l;
}

// How much of a pixel at `y` (points) is inside the band `clip` (top, foot; a foot of 0 is none): a cut across, its edge
// antialiased on the pixel grid, that leaves what it keeps as it was.
float in_band(float y, float4 clip, float scale)
{
    float a = saturate((y - clip.x) * scale + 0.5);
    return clip.y > 0.0 ? a * saturate((clip.y - y) * scale + 0.5) : a;
}

// ---- panels --------------------------------------------------------------------------------------------------

struct PanelV
{
    float4 pos : SV_Position;
    float2 local : TEXCOORD0;
    nointerpolation uint inst : TEXCOORD1;
};

[shader("vertex")]
PanelV panel_vs(uint vid : SV_VertexID, uint iid : SV_InstanceID, ConstantBuffer<Frame> frame, StructuredBuffer<PanelInst> insts)
{
    PanelInst p = insts[iid];
    float pad = p.fx.x * 2.0 + p.shape.w + abs(p.fx2.x) + 3.0;
    float2 half_size = p.rect.zw * 0.5;
    float2 center = p.rect.xy + half_size;
    float2 c = CORNERS[vid];
    float2 local = (c * 2.0 - 1.0) * (half_size + pad);
    PanelV o;
    o.pos = float4(to_ndc(center + local, frame.view), 0, 1);
    o.local = local;
    o.inst = iid;
    return o;
}

[shader("fragment")]
float4 panel_fs(PanelV v, ConstantBuffer<Frame> frame, StructuredBuffer<PanelInst> insts)
{
    PanelInst p = insts[v.inst];
    float y = p.rect.y + p.rect.w * 0.5 + v.local.y;
    return panel_shade(v, frame, p) * in_band(y, p.clip, frame.scale);
}

float4 panel_shade(PanelV v, ConstantBuffer<Frame> frame, PanelInst p)
{
    float opacity = p.fx2.y;
    float4 fill = pick(frame.dark, p.fill_d, p.fill_l);
    float4 edge = pick(frame.dark, p.edge_d, p.edge_l);
    float2 half_size = p.rect.zw * 0.5;
    float aa = 1.0 / frame.scale;

    if (p.fx.w > 2.5 && p.fx.w < 3.5)
    {
        // Meter ring: a track of the edge colour, shape.z thick, just inside the square rect, and over it an arc of the
        // fill clockwise for the share fx2.z, starting fx3.x of a turn past twelve o'clock (a spinner turns it), its ends
        // cut square across the stroke, or round when fx3.y is 1. Coverage is the distance to the band and, along it, to
        // the arc's nearer end, so it is crisp at any scale.
        float R = half_size.x;
        float sw = max(p.shape.z, aa);
        float mid = R - sw * 0.5;
        float r = length(v.local);
        float band = saturate(0.5 - (abs(r - mid) - sw * 0.5) / aa);
        float share = p.fx2.z;
        float start = p.fx3.x;
        float turn = frac(atan2(v.local.x, -v.local.y) / 6.2831853 + 2.0 - start);
        float outside = turn <= share ? -min(turn, share - turn) : min(turn - share, 1.0 - turn);
        float lit = share <= 0.0 ? 0.0 : (share >= 1.0 ? 1.0 : saturate(0.5 - outside * 6.2831853 * mid / aa));
        float arc = band * lit;
        if (p.fx3.y > 0.5 && share > 0.0 && share < 1.0)
        {
            // Each round end is a disc the stroke wide on the arc's end, which also hides the hard cut under it.
            float a0 = start * 6.2831853;
            float a1 = (start + share) * 6.2831853;
            float2 e0 = mid * float2(sin(a0), -cos(a0));
            float2 e1 = mid * float2(sin(a1), -cos(a1));
            float cap = saturate(0.5 - (min(length(v.local - e0), length(v.local - e1)) - sw * 0.5) / aa);
            arc = max(turn <= share ? band : 0.0, cap);
        }
        float ta = edge.a * band;
        float fa = fill.a * arc;
        float4 acc = float4(fill.rgb * fa, fa) + float4(edge.rgb * ta, ta) * (1.0 - fa);
        return acc * opacity;
    }
    if (p.fx.w > 7.5)
    {
        // Disclosure chevron: a right angle of round-capped stroke (shape.z points) filling the square rect, pointing
        // down at fx2.z 0 and turned a quarter to point right at 1. It spans the rect's width and half its height,
        // centred on the rect, so it sits centred at every turn.
        float turn = p.fx2.z * 1.5707963;
        float2 q = v.local / half_size.x;
        q = float2(q.x * cos(turn) - q.y * sin(turn), q.x * sin(turn) + q.y * cos(turn));
        float2 apex = float2(0.0, 0.5);
        float dd = min(sd_seg(q, float2(-1.0, -0.5), apex), sd_seg(q, float2(1.0, -0.5), apex)) * half_size.x;
        float cov = saturate(0.5 - (dd - p.shape.z * 0.5) / aa);
        float a = fill.a * cov * opacity;
        return float4(fill.rgb * a, a);
    }
    if (p.fx.w > 3.5)
    {
        // Caret: a thin bar at rest in edge_l, blinking from fx2.w, when it last moved, so it is solid while typed
        // with; over a sweep from where it was (edge_d, none when its width is 0) that shrinks into it over
        // shape.y seconds (CaretLook::glide) with an ease-out cubic, as Ghostty's cursor_sweep. fx2.z is when the sweep
        // began; shape.x is the bar's corner radius, shape.z how long it stays solid after it moved, fx.y the blink's cycle.
        float2 pt = p.rect.xy + half_size + v.local;
        float4 to = p.edge_l;
        float4 from = p.edge_d;
        float2 hb = to.zw * 0.5;
        float db = sd_squircle(pt - (to.xy + to.zw * 0.5), hb, min(p.shape.x, hb.x), 2.0);
        float bar = saturate(0.5 - db / aa) * caret_blink(max(frame.time - p.fx2.w, 0.0), p.shape.z, p.fx.y);
        float trail = 0.0;
        float prog = (frame.time - p.fx2.z) / p.shape.y;
        if (from.z > 0.0 && prog < 1.0)
        {
            float q = 1.0 - saturate(prog);
            float4 tail = lerp(from, to, 1.0 - q * q * q);
            // The leading edge follows the direction of travel, so a vertical move still has area.
            bool fwd = to.x >= tail.x;
            float tx = fwd ? tail.x : tail.x + tail.z;
            float hx = fwd ? to.x + to.z : to.x;
            float dt = sd_quad(pt, float2(tx, tail.y), float2(hx, to.y), float2(hx, to.y + to.w), float2(tx, tail.y + tail.w));
            trail = saturate(0.5 - dt / aa) * 0.5 * q;
        }
        // lens is the text box (x, y, w, h): the bar and its sweep never leave it.
        float2 lo = p.lens.xy;
        float2 hi = p.lens.xy + p.lens.zw;
        float inside = p.lens.z > 0.0 ? saturate(min(min(pt.x - lo.x, hi.x - pt.x), min(pt.y - lo.y, hi.y - pt.y)) / aa + 0.5) : 1.0;
        float a = fill.a * (bar + trail * (1.0 - bar)) * opacity * inside;
        return float4(fill.rgb * a, a);
    }
    // Corners flagged in the mask are square (stacked selection plates read as one shape).
    float radius = p.shape.x;
    uint mask = (uint)p.fx2.w;
    uint quad = (v.local.x > 0.0 ? 1u : 0u) + (v.local.y > 0.0 ? 2u : 0u);
    uint bit = quad == 0u ? 1u : (quad == 1u ? 2u : (quad == 2u ? 8u : 4u));
    if ((mask & bit) != 0u)
    {
        radius = 0.0;
    }
    float d = sd_squircle(v.local, half_size, radius, p.shape.y);
    float w = max(aa, p.shape.w);
    float cov = saturate(0.5 - d / w);
    if (p.shape.w > 0.0)
    {
        cov = cov * cov * (3.0 - 2.0 * cov);
    }

    // Soft shadow: a Gaussian-like falloff of the distance to the offset shape, hidden under the panel itself.
    float4 outc = float4(0, 0, 0, 0);
    if (p.fx.y > 0.0)
    {
        float ds = sd_squircle(v.local - float2(0, p.fx2.x), half_size, radius, p.shape.y);
        float s = saturate(0.5 - ds / (2.0 * max(p.fx.x, 0.001)));
        s = s * s * (3.0 - 2.0 * s);
        float sa = s * p.fx.y * opacity;
        outc = float4(0, 0, 0, sa);
    }

    float2 uv = (p.rect.xy + half_size + v.local) / frame.view;
    // The plate, premultiplied: the fill over nothing.
    float3 base = fill.rgb * fill.a;
    float base_a = fill.a;
    // Glass amount 0..1: the fill is a tint laid over the blurred backdrop (its alpha is how much of the page's own
    // colour the plate keeps, so what is under shows through by the rest), the plain fill at 0. The backdrop is
    // premultiplied: where it is transparent (the card's margin over the desktop) the plate is too.
    float ia = base_a * cov * opacity;
    outc = float4(base * cov * opacity, ia) + outc * (1.0 - ia);

    if (p.shape.z > 0.0)
    {
        float inner = saturate(0.5 - (d + p.shape.z) / aa);
        float ring = saturate(cov - inner) * edge.a * opacity;
        outc = float4(edge.rgb * ring, ring) + outc * (1.0 - ring);
    }
    return outc;
}

// ---- lines ----------------------------------------------------------------------------------------------------
//
// A stroked polyline is one instance per segment (a dot is a segment of no length). Each instance is a capsule: the
// distance to its segment, less the half width, is the signed distance to the stroke, and one device pixel of it is the
// edge's ramp, so the line is anti-aliased at any backing scale. Two capsules that meet overlap, and blending both would
// show the join (a translucent line gets darker where its segments meet), so the join is split on its bisector: each
// segment draws only the side of it that its own end lies on. Beyond the vertex both capsules are the same disc, so
// the split is invisible and the join is round.

struct LineV
{
    float4 pos : SV_Position;
    float2 pt : TEXCOORD0;
    nointerpolation uint inst : TEXCOORD1;
};

float2 unit2(float2 v)
{
    float l = length(v);
    return l > 1e-5 ? v / l : float2(0.0, 0.0);
}

// Distance to the segment a b; a segment of no length is a point.
float sd_stub(float2 p, float2 a, float2 b)
{
    float2 pa = p - a;
    float2 ba = b - a;
    float h = saturate(dot(pa, ba) / max(dot(ba, ba), 1e-8));
    return length(pa - ba * h);
}

[shader("vertex")]
LineV line_vs(uint vid : SV_VertexID, uint iid : SV_InstanceID, ConstantBuffer<Frame> frame, StructuredBuffer<LineInst> insts)
{
    LineInst l = insts[iid];
    float2 a = l.seg.xy;
    float2 ab = l.seg.zw - a;
    float len = length(ab);
    float2 along = len > 1e-4 ? ab / len : float2(1.0, 0.0);
    float2 across = float2(-along.y, along.x);
    // The box round the capsule: its half width and two device pixels of edge past it.
    float reach = l.shape.x + 2.0 / frame.scale;
    float2 c = CORNERS[vid];
    float2 pt = a + along * lerp(-reach, len + reach, c.x) + across * (c.y * 2.0 - 1.0) * reach;
    LineV o;
    o.pos = float4(to_ndc(pt, frame.view), 0, 1);
    o.pt = pt;
    o.inst = iid;
    return o;
}

[shader("fragment")]
float4 line_fs(LineV v, ConstantBuffer<Frame> frame, StructuredBuffer<LineInst> insts)
{
    LineInst l = insts[v.inst];
    float2 a = l.seg.xy;
    float2 b = l.seg.zw;
    float2 dir = unit2(b - a);
    // A pixel on the other side of a join's bisector belongs to the neighbour. The margin keeps a pixel on the line itself
    // drawn by both (not by neither) however the two instances round it.
    if (l.shape.y > 0.5)
    {
        float2 m = unit2(unit2(a - l.around.xy) + dir);
        if (dot(v.pt - a, m) < -1e-3)
        {
            return float4(0, 0, 0, 0);
        }
    }
    if (l.shape.z > 0.5)
    {
        float2 m = unit2(dir + unit2(l.around.zw - b));
        if (dot(v.pt - b, m) > 1e-3)
        {
            return float4(0, 0, 0, 0);
        }
    }
    float aa = 1.0 / frame.scale;
    float cov = saturate(0.5 - (sd_stub(v.pt, a, b) - l.shape.x) / aa);
    float4 col = pick(frame.dark, l.col_d, l.col_l);
    float alpha = col.a * cov * l.shape.w;
    return float4(col.rgb * alpha, alpha);
}

// ---- text -----------------------------------------------------------------------------------------------------

struct GlyphV
{
    float4 pos : SV_Position;
    float2 uv : TEXCOORD0;
    float4 color : TEXCOORD1;
    float dark : TEXCOORD2;
    float colour : TEXCOORD3;
    nointerpolation float4 clip : TEXCOORD4;
    float y : TEXCOORD5;
};

[shader("vertex")]
GlyphV glyph_vs(uint vid : SV_VertexID, uint iid : SV_InstanceID, ConstantBuffer<Frame> frame, StructuredBuffer<GlyphInst> insts)
{
    GlyphInst g = insts[iid];
    float s = frame.time - g.anim.x;
    float a = 0.0;
    float rise = 0.0;
    if (s >= 0.0)
    {
        // Step response of a critically damped spring: 0 at birth, settling to 1 with no overshoot.
        float x = g.anim.y * s;
        float k = 1.0 - (1.0 + x) * exp(-x);
        a = k;
        rise = g.anim.z * (1.0 - k);
    }
    float2 c = CORNERS[vid];
    GlyphV o;
    float4 col = pick(frame.dark, g.col_d, g.col_l);
    o.color = float4(col.rgb, col.a * a * g.anim.w);
    o.dark = frame.dark;
    o.colour = g.kind.x;
    o.clip = g.clip;
    float2 pt = g.rect.xy + c * g.rect.zw + float2(0, rise);
    o.pos = float4(to_ndc(pt, frame.view), 0, 1);
    o.uv = lerp(g.uv.xy, g.uv.zw, c);
    o.y = pt.y;
    return o;
}

[shader("fragment")]
float4 glyph_fs(GlyphV v, ConstantBuffer<Frame> frame, Texture2D<float4> atlas, SamplerState smp)
{
    // The atlas is premultiplied RGBA: a colour glyph's own pixels, or a plain glyph's coverage as white.
    float4 t = atlas.SampleLevel(smp, v.uv, 0) * in_band(v.y, v.clip, frame.scale);
    if (v.colour > 0.5)
        return t * v.color.a;
    // Coverage is gamma-adjusted per appearance: light text on dark reads thin and is boosted, dark on light is thinned.
    float g = v.dark > 0.5 ? frame.tune.x : frame.tune.y;
    float a = pow(t.a, g) * v.color.a;
    return float4(v.color.rgb * a, a);
}
