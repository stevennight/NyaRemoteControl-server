// Client presentation: YUV -> RGB with a CPU-supplied matrix, drawn as a
// 4-vertex strip.
//
// Transfer (xfer): SDR video (sRGB-like) or HDR10 video (PQ, BT.2020), shown
// on an SDR back buffer (8-bit, sRGB) or an HDR one (FP16 scRGB: linear,
// BT.709 primaries, 1.0 = 80 nits). HDR10 on an SDR screen is tone-mapped;
// SDR on an HDR screen is placed at the screen's SDR white.

cbuffer Params : register(b0) {
    float4 dst;    // NDC: x0, y_top, x1, y_bottom
    float4 m0;     // RGB = M * ((yuv - off) * scale)
    float4 m1;
    float4 m2;
    float4 yoff;
    float4 yscale;
    float4 xfer;   // x: PQ input; y: scRGB output; z: SDR white in scRGB units; w: nits shown as SDR white when tone-mapping
};

Texture2D t0 : register(t0);
Texture2D t1 : register(t1);
Texture2D t2 : register(t2);
SamplerState samp : register(s0);

struct VSOut {
    float4 pos : SV_Position;
    float2 uv : TEXCOORD0;
};

VSOut vs_quad(uint id : SV_VertexID) {
    float2 t = float2(id & 1, (id >> 1) & 1);
    VSOut o;
    o.pos = float4(lerp(dst.x, dst.z, t.x), lerp(dst.y, dst.w, t.y), 0.0, 1.0);
    o.uv = t;
    return o;
}

float3 srgb_eotf(float3 c) {
    float3 lo = c / 12.92;
    float3 hi = pow((c + 0.055) / 1.055, 2.4);
    return lerp(lo, hi, step(0.04045, c));
}

float3 srgb_oetf(float3 c) {
    float3 lo = c * 12.92;
    float3 hi = 1.055 * pow(c, 1.0 / 2.4) - 0.055;
    return lerp(lo, hi, step(0.0031308, c));
}

// SMPTE ST 2084 EOTF: PQ signal -> absolute nits.
float3 pq_eotf(float3 e) {
    const float m1 = 0.1593017578125, m2 = 78.84375;
    const float c1 = 0.8359375, c2 = 18.8515625, c3 = 18.6875;
    float3 p = pow(saturate(e), 1.0 / m2);
    return 10000.0 * pow(max(p - c1, 0.0) / (c2 - c3 * p), 1.0 / m1);
}

// Rows: linear BT.709 RGB from linear BT.2020 RGB.
static const float3x3 BT2020_TO_709 = {
     1.660491, -0.587641, -0.072850,
    -0.124551,  1.132900, -0.008349,
    -0.018151, -0.100579,  1.118730,
};

float4 finish(float3 rgb) {
    if (xfer.x > 0.5) {
        float3 nits = mul(BT2020_TO_709, pq_eotf(rgb));
        if (xfer.y > 0.5) {
            return float4(nits / 80.0, 1.0);   // wide-gamut values may go negative: scRGB keeps them
        }
        // SDR screen: SDR white stays white; brighter highlights are scaled
        // back by their largest channel (keeps the hue).
        float3 c = max(nits / xfer.w, 0.0);
        float m = max(max(c.r, c.g), c.b);
        return float4(srgb_oetf(m > 1.0 ? c / m : c), 1.0);
    }
    float3 c = saturate(rgb);
    return xfer.y > 0.5 ? float4(srgb_eotf(c) * xfer.z, 1.0) : float4(c, 1.0);
}

float4 to_rgb(float3 yuv) {
    float3 v = (yuv - yoff.xyz) * yscale.xyz;
    return finish(float3(dot(m0.xyz, v), dot(m1.xyz, v), dot(m2.xyz, v)));
}

float4 ps_nv12(VSOut i) : SV_Target {
    return to_rgb(float3(t0.Sample(samp, i.uv).r, t1.Sample(samp, i.uv).rg));
}

float4 ps_ayuv(VSOut i) : SV_Target {
    float4 c = t0.Sample(samp, i.uv); // R=V G=U B=Y
    return to_rgb(float3(c.b, c.g, c.r));
}

float4 ps_planar(VSOut i) : SV_Target {
    return to_rgb(float3(t0.Sample(samp, i.uv).r, t1.Sample(samp, i.uv).r, t2.Sample(samp, i.uv).r));
}

// The UI layer (8-bit, premultiplied, sRGB) onto an HDR back buffer, at the
// screen's SDR white. Blend: ONE, INV_SRC_ALPHA.
float4 ps_overlay(VSOut i) : SV_Target {
    float4 c = t0.Sample(samp, i.uv);
    float3 straight = c.a > 0.0 ? c.rgb / c.a : 0.0;
    return float4(srgb_eotf(saturate(straight)) * xfer.z * c.a, c.a);
}
