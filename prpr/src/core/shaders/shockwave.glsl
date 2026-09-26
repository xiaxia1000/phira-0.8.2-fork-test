#version 100
// 【用途】冲击波：从指定圆心（`centerX`/`centerY`）向外扩散一个可调宽度的环形扰动带，
//         带内的 UV 被扭曲（`distortion`），环半径随 `progress` 推进（`expand` 控制扩张量），
//         是谱面里表现"重击/爆炸"的核心后处理。
// 【使用位置】prpr 内置后处理预设，注册于 `prpr/src/core/effect.rs:11-22` 的 SHADERS 表
//         （预设名 `shockwave`），由 `Effect::render`（`prpr/src/core/effect.rs:155-184`）执行；
//         由 `Effect` 的 `time_range` 限定生效时间段，常配合动画 uniform 推进 `progress`。
// 【如何被引用】谱面 `extra.json` 的 effect 项（解析见 `prpr/src/parse/extra.rs:153-178`）。
// 【可调 uniform】`progress`（默认 0.2，取值 0..1，波前位置）、`centerX`/`centerY`（默认 0.5，
//         圆心，屏幕归一化坐标）、`width`（默认 0.1，扰动带宽度）、`distortion`（扭曲强度）、
//         `expand`（默认 10.0，扩张量）；默认值来自声明行尾的 `// %值%` 注释，
//         由 `Effect::new` 的 `DEF_REGEX`（`prpr/src/core/effect.rs:87`）解析。
// Adapted from https://www.shadertoy.com/view/llj3Dz
precision mediump float;

varying lowp vec2 uv;
uniform vec2 screenSize;
uniform sampler2D screenTexture;

uniform float progress; // %0.2% 0..1
uniform float centerX; // %0.5% 0..1
uniform float centerY; // %0.5% 0..1
uniform float width; // %0.1%
uniform float distortion; // %0.8%
uniform float expand; // %10.0%

void main() {
  float aspect = screenSize.y / screenSize.x;

  vec2 center = vec2(centerX, centerY);
  center.y = (center.y - 0.5) * aspect + 0.5;

  vec2 tex_coord = uv;
    tex_coord.y = (tex_coord.y - 0.5) * aspect + 0.5;
  float dist = distance(tex_coord, center);

  if (progress - width <= dist && dist <= progress + width) {
    float diff = dist - progress;
    float scale_diff = 1.0 - pow(abs(diff * expand), distortion);
    float dt = diff * scale_diff;

    vec2 dir = normalize(tex_coord - center);

    tex_coord += ((dir * dt) / (progress * dist * 40.0));
    gl_FragColor = texture2D(screenTexture, vec2(tex_coord.x, (tex_coord.y - 0.5) / aspect + 0.5));

    gl_FragColor += (gl_FragColor * scale_diff) / (progress * dist * 40.0);
  } else {
    gl_FragColor = texture2D(screenTexture, vec2(tex_coord.x, (tex_coord.y - 0.5) / aspect + 0.5));
  }
}