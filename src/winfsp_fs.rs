//! Windows-only WinFSP virtual filesystem backed by S3.
//!
//! Presents a configured drive as a Windows volume:
//! - file writes (e.g. drag/drop into the mounted drive) are staged in
//!   memory per open handle and uploaded with `PutObject` when the handle is
//!   closed (or flushed),
//! - root directory enumeration lists objects with `ListObjectsV2`.
//!
//! v1 presents a flat namespace: only the root directory exists and only
//! top-level keys (no `/`) are enumerated. Nested keys created through other
//! tools stay directly addressable but are hidden from listings.
//!
//! WinFSP invokes filesystem callbacks on its own dispatcher threads, so the
//! blocking S3 calls below run through the Tokio handle captured at mount
//! time via [`tokio::runtime::Handle::block_on`].

use aws_sdk_s3 as s3;
use std::collections::HashMap;
use std::ffi::c_void;
use std::io::{Error, ErrorKind};
use std::sync::Mutex;
use winfsp::U16CStr;
use winfsp::filesystem::{
    DirBuffer, DirInfo, DirMarker, FileInfo, FileSecurity, FileSystemContext, OpenFileInfo,
    VolumeInfo, WideNameInfo,
};
use winfsp::host::{FileSystemHost, VolumeParams};
use winfsp_sys::{FILE_ACCESS_RIGHTS, FILE_FLAGS_AND_ATTRIBUTES};

use crate::fs_mapping::{file_name_from_key, is_root_path, s3_key_from_win_path};

const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_ARCHIVE: u32 = 0x20;
/// `CreateOptions & FILE_DIRECTORY_FILE`; set when the caller wants a directory.
const CREATE_OPTION_DIRECTORY_FILE: u32 = 0x0000_0001;
/// `Flags & FspCleanupDelete`; set when the file is being deleted.
const CLEANUP_DELETE: u32 = 0x01;

fn not_found() -> winfsp::FspError {
    Error::from(ErrorKind::NotFound).into()
}

fn invalid_input(msg: &'static str) -> winfsp::FspError {
    Error::new(ErrorKind::InvalidInput, msg).into()
}

fn s3_failed(msg: String) -> winfsp::FspError {
    Error::new(ErrorKind::Other, msg).into()
}

#[derive(Default)]
struct StagedBytes {
    data: Vec<u8>,
    dirty: bool,
}

/// Per-handle state for an open file or the root directory.
///
/// All mutable fields use interior mutability because WinFSP calls into the
/// context through shared references, potentially from several dispatcher
/// threads at once.
pub struct S3FileContext {
    /// S3 key for files; `None` for the root directory.
    key: Mutex<Option<String>>,
    is_dir: bool,
    /// True for handles created by [`FileSystemContext::create`] that have
    /// not been uploaded yet, so even an empty file materialises on close.
    is_new: bool,
    delete_pending: Mutex<bool>,
    staging: Mutex<StagedBytes>,
    dir_buffer: DirBuffer,
}

impl S3FileContext {
    fn dir() -> Self {
        Self {
            key: Mutex::new(None),
            is_dir: true,
            is_new: false,
            delete_pending: Mutex::new(false),
            staging: Mutex::new(StagedBytes::default()),
            dir_buffer: DirBuffer::new(),
        }
    }

    fn file(key: String) -> Self {
        Self {
            key: Mutex::new(Some(key)),
            is_dir: false,
            is_new: false,
            delete_pending: Mutex::new(false),
            staging: Mutex::new(StagedBytes::default()),
            dir_buffer: DirBuffer::new(),
        }
    }

    fn new_file(key: String) -> Self {
        Self {
            key: Mutex::new(Some(key)),
            is_dir: false,
            is_new: true,
            delete_pending: Mutex::new(false),
            staging: Mutex::new(StagedBytes::default()),
            dir_buffer: DirBuffer::new(),
        }
    }
}

/// WinFSP filesystem context mapping one S3 bucket to a flat drive.
pub struct S3FileSystem {
    client: s3::Client,
    bucket: String,
    label: String,
    rt: tokio::runtime::Handle,
    /// Last-known object sizes, refreshed by listing/reads/writes.
    sizes: Mutex<HashMap<String, u64>>,
}

impl S3FileSystem {
    pub fn new(
        client: s3::Client,
        bucket: String,
        label: String,
        rt: tokio::runtime::Handle,
    ) -> Self {
        Self {
            client,
            bucket,
            label,
            rt,
            sizes: Mutex::new(HashMap::new()),
        }
    }

    /// List top-level keys in the bucket via `ListObjectsV2` (paginated).
    fn list_root(&self) -> Result<Vec<(String, u64)>, String> {
        self.rt.block_on(async {
            let mut out = Vec::new();
            let mut token: Option<String> = None;
            loop {
                let resp = self
                    .client
                    .list_objects_v2()
                    .bucket(self.bucket.as_str())
                    .set_continuation_token(token)
                    .send()
                    .await
                    .map_err(|e| e.to_string())?;
                for obj in resp.contents() {
                    if let Some(key) = obj.key() {
                        if key.contains('/') {
                            continue; // flat v1 view: hide nested keys
                        }
                        let size = obj.size().unwrap_or(0).max(0) as u64;
                        out.push((key.to_owned(), size));
                    }
                }
                if resp.is_truncated().unwrap_or(false) {
                    token = resp.next_continuation_token().map(str::to_owned);
                    if token.is_none() {
                        break;
                    }
                } else {
                    break;
                }
            }
            Ok(out)
        })
    }

    fn head_size(&self, key: &str) -> Option<u64> {
        self.rt
            .block_on(async {
                self.client
                    .head_object()
                    .bucket(self.bucket.as_str())
                    .key(key)
                    .send()
                    .await
                    .ok()
            })
            .and_then(|o| o.content_length())
            .map(|n| n.max(0) as u64)
    }

    fn put_bytes(&self, key: &str, bytes: Vec<u8>) -> Result<(), String> {
        self.rt.block_on(async {
            self.client
                .put_object()
                .bucket(self.bucket.as_str())
                .key(key)
                .body(s3::primitives::ByteStream::from(bytes))
                .send()
                .await
                .map_err(|e| e.to_string())?;
            Ok(())
        })
    }

    fn get_bytes(&self, key: &str) -> Result<Vec<u8>, String> {
        self.rt.block_on(async {
            let resp = self
                .client
                .get_object()
                .bucket(self.bucket.as_str())
                .key(key)
                .send()
                .await
                .map_err(|e| e.to_string())?;
            let data = resp.body.collect().await.map_err(|e| e.to_string())?;
            Ok(data.into_bytes().to_vec())
        })
    }

    fn delete_key(&self, key: &str) -> Result<(), String> {
        self.rt.block_on(async {
            self.client
                .delete_object()
                .bucket(self.bucket.as_str())
                .key(key)
                .send()
                .await
                .map_err(|e| e.to_string())?;
            Ok(())
        })
    }

    fn cached_size(&self, key: &str) -> Option<u64> {
        if let Some(size) = self.sizes.lock().unwrap().get(key).copied() {
            return Some(size);
        }
        let size = self.head_size(key)?;
        self.sizes.lock().unwrap().insert(key.to_owned(), size);
        Some(size)
    }
}

impl FileSystemContext for S3FileSystem {
    type FileContext = S3FileContext;

    fn get_security_by_name(
        &self,
        file_name: &U16CStr,
        _security_descriptor: Option<&mut [c_void]>,
        _reparse_point_resolver: impl FnOnce(&U16CStr) -> Option<FileSecurity>,
    ) -> winfsp::Result<FileSecurity> {
        let path = file_name.to_string_lossy();
        let attributes = if is_root_path(&path) {
            FILE_ATTRIBUTE_DIRECTORY
        } else {
            let Some(key) = s3_key_from_win_path(&path) else {
                return Err(not_found());
            };
            if key.contains('/') || self.cached_size(&key).is_none() {
                return Err(not_found());
            }
            FILE_ATTRIBUTE_ARCHIVE
        };
        Ok(FileSecurity {
            reparse: false,
            sz_security_descriptor: 0,
            attributes,
        })
    }

    fn open(
        &self,
        file_name: &U16CStr,
        create_options: u32,
        _granted_access: FILE_ACCESS_RIGHTS,
        file_info: &mut OpenFileInfo,
    ) -> winfsp::Result<Self::FileContext> {
        let path = file_name.to_string_lossy();
        if is_root_path(&path) {
            let info = file_info.as_mut();
            info.file_attributes = FILE_ATTRIBUTE_DIRECTORY;
            info.file_size = 0;
            info.allocation_size = 0;
            return Ok(S3FileContext::dir());
        }
        let Some(key) = s3_key_from_win_path(&path) else {
            return Err(not_found());
        };
        if key.contains('/') || create_options & CREATE_OPTION_DIRECTORY_FILE != 0 {
            return Err(not_found());
        }
        let Some(size) = self.cached_size(&key) else {
            return Err(not_found());
        };
        let info = file_info.as_mut();
        info.file_attributes = FILE_ATTRIBUTE_ARCHIVE;
        info.file_size = size;
        info.allocation_size = size;
        Ok(S3FileContext::file(key))
    }

    #[allow(clippy::too_many_arguments)]
    fn create(
        &self,
        file_name: &U16CStr,
        create_options: u32,
        _granted_access: FILE_ACCESS_RIGHTS,
        _file_attributes: FILE_FLAGS_AND_ATTRIBUTES,
        _security_descriptor: Option<&[c_void]>,
        _allocation_size: u64,
        _extra_buffer: Option<&[u8]>,
        _extra_buffer_is_reparse_point: bool,
        file_info: &mut OpenFileInfo,
    ) -> winfsp::Result<Self::FileContext> {
        let path = file_name.to_string_lossy();
        if is_root_path(&path) {
            return Err(Error::from(ErrorKind::AlreadyExists).into());
        }
        let Some(key) = s3_key_from_win_path(&path) else {
            return Err(invalid_input("invalid file name"));
        };
        if key.contains('/') {
            return Err(invalid_input("nested paths are not supported"));
        }
        if create_options & CREATE_OPTION_DIRECTORY_FILE != 0 {
            return Err(invalid_input("subdirectories are not supported"));
        }
        let info = file_info.as_mut();
        info.file_attributes = FILE_ATTRIBUTE_ARCHIVE;
        info.file_size = 0;
        info.allocation_size = 0;
        self.sizes.lock().unwrap().insert(key.clone(), 0);
        Ok(S3FileContext::new_file(key))
    }

    fn close(&self, context: Self::FileContext) {
        if context.is_dir {
            return;
        }
        let key = context.key.lock().unwrap().clone();
        let Some(key) = key else { return };
        if *context.delete_pending.lock().unwrap() {
            if self.delete_key(&key).is_ok() {
                self.sizes.lock().unwrap().remove(&key);
            }
            return;
        }
        // `close` owns the handle, so move the staged bytes out without cloning.
        let staged = context.staging.into_inner().unwrap();
        if staged.dirty || context.is_new {
            let len = staged.data.len() as u64;
            if let Err(e) = self.put_bytes(&key, staged.data) {
                eprintln!("upload of {key} failed: {e}");
            } else {
                self.sizes.lock().unwrap().insert(key, len);
            }
        }
    }

    fn cleanup(&self, context: &Self::FileContext, _file_name: Option<&U16CStr>, flags: u32) {
        if flags & CLEANUP_DELETE != 0 {
            *context.delete_pending.lock().unwrap() = true;
        }
    }

    fn set_delete(
        &self,
        context: &Self::FileContext,
        _file_name: &U16CStr,
        delete_file: bool,
    ) -> winfsp::Result<()> {
        *context.delete_pending.lock().unwrap() = delete_file;
        Ok(())
    }

    fn flush(
        &self,
        context: Option<&Self::FileContext>,
        _file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        let Some(context) = context else {
            return Ok(());
        };
        if context.is_dir || *context.delete_pending.lock().unwrap() {
            return Ok(());
        }
        let (key, data) = {
            let staged = context.staging.lock().unwrap();
            if !staged.dirty {
                return Ok(());
            }
            (context.key.lock().unwrap().clone(), staged.data.clone())
        };
        if let Some(key) = key {
            let len = data.len() as u64;
            self.put_bytes(&key, data).map_err(s3_failed)?;
            self.sizes.lock().unwrap().insert(key, len);
            context.staging.lock().unwrap().dirty = false;
        }
        Ok(())
    }

    fn get_file_info(
        &self,
        context: &Self::FileContext,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        if context.is_dir {
            file_info.file_attributes = FILE_ATTRIBUTE_DIRECTORY;
            file_info.file_size = 0;
            file_info.allocation_size = 0;
            return Ok(());
        }
        let key = context.key.lock().unwrap().clone();
        let Some(key) = key else {
            return Err(not_found());
        };
        // Prefer in-handle staged length so recent writes are reflected.
        let staged_len = {
            let staged = context.staging.lock().unwrap();
            if staged.dirty || context.is_new {
                Some(staged.data.len() as u64)
            } else {
                None
            }
        };
        let size = match staged_len {
            Some(len) => len,
            None => self.cached_size(&key).ok_or_else(not_found)?,
        };
        file_info.file_attributes = FILE_ATTRIBUTE_ARCHIVE;
        file_info.file_size = size;
        file_info.allocation_size = size;
        Ok(())
    }

    fn get_security(
        &self,
        _context: &Self::FileContext,
        _security_descriptor: Option<&mut [c_void]>,
    ) -> winfsp::Result<u64> {
        // No ACLs in v1; report an empty descriptor.
        Ok(0)
    }

    fn read_directory(
        &self,
        context: &Self::FileContext,
        _pattern: Option<&U16CStr>,
        marker: DirMarker<'_>,
        buffer: &mut [u8],
    ) -> winfsp::Result<u32> {
        if !context.is_dir {
            return Err(invalid_input("not a directory"));
        }
        if marker.is_none() {
            let entries = self.list_root().map_err(s3_failed)?;
            for (key, size) in &entries {
                self.sizes.lock().unwrap().insert(key.clone(), *size);
            }
            let lock = context.dir_buffer.acquire(true, None)?;
            for (key, size) in &entries {
                let mut info = DirInfo::<255>::new();
                info.set_name(file_name_from_key(key))?;
                let file_info = info.file_info_mut();
                file_info.file_attributes = FILE_ATTRIBUTE_ARCHIVE;
                file_info.file_size = *size;
                file_info.allocation_size = *size;
                lock.write(&mut info)?;
            }
        }
        Ok(context.dir_buffer.read(marker, buffer))
    }

    fn rename(
        &self,
        context: &Self::FileContext,
        _file_name: &U16CStr,
        new_file_name: &U16CStr,
        replace_if_exists: bool,
    ) -> winfsp::Result<()> {
        if context.is_dir {
            return Err(invalid_input("directory rename is not supported"));
        }
        let new_key = s3_key_from_win_path(&new_file_name.to_string_lossy())
            .ok_or_else(|| invalid_input("invalid target name"))?;
        if new_key.contains('/') {
            return Err(invalid_input("nested paths are not supported"));
        }
        let old_key = context.key.lock().unwrap().clone().ok_or_else(not_found)?;
        if old_key == new_key {
            return Ok(());
        }
        if !replace_if_exists && self.head_size(&new_key).is_some() {
            return Err(Error::from(ErrorKind::AlreadyExists).into());
        }
        // Staged (not yet uploaded) content moves directly to the new key;
        // otherwise copy server-side.
        let staged_data = {
            let staged = context.staging.lock().unwrap();
            if staged.dirty {
                Some(staged.data.clone())
            } else {
                None
            }
        };
        if let Some(data) = staged_data {
            let len = data.len() as u64;
            self.put_bytes(&new_key, data).map_err(s3_failed)?;
            self.sizes.lock().unwrap().insert(new_key.clone(), len);
            context.staging.lock().unwrap().dirty = false;
        } else {
            let copy_source = format!("{}/{}", self.bucket, old_key);
            self.rt
                .block_on(async {
                    self.client
                        .copy_object()
                        .bucket(self.bucket.as_str())
                        .key(new_key.as_str())
                        .copy_source(copy_source)
                        .send()
                        .await
                        .map_err(|e| e.to_string())
                })
                .map_err(s3_failed)?;
            if let Some(len) = self.sizes.lock().unwrap().get(&old_key).copied() {
                self.sizes.lock().unwrap().insert(new_key.clone(), len);
            }
        }
        let _ = self.delete_key(&old_key);
        self.sizes.lock().unwrap().remove(&old_key);
        *context.key.lock().unwrap() = Some(new_key);
        Ok(())
    }

    fn set_file_size(
        &self,
        context: &Self::FileContext,
        new_size: u64,
        _set_allocation_size: bool,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        if context.is_dir {
            return Err(invalid_input("not a file"));
        }
        let key = context.key.lock().unwrap().clone();
        {
            let mut staged = context.staging.lock().unwrap();
            if !staged.dirty && staged.data.is_empty() {
                if let Some(ref key) = key {
                    if let Ok(data) = self.get_bytes(key) {
                        staged.data = data;
                    }
                }
            }
            staged.data.resize(new_size as usize, 0);
            staged.dirty = true;
        }
        file_info.file_size = new_size;
        file_info.allocation_size = new_size;
        if let Some(key) = key {
            self.sizes.lock().unwrap().insert(key, new_size);
        }
        Ok(())
    }

    fn read(
        &self,
        context: &Self::FileContext,
        buffer: &mut [u8],
        offset: u64,
    ) -> winfsp::Result<u32> {
        if context.is_dir {
            return Err(invalid_input("cannot read a directory"));
        }
        let key = context.key.lock().unwrap().clone();
        let Some(key) = key else {
            return Ok(0);
        };
        let mut staged = context.staging.lock().unwrap();
        if !staged.dirty && staged.data.is_empty() && !context.is_new {
            match self.get_bytes(&key) {
                Ok(data) => staged.data = data,
                Err(_) => return Ok(0), // object missing: report EOF
            }
        }
        let offset = offset as usize;
        if offset >= staged.data.len() {
            return Ok(0);
        }
        let len = (staged.data.len() - offset).min(buffer.len());
        buffer[..len].copy_from_slice(&staged.data[offset..offset + len]);
        Ok(len as u32)
    }

    fn write(
        &self,
        context: &Self::FileContext,
        buffer: &[u8],
        offset: u64,
        _write_to_eof: bool,
        _constrained_io: bool,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<u32> {
        if context.is_dir {
            return Err(invalid_input("cannot write to a directory"));
        }
        let key = context.key.lock().unwrap().clone().ok_or_else(not_found)?;
        // Preserve bytes outside the written range when an existing object is
        // opened for an overwrite. New files start empty by design.
        let needs_hydration = {
            let staged = context.staging.lock().unwrap();
            !context.is_new && !staged.dirty && staged.data.is_empty()
        };
        if needs_hydration {
            let data = self.get_bytes(&key).map_err(s3_failed)?;
            let mut staged = context.staging.lock().unwrap();
            if !staged.dirty && staged.data.is_empty() {
                staged.data = data;
            }
        }
        // Stage the bytes; the actual `PutObject` happens on close/flush.
        let len = {
            let mut staged = context.staging.lock().unwrap();
            let offset = offset as usize;
            if staged.data.len() < offset {
                staged.data.resize(offset, 0);
            }
            let end = offset + buffer.len();
            if staged.data.len() < end {
                staged.data.resize(end, 0);
            }
            staged.data[offset..end].copy_from_slice(buffer);
            staged.dirty = true;
            staged.data.len() as u64
        };
        file_info.file_size = len;
        file_info.allocation_size = len;
        self.sizes.lock().unwrap().insert(key, len);
        Ok(buffer.len() as u32)
    }

    fn get_volume_info(&self, out_volume_info: &mut VolumeInfo) -> winfsp::Result<()> {
        out_volume_info.total_size = 1 << 40;
        out_volume_info.free_size = 1 << 39;
        out_volume_info.set_volume_label(self.label.as_str());
        Ok(())
    }

    fn set_volume_label(
        &self,
        volume_label: &U16CStr,
        volume_info: &mut VolumeInfo,
    ) -> winfsp::Result<()> {
        volume_info.set_volume_label(volume_label.to_string_lossy());
        Ok(())
    }
}

/// Mount the configured drive as a Windows volume and block until Ctrl+C.
///
/// `letter` is the drive letter from the drive config (e.g. `'D'` mounts `D:`).
pub async fn run_mount(
    client: s3::Client,
    label: String,
    bucket: String,
    letter: char,
    rt: tokio::runtime::Handle,
) -> Result<(), String> {
    winfsp::winfsp_init().map_err(|e| format!("winfsp init failed: {e:?}"))?;
    let context = S3FileSystem::new(client, bucket, label.clone(), rt);
    let mut params = VolumeParams::new();
    params.filesystem_name("ISE");
    let mut host = FileSystemHost::new(params, context)
        .map_err(|e| format!("create filesystem failed: {e:?}"))?;
    let mount_point = format!("{letter}:");
    host.mount(mount_point.clone())
        .map_err(|e| format!("mount {mount_point} failed (is WinFSP installed?): {e:?}"))?;
    host.start()
        .map_err(|e| format!("start filesystem dispatcher failed: {e:?}"))?;
    println!("mounted drive '{label}' at {mount_point} (press Ctrl+C to unmount)");
    tokio::signal::ctrl_c()
        .await
        .map_err(|e| format!("signal handler failed: {e}"))?;
    println!("unmounting {mount_point}...");
    host.unmount();
    host.stop();
    Ok(())
}
