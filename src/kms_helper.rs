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
    cmp::Reverse,
    fs,
    fs::OpenOptions,
    io::{self, BufRead, BufReader, ErrorKind, Write},
    os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd},
    os::unix::net::UnixDatagram,
    path::{Path, PathBuf},
    process::Command,
};

const DMA_BUF_IOCTL_SYNC: libc::c_ulong = 0x4008_6200;
const DMA_BUF_SYNC_READ: u64 = 1 << 0;
const DMA_BUF_SYNC_END: u64 = 1 << 2;
const DRM_IOCTL_MODE_MAP_DUMB: libc::c_ulong = 0xc010_64b3;

#[derive(Serialize)]
struct ProbeOutput {
    displays: Vec<ProbeDisplay>,
}

#[derive(Serialize)]
struct ProbeDisplay {
    card_path: String,
    connector_path: String,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    render_node: Option<String>,
    width: usize,
    height: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    x: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    y: Option<i32>,
    // false when the active scanout buffer uses a tiled/compressed modifier the
    // CPU readback path cannot decode; such displays must use the dmabuf path.
    is_linear: bool,
    modifier_known: bool,
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

#[derive(Serialize)]
struct DmabufFrameOutput {
    card_path: String,
    connector_path: String,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    render_node: Option<String>,
    width: usize,
    height: usize,
    fourcc: u32,
    modifier: u64,
    planes: Vec<DmabufPlaneOutput>,
}

#[derive(Serialize)]
struct DmabufPlaneOutput {
    stride: u32,
    offset: u32,
}

struct DmabufFrameCapture {
    header: DmabufFrameOutput,
    fds: Vec<OwnedFd>,
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
        Some("stream") => {
            let display_name = args.get(1).context("missing kms helper display name")?;
            stream(display_name, false)
        }
        Some("stream-privileged") => {
            let display_name = args.get(1).context("missing kms helper display name")?;
            stream(display_name, true)
        }
        Some("dmabuf-stream") => {
            let display_name = args.get(1).context("missing kms helper display name")?;
            let socket_path = args.get(2).context("missing kms helper dmabuf socket")?;
            stream_dmabuf(display_name, socket_path, false)
        }
        Some("dmabuf-stream-privileged") => {
            let display_name = args.get(1).context("missing kms helper display name")?;
            let socket_path = args.get(2).context("missing kms helper dmabuf socket")?;
            stream_dmabuf(display_name, socket_path, true)
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

fn write_dmabuf_frame(socket: &UnixDatagram, frame: &DmabufFrameCapture) -> ResultType<()> {
    let mut header = serde_json::to_vec(&frame.header)?;
    header.push(b'\n');
    let fds = frame
        .fds
        .iter()
        .map(AsRawFd::as_raw_fd)
        .collect::<Vec<_>>();
    send_fds(socket.as_raw_fd(), &header, &fds)?;
    Ok(())
}

fn cmsg_align(len: usize) -> usize {
    let align = std::mem::size_of::<usize>();
    (len + align - 1) & !(align - 1)
}

fn send_fds(socket_fd: RawFd, bytes: &[u8], fds: &[RawFd]) -> io::Result<()> {
    let mut iov = libc::iovec {
        iov_base: bytes.as_ptr() as *mut libc::c_void,
        iov_len: bytes.len(),
    };
    let fd_bytes = std::mem::size_of_val(fds);
    let control_len = cmsg_align(std::mem::size_of::<libc::cmsghdr>()) + cmsg_align(fd_bytes);
    let mut control = vec![0_u8; control_len];
    let cmsg = control.as_mut_ptr().cast::<libc::cmsghdr>();
    unsafe {
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = (cmsg_align(std::mem::size_of::<libc::cmsghdr>()) + fd_bytes) as _;
        let data = control
            .as_mut_ptr()
            .add(cmsg_align(std::mem::size_of::<libc::cmsghdr>()))
            .cast::<RawFd>();
        std::ptr::copy_nonoverlapping(fds.as_ptr(), data, fds.len());
        let msg = libc::msghdr {
            msg_name: std::ptr::null_mut(),
            msg_namelen: 0,
            msg_iov: &mut iov,
            msg_iovlen: 1,
            msg_control: control.as_mut_ptr().cast::<libc::c_void>(),
            msg_controllen: control.len() as _,
            msg_flags: 0,
        };
        let written = libc::sendmsg(socket_fd, &msg, 0);
        if written < 0 {
            return Err(io::Error::last_os_error());
        }
        if written as usize != bytes.len() {
            return Err(io::Error::new(
                ErrorKind::WriteZero,
                format!(
                    "short kms dmabuf datagram write: expected {}, wrote {written}",
                    bytes.len()
                ),
            ));
        }
    }
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
    configure_card(&card);
    match capture_frame(&card, &display) {
        Ok(frame) => Ok(frame),
        Err(err) if !privileged && should_retry_privileged(&err) => {
            retry_privileged_frame(display_name)
        }
        Err(err) => Err(err),
    }
}

fn stream(display_name: &str, privileged: bool) -> ResultType<()> {
    let display = find_display(display_name)?;
    if !privileged && !display.can_open {
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
    configure_card(&card);

    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    stdout.write_all(b"ready\n")?;
    stdout.flush()?;
    drop(stdout);

    let stdin = io::stdin();
    let mut stdin = BufReader::new(stdin.lock());
    let mut line = String::new();
    loop {
        line.clear();
        let read = stdin.read_line(&mut line)?;
        if read == 0 {
            break;
        }
        match line.trim() {
            "frame" => write_frame(&capture_frame(&card, &display)?)?,
            "quit" => break,
            "" => {}
            cmd => bail!("unsupported kms helper stream command: {cmd}"),
        }
    }
    Ok(())
}

fn stream_dmabuf(display_name: &str, socket_path: &str, privileged: bool) -> ResultType<()> {
    let display = find_display(display_name)?;
    if !privileged && !display.can_open {
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
    configure_card(&card);
    let socket = UnixDatagram::unbound()
        .context("failed to create kms dmabuf datagram socket")?;
    socket
        .connect(socket_path)
        .with_context(|| format!("failed to connect kms dmabuf socket {socket_path}"))?;

    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    stdout.write_all(b"ready\n")?;
    stdout.flush()?;
    drop(stdout);

    let stdin = io::stdin();
    let mut stdin = BufReader::new(stdin.lock());
    let mut line = String::new();
    loop {
        line.clear();
        let read = stdin.read_line(&mut line)?;
        if read == 0 {
            break;
        }
        match line.trim() {
            "frame" => write_dmabuf_frame(&socket, &capture_dmabuf_frame(&card, &display)?)?,
            "quit" => break,
            "" => {}
            cmd => bail!("unsupported kms helper dmabuf stream command: {cmd}"),
        }
    }
    Ok(())
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
    let Some((mut width, mut height)) = parse_mode(&mode) else {
        return Ok(None);
    };

    let card_name = name
        .split_once('-')
        .map(|(card, _)| card)
        .context("invalid drm connector name")?;
    let card_path = PathBuf::from("/dev/dri").join(card_name);
    let render_node = find_render_node(&card_path).map(|path| path.display().to_string());
    let (can_open, open_error) = check_card_access(&card_path);
    let mut is_linear = true;
    let mut modifier_known = false;
    let mut origin = None;
    if can_open {
        if let Ok(card) = open_card(&card_path) {
            let card = Card(card);
            configure_card(&card);
            if let Ok((active_width, active_height, active_is_linear, active_x, active_y)) =
                active_framebuffer_info(&card, name, width, height)
            {
                width = active_width;
                height = active_height;
                is_linear = active_is_linear;
                modifier_known = true;
                origin = Some((active_x, active_y));
            }
        }
    }

    Ok(Some(ProbeDisplay {
        card_path: card_path.display().to_string(),
        connector_path: path.display().to_string(),
        name: name.to_owned(),
        render_node,
        width,
        height,
        x: origin.map(|value| value.0),
        y: origin.map(|value| value.1),
        is_linear,
        modifier_known,
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

fn active_framebuffer_info(
    card: &Card,
    display_name: &str,
    fallback_width: usize,
    fallback_height: usize,
) -> ResultType<(usize, usize, bool, i32, i32)> {
    let connector_name = display_name
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
    let framebuffer = resolve_framebuffers(card, crtc_handle, &crtc, fallback_width, fallback_height)?
        .into_iter()
        .next()
        .context("crtc has no active framebuffer")?;
    let info = card.get_framebuffer(framebuffer)?;
    let size = info.size();
    let position = crtc.position();
    let is_linear = framebuffer_is_linear(card, framebuffer);
    Ok((
        size.0 as usize,
        size.1 as usize,
        is_linear,
        position.0.min(i32::MAX as u32) as i32,
        position.1.min(i32::MAX as u32) as i32,
    ))
}

fn open_card(card_path: &Path) -> io::Result<std::fs::File> {
    OpenOptions::new().read(true).write(true).open(card_path)
}

fn capture_frame(card: &Card, display: &ProbeDisplay) -> ResultType<FrameCapture> {
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
    let framebuffers =
        resolve_framebuffers(card, crtc_handle, &crtc, display.width, display.height)?;
    let mut fallback = None;
    let mut last_err = None;
    for framebuffer in framebuffers {
        match capture_framebuffer(card, display, framebuffer) {
            Ok(frame) => {
                if frame_has_visible_rgb(&frame) {
                    return Ok(frame);
                }
                if fallback.is_none() {
                    fallback = Some(frame);
                }
            }
            Err(err) => {
                last_err = Some(err);
            }
        }
    }
    if let Some(frame) = fallback {
        return Ok(frame);
    }
    if let Some(err) = last_err {
        return Err(err);
    }
    bail!("crtc has no active framebuffer")
}

fn capture_dmabuf_frame(card: &Card, display: &ProbeDisplay) -> ResultType<DmabufFrameCapture> {
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
    let framebuffers =
        resolve_framebuffers(card, crtc_handle, &crtc, display.width, display.height)?;
    let framebuffer = framebuffers
        .into_iter()
        .next()
        .context("crtc has no active framebuffer")?;
    capture_framebuffer_dmabuf(card, display, framebuffer)
}

fn capture_framebuffer(
    card: &Card,
    display: &ProbeDisplay,
    framebuffer: control::framebuffer::Handle,
) -> ResultType<FrameCapture> {
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

    let drm_format = planar.pixel_format();
    let pixfmt = map_drm_fourcc(drm_format)?;
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
    let mut bytes = read_prime_framebuffer(card, buffer_handle, offset, map_len, body_len)?;
    if should_force_opaque_alpha(drm_format) {
        force_opaque_alpha(&mut bytes);
    }
    if !bytes_have_visible_rgb(pixfmt, &bytes) {
        if let Ok(mut dumb_bytes) =
            read_dumb_framebuffer(card, buffer_handle, offset, map_len, body_len)
        {
            if should_force_opaque_alpha(drm_format) {
                force_opaque_alpha(&mut dumb_bytes);
            }
            if bytes_have_visible_rgb(pixfmt, &dumb_bytes) {
                bytes = dumb_bytes;
            }
        }
    }

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

fn find_render_node(card_path: &Path) -> Option<PathBuf> {
    let card_name = card_path.file_name()?.to_str()?;
    let drm_dir = PathBuf::from("/sys/class/drm")
        .join(card_name)
        .join("device/drm");
    let mut nodes = fs::read_dir(drm_dir)
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_str()?;
            name.starts_with("renderD")
                .then(|| PathBuf::from("/dev/dri").join(name))
        })
        .collect::<Vec<_>>();
    nodes.sort();
    nodes.into_iter().next()
}

fn capture_framebuffer_dmabuf(
    card: &Card,
    display: &ProbeDisplay,
    framebuffer: control::framebuffer::Handle,
) -> ResultType<DmabufFrameCapture> {
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

    let drm_format = planar.pixel_format();
    let modifier = planar
        .modifier()
        .map(u64::from)
        .unwrap_or_else(|| u64::from(drm::buffer::DrmModifier::Linear));
    let buffers = planar.buffers();
    let fallback_buffer = legacy.buffer();
    let primary_buffer = buffers[0]
        .or(fallback_buffer)
        .context("framebuffer has no accessible GEM handle")?;
    let pitches = planar.pitches();
    let offsets = planar.offsets();
    let mut planes = Vec::new();
    let mut fds = Vec::new();
    for (idx, handle) in buffers.iter().enumerate() {
        if pitches[idx] == 0 {
            continue;
        }
        // drmModeGetFB2 may report a handle only for plane 0 when multiple
        // planes (for example NV12 Y and UV) share one GEM object.
        let handle = handle.unwrap_or(primary_buffer);
        fds.push(card.buffer_to_prime_fd(handle, 0)?);
        planes.push(DmabufPlaneOutput {
            stride: pitches[idx],
            offset: offsets[idx],
        });
    }
    if planes.is_empty() {
        bail!("framebuffer has no exportable dmabuf planes");
    }

    Ok(DmabufFrameCapture {
        header: DmabufFrameOutput {
            card_path: display.card_path.clone(),
            connector_path: display.connector_path.clone(),
            name: display.name.clone(),
            render_node: display.render_node.clone(),
            width: planar.size().0 as usize,
            height: planar.size().1 as usize,
            fourcc: drm_format as u32,
            modifier,
            planes,
        },
        fds,
    })
}

// Only enable universal planes here: this call does not need DRM master and is
// safe on a non-master fd. Never call drmSetMaster: the privileged/pkexec path
// runs as root, where CAP_SYS_ADMIN already lets drmModeGetFB2 return a usable
// GEM handle without master. Grabbing master would race an active compositor —
// during a VT-switch/handover window it could steal master and leave the
// desktop session unable to modeset, blanking the screen.
fn configure_card(card: &Card) {
    let _ = card.set_client_capability(ClientCapability::UniversalPlanes, true);
    // Enable atomic so planes expose their CRTC_ID/FB_ID/type properties, which
    // resolve_framebuffers uses to find the PRIMARY (desktop) plane. Read-only — we
    // never commit; if the driver lacks atomic the call just fails and is ignored.
    let _ = card.set_client_capability(ClientCapability::Atomic, true);
}

fn should_retry_privileged(err: &hbb_common::anyhow::Error) -> bool {
    let text = err.to_string().to_ascii_lowercase();
    text.contains("permission denied")
        || text.contains("operation not permitted")
        || text.contains("no accessible gem handle")
}

fn retry_privileged_frame(display_name: &str) -> ResultType<FrameCapture> {
    let output = Command::new("pkexec")
        .arg("--disable-internal-agent")
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

fn read_prime_framebuffer(
    card: &Card,
    buffer_handle: drm::buffer::Handle,
    offset: usize,
    map_len: usize,
    body_len: usize,
) -> ResultType<Vec<u8>> {
    let prime_fd = card.buffer_to_prime_fd(buffer_handle, 0)?;
    let _sync = DmaBufReadSync::start(prime_fd.as_raw_fd());
    let mapping = MappedReadOnly::new_at(prime_fd.as_raw_fd(), map_len, 0)?;
    framebuffer_slice(&mapping, offset, body_len)
}

fn read_dumb_framebuffer(
    card: &Card,
    buffer_handle: drm::buffer::Handle,
    offset: usize,
    map_len: usize,
    body_len: usize,
) -> ResultType<Vec<u8>> {
    let info = map_dumb_buffer(card.as_fd().as_raw_fd(), buffer_handle.into())?;
    let mapping = MappedReadOnly::new_at(card.as_fd().as_raw_fd(), map_len, info.offset)?;
    framebuffer_slice(&mapping, offset, body_len)
}

fn framebuffer_slice(
    mapping: &MappedReadOnly,
    offset: usize,
    body_len: usize,
) -> ResultType<Vec<u8>> {
    let end = offset
        .checked_add(body_len)
        .context("framebuffer slice overflow")?;
    Ok(mapping
        .as_slice()
        .get(offset..end)
        .context("framebuffer mapping smaller than expected")?
        .to_vec())
}

fn resolve_framebuffers(
    card: &Card,
    crtc_handle: control::crtc::Handle,
    crtc: &control::crtc::Info,
    min_width: usize,
    min_height: usize,
) -> ResultType<Vec<control::framebuffer::Handle>> {
    // Preferred: pick planes by their standardized DRM atomic *properties*, not the
    // legacy GetPlane fields. Wayland/atomic compositors (KWin, mutter, …) drive the
    // planes via atomic state and leave the legacy crtc_id/fb_id at 0, so the old
    // size heuristic missed the desktop (PRIMARY) plane and grabbed the 64x64 CURSOR
    // plane. Plane `type` is device-independent (OVERLAY=0, PRIMARY=1, CURSOR=2);
    // `CRTC_ID` says which CRTC it's on; `FB_ID` is the currently bound framebuffer.
    const DRM_PLANE_TYPE_PRIMARY: u64 = 1;
    const DRM_PLANE_TYPE_CURSOR: u64 = 2;
    let crtc_id: u32 = crtc_handle.into();
    let mut typed: Vec<(u64, control::framebuffer::Handle)> = Vec::new();
    if let Ok(planes) = card.plane_handles() {
        for plane_handle in planes {
            let Ok(props) = card.get_properties(plane_handle) else {
                continue;
            };
            let mut p_type: Option<u64> = None;
            let mut p_crtc: Option<u32> = None;
            let mut p_fb: Option<u32> = None;
            for (prop, value) in props.iter() {
                let Ok(info) = card.get_property(*prop) else {
                    continue;
                };
                match info.name().to_str() {
                    Ok("type") => p_type = Some(*value),
                    Ok("CRTC_ID") => p_crtc = Some(*value as u32),
                    Ok("FB_ID") => p_fb = Some(*value as u32),
                    _ => {}
                }
            }
            if p_crtc != Some(crtc_id) {
                continue; // not scanned out on our CRTC
            }
            let Some(fb_raw) = p_fb.filter(|fb| *fb != 0) else {
                continue; // no framebuffer currently bound
            };
            let Some(fb) = control::from_u32::<control::framebuffer::Handle>(fb_raw) else {
                continue;
            };
            let ty = p_type.unwrap_or(0);
            if ty == DRM_PLANE_TYPE_CURSOR {
                continue; // never capture the cursor sprite
            }
            typed.push((ty, fb));
        }
    }
    if !typed.is_empty() {
        // PRIMARY (the desktop) first; overlays after, as fallbacks.
        typed.sort_by_key(|(ty, _)| if *ty == DRM_PLANE_TYPE_PRIMARY { 0 } else { 1 });
        let mut framebuffers = Vec::new();
        for (_, fb) in typed {
            push_unique_framebuffer(&mut framebuffers, fb);
        }
        return Ok(framebuffers);
    }

    // Fallback for non-atomic drivers / when properties are unavailable: legacy
    // fields plus the CRTC's own framebuffer, ranked by mode coverage then area.
    let mut candidates = Vec::new();
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
        let covers_mode = size.0 as usize >= min_width && size.1 as usize >= min_height;
        candidates.push((!covers_mode, Reverse(area), framebuffer));
    }

    candidates.sort_by_key(|candidate| (candidate.0, candidate.1));
    let mut framebuffers = Vec::new();
    for (_, _, framebuffer) in candidates {
        push_unique_framebuffer(&mut framebuffers, framebuffer);
    }

    if let Some(framebuffer) = crtc.framebuffer() {
        push_unique_framebuffer(&mut framebuffers, framebuffer);
    }

    if framebuffers.is_empty() {
        bail!("crtc has no active framebuffer");
    }
    Ok(framebuffers)
}

fn push_unique_framebuffer(
    framebuffers: &mut Vec<control::framebuffer::Handle>,
    framebuffer: control::framebuffer::Handle,
) {
    if !framebuffers.iter().any(|existing| *existing == framebuffer) {
        framebuffers.push(framebuffer);
    }
}

fn frame_has_visible_rgb(frame: &FrameCapture) -> bool {
    bytes_have_visible_rgb(frame.header.pixfmt, &frame.bytes)
}

fn bytes_have_visible_rgb(pixfmt: &str, bytes: &[u8]) -> bool {
    match pixfmt {
        "BGRA" | "RGBA" => bytes
            .chunks_exact(4)
            .any(|pixel| pixel[0] != 0 || pixel[1] != 0 || pixel[2] != 0),
        "RGB565LE" => bytes.chunks_exact(2).any(|pixel| pixel[0] != 0 || pixel[1] != 0),
        _ => bytes.iter().any(|byte| *byte != 0),
    }
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
            // The CPU readback path mmaps the scanout buffer as a linear array.
            // Tiled/compressed modifiers (the default on modern Intel/AMD) would
            // decode to garbage, so reject them with a typed, actionable error.
            // The GPU dmabuf/VAAPI path understands tiling and should be used
            // instead. ErrorKind::Unsupported on purpose: it must not be mistaken
            // for a permission error and trigger a pointless pkexec retry.
            return Err(io::Error::new(
                ErrorKind::Unsupported,
                format!(
                    "non-linear framebuffer modifier {modifier:?}; the CPU/KMS readback path \
                     requires a linear buffer — use the dmabuf/VAAPI capture path"
                ),
            )
            .into());
        }
    }
    Ok(())
}

fn framebuffer_is_linear(card: &Card, framebuffer: control::framebuffer::Handle) -> bool {
    // drmModeGetFB2 reports the format modifier without needing DRM master, so
    // even an unprivileged probe can tell tiled buffers from linear ones. If the
    // modifier can't be determined, assume linear so we don't force the dmabuf
    // path onto buffers the CPU readback could have handled.
    match card.get_planar_framebuffer(framebuffer) {
        Ok(planar) => matches!(
            planar.modifier(),
            None | Some(drm::buffer::DrmModifier::Linear)
        ),
        Err(_) => true,
    }
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

fn should_force_opaque_alpha(format: DrmFourcc) -> bool {
    matches!(format, DrmFourcc::Xrgb8888 | DrmFourcc::Xbgr8888)
}

fn force_opaque_alpha(bytes: &mut [u8]) {
    for pixel in bytes.chunks_exact_mut(4) {
        pixel[3] = 0xff;
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

struct DmaBufReadSync {
    fd: i32,
}

impl DmaBufReadSync {
    fn start(fd: i32) -> Option<Self> {
        if sync_dmabuf(fd, DMA_BUF_SYNC_READ).is_ok() {
            Some(Self { fd })
        } else {
            None
        }
    }
}

impl Drop for DmaBufReadSync {
    fn drop(&mut self) {
        let _ = sync_dmabuf(self.fd, DMA_BUF_SYNC_READ | DMA_BUF_SYNC_END);
    }
}

#[repr(C)]
struct DmaBufSync {
    flags: u64,
}

fn sync_dmabuf(fd: i32, flags: u64) -> io::Result<()> {
    let mut sync = DmaBufSync { flags };
    let ret = unsafe { libc::ioctl(fd, DMA_BUF_IOCTL_SYNC, &mut sync) };
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[repr(C)]
struct DrmModeMapDumb {
    handle: u32,
    pad: u32,
    offset: u64,
}

fn map_dumb_buffer(fd: i32, handle: u32) -> io::Result<DrmModeMapDumb> {
    let mut map = DrmModeMapDumb {
        handle,
        pad: 0,
        offset: 0,
    };
    let ret = unsafe { libc::ioctl(fd, DRM_IOCTL_MODE_MAP_DUMB, &mut map) };
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(map)
    }
}

impl MappedReadOnly {
    fn new_at(fd: i32, len: usize, offset: u64) -> io::Result<Self> {
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_SHARED,
                fd,
                offset as libc::off_t,
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
