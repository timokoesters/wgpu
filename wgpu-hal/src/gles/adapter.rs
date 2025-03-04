use glow::HasContext;
use std::sync::Arc;

// https://webgl2fundamentals.org/webgl/lessons/webgl-data-textures.html

const GL_UNMASKED_VENDOR_WEBGL: u32 = 0x9245;
const GL_UNMASKED_RENDERER_WEBGL: u32 = 0x9246;

impl super::Adapter {
    /// According to the OpenGL specification, the version information is
    /// expected to follow the following syntax:
    ///
    /// ~~~bnf
    /// <major>       ::= <number>
    /// <minor>       ::= <number>
    /// <revision>    ::= <number>
    /// <vendor-info> ::= <string>
    /// <release>     ::= <major> "." <minor> ["." <release>]
    /// <version>     ::= <release> [" " <vendor-info>]
    /// ~~~
    ///
    /// Note that this function is intentionally lenient in regards to parsing,
    /// and will try to recover at least the first two version numbers without
    /// resulting in an `Err`.
    /// # Notes
    /// `WebGL 2` version returned as `OpenGL ES 3.0`
    fn parse_version(mut src: &str) -> Result<(u8, u8), crate::InstanceError> {
        let webgl_sig = "WebGL ";
        // According to the WebGL specification
        // VERSION  WebGL<space>1.0<space><vendor-specific information>
        // SHADING_LANGUAGE_VERSION WebGL<space>GLSL<space>ES<space>1.0<space><vendor-specific information>
        let is_webgl = src.starts_with(webgl_sig);
        if is_webgl {
            let pos = src.rfind(webgl_sig).unwrap_or(0);
            src = &src[pos + webgl_sig.len()..];
        } else {
            let es_sig = " ES ";
            match src.rfind(es_sig) {
                Some(pos) => {
                    src = &src[pos + es_sig.len()..];
                }
                None => {
                    log::warn!("ES not found in '{}'", src);
                    return Err(crate::InstanceError);
                }
            }
        };

        let glsl_es_sig = "GLSL ES ";
        let is_glsl = match src.find(glsl_es_sig) {
            Some(pos) => {
                src = &src[pos + glsl_es_sig.len()..];
                true
            }
            None => false,
        };

        let (version, _vendor_info) = match src.find(' ') {
            Some(i) => (&src[..i], src[i + 1..].to_string()),
            None => (src, String::new()),
        };

        // TODO: make this even more lenient so that we can also accept
        // `<major> "." <minor> [<???>]`
        let mut it = version.split('.');
        let major = it.next().and_then(|s| s.parse().ok());
        let minor = it.next().and_then(|s| {
            let trimmed = if s.starts_with('0') {
                "0"
            } else {
                s.trim_end_matches('0')
            };
            trimmed.parse().ok()
        });

        match (major, minor) {
            (Some(major), Some(minor)) => Ok((
                // Return WebGL 2.0 version as OpenGL ES 3.0
                if is_webgl && !is_glsl {
                    major + 1
                } else {
                    major
                },
                minor,
            )),
            _ => {
                log::warn!("Unable to extract the version from '{}'", version);
                Err(crate::InstanceError)
            }
        }
    }

    fn make_info(vendor_orig: String, renderer_orig: String) -> wgt::AdapterInfo {
        let vendor = vendor_orig.to_lowercase();
        let renderer = renderer_orig.to_lowercase();

        // opengl has no way to discern device_type, so we can try to infer it from the renderer string
        let strings_that_imply_integrated = [
            " xpress", // space here is on purpose so we don't match express
            "radeon hd 4200",
            "radeon hd 4250",
            "radeon hd 4290",
            "radeon hd 4270",
            "radeon hd 4225",
            "radeon hd 3100",
            "radeon hd 3200",
            "radeon hd 3000",
            "radeon hd 3300",
            "radeon(tm) r4 graphics",
            "radeon(tm) r5 graphics",
            "radeon(tm) r6 graphics",
            "radeon(tm) r7 graphics",
            "radeon r7 graphics",
            "nforce", // all nvidia nforce are integrated
            "tegra",  // all nvidia tegra are integrated
            "shield", // all nvidia shield are integrated
            "igp",
            "mali",
            "intel",
            "v3d",
        ];
        let strings_that_imply_cpu = ["mesa offscreen", "swiftshader", "llvmpipe"];

        //TODO: handle Intel Iris XE as discreet
        let inferred_device_type = if vendor.contains("qualcomm")
            || vendor.contains("intel")
            || strings_that_imply_integrated
                .iter()
                .any(|&s| renderer.contains(s))
        {
            wgt::DeviceType::IntegratedGpu
        } else if strings_that_imply_cpu.iter().any(|&s| renderer.contains(s)) {
            wgt::DeviceType::Cpu
        } else {
            wgt::DeviceType::DiscreteGpu
        };

        // source: Sascha Willems at Vulkan
        let vendor_id = if vendor.contains("amd") {
            0x1002
        } else if vendor.contains("imgtec") {
            0x1010
        } else if vendor.contains("nvidia") {
            0x10DE
        } else if vendor.contains("arm") {
            0x13B5
        } else if vendor.contains("qualcomm") {
            0x5143
        } else if vendor.contains("intel") {
            0x8086
        } else if vendor.contains("broadcom") {
            0x14e4
        } else {
            0
        };

        wgt::AdapterInfo {
            name: renderer_orig,
            vendor: vendor_id,
            device: 0,
            device_type: inferred_device_type,
            backend: wgt::Backend::Gl,
        }
    }

    pub(super) unsafe fn expose(
        context: super::AdapterContext,
    ) -> Option<crate::ExposedAdapter<super::Api>> {
        let gl = context.lock();
        let extensions = gl.supported_extensions();

        let (vendor_const, renderer_const) = if extensions.contains("WEBGL_debug_renderer_info") {
            (GL_UNMASKED_VENDOR_WEBGL, GL_UNMASKED_RENDERER_WEBGL)
        } else {
            (glow::VENDOR, glow::RENDERER)
        };
        let (vendor, renderer) = {
            let vendor = gl.get_parameter_string(vendor_const);
            let renderer = gl.get_parameter_string(renderer_const);

            (vendor, renderer)
        };
        let version = gl.get_parameter_string(glow::VERSION);

        log::info!("Vendor: {}", vendor);
        log::info!("Renderer: {}", renderer);
        log::info!("Version: {}", version);

        log::debug!("Extensions: {:#?}", extensions);

        let ver = Self::parse_version(&version).ok()?;

        let supports_storage = ver >= (3, 1);
        let shading_language_version = {
            let sl_version = gl.get_parameter_string(glow::SHADING_LANGUAGE_VERSION);
            log::info!("SL version: {}", &sl_version);
            let (sl_major, sl_minor) = Self::parse_version(&sl_version).ok()?;
            let value = sl_major as u16 * 100 + sl_minor as u16 * 10;
            naga::back::glsl::Version::Embedded(value)
        };

        let vertex_shader_storage_blocks = if supports_storage {
            gl.get_parameter_i32(glow::MAX_VERTEX_SHADER_STORAGE_BLOCKS) as u32
        } else {
            0
        };
        let fragment_shader_storage_blocks = if supports_storage {
            gl.get_parameter_i32(glow::MAX_FRAGMENT_SHADER_STORAGE_BLOCKS) as u32
        } else {
            0
        };
        let vertex_shader_storage_textures = if supports_storage {
            gl.get_parameter_i32(glow::MAX_VERTEX_IMAGE_UNIFORMS) as u32
        } else {
            0
        };
        let fragment_shader_storage_textures = if supports_storage {
            gl.get_parameter_i32(glow::MAX_FRAGMENT_IMAGE_UNIFORMS) as u32
        } else {
            0
        };
        let max_storage_block_size = if supports_storage {
            gl.get_parameter_i32(glow::MAX_SHADER_STORAGE_BLOCK_SIZE) as u32
        } else {
            0
        };

        // WORKAROUND: In order to work around an issue with GL on RPI4 and similar, we ignore a
        // zero vertex ssbo count if there are vertex sstos. (more info:
        // https://github.com/gfx-rs/wgpu/pull/1607#issuecomment-874938961) The hardware does not
        // want us to write to these SSBOs, but GLES cannot express that. We detect this case and
        // disable writing to SSBOs.
        let vertex_ssbo_false_zero =
            vertex_shader_storage_blocks == 0 && vertex_shader_storage_textures != 0;
        if vertex_ssbo_false_zero {
            // We only care about fragment here as the 0 is a lie.
            log::warn!("Max vertex shader SSBO == 0 and SSTO != 0. Interpreting as false zero.");
        }

        let max_storage_buffers_per_shader_stage = if vertex_shader_storage_blocks == 0 {
            fragment_shader_storage_blocks
        } else {
            vertex_shader_storage_blocks.min(fragment_shader_storage_blocks)
        };
        let max_storage_textures_per_shader_stage = if vertex_shader_storage_textures == 0 {
            fragment_shader_storage_textures
        } else {
            vertex_shader_storage_textures.min(fragment_shader_storage_textures)
        };

        let mut downlevel_flags = wgt::DownlevelFlags::empty()
            | wgt::DownlevelFlags::DEVICE_LOCAL_IMAGE_COPIES
            | wgt::DownlevelFlags::NON_POWER_OF_TWO_MIPMAPPED_TEXTURES
            | wgt::DownlevelFlags::CUBE_ARRAY_TEXTURES
            | wgt::DownlevelFlags::COMPARISON_SAMPLERS;
        downlevel_flags.set(wgt::DownlevelFlags::COMPUTE_SHADERS, ver >= (3, 1));
        downlevel_flags.set(
            wgt::DownlevelFlags::FRAGMENT_WRITABLE_STORAGE,
            max_storage_block_size != 0,
        );
        downlevel_flags.set(wgt::DownlevelFlags::INDIRECT_EXECUTION, ver >= (3, 1));
        //TODO: we can actually support positive `base_vertex` in the same way
        // as we emulate the `start_instance`. But we can't deal with negatives...
        downlevel_flags.set(wgt::DownlevelFlags::BASE_VERTEX, ver >= (3, 2));
        downlevel_flags.set(
            wgt::DownlevelFlags::INDEPENDENT_BLENDING,
            ver >= (3, 2) || extensions.contains("GL_EXT_draw_buffers_indexed"),
        );
        downlevel_flags.set(
            wgt::DownlevelFlags::VERTEX_STORAGE,
            max_storage_block_size != 0
                && (vertex_shader_storage_blocks != 0 || vertex_ssbo_false_zero),
        );
        downlevel_flags.set(wgt::DownlevelFlags::FRAGMENT_STORAGE, supports_storage);

        let mut features = wgt::Features::empty()
            | wgt::Features::TEXTURE_COMPRESSION_ETC2
            | wgt::Features::TEXTURE_ADAPTER_SPECIFIC_FORMAT_FEATURES
            | wgt::Features::CLEAR_COMMANDS;
        features.set(
            wgt::Features::DEPTH_CLAMPING,
            extensions.contains("GL_EXT_depth_clamp"),
        );
        features.set(
            wgt::Features::VERTEX_WRITABLE_STORAGE,
            downlevel_flags.contains(wgt::DownlevelFlags::VERTEX_STORAGE)
                && vertex_shader_storage_textures != 0,
        );

        let mut private_caps = super::PrivateCapabilities::empty();
        private_caps.set(
            super::PrivateCapabilities::BUFFER_ALLOCATION,
            extensions.contains("GL_EXT_buffer_storage"),
        );
        private_caps.set(
            super::PrivateCapabilities::SHADER_BINDING_LAYOUT,
            ver >= (3, 1),
        );
        private_caps.set(
            super::PrivateCapabilities::SHADER_TEXTURE_SHADOW_LOD,
            extensions.contains("GL_EXT_texture_shadow_lod"),
        );
        private_caps.set(super::PrivateCapabilities::MEMORY_BARRIERS, ver >= (3, 1));
        private_caps.set(
            super::PrivateCapabilities::VERTEX_BUFFER_LAYOUT,
            ver >= (3, 1),
        );
        private_caps.set(
            super::PrivateCapabilities::INDEX_BUFFER_ROLE_CHANGE,
            cfg!(not(target_arch = "wasm32")),
        );
        private_caps.set(
            super::PrivateCapabilities::CAN_DISABLE_DRAW_BUFFER,
            cfg!(not(target_arch = "wasm32")),
        );

        let max_texture_size = gl.get_parameter_i32(glow::MAX_TEXTURE_SIZE) as u32;
        let max_texture_3d_size = gl.get_parameter_i32(glow::MAX_3D_TEXTURE_SIZE) as u32;

        let min_uniform_buffer_offset_alignment =
            gl.get_parameter_i32(glow::UNIFORM_BUFFER_OFFSET_ALIGNMENT) as u32;
        let min_storage_buffer_offset_alignment = if ver >= (3, 1) {
            gl.get_parameter_i32(glow::SHADER_STORAGE_BUFFER_OFFSET_ALIGNMENT) as u32
        } else {
            256
        };
        let max_uniform_buffers_per_shader_stage =
            gl.get_parameter_i32(glow::MAX_VERTEX_UNIFORM_BLOCKS)
                .min(gl.get_parameter_i32(glow::MAX_FRAGMENT_UNIFORM_BLOCKS)) as u32;

        let max_compute_workgroups_per_dimension = gl
            .get_parameter_indexed_i32(glow::MAX_COMPUTE_WORK_GROUP_COUNT, 0)
            .min(gl.get_parameter_indexed_i32(glow::MAX_COMPUTE_WORK_GROUP_COUNT, 1))
            .min(gl.get_parameter_indexed_i32(glow::MAX_COMPUTE_WORK_GROUP_COUNT, 2))
            as u32;

        let limits = wgt::Limits {
            max_texture_dimension_1d: max_texture_size,
            max_texture_dimension_2d: max_texture_size,
            max_texture_dimension_3d: max_texture_3d_size,
            max_texture_array_layers: gl.get_parameter_i32(glow::MAX_ARRAY_TEXTURE_LAYERS) as u32,
            max_bind_groups: crate::MAX_BIND_GROUPS as u32,
            max_dynamic_uniform_buffers_per_pipeline_layout: max_uniform_buffers_per_shader_stage,
            max_dynamic_storage_buffers_per_pipeline_layout: max_storage_buffers_per_shader_stage,
            max_sampled_textures_per_shader_stage: super::MAX_TEXTURE_SLOTS as u32,
            max_samplers_per_shader_stage: super::MAX_SAMPLERS as u32,
            max_storage_buffers_per_shader_stage,
            max_storage_textures_per_shader_stage,
            max_uniform_buffers_per_shader_stage,
            max_uniform_buffer_binding_size: gl.get_parameter_i32(glow::MAX_UNIFORM_BLOCK_SIZE)
                as u32,
            max_storage_buffer_binding_size: if ver >= (3, 1) {
                gl.get_parameter_i32(glow::MAX_SHADER_STORAGE_BLOCK_SIZE)
            } else {
                0
            } as u32,
            max_vertex_buffers: if private_caps
                .contains(super::PrivateCapabilities::VERTEX_BUFFER_LAYOUT)
            {
                gl.get_parameter_i32(glow::MAX_VERTEX_ATTRIB_BINDINGS) as u32
            } else {
                16 // should this be different?
            },
            max_vertex_attributes: (gl.get_parameter_i32(glow::MAX_VERTEX_ATTRIBS) as u32)
                .min(super::MAX_VERTEX_ATTRIBUTES as u32),
            max_vertex_buffer_array_stride: if private_caps
                .contains(super::PrivateCapabilities::VERTEX_BUFFER_LAYOUT)
            {
                gl.get_parameter_i32(glow::MAX_VERTEX_ATTRIB_STRIDE) as u32
            } else {
                !0
            },
            max_push_constant_size: 0,
            min_uniform_buffer_offset_alignment,
            min_storage_buffer_offset_alignment,
            max_compute_workgroup_size_x: gl
                .get_parameter_indexed_i32(glow::MAX_COMPUTE_WORK_GROUP_SIZE, 0)
                as u32,
            max_compute_workgroup_size_y: gl
                .get_parameter_indexed_i32(glow::MAX_COMPUTE_WORK_GROUP_SIZE, 1)
                as u32,
            max_compute_workgroup_size_z: gl
                .get_parameter_indexed_i32(glow::MAX_COMPUTE_WORK_GROUP_SIZE, 2)
                as u32,
            max_compute_workgroups_per_dimension,
        };

        let mut workarounds = super::Workarounds::empty();

        workarounds.set(
            super::Workarounds::EMULATE_BUFFER_MAP,
            cfg!(target_arch = "wasm32"),
        );

        let r = renderer.to_lowercase();
        // Check for Mesa sRGB clear bug. See
        // [`super::PrivateCapabilities::MESA_I915_SRGB_SHADER_CLEAR`].
        if r.contains("mesa")
            && r.contains("intel")
            && r.split(&[' ', '(', ')'][..])
                .any(|substr| substr.len() == 3 && substr.chars().nth(2) == Some('l'))
        {
            log::warn!(
                "Detected skylake derivative running on mesa i915. Clears to srgb textures will \
                use manual shader clears."
            );
            workarounds.set(super::Workarounds::MESA_I915_SRGB_SHADER_CLEAR, true);
        }

        let downlevel_defaults = wgt::DownlevelLimits {};

        // Drop the GL guard so we can move the context into AdapterShared
        // ( on WASM the gl handle is just a ref so we tell clippy to allow
        // dropping the ref )
        #[allow(clippy::drop_ref)]
        drop(gl);

        Some(crate::ExposedAdapter {
            adapter: super::Adapter {
                shared: Arc::new(super::AdapterShared {
                    context,
                    private_caps,
                    workarounds,
                    shading_language_version,
                }),
            },
            info: Self::make_info(vendor, renderer),
            features,
            capabilities: crate::Capabilities {
                limits,
                downlevel: wgt::DownlevelCapabilities {
                    flags: downlevel_flags,
                    limits: downlevel_defaults,
                    shader_model: wgt::ShaderModel::Sm5,
                },
                alignments: crate::Alignments {
                    buffer_copy_offset: wgt::BufferSize::new(4).unwrap(),
                    buffer_copy_pitch: wgt::BufferSize::new(4).unwrap(),
                },
            },
        })
    }

    unsafe fn create_shader_clear_program(
        gl: &glow::Context,
    ) -> (glow::Program, glow::UniformLocation) {
        let program = gl
            .create_program()
            .expect("Could not create shader program");
        let vertex = gl
            .create_shader(glow::VERTEX_SHADER)
            .expect("Could not create shader");
        gl.shader_source(vertex, include_str!("./shaders/clear.vert"));
        gl.compile_shader(vertex);
        let fragment = gl
            .create_shader(glow::FRAGMENT_SHADER)
            .expect("Could not create shader");
        gl.shader_source(fragment, include_str!("./shaders/clear.frag"));
        gl.compile_shader(fragment);
        gl.attach_shader(program, vertex);
        gl.attach_shader(program, fragment);
        gl.link_program(program);
        let color_uniform_location = gl
            .get_uniform_location(program, "color")
            .expect("Could not find color uniform in shader clear shader");
        gl.delete_shader(vertex);
        gl.delete_shader(fragment);

        (program, color_uniform_location)
    }
}

impl crate::Adapter<super::Api> for super::Adapter {
    unsafe fn open(
        &self,
        features: wgt::Features,
        _limits: &wgt::Limits,
    ) -> Result<crate::OpenDevice<super::Api>, crate::DeviceError> {
        let gl = &self.shared.context.lock();
        gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
        gl.pixel_store_i32(glow::PACK_ALIGNMENT, 1);
        let main_vao = gl
            .create_vertex_array()
            .map_err(|_| crate::DeviceError::OutOfMemory)?;
        gl.bind_vertex_array(Some(main_vao));

        let zero_buffer = gl
            .create_buffer()
            .map_err(|_| crate::DeviceError::OutOfMemory)?;
        gl.bind_buffer(glow::COPY_READ_BUFFER, Some(zero_buffer));
        let zeroes = vec![0u8; super::ZERO_BUFFER_SIZE];
        gl.buffer_data_u8_slice(glow::COPY_READ_BUFFER, &zeroes, glow::STATIC_DRAW);

        // Compile the shader program we use for doing manual clears to work around Mesa fastclear
        // bug.
        let (shader_clear_program, shader_clear_program_color_uniform_location) =
            Self::create_shader_clear_program(gl);

        Ok(crate::OpenDevice {
            device: super::Device {
                shared: Arc::clone(&self.shared),
                main_vao,
                #[cfg(feature = "renderdoc")]
                render_doc: Default::default(),
            },
            queue: super::Queue {
                shared: Arc::clone(&self.shared),
                features,
                draw_fbo: gl
                    .create_framebuffer()
                    .map_err(|_| crate::DeviceError::OutOfMemory)?,
                copy_fbo: gl
                    .create_framebuffer()
                    .map_err(|_| crate::DeviceError::OutOfMemory)?,
                shader_clear_program,
                shader_clear_program_color_uniform_location,
                zero_buffer,
                temp_query_results: Vec::new(),
                draw_buffer_count: 1,
                current_index_buffer: None,
            },
        })
    }

    unsafe fn texture_format_capabilities(
        &self,
        format: wgt::TextureFormat,
    ) -> crate::TextureFormatCapabilities {
        use crate::TextureFormatCapabilities as Tfc;
        use wgt::TextureFormat as Tf;
        // The storage types are sprinkled based on section
        // "TEXTURE IMAGE LOADS AND STORES" of GLES-3.2 spec.
        let unfiltered_color = Tfc::SAMPLED | Tfc::COLOR_ATTACHMENT;
        let filtered_color = unfiltered_color | Tfc::SAMPLED_LINEAR | Tfc::COLOR_ATTACHMENT_BLEND;
        match format {
            Tf::R8Unorm | Tf::R8Snorm => filtered_color,
            Tf::R8Uint | Tf::R8Sint | Tf::R16Uint | Tf::R16Sint => unfiltered_color,
            Tf::R16Float | Tf::Rg8Unorm | Tf::Rg8Snorm => filtered_color,
            Tf::Rg8Uint | Tf::Rg8Sint | Tf::R32Uint | Tf::R32Sint => {
                unfiltered_color | Tfc::STORAGE
            }
            Tf::R32Float => unfiltered_color,
            Tf::Rg16Uint | Tf::Rg16Sint => unfiltered_color,
            Tf::Rg16Float | Tf::Rgba8Unorm | Tf::Rgba8UnormSrgb => filtered_color | Tfc::STORAGE,
            Tf::Bgra8UnormSrgb | Tf::Rgba8Snorm | Tf::Bgra8Unorm => filtered_color,
            Tf::Rgba8Uint | Tf::Rgba8Sint => unfiltered_color | Tfc::STORAGE,
            Tf::Rgb10a2Unorm | Tf::Rg11b10Float => filtered_color,
            Tf::Rg32Uint | Tf::Rg32Sint => unfiltered_color,
            Tf::Rg32Float => unfiltered_color | Tfc::STORAGE,
            Tf::Rgba16Uint | Tf::Rgba16Sint => unfiltered_color | Tfc::STORAGE,
            Tf::Rgba16Float => filtered_color | Tfc::STORAGE,
            Tf::Rgba32Uint | Tf::Rgba32Sint => unfiltered_color | Tfc::STORAGE,
            Tf::Rgba32Float => unfiltered_color | Tfc::STORAGE,
            Tf::Depth32Float => Tfc::SAMPLED | Tfc::DEPTH_STENCIL_ATTACHMENT,
            Tf::Depth24Plus => Tfc::SAMPLED | Tfc::DEPTH_STENCIL_ATTACHMENT,
            Tf::Depth24PlusStencil8 => Tfc::SAMPLED | Tfc::DEPTH_STENCIL_ATTACHMENT,
            Tf::Rgb9e5Ufloat
            | Tf::Bc1RgbaUnorm
            | Tf::Bc1RgbaUnormSrgb
            | Tf::Bc2RgbaUnorm
            | Tf::Bc2RgbaUnormSrgb
            | Tf::Bc3RgbaUnorm
            | Tf::Bc3RgbaUnormSrgb
            | Tf::Bc4RUnorm
            | Tf::Bc4RSnorm
            | Tf::Bc5RgUnorm
            | Tf::Bc5RgSnorm
            | Tf::Bc6hRgbSfloat
            | Tf::Bc6hRgbUfloat
            | Tf::Bc7RgbaUnorm
            | Tf::Bc7RgbaUnormSrgb
            | Tf::Etc2Rgb8Unorm
            | Tf::Etc2Rgb8UnormSrgb
            | Tf::Etc2Rgb8A1Unorm
            | Tf::Etc2Rgb8A1UnormSrgb
            | Tf::Etc2Rgba8Unorm
            | Tf::Etc2Rgba8UnormSrgb
            | Tf::EacR11Unorm
            | Tf::EacR11Snorm
            | Tf::EacRg11Unorm
            | Tf::EacRg11Snorm
            | Tf::Astc4x4RgbaUnorm
            | Tf::Astc4x4RgbaUnormSrgb
            | Tf::Astc5x4RgbaUnorm
            | Tf::Astc5x4RgbaUnormSrgb
            | Tf::Astc5x5RgbaUnorm
            | Tf::Astc5x5RgbaUnormSrgb
            | Tf::Astc6x5RgbaUnorm
            | Tf::Astc6x5RgbaUnormSrgb
            | Tf::Astc6x6RgbaUnorm
            | Tf::Astc6x6RgbaUnormSrgb
            | Tf::Astc8x5RgbaUnorm
            | Tf::Astc8x5RgbaUnormSrgb
            | Tf::Astc8x6RgbaUnorm
            | Tf::Astc8x6RgbaUnormSrgb
            | Tf::Astc10x5RgbaUnorm
            | Tf::Astc10x5RgbaUnormSrgb
            | Tf::Astc10x6RgbaUnorm
            | Tf::Astc10x6RgbaUnormSrgb
            | Tf::Astc8x8RgbaUnorm
            | Tf::Astc8x8RgbaUnormSrgb
            | Tf::Astc10x8RgbaUnorm
            | Tf::Astc10x8RgbaUnormSrgb
            | Tf::Astc10x10RgbaUnorm
            | Tf::Astc10x10RgbaUnormSrgb
            | Tf::Astc12x10RgbaUnorm
            | Tf::Astc12x10RgbaUnormSrgb
            | Tf::Astc12x12RgbaUnorm
            | Tf::Astc12x12RgbaUnormSrgb => Tfc::SAMPLED | Tfc::SAMPLED_LINEAR,
        }
    }

    unsafe fn surface_capabilities(
        &self,
        surface: &super::Surface,
    ) -> Option<crate::SurfaceCapabilities> {
        if surface.presentable {
            Some(crate::SurfaceCapabilities {
                formats: if surface.supports_srgb() {
                    vec![
                        wgt::TextureFormat::Rgba8UnormSrgb,
                        #[cfg(not(target_arch = "wasm32"))]
                        wgt::TextureFormat::Bgra8UnormSrgb,
                    ]
                } else {
                    vec![
                        wgt::TextureFormat::Rgba8Unorm,
                        #[cfg(not(target_arch = "wasm32"))]
                        wgt::TextureFormat::Bgra8Unorm,
                    ]
                },
                present_modes: vec![wgt::PresentMode::Fifo], //TODO
                composite_alpha_modes: vec![crate::CompositeAlphaMode::Opaque], //TODO
                swap_chain_sizes: 2..=2,
                current_extent: None,
                extents: wgt::Extent3d {
                    width: 4,
                    height: 4,
                    depth_or_array_layers: 1,
                }..=wgt::Extent3d {
                    width: 4096,
                    height: 4096,
                    depth_or_array_layers: 1,
                },
                usage: crate::TextureUses::COLOR_TARGET,
            })
        } else {
            None
        }
    }
}

// SAFE: WASM doesn't have threads
#[cfg(target_arch = "wasm32")]
unsafe impl Sync for super::Adapter {}
#[cfg(target_arch = "wasm32")]
unsafe impl Send for super::Adapter {}

#[cfg(test)]
mod tests {
    use super::super::Adapter;

    #[test]
    fn test_version_parse() {
        let error = Err(crate::InstanceError);
        assert_eq!(Adapter::parse_version("1"), error);
        assert_eq!(Adapter::parse_version("1."), error);
        assert_eq!(Adapter::parse_version("1 h3l1o. W0rld"), error);
        assert_eq!(Adapter::parse_version("1. h3l1o. W0rld"), error);
        assert_eq!(Adapter::parse_version("1.2.3"), error);
        assert_eq!(Adapter::parse_version("OpenGL ES 3.1"), Ok((3, 1)));
        assert_eq!(
            Adapter::parse_version("OpenGL ES 2.0 Google Nexus"),
            Ok((2, 0))
        );
        assert_eq!(Adapter::parse_version("GLSL ES 1.1"), Ok((1, 1)));
        assert_eq!(Adapter::parse_version("OpenGL ES GLSL ES 3.20"), Ok((3, 2)));
        assert_eq!(
            // WebGL 2.0 should parse as OpenGL ES 3.0
            Adapter::parse_version("WebGL 2.0 (OpenGL ES 3.0 Chromium)"),
            Ok((3, 0))
        );
        assert_eq!(
            Adapter::parse_version("WebGL GLSL ES 3.00 (OpenGL ES GLSL ES 3.0 Chromium)"),
            Ok((3, 0))
        );
    }
}
