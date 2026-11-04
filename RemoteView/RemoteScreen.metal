// Section 4: TeamViewer-style remote screen scaling on the GPU. Mirrors
// openview/crates/ovapp/src/scale.rs (the Rust reference):
//
//   output scale < 1  separable area filter ("pixel mixing"): every output
//                     pixel is the coverage-weighted mean of the source
//                     pixels its footprint overlaps — X pass, then Y pass
//   output scale >= 1 bilinear, centre-aligned
//   then always       5-tap unsharp mask  out = C + k * (C - avg(N,S,E,W)),
//                     k = 0.5 (0 = off), clamped neighbours
//
// Colour is filtered as stored (BGRA8Unorm, no linearisation, no colour
// management — what TeamViewer renders). Only the visible part of the frame
// is processed, at device-pixel resolution.
//
// Coordinates: "out" pixels are the visible rect of the drawable (its
// origin = `dstOrigin` drawable pixels); output pixel x covers source
// columns [(x - org.x) * invScale, (x + 1 - org.x) * invScale).

#include <metal_stdlib>
using namespace metal;

// Keep in sync with `ScaleParams` in MetalScreenRenderer.swift (56 bytes,
// 8-byte aligned: four 2-component vectors, then six 4-byte scalars).
struct ScaleParams {
    float2 org;        // source (0,0) in out-pixel space (visible-rect relative)
    int2 srcSize;      // source frame size (pixels)
    int2 dstOrigin;    // visible rect origin in the drawable (pixels)
    int2 dstSize;      // visible rect size (pixels)
    float scale;       // out pixels per source pixel
    float invScale;    // source pixels per out pixel
    int rowBase;       // first source row held by the X-pass texture
    int rowCount;      // rows held by the X-pass texture
    float sharpen;     // unsharp-mask strength (0.5 TeamViewer, 0 = off)
    float pad;
};

struct VOut {
    float4 pos [[position]];
};

// One triangle covering the whole viewport.
vertex VOut ov_fullscreen_vs(uint vid [[vertex_id]]) {
    float2 p = float2(float((vid << 1) & 2u), float(vid & 2u));
    VOut o;
    o.pos = float4(p * 2.0 - 1.0, 0.0, 1.0);
    return o;
}

// Coverage of [a, b) over the unit cell [i, i + 1).
static inline float overlap(float a, float b, int i) {
    return min(b, float(i + 1)) - max(a, float(i));
}

// Pass 1 (downscale): X. Target = out width x rowCount source rows.
fragment float4 ov_area_x(VOut in [[stage_in]],
                          texture2d<float, access::read> src [[texture(0)]],
                          constant ScaleParams &p [[buffer(0)]]) {
    int x = int(in.pos.x);
    int j = int(in.pos.y);
    int sw = max(p.srcSize.x, 1);
    int row = clamp(p.rowBase + j, 0, max(p.srcSize.y - 1, 0));
    float a = (float(x) - p.org.x) * p.invScale;
    float b = a + p.invScale;
    a = max(a, 0.0);
    b = min(b, float(sw));
    int i0 = int(floor(a));
    int i1 = int(ceil(b));
    float4 acc = float4(0.0);
    float wsum = 0.0;
    for (int i = i0; i < i1 && i < i0 + 128; ++i) {
        float w = overlap(a, b, i);
        if (w > 0.0) {
            acc += w * src.read(uint2(uint(clamp(i, 0, sw - 1)), uint(row)));
            wsum += w;
        }
    }
    if (wsum <= 0.0) {
        int i = clamp(int(floor(a)), 0, sw - 1);
        return src.read(uint2(uint(i), uint(row)));
    }
    return acc / wsum;
}

// Pass 2 (downscale): Y over the X-pass result. Target = visible rect size.
fragment float4 ov_area_y(VOut in [[stage_in]],
                          texture2d<float, access::read> tmp [[texture(0)]],
                          constant ScaleParams &p [[buffer(0)]]) {
    int x = int(in.pos.x);
    int y = int(in.pos.y);
    int sh = max(p.srcSize.y, 1);
    int last = max(p.rowCount - 1, 0);
    float a = (float(y) - p.org.y) * p.invScale;
    float b = a + p.invScale;
    a = max(a, 0.0);
    b = min(b, float(sh));
    int i0 = int(floor(a));
    int i1 = int(ceil(b));
    float4 acc = float4(0.0);
    float wsum = 0.0;
    for (int i = i0; i < i1 && i < i0 + 128; ++i) {
        float w = overlap(a, b, i);
        if (w > 0.0) {
            int r = clamp(i - p.rowBase, 0, last);
            acc += w * tmp.read(uint2(uint(x), uint(r)));
            wsum += w;
        }
    }
    if (wsum <= 0.0) {
        int r = clamp(int(floor(a)) - p.rowBase, 0, last);
        return tmp.read(uint2(uint(x), uint(r)));
    }
    return acc / wsum;
}

// Upscale (scale >= 1): centre-aligned bilinear. Target = visible rect size.
fragment float4 ov_bilinear(VOut in [[stage_in]],
                            texture2d<float> src [[texture(0)]],
                            constant ScaleParams &p [[buffer(0)]]) {
    constexpr sampler lin(coord::normalized, address::clamp_to_edge, filter::linear);
    float2 size = float2(max(p.srcSize, int2(1)));
    // Output pixel centre -> source position (pixel units); the sampler's
    // texel-centre convention supplies the "- 0.5" of scale.rs.
    float2 s = (in.pos.xy - p.org) * p.invScale;
    return src.sample(lin, s / size);
}

// Final pass: unsharp mask into the drawable (viewport = visible rect).
fragment float4 ov_sharpen(VOut in [[stage_in]],
                           texture2d<float, access::read> img [[texture(0)]],
                           constant ScaleParams &p [[buffer(0)]]) {
    int2 hi = max(p.dstSize - 1, int2(0));
    int2 c = clamp(int2(in.pos.xy) - p.dstOrigin, int2(0), hi);
    float4 cv = img.read(uint2(c));
    if (p.sharpen != 0.0) {
        float4 n = img.read(uint2(int2(c.x, max(c.y - 1, 0))));
        float4 s = img.read(uint2(int2(c.x, min(c.y + 1, hi.y))));
        float4 w = img.read(uint2(int2(max(c.x - 1, 0), c.y)));
        float4 e = img.read(uint2(int2(min(c.x + 1, hi.x), c.y)));
        cv = cv + p.sharpen * (cv - 0.25 * (n + s + w + e));
    }
    return float4(saturate(cv.rgb), 1.0);
}
