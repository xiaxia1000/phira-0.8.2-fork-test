#version 100
// 【用途】圆形（放射状）模糊：以屏幕某点为圆心，按极坐标对环带做加权采样，形成"绕着圆心旋转
//         拖开"的艺术化模糊，常用于转场、聚焦或情绪渲染。
// 【使用位置】prpr 内置后处理预设，注册于 `prpr/src/core/effect.rs:11-22` 的 SHADERS 表
//         （预设名 `circleBlur`），由 `Effect::render`（`prpr/src/core/effect.rs:155-184`）
//         作为谱面/场景的全屏后处理执行（场景级 effect 的调用点在
//         `prpr/src/scene/game.rs:1219-1229`）。
// 【如何被引用】谱面 `extra.json` 的 effect 项（解析见 `prpr/src/parse/extra.rs:153-178`）。
// 【可调 uniform】半径/强度/圆心等参数见文件内的 uniform 声明；默认值由声明行尾的
//         `// %值%` 注释给出，经 `Effect::new` 的 `DEF_REGEX`（`prpr/src/core/effect.rs:87`）解析。
// Adapted from https://godotshaders.com/shader/artsy-circle-blur-type-thingy/
precision mediump float;

varying lowp vec2 uv;
uniform vec2 screenSize;
uniform sampler2D screenTexture;

uniform float size; // %10.0%

void main() {
  vec4 c = texture2D(screenTexture, uv);
  float length = dot(c, c);
  vec2 pixel_size = 1.0 / screenSize;
  for (float x = -size; x < size; x++) {
    for (float y = -size; y < size; ++y) {
      if (x * x + y * y > size * size) continue;
      vec4 new_c = texture2D(screenTexture, uv + pixel_size * vec2(x, y));
      float new_length = dot(new_c, new_c);
      if (new_length > length) {
        length = new_length;
        c = new_c;
      }
    }
  }
  gl_FragColor = c;
}
