use iced::wgpu;
use iced::widget::shader;
use iced::{
    Rectangle,
    wgpu::util::{BufferInitDescriptor, DeviceExt},
};

mod instance;
mod uniforms;

use super::super::Renderable;
use crate::yuv::Size;
use instance::Instance;
pub use uniforms::Uniforms;

struct TextureState {
    dimensions: Size<u32>,
    texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
}

pub struct Pipeline {
    pipeline: wgpu::RenderPipeline,
    uniforms_buffer: wgpu::Buffer,
    uniform_bind_group: wgpu::BindGroup,
    texture_bind_group_layout: wgpu::BindGroupLayout,
    vertex_buffer: wgpu::Buffer,
    scale_factor: f32,
    texture: Option<TextureState>,
}

impl shader::Pipeline for Pipeline {
    fn new(device: &wgpu::Device, _queue: &wgpu::Queue, format: wgpu::TextureFormat) -> Self {
        let uniforms_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("yuv uniform buffer"),
            size: std::mem::size_of::<Uniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let uniform_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("yuv uniform bind group layout"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: wgpu::BufferSize::new(
                            std::mem::size_of::<Uniforms>() as u64
                        ),
                    },
                    count: None,
                }],
            });

        let uniform_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("yuv uniform bind group"),
            layout: &uniform_bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                    buffer: &uniforms_buffer,
                    offset: 0,
                    size: None,
                }),
            }],
        });

        let texture_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("yuv texture bind group layout"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            multisampled: false,
                            view_dimension: wgpu::TextureViewDimension::D2Array,
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
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

        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("yuv pipeline layout"),
            bind_group_layouts: &[&uniform_bind_group_layout, &texture_bind_group_layout],
            push_constant_ranges: &[],
        });

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("yuv shader"),
            source: wgpu::ShaderSource::Wgsl(std::borrow::Cow::Borrowed(concat!(include_str!(
                "shader.wgsl"
            )))),
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("mitb_av::yuv pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[Instance::desc()],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState {
                        color: wgpu::BlendComponent {
                            src_factor: wgpu::BlendFactor::SrcAlpha,
                            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                            operation: wgpu::BlendOperation::Add,
                        },
                        alpha: wgpu::BlendComponent {
                            src_factor: wgpu::BlendFactor::One,
                            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                            operation: wgpu::BlendOperation::Add,
                        },
                    }),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                front_face: wgpu::FrontFace::Cw,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState {
                count: 1,
                mask: !0,
                alpha_to_coverage_enabled: false,
            },
            multiview: None,
            cache: None,
        });

        let vertex_buffer = device.create_buffer_init(&BufferInitDescriptor {
            label: Some("yuv vertex buffer"),
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            contents: bytemuck::cast_slice(&Instance::frame(
                Rectangle::new(iced::Point::ORIGIN, iced::Size::new(1.0, 1.0)),
                Size {
                    width: 1.0,
                    height: 1.0,
                },
            )),
        });

        Self {
            pipeline,
            uniforms_buffer,
            uniform_bind_group,
            texture_bind_group_layout,
            vertex_buffer,
            scale_factor: 1.0,
            texture: None,
        }
    }
}

impl Pipeline {
    fn recreate_texture_if_needed(&mut self, device: &wgpu::Device, image_dimensions: Size<u32>) {
        if self.texture.as_ref().is_some_and(|texture| {
            texture.dimensions.width == image_dimensions.width
                && texture.dimensions.height == image_dimensions.height
        }) {
            return;
        }

        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("yuv texture"),
            size: wgpu::Extent3d {
                width: image_dimensions.width,
                height: image_dimensions.height,
                depth_or_array_layers: 3,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::COPY_SRC
                | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });

        let texture_view = texture.create_view(&wgpu::TextureViewDescriptor {
            dimension: Some(wgpu::TextureViewDimension::D2Array),
            ..Default::default()
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("yuv sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            layout: &self.texture_bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&texture_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
            ],
            label: Some("yuv texture bind group"),
        });

        self.texture = Some(TextureState {
            dimensions: image_dimensions,
            texture,
            bind_group,
        });
    }

    pub(crate) fn ensure_texture(&mut self, image_dimensions: Size<u32>) -> bool {
        if image_dimensions.width == 0 || image_dimensions.height == 0 {
            return false;
        }

        true
    }

    pub(crate) fn update_uniforms(&mut self, queue: &wgpu::Queue, uniforms: &Uniforms) {
        queue.write_buffer(&self.uniforms_buffer, 0, bytemuck::bytes_of(uniforms));
    }

    pub(crate) fn update_frame(&mut self, queue: &wgpu::Queue, yuv: &Renderable) {
        let Some(texture_state) = self.texture.as_ref() else {
            return;
        };

        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture_state.texture,
                mip_level: 0,
                origin: wgpu::Origin3d { x: 0, y: 0, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            yuv.y(),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(yuv.y().len() as u32 / yuv.dimensions().height),
                rows_per_image: Some(yuv.dimensions().height),
            },
            wgpu::Extent3d {
                width: yuv.dimensions().width,
                height: yuv.dimensions().height,
                depth_or_array_layers: 1,
            },
        );

        let downsampled_width = yuv.dimensions().width / yuv.downsampling_factor() as u32;
        let downsampled_height = yuv.dimensions().height / yuv.downsampling_factor() as u32;

        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture_state.texture,
                mip_level: 0,
                origin: wgpu::Origin3d { x: 0, y: 0, z: 1 },
                aspect: wgpu::TextureAspect::All,
            },
            yuv.u(),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(
                    yuv.y().len() as u32
                        / yuv.dimensions().height
                        / yuv.downsampling_factor() as u32,
                ),
                rows_per_image: Some(downsampled_height),
            },
            wgpu::Extent3d {
                width: downsampled_width,
                height: downsampled_height,
                depth_or_array_layers: 1,
            },
        );

        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture_state.texture,
                mip_level: 0,
                origin: wgpu::Origin3d { x: 0, y: 0, z: 2 },
                aspect: wgpu::TextureAspect::All,
            },
            yuv.v(),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(
                    yuv.y().len() as u32
                        / yuv.dimensions().height
                        / yuv.downsampling_factor() as u32,
                ),
                rows_per_image: Some(downsampled_height),
            },
            wgpu::Extent3d {
                width: downsampled_width,
                height: downsampled_height,
                depth_or_array_layers: 1,
            },
        );
    }

    pub(crate) fn update_vertices(
        &mut self,
        queue: &wgpu::Queue,
        bounds: Rectangle,
        target_size: Size,
        scale_factor: f32,
    ) {
        self.scale_factor = scale_factor;
        queue.write_buffer(
            &self.vertex_buffer,
            0,
            bytemuck::bytes_of(&Instance::frame(bounds, target_size)),
        );
    }

    pub(crate) fn draw(&self, render_pass: &mut wgpu::RenderPass<'_>, bounds: Rectangle) {
        let Some(texture_state) = self.texture.as_ref() else {
            return;
        };

        render_pass.set_scissor_rect(
            (bounds.x * self.scale_factor) as u32,
            (bounds.y * self.scale_factor) as u32,
            (bounds.width * self.scale_factor) as u32,
            (bounds.height * self.scale_factor) as u32,
        );

        render_pass.set_vertex_buffer(0, self.vertex_buffer.slice(..));
        render_pass.set_pipeline(&self.pipeline);
        render_pass.set_bind_group(0, &self.uniform_bind_group, &[]);
        render_pass.set_bind_group(1, &texture_state.bind_group, &[]);
        render_pass.draw(0..6, 0..1);
    }

    pub(crate) fn prepare_texture(&mut self, device: &wgpu::Device, image_dimensions: Size<u32>) {
        self.recreate_texture_if_needed(device, image_dimensions);
    }
}
