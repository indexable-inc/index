// Square every element of the bound buffer in place. One invocation per
// element (`dispatch_workgroups(len, 1, 1)` with @workgroup_size(1)) keeps
// the index arithmetic trivial to read; this is a wiring demo, not a
// throughput benchmark.
@group(0) @binding(0)
var<storage, read_write> data: array<u32>;

@compute @workgroup_size(1)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    let i = id.x;
    data[i] = data[i] * data[i];
}
