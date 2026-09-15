//! Standard wgpu compute, end to end: upload a buffer of `u32`s, square each
//! element in a WGSL compute shader, copy the result back, and print it.
//!
//! Nothing in the compute path below is ix-specific -- it is the ordinary
//! instance -> adapter -> device -> pipeline -> dispatch -> map-back sequence
//! any wgpu tutorial teaches. The only platform seam is [`create_instance`].

use wgpu::util::DeviceExt;

const SHADER: &str = include_str!("square.wgsl");
const INPUT_LEN: usize = 16;

/// The one seam between this demo and the platform: everything else in this
/// file runs against whatever `wgpu::Instance` this returns.
///
/// On an ix VM the `ix-wgpu` guest crate (indexable-inc/ix#6537, draft, not
/// yet published) builds the instance from its custom wgpu backend, which
/// forwards every wgpu call as postcard frames over AF_VSOCK guest port 5010
/// to the host GPU service -- with the rest of this file byte-for-byte
/// unchanged. Until that crate is published, the stock backends (Vulkan/GL)
/// serve the instance, so the demo also runs on any workstation with a GPU
/// and skips cleanly on headless machines.
fn create_instance() -> wgpu::Instance {
    wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle())
}

fn main() {
    let instance = create_instance();

    let adapter =
        match pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
        {
            Ok(adapter) => adapter,
            Err(err) => {
                // GPU-less VMs and headless CI land here today; once ix-wgpu
                // (ix#6537) is published, `create_instance` will hand back the
                // host GPU here instead. Exit 0 so the fleet health check
                // reads "wired up, awaiting a GPU" rather than "broken".
                println!("wgpu-compute-demo: no GPU adapter available, skipping ({err})");
                return;
            }
        };

    let info = adapter.get_info();
    println!(
        "wgpu-compute-demo: adapter {:?} (backend {}, type {:?})",
        info.name, info.backend, info.device_type
    );

    let (device, queue) =
        pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
            .expect("adapter refused the default device descriptor");

    // Upload the input operand. WGSL storage buffers are little-endian on
    // every wgpu platform, so plain `to_le_bytes` replaces a bytemuck dep.
    let input: Vec<u32> = (0..INPUT_LEN as u32).collect();
    let input_bytes: Vec<u8> = input.iter().flat_map(|v| v.to_le_bytes()).collect();
    let storage = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("squares (in-place storage)"),
        contents: &input_bytes,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
    });

    // STORAGE buffers cannot be MAP_READ, so results come home through a
    // dedicated readback staging buffer.
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("squares (readback staging)"),
        size: input_bytes.len() as u64,
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });

    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("square.wgsl"),
        source: wgpu::ShaderSource::Wgsl(SHADER.into()),
    });
    let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some("square"),
        layout: None,
        module: &module,
        entry_point: Some("main"),
        compilation_options: wgpu::PipelineCompilationOptions::default(),
        cache: None,
    });
    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("square io"),
        layout: &pipeline.get_bind_group_layout(0),
        entries: &[wgpu::BindGroupEntry {
            binding: 0,
            resource: storage.as_entire_binding(),
        }],
    });

    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    {
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor::default());
        pass.set_pipeline(&pipeline);
        pass.set_bind_group(0, &bind_group, &[]);
        pass.dispatch_workgroups(INPUT_LEN as u32, 1, 1);
    }
    encoder.copy_buffer_to_buffer(&storage, 0, &readback, 0, input_bytes.len() as u64);
    queue.submit([encoder.finish()]);

    let slice = readback.slice(..);
    slice.map_async(wgpu::MapMode::Read, |result| {
        result.expect("readback buffer failed to map");
    });
    device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("device lost while waiting for the readback map");

    let mapped = slice
        .get_mapped_range()
        .expect("readback buffer range is mapped after a successful poll");
    let squares: Vec<u32> = mapped
        .chunks_exact(4)
        .map(|chunk| u32::from_le_bytes(chunk.try_into().expect("4-byte chunk")))
        .collect();
    drop(mapped);
    readback.unmap();

    for (value, square) in input.iter().zip(&squares) {
        assert_eq!(
            value * value,
            *square,
            "GPU disagrees with the CPU about {value}^2"
        );
    }
    println!("wgpu-compute-demo: squares of 0..{INPUT_LEN} = {squares:?}");
    println!("wgpu-compute-demo: OK");
}
