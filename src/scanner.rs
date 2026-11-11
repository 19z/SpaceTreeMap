use std::{
    collections::HashMap,
    ffi::OsString,
    fs::{File, OpenOptions},
    io::{self, BufReader, Read, Seek, SeekFrom},
    os::windows::{ffi::OsStringExt, fs::OpenOptionsExt},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

use mft::{
    attribute::{
        header::ResidentialHeader,
        x30::{FileNameAttr, FileNamespace},
        MftAttributeType,
    },
    MftEntry, MftReference,
};
use ntfs::Ntfs;
use windows::{
    core::PCWSTR,
    Win32::Storage::FileSystem::{
        GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW,
    },
};

use crate::model::{FileRecord, Index, LinkName, Statistics};

#[derive(Clone, Debug)]
pub struct Volume {
    pub root: PathBuf,
    pub label: String,
    pub total: u64,
    pub free: u64,
}

pub fn wide(value: &std::ffi::OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    value.encode_wide().chain(Some(0)).collect()
}

pub fn volumes() -> Vec<Volume> {
    let mut volumes = Vec::new();
    let mask = unsafe { GetLogicalDrives() };
    for letter in 0..26u8 {
        if mask & (1 << letter) == 0 {
            continue;
        }
        let root = PathBuf::from(format!("{}:\\", (b'A' + letter) as char));
        let root_w = wide(root.as_os_str());
        // Fixed and removable NTFS volumes; skip optical/network devices.
        if !matches!(unsafe { GetDriveTypeW(PCWSTR(root_w.as_ptr())) }, 2 | 3) {
            continue;
        }
        let mut label = [0u16; 261];
        let mut fs = [0u16; 32];
        let ok = unsafe {
            GetVolumeInformationW(
                PCWSTR(root_w.as_ptr()),
                Some(&mut label),
                None,
                None,
                None,
                Some(&mut fs),
            )
        }
        .is_ok();
        if !ok || String::from_utf16_lossy(&fs).trim_end_matches('\0') != "NTFS" {
            continue;
        }
        let (mut total, mut free) = (0u64, 0u64);
        unsafe {
            let _ = GetDiskFreeSpaceExW(
                PCWSTR(root_w.as_ptr()),
                None,
                Some(&mut total),
                Some(&mut free),
            );
        }
        volumes.push(Volume {
            root,
            label: String::from_utf16_lossy(&label)
                .trim_end_matches('\0')
                .to_owned(),
            total,
            free,
        });
    }
    volumes
}

#[derive(Clone, Default, Debug)]
pub struct ScanProgress {
    pub read: u64,
    pub total: u64,
    pub active: u64,
    pub phase: &'static str,
}

/// Raw Windows volumes require sector-aligned offsets and lengths. A read-through
/// cache allows NTFS's small, unaligned metadata reads without issuing them to Windows.
struct VolumeReader {
    file: File,
    position: u64,
    cache_start: u64,
    cache_len: usize,
    cache: Vec<u8>,
    length: Option<u64>,
}

impl VolumeReader {
    fn open(volume: &Path) -> io::Result<Self> {
        let letter = volume.to_string_lossy();
        let raw = format!(r"\\.\{}", letter.trim_end_matches('\\'));
        let file = OpenOptions::new().read(true).share_mode(7).open(raw)?;
        Ok(Self {
            file,
            position: 0,
            cache_start: 0,
            cache_len: 0,
            cache: vec![0; 1024 * 1024],
            length: None,
        })
    }
}

impl Read for VolumeReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() || self.length.is_some_and(|len| self.position >= len) {
            return Ok(0);
        }
        if self.position < self.cache_start
            || self.position >= self.cache_start + self.cache_len as u64
        {
            self.cache_start = self.position / 4096 * 4096;
            self.file.seek(SeekFrom::Start(self.cache_start))?;
            let count = self
                .length
                .map(|len| (len - self.cache_start).min(self.cache.len() as u64) as usize)
                .unwrap_or(self.cache.len());
            self.cache_len = self.file.read(&mut self.cache[..count])?;
        }
        let offset = (self.position - self.cache_start) as usize;
        let count = out.len().min(self.cache_len.saturating_sub(offset));
        out[..count].copy_from_slice(&self.cache[offset..offset + count]);
        self.position += count as u64;
        Ok(count)
    }
}

impl Seek for VolumeReader {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let next = match to {
            SeekFrom::Start(value) => Some(value),
            SeekFrom::Current(delta) => self.position.checked_add_signed(delta),
            SeekFrom::End(delta) => self.length.and_then(|len| len.checked_add_signed(delta)),
        }
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid volume offset"))?;
        self.position = next;
        Ok(next)
    }
}

#[derive(Debug)]
struct ParsedRecord {
    id: u64,
    sequence: u16,
    base: MftReference,
    is_dir: bool,
    names: Vec<LinkName>,
    logical: Option<u64>,
    damaged_attributes: bool,
}

fn parse_record(bytes: Vec<u8>, id: u64) -> Result<Option<ParsedRecord>, String> {
    if bytes.get(..4) != Some(b"FILE") {
        return Ok(None);
    }
    let entry = MftEntry::from_buffer(bytes, id).map_err(|error| error.to_string())?;
    if !entry.is_allocated() {
        return Ok(None);
    }
    if entry.valid_fixup != Some(true) {
        return Err("MFT 更新序列校验失败".into());
    }
    let mut parsed = ParsedRecord {
        id,
        sequence: entry.header.sequence,
        base: entry.header.base_reference,
        is_dir: entry.is_dir(),
        names: Vec::new(),
        logical: None,
        damaged_attributes: false,
    };
    // Read headers for DATA only: resident file content is never interpreted or emitted.
    for attribute in entry.iter_attribute_records_matching(Some(vec![
        MftAttributeType::FileName,
        MftAttributeType::DATA,
    ])) {
        let (header, record) = match attribute {
            Ok(attribute) => attribute,
            Err(_) => {
                parsed.damaged_attributes = true;
                continue;
            }
        };
        if header.type_code == MftAttributeType::DATA
            && header.name_size == 0
            && !header.is_continuation()
        {
            parsed.logical = Some(header.value_size());
        }
        if header.type_code != MftAttributeType::FileName {
            continue;
        }
        if let ResidentialHeader::Resident(resident) = &header.residential_header {
            let name = resident
                .value(record)
                .ok()
                .and_then(|value| FileNameAttr::from_slice(value).ok());
            if let Some(name) = name {
                let (pairs, _) = name.name.as_utf16le_bytes().as_chunks::<2>();
                let units: Vec<u16> = pairs.iter().map(|pair| u16::from_le_bytes(*pair)).collect();
                parsed.names.push(LinkName {
                    parent: name.parent,
                    name: OsString::from_wide(&units),
                    dos: name.namespace == FileNamespace::DOS,
                });
            } else {
                parsed.damaged_attributes = true;
            }
        }
    }
    Ok(Some(parsed))
}

fn collect_records<R: Read>(
    reader: &mut R,
    record_size: usize,
    total: u64,
    cancel: &AtomicBool,
    progress: &mut impl FnMut(ScanProgress),
) -> Result<(HashMap<u64, FileRecord>, Statistics), String> {
    let mut files = HashMap::new();
    let mut extensions = Vec::new();
    let mut stats = Statistics::default();
    let mut active = 0;
    for id in 0..total {
        if cancel.load(Ordering::Relaxed) {
            return Err("扫描已取消".into());
        }
        let mut bytes = vec![0u8; record_size];
        reader
            .read_exact(&mut bytes)
            .map_err(|error| format!("读取 MFT 记录 {id} 失败：{error}"))?;
        stats.records_read += 1;
        match parse_record(bytes, id) {
            Ok(Some(record)) => {
                if record.damaged_attributes {
                    stats.skipped_records += 1;
                }
                active += 1;
                if record.base.entry != 0 || record.base.sequence != 0 {
                    extensions.push(record);
                } else {
                    files.insert(
                        record.id,
                        FileRecord {
                            sequence: record.sequence,
                            is_dir: record.is_dir,
                            names: record.names,
                            logical_size: record.logical.unwrap_or(0),
                        },
                    );
                }
            }
            Ok(None) => {}
            Err(_) => stats.skipped_records += 1,
        }
        if id % 2048 == 0 || id + 1 == total {
            progress(ScanProgress {
                read: id + 1,
                total,
                active,
                phase: "顺序读取 MFT",
            });
        }
    }
    for extension in extensions {
        if cancel.load(Ordering::Relaxed) {
            return Err("扫描已取消".into());
        }
        if let Some(base) = files
            .get_mut(&extension.base.entry)
            .filter(|base| base.sequence == extension.base.sequence)
        {
            base.names.extend(extension.names);
            if let Some(size) = extension.logical {
                base.logical_size = size;
            }
        } else {
            stats.skipped_records += 1;
        }
    }
    Ok((files, stats))
}

pub fn scan_volume(
    root: &Path,
    cancel: &AtomicBool,
    mut progress: impl FnMut(ScanProgress),
) -> Result<Index, String> {
    progress(ScanProgress {
        phase: "打开 NTFS 卷",
        ..Default::default()
    });
    let mut volume = VolumeReader::open(root).map_err(|error| {
        format!(
            "无法打开原始卷 {}：{error}。请以管理员身份运行，并确认卷已解锁。",
            root.display()
        )
    })?;
    let ntfs =
        Ntfs::new(&mut volume).map_err(|error| format!("读取 NTFS 引导扇区失败：{error}"))?;
    volume.length = Some(ntfs.size());
    let mft = ntfs
        .file(&mut volume, 0)
        .map_err(|error| format!("读取 $MFT 记录失败：{error}"))?;
    let data = mft
        .data(&mut volume, "")
        .ok_or("$MFT 没有数据流")?
        .map_err(|error| format!("读取 $MFT 属性列表失败：{error}"))?;
    let attribute = data.to_attribute().map_err(|error| error.to_string())?;
    let value = attribute
        .value(&mut volume)
        .map_err(|error| error.to_string())?;
    let record_size = ntfs.file_record_size() as usize;
    if !(512..=65536).contains(&record_size) {
        return Err("不支持的 MFT 记录大小".into());
    }
    let total = value.len() / record_size as u64;
    if total == 0 {
        return Err("$MFT 数据流为空".into());
    }
    let mut reader = BufReader::with_capacity(4 * 1024 * 1024, value.attach(&mut volume));
    let (files, stats) = collect_records(&mut reader, record_size, total, cancel, &mut progress)?;
    progress(ScanProgress {
        read: total,
        total,
        active: files.len() as u64,
        phase: "建立目录树并汇总大小",
    });
    Index::build(root.to_path_buf(), files, stats, cancel)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cancelled_scan_and_truncated_stream_are_errors() {
        let mut progress = |_| {};
        let mut reader = io::Cursor::new(vec![0u8; 1024]);
        assert!(
            collect_records(&mut reader, 1024, 1, &AtomicBool::new(true), &mut progress)
                .unwrap_err()
                .contains("取消")
        );
        let mut reader = io::Cursor::new(vec![0u8; 512]);
        assert!(
            collect_records(&mut reader, 1024, 1, &AtomicBool::new(false), &mut progress)
                .unwrap_err()
                .contains("记录 0")
        );
    }

    #[test]
    fn raw_reader_adapts_unaligned_reads_and_seeks() {
        let path =
            std::env::temp_dir().join(format!("spacetreemap-reader-{}.bin", std::process::id()));
        let data: Vec<u8> = (0..8192).map(|value| (value % 251) as u8).collect();
        std::fs::write(&path, &data).unwrap();
        let mut reader = VolumeReader {
            file: File::open(&path).unwrap(),
            position: 0,
            cache_start: 0,
            cache_len: 0,
            cache: vec![0; 4096],
            length: Some(data.len() as u64),
        };
        reader.seek(SeekFrom::Start(4091)).unwrap();
        let mut out = [0u8; 17];
        reader.read_exact(&mut out).unwrap();
        assert_eq!(out, data[4091..4108]);
        reader.seek(SeekFrom::End(-3)).unwrap();
        assert_eq!(reader.read(&mut out).unwrap(), 3);
        assert_eq!(reader.read(&mut out).unwrap(), 0);
        assert!(reader.seek(SeekFrom::Start(0)).is_ok());
        assert!(reader.seek(SeekFrom::Current(-1)).is_err());
        drop(reader);
        std::fs::remove_file(path).unwrap();
    }
}
