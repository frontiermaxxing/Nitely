//! Native counters: English PDH names work on localized Windows; invalid/missing values are null.
use super::Metrics;
use std::ffi::c_void;
type Handle = *mut c_void;
#[repr(C)]
struct Value { status: u32, value: f64 }
#[link(name = "pdh")]
extern "system" {
    fn PdhOpenQueryW(source: *const u16, data: usize, query: *mut Handle) -> u32;
    fn PdhAddEnglishCounterW(query: Handle, path: *const u16, data: usize, counter: *mut Handle) -> u32;
    fn PdhCollectQueryData(query: Handle) -> u32;
    fn PdhGetFormattedCounterValue(counter: Handle, format: u32, kind: *mut u32, value: *mut Value) -> u32;
    fn PdhCloseQuery(query: Handle) -> u32;
}
#[repr(C)]
#[derive(Default)]
struct PerformanceInfo {
    size: u32,
    commit: usize, limit: usize, peak: usize, physical: usize, available: usize,
    cache: usize, kernel: usize, paged: usize, nonpaged: usize, page_size: usize,
    handles: u32, processes: u32, threads: u32,
}
#[link(name = "psapi")]
extern "system" { fn GetPerformanceInfo(info: *mut PerformanceInfo, size: u32) -> i32; }
#[link(name = "kernel32")]
extern "system" { fn GetTickCount64() -> u64; }
#[link(name = "ntdll")]
extern "system" { fn NtQuerySystemInformation(class: u32, info: *mut c_void, size: u32, returned: *mut u32) -> i32; }

pub struct Counters { query: Handle, counters: [Handle; 7], boot: Option<String> }
impl Counters {
    pub fn new() -> Self {
        let mut result = Self { query: std::ptr::null_mut(), counters: [std::ptr::null_mut(); 7], boot: boot_id() };
        // SAFETY: handles are owned by this struct; every out pointer lives through its call.
        unsafe {
            if PdhOpenQueryW(std::ptr::null(), 0, &mut result.query) != 0 { return result; }
            for (counter, name) in result.counters.iter_mut().zip([
                r"\Processor(_Total)\% Processor Time", r"\Memory\Pages Input/sec", r"\Memory\Pages Output/sec",
                r"\PhysicalDisk(_Total)\Disk Read Bytes/sec", r"\PhysicalDisk(_Total)\Disk Write Bytes/sec",
                r"\PhysicalDisk(_Total)\Avg. Disk sec/Transfer", r"\PhysicalDisk(_Total)\Current Disk Queue Length",
            ]) {
                let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
                if PdhAddEnglishCounterW(result.query, wide.as_ptr(), 0, counter) != 0 { *counter = std::ptr::null_mut(); }
            }
        }
        result
    }
    pub fn read(&mut self) -> (Metrics, Option<String>, u64) {
        let mut info = PerformanceInfo { size: std::mem::size_of::<PerformanceInfo>() as u32, ..PerformanceInfo::default() };
        let mut metrics = Metrics::default();
        // SAFETY: exact PERFORMANCE_INFORMATION layout, initialized writable buffer of stated size.
        if unsafe { GetPerformanceInfo(&mut info, std::mem::size_of::<PerformanceInfo>() as u32) } != 0 {
            let bytes = |pages: usize| (pages as u64).checked_mul(info.page_size as u64);
            metrics.physical_total_bytes = bytes(info.physical);
            metrics.physical_available_bytes = bytes(info.available);
            metrics.commit_bytes = bytes(info.commit);
            metrics.commit_limit_bytes = bytes(info.limit);
            metrics.kernel_paged_bytes = bytes(info.paged);
            metrics.kernel_nonpaged_bytes = bytes(info.nonpaged);
        }
        // SAFETY: a successfully opened query stays live until Drop; failed counters stay null.
        if !self.query.is_null() && unsafe { PdhCollectQueryData(self.query) } == 0 {
            let values: Vec<Option<f64>> = self.counters.iter().map(|&counter| {
                if counter.is_null() { return None; }
                let mut value = Value { status: 0, value: 0.0 };
                // PDH_FMT_DOUBLE. Both return code and data status must be successful.
                let status = unsafe { PdhGetFormattedCounterValue(counter, 0x8200, std::ptr::null_mut(), &mut value) };
                (status == 0 && value.status <= 1 && value.value.is_finite() && value.value >= 0.0).then_some(value.value)
            }).collect();
            metrics.cpu_percent = values[0]; metrics.pages_input_per_sec = values[1]; metrics.pages_output_per_sec = values[2];
            metrics.disk_read_bytes_per_sec = values[3]; metrics.disk_write_bytes_per_sec = values[4];
            metrics.disk_latency_seconds = values[5]; metrics.disk_queue = values[6];
        }
        // SAFETY: no pointers; monotonic boot-relative milliseconds, including sleep.
        (metrics, self.boot.clone(), unsafe { GetTickCount64() })
    }
}
impl Drop for Counters {
    fn drop(&mut self) { if !self.query.is_null() { unsafe { PdhCloseQuery(self.query); } } }
}
pub(super) fn boot_id() -> Option<String> {
    // SystemBootEnvironmentInformation (90): GUID followed by firmware type and boot flags.
    // Read only the GUID, stable for this boot and independent of wall clock adjustments.
    let mut buffer = [0u64; 4];
    let mut returned = 0;
    // SAFETY: aligned 32-byte output buffer; NTSTATUS failure yields unavailable, not a guess.
    let status = unsafe { NtQuerySystemInformation(90, buffer.as_mut_ptr().cast(), std::mem::size_of_val(&buffer) as u32, &mut returned) };
    (status >= 0 && returned >= 16).then(|| format!("{:016x}{:016x}", buffer[0], buffer[1]))
}

#[cfg(test)]
pub fn resident_bytes() -> Option<u64> {
    #[repr(C)]
    #[derive(Default)]
    struct Memory { size: u32, faults: u32, peak: usize, resident: usize, peak_paged: usize, paged: usize, peak_nonpaged: usize, nonpaged: usize, committed: usize, peak_committed: usize, private: usize }
    #[link(name = "kernel32")]
    extern "system" { fn GetCurrentProcess() -> Handle; }
    #[link(name = "psapi")]
    extern "system" { fn GetProcessMemoryInfo(process: Handle, counters: *mut Memory, size: u32) -> i32; }
    let mut memory = Memory { size: std::mem::size_of::<Memory>() as u32, ..Memory::default() };
    // SAFETY: current-process pseudo handle; exact PROCESS_MEMORY_COUNTERS_EX layout and size.
    (unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut memory, std::mem::size_of::<Memory>() as u32) } != 0).then_some(memory.resident as u64)
}
