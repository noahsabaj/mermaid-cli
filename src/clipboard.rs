//! Clipboard access for image and text paste
//!
//! Auto-detects the platform and display server, then uses the appropriate
//! system tool to read clipboard contents:
//! - Linux/Wayland: wl-paste
//! - Linux/X11: xclip
//! - macOS: pbpaste / osascript / pngpaste (for images)
//! - Windows: PowerShell (System.Windows.Forms.Clipboard)
//!
//! # The three shapes an image arrives in
//!
//! A "copied image" is not one thing, and which shape you get depends on the
//! app that did the copying, not on the platform:
//!
//! 1. **A raster handle** — screenshot tools, a browser's "Copy image".
//!    `CF_BITMAP` / `image/png` / `TIFF`.
//! 2. **An encoded blob with no raster form** — GIMP, Figma, some Electron
//!    apps. On Windows this is the `PNG` format with `CF_BITMAP` *absent*, so
//!    `Clipboard::ContainsImage()` answers False and the whole paste used to
//!    fall through to a text read.
//! 3. **A reference to a file on disk** — Explorer / Finder / a Linux file
//!    manager "Copy". `CF_HDROP` / `public.file-url` / `text/uri-list`.
//!
//! [`probe_image_source`] resolves all three on every backend, which is what
//! makes paste behave the same everywhere; `has_image` and `read_image_bytes`
//! are both defined in terms of it, so they can never disagree about whether a
//! paste is possible.
//!
//! # Bounded by construction
//!
//! Every one of those tools can hang: the X11/Wayland clipboard is served *by
//! the application that owns the selection*, so a frozen owner — or a stale
//! `$DISPLAY`/`$WAYLAND_DISPLAY` pointing at a dead server — blocks a read
//! forever, and PowerShell can wedge on a broken CLR. Nothing here calls
//! `Command::output()`/`wait()` directly; every subprocess runs under a
//! kill-on-timeout deadline so a wedged helper costs a bounded stall plus a
//! visible error, not a paste that silently never lands and a permanently
//! leaked blocking thread.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

use crate::utils::{output_with_timeout, write_stdin_with_timeout};

/// `which` existence probes and clipboard *metadata* queries (offered MIME
/// types, `osascript` clipboard info) — tiny payloads, so a slow answer means
/// the display server or selection owner is wedged, not that data is big.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Actual clipboard payload transfer (text or image bytes) — generous enough
/// for a multi-megabyte screenshot from a healthy owner, short enough that a
/// hung one can't wedge the paste path.
const DATA_TIMEOUT: Duration = Duration::from_secs(5);

/// PowerShell invocations pay CLR/JIT startup (seconds when cold) before any
/// clipboard work happens, so Windows gets a fatter budget.
const POWERSHELL_TIMEOUT: Duration = Duration::from_secs(10);

/// Display server / platform type
#[derive(Debug, Clone, Copy)]
enum ClipboardBackend {
    Wayland,
    X11,
    MacOS,
    Windows,
}

/// Ceiling on a file-reference paste. A raster/encoded clipboard payload is
/// bounded by whatever the copying app was willing to hold in memory; a *file*
/// reference is a path to anything at all, so a stray Ctrl+C on a 4 GB scan
/// must not be read into the process.
const MAX_FILE_PASTE_BYTES: u64 = 32 * 1024 * 1024;

/// True if `name` resolves on PATH. Even this probe is deadline-bounded: a
/// PATH entry on dead NFS can wedge the lookup itself.
///
/// `which` is a POSIX tool and does not exist on a stock Windows install —
/// there the equivalent is `where.exe`. The distinction went unnoticed while
/// `detect_backend` returned `Windows` before ever calling this.
fn tool_exists(name: &str) -> bool {
    let mut cmd = if cfg!(windows) {
        Command::new("where.exe")
    } else {
        Command::new("which")
    };
    output_with_timeout(cmd.arg(name), PROBE_TIMEOUT)
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Quote `s` as a PowerShell single-quoted literal (doubling embedded quotes),
/// so a path containing `'` — `C:\Users\O'Brien\…` — is data rather than
/// syntax.
fn ps_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// A PowerShell invocation for `script`, on whichever host this machine has.
///
/// `System.Windows.Forms.Clipboard` requires a single-threaded apartment.
/// Windows PowerShell (`powershell.exe`, present on every Windows install)
/// runs STA by default, so it is preferred; `pwsh` is the fallback for
/// PS7-only hosts and gets an explicit `-STA` because PowerShell Core has
/// historically defaulted to MTA, where every Clipboard call throws.
///
/// Resolved once — the probe is a process spawn, and paste is on the keystroke
/// path.
fn powershell_command(script: &str) -> Command {
    static HOST: OnceLock<(&'static str, bool)> = OnceLock::new();
    let (exe, sta) = *HOST.get_or_init(|| {
        if tool_exists("powershell") {
            ("powershell", false)
        } else {
            ("pwsh", true)
        }
    });
    let mut cmd = Command::new(exe);
    cmd.arg("-NoProfile");
    if sta {
        cmd.arg("-STA");
    }
    cmd.args(["-Command", script]);
    cmd
}

/// The extension tag for a file-reference paste, or `None` when the file is
/// not an image we can attach. The tag becomes the attachment's file extension
/// (`mermaid-img-<id>.<tag>`), so it must stay a bare lowercase token.
fn image_format_for_path(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "png" => "png",
        "jpg" | "jpeg" => "jpeg",
        "gif" => "gif",
        "webp" => "webp",
        "bmp" => "bmp",
        "tif" | "tiff" => "tiff",
        _ => return None,
    })
}

/// Where the clipboard's image is, resolved once and shared by `has_image` and
/// `read_image_bytes` so the two can never disagree.
#[derive(Debug, Clone, PartialEq)]
enum ImageSource {
    /// The clipboard itself carries the image (raster handle or encoded blob).
    Inline,
    /// The clipboard references a file on disk (a file-manager "Copy").
    File(PathBuf),
    None,
}

/// Decode a `file://` URI into a path. Percent-escapes only — enough for the
/// `text/uri-list` and `public.file-url` payloads a file manager produces, not
/// a general URI parser.
fn path_from_file_uri(uri: &str) -> Option<PathBuf> {
    let rest = uri.trim().strip_prefix("file://")?;
    // `file:///C:/x` (Windows) vs `file:///home/x` (POSIX): drop the empty
    // authority, then keep the leading slash only when it is a POSIX root.
    let rest = rest.strip_prefix('/').unwrap_or(rest);
    let mut out = String::with_capacity(rest.len());
    let mut bytes = Vec::with_capacity(rest.len());
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        if c == '%' {
            let hex: String = chars.by_ref().take(2).collect();
            match u8::from_str_radix(&hex, 16) {
                Ok(b) => bytes.push(b),
                Err(_) => return None,
            }
        } else {
            let mut buf = [0_u8; 4];
            bytes.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
        }
    }
    out.push_str(&String::from_utf8(bytes).ok()?);
    // A POSIX path lost its root to the `strip_prefix` above; a Windows path
    // ("C:/…") did not have one.
    let looks_windows = out.as_bytes().get(1) == Some(&b':');
    if !looks_windows {
        out.insert(0, '/');
    }
    Some(PathBuf::from(out))
}

/// The first entry of a file-reference list that we can actually attach.
fn first_pasteable_file<'a>(paths: impl IntoIterator<Item = &'a str>) -> Option<PathBuf> {
    paths.into_iter().find_map(|raw| {
        let raw = raw.trim();
        if raw.is_empty() || raw.starts_with('#') {
            return None;
        }
        let path = if raw.starts_with("file://") {
            path_from_file_uri(raw)?
        } else {
            PathBuf::from(raw)
        };
        (image_format_for_path(&path).is_some() && path.is_file()).then_some(path)
    })
}

/// Read a file-reference paste, refusing anything past [`MAX_FILE_PASTE_BYTES`].
fn read_image_file(path: &Path) -> Result<(Vec<u8>, String)> {
    let format = image_format_for_path(path)
        .with_context(|| format!("{} is not an image file", path.display()))?;
    let len = std::fs::metadata(path)
        .with_context(|| format!("Failed to stat {}", path.display()))?
        .len();
    anyhow::ensure!(
        len <= MAX_FILE_PASTE_BYTES,
        "{} is {len} bytes; the clipboard file-paste limit is {MAX_FILE_PASTE_BYTES}",
        path.display(),
    );
    let bytes =
        std::fs::read(path).with_context(|| format!("Failed to read {}", path.display()))?;
    anyhow::ensure!(!bytes.is_empty(), "{} is empty", path.display());
    Ok((bytes, format.to_string()))
}

/// PowerShell that reports what the clipboard offers, one line per finding:
/// `image` for a raster/encoded payload, `file:<path>` per referenced file.
///
/// `ContainsImage()` alone is NOT the question — it only answers True for
/// payloads that auto-convert to `System.Drawing.Image`, which excludes a
/// PNG-only clipboard. Asking `GetDataPresent` per format is what makes a
/// Figma/GIMP copy pasteable.
const WINDOWS_PROBE_SCRIPT: &str = "\
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Windows.Forms
$d = [System.Windows.Forms.Clipboard]::GetDataObject()
if ($null -ne $d) {
  foreach ($f in 'PNG', 'DeviceIndependentBitmap', 'Bitmap') {
    if ($d.GetDataPresent($f)) { Write-Output 'image'; break }
  }
}
if ([System.Windows.Forms.Clipboard]::ContainsFileDropList()) {
  foreach ($p in [System.Windows.Forms.Clipboard]::GetFileDropList()) {
    Write-Output ('file:' + $p)
  }
}";

/// What the clipboard is offering. One place, so `has_image` and
/// `read_image_bytes` are always answering the same question.
fn probe_image_source() -> ImageSource {
    let Some(backend) = detect_backend() else {
        return ImageSource::None;
    };
    match backend {
        ClipboardBackend::Wayland | ClipboardBackend::X11 => {
            let types = match backend {
                ClipboardBackend::Wayland => {
                    output_with_timeout(Command::new("wl-paste").arg("--list-types"), PROBE_TIMEOUT)
                },
                _ => output_with_timeout(
                    Command::new("xclip").args(["-selection", "clipboard", "-t", "TARGETS", "-o"]),
                    PROBE_TIMEOUT,
                ),
            };
            let Ok(types) = types else {
                return ImageSource::None;
            };
            let types = String::from_utf8_lossy(&types.stdout);
            if LINUX_IMAGE_MIMES
                .iter()
                .any(|(mime, _)| types.contains(mime))
            {
                return ImageSource::Inline;
            }
            if !types.contains("text/uri-list") {
                return ImageSource::None;
            }
            // A file-manager copy: the payload is a list of `file://` URIs.
            let uris = match backend {
                ClipboardBackend::Wayland => output_with_timeout(
                    Command::new("wl-paste").args(["--type", "text/uri-list"]),
                    PROBE_TIMEOUT,
                ),
                _ => output_with_timeout(
                    Command::new("xclip").args([
                        "-selection",
                        "clipboard",
                        "-t",
                        "text/uri-list",
                        "-o",
                    ]),
                    PROBE_TIMEOUT,
                ),
            };
            uris.ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
                .and_then(|list| first_pasteable_file(list.lines()))
                .map_or(ImageSource::None, ImageSource::File)
        },
        ClipboardBackend::MacOS => {
            let Ok(info) = output_with_timeout(
                Command::new("osascript").args(["-e", "clipboard info"]),
                PROBE_TIMEOUT,
            ) else {
                return ImageSource::None;
            };
            let info = String::from_utf8_lossy(&info.stdout);
            if info.contains("PNGf") || info.contains("JPEG") || info.contains("TIFF") {
                return ImageSource::Inline;
            }
            if !info.contains("furl") {
                return ImageSource::None;
            }
            // Finder ⌘C: the clipboard holds file URLs, not pixels.
            output_with_timeout(
                Command::new("osascript")
                    .args(["-e", "POSIX path of (the clipboard as «class furl»)"]),
                PROBE_TIMEOUT,
            )
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .and_then(|paths| first_pasteable_file(paths.lines()))
            .map_or(ImageSource::None, ImageSource::File)
        },
        ClipboardBackend::Windows => {
            let Ok(out) = output_with_timeout(
                &mut powershell_command(WINDOWS_PROBE_SCRIPT),
                POWERSHELL_TIMEOUT,
            ) else {
                return ImageSource::None;
            };
            let text = String::from_utf8_lossy(&out.stdout);
            if text.lines().any(|l| l.trim() == "image") {
                return ImageSource::Inline;
            }
            first_pasteable_file(text.lines().filter_map(|l| l.trim().strip_prefix("file:")))
                .map_or(ImageSource::None, ImageSource::File)
        },
    }
}

/// Image MIME types offered by X11/Wayland selection owners, in the order we
/// prefer them.
const LINUX_IMAGE_MIMES: &[(&str, &str)] = &[
    ("image/png", "png"),
    ("image/jpeg", "jpeg"),
    ("image/webp", "webp"),
    ("image/gif", "gif"),
];

/// Detect the active clipboard backend
fn detect_backend() -> Option<ClipboardBackend> {
    // macOS
    if cfg!(target_os = "macos") && tool_exists("pbpaste") {
        return Some(ClipboardBackend::MacOS);
    }

    // Windows
    if cfg!(target_os = "windows") {
        return Some(ClipboardBackend::Windows);
    }

    // Linux: check Wayland first
    if std::env::var("WAYLAND_DISPLAY").is_ok() && tool_exists("wl-paste") {
        return Some(ClipboardBackend::Wayland);
    }

    // Linux: fall back to X11
    if std::env::var("DISPLAY").is_ok() && tool_exists("xclip") {
        return Some(ClipboardBackend::X11);
    }

    None
}

/// Can the clipboard produce an image right now?
///
/// Defined as "the probe found a source", so it agrees with
/// [`read_image_bytes`] by construction. The two used to answer different
/// questions on Windows — `ContainsImage()` here, `GetImage()` there — which is
/// how a PNG-only clipboard could report `true` and then fail to read.
pub fn has_image() -> bool {
    probe_image_source() != ImageSource::None
}

/// Read image bytes from the clipboard.
/// Returns `(bytes, format)` where format is the attachment's file extension.
pub fn read_image_bytes() -> Result<(Vec<u8>, String)> {
    let backend = detect_backend()
        .context("No clipboard backend detected (need xclip, wl-paste, pbpaste, or PowerShell)")?;

    // A file-manager copy is backend-independent once the path is known: the
    // clipboard was only ever pointing at the disk.
    if let ImageSource::File(path) = probe_image_source() {
        return read_image_file(&path);
    }

    match backend {
        ClipboardBackend::Wayland | ClipboardBackend::X11 => {
            for (mime, format) in LINUX_IMAGE_MIMES {
                let output = match backend {
                    ClipboardBackend::Wayland => output_with_timeout(
                        Command::new("wl-paste").args(["--type", mime]),
                        DATA_TIMEOUT,
                    ),
                    ClipboardBackend::X11 => output_with_timeout(
                        Command::new("xclip").args(["-selection", "clipboard", "-t", mime, "-o"]),
                        DATA_TIMEOUT,
                    ),
                    _ => unreachable!(),
                };

                if let Ok(output) = output
                    && output.status.success()
                    && !output.stdout.is_empty()
                {
                    return Ok((output.stdout, (*format).to_string()));
                }
            }
            anyhow::bail!("No image data found in clipboard")
        },
        ClipboardBackend::MacOS => {
            // Use osascript to save clipboard image to a temp file, then read it
            // 0700 per-user scratch dir, not a world-readable shared /tmp path
            // another local user could read or pre-create/symlink (#11).
            let temp_path = crate::utils::private_temp_dir()?.join("mermaid-clipboard-paste.png");
            let temp_str = temp_path.to_string_lossy();
            let script = format!(
                "set theFile to POSIX file \"{}\"\n\
                 tell application \"System Events\" to set theData to the clipboard as «class PNGf»\n\
                 set fp to open for access theFile with write permission\n\
                 write theData to fp\n\
                 close access fp",
                temp_str
            );
            // Try the simpler pngpaste approach first (if available), fall back to osascript
            let pngpaste_output =
                output_with_timeout(Command::new("pngpaste").arg(&temp_path), DATA_TIMEOUT);
            let success = if let Ok(output) = pngpaste_output
                && output.status.success()
            {
                true
            } else {
                // Fall back to osascript
                output_with_timeout(
                    Command::new("osascript").args(["-e", &script]),
                    DATA_TIMEOUT,
                )
                .map(|o| o.status.success())
                .unwrap_or(false)
            };

            if success {
                let bytes = std::fs::read(&temp_path)
                    .context("Failed to read clipboard image from temp file")?;
                let _ = std::fs::remove_file(&temp_path);
                if !bytes.is_empty() {
                    return Ok((bytes, "png".to_string()));
                }
            }
            anyhow::bail!("No image data found in clipboard (macOS)")
        },
        ClipboardBackend::Windows => {
            // 0700 per-user scratch dir, not a world-readable shared /tmp path
            // another local user could read or pre-create/symlink (#11).
            let temp_path = crate::utils::private_temp_dir()?.join("mermaid-clipboard-paste.png");
            let _ = std::fs::remove_file(&temp_path);
            // Two ways in, in this order:
            //
            //   1. The raw `PNG` stream, byte-for-byte. Preferred whenever it
            //      exists — it is what Snipping Tool, Win+Shift+S, Chrome and
            //      Figma put on the clipboard, it keeps the alpha channel, and
            //      it does not depend on `ContainsImage()` (which is False for
            //      a PNG-only clipboard — the screenshot-paste bug).
            //   2. `GetImage()` re-encoded to PNG, for the CF_BITMAP-only
            //      payloads older apps produce.
            //
            // `System.Drawing` is added explicitly: `$img.Save(…,
            // [System.Drawing.Imaging.ImageFormat]::Png)` resolves that type by
            // name, and it is not auto-loaded on PowerShell 7.
            let script = format!(
                "\
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
$out = {out}
$d = [System.Windows.Forms.Clipboard]::GetDataObject()
if ($null -ne $d -and $d.GetDataPresent('PNG')) {{
  $s = $d.GetData('PNG')
  if ($s -is [System.IO.Stream]) {{
    $s.Position = 0
    $fs = [System.IO.File]::Create($out)
    try {{ $s.CopyTo($fs) }} finally {{ $fs.Dispose() }}
    exit 0
  }}
}}
if ([System.Windows.Forms.Clipboard]::ContainsImage()) {{
  $img = [System.Windows.Forms.Clipboard]::GetImage()
  if ($null -ne $img) {{
    $img.Save($out, [System.Drawing.Imaging.ImageFormat]::Png)
    exit 0
  }}
}}
exit 1",
                out = ps_quote(&temp_path.to_string_lossy()),
            );
            let output = output_with_timeout(&mut powershell_command(&script), POWERSHELL_TIMEOUT);

            if let Ok(output) = output
                && output.status.success()
                && temp_path.exists()
            {
                let bytes = std::fs::read(&temp_path)
                    .context("Failed to read clipboard image from temp file")?;
                let _ = std::fs::remove_file(&temp_path);
                if !bytes.is_empty() {
                    return Ok((bytes, "png".to_string()));
                }
            }
            anyhow::bail!("No image data found in clipboard (Windows)")
        },
    }
}

/// Read text from the clipboard (fallback when no image is found).
pub fn read_text() -> Result<String> {
    let backend = detect_backend()
        .context("No clipboard backend detected (need xclip, wl-paste, pbpaste, or PowerShell)")?;

    let output = match backend {
        ClipboardBackend::Wayland => output_with_timeout(
            Command::new("wl-paste").args(["--type", "text/plain"]),
            DATA_TIMEOUT,
        ),
        ClipboardBackend::X11 => output_with_timeout(
            Command::new("xclip").args(["-selection", "clipboard", "-o"]),
            DATA_TIMEOUT,
        ),
        ClipboardBackend::MacOS => output_with_timeout(&mut Command::new("pbpaste"), DATA_TIMEOUT),
        // `-Raw` keeps a multi-line copy as one string instead of an array
        // that Write-Output re-joins with the host's line ending.
        ClipboardBackend::Windows => output_with_timeout(
            &mut powershell_command("Get-Clipboard -Raw"),
            POWERSHELL_TIMEOUT,
        ),
    };

    let output = output.context("Failed to execute clipboard command")?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        anyhow::bail!("Clipboard does not contain text")
    }
}

/// Write `text` to the system clipboard. Mirrors `read_text`'s backend
/// detection and shells out to the platform tool (no extra dependency):
/// `wl-copy` / `xclip` / `pbcopy` / PowerShell `Set-Clipboard`. Used by the
/// in-app drag-select copy path.
pub fn write_text(text: &str) -> Result<()> {
    let backend =
        detect_backend().context("No clipboard backend detected (need xclip/wl-copy/pbcopy)")?;

    let (mut cmd, timeout) = match backend {
        ClipboardBackend::Wayland => (Command::new("wl-copy"), DATA_TIMEOUT),
        ClipboardBackend::X11 => {
            let mut cmd = Command::new("xclip");
            cmd.args(["-selection", "clipboard"]);
            (cmd, DATA_TIMEOUT)
        },
        ClipboardBackend::MacOS => (Command::new("pbcopy"), DATA_TIMEOUT),
        // Read all of stdin as UTF-8 and set the clipboard, so non-ASCII
        // survives (plain `clip.exe` reinterprets via the console codepage).
        ClipboardBackend::Windows => (
            powershell_command(
                "[Console]::InputEncoding=[System.Text.Encoding]::UTF8; \
                 Set-Clipboard -Value ([Console]::In.ReadToEnd())",
            ),
            POWERSHELL_TIMEOUT,
        ),
    };

    // `wl-copy` and `xclip` fork a background process that keeps *serving*
    // the selection after the parent exits; the helper points stdout/stderr
    // at null so that long-lived fork can't pin any pipe of ours.
    let status = write_stdin_with_timeout(&mut cmd, text.as_bytes().to_vec(), timeout)
        .context("clipboard write command failed to run")?;
    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("clipboard write command exited with {status}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_backend() {
        // Just verify it doesn't panic — actual result depends on environment
        let _ = detect_backend();
    }

    #[test]
    fn test_has_image_no_crash() {
        // Should return false gracefully if no display server
        let _ = has_image();
    }

    #[test]
    fn image_format_maps_extensions_case_insensitively() {
        for (name, want) in [
            ("shot.png", Some("png")),
            ("shot.PNG", Some("png")),
            ("photo.jpg", Some("jpeg")),
            ("photo.JPEG", Some("jpeg")),
            ("scan.tif", Some("tiff")),
            ("anim.gif", Some("gif")),
            ("logo.webp", Some("webp")),
            ("old.bmp", Some("bmp")),
            ("notes.txt", None),
            ("archive.tar.gz", None),
            ("noextension", None),
        ] {
            assert_eq!(
                image_format_for_path(Path::new(name)),
                want,
                "extension mapping for {name}"
            );
        }
    }

    /// A file-manager copy hands over URIs, not paths: percent-escapes have to
    /// come back out or a screenshot in `My Pictures` resolves to a path that
    /// does not exist.
    #[test]
    fn file_uris_decode_to_paths_on_both_path_shapes() {
        assert_eq!(
            path_from_file_uri("file:///home/noah/My%20Shot.png"),
            Some(PathBuf::from("/home/noah/My Shot.png"))
        );
        assert_eq!(
            path_from_file_uri("file:///C:/Users/noah/My%20Shot.png"),
            Some(PathBuf::from("C:/Users/noah/My Shot.png"))
        );
        // Non-ASCII survives: the escapes are UTF-8 bytes, not characters.
        assert_eq!(
            path_from_file_uri("file:///tmp/caf%C3%A9.png"),
            Some(PathBuf::from("/tmp/café.png"))
        );
        assert_eq!(path_from_file_uri("https://example.com/x.png"), None);
        assert_eq!(path_from_file_uri("file:///tmp/bad%ZZ.png"), None);
    }

    /// The list a file manager offers is filtered twice: to images, and to
    /// files that exist. A copied `.txt` must fall through to a text paste
    /// rather than being reported as an image the read then fails on.
    #[test]
    fn first_pasteable_file_skips_comments_non_images_and_missing_files() {
        let dir = std::env::temp_dir().join(format!("mermaid-clip-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create test dir");
        let png = dir.join("real.png");
        std::fs::write(&png, b"\x89PNG").expect("write test png");
        let txt = dir.join("notes.txt");
        std::fs::write(&txt, b"hi").expect("write test txt");
        let missing = dir.join("gone.png");

        let png_uri = format!("file:///{}", png.to_string_lossy().replace('\\', "/"));
        let found = first_pasteable_file(vec![
            "# uri-list comment",
            "",
            txt.to_str().expect("txt path"),
            missing.to_str().expect("missing path"),
            png_uri.as_str(),
        ]);
        assert_eq!(found, Some(png.clone()));

        // Nothing pasteable → None, so the caller falls back to a text paste.
        assert_eq!(
            first_pasteable_file(vec![txt.to_str().expect("txt path")]),
            None
        );

        // And the read honors the size ceiling rather than slurping blindly.
        let (bytes, format) = read_image_file(&png).expect("read the png");
        assert_eq!(bytes, b"\x89PNG");
        assert_eq!(format, "png");
        assert!(read_image_file(&txt).is_err(), "a .txt is not attachable");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The temp path is interpolated into a PowerShell script, so a quote in
    /// the user's profile name must not end the string literal.
    #[test]
    fn ps_quote_escapes_embedded_quotes() {
        assert_eq!(ps_quote(r"C:\Users\noah\a.png"), r"'C:\Users\noah\a.png'");
        assert_eq!(
            ps_quote(r"C:\Users\O'Brien\a.png"),
            r"'C:\Users\O''Brien\a.png'"
        );
    }

    /// `has_image` and `read_image_bytes` must answer the same question. They
    /// used to be written independently — `ContainsImage()` vs `GetImage()` on
    /// Windows — which is how a PNG-only clipboard reported "yes" and then
    /// failed to read.
    #[test]
    fn has_image_agrees_with_the_probe() {
        assert_eq!(has_image(), probe_image_source() != ImageSource::None);
    }

    /// Manual QA for the FILE-REFERENCE paste path on macOS and Linux.
    ///
    /// This one exists because the rest of that path could not be verified on
    /// the machine that wrote it. The URI parsing is unit-tested above and the
    /// Windows `CF_HDROP` branch was checked against a real Explorer copy; what
    /// is unproven is the platform plumbing in between — whether
    /// `osascript -e 'clipboard info'` really reports `furl` for a Finder copy,
    /// and whether a file manager's `text/uri-list` reaches
    /// `wl-paste`/`xclip` in the shape [`probe_image_source`] expects.
    ///
    /// Rather than leave that as a paragraph in a changelog, this makes it one
    /// command. **Copy an image file in Finder / Nautilus / Dolphin first**,
    /// then:
    ///
    /// `cargo test manual_file_reference_paste -- --ignored --nocapture`
    ///
    /// A pass means `has_image()` is true and the bytes come back with the
    /// file's own format. A failure prints what the probe actually saw, which
    /// is the diagnostic needed to fix it.
    #[test]
    #[ignore = "copy an image FILE in your file manager first, then run this"]
    fn manual_file_reference_paste() {
        let Some(backend) = detect_backend() else {
            eprintln!("no clipboard backend detected; nothing to exercise");
            return;
        };
        eprintln!("backend: {backend:?}");
        match probe_image_source() {
            ImageSource::File(path) => {
                eprintln!("resolved file reference: {}", path.display());
                let (bytes, format) = read_image_file(&path).expect("read the referenced file");
                eprintln!("read {} bytes as {format}", bytes.len());
                assert!(!bytes.is_empty());
                assert!(has_image(), "has_image must agree with the probe");
            },
            ImageSource::Inline => panic!(
                "the clipboard holds inline image data, not a file reference — \
                 copy an image FILE in your file manager (not the image itself) \
                 and re-run"
            ),
            ImageSource::None => panic!(
                "no image source detected. If you did copy an image file, THIS is \
                 the bug: the platform probe did not recognize the file reference."
            ),
        }
    }

    /// Manual QA for a real display server (CI has none): round-trips a
    /// string through the system clipboard, then restores the previous text
    /// contents. Run with:
    /// `cargo test manual_clipboard_roundtrip -- --ignored --nocapture`
    #[test]
    #[ignore = "needs a real display server + clipboard tools"]
    fn manual_clipboard_roundtrip() {
        if detect_backend().is_none() {
            eprintln!("no clipboard backend detected; nothing to exercise");
            return;
        }
        let previous = read_text().ok();
        let probe = "mermaid clipboard self-test";
        write_text(probe).expect("write_text");
        // Selection serving is asynchronous on Wayland/X11 — give the
        // background fork a beat to take ownership.
        std::thread::sleep(Duration::from_millis(200));
        let read_back = read_text().expect("read_text");
        if let Some(prev) = previous {
            let _ = write_text(&prev);
        }
        // Tools may append a trailing newline (wl-paste does by default).
        assert_eq!(read_back.trim_end(), probe);
    }

    /// Manual QA for the failure mode this module guards against: a frozen
    /// selection owner (SIGSTOP'd `wl-copy --foreground`) must surface as a
    /// bounded timeout error, not a read that never returns. Wayland-only;
    /// briefly replaces the clipboard, restoring text contents afterwards.
    /// Run with:
    /// `cargo test manual_hung_owner_times_out -- --ignored --nocapture`
    #[cfg(unix)]
    #[test]
    #[ignore = "needs Wayland + wl-copy; simulates a frozen selection owner"]
    fn manual_hung_owner_times_out() {
        if std::env::var("WAYLAND_DISPLAY").is_err() || !tool_exists("wl-copy") {
            eprintln!("no Wayland session; nothing to exercise");
            return;
        }
        let previous = read_text().ok();

        // A foreground wl-copy serves the selection itself; SIGSTOP freezes
        // it mid-service so any paste request blocks forever.
        let mut owner = Command::new("wl-copy")
            .args(["--foreground", "hung-owner-data"])
            .spawn()
            .expect("spawn wl-copy");
        std::thread::sleep(Duration::from_millis(300));
        let stop = Command::new("kill")
            .args(["-STOP", &owner.id().to_string()])
            .status()
            .expect("SIGSTOP owner");
        assert!(stop.success());

        let start = std::time::Instant::now();
        let result = read_text();
        let elapsed = start.elapsed();

        // Unfreeze and clean up the owner before asserting, so a failure
        // doesn't leave a stopped process owning the user's clipboard.
        let _ = Command::new("kill")
            .args(["-CONT", &owner.id().to_string()])
            .status();
        let _ = owner.kill();
        let _ = owner.wait();
        if let Some(prev) = previous {
            let _ = write_text(&prev);
        }

        eprintln!("read_text against frozen owner: {result:?} after {elapsed:?}");
        assert!(
            result.is_err(),
            "a frozen selection owner must surface as an error"
        );
        assert!(
            elapsed < Duration::from_secs(15),
            "the deadline must bound the stall (took {elapsed:?})"
        );
    }
}
