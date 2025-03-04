use super::Command as C;
use arrayvec::ArrayVec;
use glow::HasContext;
use std::{mem, slice, sync::Arc};

#[cfg(not(target_arch = "wasm32"))]
const DEBUG_ID: u32 = 0;

const CUBEMAP_FACES: [u32; 6] = [
    glow::TEXTURE_CUBE_MAP_POSITIVE_X,
    glow::TEXTURE_CUBE_MAP_NEGATIVE_X,
    glow::TEXTURE_CUBE_MAP_POSITIVE_Y,
    glow::TEXTURE_CUBE_MAP_NEGATIVE_Y,
    glow::TEXTURE_CUBE_MAP_POSITIVE_Z,
    glow::TEXTURE_CUBE_MAP_NEGATIVE_Z,
];

#[cfg(not(target_arch = "wasm32"))]
fn extract_marker<'a>(data: &'a [u8], range: &std::ops::Range<u32>) -> &'a str {
    std::str::from_utf8(&data[range.start as usize..range.end as usize]).unwrap()
}

fn is_layered_target(target: super::BindTarget) -> bool {
    match target {
        glow::TEXTURE_2D_ARRAY | glow::TEXTURE_3D | glow::TEXTURE_CUBE_MAP_ARRAY => true,
        _ => false,
    }
}

impl super::Queue {
    /// Performs a manual shader clear, used as a workaround for a clearing bug on mesa
    unsafe fn perform_shader_clear(&self, gl: &glow::Context, draw_buffer: u32, color: [f32; 4]) {
        gl.use_program(Some(self.shader_clear_program));
        gl.uniform_4_f32(
            Some(&self.shader_clear_program_color_uniform_location),
            color[0],
            color[1],
            color[2],
            color[3],
        );
        gl.disable(glow::DEPTH_TEST);
        gl.disable(glow::STENCIL_TEST);
        gl.disable(glow::SCISSOR_TEST);
        gl.disable(glow::BLEND);
        gl.disable(glow::CULL_FACE);
        gl.draw_buffers(&[glow::COLOR_ATTACHMENT0 + draw_buffer]);
        gl.draw_arrays(glow::TRIANGLES, 0, 3);

        // Reset the draw buffers to what they were before the clear
        let indices = (0..self.draw_buffer_count as u32)
            .map(|i| glow::COLOR_ATTACHMENT0 + i)
            .collect::<ArrayVec<_, { crate::MAX_COLOR_TARGETS }>>();
        gl.draw_buffers(&indices);
        #[cfg(not(target_arch = "wasm32"))]
        for draw_buffer in 0..self.draw_buffer_count as u32 {
            gl.disable_draw_buffer(glow::BLEND, draw_buffer);
        }
    }

    unsafe fn reset_state(&self, gl: &glow::Context) {
        gl.use_program(None);
        gl.bind_framebuffer(glow::FRAMEBUFFER, None);
        gl.disable(glow::DEPTH_TEST);
        gl.disable(glow::STENCIL_TEST);
        gl.disable(glow::SCISSOR_TEST);
        gl.disable(glow::BLEND);
        gl.disable(glow::CULL_FACE);
        gl.disable(glow::POLYGON_OFFSET_FILL);
        if self.features.contains(wgt::Features::DEPTH_CLAMPING) {
            gl.disable(glow::DEPTH_CLAMP);
        }
    }

    unsafe fn set_attachment(
        &self,
        gl: &glow::Context,
        fbo_target: u32,
        attachment: u32,
        view: &super::TextureView,
    ) {
        match view.inner {
            super::TextureInner::Renderbuffer { raw } => {
                gl.framebuffer_renderbuffer(fbo_target, attachment, glow::RENDERBUFFER, Some(raw));
            }
            super::TextureInner::Texture { raw, target } => {
                if is_layered_target(target) {
                    gl.framebuffer_texture_layer(
                        fbo_target,
                        attachment,
                        Some(raw),
                        view.mip_levels.start as i32,
                        view.array_layers.start as i32,
                    );
                } else if target == glow::TEXTURE_CUBE_MAP {
                    gl.framebuffer_texture_2d(
                        fbo_target,
                        attachment,
                        CUBEMAP_FACES[view.array_layers.start as usize],
                        Some(raw),
                        view.mip_levels.start as i32,
                    );
                } else {
                    gl.framebuffer_texture_2d(
                        fbo_target,
                        attachment,
                        target,
                        Some(raw),
                        view.mip_levels.start as i32,
                    );
                }
            }
        }
    }

    unsafe fn process(
        &mut self,
        gl: &glow::Context,
        command: &C,
        #[cfg_attr(target_arch = "wasm32", allow(unused))] data_bytes: &[u8],
        queries: &[glow::Query],
    ) {
        match *command {
            C::Draw {
                topology,
                start_vertex,
                vertex_count,
                instance_count,
            } => {
                if instance_count == 1 {
                    gl.draw_arrays(topology, start_vertex as i32, vertex_count as i32);
                } else {
                    gl.draw_arrays_instanced(
                        topology,
                        start_vertex as i32,
                        vertex_count as i32,
                        instance_count as i32,
                    );
                }
            }
            C::DrawIndexed {
                topology,
                index_type,
                index_count,
                index_offset,
                base_vertex,
                instance_count,
            } => match (base_vertex, instance_count) {
                (0, 1) => gl.draw_elements(
                    topology,
                    index_count as i32,
                    index_type,
                    index_offset as i32,
                ),
                (0, _) => gl.draw_elements_instanced(
                    topology,
                    index_count as i32,
                    index_type,
                    index_offset as i32,
                    instance_count as i32,
                ),
                (_, 1) => gl.draw_elements_base_vertex(
                    topology,
                    index_count as i32,
                    index_type,
                    index_offset as i32,
                    base_vertex,
                ),
                (_, _) => gl.draw_elements_instanced_base_vertex(
                    topology,
                    index_count as _,
                    index_type,
                    index_offset as i32,
                    instance_count as i32,
                    base_vertex,
                ),
            },
            C::DrawIndirect {
                topology,
                indirect_buf,
                indirect_offset,
            } => {
                gl.bind_buffer(glow::DRAW_INDIRECT_BUFFER, Some(indirect_buf));
                gl.draw_arrays_indirect_offset(topology, indirect_offset as i32);
            }
            C::DrawIndexedIndirect {
                topology,
                index_type,
                indirect_buf,
                indirect_offset,
            } => {
                gl.bind_buffer(glow::DRAW_INDIRECT_BUFFER, Some(indirect_buf));
                gl.draw_elements_indirect_offset(topology, index_type, indirect_offset as i32);
            }
            C::Dispatch(group_counts) => {
                gl.dispatch_compute(group_counts[0], group_counts[1], group_counts[2]);
            }
            C::DispatchIndirect {
                indirect_buf,
                indirect_offset,
            } => {
                gl.bind_buffer(glow::DISPATCH_INDIRECT_BUFFER, Some(indirect_buf));
                gl.dispatch_compute_indirect(indirect_offset as i32);
            }
            C::ClearBuffer {
                ref dst,
                dst_target,
                ref range,
            } => match *dst {
                super::BufferInner::Buffer(buffer) => {
                    gl.bind_buffer(glow::COPY_READ_BUFFER, Some(self.zero_buffer));
                    gl.bind_buffer(dst_target, Some(buffer));
                    let mut dst_offset = range.start;
                    while dst_offset < range.end {
                        let size = (range.end - dst_offset).min(super::ZERO_BUFFER_SIZE as u64);
                        gl.copy_buffer_sub_data(
                            glow::COPY_READ_BUFFER,
                            dst_target,
                            0,
                            dst_offset as i32,
                            size as i32,
                        );
                        dst_offset += size;
                    }
                }
                super::BufferInner::Data(ref data) => {
                    data.lock().unwrap().as_mut_slice()[range.start as usize..range.end as usize]
                        .fill(0);
                }
            },
            C::CopyBufferToBuffer {
                ref src,
                src_target,
                ref dst,
                dst_target,
                copy,
            } => {
                let copy_src_target = glow::COPY_READ_BUFFER;
                let is_index_buffer_only_element_dst = !self
                    .shared
                    .private_caps
                    .contains(super::PrivateCapabilities::INDEX_BUFFER_ROLE_CHANGE)
                    && dst_target == glow::ELEMENT_ARRAY_BUFFER
                    || src_target == glow::ELEMENT_ARRAY_BUFFER;

                // WebGL not allowed to copy data from other targets to element buffer and can't copy element data to other buffers
                let copy_dst_target = if is_index_buffer_only_element_dst {
                    glow::ELEMENT_ARRAY_BUFFER
                } else {
                    glow::COPY_WRITE_BUFFER
                };
                let size = copy.size.get() as usize;
                match (src, dst) {
                    (
                        &super::BufferInner::Buffer(ref src),
                        &super::BufferInner::Buffer(ref dst),
                    ) => {
                        gl.bind_buffer(copy_src_target, Some(*src));
                        gl.bind_buffer(copy_dst_target, Some(*dst));
                        gl.copy_buffer_sub_data(
                            copy_src_target,
                            copy_dst_target,
                            copy.src_offset as _,
                            copy.dst_offset as _,
                            copy.size.get() as _,
                        );
                    }
                    (&super::BufferInner::Buffer(src), &super::BufferInner::Data(ref data)) => {
                        let mut data = data.lock().unwrap();
                        let dst_data = &mut data.as_mut_slice()
                            [copy.dst_offset as usize..copy.dst_offset as usize + size];

                        gl.bind_buffer(copy_src_target, Some(src));
                        gl.get_buffer_sub_data(copy_src_target, copy.src_offset as i32, dst_data);
                    }
                    (&super::BufferInner::Data(ref data), &super::BufferInner::Buffer(dst)) => {
                        let data = data.lock().unwrap();
                        let src_data = &data.as_slice()
                            [copy.src_offset as usize..copy.src_offset as usize + size];
                        gl.bind_buffer(copy_dst_target, Some(dst));
                        gl.buffer_sub_data_u8_slice(
                            copy_dst_target,
                            copy.dst_offset as i32,
                            src_data,
                        );
                    }
                    (&super::BufferInner::Data(_), &super::BufferInner::Data(_)) => {
                        todo!()
                    }
                }
                gl.bind_buffer(copy_src_target, None);
                if is_index_buffer_only_element_dst {
                    gl.bind_buffer(glow::ELEMENT_ARRAY_BUFFER, self.current_index_buffer);
                } else {
                    gl.bind_buffer(copy_dst_target, None);
                }
            }
            C::CopyTextureToTexture {
                src,
                src_target,
                dst,
                dst_target,
                ref copy,
            } => {
                //TODO: handle 3D copies
                //TODO: handle cubemap copies
                gl.bind_framebuffer(glow::READ_FRAMEBUFFER, Some(self.copy_fbo));
                if is_layered_target(src_target) {
                    //TODO: handle GLES without framebuffer_texture_3d
                    gl.framebuffer_texture_layer(
                        glow::READ_FRAMEBUFFER,
                        glow::COLOR_ATTACHMENT0,
                        Some(src),
                        copy.src_base.mip_level as i32,
                        copy.src_base.array_layer as i32,
                    );
                } else {
                    gl.framebuffer_texture_2d(
                        glow::READ_FRAMEBUFFER,
                        glow::COLOR_ATTACHMENT0,
                        src_target,
                        Some(src),
                        copy.src_base.mip_level as i32,
                    );
                }

                gl.bind_texture(dst_target, Some(dst));
                if is_layered_target(dst_target) {
                    gl.copy_tex_sub_image_3d(
                        dst_target,
                        copy.dst_base.mip_level as i32,
                        copy.dst_base.origin.x as i32,
                        copy.dst_base.origin.y as i32,
                        copy.dst_base.origin.z as i32,
                        copy.src_base.origin.x as i32,
                        copy.src_base.origin.y as i32,
                        copy.size.width as i32,
                        copy.size.height as i32,
                    );
                } else {
                    gl.copy_tex_sub_image_2d(
                        dst_target,
                        copy.dst_base.mip_level as i32,
                        copy.dst_base.origin.x as i32,
                        copy.dst_base.origin.y as i32,
                        copy.src_base.origin.x as i32,
                        copy.src_base.origin.y as i32,
                        copy.size.width as i32,
                        copy.size.height as i32,
                    );
                }
            }
            C::CopyBufferToTexture {
                ref src,
                src_target: _,
                dst,
                dst_target,
                dst_format,
                ref copy,
            } => {
                let format_info = dst_format.describe();
                let format_desc = self.shared.describe_texture_format(dst_format);
                let row_texels = copy.buffer_layout.bytes_per_row.map_or(0, |bpr| {
                    format_info.block_dimensions.0 as u32 * bpr.get()
                        / format_info.block_size as u32
                });
                let column_texels = copy
                    .buffer_layout
                    .rows_per_image
                    .map_or(0, |rpi| format_info.block_dimensions.1 as u32 * rpi.get());

                gl.bind_texture(dst_target, Some(dst));
                gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, row_texels as i32);
                gl.pixel_store_i32(glow::UNPACK_IMAGE_HEIGHT, column_texels as i32);
                if format_info.block_dimensions == (1, 1) {
                    let buffer_data;
                    let unpack_data = match *src {
                        super::BufferInner::Buffer(buffer) => {
                            gl.bind_buffer(glow::PIXEL_UNPACK_BUFFER, Some(buffer));
                            glow::PixelUnpackData::BufferOffset(copy.buffer_layout.offset as u32)
                        }
                        super::BufferInner::Data(ref data) => {
                            buffer_data = data.lock().unwrap();
                            let src_data =
                                &buffer_data.as_slice()[copy.buffer_layout.offset as usize..];
                            glow::PixelUnpackData::Slice(src_data)
                        }
                    };
                    match dst_target {
                        glow::TEXTURE_3D | glow::TEXTURE_2D_ARRAY => {
                            gl.tex_sub_image_3d(
                                dst_target,
                                copy.texture_base.mip_level as i32,
                                copy.texture_base.origin.x as i32,
                                copy.texture_base.origin.y as i32,
                                copy.texture_base.origin.z as i32,
                                copy.size.width as i32,
                                copy.size.height as i32,
                                copy.size.depth as i32,
                                format_desc.external,
                                format_desc.data_type,
                                unpack_data,
                            );
                        }
                        glow::TEXTURE_2D => {
                            gl.tex_sub_image_2d(
                                dst_target,
                                copy.texture_base.mip_level as i32,
                                copy.texture_base.origin.x as i32,
                                copy.texture_base.origin.y as i32,
                                copy.size.width as i32,
                                copy.size.height as i32,
                                format_desc.external,
                                format_desc.data_type,
                                unpack_data,
                            );
                        }
                        glow::TEXTURE_CUBE_MAP => {
                            gl.tex_sub_image_2d(
                                CUBEMAP_FACES[copy.texture_base.array_layer as usize],
                                copy.texture_base.mip_level as i32,
                                copy.texture_base.origin.x as i32,
                                copy.texture_base.origin.y as i32,
                                copy.size.width as i32,
                                copy.size.height as i32,
                                format_desc.external,
                                format_desc.data_type,
                                unpack_data,
                            );
                        }
                        glow::TEXTURE_CUBE_MAP_ARRAY => {
                            //Note: not sure if this is correct!
                            gl.tex_sub_image_3d(
                                dst_target,
                                copy.texture_base.mip_level as i32,
                                copy.texture_base.origin.x as i32,
                                copy.texture_base.origin.y as i32,
                                copy.texture_base.origin.z as i32,
                                copy.size.width as i32,
                                copy.size.height as i32,
                                copy.size.depth as i32,
                                format_desc.external,
                                format_desc.data_type,
                                unpack_data,
                            );
                        }
                        _ => unreachable!(),
                    }
                } else {
                    let bytes_per_image =
                        copy.buffer_layout.rows_per_image.map_or(1, |rpi| rpi.get())
                            * copy.buffer_layout.bytes_per_row.map_or(1, |bpr| bpr.get());
                    let offset = copy.buffer_layout.offset as u32;

                    let buffer_data;
                    let unpack_data = match *src {
                        super::BufferInner::Buffer(buffer) => {
                            gl.bind_buffer(glow::PIXEL_UNPACK_BUFFER, Some(buffer));
                            glow::CompressedPixelUnpackData::BufferRange(
                                offset..offset + bytes_per_image,
                            )
                        }
                        super::BufferInner::Data(ref data) => {
                            buffer_data = data.lock().unwrap();
                            let src_data = &buffer_data.as_slice()
                                [(offset as usize)..(offset + bytes_per_image) as usize];
                            glow::CompressedPixelUnpackData::Slice(src_data)
                        }
                    };
                    match dst_target {
                        glow::TEXTURE_3D | glow::TEXTURE_2D_ARRAY => {
                            gl.compressed_tex_sub_image_3d(
                                dst_target,
                                copy.texture_base.mip_level as i32,
                                copy.texture_base.origin.x as i32,
                                copy.texture_base.origin.y as i32,
                                copy.texture_base.origin.z as i32,
                                copy.size.width as i32,
                                copy.size.height as i32,
                                copy.size.depth as i32,
                                format_desc.internal,
                                unpack_data,
                            );
                        }
                        glow::TEXTURE_2D => {
                            gl.compressed_tex_sub_image_2d(
                                dst_target,
                                copy.texture_base.mip_level as i32,
                                copy.texture_base.origin.x as i32,
                                copy.texture_base.origin.y as i32,
                                copy.size.width as i32,
                                copy.size.height as i32,
                                format_desc.internal,
                                unpack_data,
                            );
                        }
                        glow::TEXTURE_CUBE_MAP => {
                            gl.compressed_tex_sub_image_2d(
                                CUBEMAP_FACES[copy.texture_base.array_layer as usize],
                                copy.texture_base.mip_level as i32,
                                copy.texture_base.origin.x as i32,
                                copy.texture_base.origin.y as i32,
                                copy.size.width as i32,
                                copy.size.height as i32,
                                format_desc.internal,
                                unpack_data,
                            );
                        }
                        glow::TEXTURE_CUBE_MAP_ARRAY => {
                            //Note: not sure if this is correct!
                            gl.compressed_tex_sub_image_3d(
                                dst_target,
                                copy.texture_base.mip_level as i32,
                                copy.texture_base.origin.x as i32,
                                copy.texture_base.origin.y as i32,
                                copy.texture_base.origin.z as i32,
                                copy.size.width as i32,
                                copy.size.height as i32,
                                copy.size.depth as i32,
                                format_desc.internal,
                                unpack_data,
                            );
                        }
                        _ => unreachable!(),
                    }
                }
            }
            C::CopyTextureToBuffer {
                src,
                src_target,
                src_format,
                ref dst,
                dst_target: _,
                ref copy,
            } => {
                let format_info = src_format.describe();
                if format_info.block_dimensions != (1, 1) {
                    log::error!("Not implemented yet: compressed texture copy to buffer");
                    return;
                }
                if src_target == glow::TEXTURE_CUBE_MAP
                    || src_target == glow::TEXTURE_CUBE_MAP_ARRAY
                {
                    log::error!("Not implemented yet: cubemap texture copy to buffer");
                    return;
                }
                let format_desc = self.shared.describe_texture_format(src_format);
                let row_texels = copy
                    .buffer_layout
                    .bytes_per_row
                    .map_or(copy.size.width, |bpr| {
                        bpr.get() / format_info.block_size as u32
                    });

                gl.bind_framebuffer(glow::READ_FRAMEBUFFER, Some(self.copy_fbo));
                //TODO: handle cubemap copies
                if is_layered_target(src_target) {
                    //TODO: handle GLES without framebuffer_texture_3d
                    gl.framebuffer_texture_layer(
                        glow::READ_FRAMEBUFFER,
                        glow::COLOR_ATTACHMENT0,
                        Some(src),
                        copy.texture_base.mip_level as i32,
                        copy.texture_base.array_layer as i32,
                    );
                } else {
                    gl.framebuffer_texture_2d(
                        glow::READ_FRAMEBUFFER,
                        glow::COLOR_ATTACHMENT0,
                        src_target,
                        Some(src),
                        copy.texture_base.mip_level as i32,
                    );
                }
                let mut buffer_data;
                let unpack_data = match *dst {
                    super::BufferInner::Buffer(buffer) => {
                        gl.pixel_store_i32(glow::PACK_ROW_LENGTH, row_texels as i32);
                        gl.bind_buffer(glow::PIXEL_PACK_BUFFER, Some(buffer));
                        glow::PixelPackData::BufferOffset(copy.buffer_layout.offset as u32)
                    }
                    super::BufferInner::Data(ref data) => {
                        buffer_data = data.lock().unwrap();
                        let dst_data =
                            &mut buffer_data.as_mut_slice()[copy.buffer_layout.offset as usize..];
                        glow::PixelPackData::Slice(dst_data)
                    }
                };
                gl.read_pixels(
                    copy.texture_base.origin.x as i32,
                    copy.texture_base.origin.y as i32,
                    copy.size.width as i32,
                    copy.size.height as i32,
                    format_desc.external,
                    format_desc.data_type,
                    unpack_data,
                );
            }
            C::SetIndexBuffer(buffer) => {
                gl.bind_buffer(glow::ELEMENT_ARRAY_BUFFER, Some(buffer));
                self.current_index_buffer = Some(buffer);
            }
            C::BeginQuery(query, target) => {
                gl.begin_query(target, query);
            }
            C::EndQuery(target) => {
                gl.end_query(target);
            }
            C::CopyQueryResults {
                ref query_range,
                ref dst,
                dst_target,
                dst_offset,
            } => {
                self.temp_query_results.clear();
                for &query in queries[query_range.start as usize..query_range.end as usize].iter() {
                    let result = gl.get_query_parameter_u32(query, glow::QUERY_RESULT);
                    self.temp_query_results.push(result as u64);
                }
                let query_data = slice::from_raw_parts(
                    self.temp_query_results.as_ptr() as *const u8,
                    self.temp_query_results.len() * mem::size_of::<u64>(),
                );
                match *dst {
                    super::BufferInner::Buffer(buffer) => {
                        gl.bind_buffer(dst_target, Some(buffer));
                        gl.buffer_sub_data_u8_slice(dst_target, dst_offset as i32, query_data);
                    }
                    super::BufferInner::Data(ref data) => {
                        let data = &mut *data.lock().unwrap();
                        let len = query_data.len().min(data.len());
                        data[..len].copy_from_slice(&query_data[..len]);
                    }
                }
            }
            C::ResetFramebuffer => {
                gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, Some(self.draw_fbo));
                gl.framebuffer_texture_2d(
                    glow::DRAW_FRAMEBUFFER,
                    glow::DEPTH_STENCIL_ATTACHMENT,
                    glow::TEXTURE_2D,
                    None,
                    0,
                );
                for i in 0..crate::MAX_COLOR_TARGETS {
                    let target = glow::COLOR_ATTACHMENT0 + i as u32;
                    gl.framebuffer_texture_2d(
                        glow::DRAW_FRAMEBUFFER,
                        target,
                        glow::TEXTURE_2D,
                        None,
                        0,
                    );
                }
                gl.color_mask(true, true, true, true);
                gl.depth_mask(true);
                gl.stencil_mask(!0);
                gl.disable(glow::DEPTH_TEST);
                gl.disable(glow::STENCIL_TEST);
                gl.disable(glow::SCISSOR_TEST);
            }
            C::BindAttachment {
                attachment,
                ref view,
            } => {
                self.set_attachment(gl, glow::DRAW_FRAMEBUFFER, attachment, view);
            }
            C::ResolveAttachment {
                attachment,
                ref dst,
                ref size,
            } => {
                gl.bind_framebuffer(glow::READ_FRAMEBUFFER, Some(self.draw_fbo));
                gl.read_buffer(attachment);
                gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, Some(self.copy_fbo));
                self.set_attachment(gl, glow::DRAW_FRAMEBUFFER, glow::COLOR_ATTACHMENT0, dst);
                gl.blit_framebuffer(
                    0,
                    0,
                    size.width as i32,
                    size.height as i32,
                    0,
                    0,
                    size.width as i32,
                    size.height as i32,
                    glow::COLOR_BUFFER_BIT,
                    glow::NEAREST,
                );
                gl.bind_framebuffer(glow::READ_FRAMEBUFFER, None);
                gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, Some(self.draw_fbo));
            }
            C::InvalidateAttachments(ref list) => {
                gl.invalidate_framebuffer(glow::DRAW_FRAMEBUFFER, list);
            }
            C::SetDrawColorBuffers(count) => {
                self.draw_buffer_count = count;
                let indices = (0..count as u32)
                    .map(|i| glow::COLOR_ATTACHMENT0 + i)
                    .collect::<ArrayVec<_, { crate::MAX_COLOR_TARGETS }>>();
                gl.draw_buffers(&indices);

                if self
                    .shared
                    .private_caps
                    .contains(super::PrivateCapabilities::CAN_DISABLE_DRAW_BUFFER)
                {
                    for draw_buffer in 0..count as u32 {
                        gl.disable_draw_buffer(glow::BLEND, draw_buffer);
                    }
                }
            }
            C::ClearColorF {
                draw_buffer,
                ref color,
                is_srgb,
            } => {
                if self
                    .shared
                    .workarounds
                    .contains(super::Workarounds::MESA_I915_SRGB_SHADER_CLEAR)
                    && is_srgb
                {
                    self.perform_shader_clear(gl, draw_buffer, *color);
                } else {
                    gl.clear_buffer_f32_slice(glow::COLOR, draw_buffer, color);
                }
            }
            C::ClearColorU(draw_buffer, ref color) => {
                gl.clear_buffer_u32_slice(glow::COLOR, draw_buffer, color);
            }
            C::ClearColorI(draw_buffer, ref color) => {
                gl.clear_buffer_i32_slice(glow::COLOR, draw_buffer, color);
            }
            C::ClearDepth(depth) => {
                gl.clear_buffer_f32_slice(glow::DEPTH, 0, &[depth]);
            }
            C::ClearStencil(value) => {
                gl.clear_buffer_i32_slice(glow::STENCIL, 0, &[value as i32]);
            }
            C::BufferBarrier(raw, usage) => {
                let mut flags = 0;
                if usage.contains(crate::BufferUses::VERTEX) {
                    flags |= glow::VERTEX_ATTRIB_ARRAY_BARRIER_BIT;
                    gl.bind_buffer(glow::ARRAY_BUFFER, Some(raw));
                    gl.vertex_attrib_pointer_f32(0, 1, glow::BYTE, true, 0, 0);
                }
                if usage.contains(crate::BufferUses::INDEX) {
                    flags |= glow::ELEMENT_ARRAY_BARRIER_BIT;
                    gl.bind_buffer(glow::ELEMENT_ARRAY_BUFFER, Some(raw));
                }
                if usage.contains(crate::BufferUses::UNIFORM) {
                    flags |= glow::UNIFORM_BARRIER_BIT;
                }
                if usage.contains(crate::BufferUses::INDIRECT) {
                    flags |= glow::COMMAND_BARRIER_BIT;
                    gl.bind_buffer(glow::DRAW_INDIRECT_BUFFER, Some(raw));
                }
                if usage.contains(crate::BufferUses::COPY_SRC) {
                    flags |= glow::PIXEL_BUFFER_BARRIER_BIT;
                    gl.bind_buffer(glow::PIXEL_UNPACK_BUFFER, Some(raw));
                }
                if usage.contains(crate::BufferUses::COPY_DST) {
                    flags |= glow::PIXEL_BUFFER_BARRIER_BIT;
                    gl.bind_buffer(glow::PIXEL_PACK_BUFFER, Some(raw));
                }
                if usage.intersects(crate::BufferUses::MAP_READ | crate::BufferUses::MAP_WRITE) {
                    flags |= glow::BUFFER_UPDATE_BARRIER_BIT;
                }
                if usage
                    .intersects(crate::BufferUses::STORAGE_READ | crate::BufferUses::STORAGE_WRITE)
                {
                    flags |= glow::SHADER_STORAGE_BARRIER_BIT;
                }
                gl.memory_barrier(flags);
            }
            C::TextureBarrier(usage) => {
                let mut flags = 0;
                if usage.contains(crate::TextureUses::RESOURCE) {
                    flags |= glow::TEXTURE_FETCH_BARRIER_BIT;
                }
                if usage.intersects(
                    crate::TextureUses::STORAGE_READ | crate::TextureUses::STORAGE_WRITE,
                ) {
                    flags |= glow::SHADER_IMAGE_ACCESS_BARRIER_BIT;
                }
                if usage.contains(crate::TextureUses::COPY_DST) {
                    flags |= glow::TEXTURE_UPDATE_BARRIER_BIT;
                }
                if usage.intersects(
                    crate::TextureUses::COLOR_TARGET
                        | crate::TextureUses::DEPTH_STENCIL_READ
                        | crate::TextureUses::DEPTH_STENCIL_WRITE,
                ) {
                    flags |= glow::FRAMEBUFFER_BARRIER_BIT;
                }
                gl.memory_barrier(flags);
            }
            C::SetViewport {
                ref rect,
                ref depth,
            } => {
                gl.viewport(rect.x, rect.y, rect.w, rect.h);
                gl.depth_range_f32(depth.start, depth.end);
            }
            C::SetScissor(ref rect) => {
                gl.scissor(rect.x, rect.y, rect.w, rect.h);
                gl.enable(glow::SCISSOR_TEST);
            }
            C::SetStencilFunc {
                face,
                function,
                reference,
                read_mask,
            } => {
                gl.stencil_func_separate(face, function, reference as i32, read_mask);
            }
            C::SetStencilOps {
                face,
                write_mask,
                ref ops,
            } => {
                gl.stencil_mask_separate(face, write_mask);
                gl.stencil_op_separate(face, ops.fail, ops.depth_fail, ops.pass);
            }
            C::SetVertexAttribute {
                buffer,
                ref buffer_desc,
                attribute_desc: ref vat,
            } => {
                gl.bind_buffer(glow::ARRAY_BUFFER, buffer);
                gl.enable_vertex_attrib_array(vat.location);

                if buffer.is_none() {
                    match vat.format_desc.attrib_kind {
                        super::VertexAttribKind::Float => gl.vertex_attrib_format_f32(
                            vat.location,
                            vat.format_desc.element_count,
                            vat.format_desc.element_format,
                            true, // always normalized
                            vat.offset,
                        ),
                        super::VertexAttribKind::Integer => gl.vertex_attrib_format_i32(
                            vat.location,
                            vat.format_desc.element_count,
                            vat.format_desc.element_format,
                            vat.offset,
                        ),
                    }

                    //Note: there is apparently a bug on AMD 3500U:
                    // this call is ignored if the current array is disabled.
                    gl.vertex_attrib_binding(vat.location, vat.buffer_index);
                } else {
                    match vat.format_desc.attrib_kind {
                        super::VertexAttribKind::Float => gl.vertex_attrib_pointer_f32(
                            vat.location,
                            vat.format_desc.element_count,
                            vat.format_desc.element_format,
                            true, // always normalized
                            buffer_desc.stride as i32,
                            vat.offset as i32,
                        ),
                        super::VertexAttribKind::Integer => gl.vertex_attrib_pointer_i32(
                            vat.location,
                            vat.format_desc.element_count,
                            vat.format_desc.element_format,
                            buffer_desc.stride as i32,
                            vat.offset as i32,
                        ),
                    }
                    gl.vertex_attrib_divisor(vat.location, buffer_desc.step as u32);
                }
            }
            C::UnsetVertexAttribute(location) => {
                gl.disable_vertex_attrib_array(location);
            }
            C::SetVertexBuffer {
                index,
                ref buffer,
                ref buffer_desc,
            } => {
                gl.vertex_binding_divisor(index, buffer_desc.step as u32);
                gl.bind_vertex_buffer(
                    index,
                    Some(buffer.raw),
                    buffer.offset as i32,
                    buffer_desc.stride as i32,
                );
            }
            C::SetDepth(ref depth) => {
                gl.depth_func(depth.function);
                gl.depth_mask(depth.mask);
            }
            C::SetDepthBias(bias) => {
                if bias.is_enabled() {
                    gl.enable(glow::POLYGON_OFFSET_FILL);
                    gl.polygon_offset(bias.constant as f32, bias.slope_scale);
                } else {
                    gl.disable(glow::POLYGON_OFFSET_FILL);
                }
            }
            C::ConfigureDepthStencil(aspects) => {
                if aspects.contains(crate::FormatAspects::DEPTH) {
                    gl.enable(glow::DEPTH_TEST);
                } else {
                    gl.disable(glow::DEPTH_TEST);
                }
                if aspects.contains(crate::FormatAspects::STENCIL) {
                    gl.enable(glow::STENCIL_TEST);
                } else {
                    gl.disable(glow::STENCIL_TEST);
                }
            }
            C::SetProgram(program) => {
                gl.use_program(Some(program));
            }
            C::SetPrimitive(ref state) => {
                gl.front_face(state.front_face);
                if state.cull_face != 0 {
                    gl.enable(glow::CULL_FACE);
                    gl.cull_face(state.cull_face);
                } else {
                    gl.disable(glow::CULL_FACE);
                }
                if self.features.contains(wgt::Features::DEPTH_CLAMPING) {
                    if state.clamp_depth {
                        gl.enable(glow::DEPTH_CLAMP);
                    } else {
                        gl.disable(glow::DEPTH_CLAMP);
                    }
                }
            }
            C::SetBlendConstant(c) => {
                gl.blend_color(c[0], c[1], c[2], c[3]);
            }
            C::SetColorTarget {
                draw_buffer_index,
                desc: super::ColorTargetDesc { mask, ref blend },
            } => {
                use wgt::ColorWrites as Cw;
                if let Some(index) = draw_buffer_index {
                    gl.color_mask_draw_buffer(
                        index,
                        mask.contains(Cw::RED),
                        mask.contains(Cw::GREEN),
                        mask.contains(Cw::BLUE),
                        mask.contains(Cw::ALPHA),
                    );
                    if let Some(ref blend) = *blend {
                        gl.enable_draw_buffer(index, glow::BLEND);
                        if blend.color != blend.alpha {
                            gl.blend_equation_separate_draw_buffer(
                                index,
                                blend.color.equation,
                                blend.alpha.equation,
                            );
                            gl.blend_func_separate_draw_buffer(
                                index,
                                blend.color.src,
                                blend.color.dst,
                                blend.alpha.src,
                                blend.alpha.dst,
                            );
                        } else {
                            gl.blend_equation_draw_buffer(index, blend.color.equation);
                            gl.blend_func_draw_buffer(index, blend.color.src, blend.color.dst);
                        }
                    } else if self
                        .shared
                        .private_caps
                        .contains(super::PrivateCapabilities::CAN_DISABLE_DRAW_BUFFER)
                    {
                        gl.disable_draw_buffer(index, glow::BLEND);
                    }
                } else {
                    gl.color_mask(
                        mask.contains(Cw::RED),
                        mask.contains(Cw::GREEN),
                        mask.contains(Cw::BLUE),
                        mask.contains(Cw::ALPHA),
                    );
                    if let Some(ref blend) = *blend {
                        gl.enable(glow::BLEND);
                        if blend.color != blend.alpha {
                            gl.blend_equation_separate(blend.color.equation, blend.alpha.equation);
                            gl.blend_func_separate(
                                blend.color.src,
                                blend.color.dst,
                                blend.alpha.src,
                                blend.alpha.dst,
                            );
                        } else {
                            gl.blend_equation(blend.color.equation);
                            gl.blend_func(blend.color.src, blend.color.dst);
                        }
                    } else {
                        gl.disable(glow::BLEND);
                    }
                }
            }
            C::BindBuffer {
                target,
                slot,
                buffer,
                offset,
                size,
            } => {
                gl.bind_buffer_range(target, slot, Some(buffer), offset, size);
            }
            C::BindSampler(texture_index, sampler) => {
                gl.bind_sampler(texture_index, Some(sampler));
            }
            C::BindTexture {
                slot,
                texture,
                target,
            } => {
                gl.active_texture(glow::TEXTURE0 + slot);
                gl.bind_texture(target, Some(texture));
            }
            C::BindImage { slot, ref binding } => {
                gl.bind_image_texture(
                    slot,
                    binding.raw,
                    binding.mip_level as i32,
                    binding.array_layer.is_none(),
                    binding.array_layer.unwrap_or_default() as i32,
                    binding.access,
                    binding.format,
                );
            }
            #[cfg(not(target_arch = "wasm32"))]
            C::InsertDebugMarker(ref range) => {
                let marker = extract_marker(data_bytes, range);
                gl.debug_message_insert(
                    glow::DEBUG_SOURCE_APPLICATION,
                    glow::DEBUG_TYPE_MARKER,
                    DEBUG_ID,
                    glow::DEBUG_SEVERITY_NOTIFICATION,
                    marker,
                );
            }
            #[cfg(target_arch = "wasm32")]
            C::InsertDebugMarker(_) => (),
            #[cfg_attr(target_arch = "wasm32", allow(unused))]
            C::PushDebugGroup(ref range) => {
                #[cfg(not(target_arch = "wasm32"))]
                let marker = extract_marker(data_bytes, range);
                #[cfg(not(target_arch = "wasm32"))]
                gl.push_debug_group(glow::DEBUG_SOURCE_APPLICATION, DEBUG_ID, marker);
            }
            C::PopDebugGroup => {
                #[cfg(not(target_arch = "wasm32"))]
                gl.pop_debug_group();
            }
        }
    }
}

impl crate::Queue<super::Api> for super::Queue {
    unsafe fn submit(
        &mut self,
        command_buffers: &[&super::CommandBuffer],
        signal_fence: Option<(&mut super::Fence, crate::FenceValue)>,
    ) -> Result<(), crate::DeviceError> {
        let shared = Arc::clone(&self.shared);
        let gl = &shared.context.lock();
        self.reset_state(gl);
        for cmd_buf in command_buffers.iter() {
            #[cfg(not(target_arch = "wasm32"))]
            if let Some(ref label) = cmd_buf.label {
                gl.push_debug_group(glow::DEBUG_SOURCE_APPLICATION, DEBUG_ID, label);
            }

            for command in cmd_buf.commands.iter() {
                self.process(gl, command, &cmd_buf.data_bytes, &cmd_buf.queries);
            }

            #[cfg(not(target_arch = "wasm32"))]
            if cmd_buf.label.is_some() {
                gl.pop_debug_group();
            }
        }

        if let Some((fence, value)) = signal_fence {
            fence.maintain(gl);
            let sync = gl
                .fence_sync(glow::SYNC_GPU_COMMANDS_COMPLETE, 0)
                .map_err(|_| crate::DeviceError::OutOfMemory)?;
            fence.pending.push((value, sync));
        }

        Ok(())
    }

    unsafe fn present(
        &mut self,
        surface: &mut super::Surface,
        texture: super::Texture,
    ) -> Result<(), crate::SurfaceError> {
        #[cfg(not(target_arch = "wasm32"))]
        let gl = &self.shared.context.get_without_egl_lock();

        #[cfg(target_arch = "wasm32")]
        let gl = &self.shared.context.glow_context;

        surface.present(texture, gl)
    }

    unsafe fn get_timestamp_period(&self) -> f32 {
        1.0
    }
}

// SAFE: WASM doesn't have threads
#[cfg(target_arch = "wasm32")]
unsafe impl Sync for super::Queue {}
#[cfg(target_arch = "wasm32")]
unsafe impl Send for super::Queue {}
