#version 450

// the PICA's triangle setup for draws w/ fragment lighting, the vertices pass thru as they are but for the quaternions:
// q and -q are the same orientation but halfway between them is no rotation at all, so a triangle's quaternions all go to
// its first one's side b4 they're interpolated, like the software rasterizer does, else the normal falls apart in between
// and the lighting goes dark there
//   glslc flip.geom -o flip.geom.spv

layout(triangles) in;
layout(triangle_strip, max_vertices = 3) out;

layout(location = 0) in vec4 in_color[];
layout(location = 1) in vec4 in_texcoords01[];
layout(location = 2) in vec2 in_texcoord2[];
layout(location = 3) noperspective in float in_depth[];
layout(location = 4) in vec4 in_quaternion[];
layout(location = 5) in vec4 in_view[];

layout(location = 0) out vec4 out_color;
layout(location = 1) out vec4 out_texcoords01;
layout(location = 2) out vec2 out_texcoord2;
layout(location = 3) noperspective out float out_depth;
layout(location = 4) out vec4 out_quaternion;
layout(location = 5) out vec4 out_view;

void main() {
    for (int i = 0; i < 3; i++) {
        gl_Position = gl_in[i].gl_Position;
        out_color = in_color[i];
        out_texcoords01 = in_texcoords01[i];
        out_texcoord2 = in_texcoord2[i];
        out_depth = in_depth[i];
        vec4 q = in_quaternion[i];
        out_quaternion = dot(q, in_quaternion[0]) < 0.0 ? -q : q;
        out_view = in_view[i];
        EmitVertex();
    }
    EndPrimitive();
}
