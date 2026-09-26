#version 100
// 【用途】像素化（马赛克）：把 UV 量化到较大的像素网格，模拟低分辨率 / 复古显示效果。
// 【使用位置】prpr 内置后处理预设，注册于 `prpr/src/core/effect.rs:11-22` 的 SHADERS 表
//         （预设名 `pixel`），由 `Effect::render`（`prpr/src/core/effect.rs:155-184`）执行。
// 【如何被引用】谱面 `extra.json` 的 effect 项（解析见 `prpr/src/parse/extra.rs:153-178`）。
// 【可调 uniform】像素尺寸等参数见文件内 uniform 声明；默认值由声明行尾的 `// %值%`
//         注释给出，经 `Effect::new` 的 `DEF_REGEX`（`prpr/src/core/effect.rs:87`）解析。
// Adapted from https://godotshaders.com/shader/pixelate-2/
precision mediump float;

varying lowp vec2 uv;
uniform vec2 screenSize;
uniform sampler2D screenTexture;

uniform float size; // %10.0%

void main() {
  vec2 factor = screenSize / size;
  float x = floor(uv.x * factor.x + 0.5) / factor.x;
  float y = floor(uv.y * factor.y + 0.5) / factor.y;
  gl_FragColor = texture2D(screenTexture, vec2(x, y));
}
