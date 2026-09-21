//! wgpu renderer: textured quads (images, thumbnails, solid rects) plus glyphon text.

use std::ops::Range;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use glyphon::{
    Attrs, Buffer, Cache, Color, Family, FontSystem, Metrics, Resolution, Shaping, SwashCache,
    TextArea, TextAtlas, TextBounds, TextRenderer, Viewport, Weight,
};
use wgpu::util::DeviceExt;
use winit::window::Window;

use crate::loader::Rgba;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub fn new(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect { x, y, w, h }
    }

    pub fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && py >= self.y && px < self.x + self.w && py < self.y + self.h
    }
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Vertex {
    pos: [f32; 2],
    uv: [f32; 2],
    color: [f32; 4],
    dim: f32,
}

/// A GPU image, split into tiles if it exceeds the device's max texture size.
pub struct Texture {
    pub width: u32,
    pub height: u32,
    tiles: Vec<Tile>,
}

struct Tile {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    bind_group: wgpu::BindGroup,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Center,
    Right,
}

struct TextItem {
    text: String,
    x: f32,
    y: f32,
    size: f32,
    color: [u8; 4],
    bold: bool,
    align: Align,
}

/// Everything to draw this frame, in physical pixels.
#[derive(Default)]
pub struct Frame {
    verts: Vec<Vertex>,
    draws: Vec<(wgpu::BindGroup, Range<u32>)>,
    texts: Vec<TextItem>,
}

pub fn srgb(r: u8, g: u8, b: u8, a: f32) -> [f32; 4] {
    let lin = |c: u8| {
        let c = c as f32 / 255.0;
        if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
    };
    [lin(r), lin(g), lin(b), a]
}

impl Frame {
    fn quad(&mut self, bind_group: &wgpu::BindGroup, dst: Rect, uv: Rect, color: [f32; 4], dim: f32) {
        let start = self.verts.len() as u32;
        let (x0, y0, x1, y1) = (dst.x, dst.y, dst.x + dst.w, dst.y + dst.h);
        let (u0, v0, u1, v1) = (uv.x, uv.y, uv.x + uv.w, uv.y + uv.h);
        for (pos, uv) in [
            ([x0, y0], [u0, v0]),
            ([x1, y0], [u1, v0]),
            ([x0, y1], [u0, v1]),
            ([x0, y1], [u0, v1]),
            ([x1, y0], [u1, v0]),
            ([x1, y1], [u1, v1]),
        ] {
            self.verts.push(Vertex { pos, uv, color, dim });
        }
        let end = self.verts.len() as u32;
        match self.draws.last_mut() {
            Some((bg, range)) if bg == bind_group && range.end == start => range.end = end,
            _ => self.draws.push((bind_group.clone(), start..end)),
        }
    }

    /// Draw a whole texture stretched into `dst`. `dim` in 0..1 greys it out.
    pub fn image(&mut self, tex: &Texture, dst: Rect, dim: f32) {
        let sx = dst.w / tex.width as f32;
        let sy = dst.h / tex.height as f32;
        for t in &tex.tiles {
            let r = Rect::new(dst.x + t.x as f32 * sx, dst.y + t.y as f32 * sy, t.w as f32 * sx, t.h as f32 * sy);
            self.quad(&t.bind_group, r, Rect::new(0.0, 0.0, 1.0, 1.0), [1.0; 4], dim);
        }
    }

    pub fn text(&mut self, text: impl Into<String>, x: f32, y: f32, size: f32, color: [u8; 4], align: Align) {
        self.texts.push(TextItem { text: text.into(), x, y, size, color, bold: false, align });
    }

    pub fn bold_text(&mut self, text: impl Into<String>, x: f32, y: f32, size: f32, color: [u8; 4], align: Align) {
        self.texts.push(TextItem { text: text.into(), x, y, size, color, bold: true, align });
    }
}

pub struct Gpu {
    pub window: Arc<Window>,
    instance: wgpu::Instance,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    globals_buf: wgpu::Buffer,
    globals_bg: wgpu::BindGroup,
    tex_layout: wgpu::BindGroupLayout,
    max_tex: u32,
    white: Texture,

    font_system: FontSystem,
    swash_cache: SwashCache,
    viewport: Viewport,
    atlas: TextAtlas,
    text_renderer: TextRenderer,
}

const SHADER: &str = r#"
struct Globals { screen: vec2<f32>, pad: vec2<f32> };
@group(0) @binding(0) var<uniform> g: Globals;
@group(0) @binding(1) var samp: sampler;
@group(1) @binding(0) var tex: texture_2d<f32>;

struct VOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
    @location(2) dim: f32,
};

@vertex
fn vs_main(@location(0) pos: vec2<f32>, @location(1) uv: vec2<f32>, @location(2) color: vec4<f32>, @location(3) dim: f32) -> VOut {
    var out: VOut;
    out.pos = vec4<f32>(pos.x / g.screen.x * 2.0 - 1.0, 1.0 - pos.y / g.screen.y * 2.0, 0.0, 1.0);
    out.uv = uv;
    out.color = color;
    out.dim = dim;
    return out;
}

@fragment
fn fs_main(in: VOut) -> @location(0) vec4<f32> {
    var c = textureSample(tex, samp, in.uv) * in.color;
    let l = dot(c.rgb, vec3<f32>(0.2126, 0.7152, 0.0722));
    c = vec4<f32>(mix(c.rgb, vec3<f32>(l * 0.18), in.dim), c.a);
    return c;
}
"#;

impl Gpu {
    pub async fn new(window: Arc<Window>) -> Result<Gpu> {
        let size = window.inner_size();
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let surface = instance.create_surface(window.clone())?;
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                compatible_surface: Some(&surface),
                ..Default::default()
            })
            .await
            .map_err(|e| anyhow!("no GPU adapter: {e}"))?;
        let limits = adapter.limits();
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("cull"),
                required_limits: wgpu::Limits {
                    max_texture_dimension_2d: limits.max_texture_dimension_2d,
                    ..wgpu::Limits::downlevel_defaults()
                },
                ..Default::default()
            })
            .await?;
        let max_tex = limits.max_texture_dimension_2d;

        let caps = surface.get_capabilities(&adapter);
        let format = caps.formats.iter().copied().find(|f| f.is_srgb()).unwrap_or(caps.formats[0]);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::AutoVsync,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
            color_space: wgpu::SurfaceColorSpace::Auto,
        };
        surface.configure(&device, &config);

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("quad"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let globals_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("globals"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let tex_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("texture"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    multisampled: false,
                    view_dimension: wgpu::TextureViewDimension::D2,
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                },
                count: None,
            }],
        });
        let globals_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("globals"),
            contents: bytemuck::cast_slice(&[size.width as f32, size.height as f32, 0.0, 0.0]),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("linear"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let globals_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("globals"),
            layout: &globals_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: globals_buf.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&sampler) },
            ],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            bind_group_layouts: &[Some(&globals_layout), Some(&tex_layout)],
            ..Default::default()
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("quad"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<Vertex>() as u64,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2, 2 => Float32x4, 3 => Float32],
                })],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            cache: None,
            multiview_mask: None,
        });

        let mut font_system = FontSystem::new();
        font_system.db_mut().set_sans_serif_family("Helvetica Neue");
        let cache = Cache::new(&device);
        let viewport = Viewport::new(&device, &cache);
        let mut atlas = TextAtlas::new(&device, &queue, &cache, format);
        let text_renderer = TextRenderer::new(&mut atlas, &device, wgpu::MultisampleState::default(), None);

        let mut gpu = Gpu {
            window,
            instance,
            surface,
            device,
            queue,
            config,
            pipeline,
            globals_buf,
            globals_bg,
            tex_layout,
            max_tex,
            white: Texture { width: 1, height: 1, tiles: vec![] },
            font_system,
            swash_cache: SwashCache::new(),
            viewport,
            atlas,
            text_renderer,
        };
        gpu.white = gpu.upload(&Rgba { width: 1, height: 1, data: vec![255; 4] });
        Ok(gpu)
    }

    pub fn size(&self) -> (u32, u32) {
        (self.config.width, self.config.height)
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        self.config.width = width.max(1);
        self.config.height = height.max(1);
        self.surface.configure(&self.device, &self.config);
    }

    pub fn upload(&self, img: &Rgba) -> Texture {
        let max = self.max_tex;
        let mut tiles = Vec::new();
        for y in (0..img.height).step_by(max as usize) {
            for x in (0..img.width).step_by(max as usize) {
                let (w, h) = ((img.width - x).min(max), (img.height - y).min(max));
                let size = wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 };
                let texture = self.device.create_texture(&wgpu::TextureDescriptor {
                    label: None,
                    size,
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::Rgba8UnormSrgb,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                    view_formats: &[],
                });
                self.queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture: &texture,
                        mip_level: 0,
                        origin: wgpu::Origin3d::ZERO,
                        aspect: wgpu::TextureAspect::All,
                    },
                    &img.data,
                    wgpu::TexelCopyBufferLayout {
                        offset: (y as u64 * img.width as u64 + x as u64) * 4,
                        bytes_per_row: Some(img.width * 4),
                        rows_per_image: Some(h),
                    },
                    size,
                );
                let view = texture.create_view(&Default::default());
                let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: None,
                    layout: &self.tex_layout,
                    entries: &[wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) }],
                });
                tiles.push(Tile { x, y, w, h, bind_group });
            }
        }
        Texture { width: img.width, height: img.height, tiles }
    }

    /// Solid (optionally translucent) rectangle; color is linear RGBA, see [`srgb`].
    pub fn rect(&self, frame: &mut Frame, dst: Rect, color: [f32; 4]) {
        frame.quad(&self.white.tiles[0].bind_group, dst, Rect::new(0.0, 0.0, 1.0, 1.0), color, 0.0);
    }

    pub fn render(&mut self, frame: Frame) {
        let (w, h) = self.size();
        self.queue.write_buffer(&self.globals_buf, 0, bytemuck::cast_slice(&[w as f32, h as f32, 0.0, 0.0]));
        self.viewport.update(&self.queue, Resolution { width: w, height: h });

        // Shape text; each item is drawn twice, a dark shadow then the colour, for legibility.
        let mut buffers = Vec::with_capacity(frame.texts.len());
        for item in &frame.texts {
            let mut buf = Buffer::new(&mut self.font_system, Metrics::new(item.size, item.size * 1.25));
            buf.set_size(None, None);
            let attrs = Attrs::new().family(Family::SansSerif).weight(if item.bold { Weight::BOLD } else { Weight::NORMAL });
            buf.set_text(&item.text, &attrs, Shaping::Advanced, None);
            buf.shape_until_scroll(&mut self.font_system, false);
            let width = buf.layout_runs().map(|r| r.line_w).fold(0.0, f32::max);
            let left = match item.align {
                Align::Left => item.x,
                Align::Center => item.x - width / 2.0,
                Align::Right => item.x - width,
            };
            buffers.push((buf, left));
        }
        let bounds = TextBounds { left: 0, top: 0, right: w as i32, bottom: h as i32 };
        let mut areas = Vec::with_capacity(buffers.len() * 2);
        for ((buf, left), item) in buffers.iter().zip(&frame.texts) {
            let off = (item.size / 14.0).max(1.0);
            let [r, g, b, a] = item.color;
            areas.push(TextArea {
                buffer: buf,
                left: left + off,
                top: item.y + off,
                scale: 1.0,
                bounds,
                default_color: Color::rgba(0, 0, 0, (a as u32 * 3 / 4) as u8),
                custom_glyphs: &[],
            });
            areas.push(TextArea {
                buffer: buf,
                left: *left,
                top: item.y,
                scale: 1.0,
                bounds,
                default_color: Color::rgba(r, g, b, a),
                custom_glyphs: &[],
            });
        }
        if let Err(e) = self.text_renderer.prepare(
            &self.device,
            &self.queue,
            &mut self.font_system,
            &mut self.atlas,
            &self.viewport,
            areas,
            &mut self.swash_cache,
        ) {
            eprintln!("text: {e}");
        }

        let surface_tex = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t) => t,
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                self.window.request_redraw();
                return;
            }
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Suboptimal(_) => {
                self.surface.configure(&self.device, &self.config);
                self.window.request_redraw();
                return;
            }
            wgpu::CurrentSurfaceTexture::Lost => {
                match self.instance.create_surface(self.window.clone()) {
                    Ok(s) => self.surface = s,
                    Err(e) => eprintln!("surface lost: {e}"),
                }
                self.surface.configure(&self.device, &self.config);
                self.window.request_redraw();
                return;
            }
            wgpu::CurrentSurfaceTexture::Validation => {
                eprintln!("surface validation error");
                return;
            }
        };
        let view = surface_tex.texture.create_view(&Default::default());
        let vbuf = (!frame.verts.is_empty()).then(|| {
            self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("verts"),
                contents: bytemuck::cast_slice(&frame.verts),
                usage: wgpu::BufferUsages::VERTEX,
            })
        });

        let mut encoder = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color { r: 0.004, g: 0.004, b: 0.005, a: 1.0 }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            if let Some(vbuf) = &vbuf {
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, &self.globals_bg, &[]);
                pass.set_vertex_buffer(0, vbuf.slice(..));
                for (bg, range) in &frame.draws {
                    pass.set_bind_group(1, bg, &[]);
                    pass.draw(range.clone(), 0..1);
                }
            }
            if let Err(e) = self.text_renderer.render(&self.atlas, &self.viewport, &mut pass) {
                eprintln!("text: {e}");
            }
        }
        self.queue.submit(Some(encoder.finish()));
        self.window.pre_present_notify();
        self.queue.present(surface_tex);
        self.atlas.trim();
    }
}
