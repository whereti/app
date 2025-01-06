//! Raw access to an NTFS volume (`\\.\C:`). Opening it requires administrator rights.

use std::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::fs::{FileExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;

use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAG_NO_BUFFERING, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
};
use windows_sys::Win32::System::Ioctl::{FSCTL_GET_NTFS_VOLUME_DATA, FSCTL_QUERY_USN_JOURNAL};
use windows_sys::Win32::System::IO::DeviceIoControl;

/// Mirror of `NTFS_VOLUME_DATA_BUFFER`.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct NtfsVolumeData {
    pub volume_serial_number: i64,
    pub number_sectors: i64,
    pub total_clusters: i64,
    pub free_clusters: i64,
    pub total_reserved: i64,
    pub bytes_per_sector: u32,
    pub bytes_per_cluster: u32,
    pub bytes_per_file_record_segment: u32,
    pub clusters_per_file_record_segment: u32,
    pub mft_valid_data_length: i64,
    pub mft_start_lcn: i64,
    pub mft2_start_lcn: i64,
    pub mft_zone_start: i64,
    pub mft_zone_end: i64,
}

/// Mirror of `USN_JOURNAL_DATA_V0`.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct UsnJournalData {
    pub usn_journal_id: u64,
    pub first_usn: i64,
    pub next_usn: i64,
    pub lowest_valid_usn: i64,
    pub max_usn: i64,
    pub maximum_size: u64,
    pub allocation_delta: u64,
}

pub struct Volume {
    file: File,
    pub letter: char,
    pub ntfs: NtfsVolumeData,
    pub unbuffered: bool,
}

impl Volume {
    /// Opens `\\.\X:`. With `unbuffered`, reads bypass the Windows file cache,
    /// so they measure the disk and do not push user data out of the cache.
    pub fn open(letter: char, unbuffered: bool) -> io::Result<Self> {
        let letter = letter.to_ascii_uppercase();
        if !letter.is_ascii_alphabetic() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid drive letter"));
        }
        let mut opts = OpenOptions::new();
        opts.read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE);
        if unbuffered {
            opts.custom_flags(FILE_FLAG_NO_BUFFERING);
        }
        let file = opts.open(format!(r"\\.\{letter}:"))?;
        let mut vol = Volume { file, letter, ntfs: NtfsVolumeData::default(), unbuffered };
        let mut data = NtfsVolumeData::default();
        // SAFETY: NtfsVolumeData is a plain repr(C) struct; any byte pattern is valid.
        let out = unsafe { as_bytes_mut(&mut data) };
        vol.ioctl(FSCTL_GET_NTFS_VOLUME_DATA, &[], out).map_err(|e| {
            if e.raw_os_error() == Some(1) {
                // ERROR_INVALID_FUNCTION: not NTFS
                io::Error::new(io::ErrorKind::Unsupported, format!("{letter}: is not an NTFS volume"))
            } else {
                e
            }
        })?;
        if data.bytes_per_file_record_segment == 0 || data.bytes_per_cluster == 0 || data.bytes_per_sector == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid NTFS volume data"));
        }
        vol.ntfs = data;
        Ok(vol)
    }

    pub fn query_usn_journal(&self) -> io::Result<UsnJournalData> {
        let mut data = UsnJournalData::default();
        // SAFETY: plain repr(C) struct.
        let out = unsafe { as_bytes_mut(&mut data) };
        self.ioctl(FSCTL_QUERY_USN_JOURNAL, &[], out)?;
        Ok(data)
    }

    /// Sends an FSCTL. Returns the number of output bytes.
    pub fn ioctl(&self, code: u32, input: &[u8], output: &mut [u8]) -> io::Result<usize> {
        let mut returned = 0u32;
        // SAFETY: buffers are valid for the lengths passed; synchronous call.
        let ok = unsafe {
            DeviceIoControl(
                self.file.as_raw_handle() as _,
                code,
                if input.is_empty() { std::ptr::null() } else { input.as_ptr() as *const c_void },
                input.len() as u32,
                output.as_mut_ptr() as *mut c_void,
                output.len() as u32,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(returned as usize)
    }

    /// Reads exactly `buf.len()` bytes at `offset`. In unbuffered mode the offset,
    /// length and buffer address must be multiples of the sector size.
    pub fn read_exact_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let mut done = 0usize;
        while done < buf.len() {
            let n = self.file.seek_read(&mut buf[done..], offset + done as u64)?;
            if n == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "read past end of volume"));
            }
            done += n;
        }
        Ok(())
    }
}

/// Views a plain-old-data struct as bytes.
///
/// # Safety
/// `T` must be `repr(C)` with no padding-sensitive invariants and valid for any bit pattern.
pub(crate) unsafe fn as_bytes_mut<T>(v: &mut T) -> &mut [u8] {
    std::slice::from_raw_parts_mut(v as *mut T as *mut u8, std::mem::size_of::<T>())
}

/// Heap buffer whose start is aligned, as unbuffered I/O requires.
pub struct AlignedBuf {
    raw: Vec<u8>,
    start: usize,
    len: usize,
}

impl AlignedBuf {
    pub fn new(len: usize, align: usize) -> Self {
        assert!(align.is_power_of_two());
        let raw = vec![0u8; len + align];
        let start = raw.as_ptr().align_offset(align);
        AlignedBuf { raw, start, len }
    }
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut self.raw[self.start..self.start + self.len]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn struct_sizes_match_windows_headers() {
        assert_eq!(std::mem::size_of::<NtfsVolumeData>(), 96);
        assert_eq!(std::mem::size_of::<UsnJournalData>(), 56);
    }

    #[test]
    fn aligned_buffer_is_aligned() {
        let mut b = AlignedBuf::new(10_000, 4096);
        let s = b.as_mut_slice();
        assert_eq!(s.as_ptr() as usize % 4096, 0);
        assert_eq!(s.len(), 10_000);
    }
}
