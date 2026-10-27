//! Filesystem reads and optional Poppler rendering stay on server read workers.
use super::read_process::run_bounded;
use super::reads::{relative_path, root_path};
use crate::api::schema::{FilePreviewContent, FilePreviewParams, PreviewRaster, ResponseResult};
use crate::platform::PdfTool;
use std::{io, time::Duration};

const PDF_BYTES: usize = 32 * 1024 * 1024;
const TEXT_BYTES: usize = 64 * 1024;
const RASTER_JSON_BYTES: usize = 120 * 1024;

fn unavailable(reason: &str, message: impl Into<String>) -> ResponseResult {
    ResponseResult::FilePreview {
        content: FilePreviewContent::Unavailable {
            reason: reason.into(),
            message: message.into(),
        },
    }
}

fn failure(error: io::Error) -> ResponseResult {
    let message = error.to_string();
    let reason = match error.kind() {
        io::ErrorKind::NotFound => "tool_missing",
        io::ErrorKind::TimedOut => "timeout",
        _ if message.contains("too_large") || message.contains("limit") => "too_large",
        _ if message.to_ascii_lowercase().contains("password") => "encrypted",
        _ if message.contains("unsupported") => "unsupported",
        _ => "invalid_data",
    };
    unavailable(reason, message)
}

fn pdf_command(tool: PdfTool) -> io::Result<std::process::Command> {
    Ok(crate::noninteractive_process::command(
        crate::platform::resolve_pdf_tool(tool)?,
    ))
}

fn pdf_failure(tool: PdfTool, error: io::Error) -> ResponseResult {
    // Also contextualize a spawn-time disappearance or missing executable loader.
    failure(if error.kind() == io::ErrorKind::NotFound {
        tool.missing()
    } else {
        error
    })
}

async fn decode(bytes: Vec<u8>, width: u32, height: u32) -> io::Result<PreviewRaster> {
    let mut command = crate::noninteractive_process::command(std::env::current_exe()?);
    command.args([
        "--internal-file-preview",
        "--width",
        &width.to_string(),
        "--height",
        &height.to_string(),
    ]);
    let output = run_bounded(
        command,
        RASTER_JSON_BYTES,
        Duration::from_secs(8),
        false,
        Some(bytes),
    )
    .await?;
    if output.truncated {
        return Err(io::Error::other("too_large: raster JSON output"));
    }
    serde_json::from_slice(&output.bytes).map_err(io::Error::other)
}

fn pdf_info(bytes: &[u8]) -> io::Result<u32> {
    let text = std::str::from_utf8(bytes).map_err(io::Error::other)?;
    let encrypted = text
        .lines()
        .find_map(|line| line.strip_prefix("Encrypted:"))
        .map(str::trim);
    if encrypted != Some("no") {
        return Err(io::Error::other("password: encrypted or unrecognized PDF"));
    }
    let pages = text
        .lines()
        .find_map(|line| line.strip_prefix("Pages:"))
        .and_then(|value| value.trim().parse::<u32>().ok())
        .ok_or_else(|| io::Error::other("invalid PDF page count"))?;
    if pages == 0 || pages > 200 {
        return Err(io::Error::other(
            "too_large: PDF must contain 1..=200 pages",
        ));
    }
    Ok(pages)
}

pub(super) async fn read_preview(params: FilePreviewParams) -> io::Result<ResponseResult> {
    crate::file_preview::validate_size(params.width, params.height)?;
    if params.page == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "page must be at least 1",
        ));
    }
    let root = root_path(&params.root)?;
    let path = relative_path(&params.path)?;
    let mut bytes = crate::platform::read_confined_file(root, path, TEXT_BYTES + 1)?;
    let pdf = bytes.starts_with(b"%PDF-");
    let image = image::guess_format(&bytes).is_ok();
    if pdf || image {
        let limit = if pdf {
            PDF_BYTES
        } else {
            crate::file_preview::IMAGE_BYTES
        };
        bytes = crate::platform::read_confined_file(root, path, limit + 1)?;
        if bytes.len() > limit {
            return Ok(unavailable(
                "too_large",
                "encoded preview source exceeds read limit",
            ));
        }
    }
    if pdf {
        let mut info = match pdf_command(PdfTool::Info) {
            Ok(command) => command,
            Err(error) => return Ok(pdf_failure(PdfTool::Info, error)),
        };
        info.env("LC_ALL", "C").arg("-");
        let info = match run_bounded(
            info,
            16 * 1024,
            Duration::from_secs(2),
            false,
            Some(bytes.clone()),
        )
        .await
        {
            Ok(output) if !output.truncated => output,
            Ok(_) => {
                return Ok(unavailable(
                    "too_large",
                    "PDF metadata exceeds output limit",
                ))
            }
            Err(error) => return Ok(pdf_failure(PdfTool::Info, error)),
        };
        let pages = match pdf_info(&info.bytes) {
            Ok(pages) => pages,
            Err(error) => return Ok(failure(error)),
        };
        if params.page > pages {
            return Ok(unavailable(
                "invalid_page",
                format!("page {} is outside 1..={pages}", params.page),
            ));
        }
        let mut render = match pdf_command(PdfTool::ToPpm) {
            Ok(command) => command,
            Err(error) => return Ok(pdf_failure(PdfTool::ToPpm, error)),
        };
        render.env("LC_ALL", "C").args([
            "-f",
            &params.page.to_string(),
            "-l",
            &params.page.to_string(),
            "-singlefile",
            "-scale-to",
            "160",
            "-png",
            "-hide-annotations",
            "-",
        ]);
        let png = match run_bounded(
            render,
            512 * 1024,
            Duration::from_secs(5),
            false,
            Some(bytes),
        )
        .await
        {
            Ok(output) if !output.truncated => output.bytes,
            Ok(_) => return Ok(unavailable("too_large", "PDF raster exceeds output limit")),
            Err(error) => return Ok(pdf_failure(PdfTool::ToPpm, error)),
        };
        return Ok(match decode(png, params.width, params.height).await {
            Ok(image) => ResponseResult::FilePreview {
                content: FilePreviewContent::Pdf {
                    image,
                    page: params.page,
                    pages,
                },
            },
            Err(error) => failure(error),
        });
    }
    if image {
        return Ok(match decode(bytes, params.width, params.height).await {
            Ok(image) => ResponseResult::FilePreview {
                content: FilePreviewContent::Image { image },
            },
            Err(error) => failure(error),
        });
    }
    let truncated = bytes.len() > TEXT_BYTES;
    bytes.truncate(TEXT_BYTES);
    if truncated {
        if let Err(error) = std::str::from_utf8(&bytes) {
            if error.error_len().is_none() {
                bytes.truncate(error.valid_up_to());
            }
        }
    }
    if bytes.contains(&0) {
        return Ok(unavailable(
            "unsupported",
            "binary file is not a supported image or PDF",
        ));
    }
    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(_) => {
            return Ok(unavailable(
                "unsupported",
                "non-UTF8 file is not previewable",
            ))
        }
    };
    Ok(ResponseResult::FilePreview {
        content: FilePreviewContent::Text { text, truncated },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_pdf_tools_are_typed_and_actionable_even_after_resolution() {
        for tool in [PdfTool::Info, PdfTool::ToPpm] {
            let result = pdf_failure(tool, io::Error::from(io::ErrorKind::NotFound));
            let ResponseResult::FilePreview {
                content: FilePreviewContent::Unavailable { reason, message },
            } = result
            else {
                panic!("expected typed unavailable");
            };
            assert_eq!(reason, "tool_missing");
            assert!(message.contains(tool.name()));
            assert!(message.contains("install Poppler"));
            assert!(!message.contains("os error 2"));
        }
    }

    #[test]
    fn pdf_metadata_rejects_encrypted_excessive_and_missing_page_counts() {
        assert_eq!(pdf_info(b"Pages: 2\nEncrypted: no\n").unwrap(), 2);
        for value in [
            b"Pages: 201\nEncrypted: no\n".as_slice(),
            b"Pages: 2\nEncrypted: yes\n",
            b"Pages: 0\nEncrypted: no\n",
            b"Pages: nope\nEncrypted: no\n",
        ] {
            assert!(pdf_info(value).is_err());
        }
    }
}

#[cfg(test)]
mod poppler_tests {
    use super::*;
    fn fixture(url: &str) -> Vec<u8> {
        let red = "1 0 0 rg 0 0 100 200 re f\n";
        let blue = "0 0 1 rg 0 0 100 200 re f\n";
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R /OpenAction 7 0 R >>".into(),
            "<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>".into(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 200] /Contents 5 0 R >>".into(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 200] /Contents 6 0 R >>".into(),
            format!("<< /Length {} >>\nstream\n{red}endstream", red.len()),
            format!("<< /Length {} >>\nstream\n{blue}endstream", blue.len()),
            format!("<< /S /JavaScript /JS (this.submitForm\\(\\{{cURL: '{url}'\\}}\\);) >>"),
        ];
        let mut bytes = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (index, object) in objects.iter().enumerate() {
            offsets.push(bytes.len());
            bytes.extend_from_slice(format!("{} 0 obj\n{object}\nendobj\n", index + 1).as_bytes());
        }
        let start = bytes.len();
        bytes.extend_from_slice(
            format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
        );
        for offset in offsets {
            bytes.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        bytes.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{start}\n%%EOF\n",
                objects.len() + 1
            )
            .as_bytes(),
        );
        bytes
    }

    #[test]
    fn optional_poppler_renders_only_requested_page_without_openaction_network() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let bytes = fixture(&format!(
            "http://{}/synthetic-action",
            listener.local_addr().unwrap()
        ));
        let mut info = match pdf_command(PdfTool::Info) {
            Ok(command) => command,
            Err(error) => {
                assert!(
                    matches!(pdf_failure(PdfTool::Info, error), ResponseResult::FilePreview { content: FilePreviewContent::Unavailable { reason, .. } } if reason == "tool_missing")
                );
                return;
            }
        };
        info.env("LC_ALL", "C").arg("-");
        let output = match runtime.block_on(run_bounded(
            info,
            16384,
            Duration::from_secs(2),
            false,
            Some(bytes.clone()),
        )) {
            Ok(output) => output,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                assert!(
                    matches!(failure(error), ResponseResult::FilePreview { content: FilePreviewContent::Unavailable { reason, .. } } if reason == "tool_missing")
                );
                return;
            }
            Err(error) => panic!("pdfinfo: {error}"),
        };
        assert_eq!(pdf_info(&output.bytes).unwrap(), 2);
        let mut render = pdf_command(PdfTool::ToPpm).expect("pdftoppm from Poppler");
        render.args([
            "-f",
            "2",
            "-l",
            "2",
            "-singlefile",
            "-scale-to",
            "160",
            "-png",
            "-hide-annotations",
            "-",
        ]);
        let png = runtime
            .block_on(run_bounded(
                render,
                512 * 1024,
                Duration::from_secs(5),
                false,
                Some(bytes),
            ))
            .unwrap();
        assert!(!png.truncated);
        let raster = crate::file_preview::decode(&png.bytes, 40, 80).unwrap();
        assert_eq!((raster.width, raster.height), (40, 80));
        use base64::Engine;
        let rgb = base64::engine::general_purpose::STANDARD
            .decode(raster.rgb_base64)
            .unwrap();
        assert!(rgb[2] > 200 && rgb[0] < 20, "second page is blue");
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    }
}
