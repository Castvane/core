use std::fs::Metadata;

#[cfg(windows)]
pub const OPEN_REPARSE_POINT: u32 = 0x0020_0000;

#[cfg(unix)]
pub fn private_metadata(metadata: &Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o077 == 0
}

#[cfg(windows)]
pub fn private_metadata(metadata: &Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    // Windows access control is inherited from the operator's private data directory.
    // Refuse links and reparse points when the path was opened without following them.
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0
}
