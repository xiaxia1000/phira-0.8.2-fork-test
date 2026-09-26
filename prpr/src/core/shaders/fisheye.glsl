#version 100
// 【用途】鱼眼 / 桶形畸变：按屏幕归一化坐标的半径做非线性重映射，产生镜头被"拉伸鼓起"的
//         视觉效果，常用于高能段落或转场冲击。
// 【使用位置】prpr 内置后处理预设，注册于 `prpr/src/core/effect.rs:11-22` 的 SHADERS 表
//         （预设名 `fisheye`），由 `Effect::render`（`prpr/src/core/effect.rs:155-184`）执行。
// 【如何被引用】谱面 `extra.json` 的 effect 项（解析见 `prpr/src/parse/extra.rs:153-178`）。
// 【可调 uniform】畸变强度等参数见文件内 uniform 声明；默认值由声明行尾的 `// %值%`
//         注释给出，经 `Effect::new` 的 `DEF_REGEX`（`prpr/src/core/effect.rs:87`）解析。
// Adapted from https://www.shadertoy.com/view/4s2GRR
precision mediump float;

varying lowp vec2 uv;
uniform vec2 screenSize;
uniform sampler2D screenTexture;

uniform float power; // %-0.1%

void main() {
  vec2 p = vec2(uv.x, uv.y * screenSize.y / screenSize.x);
  float aspect = screenSize.x / screenSize.y;
  vec2 m = vec2(0.5, 0.5 / aspect);
  vec2 d = p - m;
  float r = sqrt(dot(d, d));

  float new_power = (2.0 * 3.141592 / (2.0 * sqrt(dot(m, m)))) * power;

  float bind = new_power > 0.0? sqrt(dot(m, m)): (aspect < 1.0? m.x: m.y);

  vec2 nuv;
  if (new_power > 0.0)
    nuv = m + normalize(d) * tan(r * new_power) * bind / tan(bind * new_power);
  else
    nuv = m + normalize(d) * atan(r * -new_power * 10.0) * bind / atan(-new_power * bind * 10.0);

  gl_FragColor = texture2D(screenTexture, vec2(nuv.x, nuv.y * aspect));
}
