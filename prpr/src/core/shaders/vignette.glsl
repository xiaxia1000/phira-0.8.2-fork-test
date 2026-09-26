#version 100
// 【用途】暗角（vignette）：按到屏幕中心的距离压暗边缘，把视觉焦点收紧到画面中央。
// 【使用位置】prpr 内置后处理预设，注册于 `prpr/src/core/effect.rs:11-22` 的 SHADERS 表
//         （预设名 `vignette`），由 `Effect::render`（`prpr/src/core/effect.rs:155-184`）执行。
// 【如何被引用】谱面 `extra.json` 的 effect 项（解析见 `prpr/src/parse/extra.rs:153-178`）。
// 【可调 uniform】暗角强度/范围等参数见文件内 uniform 声明；默认值由声明行尾的 `// %值%`
//         注释给出，经 `Effect::new` 的 `DEF_REGEX`（`prpr/src/core/effect.rs:87`）解析。
// Adapted from https://www.shadertoy.com/view/lsKSWR
precision mediump float;

varying lowp vec2 uv;
uniform vec2 screenSize;
uniform sampler2D screenTexture;

uniform vec4 color; // %0.0, 0.0, 0.0, 1.0%
uniform float extend; // %0.25% 0..1
uniform float radius; // %15.0%

void main() {
  vec2 new_uv = uv * (1.0 - uv.yx);
  float vig = new_uv.x * new_uv.y * radius;
  vig = pow(vig, extend);
  gl_FragColor = mix(color, texture2D(screenTexture, uv), vig);
}
