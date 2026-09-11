//! Native image input from the macOS clipboard.

use std::process::Command;

pub(super) const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;
const PNG_MAGIC: &[u8] = b"\x89PNG\r\n\x1a\n";

#[cfg(target_os = "macos")]
const READ_PNG_SCRIPT: &str = r#"
ObjC.import("AppKit");
ObjC.import("Foundation");
function run() {
    const board = $.NSPasteboard.generalPasteboard;
    let data = board.dataForType($.NSPasteboardTypePNG);
    if (!data) {
        const image = $.NSImage.alloc.initWithPasteboard(board);
        if (!image) return;
        const tiff = image.TIFFRepresentation;
        if (!tiff) return;
        const bitmap = $.NSBitmapImageRep.imageRepWithData(tiff);
        if (!bitmap) return;
        data = bitmap.representationUsingTypeProperties(
            $.NSBitmapImageFileTypePNG,
            $({})
        );
    }
    if (data) $.NSFileHandle.fileHandleWithStandardOutput.writeData(data);
}
"#;

#[cfg(target_os = "macos")]
pub(super) fn read() -> Result<Option<Vec<u8>>, String> {
    let output = Command::new("/usr/bin/osascript")
        .args(["-l", "JavaScript", "-e", READ_PNG_SCRIPT])
        .output()
        .map_err(|error| format!("could not read the clipboard: {error}"))?;
    interpret(output.status.success(), output.stdout, &output.stderr)
}

#[cfg(not(target_os = "macos"))]
pub(super) fn read() -> Result<Option<Vec<u8>>, String> {
    Err("clipboard image paste is only supported on macOS".into())
}

fn interpret(success: bool, bytes: Vec<u8>, stderr: &[u8]) -> Result<Option<Vec<u8>>, String> {
    if !success {
        let detail = String::from_utf8_lossy(stderr);
        let detail = detail.trim();
        return Err(if detail.is_empty() {
            "could not read an image from the clipboard".into()
        } else {
            format!("could not read the clipboard: {detail}")
        });
    }
    if bytes.is_empty() {
        return Ok(None);
    }
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err("clipboard image exceeds 20 MiB".into());
    }
    if !bytes.starts_with(PNG_MAGIC) {
        return Err("clipboard returned invalid image data".into());
    }
    Ok(Some(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_success_means_no_image() {
        assert_eq!(interpret(true, Vec::new(), b"").unwrap(), None);
    }

    #[test]
    fn valid_png_bytes_are_returned() {
        let bytes = b"\x89PNG\r\n\x1a\nimage".to_vec();
        assert_eq!(interpret(true, bytes.clone(), b"").unwrap(), Some(bytes));
    }

    #[test]
    fn invalid_output_and_process_errors_are_distinct() {
        assert!(interpret(true, b"text".to_vec(), b"")
            .unwrap_err()
            .contains("invalid image"));
        assert!(interpret(false, Vec::new(), b"denied")
            .unwrap_err()
            .contains("denied"));
    }
}
