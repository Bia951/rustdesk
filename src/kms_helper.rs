use drm::{
    buffer::DrmFourcc,
    control::{self, connector, Device as ControlDevice},
    ClientCapability, Device as BasicDevice,
};
use hbb_common::{
    anyhow::{bail, Context},
    libc, ResultType,
};
use serde::Serialize;
use std::{
    fs,
    fs::OpenOptions,
    io::{self, ErrorKind, Write},
    os::fd::{AsFd, AsRawFd, BorrowedFd},
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Serialize)]
struct ProbeOutput {
    displays: Vec<ProbeDisplay>,
}

#[derive(Serialize)]
struct ProbeDisplay {
    card_path: String,
    connector_path: String,
    name: String,
    width: usize,
    height: usize,
    online: bool,
    can_open: bool,
    open_error: Option<String>,
}

#[derive(Serialize)]
struct FrameOutput {
    card_path: String,
    connector_path: String,
    name: String,
    width: usize,
    height: usize,
    stride: usize,
    pixfmt: &'static str,
    byte_len: usize,
}

struct FrameCapture {
    header: FrameOutput,
    bytes: Vec<u8>,
}

struct Card(std::fs::File);

impl AsFd for Card {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl BasicDevice for Card {}
impl ControlDevice for Card {}

pub fn run(args: &[String]) -> ResultType<()> {
    match args.first().map(|arg| arg.as_str()) {
        Some("probe") => write_json(&probe()?),
        Some("frame") => {
            let display_name = args.get(1).context("missing kms helper display name")?;
            write_frame(&frame(display_name, false)?)
        }
        Some("frame-privileged") => {
            let display_name = args.get(1).context("missing kms helper display name")?;
            write_frame(&frame(display_name, true)?)
        }
        Some(cmd) => bail!("unsupported kms helper command: {cmd}"),
        None => bail!("missing kms helper command"),
    }
}

fn write_json<T: Serialize>(value: &T) -> ResultType<()> {
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    serde_json::to_writer(&mut stdout, value)?;
    stdout.write_all(b"\n")?;
    stdout.flush()?;
    Ok(())
}

fn write_frame(frame: &FrameCapture) -> ResultType<()> {
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    serde_json::to_writer(&mut stdout, &frame.header)?;
    stdout.write_all(b"\n")?;
    stdout.write_all(&frame.bytes)?;
    stdout.flush()?;
    Ok(())
}

fn probe() -> ResultType<ProbeOutput> {
    let mut displays = Vec::new();
    for entry in fs::read_dir("/sys/class/drm").context("failed to read /sys/class/drm")? {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        match probe_display(entry.path()) {
            Ok(Some(display)) => displays.push(display),
            Ok(None) => {}
            Err(_) => {}
        }
    }
    displays.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(ProbeOutput { displays })
}

fn frame(display_name: &str, privileged: bool) -> ResultType<FrameCapture> {
    let display = find_display(display_name)?;
    if !display.can_open {
        return Err(io::Error::new(
            ErrorKind::PermissionDenied,
            display
                .open_error
                .clone()
                .unwrap_or_else(|| "failed to open drm device".to_owned()),
        )
        .into());
    }

    let card = Card(open_card(Path::new(&display.card_path))?);
    match capture_frame(&card, &display, privileged) {
        Ok(frame) => Ok(frame),
        Err(err) if !privileged && should_retry_privileged(&err) => {
            retry_privileged_frame(display_name)
        }
        Err(err) => Err(err),
    }
}

fn probe_display(path: PathBuf) -> ResultType<Option<ProbeDisplay>> {
    let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
        return Ok(None);
    };
    if !name.starts_with("card") || !name.contains('-') {
        return Ok(None);
    }

    let status = read_trimmed(path.join("status"))?;
    if !status.eq_ignore_ascii_case("connected") {
        return Ok(None);
    }

    let mode = read_first_line(path.join("modes"))?;
    let Some((width, height)) = parse_mode(&mode) else {
        return Ok(None);
    };

    let card_name = name
        .split_once('-')
        .map(|(card, _)| card)
        .context("invalid drm connector name")?;
    let card_path = PathBuf::from("/dev/dri").join(card_name);
    let (can_open, open_error) = check_card_access(&card_path);

    Ok(Some(ProbeDisplay {
        card_path: card_path.display().to_string(),
        connector_path: path.display().to_string(),
        name: name.to_owned(),
        width,
        height,
        online: true,
        can_open,
        open_error,
    }))
}

fn find_display(display_name: &str) -> ResultType<ProbeDisplay> {
    for entry in fs::read_dir("/sys/class/drm").context("failed to read /sys/class/drm")? {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        if let Some(display) = probe_display(entry.path())? {
            if display.name == display_name {
                return Ok(display);
            }
        }
    }
    bail!("kms display '{display_name}' not found");
}

fn open_card(card_path: &Path) -> io::Result<std::fs::File> {
    OpenOptions::new().read(true).write(true).open(card_path)
}

fn capture_frame(
    card: &Card,
    display: &ProbeDisplay,
    privileged: bool,
) -> ResultType<FrameCapture> {
    configure_card(card, privileged);
    let connector_name = display
        .name
        .split_once('-')
        .map(|(_, name)| name)
        .context("invalid display name")?;
    let resources = card.resource_handles()?;
    let connector = resolve_connector(card, connector_name, resources.connectors())?;
    let encoder_handle = connector
        .current_encoder()
        .or_else(|| connector.encoders().first().copied())
        .context("connector has no active encoder")?;
    let encoder = card.get_encoder(encoder_handle)?;
    let crtc_handle = encoder
        .crtc()
        .or_else(|| {
            resources
                .filter_crtcs(encoder.possible_crtcs())
                .into_iter()
                .next()
        })
        .context("encoder has no active CRTC")?;
    let crtc = card.get_crtc(crtc_handle)?;
    let framebuffer = resolve_framebuffer(card, crtc_handle, &crtc, display)?;
    let legacy = card.get_framebuffer(framebuffer)?;
    let planar = match card.get_planar_framebuffer(framebuffer) {
        Ok(planar) => planar,
        Err(control::GetPlanarFramebufferError::Io(err)) => return Err(err.into()),
        Err(control::GetPlanarFramebufferError::UnrecognizedFourcc(err)) => {
            return Err(io::Error::new(
                ErrorKind::Unsupported,
                format!("unsupported framebuffer format: {err}"),
            )
            .into())
        }
    };

    validate_modifier(planar.modifier())?;
    let buffer_handle = planar.buffers()[0]
        .or_else(|| legacy.buffer())
        .context("framebuffer has no accessible GEM handle")?;
    if planar.buffers()[1..].iter().any(|handle| handle.is_some()) {
        return Err(io::Error::new(
            ErrorKind::Unsupported,
            "multi-plane framebuffers are not supported yet",
        )
        .into());
    }

    let pixfmt = map_drm_fourcc(planar.pixel_format())?;
    let width = planar.size().0 as usize;
    let height = planar.size().1 as usize;
    let stride = planar.pitches()[0] as usize;
    let offset = planar.offsets()[0] as usize;
    let body_len = stride
        .checked_mul(height)
        .context("framebuffer size overflow")?;
    let map_len = offset
        .checked_add(body_len)
        .context("framebuffer map size overflow")?;
    let prime_fd = card.buffer_to_prime_fd(buffer_handle, 0)?;
    let mapping = MappedReadOnly::new(prime_fd.as_raw_fd(), map_len)?;
    let end = offset
        .checked_add(body_len)
        .context("framebuffer slice overflow")?;
    let bytes = mapping
        .as_slice()
        .get(offset..end)
        .context("framebuffer mapping smaller than expected")?
        .to_vec();

    Ok(FrameCapture {
        header: FrameOutput {
            card_path: display.card_path.clone(),
            connector_path: display.connector_path.clone(),
            name: display.name.clone(),
            width,
            height,
            stride,
            pixfmt,
            byte_len: bytes.len(),
        },
        bytes,
    })
}

fn configure_card(card: &Card, privileged: bool) {
    let _ = card.set_client_capability(ClientCapability::UniversalPlanes, true);
    if privileged {
        let _ = card.acquire_master_lock();
    }
}

fn should_retry_privileged(err: &hbb_common::anyhow::Error) -> bool {
    let text = err.to_string().to_ascii_lowercase();
    text.contains("permission denied")
        || text.contains("operation not permitted")
        || text.contains("no accessible gem handle")
        || text.contains("no active framebuffer")
}

fn retry_privileged_frame(display_name: &str) -> ResultType<FrameCapture> {
    let output = Command::new("pkexec")
        .arg(std::env::current_exe()?)
        .arg("--kms-helper")
        .arg("frame-privileged")
        .arg(display_name)
        .output()
        .context("failed to launch privileged kms helper")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let message = if stderr.is_empty() {
            "privileged kms helper failed".to_owned()
        } else {
            stderr
        };
        return Err(io::Error::new(ErrorKind::PermissionDenied, message).into());
    }
    parse_frame_capture_bytes(&output.stdout)
}

fn parse_frame_capture_bytes(bytes: &[u8]) -> ResultType<FrameCapture> {
    let split = bytes
        .iter()
        .position(|byte| *byte == b'\n')
        .context("privileged kms helper returned malformed frame header")?;
    let header: FrameOutputOwned =
        serde_json::from_slice(&bytes[..split]).context("failed to parse frame header")?;
    let body = bytes[split + 1..].to_vec();
    if body.len() != header.byte_len {
        bail!(
            "privileged kms helper frame length mismatch: expected {}, got {}",
            header.byte_len,
            body.len()
        );
    }
    Ok(FrameCapture {
        header: FrameOutput {
            card_path: header.card_path,
            connector_path: header.connector_path,
            name: header.name,
            width: header.width,
            height: header.height,
            stride: header.stride,
            pixfmt: leak_pixfmt(header.pixfmt),
            byte_len: header.byte_len,
        },
        bytes: body,
    })
}

fn leak_pixfmt(pixfmt: String) -> &'static str {
    match pixfmt.as_str() {
        "BGRA" => "BGRA",
        "RGBA" => "RGBA",
        "RGB565LE" => "RGB565LE",
        _ => Box::leak(pixfmt.into_boxed_str()),
    }
}

fn resolve_framebuffer(
    card: &Card,
    crtc_handle: control::crtc::Handle,
    crtc: &control::crtc::Info,
    display: &ProbeDisplay,
) -> ResultType<control::framebuffer::Handle> {
    if let Some(framebuffer) = crtc.framebuffer() {
        return Ok(framebuffer);
    }

    let mut best: Option<(usize, control::framebuffer::Handle)> = None;
    for plane_handle in card.plane_handles()? {
        let plane = match card.get_plane(plane_handle) {
            Ok(plane) => plane,
            Err(_) => continue,
        };
        if plane.crtc() != Some(crtc_handle) {
            continue;
        }
        let Some(framebuffer) = plane.framebuffer() else {
            continue;
        };
        let info = match card.get_framebuffer(framebuffer) {
            Ok(info) => info,
            Err(_) => continue,
        };
        let size = info.size();
        let area = size.0 as usize * size.1 as usize;

        // Prefer framebuffers that are at least as large as the connector mode.
        if size.0 as usize >= display.width && size.1 as usize >= display.height {
            return Ok(framebuffer);
        }

        match best {
            Some((best_area, _)) if best_area >= area => {}
            _ => best = Some((area, framebuffer)),
        }
    }

    if let Some((_, framebuffer)) = best {
        return Ok(framebuffer);
    }

    bail!("crtc has no active framebuffer")
}

fn resolve_connector(
    card: &Card,
    connector_name: &str,
    handles: &[connector::Handle],
) -> ResultType<connector::Info> {
    for handle in handles {
        let connector = match card.get_connector(*handle, false) {
            Ok(connector) => connector,
            Err(_) => continue,
        };
        if connector.state() != connector::State::Connected {
            continue;
        }
        if connector.to_string() == connector_name {
            return Ok(connector);
        }
    }
    bail!("active connector '{connector_name}' not found");
}

fn validate_modifier(modifier: Option<drm::buffer::DrmModifier>) -> ResultType<()> {
    if let Some(modifier) = modifier {
        if modifier != drm::buffer::DrmModifier::Linear {
            return Err(io::Error::new(
                ErrorKind::Unsupported,
                format!("unsupported framebuffer modifier: {modifier:?}"),
            )
            .into());
        }
    }
    Ok(())
}

fn map_drm_fourcc(format: DrmFourcc) -> ResultType<&'static str> {
    match format {
        DrmFourcc::Argb8888 | DrmFourcc::Xrgb8888 => Ok("BGRA"),
        DrmFourcc::Abgr8888 | DrmFourcc::Xbgr8888 => Ok("RGBA"),
        DrmFourcc::Rgb565 => Ok("RGB565LE"),
        other => Err(io::Error::new(
            ErrorKind::Unsupported,
            format!("unsupported framebuffer format: {other}"),
        )
        .into()),
    }
}

fn check_card_access(card_path: &Path) -> (bool, Option<String>) {
    match open_card(card_path) {
        Ok(_) => (true, None),
        Err(err) => (false, Some(err.to_string())),
    }
}

fn read_trimmed(path: PathBuf) -> ResultType<String> {
    Ok(fs::read_to_string(path)?.trim().to_owned())
}

fn read_first_line(path: PathBuf) -> ResultType<String> {
    fs::read_to_string(path)?
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .context("missing drm mode")
}

fn parse_mode(mode: &str) -> Option<(usize, usize)> {
    let (width, height) = mode.split_once('x')?;
    Some((width.parse().ok()?, height.parse().ok()?))
}

struct MappedReadOnly {
    ptr: *mut libc::c_void,
    len: usize,
}

impl MappedReadOnly {
    fn new(fd: i32, len: usize) -> io::Result<Self> {
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { ptr, len })
    }

    fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr.cast::<u8>(), self.len) }
    }
}

impl Drop for MappedReadOnly {
    fn drop(&mut self) {
        if !self.ptr.is_null() && self.ptr != libc::MAP_FAILED {
            unsafe {
                libc::munmap(self.ptr, self.len);
            }
        }
    }
}

#[derive(serde::Deserialize)]
struct FrameOutputOwned {
    card_path: String,
    connector_path: String,
    name: String,
    width: usize,
    height: usize,
    stride: usize,
    pixfmt: String,
    byte_len: usize,
}
