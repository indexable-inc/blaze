// A stand-in for anim (indexable-inc/anim, private) with its manifest and lockfile: each
// dependency reached once, so the load and the build cover the same crates.
use std::io::Write as _;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Vertex {
    at: [f32; 2],
}

fn main() {
    let _edit = 0u32;
    use base64::Engine as _;
    println!("{}", base64::engine::general_purpose::STANDARD.encode(b"anim"));
    let vertices = [Vertex { at: [1.0, 2.0] }];
    println!("{}", bytemuck::cast_slice::<Vertex, u8>(&vertices).len());
    let mut zlib = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    zlib.write_all(b"anim anim anim").unwrap();
    println!("{}", zlib.finish().unwrap().len());
    println!("{:?}", crossterm::terminal::size().is_ok());
    println!("{}", unsafe { libc::getpid() } > 0);
    for segment in svgtypes::SimplifyingPathParser::from("M10 10 L20 20 Z") {
        println!("{segment:?}");
    }
    let mut tree: taffy::TaffyTree<()> = taffy::TaffyTree::new();
    let leaf = tree.new_leaf(taffy::Style::default()).unwrap();
    let space = taffy::Size { width: taffy::AvailableSpace::MaxContent, height: taffy::AvailableSpace::MaxContent };
    tree.compute_layout(leaf, space).unwrap();
    println!("{:?}", tree.layout(leaf).unwrap().size);
    let mut pixmap = tiny_skia::Pixmap::new(16, 16).unwrap();
    let mut paint = tiny_skia::Paint::default();
    paint.set_color_rgba8(255, 0, 0, 255);
    let rect = tiny_skia::Rect::from_xywh(1.0, 1.0, 8.0, 8.0).unwrap();
    pixmap.fill_rect(rect, &paint, tiny_skia::Transform::identity(), None);
    println!("{}", pixmap.data().len());
    {
        use cpal::traits::HostTrait as _;
        let host = cpal::default_host();
        println!("{}", host.default_output_device().is_some());
    }
    let instance = wgpu::Instance::default();
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()));
    println!("{:?}", adapter.map(|it| it.get_info()));
}
