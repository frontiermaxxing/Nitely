//! ReFS cache files: clone aligned extents and copy only the final partial cluster.
use anyhow::{bail, Context, Result};
use std::{
    ffi::c_void, fs::OpenOptions,
    io::{Read, Seek, SeekFrom, Write},
    os::windows::{ffi::OsStrExt, fs::OpenOptionsExt, io::AsRawHandle},
    path::Path, time::{Duration, Instant},
};

#[repr(C)]
struct DuplicateExtents {
    source: *mut c_void,
    source_offset: i64,
    target_offset: i64,
    bytes: i64,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetVolumePathNameW(path: *const u16, volume: *mut u16, length: u32) -> i32;
    fn GetVolumeInformationW(root: *const u16, name: *mut u16, name_len: u32,
        serial: *mut u32, component_len: *mut u32, flags: *mut u32,
        filesystem: *mut u16, filesystem_len: u32) -> i32;
    fn GetDiskFreeSpaceW(root: *const u16, sectors_per_cluster: *mut u32,
        bytes_per_sector: *mut u32, free_clusters: *mut u32, total_clusters: *mut u32) -> i32;
    fn DeviceIoControl(handle: *mut c_void, code: u32, input: *const c_void, input_len: u32,
        output: *mut c_void, output_len: u32, returned: *mut u32, overlapped: *mut c_void) -> i32;
}

fn volume(path: &Path) -> Result<(Vec<u16>, u64)> {
    let path = std::fs::canonicalize(path)?;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut root = vec![0u16; 32768];
    // SAFETY: input is terminated and output has the declared capacity.
    if unsafe { GetVolumePathNameW(wide.as_ptr(), root.as_mut_ptr(), root.len() as u32) } == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    root.truncate(root.iter().position(|v| *v == 0).context("unterminated volume path")? + 1);
    let mut fs = [0u16; 64];
    // SAFETY: all optional outputs are null, filesystem points to its declared buffer.
    if unsafe { GetVolumeInformationW(root.as_ptr(), std::ptr::null_mut(), 0,
        std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), fs.as_mut_ptr(), fs.len() as u32) } == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let end = fs.iter().position(|v| *v == 0).unwrap_or(fs.len());
    if !String::from_utf16_lossy(&fs[..end]).eq_ignore_ascii_case("ReFS") {
        bail!("lane block cloning requires ReFS; other Windows volumes start cold");
    }
    let (mut sectors, mut bytes, mut free, mut total) = (0, 0, 0, 0);
    // SAFETY: terminated root and valid output values.
    if unsafe { GetDiskFreeSpaceW(root.as_ptr(), &mut sectors, &mut bytes, &mut free, &mut total) } == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let cluster = u64::from(sectors) * u64::from(bytes);
    if !cluster.is_power_of_two() || !(4096..=65536).contains(&cluster) {
        bail!("unsupported ReFS cluster size");
    }
    Ok((root, cluster))
}

pub(crate) fn clone_file(from: &Path, to: &Path) -> Result<()> {
    let (source_volume, cluster) = volume(from)?;
    let (target_volume, target_cluster) = volume(to.parent().context("clone destination has no parent")?)?;
    if source_volume != target_volume || cluster != target_cluster {
        bail!("ReFS clone requires the same volume");
    }
    // Deny writers/deleters while cloning. A donor locked by another process starts cold.
    let mut source = OpenOptions::new().read(true).share_mode(1).open(from)?;
    let length = source.metadata()?.len();
    if length > i64::MAX as u64 { bail!("clone file is too large"); }
    let mut target = OpenOptions::new().read(true).write(true).create_new(true).share_mode(0).open(to)?;
    target.set_len(length)?;
    let aligned = length / cluster * cluster;
    let started = Instant::now();
    let mut offset = 0;
    while offset < aligned {
        if started.elapsed() > Duration::from_secs(30) { bail!("ReFS clone time budget exhausted"); }
        // Each request is cluster aligned and strictly less than the 4 GiB API limit.
        let bytes = (aligned - offset).min(1024 * 1024 * 1024);
        let request = DuplicateExtents { source: source.as_raw_handle(),
            source_offset: offset as i64, target_offset: offset as i64, bytes: bytes as i64 };
        let mut returned = 0;
        // SAFETY: synchronous ioctl, repr(C) input and both live file handles. No output buffer.
        if unsafe { DeviceIoControl(target.as_raw_handle(), 0x0009_8344,
            (&request as *const DuplicateExtents).cast(), std::mem::size_of::<DuplicateExtents>() as u32,
            std::ptr::null_mut(), 0, &mut returned, std::ptr::null_mut()) } == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        offset += bytes;
    }
    // ReFS forbids unaligned extents. Copy only this bounded tail, including tiny files.
    // A failed ioctl never triggers a byte-copy fallback for the aligned portion.
    let tail = (length - aligned) as usize;
    if tail > 0 {
        let mut data = vec![0; tail];
        source.seek(SeekFrom::Start(aligned))?;
        source.read_exact(&mut data)?;
        target.seek(SeekFrom::Start(aligned))?;
        target.write_all(&data)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    /// Opt-in on a real ReFS test directory; lack of ReFS is a failure, not a fake pass.
    #[test]
    #[ignore = "requires NITELY_REFS_TEST_ROOT on a real ReFS volume"]
    fn refs_clone_preserves_tail_and_source_after_destination_write() {
        let parent = std::env::var_os("NITELY_REFS_TEST_ROOT").expect("set a disposable ReFS test parent");
        let root = Path::new(&parent).join(crate::paths::unique_temp("nitely-refs", "").file_name().unwrap());
        std::fs::create_dir(&root).unwrap();
        let (_, cluster) = volume(&root).expect("test root must be ReFS");
        for length in [0, 37, cluster, cluster + 37] {
            let from = root.join(format!("source-{length}"));
            let to = root.join(format!("clone-{length}"));
            let data = vec![42u8; length as usize];
            std::fs::write(&from, &data).unwrap();
            clone_file(&from, &to).unwrap();
            assert_eq!(std::fs::read(&to).unwrap(), data);
            std::fs::write(&to, b"independent").unwrap();
            assert_eq!(std::fs::read(&from).unwrap(), data);
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unsupported_windows_volume_stays_cold_without_touching_source() {
        let root = crate::paths::unique_temp("lane-clone-volume", "");
        std::fs::create_dir_all(root.join("source/target")).unwrap();
        std::fs::create_dir_all(root.join("destination")).unwrap();
        let from = root.join("source/target/object");
        std::fs::write(&from, vec![42u8; 8192 + 37]).unwrap();
        if volume(&from).is_err() {
            assert!(!crate::lane_clone::seed(&root.join("source"), &root.join("destination"), "target").unwrap());
            assert!(!root.join("destination/target").exists());
            assert_eq!(std::fs::read(&from).unwrap(), vec![42u8; 8192 + 37]);
        } else {
            eprintln!("ReFS available; exercise native acceptance with the explicit ignored test");
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
