#version 100
// 【用途】灰度：按亮度权重把画面去色，用于回忆/失落等情绪表达（常与 alpha 动画配合渐变）。
// 【使用位置】prpr 内置后处理预设，注册于 `prpr/src/core/effect.rs:11-22` 的 SHADERS 表
//         （预设名 `grayscale`），由 `Effect::render`（`prpr/src/core/effect.rs:155-184`）执行。
// 【如何被引用】谱面 `extra.json` 的 effect 项（解析见 `prpr/src/parse/extra.rs:153-178`）。
// 【可调 uniform】混合强度等参数见文件内 uniform 声明；默认值由声明行尾的 `// %值%`
//         注释给出，经 `Effect::new` 的 `DEF_REGEX`（`prpr/src/core/effect.rs:87`）解析。
// Adapted from https://www.shadertoy.com/view/lsdXDH
precision mediump float;

varying lowp vec2 uv;
uniform sampler2D screenTexture;

uniform float factor; // %1.0% 0..1

void main() {
  vec3 color = texture2D(screenTexture, uv).xyz;
  vec3 lum = vec3(0.299, 0.587, 0.114);
  vec3 gray = vec3(dot(lum, color));
  gl_FragColor = vec4(mix(color, gray, factor), 1.0);
}
