//! Safe abstractions around [crate::cublas::result] for doing gemm and gemv.
#![allow(clippy::too_many_arguments)]

use super::{result, result::CublasError, sys};
use crate::driver::CudaStream;
use std::sync::Arc;

mod asum;
mod gemm;
mod gemv;
mod grouped_gemm;

pub use asum::*;
pub use gemm::*;
pub use gemv::*;
pub use grouped_gemm::*;

/// Wrapper around [sys::cublasHandle_t]
///
/// 1. Create with [CudaBlas::new()]
/// 2. Execute gemm/gemv/gmm kernels with [Gemv], [Gemm] and [Gmm]. Both f32 and f64 are supported
///    for [Gemm] and [Gemv], f16 and bf16 are supported for [Gmm] if feature `half` is activated.
///
/// Note: This maintains a instance of [`Arc<CudaDevice>`], so will prevent the device
/// from being dropped.
#[derive(Debug)]
pub struct CudaBlas {
    pub(crate) handle: sys::cublasHandle_t,
    pub(crate) stream: Arc<CudaStream>,
    #[cfg(feature = "cuda-13000")]
    workspace: Option<crate::driver::CudaSlice<u8>>,
}

unsafe impl Send for CudaBlas {}
unsafe impl Sync for CudaBlas {}

impl CudaBlas {
    /// Creates a new cublas handle and sets the stream to the `device`'s stream.
    pub fn new(stream: Arc<CudaStream>) -> Result<Self, CublasError> {
        let ctx = stream.context();
        ctx.record_err(ctx.bind_to_thread());
        let handle = result::create_handle()?;
        let mut blas = Self { handle, stream, #[cfg(feature = "cuda-13000")] workspace: None };
        unsafe { result::set_stream(blas.handle, blas.stream.cu_stream() as _) }?;
        #[cfg(feature = "cuda-13000")]
        if crate::driver::quota::managed_pool_enabled() {
            // Explicit 32 MiB workspace is charged to the private pool. This is a
            // candidate setting requiring parity/throughput measurement, not promotion.
            let workspace = unsafe { blas.stream.alloc::<u8>(32 * 1024 * 1024) }
                .map_err(|_| CublasError(sys::cublasStatus_t::CUBLAS_STATUS_ALLOC_FAILED))?;
            blas.workspace = Some(workspace);
            unsafe { blas.restore_managed_workspace()?; }
        }
        Ok(blas)
    }

    /// Returns a reference to the underlying cublas handle.
    pub fn handle(&self) -> &sys::cublasHandle_t {
        &self.handle
    }

    /// Sets the handle's current to either the stream specified, or the device's default work
    /// stream.
    ///
    /// # Safety
    /// This is unsafe because you can end up scheduling multiple concurrent kernels that all
    /// write to the same memory address.
    pub unsafe fn set_stream(&mut self, stream: Arc<CudaStream>) -> Result<(), CublasError> {
        #[cfg(feature = "cuda-13000")]
        if self.workspace.is_some() { self.stream.synchronize().map_err(|_| CublasError(sys::cublasStatus_t::CUBLAS_STATUS_EXECUTION_FAILED))?; }
        self.stream = stream;
        unsafe { result::set_stream(self.handle, self.stream.cu_stream() as _) }?;
        #[cfg(feature = "cuda-13000")]
        self.restore_managed_workspace()?;
        Ok(())
    }

    #[cfg(feature = "cuda-13000")]
    unsafe fn restore_managed_workspace(&self) -> Result<(), CublasError> {
        if let Some(workspace) = &self.workspace {
            sys::cublasSetWorkspace_v2(self.handle, workspace.cu_device_ptr as _, workspace.len).result()?;
        }
        Ok(())
    }

    /// Set the handle's pointer mode.
    /// ref: <https://docs.nvidia.com/cuda/cublas/#cublassetpointermode>
    ///
    /// Some cublas functions require the pointer mode to be set to `cublasPointerMode_t::CUBLAS_POINTER_MODE_DEVICE`
    /// when passing a device memory result buffer into the function, such as `cublas<t>asum()`.
    /// Otherwise the operation will panic with `SIGSEGV: invalid memory reference`,
    /// or one has to use a host memory reference, which has performance implications.
    pub fn set_pointer_mode(
        &self,
        pointer_mode: sys::cublasPointerMode_t,
    ) -> Result<(), CublasError> {
        unsafe {
            sys::cublasSetPointerMode_v2(self.handle, pointer_mode).result()?;
        }
        Ok(())
    }

    /// Get the handle's current pointer mode.
    /// ref: <https://docs.nvidia.com/cuda/cublas/#cublasgetpointermode>
    pub fn get_pointer_mode(&self) -> Result<sys::cublasPointerMode_t, CublasError> {
        unsafe {
            let mut mode = ::core::mem::MaybeUninit::uninit();
            sys::cublasGetPointerMode_v2(self.handle, mode.as_mut_ptr()).result()?;
            Ok(mode.assume_init())
        }
    }
}

impl Drop for CudaBlas {
    fn drop(&mut self) {
        let handle = std::mem::replace(&mut self.handle, std::ptr::null_mut());
        if !handle.is_null() {
            unsafe { result::destroy_handle(handle) }.unwrap();
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::needless_range_loop)]

    use crate::driver::CudaContext;

    use super::*;

    #[test]
    fn cublas_pointer_mode() {
        let ctx = CudaContext::new(0).unwrap();
        let stream = ctx.default_stream();
        let blas = CudaBlas::new(stream.clone()).unwrap();
        assert_eq!(
            blas.get_pointer_mode().unwrap(),
            sys::cublasPointerMode_t::CUBLAS_POINTER_MODE_HOST,
            "The default pointer mode uses host pointers"
        );

        blas.set_pointer_mode(sys::cublasPointerMode_t::CUBLAS_POINTER_MODE_DEVICE)
            .unwrap();
        assert_eq!(
            blas.get_pointer_mode().unwrap(),
            sys::cublasPointerMode_t::CUBLAS_POINTER_MODE_DEVICE,
            "We have set the mode to use device pointers"
        );
    }
}
