#version 100
// 【用途】故障 / 抖动（glitch）：按时间随机把画面按行/块做水平错位并做通道分离，
//         模拟信号干扰或数字撕裂，是节奏游戏的常用"爆点"滤镜。
// 【使用位置】prpr 内置后处理预设，注册于 `prpr/src/core/effect.rs:11-22` 的 SHADERS 表
//         （预设名 `glitch`），由 `Effect::render`（`prpr/src/core/effect.rs:155-184`）执行。
//         注意：`time` uniform 由 `Effect::new` 自动补入（`prpr/src/core/effect.rs:121-123`），
//         不需要在谱面里写，因此本文件的抖动会随播放时间自动推进。
// 【如何被引用】谱面 `extra.json` 的 effect 项（解析见 `prpr/src/parse/extra.rs:153-178`）。
// 【可调 uniform】强度/行高/偏移量等见文件内 uniform 声明；默认值由声明行尾的
//         `// %值%` 注释给出，经 `Effect::new` 的 `DEF_REGEX`（`prpr/src/core/effect.rs:87`）解析。
// Adapted from https://godotshaders.com/shader/glitch-effect-shader/
precision highp float;

varying lowp vec2 uv;
uniform sampler2D screenTexture;
uniform float time;

uniform float power; // %0.03%
uniform float rate; // %0.6% 0..1
uniform float speed; // %5.0%
uniform float blockCount; // %30.5%
uniform float colorRate; // %0.01% 0..1

float my_trunc(float x) {
  return x < 0.0? -floor(-x): floor(x);
}

float random(float seed) {
  return fract(543.2543 * sin(dot(vec2(seed, seed), vec2(3525.46, -54.3415))));
}

void main() {
  float enable_shift = float(random(my_trunc(time * speed)) < rate);

  vec2 fixed_uv = uv;
  fixed_uv.x += (random((my_trunc(uv.y * blockCount) / blockCount) + time) - 0.5) * power * enable_shift;

  vec4 pixel_color = texture2D(screenTexture, fixed_uv);
  pixel_color.r = mix(
    pixel_color.r,
    texture2D(screenTexture, fixed_uv + vec2(colorRate, 0.0)).r,
    enable_shift
  );
  pixel_color.b = mix(
    pixel_color.b,
    texture2D(screenTexture, fixed_uv + vec2(-colorRate, 0.0)).b,
    enable_shift
  );
  gl_FragColor = pixel_color;
}
