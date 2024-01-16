use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

#[allow(unused_imports)]
use tracing::{trace, debug, error, info, warn};

use winit::event::VirtualKeyCode;
use image::GenericImageView;
use wgpu::util::DeviceExt;
use cgmath::prelude::*;

use crate::*;
use gui::{App, AppWindow};

use n64::SystemCommunication;
use n64::hle::{HleRenderCommand, HleCommandBuffer};
use n64::mips::{InterruptUpdate, InterruptUpdateMode, IMask_DP};

#[repr(C)]
#[derive(Copy, Clone, Debug, bytemuck::Pod, bytemuck::Zeroable)]
struct Vertex {
    position  : [f32; 3],
    tex_coords: [f32; 2],
    color     : [f32; 4],
}

// Y texture coordinate is inverted to flip the resulting image
const GAME_TEXTURE_VERTICES: &[Vertex] = &[
    Vertex { position: [-1.0,  1.0, 0.0], tex_coords: [0.0, 0.0], color: [0.0, 0.0, 0.0, 0.0], }, // TL
    Vertex { position: [ 1.0,  1.0, 0.0], tex_coords: [1.0, 0.0], color: [0.0, 0.0, 0.0, 0.0], }, // TR
    Vertex { position: [-1.0, -1.0, 0.0], tex_coords: [0.0, 1.0], color: [0.0, 0.0, 0.0, 0.0], }, // BL
    Vertex { position: [ 1.0, -1.0, 0.0], tex_coords: [1.0, 1.0], color: [0.0, 0.0, 0.0, 0.0], }, // BR
];

const GAME_TEXTURE_INDICES: &[u16] = &[
    2, 1, 0,
    1, 3, 2,
];

impl Vertex {
    fn _new() -> Self {
        Vertex { position: [0.0, 0.0, 0.0], tex_coords: [0.0, 0.0], color: [0.0, 0.0, 0.0, 1.0], }
    }

    fn desc() -> wgpu::VertexBufferLayout<'static> {
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Vertex>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &[
                wgpu::VertexAttribute { // position
                    offset: 0,
                    shader_location: 0,
                    format: wgpu::VertexFormat::Float32x3,
                },
                wgpu::VertexAttribute { // tex_coords
                    offset: std::mem::size_of::<[f32; 3]>() as wgpu::BufferAddress,
                    shader_location: 1,
                    format: wgpu::VertexFormat::Float32x2,
                },
                wgpu::VertexAttribute { // color
                    offset: std::mem::size_of::<[f32; 5]>() as wgpu::BufferAddress,
                    shader_location: 2,
                    format: wgpu::VertexFormat::Float32x4,
                },
            ]
        }
    }

    fn size() -> usize {
        std::mem::size_of::<[f32; 9]>() as usize
    }

    fn _offset_of(index: usize) -> wgpu::BufferAddress {
        (index * Self::size()) as wgpu::BufferAddress
    }
}

#[repr(C)]
#[derive(Debug, Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
struct MvpPacked {
    mvp_matrix: [[f32; 4]; 4], // 64
    // padding to 256 bytes
    padding: [u64; 24],
}

impl MvpPacked {
    fn new(mat: [[f32; 4]; 4]) -> Self {
        Self {
           mvp_matrix: mat,
           padding: [0; 24]
        }
    }

    fn size() -> usize {
        (std::mem::size_of::<[f32; 16]>() 
          + std::mem::size_of::<[u64; 24]>()) as usize
    }

    fn offset_of(index: usize) -> wgpu::DynamicOffset {
        (index * Self::size()) as wgpu::DynamicOffset
    }
}

#[derive(Debug,Copy,Clone)]
enum ViewMode {
    Game,
    Color(usize),
    Depth(usize),
}

pub struct Game {
    comms: SystemCommunication,
    hle_command_buffer: Arc<HleCommandBuffer>,

    view_mode: ViewMode,

    game_render_textures: HashMap<u32, wgpu::Texture>,
    game_render_color_texture_bind_group_layout: wgpu::BindGroupLayout,
    game_render_depth_texture_bind_group_layout: wgpu::BindGroupLayout,
    game_render_color_texture_pipeline: wgpu::RenderPipeline,
    game_render_depth_texture_pipeline: wgpu::RenderPipeline,
    game_render_texture_vertex_buffer: wgpu::Buffer,
    game_render_texture_index_buffer: wgpu::Buffer,
    game_render_texture_bind_groups: HashMap<u32, wgpu::BindGroup>,

    game_depth_textures: HashMap<u32, wgpu::Texture>,
    game_depth_texture_bind_groups: HashMap<u32, wgpu::BindGroup>,

    raw_render_texture: Option<wgpu::Texture>,
    raw_render_texture_bind_group: Option<wgpu::BindGroup>,

    game_pipeline: wgpu::RenderPipeline,
    game_pipeline_no_depth: wgpu::RenderPipeline,

    game_viewport: HleRenderCommand,
    game_modelview: cgmath::Matrix4<f32>,
    game_projection: cgmath::Matrix4<f32>,

    vertex_buffer: wgpu::Buffer,
    index_buffer: wgpu::Buffer,
    diffuse_bind_group: wgpu::BindGroup,

    mvp_buffer: wgpu::Buffer,
    mvp_bind_group: wgpu::BindGroup,

    //speed: f32,
    //is_forward_pressed: bool,
    //is_backward_pressed: bool,
    //is_left_pressed: bool,
    //is_right_pressed: bool,

    ui_frame_count: u64,
    ui_last_fps_time: Instant,
    ui_fps: f64,

    game_frame_count: u64,
    game_last_fps_time: Instant,
    game_fps: f64,

    vertex_buffer_writes: u32,
    index_buffer_writes: u32,
}

impl App for Game {
    fn create(appwnd: &AppWindow, mut comms: SystemCommunication) -> Self {
        let device: &wgpu::Device = appwnd.device();

        // create the main color texture render shader
        let game_render_color_texture_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("Game Render Color Texture Shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("gametexture.wgsl").into()),
        });

        // create the depth texture render shader
        let game_render_depth_texture_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("Game Render Depth Texture Shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("gamedepth.wgsl").into()),
        });

        // create the texture bind group for the game textures
        let game_render_color_texture_bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("Game Render Color Texture Bind Group"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        multisampled: false,
                        view_dimension: wgpu::TextureViewDimension::D2,
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

        let game_render_depth_texture_bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("Game Render Depth Texture Bind Group"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        multisampled: false,
                        view_dimension: wgpu::TextureViewDimension::D2,
                        sample_type: wgpu::TextureSampleType::Depth,
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

        let game_render_color_texture_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("Game Render Color Texture Pipeline Layout"),
            bind_group_layouts: &[&game_render_color_texture_bind_group_layout],
            push_constant_ranges: &[],
        });

        let game_render_depth_texture_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("Game Render Depth Texture Pipeline Layout"),
            bind_group_layouts: &[&game_render_depth_texture_bind_group_layout],
            push_constant_ranges: &[],
        });

        let game_render_color_texture_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("Game Render Color Texture Pipeline"),
            layout: Some(&game_render_color_texture_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &game_render_color_texture_shader,
                entry_point: "vs_main",
                buffers: &[
                    Vertex::desc(),
                ],
            },
            fragment: Some(wgpu::FragmentState {
                module: &game_render_color_texture_shader,
                entry_point: "fs_main",
                targets: &[Some(wgpu::ColorTargetState {
                    format: appwnd.surface_config().format,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: None,
                polygon_mode: wgpu::PolygonMode::Fill,
                unclipped_depth: false,
                conservative: false,
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState {
                count: 1,
                mask: !0,
                alpha_to_coverage_enabled: false,
            },
            multiview: None,
        });

        let game_render_depth_texture_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("Game Render Depth Texture Pipeline"),
            layout: Some(&game_render_depth_texture_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &game_render_depth_texture_shader,
                entry_point: "vs_main",
                buffers: &[
                    Vertex::desc(),
                ],
            },
            fragment: Some(wgpu::FragmentState {
                module: &game_render_depth_texture_shader,
                entry_point: "fs_main",
                targets: &[Some(wgpu::ColorTargetState {
                    format: appwnd.surface_config().format,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: None,
                polygon_mode: wgpu::PolygonMode::Fill,
                unclipped_depth: false,
                conservative: false,
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState {
                count: 1,
                mask: !0,
                alpha_to_coverage_enabled: false,
            },
            multiview: None,
        });


        let game_render_texture_vertex_buffer = device.create_buffer_init(
            &wgpu::util::BufferInitDescriptor {
                label: Some("Game Render Texture Vertex Buffer"),
                contents: bytemuck::cast_slice(GAME_TEXTURE_VERTICES),
                usage: wgpu::BufferUsages::VERTEX,
            }
        );

        let game_render_texture_index_buffer = device.create_buffer_init(
            &wgpu::util::BufferInitDescriptor {
                label: Some("Game Render Texture Index Buffer"),
                contents: bytemuck::cast_slice(GAME_TEXTURE_INDICES),
                usage: wgpu::BufferUsages::INDEX,
            }
        );

        let diffuse_bytes = include_bytes!("happy-tree.png");
        let diffuse_image = image::load_from_memory(diffuse_bytes).unwrap();
        let diffuse_rgba  = diffuse_image.to_rgba8();
        let diffuse_dim   = diffuse_image.dimensions();

        let texture_size = wgpu::Extent3d {
            width: diffuse_dim.0,
            height: diffuse_dim.1,
            depth_or_array_layers: 1,
        };

        let diffuse_texture = device.create_texture(
            &wgpu::TextureDescriptor {
                size: texture_size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8UnormSrgb,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                label: Some("Game Diffuse Texture"),
                view_formats: &[],
            }
        );

        appwnd.queue().write_texture(
            wgpu::ImageCopyTexture {
                texture: &diffuse_texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &diffuse_rgba,
            wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(4 * diffuse_dim.0),
                rows_per_image: Some(diffuse_dim.1),
            },
            texture_size,
        );

        let texture_bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("Game Texture Bind Group"),
            entries: &[
                wgpu::BindGroupLayoutEntry { // TextureView
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        multisampled: false,
                        view_dimension: wgpu::TextureViewDimension::D2,
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry { // Sampler
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let diffuse_texture_view = diffuse_texture.create_view(&wgpu::TextureViewDescriptor::default());
        let diffuse_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Nearest,
            mipmap_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });

        let diffuse_bind_group = device.create_bind_group( &wgpu::BindGroupDescriptor {
            label: Some("Game Diffuse Bind Group"),
            layout: &texture_bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&diffuse_texture_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&diffuse_sampler),
                },
            ],
        });

        let game_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("Game Shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("game.wgsl").into()),
        });

        let mvp_buffer = device.create_buffer(
            &wgpu::BufferDescriptor {
                label: Some("Game MVP Matrix Buffer"),
                size : (MvpPacked::size() * 1024) as u64,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }
        );

        let mvp_bind_group_layout = device.create_bind_group_layout(
            &wgpu::BindGroupLayoutDescriptor {
                label: Some("Game MVP Matrix Bind Group Layout"),
                entries: &[
                    wgpu::BindGroupLayoutEntry { // Uniform buffer (mvp_matrix)
                        binding: 0,
                        visibility: wgpu::ShaderStages::VERTEX,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: true,
                            min_binding_size: None,
                        },
                        count: None,
                    }
                ],
            }
        );

        let mvp_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("Game MVP Matrix Bind Group"),
            layout: &mvp_bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::Buffer(
                        wgpu::BufferBinding {
                            buffer: &mvp_buffer,
                            offset: 0,
                            size: core::num::NonZeroU64::new(MvpPacked::size() as u64),
                        }
                    ),
                }
            ],
        });

        let game_render_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("Game Pipeline Layout"),
            bind_group_layouts: &[
                &texture_bind_group_layout,
                &mvp_bind_group_layout,
            ],
            push_constant_ranges: &[],
        });

        let game_pipeline_vertex_state = wgpu::VertexState {
            module: &game_shader,
            entry_point: "vs_main",
            buffers: &[Vertex::desc()],
        };

        let game_pipeline_fragment_state = wgpu::FragmentState {
            module: &game_shader,
            entry_point: "fs_main",
            targets: &[Some(wgpu::ColorTargetState {
                format: appwnd.surface_config().format,
                blend: Some(wgpu::BlendState::REPLACE),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        };

        let game_pipeline_primitive_state = wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            strip_index_format: None,
            front_face: wgpu::FrontFace::Ccw,
            cull_mode: None, //Some(wgpu::Face::Back),
            polygon_mode: wgpu::PolygonMode::Fill,
            unclipped_depth: false,
            conservative: false,
        };

        let game_pipeline_depth_stencil_state = wgpu::DepthStencilState {
            format: wgpu::TextureFormat::Depth32Float,
            depth_write_enabled: true,
            depth_compare: wgpu::CompareFunction::Less,
            stencil: wgpu::StencilState::default(),
            bias: wgpu::DepthBiasState::default()
        };

        let game_pipeline_multisample_state = wgpu::MultisampleState {
            count: 1,
            mask: !0,
            alpha_to_coverage_enabled: false,
        };

        let game_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("Game Pipeline"),
            layout: Some(&game_render_pipeline_layout),
            vertex: game_pipeline_vertex_state.clone(),
            fragment: Some(game_pipeline_fragment_state.clone()),
            primitive: game_pipeline_primitive_state,
            depth_stencil: Some(game_pipeline_depth_stencil_state),
            multisample: game_pipeline_multisample_state,
            multiview: None,
        });

        let game_pipeline_no_depth = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("Game Pipeline"),
            layout: Some(&game_render_pipeline_layout),
            vertex: game_pipeline_vertex_state,
            fragment: Some(game_pipeline_fragment_state),
            primitive: game_pipeline_primitive_state,
            depth_stencil: None,
            multisample: game_pipeline_multisample_state,
            multiview: None,
        });

        // reserve space for 64k vertices
        let vertex_buffer = device.create_buffer(
            &wgpu::BufferDescriptor {
                label: Some("Game Vertex Buffer"),
                size : (Vertex::size() * 64 * 1024) as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }
        );

        // and 10k indices
        let index_buffer = device.create_buffer(
            &wgpu::BufferDescriptor {
                label: Some("Game Index Buffer"),
                size : (std::mem::size_of::<u16>() * 10 * 1024) as u64,
                usage: wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }
        );

        let hle_command_buffer = std::mem::replace(&mut comms.hle_command_buffer, None).unwrap();
        Self {
            comms: comms,
            hle_command_buffer: hle_command_buffer,

            view_mode: ViewMode::Game,

            game_render_textures: HashMap::new(),
            game_render_color_texture_bind_group_layout: game_render_color_texture_bind_group_layout,
            game_render_depth_texture_bind_group_layout: game_render_depth_texture_bind_group_layout,
            game_render_color_texture_pipeline: game_render_color_texture_pipeline,
            game_render_depth_texture_pipeline: game_render_depth_texture_pipeline,
            game_render_texture_vertex_buffer: game_render_texture_vertex_buffer,
            game_render_texture_index_buffer: game_render_texture_index_buffer,
            game_render_texture_bind_groups: HashMap::new(),

            game_depth_textures: HashMap::new(),
            game_depth_texture_bind_groups: HashMap::new(),

            raw_render_texture: None,
            raw_render_texture_bind_group: None,

            game_pipeline: game_pipeline,
            game_pipeline_no_depth: game_pipeline_no_depth,

            game_viewport: HleRenderCommand::Noop,
            game_modelview: cgmath::Matrix4::identity(),
            game_projection: cgmath::Matrix4::identity(),

            vertex_buffer: vertex_buffer,
            index_buffer: index_buffer,
            diffuse_bind_group: diffuse_bind_group,

            mvp_buffer: mvp_buffer,
            mvp_bind_group: mvp_bind_group,

            //speed: 0.2,
            //is_forward_pressed: false,
            //is_backward_pressed: false,
            //is_left_pressed: false,
            //is_right_pressed: false,

            ui_frame_count: 0,
            ui_last_fps_time: Instant::now(),
            ui_fps: 0.0,
            game_frame_count: 0,
            game_last_fps_time: Instant::now(),
            game_fps: 0.0,

            vertex_buffer_writes: 0,
            index_buffer_writes: 0,
        }
    }

    fn update(&mut self, appwnd: &AppWindow, _delta_time: f32) {
        self.ui_frame_count += 1;
        if (self.ui_frame_count % 10) == 0 {
            self.ui_fps = 10.0 / self.ui_last_fps_time.elapsed().as_secs_f64();
            self.ui_last_fps_time = Instant::now();
        }

        // CTRL+F5+n to generate interrupt signal n
        if appwnd.input().key_held(VirtualKeyCode::LControl) {
            const KEYS: &[VirtualKeyCode] = &[
                VirtualKeyCode::F5, VirtualKeyCode::F6, VirtualKeyCode::F7,
                VirtualKeyCode::F8, VirtualKeyCode::F9, VirtualKeyCode::F10,
            ];
            if let Some(mi) = &self.comms.mi_interrupts_tx {
                for i in 0..6 {
                    if appwnd.input().key_pressed(KEYS[i]) {
                        println!("generating interrupt {}", i);
                        mi.send(InterruptUpdate(i as u32, InterruptUpdateMode::SetInterrupt)).unwrap();
                    }
                }
            }

            // CTLR+V to change the view mode
            if appwnd.input().key_pressed(VirtualKeyCode::V) {
                self.view_mode = match self.view_mode {
                    ViewMode::Game => {
                        if self.game_render_texture_bind_groups.len() > 0 {
                            ViewMode::Color(0)
                        } else if self.game_depth_texture_bind_groups.len() > 0 {
                            ViewMode::Depth(0)
                        } else {
                            ViewMode::Game
                        }
                    },
                    ViewMode::Color(i) => {
                        if self.game_render_texture_bind_groups.len() > (i + 1) {
                            ViewMode::Color(i + 1)
                        } else if self.game_depth_texture_bind_groups.len() > 0 {
                            ViewMode::Depth(0)
                        } else {
                            ViewMode::Game
                        }
                    },
                    ViewMode::Depth(i) => {
                        if self.game_depth_texture_bind_groups.len() > (i + 1) {
                            ViewMode::Depth(i + 1)
                        } else {
                            ViewMode::Game
                        }
                    },
                };
            }
        }

        //let input = appwnd.input();
        //self.is_forward_pressed  = input.key_held(VirtualKeyCode::W) || input.key_held(VirtualKeyCode::Up);
        //self.is_backward_pressed = input.key_held(VirtualKeyCode::S) || input.key_held(VirtualKeyCode::Down);
        //self.is_left_pressed     = input.key_held(VirtualKeyCode::A) || input.key_held(VirtualKeyCode::Left);
        //self.is_right_pressed    = input.key_held(VirtualKeyCode::D) || input.key_held(VirtualKeyCode::Right);

        //let forward = self.camera.target - self.camera.eye;
        //let forward_norm = forward.normalize();
        //let forward_mag = forward.magnitude();

        //if self.is_forward_pressed && forward_mag > self.speed {
        //    self.camera.eye += forward_norm * self.speed;
        //}
        //if self.is_backward_pressed {
        //    self.camera.eye -= forward_norm * self.speed;
        //}

        //let right = forward_norm.cross(self.camera.up);
        //let forward = self.camera.target - self.camera.eye;
        //let forward_mag = forward.magnitude();

        //if self.is_right_pressed {
        //    self.camera.eye = self.camera.target - (forward + right * self.speed).normalize() * forward_mag;
        //}
        //if self.is_left_pressed {
        //    self.camera.eye = self.camera.target - (forward - right * self.speed).normalize() * forward_mag;
        //}

        //self.camera_uniform.update_view_proj(&self.camera);
        //appwnd.queue().write_buffer(&self.camera_buffer, 0, bytemuck::cast_slice(&[self.camera_uniform]));

        //for instance in self.instances.iter_mut() {
        //    instance.rotation = instance.rotation * 
        //        cgmath::Quaternion::from_axis_angle(cgmath::Vector3::unit_y(), cgmath::Deg(360.0 / 2.0 * delta_time));
        //}

        //let instance_data = self.instances.iter().map(Instance::to_raw).collect::<Vec<_>>();
        //appwnd.queue().write_buffer(&self.instance_buffer, 0, bytemuck::cast_slice(&instance_data));

    }

    fn render(&mut self, appwnd: &AppWindow, view: &wgpu::TextureView) {
        self.render_game(appwnd);

        let mut encoder: wgpu::CommandEncoder =
            appwnd.device().create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("Game Render Texture Encoder") });
        {
            let mut render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("Game Render Texture Pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        // the old color doesn't matter, so LoadOp::Load is more efficient
                        load: wgpu::LoadOp::Clear(wgpu::Color { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }),
                        store: true, //. wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                //.occlusion_query_set: None,
                //.timestamp_writes: None,
            });

            // look for the texture associated with the color image address
            match self.view_mode {
                ViewMode::Game => {
                    // we need the VI_ORIGIN value to know what to render..
                    let video_buffer = self.comms.vi_origin.load(Ordering::SeqCst);
                    if video_buffer == 0 { 
                        // Throw away the render pass and encoder, no biggie
                        return; 
                    }

                    // The video buffer pointer is either exact or off by 640, or it doesn't exist at all
                    let bind_group = if self.game_render_texture_bind_groups.contains_key(&video_buffer) {
                        self.game_render_texture_bind_groups.get(&video_buffer).unwrap()
                    } else if self.game_render_texture_bind_groups.contains_key(&(video_buffer - 640)) { // video_buffer is + 640 on NTSC?
                        self.game_render_texture_bind_groups.get(&(video_buffer - 640)).unwrap()
                    } else {
                        // no game render texture found, if video_buffer is valid, render directly from RDRAM if possible
                        let width = self.comms.vi_width.load(Ordering::SeqCst) as usize;
                        let height = if width == 320 { 240 } else if width == 640 { 480 } else { warn!(target: "RENDER", "unknown render size {}", width); return; } as usize;
                        let format = self.comms.vi_format.load(Ordering::SeqCst);

                        if self.raw_render_texture.is_none() {
                            let (texture, bind_group) = self.create_color_texture(appwnd, format!("${:08X}", video_buffer).as_str(), width as u32, height as u32, true, false);
                            self.raw_render_texture = Some(texture);
                            self.raw_render_texture_bind_group = Some(bind_group);
                        }

                        // access RDRAM directly
                        // would be nice if I could copy RGB555 into a texture, but this copy seems acceptable for now
                        if let Some(rdram) = self.comms.rdram.read().as_deref().unwrap() { // rdram = &[u32]
                            let start = (video_buffer >> 2) as usize;
                            let mut image_data = vec![0u8; width*height*4];
                            for i in 0..(width*height) {
                                match format {
                                    2 => {
                                        let shift = 16 - ((i & 1) << 4);
                                        let pix = (rdram[start + (i >> 1)] >> shift) as u16;
                                        let r = ((pix >> 11) & 0x1F) as u8;
                                        let g = ((pix >>  6) & 0x1F) as u8;
                                        let b = ((pix >>  1) & 0x1F) as u8;
                                        let a = (pix & 0x01) as u8;
                                        image_data[i*4..][..4].copy_from_slice(&[r << 3, g << 3, b << 3, if a == 1 { 0 } else { 255 }]);
                                    },
                                    3 => { 
                                        let pix = rdram[start+i] | 0xff;
                                        image_data[i*4..][..4].copy_from_slice(&pix.to_be_bytes());
                                    },
                                    _ => break,
                                }
                            }

                            appwnd.queue().write_texture(
                                wgpu::ImageCopyTexture {
                                    texture: self.raw_render_texture.as_ref().unwrap(),
                                    mip_level: 0,
                                    origin: wgpu::Origin3d::ZERO,
                                    aspect: wgpu::TextureAspect::All,
                                },
                                bytemuck::cast_slice(&image_data),
                                wgpu::ImageDataLayout {
                                    offset: 0,
                                    bytes_per_row: Some(1 * 4 * width as u32), // 320 pix, rgba*f32,
                                    rows_per_image: Some(height as u32),
                                },
                                wgpu::Extent3d {
                                    width: width as u32,
                                    height: height as u32,
                                    depth_or_array_layers: 1,
                                },
                            );
                        }

                        self.raw_render_texture_bind_group.as_ref().unwrap()
                    };

                    render_pass.set_pipeline(&self.game_render_color_texture_pipeline);
                    render_pass.set_bind_group(0, bind_group, &[]);
                },

                ViewMode::Color(color_buffer) => {
                    let buffers: Vec<_> = self.game_render_texture_bind_groups.iter().collect();
                    if color_buffer >= buffers.len() {
                        return;
                    }

                    render_pass.set_pipeline(&self.game_render_color_texture_pipeline);
                    render_pass.set_bind_group(0, buffers[color_buffer].1, &[]);
                },

                ViewMode::Depth(depth_buffer) => {
                    let buffers: Vec<_> = self.game_depth_texture_bind_groups.iter().collect();
                    if depth_buffer >= buffers.len() {
                        return;
                    }

                    render_pass.set_pipeline(&self.game_render_depth_texture_pipeline);
                    render_pass.set_bind_group(0, buffers[depth_buffer].1, &[]);
                },
            };

            render_pass.set_vertex_buffer(0, self.game_render_texture_vertex_buffer.slice(..));
            render_pass.set_index_buffer(self.game_render_texture_index_buffer.slice(..), wgpu::IndexFormat::Uint16);
            render_pass.draw_indexed(0..GAME_TEXTURE_INDICES.len() as _, 0, 0..1);
        }
        appwnd.queue().submit(Some(encoder.finish()));
    }

    fn render_ui(&mut self, _appwnd: &AppWindow, ui: &imgui::Ui) {
        let window = ui.window("Stats");
        window.size([300.0, 100.0], imgui::Condition::FirstUseEver)
              .position([0.0, 0.0], imgui::Condition::Once)
              .build(|| {
                  ui.text(format!("UI   FPS: {}", self.ui_fps));
                  ui.text(format!("GAME FPS: {}", self.game_fps));
                  ui.text(format!("VIEW    : {:?} (Ctrl+V)", self.view_mode));
              });
    }
}

impl Game {
    fn create_color_texture(&mut self, appwnd: &AppWindow, name: &str, width: u32, height: u32, is_copy_dst: bool, is_filtered: bool) -> (wgpu::Texture, wgpu::BindGroup) {
        let device = appwnd.device();

        // create an offscreen render target for the actual game render
        // we double buffer so we don't get flickering when the n64/hle code is drawing too slowly
        // TODO need to resize texture with the window resize
        // OR maybe the render texture should be mapped to the n64 viewport?
        let texture = device.create_texture(
            &wgpu::TextureDescriptor {
                label: Some(format!("Game Render Texture: {name}").as_str()),
                size: wgpu::Extent3d {
                    width: width,
                    height: height,
                    ..Default::default()
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: appwnd.surface_config().format,
                // TODO at some point probably need COPY_SRC to copy the framebuffer into RDRAM
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING 
                    | if is_copy_dst { wgpu::TextureUsages::COPY_DST } else { wgpu::TextureUsages::empty() },
                view_formats: &[],
            }
        );

        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter    : if is_filtered { wgpu::FilterMode::Linear } else { wgpu::FilterMode::Nearest },
            min_filter    : wgpu::FilterMode::Nearest,
            mipmap_filter : wgpu::FilterMode::Nearest,
            ..Default::default()
        });

        let bind_group = device.create_bind_group( &wgpu::BindGroupDescriptor {
            label: Some(format!("Game Render Texture Bind Group: {name}").as_str()),
            layout: &self.game_render_color_texture_bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
            ],
        });

        (texture, bind_group)
    }

    fn create_depth_texture(&mut self, appwnd: &AppWindow, name: &str, width: u32, height: u32) -> (wgpu::Texture, wgpu::BindGroup) {
        let device = appwnd.device();

        // create texture for the depth buffer
        // TODO need to resize texture with the window resize
        // OR maybe the render texture should be mapped to the n64 viewport?
        let texture = device.create_texture(
            &wgpu::TextureDescriptor {
                label: Some(format!("Game Depth Texture: {name}").as_str()),
                size: wgpu::Extent3d {
                    width: width,
                    height: height,
                    ..Default::default()
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Depth32Float,
                // TODO at some point probably need COPY_SRC to copy the buffer into RDRAM
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            }
        );

        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter    : wgpu::FilterMode::Linear,
            min_filter    : wgpu::FilterMode::Linear,
            mipmap_filter : wgpu::FilterMode::Nearest,
            //compare: Some(wgpu::CompareFunction::LessEqual),
            lod_min_clamp: 0.0,
            lod_max_clamp: 100.0,
            ..Default::default()
        });

        let bind_group = device.create_bind_group( &wgpu::BindGroupDescriptor {
            label: Some(format!("Game Depth Texture Bind Group: {name}").as_str()),
            layout: &self.game_render_depth_texture_bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
            ],
        });

        (texture, bind_group)
    }


    fn render_game(&mut self, appwnd: &AppWindow) {
        'cmd_loop: while let Some(cmd) = self.hle_command_buffer.try_pop() {
            match cmd {
                HleRenderCommand::DefineColorImage {
                    framebuffer_address: addr,
                    ..
                } => {
                    if !self.game_render_textures.contains_key(&addr) {
                        let width = appwnd.surface_config().width;
                        let height = appwnd.surface_config().height;
                        let (texture, bind_group) = self.create_color_texture(appwnd, format!("${:08X}", addr).as_str(), width, height, false, false);
                        self.game_render_textures.insert(addr, texture);
                        self.game_render_texture_bind_groups.insert(addr, bind_group);
                        info!(target: "RENDER", "created color render target for address ${:08X} (width={})", addr, width);
                    }
                },

                HleRenderCommand::DefineDepthImage {
                    framebuffer_address: addr,
                    ..
                } => {
                    if !self.game_depth_textures.contains_key(&addr) {
                        let width = appwnd.surface_config().width;
                        let height = appwnd.surface_config().height;
                        let (texture, bind_group) = self.create_depth_texture(appwnd, format!("${:08X}", addr).as_str(), width, height);
                        self.game_depth_textures.insert(addr, texture);
                        self.game_depth_texture_bind_groups.insert(addr, bind_group);
                        info!(target: "RENDER", "created depth render target for address ${:08X} (width={})", addr, width);
                    }
                },


                HleRenderCommand::Viewport { .. } => {
                    //println!("Viewport: {:?}", cmd);
                    self.game_viewport = cmd;
                    //render_pass.set_viewport(x, y, w, h, 0.0, 1.0);
                    //render_pass.set_viewport(0.0, 0.0, 1024.0, 768.0, 0.0, 1.0);
                },

                HleRenderCommand::VertexData(v) => {
                    let mut vcopy = Vec::new();
                    for vdata in v.iter() {
                        let vnew = Vertex {
                            position: vdata.position,
                            tex_coords: vdata.tex_coords,
                            color: vdata.color,
                        };
                        vcopy.push(vnew);
                    }

                    let vertices = &vcopy;
                    appwnd.queue().write_buffer(&self.vertex_buffer, 0, bytemuck::cast_slice(vertices));
                    self.vertex_buffer_writes += vcopy.len() as u32;
                },

                HleRenderCommand::IndexData(v) => {
                    let indices = &v;
                    appwnd.queue().write_buffer(&self.index_buffer, 0, bytemuck::cast_slice(indices));
                },

                HleRenderCommand::MatrixData(v) => {
                    let mut vcopy = Vec::new();
                    for vdata in v.iter() {
                        let vnew = MvpPacked::new((*vdata).into());
                        vcopy.push(vnew);
                    }

                    let matrices = &vcopy;
                    appwnd.queue().write_buffer(&self.mvp_buffer, 0, bytemuck::cast_slice(matrices));
                },

                HleRenderCommand::RenderPass(rp) => {
                    let res = self.game_render_textures.get(&rp.color_buffer.or(Some(0xFFFF_FFFF)).unwrap());
                    let color_texture: &wgpu::Texture = if res.is_none() {
                        warn!(target: "HLE", "render pass without a color target!");
                        continue;
                    } else {
                        res.unwrap()
                    };
                    let color_view = color_texture.create_view(&wgpu::TextureViewDescriptor::default());

                    let res = self.game_depth_textures.get(&rp.depth_buffer.or(Some(0xFFFF_FFFF)).unwrap());
                    let depth_view: Option<wgpu::TextureView>;
                    let (pipeline, depth_stencil_attachment) = if res.is_none() {
                        (&self.game_pipeline_no_depth, None)
                    } else {
                        depth_view = Some(res.unwrap().create_view(&wgpu::TextureViewDescriptor::default()));
                        (&self.game_pipeline, Some(wgpu::RenderPassDepthStencilAttachment {
                            view: depth_view.as_ref().unwrap(),
                            depth_ops: Some(wgpu::Operations {
                                load: if rp.clear_depth { wgpu::LoadOp::Clear(1.0) } else { wgpu::LoadOp::Load },
                                store: true,
                            }),
                            stencil_ops: None,
                        }))
                    };

                    let mut encoder: wgpu::CommandEncoder =
                        appwnd.device().create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("Game Render Pass Encoder") });
                    {
                        let mut render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                            label: Some("Game Render Pass"),
                            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                                view: &color_view,
                                resolve_target: None,
                                ops: wgpu::Operations {
                                    load: if let Some(c) = rp.clear_color { 
                                        wgpu::LoadOp::Clear(wgpu::Color { r: c[0] as f64, g: c[1] as f64, b: c[2] as f64, a: c[3] as f64 }) 
                                    } else { 
                                        wgpu::LoadOp::Load 
                                    },
                                    store: true,
                                },
                            })],
                            depth_stencil_attachment: depth_stencil_attachment,
                        });

                        render_pass.set_pipeline(pipeline);
                        render_pass.set_bind_group(0, &self.diffuse_bind_group, &[]);
                        render_pass.set_vertex_buffer(0, self.vertex_buffer.slice(..));
                        render_pass.set_index_buffer(self.index_buffer.slice(..), wgpu::IndexFormat::Uint16);

                        for dl in rp.draw_list {
                            // using the dynamic offset into the mvp uniform buffer, we can select which matrix is used for the triangle list
                            render_pass.set_bind_group(1, &self.mvp_bind_group, &[MvpPacked::offset_of(dl.matrix_index as usize)]);
                            let last_index = dl.start_index + dl.num_indices;
                            render_pass.draw_indexed(dl.start_index..last_index as _, 0, 0..1);
                        }
                    }

                    appwnd.queue().submit(Some(encoder.finish()));
                },

                HleRenderCommand::Sync => {
                    self.game_frame_count += 1;
                    if (self.game_frame_count % 10) == 0 {
                        self.game_fps = 10.0 / self.game_last_fps_time.elapsed().as_secs_f64();
                        self.game_last_fps_time = Instant::now();
                    }

                    self.reset_render_state();

                    trace!(target: "RENDER", "vertex buffer writes: {}, index buffer writes: {}", self.vertex_buffer_writes, self.index_buffer_writes);
                    self.vertex_buffer_writes = 0;
                    self.index_buffer_writes = 0;

                    // trigger RDP interrupt to signal render is done
                    if let Some(mi) = &self.comms.mi_interrupts_tx {
                        mi.send(InterruptUpdate(IMask_DP, InterruptUpdateMode::SetInterrupt)).unwrap();
                        self.comms.check_interrupts.store(1, Ordering::SeqCst);
                    }
                    
                    break 'cmd_loop;
                },
    
                z => unimplemented!("unhandled HLE render comand {:?}", z),
            };
        }
    }

    fn reset_render_state(&mut self) {
        self.game_viewport   = HleRenderCommand::Noop;
        self.game_modelview  = cgmath::Matrix4::identity();
        self.game_projection = cgmath::Matrix4::identity();
    }
}

