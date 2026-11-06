// Kip, den's mascot (harness-native-kip; copied from den-native's shaders/mascot.slang): a voxel character drawn off screen into a picture of its own device pixels (4x MSAA, with
// depth), then set into the frame as a premultiplied quad over a soft contact shadow. Its own metallib (SPIR-V on
// Linux in den), so harness.slang's binding table is untouched.
//
// Light is flat per face and worked in linear light: a key light, a fill from the sky above and the ground below, a rim
// from behind, each corner's ambient occlusion from the mesher (Lysenko's voxel AO, src/mascot/model.rs), and a faint
// line between cells that fades out once a cell is only a few pixels across. Inks marked to glow (the antennae's tips)
// add their own light while the character thinks. The picture is written as extended sRGB like every other pass.

static const int MAX_PARTS = 24;
static const int MAX_INKS = 16;

struct MascotFrame
{
    float4 parts[72];    // each part's transform from its cells to the picture's clip space (x, y, depth), three rows
    float4 normals[72];  // each part's transform of a normal into view space (the inverse transpose), three rows
    float4 inks[16];     // linear rgb; w how much it glows
    float4 key;          // xyz the direction toward the key light (view space), w unused
    float4 key_color;    // linear rgb times its strength
    float4 sky;          // the fill from above, linear rgb
    float4 ground;       // the fill from below, linear rgb
    float4 rim;          // the rim from behind, linear rgb times its strength; w how tight it is
    float4 rim_dir;      // xyz the direction toward the rim light (view space)
    float4 shade;        // x ambient occlusion's strength, y the cell lines' strength, z unused, w glow (0 to 1)
};

struct MascotVertex
{
    float4 pos;          // the corner in its part's cells; w the part
    uint4 info;          // normal (0 +x, 1 -x, 2 +y, 3 -y, 4 +z, 5 -z), ink, occlusion (0 darkest to 3 open), corner
};

static const float3 NORMALS[6] = { float3(1, 0, 0), float3(-1, 0, 0), float3(0, 1, 0), float3(0, -1, 0), float3(0, 0, 1), float3(0, 0, -1) };
static const float2 FACE_UV[4] = { float2(0, 0), float2(1, 0), float2(1, 1), float2(0, 1) };

float m_srgb_to_linear(float v)
{
    float a = abs(v);
    float l = a <= 0.04045 ? a / 12.92 : pow((a + 0.055) / 1.055, 2.4);
    return v < 0.0 ? -l : l;
}

float m_linear_to_srgb(float v)
{
    float a = abs(v);
    float s = a <= 0.0031308 ? a * 12.92 : 1.055 * pow(a, 1.0 / 2.4) - 0.055;
    return v < 0.0 ? -s : s;
}

struct MeshV
{
    float4 pos : SV_Position;
    nointerpolation float3 normal : NORMAL;
    nointerpolation float4 color : COLOR0;
    float ao : TEXCOORD0;
    float2 uv : TEXCOORD1;
};

[shader("vertex")]
MeshV mascot_vs(uint vid : SV_VertexID, ConstantBuffer<MascotFrame> mf, StructuredBuffer<MascotVertex> verts)
{
    MascotVertex v = verts[vid];
    int p = clamp(int(v.pos.w), 0, MAX_PARTS - 1);
    float4 q = float4(v.pos.xyz, 1.0);
    float3 c = float3(dot(mf.parts[p * 3], q), dot(mf.parts[p * 3 + 1], q), dot(mf.parts[p * 3 + 2], q));
    float3 n0 = NORMALS[min(v.info.x, 5u)];
    float3 n = float3(dot(mf.normals[p * 3].xyz, n0), dot(mf.normals[p * 3 + 1].xyz, n0), dot(mf.normals[p * 3 + 2].xyz, n0));
    MeshV o;
    o.pos = float4(c.xy, c.z, 1.0);
    o.normal = normalize(n);
    o.color = mf.inks[min(v.info.y, uint(MAX_INKS - 1))];
    o.ao = float(v.info.z) / 3.0;
    o.uv = FACE_UV[v.info.w & 3u];
    return o;
}

[shader("fragment")]
float4 mascot_fs(MeshV v, ConstantBuffer<MascotFrame> mf) : SV_Target
{
    float3 n = normalize(v.normal);
    float3 albedo = v.color.rgb;
    float ao = lerp(1.0 - mf.shade.x, 1.0, smoothstep(0.0, 1.0, v.ao));
    float3 fill = lerp(mf.ground.rgb, mf.sky.rgb, n.y * 0.5 + 0.5);
    float3 key = mf.key_color.rgb * max(dot(n, mf.key.xyz), 0.0);
    float3 rim = mf.rim.rgb * pow(max(dot(n, mf.rim_dir.xyz), 0.0), max(mf.rim.w, 1.0));
    float3 lit = albedo * (fill * ao + key * lerp(1.0, ao, 0.5)) + rim * ao;
    lit += albedo * v.color.w * mf.shade.w * 1.4;

    // The line between cells: within a pixel of a cell's edge, gone once a cell is under about six pixels.
    float2 w = max(fwidth(v.uv), float2(1e-4, 1e-4));
    float2 e = min(v.uv, 1.0 - v.uv) / w;
    float seam = 1.0 - saturate(min(e.x, e.y) - 0.25);
    float cell = 1.0 / max(w.x, w.y);
    lit *= 1.0 - mf.shade.y * seam * saturate((cell - 6.0) / 10.0);

    return float4(m_linear_to_srgb(lit.r), m_linear_to_srgb(lit.g), m_linear_to_srgb(lit.b), 1.0);
}

// The composite: the picture, or the shadow under it.

struct Frame
{
    float2 view;
    float scale;
    float time;
    float dark;
    float live;
    float2 pad1;
    float4 tune;
    float4 slug;
    float4 place;    // harness.slang's Frame.place: the view point the target's corner shows
};

struct MascotQuad
{
    float4 rect;     // x, y, w, h (points, y down)
    float4 shape;    // x opacity, y what it is (0 the picture, 1 the shadow), z, w unused
    float4 color;    // the shadow's colour (extended sRGB) and alpha
};

static const float2 CORNERS[4] = { float2(0, 0), float2(1, 0), float2(0, 1), float2(1, 1) };

struct QuadV
{
    float4 pos : SV_Position;
    float2 uv : TEXCOORD0;
};

[shader("vertex")]
QuadV mascot_quad_vs(uint vid : SV_VertexID, ConstantBuffer<Frame> frame, ConstantBuffer<MascotQuad> quad)
{
    float2 c = CORNERS[vid];
    float2 pt = quad.rect.xy + c * quad.rect.zw - frame.place.xy;
    QuadV o;
    o.pos = float4(pt.x / frame.view.x * 2.0 - 1.0, 1.0 - pt.y / frame.view.y * 2.0, 0, 1);
    o.uv = c;
    return o;
}

// Premultiplied: the picture as it is, the shadow a soft ellipse darkest under the feet.
[shader("fragment")]
float4 mascot_quad_fs(QuadV v, ConstantBuffer<MascotQuad> quad, Texture2D<float4> src, SamplerState smp) : SV_Target
{
    if (quad.shape.y < 0.5)
    {
        return src.Sample(smp, v.uv) * quad.shape.x;
    }
    float d = length(v.uv * 2.0 - 1.0);
    float a = (1.0 - smoothstep(0.2, 1.0, d)) * quad.color.a * quad.shape.x;
    return float4(quad.color.rgb * a, a);
}
