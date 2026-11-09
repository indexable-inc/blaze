// harness-native: the panel, line and glyph passes. One slangc run, one metallib.
// Copied from den-native's shaders/den.slang (github.com/andrewgazelka/den) and stripped to what harness draws: plates (base,
// meter ring, caret, disclosure chevron), hairlines, atlas glyphs and outline glyphs (the brand marks). The structs are den's,
// unchanged, so pack.rs matches byte for byte.
// Units: points in the instance buffers; Frame.view is the window size in points, Frame.scale the backing scale.
// All per-pass buffers and textures are entry parameters (never globals) so each entry's Metal binding table is its own.
// Outline glyphs (glyph_vs / glyph_fs, `slug_*`: the brand marks of icons.rs) are the Slug algorithm: Eric Lengyel,
// "GPU-Centered Font Rendering Directly from Glyph Outlines", JCGT 6(2), 2017. The patent is dedicated to the public
// domain; the reference shaders (https://github.com/EricLengyel/Slug) are MIT OR Apache-2.0, Copyright 2017 Eric
// Lengyel, and `slug_dilate`, `slug_root_code`, `slug_solve_*` and `slug_render` are ports of them, copied from
// den-native's den.slang (github.com/andrewgazelka/den).

struct Frame
{
    float2 view;
    float scale;
    float time;
    float dark;      // 1 in the dark appearance, 0 in the light one
    float live;      // device pixels to a point this frame (the eased zoom's; `scale` is the zoom stepped to), 0: scale
    float2 textfx;   // x the display's EDR headroom (linear over SDR white, 1: SDR; hdr.rs `Frame::headroom`, read by `frame_headroom`): how far past white HDR pictures, the mark's fire and glowing words go; y a motion's seconds (0: nothing moves)
    float4 tune;     // x: text gamma dark, y: text gamma light, z: frosted glass saturation, w: unused
    float4 slug;     // outline glyphs: x emboldening (ems a side), y coverage gamma dark, z light, w the emboldening's limit (device pixels)
    float4 place;    // xy: the point (view points) the target's top-left corner shows, 0 for the window; a windowed overlay's own window shows only its part of the view (view = its size). zw unused. Instances stay in the view's points; every vertex subtracts xy, so clip bands stay where they were. Not applied to sprites' pixel grid (terminal cells, never in an overlay's window)
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
    float4 clip;     // the band it shows in: top, foot (points, y down; a foot of 0 is none), then how far it fades in from each (points; 0: a hard cut)
    float4 clipx;    // the band across: left, right (points; a right of 0 is none), then how far it fades in from each
};


struct LineInst
{
    float4 seg;      // ax, ay, bx, by (points, y down)
    float4 around;   // the previous segment's start and the next segment's end: prev.x, prev.y, next.x, next.y
    float4 col_d;
    float4 col_l;
    float4 shape;    // half width (points), 1 when a segment comes before this one, 1 when one comes after, opacity
    float4 clip;     // the band it shows in, as a panel's
    float4 clipx;    // the band across, as a panel's
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
    float4 fade;     // how far it fades in from its band's top and foot (points; 0: a hard cut), 0, 0
    float4 clipx;    // the band across, as a panel's
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
// 0 hidden. `hold`, `period` and `fade` come from the scene's CaretLook (the caret panel's shape.z, fx.y and shape.w).
float caret_blink(float t, float hold, float period, float fade)
{
    if (t < hold)
        return 1.0;
    float ph = frac((t - hold) / period);
    float off = smoothstep(0.5 - fade, 0.5, ph) * (1.0 - smoothstep(1.0 - fade, 1.0, ph));
    return 1.0 - off;
}

float4 pick(float dark, float4 d, float4 l)
{
    return dark > 0.5 ? d : l;
}

// How much of a pixel at `y` (points) is inside the band `clip` (top, foot; a foot of 0 is none): a cut across, its edge
// antialiased on the pixel grid, that leaves what it keeps as it was. `fade` (top, foot; points) softens each cut into a
// ramp that long, where a scroll has more beyond that edge. The same along x (`clipx`: left, right) cuts the sides.
float in_band(float y, float4 clip, float2 fade, float scale)
{
    float a = saturate((y - clip.x) * scale + 0.5);
    if (fade.x > 0.0)
        a *= smoothstep(0.0, 1.0, saturate((y - clip.x) / fade.x));
    if (clip.y > 0.0)
    {
        a *= saturate((clip.y - y) * scale + 0.5);
        if (fade.y > 0.0)
            a *= smoothstep(0.0, 1.0, saturate((clip.y - y) / fade.y));
    }
    return a;
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
    o.pos = float4(to_ndc(center + local - frame.place.xy, frame.view), 0, 1);
    o.local = local;
    o.inst = iid;
    return o;
}

[shader("fragment")]
float4 panel_fs(PanelV v, ConstantBuffer<Frame> frame, StructuredBuffer<PanelInst> insts)
{
    PanelInst p = insts[v.inst];
    float y = p.rect.y + p.rect.w * 0.5 + v.local.y;
    float x = p.rect.x + p.rect.z * 0.5 + v.local.x;
    return panel_shade(v, frame, p) * in_band(y, p.clip, p.clip.zw, frame.scale) * in_band(x, p.clipx, p.clipx.zw, frame.scale);
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
    if (p.fx.w > 4.5 && p.fx.w < 5.5)
    {
        // A copy's light over one row of the selection (harness-native-render select.rs `Pulse`): a soft band of the fill
        // whose middle crosses the point (lens.x, lens.y) and leans back lens.w points for every point down, lens.z points
        // either side of it before it is gone (smoothstep; 0: the whole row, Reduce Motion's glow). It is given in view
        // points, so the pieces on every row are one band across the shape. fx2.z is how much of it adds to what is under
        // rather than lying over it: premultiplied, an alpha of 0 adds (light on a dark page), its own alpha lays it over (a
        // tint on a light one). Clipped to the row's corners as the selection's (fx2.w mask).
        uint lmask = (uint)p.fx2.w;
        uint lquad = (v.local.x > 0.0 ? 1u : 0u) + (v.local.y > 0.0 ? 2u : 0u);
        uint lbit = lquad == 0u ? 1u : (lquad == 1u ? 2u : (lquad == 2u ? 8u : 4u));
        float ld = sd_squircle(v.local, half_size, (lmask & lbit) != 0u ? 0.0 : p.shape.x, p.shape.y);
        float2 pt = p.rect.xy + half_size + v.local;
        float lit = 1.0;
        if (p.lens.z > 0.0)
            lit = 1.0 - smoothstep(0.0, 1.0, abs(pt.x - (p.lens.x - p.lens.w * (pt.y - p.lens.y))) / p.lens.z);
        float a = fill.a * saturate(0.5 - ld / aa) * lit * opacity;
        return float4(fill.rgb * a, a * (1.0 - saturate(p.fx2.z)));
    }
    if (p.fx.w > 3.5)
    {
        // Caret: a thin bar at rest in edge_l, blinking from fx2.w, when it last moved, so it is solid while typed
        // with; over a sweep from where it was (edge_d, none when its width is 0) that shrinks into it over
        // shape.y seconds (CaretLook::glide) with an ease-out cubic, as Ghostty's cursor_sweep. fx2.z is when the sweep
        // began; shape.x is the bar's corner radius, shape.z how long it stays solid after it moved, fx.y the blink's cycle,
        // shape.w the share of it each turn of the blink eases over.
        float2 pt = p.rect.xy + half_size + v.local;
        float4 to = p.edge_l;
        float4 from = p.edge_d;
        float2 hb = to.zw * 0.5;
        float db = sd_squircle(pt - (to.xy + to.zw * 0.5), hb, min(p.shape.x, hb.x), 2.0);
        float bar = saturate(0.5 - db / aa) * caret_blink(max(frame.time - p.fx2.w, 0.0), p.shape.z, p.fx.y, p.shape.w);
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

    float2 uv = (p.rect.xy + half_size + v.local - frame.place.xy) / frame.view;
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
    o.pos = float4(to_ndc(pt - frame.place.xy, frame.view), 0, 1);
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
    cov *= in_band(v.pt.y, l.clip, l.clip.zw, frame.scale) * in_band(v.pt.x, l.clipx, l.clipx.zw, frame.scale);
    float4 col = pick(frame.dark, l.col_d, l.col_l);
    float alpha = col.a * cov * l.shape.w;
    return float4(col.rgb * alpha, alpha);
}

// ---- filled paths ---------------------------------------------------------------------------------------------
//
// Copied from den-native's den.slang. A filled path is one instance (its bounding box) over a run of straight edges in
// their own buffer; curves are flattened on the CPU (vector.rs). Each pixel takes its winding number and its distance
// to the nearest edge: signed by the fill rule, one device pixel of that distance is the edge's ramp, so a path is as
// crisp as a panel at any scale. It is cut to its clip bands as a panel is.

struct PathInst
{
    float4 rect;     // the box round the path (points, y down)
    float4 col_d;
    float4 col_l;
    float4 shape;    // first edge, edge count, rule (0 nonzero, 1 even-odd), opacity
    float4 clip;     // the band it shows in, as a panel's
    float4 clipx;    // the band across, as a panel's
};

[shader("vertex")]
LineV path_vs(uint vid : SV_VertexID, uint iid : SV_InstanceID, ConstantBuffer<Frame> frame, StructuredBuffer<PathInst> insts)
{
    PathInst p = insts[iid];
    float reach = 2.0 / frame.scale;
    float2 pt = p.rect.xy - reach + CORNERS[vid] * (p.rect.zw + reach * 2.0);
    LineV o;
    o.pos = float4(to_ndc(pt - frame.place.xy, frame.view), 0, 1);
    o.pt = pt;
    o.inst = iid;
    return o;
}

[shader("fragment")]
float4 path_fs(LineV v, ConstantBuffer<Frame> frame, StructuredBuffer<PathInst> insts, StructuredBuffer<float4> edges)
{
    PathInst p = insts[v.inst];
    uint first = uint(p.shape.x);
    uint count = uint(p.shape.y);
    float2 q = v.pt;
    int winding = 0;
    float d = 1e9;
    for (uint i = 0; i < count; i++)
    {
        float4 e = edges[first + i];
        float2 a = e.xy;
        float2 b = e.zw;
        d = min(d, sd_stub(q, a, b));
        float side = (b.x - a.x) * (q.y - a.y) - (q.x - a.x) * (b.y - a.y);
        if (a.y <= q.y)
        {
            if (b.y > q.y && side > 0.0)
            {
                winding += 1;
            }
        }
        else if (b.y <= q.y && side < 0.0)
        {
            winding -= 1;
        }
    }
    bool inside = p.shape.z > 0.5 ? (winding & 1) != 0 : winding != 0;
    float sd = inside ? -d : d;
    float cov = saturate(0.5 - sd * frame.scale);
    cov *= in_band(q.y, p.clip, p.clip.zw, frame.scale) * in_band(q.x, p.clipx, p.clipx.zw, frame.scale);
    float4 col = pick(frame.dark, p.col_d, p.col_l);
    float alpha = col.a * cov * p.shape.w;
    return float4(col.rgb * alpha, alpha);
}

// ---- sprites --------------------------------------------------------------------------------------------------
//
// Box drawing, block elements, braille, sextants and powerline arrows (boxdraw.rs), one instance a cell, drawn from their
// geometry. The geometry is Ghostty's (src/font/sprite/draw: box.zig, block.zig, braille.zig, common.zig,
// symbols_for_legacy_computing.zig, powerline.zig; github.com/ghostty-org/ghostty, MIT), ported from its CPU canvas.
// Everything Ghostty puts on whole pixels is computed here in integers from the cell's whole-pixel size and tested against
// the pixel, so its coverage is exactly 0 or 1 and a line that leaves one cell meets the next with no seam. Arcs,
// diagonals and the powerline curves are distances (in pixels) antialiased over the distance's own screen derivative.

struct ImageInst
{
    float4 rect;     // x, y, w, h (points)
    float4 size;     // the texture's width and height (texels), opacity, 1 to filter it smoothly (a glow) rather than fetch texels
    float4 clip;     // the band it shows in, as a panel's
    float4 clipx;    // the band across, as a panel's
    float4 light;    // a light layer when x > 1: lift the picture under it to up to x times its colour (linear), by heat
    float4 hdr;      // an HDR picture when x = 1: premultiplied linear half floats, y its brightest (0: unknown); tone-mapped to the headroom
};

struct ImageV
{
    float4 pos : SV_Position;
    float2 uv : TEXCOORD0;
    float y : TEXCOORD1;
    nointerpolation uint inst : TEXCOORD2;
    float x : TEXCOORD3;
};

[shader("vertex")]
ImageV image_vs(uint vid : SV_VertexID, uint iid : SV_InstanceID, ConstantBuffer<Frame> frame, StructuredBuffer<ImageInst> insts)
{
    ImageInst m = insts[iid];
    float2 c = CORNERS[vid];
    float2 pt = m.rect.xy + c * m.rect.zw;
    ImageV o;
    o.pos = float4(to_ndc(pt - frame.place.xy, frame.view), 0, 1);
    o.uv = c;
    o.y = pt.y;
    o.x = pt.x;
    o.inst = iid;
    return o;
}

// Pixel art: the texel under the fragment, fetched (never filtered), so each one is a crisp block with no seam between
// neighbours at any size or offset.
[shader("fragment")]
float4 image_fs(ImageV v, ConstantBuffer<Frame> frame, StructuredBuffer<ImageInst> insts, Texture2D<float4> tex, SamplerState smp)
{
    ImageInst m = insts[v.inst];
    int2 at = clamp(int2(floor(v.uv * m.size.xy)), int2(0, 0), int2(m.size.xy) - 1);
    float4 t = m.size.w > 0.5 ? tex.SampleLevel(smp, v.uv, 0) : tex.Load(int3(at, 0));
    float cover = m.size.z * in_band(v.y, m.clip, m.clip.zw, frame.scale) * in_band(v.x, m.clipx, m.clipx.zw, frame.scale);
    if (m.hdr.x > 0.5)
    {
        // An HDR photo: premultiplied linear light, 1.0 SDR white. Straight, rolled off to the display, then encoded as
        // the target is; SDR colours under the knee come out as an SDR photo's would.
        float3 lin = t.a > 0.0 ? t.rgb / t.a : float3(0.0, 0.0, 0.0);
        float3 c = srgb_encode_ext(tone_map_rgb(lin, m.hdr.y, frame_headroom(frame)));
        float ah = t.a * cover;
        return float4(c * ah, ah);
    }
    if (m.light.x > 1.0)
    {
        // A light layer over the same picture: its colour c is already drawn here; add what takes it, in linear light,
        // to c times 1 + (gain - 1) * heat. Alpha 0, so over-blending adds and covers nothing.
        float k = 1.0 + (m.light.x - 1.0) * t.a;
        float3 lifted = srgb_encode_ext(srgb_decode_ext(t.rgb) * k);
        return float4((lifted - t.rgb) * cover, 0.0);
    }
    float a = t.a * cover;
    return float4(t.rgb * a, a);
}

// sRGB's curve, extended past 1 (the target is extended-range sRGB-encoded half floats).
float3 srgb_decode_ext(float3 c)
{
    float3 a = abs(c);
    float3 l = select(a <= 0.04045, a / 12.92, pow((a + 0.055) / 1.055, 2.4));
    return l * sign(c);
}

float3 srgb_encode_ext(float3 l)
{
    float3 a = abs(l);
    float3 c = select(a <= 0.0031308, a * 12.92, 1.055 * pow(a, 1.0 / 2.4) - 0.055);
    return c * sign(l);
}

// ---- HDR: the display's headroom and the tone map (ports of hdr.rs; its tests pin the curve) ------------------------

static const float HDR_WHITE_NITS = 203.0;   // hdr.rs WHITE_NITS: reference white (BT.2408)
static const float HDR_HLG_PEAK_NITS = 1000.0;
static const float HDR_KNEE = 0.75;

// The display's headroom this frame (hdr.rs `Frame::headroom`): linear over SDR white, at least 1.
float frame_headroom(Frame f)
{
    float h = f.textfx.x;
    return isfinite(h) ? max(h, 1.0) : 1.0;
}

float hdr_knee(float h)
{
    return min(HDR_KNEE * max(h, 1.0), 1.0);
}

// hdr.rs `tone_map`: identity to the knee, then rolled off so `peak` (0: unknown) lands on the headroom.
float tone_map(float x, float peak, float headroom)
{
    float h = max(headroom, 1.0);
    if (!(x > 0.0))
        return 0.0;
    bool known = isfinite(peak) && peak > 0.0;
    if (known && peak <= h)
        return min(x, h);
    float k = hdr_knee(h);
    if (x <= k)
        return x;
    float span = h - k;
    float c = known ? 1.0 / span - 1.0 / (peak - k) : 1.0 / span;
    float a = x - k;
    if (isinf(a))
        return h;
    return min(k + a / (1.0 + a * c), h);
}

// hdr.rs `tone_map_rgb`: by the brightest channel, so hue and saturation hold.
float3 tone_map_rgb(float3 rgb, float peak, float headroom)
{
    float m = max(rgb.r, max(rgb.g, rgb.b));
    if (!(m > 0.0))
        return float3(0.0, 0.0, 0.0);
    return rgb * (tone_map(m, peak, headroom) / m);
}

// hdr.rs `pq_linear`: a PQ signal as linear light over reference white.
float3 pq_linear(float3 e)
{
    const float M1 = 0.1593017578125;
    const float M2 = 78.84375;
    const float C1 = 0.8359375;
    const float C2 = 18.8515625;
    const float C3 = 18.6875;
    float3 p = pow(saturate(e), 1.0 / M2);
    float3 nits = 10000.0 * pow(max(p - C1, 0.0) / (C2 - C3 * p), 1.0 / M1);
    return nits / HDR_WHITE_NITS;
}

// hdr.rs `hlg_scene`, per channel.
float3 hlg_scene(float3 e)
{
    const float A = 0.17883277;
    const float B = 0.28466892;
    const float C = 0.55991073;
    e = saturate(e);
    return select(e <= 0.5, e * e / 3.0, (exp((e - C) / A) + B) / 12.0);
}

// hdr.rs `hlg_linear`: BT.2100's OOTF for a 1000-nit display (gamma 1.2), on BT.2020 primaries.
float3 hlg_linear(float3 e)
{
    float3 s = hlg_scene(e);
    float y = dot(s, float3(0.2627, 0.6780, 0.0593));
    return s * (HDR_HLG_PEAK_NITS * pow(max(y, 0.0), 0.2) / HDR_WHITE_NITS);
}

// hdr.rs `bt2020_to_709` (BT.2087).
float3 bt2020_to_709(float3 c)
{
    return float3(
        1.660491 * c.r - 0.5876411 * c.g - 0.0728499 * c.b,
        -0.1245505 * c.r + 1.1329 * c.g - 0.0083494 * c.b,
        -0.0181508 * c.r - 0.1005789 * c.g + 1.1187297 * c.b);
}

// ---- live pictures (media/: a Shader's or a Video's latest finished frame) ------------------------------------------

struct SurfaceInst
{
    float4 rect;     // x, y, w, h (points)
    float4 shape;    // corner radius (points), squircle exponent n, opacity, cover (1: cut to the box's shape; the host's uv says how)
    float4 clip;     // the band it shows in, as a panel's
    float4 clipx;    // the band across, as a panel's
    float4 uv;       // the part of the texture shown: u0, v0, u1, v1
    float4 hdr;      // how the texture encodes light: x 0 sRGB (as it is), 1 PQ, 2 HLG; y its brightest (0: unknown); z 1 BT.2020
};

struct SurfaceV
{
    float4 pos : SV_Position;
    float2 local : TEXCOORD0;
    float2 uv : TEXCOORD1;
    float y : TEXCOORD2;
    float x : TEXCOORD3;
    nointerpolation uint inst : TEXCOORD4;
};

[shader("vertex")]
SurfaceV surface_vs(uint vid : SV_VertexID, uint iid : SV_InstanceID, ConstantBuffer<Frame> frame, StructuredBuffer<SurfaceInst> insts)
{
    SurfaceInst m = insts[iid];
    float2 c = CORNERS[vid];
    float2 pt = m.rect.xy + c * m.rect.zw;
    SurfaceV o;
    o.pos = float4(to_ndc(pt - frame.place.xy, frame.view), 0, 1);
    o.local = (c * 2.0 - 1.0) * m.rect.zw * 0.5;
    o.uv = lerp(m.uv.xy, m.uv.zw, c);
    o.y = pt.y;
    o.x = pt.x;
    o.inst = iid;
    return o;
}

// The texture is premultiplied (a shader's picture, stored as its prelude premultiplied it) or opaque (a video frame), and
// may be brighter than white (EDR): only the corners' coverage, the clip bands and the opacity scale it. An HDR video's
// frame holds its PQ or HLG signal: decoded to linear light, brought to BT.709 primaries, rolled off to the display's
// headroom and encoded as the target is.
[shader("fragment")]
float4 surface_fs(SurfaceV v, ConstantBuffer<Frame> frame, StructuredBuffer<SurfaceInst> insts, Texture2D<float4> tex, SamplerState smp)
{
    SurfaceInst m = insts[v.inst];
    float2 half_size = m.rect.zw * 0.5;
    float r = min(m.shape.x, min(half_size.x, half_size.y));
    float d = sd_squircle(v.local, half_size, max(r, 1e-3), max(m.shape.y, 2.0));
    float cover = saturate(0.5 - d * frame.scale);
    float a = cover * m.shape.z * in_band(v.y, m.clip, m.clip.zw, frame.scale) * in_band(v.x, m.clipx, m.clipx.zw, frame.scale);
    float4 t = tex.SampleLevel(smp, v.uv, 0);
    if (m.hdr.x > 0.5)
    {
        float3 lin = m.hdr.x > 1.5 ? hlg_linear(t.rgb) : pq_linear(t.rgb);
        if (m.hdr.z > 0.5)
            lin = bt2020_to_709(lin);
        t.rgb = srgb_encode_ext(tone_map_rgb(lin, m.hdr.y, frame_headroom(frame))) * t.a;
    }
    return t * a;
}

struct SpriteInst
{
    int4 rect;       // the cell: x, y, w, h (device pixels)
    uint4 shape;     // kind, a, b (boxdraw.rs `Shape`), a light line's thickness (device pixels)
    float4 fg;       // ink, straight RGBA
    float4 bg;       // the ground under it, straight RGBA (alpha 0: none)
};

static const uint SPRITE_BLOCK = 1;
static const uint SPRITE_SHADE = 2;
static const uint SPRITE_MOSAIC = 3;
static const uint SPRITE_LINES = 4;
static const uint SPRITE_DASH = 5;
static const uint SPRITE_ARC = 6;
static const uint SPRITE_DIAGONAL = 7;
static const uint SPRITE_BRAILLE = 8;
static const uint SPRITE_POWERLINE = 9;

struct SpriteV
{
    float4 pos : SV_Position;
    nointerpolation uint inst : TEXCOORD0;
};

[shader("vertex")]
SpriteV sprite_vs(uint vid : SV_VertexID, uint iid : SV_InstanceID, ConstantBuffer<Frame> frame, StructuredBuffer<SpriteInst> insts)
{
    SpriteInst s = insts[iid];
    float2 px = float2(s.rect.xy) + CORNERS[vid] * float2(s.rect.zw);
    SpriteV o;
    o.pos = float4(to_ndc(px / frame.scale - frame.place.xy, frame.view), 0, 1);
    o.inst = iid;
    return o;
}

// Whether the pixel `p` (whole pixels from the cell's top left) is in the box between corners (x0, y0) and (x1, y1), as
// Ghostty's canvas.box fills it (its corners in either order).
bool sp_box(int2 p, int x0, int y0, int x1, int y1)
{
    return p.x >= min(x0, x1) && p.x < max(x0, x1) && p.y >= min(y0, y1) && p.y < max(y0, y1);
}

// Ghostty's Fraction.min and .max (common.zig): where the fraction `f` of `size` falls as a block's start and as its end,
// rounded so the two halves of an odd size meet (start of the second half = end of the first).
int frac_lo(float f, int size)
{
    return size - int(round((1.0 - f) * float(size)));
}

int frac_hi(float f, int size)
{
    return int(round(f * float(size)));
}

// Coverage of a signed distance `d` (pixels, negative inside), its edge a pixel wide on screen.
float sp_aa(float d)
{
    float w = length(float2(ddx(d), ddy(d)));
    return saturate(0.5 - d / max(w, 1e-4));
}

// box.zig linesChar: up to four arms from the cell's middle, each none, light, heavy or double, joined as Ghostty joins them.
bool sp_lines(int2 p, int W, int H, int t, uint arms)
{
    uint up = arms & 3u;
    uint right = (arms >> 2) & 3u;
    uint down = (arms >> 4) & 3u;
    uint left = (arms >> 6) & 3u;
    int lt = t;
    int hv = 2 * t;
    int h_light_top = max(H - lt, 0) / 2;
    int h_light_bottom = h_light_top + lt;
    int h_heavy_top = max(H - hv, 0) / 2;
    int h_heavy_bottom = h_heavy_top + hv;
    int h_double_top = max(h_light_top - lt, 0);
    int h_double_bottom = h_light_bottom + lt;
    int v_light_left = max(W - lt, 0) / 2;
    int v_light_right = v_light_left + lt;
    int v_heavy_left = max(W - hv, 0) / 2;
    int v_heavy_right = v_heavy_left + hv;
    int v_double_left = max(v_light_left - lt, 0);
    int v_double_right = v_light_right + lt;

    int up_bottom = (left == 2u || right == 2u) ? h_heavy_bottom
        : (left != right || down == up) ? ((left == 3u || right == 3u) ? h_double_bottom : h_light_bottom)
        : (left == 0u && right == 0u) ? h_light_bottom : h_light_top;
    int down_top = (left == 2u || right == 2u) ? h_heavy_top
        : (left != right || up == down) ? ((left == 3u || right == 3u) ? h_double_top : h_light_top)
        : (left == 0u && right == 0u) ? h_light_top : h_light_bottom;
    int left_right = (up == 2u || down == 2u) ? v_heavy_right
        : (up != down || left == right) ? ((up == 3u || down == 3u) ? v_double_right : v_light_right)
        : (up == 0u && down == 0u) ? v_light_right : v_light_left;
    int right_left = (up == 2u || down == 2u) ? v_heavy_left
        : (up != down || right == left) ? ((up == 3u || down == 3u) ? v_double_left : v_light_left)
        : (up == 0u && down == 0u) ? v_light_left : v_light_right;

    bool on = false;
    if (up == 1u)
        on = on || sp_box(p, v_light_left, 0, v_light_right, up_bottom);
    if (up == 2u)
        on = on || sp_box(p, v_heavy_left, 0, v_heavy_right, up_bottom);
    if (up == 3u)
    {
        int lb = left == 3u ? h_light_top : up_bottom;
        int rb = right == 3u ? h_light_top : up_bottom;
        on = on || sp_box(p, v_double_left, 0, v_light_left, lb) || sp_box(p, v_light_right, 0, v_double_right, rb);
    }
    if (right == 1u)
        on = on || sp_box(p, right_left, h_light_top, W, h_light_bottom);
    if (right == 2u)
        on = on || sp_box(p, right_left, h_heavy_top, W, h_heavy_bottom);
    if (right == 3u)
    {
        int tl = up == 3u ? v_light_right : right_left;
        int bl = down == 3u ? v_light_right : right_left;
        on = on || sp_box(p, tl, h_double_top, W, h_light_top) || sp_box(p, bl, h_light_bottom, W, h_double_bottom);
    }
    if (down == 1u)
        on = on || sp_box(p, v_light_left, down_top, v_light_right, H);
    if (down == 2u)
        on = on || sp_box(p, v_heavy_left, down_top, v_heavy_right, H);
    if (down == 3u)
    {
        int lt2 = left == 3u ? h_light_bottom : down_top;
        int rt2 = right == 3u ? h_light_bottom : down_top;
        on = on || sp_box(p, v_double_left, lt2, v_light_left, H) || sp_box(p, v_light_right, rt2, v_double_right, H);
    }
    if (left == 1u)
        on = on || sp_box(p, 0, h_light_top, left_right, h_light_bottom);
    if (left == 2u)
        on = on || sp_box(p, 0, h_heavy_top, left_right, h_heavy_bottom);
    if (left == 3u)
    {
        int tr = up == 3u ? v_light_left : left_right;
        int br = down == 3u ? v_light_left : left_right;
        on = on || sp_box(p, 0, h_double_top, tr, h_light_top) || sp_box(p, 0, h_light_bottom, br, h_double_bottom);
    }
    return on;
}

// box.zig dashHorizontal / dashVertical: `n` dashes, a horizontal row centred with half gaps at both ends, a vertical
// column from the top with a whole gap at the foot; spare pixels widen the first dashes.
bool sp_dash(int2 p, int W, int H, int t, uint n, uint b)
{
    bool vert = (b & 1u) != 0u;
    int thick = (b & 2u) != 0u ? 2 * t : t;
    uint g = b >> 2;
    int want = g == 0u ? max(4, t) : (g == 1u ? t : 2 * t);
    int len = vert ? H : W;
    int across = vert ? W : H;
    int2 q = vert ? p.yx : p;
    int count = int(n);
    if (len < 2 * count)
    {
        int y0 = max(across - t, 0) / 2;
        return sp_box(q, 0, y0, len, y0 + t);
    }
    int gap = min(want, len / (2 * count));
    int total = len - count * gap;
    int dash = total / count;
    int extra = total - dash * count;
    int y0 = max(across - thick, 0) / 2;
    int x = vert ? 0 : gap / 2;
    bool on = false;
    for (int i = 0; i < 4; i++)
    {
        if (i < count)
        {
            int x1 = x + dash + (i < extra ? 1 : 0);
            on = on || sp_box(q, x, y0, x1, y0 + thick);
            x = x1 + gap;
        }
    }
    return on;
}

// braille.zig: two columns of four square dots, Ghostty's whole-pixel layout of dot, spacing and margins.
bool sp_braille(int2 p, int W, int H, uint bits)
{
    int w = min(W / 4, H / 8);
    int xs = W / 4;
    int ys = H / 8;
    int xm = xs / 2;
    int ym = ys / 2;
    int xl = W - 2 * xm - xs - 2 * w;
    int yl = H - 2 * ym - 3 * ys - 4 * w;
    if (xl >= 2 && yl >= 4 && w == 0) { w += 1; xl -= 2; yl -= 4; }
    if (xl >= 2 && xm == 0) { xm = 1; xl -= 2; }
    if (yl >= 2 && ym == 0) { ym = 1; yl -= 2; }
    if (xl >= 1) { xs += 1; xl -= 1; }
    if (yl >= 3) { ys += 1; yl -= 3; }
    if (xl >= 2) { xm += 1; xl -= 2; }
    if (yl >= 2) { ym += 1; yl -= 2; }
    if (xl >= 2 && yl >= 4) { w += 1; xl -= 2; yl -= 4; }
    int x0 = xm;
    int x1 = xm + w + xs;
    // Dots in bit order: tl, ul, ll, tr, ur, lr, bl, br (the code point's own).
    int cols[8] = { x0, x0, x0, x1, x1, x1, x0, x1 };
    int rows[8] = { 0, 1, 2, 0, 1, 2, 3, 3 };
    bool on = false;
    for (int i = 0; i < 8; i++)
    {
        int y = ym + rows[i] * (w + ys);
        on = on || (((bits >> uint(i)) & 1u) != 0u && sp_box(p, cols[i], y, cols[i] + w, y + w));
    }
    return on;
}

// box.zig arc: a light line from the middle of one edge, down a straight piece, round a cubic (control points a quarter
// of the radius from the centre) and out to the middle of the next edge. `corner`: 0 tl, 1 tr, 2 bl, 3 br, the corner it
// bends toward (Ghostty's names: ╭ is .br). Returns the distance to the stroke's middle, pixels.
float sp_arc(float2 p, int W, int H, int t, uint corner)
{
    float fw = float(W);
    float fh = float(H);
    float ft = float(t);
    float cx = float(max(W - t, 0) / 2) + ft * 0.5;
    float cy = float(max(H - t, 0) / 2) + ft * 0.5;
    float r = min(fw, fh) * 0.5;
    float s = 0.25;
    float sx = (corner & 1u) != 0u ? 1.0 : -1.0;
    float sy = (corner & 2u) != 0u ? 1.0 : -1.0;
    float ey = sy > 0.0 ? fh : 0.0;
    float ex = sx > 0.0 ? fw : 0.0;
    float2 p0 = float2(cx, cy + sy * r);
    float2 p1 = float2(cx, cy + sy * s * r);
    float2 p2 = float2(cx + sx * s * r, cy);
    float2 p3 = float2(cx + sx * r, cy);
    float d = min(sd_stub(p, float2(cx, ey), p0), sd_stub(p, p3, float2(ex, cy)));
    float2 prev = p0;
    for (int i = 1; i <= 24; i++)
    {
        float u = float(i) / 24.0;
        float v = 1.0 - u;
        float2 q = v * v * v * p0 + 3.0 * v * v * u * p1 + 3.0 * v * u * u * p2 + u * u * u * p3;
        d = min(d, sd_stub(p, prev, q));
        prev = q;
    }
    return d;
}

float sprite_cov(SpriteInst s, int2 ip)
{
    uint kind = s.shape.x;
    uint a = s.shape.y;
    uint b = s.shape.z;
    int t = max(int(s.shape.w), 1);
    int W = s.rect.z;
    int H = s.rect.w;
    float2 p = float2(ip) + 0.5;
    float fw = float(W);
    float fh = float(H);
    float ft = float(t);
    if (kind == SPRITE_SHADE)
        return float(a) / 255.0;
    if (kind == SPRITE_BLOCK)
    {
        // block.zig blockShade: a side's share rounded to whole pixels, flush to that side.
        float f = float(b) / 8.0;
        int w = int(round(fw * f));
        int h = int(round(fh * f));
        bool on = a == 0u ? sp_box(ip, 0, 0, w, H)
            : a == 1u ? sp_box(ip, W - w, 0, W, H)
            : a == 2u ? sp_box(ip, 0, 0, W, h)
            : sp_box(ip, 0, H - h, W, H);
        return on ? 1.0 : 0.0;
    }
    if (kind == SPRITE_MOSAIC)
    {
        // Quadrants (block.zig) and sextants (symbols_for_legacy_computing.zig): common.zig `fill` of halves and thirds.
        float rows = float(b);
        bool on = false;
        for (int i = 0; i < 6; i++)
        {
            float c = float(i % 2);
            float r = float(i / 2);
            on = on || (((a >> uint(i)) & 1u) != 0u && sp_box(ip, frac_lo(c * 0.5, W), frac_lo(r / rows, H), frac_hi((c + 1.0) * 0.5, W), frac_hi((r + 1.0) / rows, H)));
        }
        return on ? 1.0 : 0.0;
    }
    if (kind == SPRITE_LINES)
        return sp_lines(ip, W, H, t, a) ? 1.0 : 0.0;
    if (kind == SPRITE_DASH)
        return sp_dash(ip, W, H, t, a, b) ? 1.0 : 0.0;
    if (kind == SPRITE_BRAILLE)
        return sp_braille(ip, W, H, a) ? 1.0 : 0.0;
    if (kind == SPRITE_ARC)
        return sp_aa(sp_arc(p, W, H, t, a) - ft * 0.5);
    if (kind == SPRITE_DIAGONAL)
    {
        // box.zig lightDiagonal*: corner to corner, overshooting each by half a pixel along the slope.
        float slx = min(1.0, fw / fh);
        float sly = min(1.0, fh / fw);
        float d1 = sd_stub(p, float2(fw + 0.5 * slx, -0.5 * sly), float2(-0.5 * slx, fh + 0.5 * sly));
        float d2 = sd_stub(p, float2(-0.5 * slx, -0.5 * sly), float2(fw + 0.5 * slx, fh + 0.5 * sly));
        float d = min((a & 1u) != 0u ? d1 : 1e9, (a & 2u) != 0u ? d2 : 1e9);
        return sp_aa(d - ft * 0.5);
    }
    if (kind == SPRITE_POWERLINE)
    {
        // powerline.zig: E0B0 a solid triangle, E0B1 its outline as a light chevron, E0B4 a solid half disc (a capsule
        // against the left edge), E0B5 the same stroked inside its edge; b mirrors them (E0B2, E0B3, E0B6, E0B7).
        float2 q = b != 0u ? float2(fw - p.x, p.y) : p;
        if (a == 0u)
            return sp_aa(sd_quad(q, float2(0.0, 0.0), float2(fw, fh * 0.5), float2(0.0, fh), float2(0.0, 0.0)));
        if (a == 1u)
            return sp_aa(min(sd_stub(q, float2(0.0, 0.0), float2(fw, fh * 0.5)), sd_stub(q, float2(fw, fh * 0.5), float2(0.0, fh))) - ft * 0.5);
        float r = min(fw, fh * 0.5);
        float d = sd_stub(q, float2(0.0, r), float2(0.0, fh - r)) - r;
        if (a == 4u)
            return sp_aa(d);
        return sp_aa(d) * sp_aa(-(d + ft));
    }
    return 0.0;
}

[shader("fragment")]
float4 sprite_fs(SpriteV v, StructuredBuffer<SpriteInst> insts)
{
    SpriteInst s = insts[v.inst];
    // The pixel this fragment is, whole pixels from the cell's top left (SV_Position is the pixel's centre).
    int2 ip = int2(floor(v.pos.xy)) - s.rect.xy;
    float cov = sprite_cov(s, ip);
    float fa = s.fg.a * cov;
    float ga = s.bg.a * (1.0 - fa);
    return float4(s.fg.rgb * fa + s.bg.rgb * ga, fa + ga);
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
    nointerpolation float2 fade : TEXCOORD6;
    nointerpolation float outline : TEXCOORD7;
    nointerpolation float4 banding : TEXCOORD8;
    nointerpolation uint4 gdata : TEXCOORD9;
    float x : TEXCOORD10;
    nointerpolation float4 clipx : TEXCOORD11;
};

// Dynamic dilation (SlugDilate): moves the vertex `pos.xy` along its outward normal `pos.zw` so every edge of the quad
// is half a pixel further out than the glyph's, whatever the transform `m0 m1 m3` (rows of the object-to-clip matrix)
// and the viewport `dim` (pixels). Returns the em-space sample coordinates for the moved vertex; `jac` is the inverse
// Jacobian from object space to em space.
float2 slug_dilate(float4 pos, float2 tex, float4 jac, float4 m0, float4 m1, float4 m3, float2 dim, out float2 vpos)
{
    float2 n = normalize(pos.zw);
    float s = dot(m3.xy, pos.xy) + m3.w;
    float t = dot(m3.xy, n);

    float u = (s * dot(m0.xy, n) - t * (dot(m0.xy, pos.xy) + m0.w)) * dim.x;
    float v = (s * dot(m1.xy, n) - t * (dot(m1.xy, pos.xy) + m1.w)) * dim.y;

    float s2 = s * s;
    float st = s * t;
    float uv = u * u + v * v;
    float2 d = pos.zw * (s2 * (st + sqrt(uv)) / (uv - st * st));

    vpos = pos.xy + d;
    return float2(tex.x + dot(d, jac.xy), tex.y + dot(d, jac.zw));
}

// Device pixels to a point for an outline glyph: the eased zoom's, so its edges are half a pixel out at every step.
float live_px(Frame frame)
{
    return frame.live > 0.0 ? frame.live : frame.scale;
}


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
    o.clipx = g.clipx;
    o.fade = g.fade.xy;
    o.outline = g.kind.w;
    o.banding = g.band;
    o.gdata = g.gdata;
    if (g.kind.w > 0.5)
    {
        // Outline glyph (a brand mark, icons.rs): the quad is its em box (uv, the top-left corner then the
        // bottom-right) shown on rect. Object space is em space; the rows take it to clip space (points to ndc, y
        // flipped), and each corner's normal points out of the box (`span` is negative in y: ems go up, points down).
        float2 em = lerp(g.uv.xy, g.uv.zw, c);
        float2 span = g.uv.zw - g.uv.xy;
        float2 n = sign(span) * (c * 2.0 - 1.0);
        float2 sc = g.rect.zw / (sign(span) * max(abs(span), float2(1e-6, 1e-6)));
        float2 top = g.rect.xy + float2(0, rise) - g.uv.xy * sc;
        float4 m0 = float4(2.0 * sc.x / frame.view.x, 0, 0, 2.0 * (top.x - frame.place.x) / frame.view.x - 1.0);
        float4 m1 = float4(0, -2.0 * sc.y / frame.view.y, 0, 1.0 - 2.0 * (top.y - frame.place.y) / frame.view.y);
        float4 m3 = float4(0, 0, 0, 1);
        float2 dim = frame.view * live_px(frame) / (1.0 + 2.0 * frame.slug.w);
        float2 vpos;
        o.uv = slug_dilate(float4(em, n), em, float4(1, 0, 0, 1), m0, m1, m3, dim, vpos);
        o.pos = float4(vpos.x * m0.x + m0.w, vpos.y * m1.y + m1.w, 0, 1);
        o.y = top.y + vpos.y * sc.y;
        o.x = top.x + vpos.x * sc.x;
        // A turned mark (the tree's disclosure chevron): its dilated corners turned about its middle, clockwise (y down).
        // The em coordinates ride along, so the outline turns whole.
        if (g.clip.z != 0.0)
        {
            float2 m = g.rect.xy + g.rect.zw * 0.5 + float2(0, rise);
            float2 d = float2(o.x, o.y) - m;
            float sn = sin(g.clip.z);
            float cs = cos(g.clip.z);
            float2 q = m + float2(d.x * cs - d.y * sn, d.x * sn + d.y * cs);
            o.pos = float4(to_ndc(q - frame.place.xy, frame.view), 0, 1);
            o.x = q.x;
            o.y = q.y;
        }
        return o;
    }
    float2 pt = g.rect.xy + c * g.rect.zw + float2(0, rise);
    // A turned bitmap (a disclosure chevron): its corners turned about its middle, clockwise (y down).
    if (g.clip.z != 0.0)
    {
        float2 m = g.rect.xy + g.rect.zw * 0.5 + float2(0, rise);
        float2 d = pt - m;
        float sn = sin(g.clip.z);
        float cs = cos(g.clip.z);
        pt = m + float2(d.x * cs - d.y * sn, d.x * sn + d.y * cs);
    }
    o.pos = float4(to_ndc(pt - frame.place.xy, frame.view), 0, 1);
    o.uv = lerp(g.uv.xy, g.uv.zw, c);
    o.y = pt.y;
    o.x = pt.x;
    return o;
}

// The root eligibility code of a sample-relative quadratic Bézier from the signs of its three y (or x) coordinates:
// bit 0 set: the first root counts, bit 8: the second does (SlugPixelShader.hlsl CalcRootCode).
uint slug_root_code(float y1, float y2, float y3)
{
    uint i1 = asuint(y1) >> 31U;
    uint i2 = asuint(y2) >> 30U;
    uint i3 = asuint(y3) >> 29U;

    uint shift = (i2 & 2U) | (i1 & ~2U);
    shift = (i3 & 4U) | (shift & ~4U);

    return (0x2E74U >> shift) & 0x0101U;
}

// Where the curve crosses y = 0 (SolveHorizPoly; its quadratic is a t^2 - 2 b t + c): the x coordinates at the two
// roots. A nearly linear polynomial solves -2 b t + c = 0 instead; the divisions are guarded so no infinity is made.
float2 slug_solve_horiz(float4 p12, float2 p3)
{
    float2 a = p12.xy - p12.zw * 2.0 + p3;
    float2 b = p12.xy - p12.zw;
    bool lin = abs(a.y) < 1.0 / 65536.0;
    float ra = 1.0 / (lin ? 1.0 : a.y);
    float rb = 0.5 / (b.y == 0.0 ? 1.0 : b.y);

    float d = sqrt(max(b.y * b.y - a.y * p12.y, 0.0));
    float t1 = lin ? p12.y * rb : (b.y - d) * ra;
    float t2 = lin ? t1 : (b.y + d) * ra;

    return float2((a.x * t1 - b.x * 2.0) * t1 + p12.x, (a.x * t2 - b.x * 2.0) * t2 + p12.x);
}

// Where the curve crosses x = 0 (SolveVertPoly): the y coordinates at the two roots.
float2 slug_solve_vert(float4 p12, float2 p3)
{
    float2 a = p12.xy - p12.zw * 2.0 + p3;
    float2 b = p12.xy - p12.zw;
    bool lin = abs(a.x) < 1.0 / 65536.0;
    float ra = 1.0 / (lin ? 1.0 : a.x);
    float rb = 0.5 / (b.x == 0.0 ? 1.0 : b.x);

    float d = sqrt(max(b.x * b.x - a.x * p12.x, 0.0));
    float t1 = lin ? p12.x * rb : (b.x - d) * ra;
    float t2 = lin ? t1 : (b.x + d) * ra;

    return float2((a.y * t1 - b.y * 2.0) * t1 + p12.y, (a.y * t2 - b.y * 2.0) * t2 + p12.y);
}

// The coverage of the pixel centred at em-space `render` (SlugRender): casts a horizontal and a vertical ray through
// the glyph's curves, reading only the curves of the band the pixel is in, and blends the two estimates by how near
// each ray's nearest edge is. `ppem` is the pixels to the em on each axis; `gd` is the glyph's `gdata`. The band
// buffer holds, from gd.x, a (count, list offset) pair per horizontal band then per vertical one, then the lists of
// curve indices into `curves` (two float4s a curve: p1 p2, then p3), sorted so a ray can stop at the first curve left
// of it. The outline winds counter-clockwise (ink on the left of every edge), so a root of the first kind is a
// left edge going down (horizontal ray) or a top edge going left (vertical ray), the second kind the opposite sides:
// `embolden` pixels moves each edge that far out, as the font smoothing of the system's bitmaps does at small sizes.
float slug_render(StructuredBuffer<float4> curves, StructuredBuffer<uint> bands, float2 render, float4 band_tf, uint4 gd, float2 ppem, float embolden)
{
    float e = embolden;
    int2 band_max = int2(int(gd.y), int(gd.z));
    int2 band_ix = clamp(int2(render * band_tf.xy + band_tf.zw), int2(0, 0), band_max);
    uint at = gd.x;

    float xcov = 0.0;
    float xwgt = 0.0;
    uint hb = at + 2U * uint(band_ix.y);
    uint hcount = bands[hb];
    uint hlist = at + bands[hb + 1U];
    for (uint i = 0U; i < hcount; i++)
    {
        uint slot = bands[hlist + i];
        float4 p12 = curves[slot] - float4(render, render);
        float2 p3 = curves[slot + 1U].xy - render;

        if (max(max(p12.x, p12.z), p3.x) * ppem.x < -0.5 - e)
            break;

        uint code = slug_root_code(p12.y, p12.w, p3.y);
        if (code != 0U)
        {
            float2 r = slug_solve_horiz(p12, p3) * ppem.x;
            if ((code & 1U) != 0U)
            {
                float x = r.x - e;
                xcov += saturate(x + 0.5);
                xwgt = max(xwgt, saturate(1.0 - abs(x) * 2.0));
            }
            if (code > 1U)
            {
                float x = r.y + e;
                xcov -= saturate(x + 0.5);
                xwgt = max(xwgt, saturate(1.0 - abs(x) * 2.0));
            }
        }
    }

    float ycov = 0.0;
    float ywgt = 0.0;
    uint vb = at + 2U * (uint(band_max.y) + 1U + uint(band_ix.x));
    uint vcount = bands[vb];
    uint vlist = at + bands[vb + 1U];
    for (uint j = 0U; j < vcount; j++)
    {
        uint slot = bands[vlist + j];
        float4 p12 = curves[slot] - float4(render, render);
        float2 p3 = curves[slot + 1U].xy - render;

        if (max(max(p12.y, p12.w), p3.y) * ppem.y < -0.5 - e)
            break;

        uint code = slug_root_code(p12.x, p12.z, p3.x);
        if (code != 0U)
        {
            float2 r = slug_solve_vert(p12, p3) * ppem.y;
            if ((code & 1U) != 0U)
            {
                float y = r.x + e;
                ycov -= saturate(y + 0.5);
                ywgt = max(ywgt, saturate(1.0 - abs(y) * 2.0));
            }
            if (code > 1U)
            {
                float y = r.y - e;
                ycov += saturate(y + 0.5);
                ywgt = max(ywgt, saturate(1.0 - abs(y) * 2.0));
            }
        }
    }

    // Absolute values make either winding convention work (CalcCoverage; nonzero fill).
    float coverage = max(abs(xcov * xwgt + ycov * ywgt) / max(xwgt + ywgt, 1.0 / 65536.0), min(abs(xcov), abs(ycov)));
    return saturate(coverage);
}

[shader("fragment")]
float4 glyph_fs(GlyphV v, ConstantBuffer<Frame> frame, Texture2D<float4> atlas, SamplerState smp, StructuredBuffer<float4> curves, StructuredBuffer<uint> bands)
{
    // The em box's size in pixels from the sample coordinates' derivatives (uniform control flow: before any branch).
    float2 fw = abs(ddx(v.uv)) + abs(ddy(v.uv));
    if (v.outline > 0.5)
    {
        float2 ppem = 1.0 / max(fw, float2(1e-9, 1e-9));
        float embolden = min(0.5 * (ppem.x + ppem.y) * frame.slug.x, frame.slug.w);
        float cov = slug_render(curves, bands, v.uv, v.banding, v.gdata, ppem, embolden);
        // Gamma on the coverage per appearance, its own: this is exact geometric coverage, not the platform's smoothing.
        float go = v.dark > 0.5 ? frame.slug.y : frame.slug.z;
        float ao = pow(cov * in_band(v.y, v.clip, v.fade, live_px(frame)) * in_band(v.x, v.clipx, v.clipx.zw, live_px(frame)), go) * v.color.a;
        return float4(v.color.rgb * ao, ao);
    }
    // The atlas is premultiplied RGBA: a colour glyph's own pixels, or a plain glyph's coverage as white.
    float4 t = atlas.SampleLevel(smp, v.uv, 0) * in_band(v.y, v.clip, v.fade, frame.scale) * in_band(v.x, v.clipx, v.clipx.zw, frame.scale);
    if (v.colour > 0.5)
        return t * v.color.a;
    // Coverage is gamma-adjusted per appearance: light text on dark reads thin and is boosted, dark on light is thinned.
    float g = v.dark > 0.5 ? frame.tune.x : frame.tune.y;
    float a = pow(t.a, g) * v.color.a;
    return float4(v.color.rgb * a, a);
}
