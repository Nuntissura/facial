//! Isolated-process candidate quota. Covers this private pool only, not driver/library overhead.
use super::core::{CudaContext, CudaStream};
use crate::driver::{sys, result::DriverError};
use std::sync::{Arc, Mutex, OnceLock};

#[derive(Clone, Copy, Debug)]
pub struct ManagedPoolPeaks { pub limit: u64, pub reserved_high: u64, pub used_high: u64 }
static LIMIT: OnceLock<usize> = OnceLock::new();
struct Pool { handle: usize, context: Arc<CudaContext> }
// The isolated child retains its CUDA context and pool until process death. Reaper-held
// parent reservations cover this lifetime; no asynchronous free is counted as reclaimed early.
static POOL: Mutex<Option<Pool>> = Mutex::new(None);
fn invalid() -> DriverError { DriverError(sys::CUresult::CUDA_ERROR_INVALID_VALUE) }
pub fn configure_managed_pool(limit: usize) -> Result<(), DriverError> {
    if limit == 0 { return Err(invalid()); }
    LIMIT.set(limit).map_err(|_| invalid())
}
pub fn managed_pool_enabled() -> bool { LIMIT.get().is_some() }
pub(crate) unsafe fn allocate(stream: &Arc<CudaStream>, bytes: usize) -> Result<Option<sys::CUdeviceptr>, DriverError> {
    let Some(&limit) = LIMIT.get() else { return Ok(None) };
    if bytes > limit { return Err(DriverError(sys::CUresult::CUDA_ERROR_OUT_OF_MEMORY)); }
    if !stream.ctx.has_async_alloc { return Err(DriverError(sys::CUresult::CUDA_ERROR_NOT_SUPPORTED)); }
    let mut guard = POOL.lock().map_err(|_| invalid())?;
    if guard.is_none() {
        let mut props: sys::CUmemPoolProps = std::mem::zeroed();
        props.allocType = sys::CUmemAllocationType::CU_MEM_ALLOCATION_TYPE_PINNED;
        props.handleTypes = sys::CUmemAllocationHandleType::CU_MEM_HANDLE_TYPE_NONE;
        props.location.type_ = sys::CUmemLocationType::CU_MEM_LOCATION_TYPE_DEVICE;
        props.location.id = stream.ctx.cu_device;
        props.maxSize = limit;
        let mut handle = std::ptr::null_mut();
        sys::cuMemPoolCreate(&mut handle, &props).result()?;
        *guard = Some(Pool { handle: handle as usize, context: stream.ctx.clone() });
    }
    let pool = guard.as_ref().ok_or_else(invalid)?;
    if pool.context.cu_ctx != stream.ctx.cu_ctx { return Err(invalid()); }
    let mut ptr = 0;
    sys::cuMemAllocFromPoolAsync(&mut ptr, bytes, pool.handle as _, stream.cu_stream).result()?;
    Ok(Some(ptr))
}
pub fn managed_pool_peaks() -> Result<Option<ManagedPoolPeaks>, DriverError> {
    let guard = POOL.lock().map_err(|_| invalid())?;
    let Some(pool) = guard.as_ref() else { return Ok(None) };
    pool.context.bind_to_thread()?;
    let mut reserved_high = 0u64; let mut used_high = 0u64;
    unsafe {
        sys::cuMemPoolGetAttribute(pool.handle as _, sys::CUmemPool_attribute::CU_MEMPOOL_ATTR_RESERVED_MEM_HIGH, (&mut reserved_high as *mut u64).cast()).result()?;
        sys::cuMemPoolGetAttribute(pool.handle as _, sys::CUmemPool_attribute::CU_MEMPOOL_ATTR_USED_MEM_HIGH, (&mut used_high as *mut u64).cast()).result()?;
    }
    Ok(Some(ManagedPoolPeaks { limit: *LIMIT.get().ok_or_else(invalid)? as u64, reserved_high, used_high }))
}

/// Fixed diagnostic protocol; every advance is separately supervised by the owning worker.
pub struct PoolBoundary {
    stream: Arc<CudaStream>,
    held: Option<super::core::CudaSlice<u8>>,
    next: u8,
}
impl PoolBoundary {
    pub fn begin() -> Result<Self, DriverError> {
        if LIMIT.get().copied() != Some(16 * 1024 * 1024) || POOL.lock().map_err(|_| invalid())?.is_some() { return Err(invalid()); }
        let context = CudaContext::new(0)?;
        let stream = context.new_stream()?;
        Ok(Self { stream, held: None, next: 1 })
    }
    pub fn advance(&mut self, step: u8) -> Result<(), DriverError> {
        if step != self.next { return Err(invalid()); }
        match step {
            1 => self.expect_oom(17 * 1024 * 1024)?,
            2 => self.held = Some(unsafe { self.stream.alloc::<u8>(8 * 1024 * 1024)? }),
            3 => self.expect_oom(9 * 1024 * 1024)?,
            4 => { self.held.take(); }, // Enqueues free; parent reservation remains held.
            5 => self.stream.synchronize()?,
            6 => self.held = Some(unsafe { self.stream.alloc::<u8>(9 * 1024 * 1024)? }),
            7 => { self.held.take(); },
            8 => self.stream.synchronize()?,
            _ => return Err(invalid()),
        }
        self.next += 1;
        Ok(())
    }
    fn expect_oom(&self, bytes: usize) -> Result<(), DriverError> {
        match unsafe { self.stream.alloc::<u8>(bytes) } {
            Err(DriverError(sys::CUresult::CUDA_ERROR_OUT_OF_MEMORY)) => Ok(()),
            Err(error) => Err(error),
            Ok(allocation) => { drop(allocation); Err(invalid()) },
        }
    }
}
