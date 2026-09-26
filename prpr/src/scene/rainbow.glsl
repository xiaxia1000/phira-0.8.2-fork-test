#version 100
// 【用途】彩虹（rainbow）mod 的视觉效果：按 `time` 循环地把画面的色相整体旋转，
//         让所有音符/判定线的配色持续流转，属于"玩法 mod"而非谱面特效。
// 【使用位置】由 `prpr/src/scene/game.rs:276-281` 在 `Mods::RAINBOW` 生效时以
//         `include_str!("rainbow.glsl")` 构造一个 `Effect` 追加进 `chart.extra.effects`，
//         因而在 `Chart::render`（`prpr/src/core/chart.rs:172-183`）的谱面后处理阶段执行。
// 【可调 uniform】`time` 由 `Effect::new` 自动补入（`prpr/src/core/effect.rs:121-123`），
//         无需外部设置；其余参数见文件内 uniform 声明。
precision highp float;

varying lowp vec2 uv;
uniform sampler2D screenTexture;
uniform float time;

const float hueSpeed = 0.5;

vec3 rgb2hsv(vec3 c) {
    vec4 K = vec4(0.0, -1.0/3.0, 2.0/3.0, -1.0);
    vec4 p = mix(vec4(c.bg, K.wz), vec4(c.gb, K.xy), step(c.b, c.g));
    vec4 q = mix(vec4(p.xyw, c.r), vec4(c.r, p.yzx), step(p.x, c.r));

    float d = q.x - min(q.w, q.y);
    float e = 1.0e-10;
    return vec3(abs(q.z + (q.w - q.y) / (6.0 * d + e)),
                d / (q.x + e),
                q.x);
}

vec3 hsv2rgb(vec3 c) {
    vec3 p = abs(fract(c.xxx + vec3(0.0, 2.0/3.0, 1.0/3.0)) * 6.0 - 3.0);
    return c.z * mix(vec3(1.0), clamp(p - 1.0, 0.0, 1.0), c.y);
}

void main() {
    vec4 color = texture2D(screenTexture, uv);

    vec3 hsv = rgb2hsv(color.rgb);

    hsv.x = fract(hsv.x + time * hueSpeed);

    vec3 rgb = hsv2rgb(hsv);

    gl_FragColor = vec4(rgb, color.a);
}
