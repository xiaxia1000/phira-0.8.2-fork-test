#version 100
// 【用途】色差 / 色散（chromatic aberration）：把 RGB 三个通道沿屏幕中心方向做不同程度的
//         径向偏移再叠加，产生"彩色拖影/镜头色散"的观感，常用于表现失焦、晕眩或高潮段。
// 【使用位置】作为 prpr 的内置后处理预设，注册于 `prpr/src/core/effect.rs:11-22` 的 SHADERS 表
//         （预设名 `chromatic`）；由 `Effect::render`（`prpr/src/core/effect.rs:155-184`）
//         在谱面渲染完成后以全屏 quad 执行。
// 【如何被引用】谱面 `extra.json` 的 effect 项通过 `prpr/src/parse/extra.rs:153-178` 解析；
//         内置预设名或 `/` 开头的自定义 shader 路径都可指向本文件（或同名自定义实现）。
// 【可调 uniform】`sampleCount`（默认 3，整数 1..64，采样次数，越大越平滑越慢）、
//         `power`（默认 0.01，偏移强度）。默认值写在声明行尾的 `// %值%` 注释里，
//         由 `Effect::new` 的正则 `DEF_REGEX`（`prpr/src/core/effect.rs:87`）解析。
// Adapted from https://godotshaders.com/shader/chromatic-abberation/
precision mediump float;

varying lowp vec2 uv;
uniform sampler2D screenTexture;

uniform float sampleCount; // %3% int 1..64
uniform float power; // %0.01%

vec3 chromatic_slice(float t) {
  vec3 res = vec3(1.0 - t, 1.0 - abs(t - 1.0), t - 1.0);
  return max(res, 0.0);
}

void main() {
  vec3 sum = vec3(0.0);
  vec3 c = vec3(0.0);
  vec2 offset = (uv - vec2(0.5)) * vec2(1, -1);
  int sample_count = int(sampleCount);
  for (int i = 0; i < 64; ++i) {
    if (i >= sample_count) break;
    float t = 2.0 * float(i) / float(sample_count - 1); // range 0.0->2.0
    vec3 slice = vec3(1.0 - t, 1.0 - abs(t - 1.0), t - 1.0);
    slice = max(slice, 0.0);
    sum += slice;
    vec2 slice_offset = (t - 1.0) * power * offset;
    c += slice * texture2D(screenTexture, uv + slice_offset).rgb;
  }
  gl_FragColor.rgb = c / sum;
}
