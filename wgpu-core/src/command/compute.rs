use crate::{
    binding_model::{BindError, BindGroup, PushConstantUploadError},
    command::{
        bind::Binder,
        end_pipeline_statistics_query,
        memory_init::{fixup_discarded_surfaces, SurfacesInDiscardState},
        BasePass, BasePassRef, CommandBuffer, CommandEncoderError, CommandEncoderStatus,
        MapPassErr, PassErrorScope, QueryUseError, StateChange,
    },
    device::MissingDownlevelFlags,
    error::{ErrorFormatter, PrettyError},
    hub::{Global, GlobalIdentityHandlerFactory, HalApi, Storage, Token},
    id,
    init_tracker::MemoryInitKind,
    resource::{Buffer, Texture},
    track::{StatefulTrackerSubset, TrackerSet, UsageConflict, UseExtendError},
    validation::{check_buffer_usage, MissingBufferUsageError},
    Label,
};

use hal::CommandEncoder as _;
use thiserror::Error;

use std::{fmt, mem, str};

#[doc(hidden)]
#[derive(Clone, Copy, Debug)]
#[cfg_attr(
    any(feature = "serial-pass", feature = "trace"),
    derive(serde::Serialize)
)]
#[cfg_attr(
    any(feature = "serial-pass", feature = "replay"),
    derive(serde::Deserialize)
)]
pub enum ComputeCommand {
    SetBindGroup {
        index: u8,
        num_dynamic_offsets: u8,
        bind_group_id: id::BindGroupId,
    },
    SetPipeline(id::ComputePipelineId),
    SetPushConstant {
        offset: u32,
        size_bytes: u32,
        values_offset: u32,
    },
    Dispatch([u32; 3]),
    DispatchIndirect {
        buffer_id: id::BufferId,
        offset: wgt::BufferAddress,
    },
    PushDebugGroup {
        color: u32,
        len: usize,
    },
    PopDebugGroup,
    InsertDebugMarker {
        color: u32,
        len: usize,
    },
    WriteTimestamp {
        query_set_id: id::QuerySetId,
        query_index: u32,
    },
    BeginPipelineStatisticsQuery {
        query_set_id: id::QuerySetId,
        query_index: u32,
    },
    EndPipelineStatisticsQuery,
}

#[cfg_attr(feature = "serial-pass", derive(serde::Deserialize, serde::Serialize))]
pub struct ComputePass {
    base: BasePass<ComputeCommand>,
    parent_id: id::CommandEncoderId,
}

impl ComputePass {
    pub fn new(parent_id: id::CommandEncoderId, desc: &ComputePassDescriptor) -> Self {
        Self {
            base: BasePass::new(&desc.label),
            parent_id,
        }
    }

    pub fn parent_id(&self) -> id::CommandEncoderId {
        self.parent_id
    }

    #[cfg(feature = "trace")]
    pub fn into_command(self) -> crate::device::trace::Command {
        crate::device::trace::Command::RunComputePass { base: self.base }
    }
}

impl fmt::Debug for ComputePass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "ComputePass {{ encoder_id: {:?}, data: {:?} commands and {:?} dynamic offsets }}",
            self.parent_id,
            self.base.commands.len(),
            self.base.dynamic_offsets.len()
        )
    }
}

#[derive(Clone, Debug, Default)]
pub struct ComputePassDescriptor<'a> {
    pub label: Label<'a>,
}

#[derive(Clone, Debug, Error, PartialEq)]
pub enum DispatchError {
    #[error("compute pipeline must be set")]
    MissingPipeline,
    #[error("current compute pipeline has a layout which is incompatible with a currently set bind group, first differing at entry index {index}")]
    IncompatibleBindGroup {
        index: u32,
        //expected: BindGroupLayoutId,
        //provided: Option<(BindGroupLayoutId, BindGroupId)>,
    },
    #[error(
        "each current dispatch group size dimension ({current:?}) must be less or equal to {limit}"
    )]
    InvalidGroupSize { current: [u32; 3], limit: u32 },
}

/// Error encountered when performing a compute pass.
#[derive(Clone, Debug, Error)]
pub enum ComputePassErrorInner {
    #[error(transparent)]
    Encoder(#[from] CommandEncoderError),
    #[error("bind group {0:?} is invalid")]
    InvalidBindGroup(id::BindGroupId),
    #[error("bind group index {index} is greater than the device's requested `max_bind_group` limit {max}")]
    BindGroupIndexOutOfRange { index: u8, max: u32 },
    #[error("compute pipeline {0:?} is invalid")]
    InvalidPipeline(id::ComputePipelineId),
    #[error("QuerySet {0:?} is invalid")]
    InvalidQuerySet(id::QuerySetId),
    #[error("indirect buffer {0:?} is invalid or destroyed")]
    InvalidIndirectBuffer(id::BufferId),
    #[error("indirect buffer uses bytes {offset}..{end_offset} which overruns indirect buffer of size {buffer_size}")]
    IndirectBufferOverrun {
        offset: u64,
        end_offset: u64,
        buffer_size: u64,
    },
    #[error("buffer {0:?} is invalid or destroyed")]
    InvalidBuffer(id::BufferId),
    #[error(transparent)]
    ResourceUsageConflict(#[from] UsageConflict),
    #[error(transparent)]
    MissingBufferUsage(#[from] MissingBufferUsageError),
    #[error("cannot pop debug group, because number of pushed debug groups is zero")]
    InvalidPopDebugGroup,
    #[error(transparent)]
    Dispatch(#[from] DispatchError),
    #[error(transparent)]
    Bind(#[from] BindError),
    #[error(transparent)]
    PushConstants(#[from] PushConstantUploadError),
    #[error(transparent)]
    QueryUse(#[from] QueryUseError),
    #[error(transparent)]
    MissingDownlevelFlags(#[from] MissingDownlevelFlags),
}

impl PrettyError for ComputePassErrorInner {
    fn fmt_pretty(&self, fmt: &mut ErrorFormatter) {
        fmt.error(self);
        match *self {
            Self::InvalidBindGroup(id) => {
                fmt.bind_group_label(&id);
            }
            Self::InvalidPipeline(id) => {
                fmt.compute_pipeline_label(&id);
            }
            Self::InvalidIndirectBuffer(id) => {
                fmt.buffer_label(&id);
            }
            _ => {}
        };
    }
}

/// Error encountered when performing a compute pass.
#[derive(Clone, Debug, Error)]
#[error("{scope}")]
pub struct ComputePassError {
    pub scope: PassErrorScope,
    #[source]
    inner: ComputePassErrorInner,
}
impl PrettyError for ComputePassError {
    fn fmt_pretty(&self, fmt: &mut ErrorFormatter) {
        // This error is wrapper for the inner error,
        // but the scope has useful labels
        fmt.error(self);
        self.scope.fmt_pretty(fmt);
    }
}

impl<T, E> MapPassErr<T, ComputePassError> for Result<T, E>
where
    E: Into<ComputePassErrorInner>,
{
    fn map_pass_err(self, scope: PassErrorScope) -> Result<T, ComputePassError> {
        self.map_err(|inner| ComputePassError {
            scope,
            inner: inner.into(),
        })
    }
}

#[derive(Debug)]
struct State {
    binder: Binder,
    pipeline: StateChange<id::ComputePipelineId>,
    trackers: StatefulTrackerSubset,
    debug_scope_depth: u32,
}

impl State {
    fn is_ready(&self) -> Result<(), DispatchError> {
        //TODO: vertex buffers
        let bind_mask = self.binder.invalid_mask();
        if bind_mask != 0 {
            //let (expected, provided) = self.binder.entries[index as usize].info();
            return Err(DispatchError::IncompatibleBindGroup {
                index: bind_mask.trailing_zeros(),
            });
        }
        if self.pipeline.is_unset() {
            return Err(DispatchError::MissingPipeline);
        }
        Ok(())
    }

    fn flush_states<A: HalApi>(
        &mut self,
        raw_encoder: &mut A::CommandEncoder,
        base_trackers: &mut TrackerSet,
        bind_group_guard: &Storage<BindGroup<A>, id::BindGroupId>,
        buffer_guard: &Storage<Buffer<A>, id::BufferId>,
        texture_guard: &Storage<Texture<A>, id::TextureId>,
    ) -> Result<(), UsageConflict> {
        for id in self.binder.list_active() {
            self.trackers.merge_extend(&bind_group_guard[id].used)?;
            //Note: stateless trackers are not merged: the lifetime reference
            // is held to the bind group itself.
        }

        log::trace!("Encoding dispatch barriers");

        CommandBuffer::insert_barriers(
            raw_encoder,
            base_trackers,
            &self.trackers.buffers,
            &self.trackers.textures,
            buffer_guard,
            texture_guard,
        );

        self.trackers.clear();
        Ok(())
    }
}

// Common routines between render/compute

impl<G: GlobalIdentityHandlerFactory> Global<G> {
    pub fn command_encoder_run_compute_pass<A: HalApi>(
        &self,
        encoder_id: id::CommandEncoderId,
        pass: &ComputePass,
    ) -> Result<(), ComputePassError> {
        self.command_encoder_run_compute_pass_impl::<A>(encoder_id, pass.base.as_ref())
    }

    #[doc(hidden)]
    pub fn command_encoder_run_compute_pass_impl<A: HalApi>(
        &self,
        encoder_id: id::CommandEncoderId,
        base: BasePassRef<ComputeCommand>,
    ) -> Result<(), ComputePassError> {
        profiling::scope!("run_compute_pass", "CommandEncoder");
        let scope = PassErrorScope::Pass(encoder_id);

        let hub = A::hub(self);
        let mut token = Token::root();

        let (device_guard, mut token) = hub.devices.read(&mut token);

        let (mut cmd_buf_guard, mut token) = hub.command_buffers.write(&mut token);
        let cmd_buf =
            CommandBuffer::get_encoder_mut(&mut *cmd_buf_guard, encoder_id).map_pass_err(scope)?;
        // will be reset to true if recording is done without errors
        cmd_buf.status = CommandEncoderStatus::Error;
        let raw = cmd_buf.encoder.open();

        let device = &device_guard[cmd_buf.device_id.value];

        #[cfg(feature = "trace")]
        if let Some(ref mut list) = cmd_buf.commands {
            list.push(crate::device::trace::Command::RunComputePass {
                base: BasePass::from_ref(base),
            });
        }

        let (_, mut token) = hub.render_bundles.read(&mut token);
        let (pipeline_layout_guard, mut token) = hub.pipeline_layouts.read(&mut token);
        let (bind_group_guard, mut token) = hub.bind_groups.read(&mut token);
        let (pipeline_guard, mut token) = hub.compute_pipelines.read(&mut token);
        let (query_set_guard, mut token) = hub.query_sets.read(&mut token);
        let (buffer_guard, mut token) = hub.buffers.read(&mut token);
        let (texture_guard, _) = hub.textures.read(&mut token);

        let mut state = State {
            binder: Binder::new(),
            pipeline: StateChange::new(),
            trackers: StatefulTrackerSubset::new(A::VARIANT),
            debug_scope_depth: 0,
        };
        let mut temp_offsets = Vec::new();
        let mut dynamic_offset_count = 0;
        let mut string_offset = 0;
        let mut active_query = None;

        let hal_desc = hal::ComputePassDescriptor { label: base.label };
        unsafe {
            raw.begin_compute_pass(&hal_desc);
        }

        // Immediate texture inits required because of prior discards. Need to be inserted before texture reads.
        let mut pending_discard_init_fixups = SurfacesInDiscardState::new();

        for command in base.commands {
            match *command {
                ComputeCommand::SetBindGroup {
                    index,
                    num_dynamic_offsets,
                    bind_group_id,
                } => {
                    let scope = PassErrorScope::SetBindGroup(bind_group_id);

                    let max_bind_groups = cmd_buf.limits.max_bind_groups;
                    if (index as u32) >= max_bind_groups {
                        return Err(ComputePassErrorInner::BindGroupIndexOutOfRange {
                            index,
                            max: max_bind_groups,
                        })
                        .map_pass_err(scope);
                    }

                    temp_offsets.clear();
                    temp_offsets.extend_from_slice(
                        &base.dynamic_offsets[dynamic_offset_count
                            ..dynamic_offset_count + (num_dynamic_offsets as usize)],
                    );
                    dynamic_offset_count += num_dynamic_offsets as usize;

                    let bind_group = cmd_buf
                        .trackers
                        .bind_groups
                        .use_extend(&*bind_group_guard, bind_group_id, (), ())
                        .map_err(|_| ComputePassErrorInner::InvalidBindGroup(bind_group_id))
                        .map_pass_err(scope)?;
                    bind_group
                        .validate_dynamic_bindings(&temp_offsets, &cmd_buf.limits)
                        .map_pass_err(scope)?;

                    cmd_buf.buffer_memory_init_actions.extend(
                        bind_group.used_buffer_ranges.iter().filter_map(
                            |action| match buffer_guard.get(action.id) {
                                Ok(buffer) => buffer.initialization_status.check_action(action),
                                Err(_) => None,
                            },
                        ),
                    );

                    for action in bind_group.used_texture_ranges.iter() {
                        pending_discard_init_fixups.extend(
                            cmd_buf
                                .texture_memory_actions
                                .register_init_action(action, &texture_guard),
                        );
                    }

                    let pipeline_layout_id = state.binder.pipeline_layout_id;
                    let entries = state.binder.assign_group(
                        index as usize,
                        id::Valid(bind_group_id),
                        bind_group,
                        &temp_offsets,
                    );
                    if !entries.is_empty() {
                        let pipeline_layout =
                            &pipeline_layout_guard[pipeline_layout_id.unwrap()].raw;
                        for (i, e) in entries.iter().enumerate() {
                            let raw_bg = &bind_group_guard[e.group_id.as_ref().unwrap().value].raw;
                            unsafe {
                                raw.set_bind_group(
                                    pipeline_layout,
                                    index as u32 + i as u32,
                                    raw_bg,
                                    &e.dynamic_offsets,
                                );
                            }
                        }
                    }
                }
                ComputeCommand::SetPipeline(pipeline_id) => {
                    let scope = PassErrorScope::SetPipelineCompute(pipeline_id);

                    if state.pipeline.set_and_check_redundant(pipeline_id) {
                        continue;
                    }

                    let pipeline = cmd_buf
                        .trackers
                        .compute_pipes
                        .use_extend(&*pipeline_guard, pipeline_id, (), ())
                        .map_err(|_| ComputePassErrorInner::InvalidPipeline(pipeline_id))
                        .map_pass_err(scope)?;

                    unsafe {
                        raw.set_compute_pipeline(&pipeline.raw);
                    }

                    // Rebind resources
                    if state.binder.pipeline_layout_id != Some(pipeline.layout_id.value) {
                        let pipeline_layout = &pipeline_layout_guard[pipeline.layout_id.value];

                        let (start_index, entries) = state.binder.change_pipeline_layout(
                            &*pipeline_layout_guard,
                            pipeline.layout_id.value,
                        );
                        if !entries.is_empty() {
                            for (i, e) in entries.iter().enumerate() {
                                let raw_bg =
                                    &bind_group_guard[e.group_id.as_ref().unwrap().value].raw;
                                unsafe {
                                    raw.set_bind_group(
                                        &pipeline_layout.raw,
                                        start_index as u32 + i as u32,
                                        raw_bg,
                                        &e.dynamic_offsets,
                                    );
                                }
                            }
                        }

                        // Clear push constant ranges
                        let non_overlapping = super::bind::compute_nonoverlapping_ranges(
                            &pipeline_layout.push_constant_ranges,
                        );
                        for range in non_overlapping {
                            let offset = range.range.start;
                            let size_bytes = range.range.end - offset;
                            super::push_constant_clear(
                                offset,
                                size_bytes,
                                |clear_offset, clear_data| unsafe {
                                    raw.set_push_constants(
                                        &pipeline_layout.raw,
                                        wgt::ShaderStages::COMPUTE,
                                        clear_offset,
                                        clear_data,
                                    );
                                },
                            );
                        }
                    }
                }
                ComputeCommand::SetPushConstant {
                    offset,
                    size_bytes,
                    values_offset,
                } => {
                    let scope = PassErrorScope::SetPushConstant;

                    let end_offset_bytes = offset + size_bytes;
                    let values_end_offset =
                        (values_offset + size_bytes / wgt::PUSH_CONSTANT_ALIGNMENT) as usize;
                    let data_slice =
                        &base.push_constant_data[(values_offset as usize)..values_end_offset];

                    let pipeline_layout_id = state
                        .binder
                        .pipeline_layout_id
                        //TODO: don't error here, lazily update the push constants
                        .ok_or(ComputePassErrorInner::Dispatch(
                            DispatchError::MissingPipeline,
                        ))
                        .map_pass_err(scope)?;
                    let pipeline_layout = &pipeline_layout_guard[pipeline_layout_id];

                    pipeline_layout
                        .validate_push_constant_ranges(
                            wgt::ShaderStages::COMPUTE,
                            offset,
                            end_offset_bytes,
                        )
                        .map_pass_err(scope)?;

                    unsafe {
                        raw.set_push_constants(
                            &pipeline_layout.raw,
                            wgt::ShaderStages::COMPUTE,
                            offset,
                            data_slice,
                        );
                    }
                }
                ComputeCommand::Dispatch(groups) => {
                    let scope = PassErrorScope::Dispatch {
                        indirect: false,
                        pipeline: state.pipeline.last_state,
                    };

                    fixup_discarded_surfaces(
                        pending_discard_init_fixups.drain(..),
                        raw,
                        &texture_guard,
                        &mut cmd_buf.trackers.textures,
                        device,
                    );

                    state.is_ready().map_pass_err(scope)?;
                    state
                        .flush_states(
                            raw,
                            &mut cmd_buf.trackers,
                            &*bind_group_guard,
                            &*buffer_guard,
                            &*texture_guard,
                        )
                        .map_pass_err(scope)?;

                    let groups_size_limit = cmd_buf.limits.max_compute_workgroups_per_dimension;

                    if groups[0] > groups_size_limit
                        || groups[1] > groups_size_limit
                        || groups[2] > groups_size_limit
                    {
                        return Err(ComputePassErrorInner::Dispatch(
                            DispatchError::InvalidGroupSize {
                                current: groups,
                                limit: groups_size_limit,
                            },
                        ))
                        .map_pass_err(scope);
                    }

                    unsafe {
                        raw.dispatch(groups);
                    }
                }
                ComputeCommand::DispatchIndirect { buffer_id, offset } => {
                    let scope = PassErrorScope::Dispatch {
                        indirect: true,
                        pipeline: state.pipeline.last_state,
                    };

                    state.is_ready().map_pass_err(scope)?;

                    device
                        .require_downlevel_flags(wgt::DownlevelFlags::INDIRECT_EXECUTION)
                        .map_pass_err(scope)?;

                    let indirect_buffer = state
                        .trackers
                        .buffers
                        .use_extend(&*buffer_guard, buffer_id, (), hal::BufferUses::INDIRECT)
                        .map_err(|_| ComputePassErrorInner::InvalidIndirectBuffer(buffer_id))
                        .map_pass_err(scope)?;
                    check_buffer_usage(indirect_buffer.usage, wgt::BufferUsages::INDIRECT)
                        .map_pass_err(scope)?;

                    let end_offset = offset + mem::size_of::<wgt::DispatchIndirectArgs>() as u64;
                    if end_offset > indirect_buffer.size {
                        return Err(ComputePassErrorInner::IndirectBufferOverrun {
                            offset,
                            end_offset,
                            buffer_size: indirect_buffer.size,
                        })
                        .map_pass_err(scope);
                    }

                    let buf_raw = indirect_buffer
                        .raw
                        .as_ref()
                        .ok_or(ComputePassErrorInner::InvalidIndirectBuffer(buffer_id))
                        .map_pass_err(scope)?;

                    let stride = 3 * 4; // 3 integers, x/y/z group size

                    cmd_buf.buffer_memory_init_actions.extend(
                        indirect_buffer.initialization_status.create_action(
                            buffer_id,
                            offset..(offset + stride),
                            MemoryInitKind::NeedsInitializedMemory,
                        ),
                    );

                    state
                        .flush_states(
                            raw,
                            &mut cmd_buf.trackers,
                            &*bind_group_guard,
                            &*buffer_guard,
                            &*texture_guard,
                        )
                        .map_pass_err(scope)?;
                    unsafe {
                        raw.dispatch_indirect(buf_raw, offset);
                    }
                }
                ComputeCommand::PushDebugGroup { color: _, len } => {
                    state.debug_scope_depth += 1;
                    let label =
                        str::from_utf8(&base.string_data[string_offset..string_offset + len])
                            .unwrap();
                    string_offset += len;
                    unsafe {
                        raw.begin_debug_marker(label);
                    }
                }
                ComputeCommand::PopDebugGroup => {
                    let scope = PassErrorScope::PopDebugGroup;

                    if state.debug_scope_depth == 0 {
                        return Err(ComputePassErrorInner::InvalidPopDebugGroup)
                            .map_pass_err(scope);
                    }
                    state.debug_scope_depth -= 1;
                    unsafe {
                        raw.end_debug_marker();
                    }
                }
                ComputeCommand::InsertDebugMarker { color: _, len } => {
                    let label =
                        str::from_utf8(&base.string_data[string_offset..string_offset + len])
                            .unwrap();
                    string_offset += len;
                    unsafe { raw.insert_debug_marker(label) }
                }
                ComputeCommand::WriteTimestamp {
                    query_set_id,
                    query_index,
                } => {
                    let scope = PassErrorScope::WriteTimestamp;

                    let query_set = cmd_buf
                        .trackers
                        .query_sets
                        .use_extend(&*query_set_guard, query_set_id, (), ())
                        .map_err(|e| match e {
                            UseExtendError::InvalidResource => {
                                ComputePassErrorInner::InvalidQuerySet(query_set_id)
                            }
                            _ => unreachable!(),
                        })
                        .map_pass_err(scope)?;

                    query_set
                        .validate_and_write_timestamp(raw, query_set_id, query_index, None)
                        .map_pass_err(scope)?;
                }
                ComputeCommand::BeginPipelineStatisticsQuery {
                    query_set_id,
                    query_index,
                } => {
                    let scope = PassErrorScope::BeginPipelineStatisticsQuery;

                    let query_set = cmd_buf
                        .trackers
                        .query_sets
                        .use_extend(&*query_set_guard, query_set_id, (), ())
                        .map_err(|e| match e {
                            UseExtendError::InvalidResource => {
                                ComputePassErrorInner::InvalidQuerySet(query_set_id)
                            }
                            _ => unreachable!(),
                        })
                        .map_pass_err(scope)?;

                    query_set
                        .validate_and_begin_pipeline_statistics_query(
                            raw,
                            query_set_id,
                            query_index,
                            None,
                            &mut active_query,
                        )
                        .map_pass_err(scope)?;
                }
                ComputeCommand::EndPipelineStatisticsQuery => {
                    let scope = PassErrorScope::EndPipelineStatisticsQuery;

                    end_pipeline_statistics_query(raw, &*query_set_guard, &mut active_query)
                        .map_pass_err(scope)?;
                }
            }
        }

        unsafe {
            raw.end_compute_pass();
        }
        cmd_buf.status = CommandEncoderStatus::Recording;

        // There can be entries left in pending_discard_init_fixups if a bind group was set, but not used (i.e. no Dispatch occurred)
        // However, we already altered the discard/init_action state on this cmd_buf, so we need to apply the promised changes.
        fixup_discarded_surfaces(
            pending_discard_init_fixups.into_iter(),
            raw,
            &texture_guard,
            &mut cmd_buf.trackers.textures,
            device,
        );

        Ok(())
    }
}

pub mod compute_ffi {
    use super::{ComputeCommand, ComputePass};
    use crate::{id, RawString};
    use std::{convert::TryInto, ffi, slice};
    use wgt::{BufferAddress, DynamicOffset};

    /// # Safety
    ///
    /// This function is unsafe as there is no guarantee that the given pointer is
    /// valid for `offset_length` elements.
    #[no_mangle]
    pub unsafe extern "C" fn wgpu_compute_pass_set_bind_group(
        pass: &mut ComputePass,
        index: u32,
        bind_group_id: id::BindGroupId,
        offsets: *const DynamicOffset,
        offset_length: usize,
    ) {
        pass.base.commands.push(ComputeCommand::SetBindGroup {
            index: index.try_into().unwrap(),
            num_dynamic_offsets: offset_length.try_into().unwrap(),
            bind_group_id,
        });
        if offset_length != 0 {
            pass.base
                .dynamic_offsets
                .extend_from_slice(slice::from_raw_parts(offsets, offset_length));
        }
    }

    #[no_mangle]
    pub extern "C" fn wgpu_compute_pass_set_pipeline(
        pass: &mut ComputePass,
        pipeline_id: id::ComputePipelineId,
    ) {
        pass.base
            .commands
            .push(ComputeCommand::SetPipeline(pipeline_id));
    }

    /// # Safety
    ///
    /// This function is unsafe as there is no guarantee that the given pointer is
    /// valid for `size_bytes` bytes.
    #[no_mangle]
    pub unsafe extern "C" fn wgpu_compute_pass_set_push_constant(
        pass: &mut ComputePass,
        offset: u32,
        size_bytes: u32,
        data: *const u8,
    ) {
        assert_eq!(
            offset & (wgt::PUSH_CONSTANT_ALIGNMENT - 1),
            0,
            "Push constant offset must be aligned to 4 bytes."
        );
        assert_eq!(
            size_bytes & (wgt::PUSH_CONSTANT_ALIGNMENT - 1),
            0,
            "Push constant size must be aligned to 4 bytes."
        );
        let data_slice = slice::from_raw_parts(data, size_bytes as usize);
        let value_offset = pass.base.push_constant_data.len().try_into().expect(
            "Ran out of push constant space. Don't set 4gb of push constants per ComputePass.",
        );

        pass.base.push_constant_data.extend(
            data_slice
                .chunks_exact(wgt::PUSH_CONSTANT_ALIGNMENT as usize)
                .map(|arr| u32::from_ne_bytes([arr[0], arr[1], arr[2], arr[3]])),
        );

        pass.base.commands.push(ComputeCommand::SetPushConstant {
            offset,
            size_bytes,
            values_offset: value_offset,
        });
    }

    #[no_mangle]
    pub extern "C" fn wgpu_compute_pass_dispatch(
        pass: &mut ComputePass,
        groups_x: u32,
        groups_y: u32,
        groups_z: u32,
    ) {
        pass.base
            .commands
            .push(ComputeCommand::Dispatch([groups_x, groups_y, groups_z]));
    }

    #[no_mangle]
    pub extern "C" fn wgpu_compute_pass_dispatch_indirect(
        pass: &mut ComputePass,
        buffer_id: id::BufferId,
        offset: BufferAddress,
    ) {
        pass.base
            .commands
            .push(ComputeCommand::DispatchIndirect { buffer_id, offset });
    }

    /// # Safety
    ///
    /// This function is unsafe as there is no guarantee that the given `label`
    /// is a valid null-terminated string.
    #[no_mangle]
    pub unsafe extern "C" fn wgpu_compute_pass_push_debug_group(
        pass: &mut ComputePass,
        label: RawString,
        color: u32,
    ) {
        let bytes = ffi::CStr::from_ptr(label).to_bytes();
        pass.base.string_data.extend_from_slice(bytes);

        pass.base.commands.push(ComputeCommand::PushDebugGroup {
            color,
            len: bytes.len(),
        });
    }

    #[no_mangle]
    pub extern "C" fn wgpu_compute_pass_pop_debug_group(pass: &mut ComputePass) {
        pass.base.commands.push(ComputeCommand::PopDebugGroup);
    }

    /// # Safety
    ///
    /// This function is unsafe as there is no guarantee that the given `label`
    /// is a valid null-terminated string.
    #[no_mangle]
    pub unsafe extern "C" fn wgpu_compute_pass_insert_debug_marker(
        pass: &mut ComputePass,
        label: RawString,
        color: u32,
    ) {
        let bytes = ffi::CStr::from_ptr(label).to_bytes();
        pass.base.string_data.extend_from_slice(bytes);

        pass.base.commands.push(ComputeCommand::InsertDebugMarker {
            color,
            len: bytes.len(),
        });
    }

    #[no_mangle]
    pub extern "C" fn wgpu_compute_pass_write_timestamp(
        pass: &mut ComputePass,
        query_set_id: id::QuerySetId,
        query_index: u32,
    ) {
        pass.base.commands.push(ComputeCommand::WriteTimestamp {
            query_set_id,
            query_index,
        });
    }

    #[no_mangle]
    pub extern "C" fn wgpu_compute_pass_begin_pipeline_statistics_query(
        pass: &mut ComputePass,
        query_set_id: id::QuerySetId,
        query_index: u32,
    ) {
        pass.base
            .commands
            .push(ComputeCommand::BeginPipelineStatisticsQuery {
                query_set_id,
                query_index,
            });
    }

    #[no_mangle]
    pub extern "C" fn wgpu_compute_pass_end_pipeline_statistics_query(pass: &mut ComputePass) {
        pass.base
            .commands
            .push(ComputeCommand::EndPipelineStatisticsQuery);
    }
}
