#version 100
// 【用途】屏幕噪点：叠加随时间变化的随机颗粒，制造旧胶片 / 信号噪声的质感。
// 【使用位置】prpr 内置后处理预设，注册于 `prpr/src/core/effect.rs:11-22` 的 SHADERS 表
//         （预设名 `noise`），由 `Effect::render`（`prpr/src/core/effect.rs:155-184`）执行。
//         `time` uniform 由 `Effect::new` 自动补入（`prpr/src/core/effect.rs:121-123`），
//         噪点因此逐帧变化。
// 【如何被引用】谱面 `extra.json` 的 effect 项（解析见 `prpr/src/parse/extra.rs:153-178`）。
// 【可调 uniform】噪点强度等参数见文件内 uniform 声明；默认值由声明行尾的 `// %值%`
//         注释给出，经 `Effect::new` 的 `DEF_REGEX`（`prpr/src/core/effect.rs:87`）解析。
// Adapted from https://godotshaders.com/shader/screen-noise-effect-shader/
precision highp float;

varying lowp vec2 uv;
uniform sampler2D screenTexture;

uniform float seed; // %81.0%
uniform float power; // %0.03% 0..1

vec2 random(vec2 pos) {
  return fract(sin(vec2(dot(pos, vec2(12.9898,78.233)), dot(pos, vec2(-148.998,-65.233)))) * 43758.5453);
}

void main() {
  vec2 new_uv = uv + (random(uv + vec2(seed, 0.0)) - vec2(0.5, 0.5)) * power;
  gl_FragColor = texture2D(screenTexture, new_uv);
}
