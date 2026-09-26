#version 100
// 【用途】径向（缩放）模糊：沿"屏幕中心 → 当前像素"的方向多次采样并叠加，产生由中心向外
//         拖拽的速度感，常与冲击波/高潮段落搭配。
// 【使用位置】prpr 内置后处理预设，注册于 `prpr/src/core/effect.rs:11-22` 的 SHADERS 表
//         （预设名 `radialBlur`），由 `Effect::render`（`prpr/src/core/effect.rs:155-184`）执行。
// 【如何被引用】谱面 `extra.json` 的 effect 项（解析见 `prpr/src/parse/extra.rs:153-178`）。
// 【可调 uniform】模糊强度/采样数等见文件内 uniform 声明；默认值由声明行尾的 `// %值%`
//         注释给出，经 `Effect::new` 的 `DEF_REGEX`（`prpr/src/core/effect.rs:87`）解析。
// Adapted from https://godotshaders.com/shader/radical-blur-shader/
precision mediump float;

varying lowp vec2 uv;
uniform sampler2D screenTexture;

uniform float centerX; // %0.5% 0..1
uniform float centerY; // %0.5% 0..1
uniform float power; // %0.01% 0..1
uniform float sampleCount; // %6% int 1..64

void main() {
  vec2 direction = uv - vec2(centerX, centerY);
  vec3 c = vec3(0.0);
  float f = 1.0 / sampleCount;
  vec2 screen_uv = uv / 2.0 + vec2(0.5, 0.5);
  for (float i = 0.0; i < 64.0; ++i) {
    if (i >= sampleCount) break;
    c += texture2D(screenTexture, uv - power * direction * i).rgb * f;
  }
  gl_FragColor.rgb = c;
}
