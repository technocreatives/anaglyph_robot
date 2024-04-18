use clap::Parser;
use glium::{implement_vertex, index::PrimitiveType, program, uniform, Surface};
use jpeg_decoder as jpeg;
use std::{
    sync::{Arc, RwLock},
    thread,
    time::Instant,
};
use v4l::{
    buffer::Type,
    io::traits::CaptureStream,
    prelude::*,
    video::{capture::Parameters, Capture},
    Format, FourCC,
};

#[derive(Debug, Parser)]
struct Cli {
    #[clap(default_value = "/dev/video0")]
    camera1: String,
    #[clap(default_value = "/dev/video2")]
    camera2: String,
    #[clap(long)]
    flip_x: bool,
}

type ImageBuffer = Arc<RwLock<Vec<u8>>>;

fn main() -> anyhow::Result<()> {
    let args = Cli::parse();

    let (raw_image1, format1) = cam(&args.camera1)?;
    let (raw_image2, format2) = cam(&args.camera2)?;

    // Setup the GL display stuff
    let event_loop = winit::event_loop::EventLoop::new()?;
    let (window, display) = glium::backend::glutin::SimpleWindowBuilder::new().build(&event_loop);
    window.request_redraw();
    window.set_fullscreen(Some(winit::window::Fullscreen::Borderless(None)));
    window.focus_window();
    event_loop.set_control_flow(winit::event_loop::ControlFlow::Poll);

    // building the vertex buffer, which contains all the vertices that we will draw
    let vertex_buffer = {
        #[derive(Copy, Clone)]
        struct Vertex {
            position: [f32; 2],
            tex_coords: [f32; 2],
        }

        implement_vertex!(Vertex, position, tex_coords);

        glium::VertexBuffer::new(
            &display,
            &[
                Vertex {
                    position: [-1.0, -1.0],
                    tex_coords: [0.0, 0.0],
                },
                Vertex {
                    position: [-1.0, 1.0],
                    tex_coords: [0.0, 1.0],
                },
                Vertex {
                    position: [1.0, 1.0],
                    tex_coords: [1.0, 1.0],
                },
                Vertex {
                    position: [1.0, -1.0],
                    tex_coords: [1.0, 0.0],
                },
            ],
        )
        .unwrap()
    };

    // building the index buffer
    let index_buffer =
        glium::IndexBuffer::new(&display, PrimitiveType::TriangleStrip, &[1u16, 2, 0, 3]).unwrap();

    // compiling shaders and linking them together
    let program = program!(&display,
        140 => {
            vertex: &format!("
                #version 140
                uniform mat4 matrix;
                in vec2 position;
                in vec2 tex_coords;
                out vec2 v_tex_coords;
                void main() {{
                    gl_Position = matrix * vec4(position, 0.0, 1.0);
                    v_tex_coords = tex_coords;
                }}
            "),

            fragment: &format!("
                #version 140
                uniform sampler2D tex;
                in vec2 v_tex_coords;
                out vec4 f_color;

                void main() {{
                    vec2 new_tex_coords = v_tex_coords;
                    {flip}
                    f_color = texture(tex, new_tex_coords);
                }}
            ", flip=if args.flip_x {
                "new_tex_coords.x = 1.0 - new_tex_coords.x;"
            } else {
                ""
            }),
        },
    )
    .unwrap();

    event_loop.run(move |event, elwt| {
        let t0 = Instant::now();

        let mut target = display.draw();
        target.clear_color(0.0, 0.0, 0.0, 0.0);

        let image_to_uniforms = |buffer: &ImageBuffer, format: Format| {
            let data: Vec<u8> = buffer.read().unwrap().clone();
            if data.is_empty() {
                return None;
            }

            let image = glium::texture::RawImage2d::from_raw_rgb_reversed(
                &data,
                (format.width, format.height),
            );
            let opengl_texture = glium::texture::Texture2d::new(&display, image).unwrap();
            // building the uniforms
            let uniforms = uniform! {
                matrix: [
                    [1.0, 0.0, 0.0, 0.0],
                    [0.0, 1.0, 0.0, 0.0],
                    [0.0, 0.0, 1.0, 0.0],
                    [0.0, 0.0, 0.0, 1.0f32]
                ],
                tex: opengl_texture
            };
            Some(uniforms)
        };

        if let Some(uniforms) = image_to_uniforms(&raw_image1, format1) {
            target
                .draw(
                    &vertex_buffer,
                    &index_buffer,
                    &program,
                    &uniforms,
                    &glium::DrawParameters {
                        blend: glium::Blend::alpha_blending(),
                        color_mask: (true, false, false, true),
                        ..Default::default()
                    },
                )
                .unwrap();
        }

        if let Some(uniforms) = image_to_uniforms(&raw_image2, format2) {
            target
                .draw(
                    &vertex_buffer,
                    &index_buffer,
                    &program,
                    &uniforms,
                    &glium::DrawParameters {
                        blend: glium::Blend::alpha_blending(),
                        color_mask: (false, true, true, true),
                        ..Default::default()
                    },
                )
                .unwrap();
        }

        let t1 = Instant::now();

        target.finish().unwrap();

        // polling and handling the events received by the window
        if let winit::event::Event::WindowEvent {
            event: winit::event::WindowEvent::CloseRequested,
            ..
        } = event
        {
            elwt.exit();
        }

        print!(
            "\rms: {}\t (buffer) + {}\t (UI)",
            t1.duration_since(t0).as_millis(),
            t0.elapsed().as_millis()
        );
    })?;
    Ok(())
}

fn cam(path: &str) -> anyhow::Result<(ImageBuffer, Format)> {
    println!("Using device: {}\n", path);

    // Allocate 4 buffers by default
    let buffer_count = 2;

    let mut format: Format;
    let params: Parameters;

    let dev = RwLock::new(Device::with_path(path)?);
    {
        let dev = dev.write().unwrap();
        format = dev.format()?;
        params = dev.params()?;

        // try RGB3 first
        format.fourcc = FourCC::new(b"RGB3");
        format = dev.set_format(&format)?;

        if format.fourcc != FourCC::new(b"RGB3") {
            // fallback to Motion-JPEG
            format.fourcc = FourCC::new(b"MJPG");
            format = dev.set_format(&format)?;

            if format.fourcc != FourCC::new(b"MJPG") {
                anyhow::bail!(
                    "neither RGB3 nor MJPG supported by the device, but required by this example!"
                );
            }
        }
    }

    println!("Active format:\n{}", format);
    println!("Active parameters:\n{}", params);

    let buffer = Arc::new(RwLock::new(Vec::new()));

    thread::spawn({
        let buffer = Arc::clone(&buffer);
        move || {
            let dev = dev.write().unwrap();

            // Setup a buffer stream
            let mut stream =
                MmapStream::with_buffers(&dev, Type::VideoCapture, buffer_count).unwrap();

            loop {
                let (buf, _) = stream.next().unwrap();
                let data = match &format.fourcc.repr {
                    b"RGB3" => buf.to_vec(),
                    b"MJPG" => {
                        // Decode the JPEG frame to RGB
                        let mut decoder = jpeg::Decoder::new(buf);
                        decoder.decode().expect("failed to decode JPEG")
                    }
                    _ => panic!("invalid buffer pixelformat"),
                };
                *buffer.write().unwrap() = data;
            }
        }
    });

    Ok((buffer, format))
}
