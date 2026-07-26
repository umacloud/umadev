use std::fs::{self, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};

use umadev_runtime::{
    BlockDeliveryReport, DeliveryReceiptStage, DeliveryReport, FileInputMode, InputDelivery,
    SessionCapabilities, SessionError, TurnInput, TurnInputBlock, TurnInputBlockKind,
};

pub(crate) const MAX_INPUT_BLOCKS: usize = 32;
pub(crate) const MAX_ATTACHMENTS: usize = 16;
pub(crate) const MAX_ATTACHMENT_BYTES: u64 = 8 * 1024 * 1024;
pub(crate) const MAX_TOTAL_ATTACHMENT_BYTES: u64 = 20 * 1024 * 1024;

#[derive(Debug)]
pub(crate) enum PreparedBlock {
    Text(String),
    Image(PreparedAttachment),
    File {
        attachment: PreparedAttachment,
        mode: FileInputMode,
    },
}

impl PreparedBlock {
    pub(crate) const fn kind(&self) -> TurnInputBlockKind {
        match self {
            Self::Text(_) => TurnInputBlockKind::Text,
            Self::Image(_) => TurnInputBlockKind::Image,
            Self::File { .. } => TurnInputBlockKind::File,
        }
    }

    pub(crate) fn source_bytes(&self) -> usize {
        match self {
            Self::Text(text) => text.len(),
            Self::Image(attachment) | Self::File { attachment, .. } => attachment.bytes.len(),
        }
    }

    pub(crate) fn media_type(&self) -> &str {
        match self {
            Self::Text(_) => "text/plain; charset=utf-8",
            Self::Image(attachment) | Self::File { attachment, .. } => &attachment.media_type,
        }
    }
}

pub(crate) struct PreparedAttachment {
    pub(crate) canonical_path: PathBuf,
    pub(crate) bytes: Vec<u8>,
    pub(crate) media_type: String,
}

impl std::fmt::Debug for PreparedAttachment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedAttachment")
            .field("canonical_path", &"[local path redacted]")
            .field("bytes", &self.bytes.len())
            .field("media_type", &self.media_type)
            .finish()
    }
}

impl PreparedAttachment {
    pub(crate) fn bounded_text(&self, index: usize) -> Result<&str, SessionError> {
        if self.bytes.contains(&0) {
            return Err(invalid(
                index,
                TurnInputBlockKind::File,
                "text materialization rejected a NUL byte",
            ));
        }
        std::str::from_utf8(&self.bytes).map_err(|_| {
            invalid(
                index,
                TurnInputBlockKind::File,
                "text materialization requires valid UTF-8",
            )
        })
    }
}

#[derive(Debug)]
pub(crate) struct PreparedTurnInput {
    pub(crate) blocks: Vec<PreparedBlock>,
}

impl PreparedTurnInput {
    pub(crate) fn report(
        &self,
        deliveries: &[InputDelivery],
        encoded_bytes: usize,
    ) -> DeliveryReport {
        let blocks = self
            .blocks
            .iter()
            .enumerate()
            .zip(deliveries.iter().copied())
            .map(|((index, block), delivery)| BlockDeliveryReport {
                index,
                kind: block.kind(),
                delivery,
                source_bytes: block.source_bytes(),
                media_type: Some(block.media_type().to_owned()),
            })
            .collect();
        DeliveryReport {
            blocks,
            encoded_bytes: Some(encoded_bytes),
            receipt: DeliveryReceiptStage::TransportWritten,
        }
    }
}

pub(crate) async fn prepare(input: TurnInput) -> Result<PreparedTurnInput, SessionError> {
    tokio::task::spawn_blocking(move || prepare_blocking(input))
        .await
        .map_err(|_| SessionError::InputInvalid {
            index: 0,
            kind: TurnInputBlockKind::File,
            reason: "attachment validation task failed".to_string(),
        })?
}

pub(crate) fn ensure_supported(
    input: &TurnInput,
    capabilities: SessionCapabilities,
) -> Result<(), SessionError> {
    for (index, block) in input.blocks.iter().enumerate() {
        match capabilities.delivery_for(block.kind()) {
            InputDelivery::Native => {}
            InputDelivery::MaterializedText => match block {
                TurnInputBlock::File {
                    mode: FileInputMode::MaterializeText,
                    ..
                } => {}
                _ => {
                    return Err(unsupported(
                        index,
                        block.kind(),
                        "this live session requires explicit text materialization",
                    ));
                }
            },
            InputDelivery::Unsupported => {
                return Err(unsupported(
                    index,
                    block.kind(),
                    "this live session did not advertise this input kind",
                ));
            }
        }
    }
    Ok(())
}

fn prepare_blocking(input: TurnInput) -> Result<PreparedTurnInput, SessionError> {
    if input.blocks.is_empty() {
        return Err(invalid(0, TurnInputBlockKind::Text, "input is empty"));
    }
    if input.blocks.len() > MAX_INPUT_BLOCKS {
        return Err(invalid(
            MAX_INPUT_BLOCKS,
            TurnInputBlockKind::Text,
            "input exceeds the 32-block limit",
        ));
    }
    let attachment_count = input
        .blocks
        .iter()
        .filter(|block| !matches!(block, TurnInputBlock::Text { .. }))
        .count();
    if attachment_count > MAX_ATTACHMENTS {
        return Err(invalid(
            MAX_ATTACHMENTS,
            TurnInputBlockKind::File,
            "input exceeds the 16-attachment limit",
        ));
    }

    let mut total = 0_u64;
    let mut blocks = Vec::with_capacity(input.blocks.len());
    for (index, block) in input.blocks.into_iter().enumerate() {
        match block {
            TurnInputBlock::Text { text } => blocks.push(PreparedBlock::Text(text)),
            TurnInputBlock::Image { path } => {
                let attachment = read_attachment(&path, index, TurnInputBlockKind::Image)?;
                total = checked_total(total, attachment.bytes.len() as u64, index)?;
                if !attachment.media_type.starts_with("image/") {
                    return Err(invalid(
                        index,
                        TurnInputBlockKind::Image,
                        "content magic is not a supported PNG, JPEG, GIF, or WebP image",
                    ));
                }
                blocks.push(PreparedBlock::Image(attachment));
            }
            TurnInputBlock::File { path, mode } => {
                let attachment = read_attachment(&path, index, TurnInputBlockKind::File)?;
                total = checked_total(total, attachment.bytes.len() as u64, index)?;
                blocks.push(PreparedBlock::File { attachment, mode });
            }
        }
    }
    Ok(PreparedTurnInput { blocks })
}

fn checked_total(total: u64, added: u64, index: usize) -> Result<u64, SessionError> {
    let total = total.saturating_add(added);
    if total > MAX_TOTAL_ATTACHMENT_BYTES {
        return Err(invalid(
            index,
            TurnInputBlockKind::File,
            "attachments exceed the 20 MiB total limit",
        ));
    }
    Ok(total)
}

fn read_attachment(
    path: &Path,
    index: usize,
    kind: TurnInputBlockKind,
) -> Result<PreparedAttachment, SessionError> {
    let before = fs::symlink_metadata(path)
        .map_err(|_| invalid(index, kind, "attachment is unavailable"))?;
    if !metadata_is_regular_no_reparse(&before) {
        return Err(invalid(index, kind, "attachment must be a regular file"));
    }
    if before.len() > MAX_ATTACHMENT_BYTES {
        return Err(invalid(
            index,
            kind,
            "attachment exceeds the 8 MiB per-file limit",
        ));
    }
    let canonical = fs::canonicalize(path)
        .map_err(|_| invalid(index, kind, "attachment path could not be resolved"))?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let mut file = options
        .open(path)
        .map_err(|_| invalid(index, kind, "attachment could not be opened"))?;
    let opened = file
        .metadata()
        .map_err(|_| invalid(index, kind, "attachment metadata is unavailable"))?;
    if !metadata_is_regular_no_reparse(&opened) || !same_file_identity(&before, &opened) {
        return Err(invalid(index, kind, "attachment changed during validation"));
    }
    let mut bytes = Vec::with_capacity(usize::try_from(opened.len()).unwrap_or(0));
    file.by_ref()
        .take(MAX_ATTACHMENT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| invalid(index, kind, "attachment could not be read"))?;
    if bytes.len() as u64 > MAX_ATTACHMENT_BYTES {
        return Err(invalid(
            index,
            kind,
            "attachment exceeds the 8 MiB per-file limit",
        ));
    }
    let after_path = fs::canonicalize(path)
        .map_err(|_| invalid(index, kind, "attachment changed during validation"))?;
    let after = fs::symlink_metadata(path)
        .map_err(|_| invalid(index, kind, "attachment changed during validation"))?;
    if after_path != canonical
        || !metadata_is_regular_no_reparse(&after)
        || !same_file_identity(&opened, &after)
    {
        return Err(invalid(index, kind, "attachment changed during validation"));
    }
    let media_type = detect_media_type(&bytes, path);
    validate_extension_claim(path, &media_type, index, kind)?;
    Ok(PreparedAttachment {
        canonical_path: canonical,
        bytes,
        media_type,
    })
}

fn metadata_is_regular_no_reparse(metadata: &fs::Metadata) -> bool {
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return false;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return false;
        }
    }
    true
}

#[cfg(unix)]
fn same_file_identity(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt as _;

    left.dev() == right.dev()
        && left.ino() == right.ino()
        && left.len() == right.len()
        && left.mtime() == right.mtime()
        && left.mtime_nsec() == right.mtime_nsec()
}

#[cfg(windows)]
fn same_file_identity(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt as _;

    left.creation_time() == right.creation_time()
        && left.last_write_time() == right.last_write_time()
        && left.file_size() == right.file_size()
}

#[cfg(not(any(unix, windows)))]
fn same_file_identity(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    left.len() == right.len() && left.modified().ok() == right.modified().ok()
}

fn detect_media_type(bytes: &[u8], path: &Path) -> String {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return "image/png".to_string();
    }
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        return "image/jpeg".to_string();
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return "image/gif".to_string();
    }
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return "image/webp".to_string();
    }
    if bytes.starts_with(b"%PDF-") {
        return "application/pdf".to_string();
    }
    if bytes.starts_with(b"PK\x03\x04") {
        return "application/zip".to_string();
    }
    if bytes.starts_with(&[0x1f, 0x8b]) {
        return "application/gzip".to_string();
    }
    if !bytes.contains(&0) && std::str::from_utf8(bytes).is_ok() {
        return text_media_type(path).to_string();
    }
    "application/octet-stream".to_string()
}

fn text_media_type(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("json") => "application/json",
        Some("html" | "htm") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js" | "mjs" | "cjs") => "text/javascript; charset=utf-8",
        Some("xml") => "application/xml",
        Some("md" | "markdown") => "text/markdown; charset=utf-8",
        _ => "text/plain; charset=utf-8",
    }
}

fn validate_extension_claim(
    path: &Path,
    media_type: &str,
    index: usize,
    kind: TurnInputBlockKind,
) -> Result<(), SessionError> {
    let claimed = path
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase);
    let expected = match claimed.as_deref() {
        Some("png") => Some("image/png"),
        Some("jpg" | "jpeg") => Some("image/jpeg"),
        Some("gif") => Some("image/gif"),
        Some("webp") => Some("image/webp"),
        Some("pdf") => Some("application/pdf"),
        _ => None,
    };
    if expected.is_some_and(|expected| media_type != expected) {
        return Err(invalid(
            index,
            kind,
            "attachment extension does not match its content magic",
        ));
    }
    Ok(())
}

pub(crate) fn unsupported(index: usize, kind: TurnInputBlockKind, reason: &str) -> SessionError {
    SessionError::InputUnsupported {
        index,
        kind,
        reason: reason.to_string(),
    }
}

pub(crate) fn file_uri(
    path: &Path,
    index: usize,
    kind: TurnInputBlockKind,
) -> Result<String, SessionError> {
    #[cfg(windows)]
    let normalized = windows_file_uri_path(path);
    #[cfg(not(windows))]
    let normalized = path.to_path_buf();
    url::Url::from_file_path(normalized)
        .map(|url| url.to_string())
        .map_err(|()| {
            invalid(
                index,
                kind,
                "attachment path cannot be represented as a file URI",
            )
        })
}

#[cfg(windows)]
fn windows_file_uri_path(path: &Path) -> PathBuf {
    let value = path.to_string_lossy();
    if let Some(rest) = value.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = value.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        path.to_path_buf()
    }
}

pub(crate) fn invalid(index: usize, kind: TurnInputBlockKind, reason: &str) -> SessionError {
    SessionError::InputInvalid {
        index,
        kind,
        reason: reason.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use tempfile::tempdir;

    #[tokio::test]
    async fn validates_magic_and_preserves_order_without_reporting_paths() {
        let dir = tempdir().unwrap();
        let image = dir.path().join("图 像.png");
        fs::write(&image, b"\x89PNG\r\n\x1a\nrest").unwrap();
        let prepared = prepare(TurnInput::new(vec![
            TurnInputBlock::Text {
                text: "before".into(),
            },
            TurnInputBlock::Image {
                path: image.clone(),
            },
            TurnInputBlock::Text {
                text: "after".into(),
            },
        ]))
        .await
        .unwrap();
        let report = prepared.report(
            &[
                InputDelivery::Native,
                InputDelivery::Native,
                InputDelivery::Native,
            ],
            123,
        );
        assert_eq!(report.blocks[1].media_type.as_deref(), Some("image/png"));
        assert_eq!(report.blocks[2].index, 2);
        assert!(!serde_json::to_string(&report).unwrap().contains("图 像"));
    }

    #[tokio::test]
    async fn extension_spoof_and_directory_are_rejected_without_path_leaks() {
        let dir = tempdir().unwrap();
        let fake = dir.path().join("private-customer.png");
        fs::write(&fake, b"not an image").unwrap();
        let error = prepare(TurnInput::new(vec![TurnInputBlock::Image { path: fake }]))
            .await
            .unwrap_err();
        assert!(!error.to_string().contains("private-customer"));
        let error = prepare(TurnInput::new(vec![TurnInputBlock::File {
            path: dir.path().to_path_buf(),
            mode: FileInputMode::NativeOnly,
        }]))
        .await
        .unwrap_err();
        assert!(error.to_string().contains("regular file"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_attachment_is_rejected() {
        use std::os::unix::fs::symlink;

        let dir = tempdir().unwrap();
        let target = dir.path().join("target.txt");
        let link = dir.path().join("link.txt");
        fs::write(&target, "secret").unwrap();
        symlink(target, &link).unwrap();
        let error = prepare(TurnInput::new(vec![TurnInputBlock::File {
            path: link,
            mode: FileInputMode::MaterializeText,
        }]))
        .await
        .unwrap_err();
        assert!(error.to_string().contains("regular file"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fifo_attachment_is_rejected_without_blocking() {
        let dir = tempdir().unwrap();
        let fifo = dir.path().join("attachment.txt");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(status.success());
        let error = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            prepare(TurnInput::new(vec![TurnInputBlock::File {
                path: fifo,
                mode: FileInputMode::MaterializeText,
            }])),
        )
        .await
        .expect("a FIFO must be rejected without waiting for a writer")
        .unwrap_err();
        assert!(error.to_string().contains("regular file"));
    }

    #[tokio::test]
    async fn attachment_count_and_sparse_file_size_limits_are_enforced() {
        let dir = tempdir().unwrap();
        let small = dir.path().join("small.txt");
        fs::write(&small, "x").unwrap();
        let blocks = (0..=MAX_ATTACHMENTS)
            .map(|_| TurnInputBlock::File {
                path: small.clone(),
                mode: FileInputMode::NativeOnly,
            })
            .collect();
        let count_error = prepare(TurnInput::new(blocks)).await.unwrap_err();
        assert!(count_error.to_string().contains("16-attachment"));

        let large = dir.path().join("large.bin");
        let file = File::create(&large).unwrap();
        file.set_len(MAX_ATTACHMENT_BYTES + 1).unwrap();
        let size_error = prepare(TurnInput::new(vec![TurnInputBlock::File {
            path: large,
            mode: FileInputMode::NativeOnly,
        }]))
        .await
        .unwrap_err();
        assert!(size_error.to_string().contains("8 MiB"));
        assert!(checked_total(MAX_TOTAL_ATTACHMENT_BYTES, 1, 3).is_err());
    }

    #[test]
    fn unsupported_delivery_is_rejected_before_any_attachment_read() {
        let input = TurnInput::new(vec![TurnInputBlock::File {
            path: PathBuf::from("missing-private-path.txt"),
            mode: FileInputMode::NativeOnly,
        }]);
        let capabilities = umadev_runtime::SessionCapabilities {
            text_input: InputDelivery::Native,
            file_input: InputDelivery::MaterializedText,
            ..umadev_runtime::SessionCapabilities::default()
        };
        let error = ensure_supported(&input, capabilities).unwrap_err();
        assert!(matches!(error, SessionError::InputUnsupported { .. }));
        assert!(!error.to_string().contains("missing-private-path"));
    }
}
