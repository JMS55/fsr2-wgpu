mod barrier;
mod fsr;

pub use crate::fsr::{
    Fsr2Exposure, Fsr2InitializationFlags, Fsr2QualityMode, Fsr2ReactiveMask, Fsr2Texture,
};

use crate::barrier::Barriers;
use crate::fsr::{
    ffxFsr2ContextCreate, ffxFsr2ContextDestroy, ffxFsr2ContextDispatch, ffxFsr2GetJitterOffset,
    ffxFsr2GetJitterPhaseCount, FfxDimensions2D, FfxFloatCoords2D, FfxFsr2Context,
    FfxFsr2ContextDescription, FfxFsr2DispatchDescription, FfxFsr2Interface, FfxResource,
    FfxResourceStates_FFX_RESOURCE_STATE_COMPUTE_READ,
};
use crate::fsr::{
    ffxFsr2GetInterfaceVK, ffxFsr2GetScratchMemorySizeVK, ffxGetCommandListVK, ffxGetDeviceVK,
    ffxGetTextureResourceVK,
};
use ash::vk::{Format, Image, ImageView};
use glam::{Mat4, UVec2, Vec2, Vec3};
use std::mem::MaybeUninit;
use std::ptr;
use std::time::Duration;
use wgpu::{Adapter, CommandEncoder, Device};
use wgpu_core::api::Vulkan;

// TODO: Documentation for the whole library
// TODO: Check FSR error codes
// TODO: Validate inputs

// TODO: Thread safety?
pub struct Fsr2Context {
    context: FfxFsr2Context,
    upscaled_resolution: UVec2,
    _scratch_memory: Vec<u8>,
}

impl Fsr2Context {
    pub fn new(
        device: &Device,
        max_input_resolution: UVec2,
        upscaled_resolution: UVec2,
        initialization_flags: Fsr2InitializationFlags,
    ) -> Self {
        unsafe {
            // Get underlying Vulkan objects from wgpu
            let (device, physical_device, get_device_proc_addr) =
                device.as_hal::<Vulkan, _, _>(|device| {
                    let device = device.unwrap();
                    let raw_device = device.raw_device().handle();
                    let physical_device = device.raw_physical_device();

                    let get_device_proc_addr = device
                        .shared_instance()
                        .raw_instance()
                        .fp_v1_0()
                        .get_device_proc_addr;

                    (raw_device, physical_device, get_device_proc_addr)
                });

            // Allocate scratch memory for FSR
            let scratch_memory_size = ffxFsr2GetScratchMemorySizeVK(physical_device);
            let mut scratch_memory = Vec::with_capacity(scratch_memory_size);

            // Setup an FSR->Vulkan interface
            let mut interface = MaybeUninit::<FfxFsr2Interface>::uninit();
            ffxFsr2GetInterfaceVK(
                interface.as_mut_ptr(),
                scratch_memory.as_mut_ptr() as *mut _,
                scratch_memory_size,
                physical_device,
                get_device_proc_addr,
            );
            let interface = interface.assume_init();

            // Create an FSR context
            let mut context = MaybeUninit::<FfxFsr2Context>::uninit();
            let context_description = FfxFsr2ContextDescription {
                flags: initialization_flags.bits(),
                maxRenderSize: uvec2_to_dim2d(max_input_resolution),
                displaySize: uvec2_to_dim2d(upscaled_resolution),
                callbacks: interface,
                device: ffxGetDeviceVK(device),
            };
            ffxFsr2ContextCreate(context.as_mut_ptr(), &context_description as *const _);
            let context = context.assume_init();

            Self {
                context,
                upscaled_resolution,
                _scratch_memory: scratch_memory,
            }
        }
    }

    pub fn set_new_upscale_resolution_if_changed(&mut self, new_upscaled_resolution: UVec2) {
        if new_upscaled_resolution != self.upscaled_resolution {
            todo!("Recreate context, destroy old one");
        }
    }

    pub fn get_suggested_input_resolution(&self, quality_mode: Fsr2QualityMode) -> UVec2 {
        let scale_factor = match quality_mode {
            Fsr2QualityMode::Quality => 1.5,
            Fsr2QualityMode::Balanced => 1.7,
            Fsr2QualityMode::Performance => 2.0,
            Fsr2QualityMode::UltraPerformance => 3.0,
        };

        (self.upscaled_resolution.as_vec2() / scale_factor).as_uvec2()
    }

    pub fn jitter_camera_projection_matrix(
        &self,
        matrix: &mut Mat4,
        input_resolution: UVec2,
        frame_index: i32,
    ) -> Vec2 {
        let jitter_offset = self.get_camera_jitter_offset(input_resolution, frame_index);

        let mut translation = 2.0 * jitter_offset / input_resolution.as_vec2();
        translation.y *= -1.0;

        let translation = Mat4::from_translation(Vec3 {
            x: translation.x,
            y: translation.y,
            z: 0.0,
        });
        *matrix = translation * *matrix;

        jitter_offset
    }

    pub fn get_camera_jitter_offset(&self, input_resolution: UVec2, frame_index: i32) -> Vec2 {
        unsafe {
            let phase_count = ffxFsr2GetJitterPhaseCount(
                input_resolution.x.try_into().unwrap(),
                self.upscaled_resolution.x.try_into().unwrap(),
            );

            let mut jitter_offset = Vec2::ZERO;
            ffxFsr2GetJitterOffset(
                &mut jitter_offset.x as *mut _,
                &mut jitter_offset.y as *mut _,
                frame_index,
                phase_count,
            );
            jitter_offset
        }
    }

    pub fn get_mip_bias(&self, input_resolution: UVec2) -> f32 {
        (input_resolution.x as f32 / self.upscaled_resolution.x as f32).log2() - 1.0
    }

    pub fn render(&mut self, parameters: Fsr2RenderParameters) {
        let mut barriers = Barriers::default();

        let (exposure, pre_exposure) = match parameters.exposure {
            Fsr2Exposure::AutoExposure => (None, 0.0),
            Fsr2Exposure::ManualExposure {
                pre_exposure,
                exposure,
            } => (Some(exposure), pre_exposure),
        };

        unsafe {
            let device = parameters
                .device
                .as_hal::<Vulkan, _, _>(|x| x.unwrap().raw_device());

            let command_buffer = parameters
                .command_encoder
                .as_hal_mut::<Vulkan, _, _>(|x| x.unwrap().open().raw_handle());

            let reactive = match parameters.reactive_mask {
                Fsr2ReactiveMask::NoMask => {
                    self.input_texture_to_ffx_resource(None, &mut barriers, parameters.adapter)
                }
                Fsr2ReactiveMask::ManualMask(mask) => self.input_texture_to_ffx_resource(
                    Some(mask),
                    &mut barriers,
                    parameters.adapter,
                ),
                #[allow(unused_variables)]
                Fsr2ReactiveMask::AutoMask {
                    color_opaque_only,
                    color_opaque_and_transparent,
                    scale,
                    threshold,
                    binary_value,
                    flags,
                } => {
                    todo!()
                }
            };

            let dispatch_description = FfxFsr2DispatchDescription {
                commandList: ffxGetCommandListVK(command_buffer),
                color: self.input_texture_to_ffx_resource(
                    Some(parameters.color),
                    &mut barriers,
                    parameters.adapter,
                ),
                depth: self.input_texture_to_ffx_resource(
                    Some(parameters.depth),
                    &mut barriers,
                    parameters.adapter,
                ),
                motionVectors: self.input_texture_to_ffx_resource(
                    Some(parameters.motion_vectors),
                    &mut barriers,
                    parameters.adapter,
                ),
                exposure: self.input_texture_to_ffx_resource(
                    exposure,
                    &mut barriers,
                    parameters.adapter,
                ),
                reactive,
                transparencyAndComposition: self.input_texture_to_ffx_resource(
                    parameters.transparency_and_composition_mask,
                    &mut barriers,
                    parameters.adapter,
                ),
                output: self.input_texture_to_ffx_resource(
                    Some(parameters.output),
                    &mut barriers,
                    parameters.adapter,
                ),
                jitterOffset: vec2_to_float_coords2d(parameters.jitter_offset),
                motionVectorScale: vec2_to_float_coords2d(
                    parameters.motion_vector_scale.unwrap_or(Vec2::ONE),
                ),
                renderSize: uvec2_to_dim2d(parameters.input_resolution),
                enableSharpening: parameters.sharpness > 0.0,
                sharpness: parameters.sharpness.clamp(0.0, 1.0),
                frameTimeDelta: parameters.frame_delta_time.as_millis() as f32,
                preExposure: pre_exposure,
                reset: parameters.reset,
                cameraNear: parameters.camera_near,
                cameraFar: parameters.camera_far.unwrap_or(0.0),
                cameraFovAngleVertical: parameters.camera_fov_angle_vertical,
            };

            barriers.cmd_start(command_buffer, &device);
            ffxFsr2ContextDispatch(
                &mut self.context as *mut _,
                &dispatch_description as *const _,
            );
            barriers.cmd_end(command_buffer, &device);

            parameters
                .command_encoder
                .as_hal_mut::<Vulkan, _, _>(|x| x.unwrap().close());
        }
    }

    unsafe fn input_texture_to_ffx_resource(
        &mut self,
        texture: Option<Fsr2Texture>,
        barriers: &mut Barriers,
        adapter: &Adapter,
    ) -> FfxResource {
        if let Some(Fsr2Texture { texture, view }) = texture {
            let image = texture.as_hal::<Vulkan, _, _>(|x| x.unwrap().raw_handle());
            let view = view.as_hal::<Vulkan, _, _>(|x| x.unwrap().raw_handle());

            barriers.add(image);

            ffxGetTextureResourceVK(
                &mut self.context as *mut _,
                image,
                view,
                texture.width(),
                texture.height(),
                adapter
                    .texture_format_as_hal::<Vulkan>(texture.format())
                    .unwrap(),
                ptr::null_mut(),
                FfxResourceStates_FFX_RESOURCE_STATE_COMPUTE_READ,
            )
        } else {
            ffxGetTextureResourceVK(
                &mut self.context as *mut _,
                Image::null(),
                ImageView::null(),
                1,
                1,
                Format::UNDEFINED,
                ptr::null_mut(),
                FfxResourceStates_FFX_RESOURCE_STATE_COMPUTE_READ,
            )
        }
    }
}

impl Drop for Fsr2Context {
    fn drop(&mut self) {
        unsafe {
            // TODO: Wait for FSR resources to not be in use somehow
            ffxFsr2ContextDestroy(&mut self.context as *mut _);
        }
    }
}

pub struct Fsr2RenderParameters<'a> {
    pub color: Fsr2Texture<'a>,
    pub depth: Fsr2Texture<'a>,
    pub motion_vectors: Fsr2Texture<'a>,
    pub motion_vector_scale: Option<Vec2>,
    pub exposure: Fsr2Exposure<'a>,
    pub reactive_mask: Fsr2ReactiveMask<'a>,
    pub transparency_and_composition_mask: Option<Fsr2Texture<'a>>,
    pub output: Fsr2Texture<'a>,
    pub input_resolution: UVec2,
    pub sharpness: f32,
    pub frame_delta_time: Duration,
    pub reset: bool,
    pub camera_near: f32,
    pub camera_far: Option<f32>,
    pub camera_fov_angle_vertical: f32,
    pub jitter_offset: Vec2,
    pub adapter: &'a Adapter,
    pub device: &'a Device,
    pub command_encoder: &'a mut CommandEncoder,
}

fn uvec2_to_dim2d(vec: UVec2) -> FfxDimensions2D {
    FfxDimensions2D {
        width: vec.x,
        height: vec.y,
    }
}

fn vec2_to_float_coords2d(vec: Vec2) -> FfxFloatCoords2D {
    FfxFloatCoords2D { x: vec.x, y: vec.y }
}
