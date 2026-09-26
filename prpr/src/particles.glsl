// 【用途】粒子系统的 GLSL 公共代码片段（不是一个可直接编译的着色器）。它提供：
//   1) 顶点属性与 uniform 的声明块（由 `DEF_VERTEX_ATTRIBUTES` 宏开关控制），
//   2) 旋转矩阵与实例属性 → 世界坐标的变换函数（`particle_transform_vertex`），
//   3) 精灵表（atlas）UV 偏移（`particle_transform_uv`），
//   4) 顶点着色器共用的随机数与粒子生命周期查询工具（`rand`/`particle_ix`/`particle_lifetime`）。
// 【使用位置】由 `prpr/src/particle.rs:441-452` 在构造 `Emitter` 时通过
//   `macroquad::material::shaders::{preprocess_shader, PreprocessorConfig}` 以 include 方式
//   注入到粒子顶点着色器中（见 `prpr/src/particle.rs` 末尾 `mod shader` 的 `VERTEX`）；
//   后处理阶段的着色器不使用本文件。
// 【注意】`#ifdef DEF_VERTEX_ATTRIBUTES` 之间是**属性声明块**，只有被注入到顶点着色器时才启用；
//   片元着色器复用本文件时不会定义该宏，因此不会重复声明顶点属性。
#ifdef DEF_VERTEX_ATTRIBUTES
attribute vec3 in_attr_pos;
attribute vec2 in_attr_uv;
attribute vec4 in_attr_color;
attribute vec4 in_attr_inst_pos;
attribute vec4 in_attr_inst_uv;
attribute vec4 in_attr_inst_data;
attribute vec4 in_attr_inst_color;
uniform mat4 _mvp;
uniform float _local_coords;
uniform vec3 _emitter_position;

lowp mat2 rotate2d(float angle){
    return mat2(cos(angle),-sin(angle),
                sin(angle),cos(angle));
}
vec4 particle_transform_vertex() {
     vec4 transformed = vec4(0.0, 0.0, 0.0, 0.0);
     mat2 rot = rotate2d(in_attr_inst_pos.z);
     vec4 in_attr_inst_pos = vec4(in_attr_inst_pos.xy, 0.0, in_attr_inst_pos.w);
     if (_local_coords == 0.0) {
        transformed = vec4(vec3(rot * in_attr_pos.xy, in_attr_pos.z) * in_attr_inst_pos.w + in_attr_inst_pos.xyz, 1.0);
     } else {
        transformed = vec4(vec3(rot * in_attr_pos.xy, in_attr_pos.z) * in_attr_inst_pos.w + in_attr_inst_pos.xyz +
                        _emitter_position.xyz, 1.0);
     }
     return _mvp * transformed;
}

vec2 particle_transform_uv() {
    return in_attr_uv * in_attr_inst_uv.zw + in_attr_inst_uv.xy;
}
#endif

highp float rand(lowp vec2 co) {
    highp float a = 12.9898;
    highp float b = 78.233;
    highp float c = 43758.5453;
    highp float dt= dot(co.xy ,vec2(a,b));
    highp float sn= mod(dt,3.14);
    return fract(sin(sn) * c);
}

lowp float particle_ix(lowp vec4 particle_data) {
    return particle_data.x;
}

lowp float particle_lifetime(lowp vec4 particle_data) {
    return particle_data.y;
}
